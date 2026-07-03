//! Integration test for the `daemon` subcommand (feature `server`,
//! which is on by default so plain `cargo test` runs this).
//!
//! Flow:
//!   1. Build a temp dir with one fixture `.org` file.
//!   2. Start the real binary in `daemon` mode on an ephemeral port.
//!   3. Poll `GET /api/todos` until it returns 200 (bounded retries),
//!      then assert on the snapshot body and the `x-sub-id` header.
//!   4. Open `GET /sse/todos?sub=<id>` (reqwest blocking, streaming
//!      read).
//!   5. Modify the fixture file and assert an SSE `data:` line
//!      containing `"file-changed"` arrives within 10 s.
//!   6. Kill the child (a Drop guard guarantees this even on a failed
//!      assertion).
//!
//! ## Port selection
//!
//! Port 0 hands the OS a free port but hides it from us until the
//! server logs its bound address. To keep the test simple and
//! log-parsing-free, we pick a free port ourselves: bind a std
//! `TcpListener` to `127.0.0.1:0`, read the assigned port, drop the
//! listener, and pass that port to `--addr`. There is a tiny race
//! (another process could grab the port between drop and re-bind);
//! it is negligible on a test host and keeps the test deterministic
//! and free of stderr scraping.

use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_org-cli"))
}

/// A unique temp directory for one test; removed on drop.
struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        p.push(format!("org-cli-daemon-it-{}-{}-{}", tag, pid, nanos));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// RAII guard that kills the spawned daemon on drop — so a panicking
/// assertion never leaks a live child holding the port.
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn write(p: &Path, s: &str) {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, s).unwrap();
}

/// Grab a currently-free port by binding to `:0` and reading it back.
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

#[test]
fn daemon_serves_snapshot_and_pushes_sse_on_file_change() {
    let td = TempDir::new("push");
    let org = td.path().join("a.org");
    write(&org, "* TODO Original task\n");
    let db = td.path().join("todos.sqlite");

    let port = free_port();
    let addr = format!("127.0.0.1:{}", port);

    // Spawn the daemon watching just our fixture dir.
    let child = Command::new(bin_path())
        .arg("daemon")
        .arg(td.path())
        .arg("--addr")
        .arg(&addr)
        .arg("--db")
        .arg(&db)
        .spawn()
        .expect("spawn org-cli daemon");
    let mut guard = ChildGuard(child);

    let base = format!("http://{}", addr);
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    // --- 1) Poll GET /api/todos until 200 (bounded retries) --------
    let deadline = Instant::now() + Duration::from_secs(10);
    let snapshot = loop {
        assert!(
            Instant::now() < deadline,
            "daemon never answered GET /api/todos with 200 in time"
        );
        match http.get(format!("{}/api/todos", base)).send() {
            Ok(resp) if resp.status().is_success() => break resp,
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    };

    // The x-sub-id header must be present; capture it for the SSE sub.
    let sub_id = snapshot
        .headers()
        .get("x-sub-id")
        .expect("snapshot must carry x-sub-id")
        .to_str()
        .unwrap()
        .to_string();
    assert!(!sub_id.is_empty(), "x-sub-id must be non-empty");
    assert!(
        snapshot.headers().get("x-snapshot-index").is_some(),
        "snapshot must carry x-snapshot-index"
    );

    // Body: one TODO record for the fixture. Parse the text ourselves
    // (reqwest's `json()` needs the `json` feature; we only enable
    // `blocking`).
    let body = snapshot.text().expect("snapshot body text");
    let records: Vec<serde_json::Value> = serde_json::from_str(&body).expect("snapshot JSON array");
    assert_eq!(
        records.len(),
        1,
        "expected one indexed TODO, got {records:?}"
    );
    assert_eq!(records[0]["title"], "Original task");
    assert_eq!(records[0]["keyword"], "TODO");

    // --- 2) Open the SSE stream carrying that sub -----------------
    // A dedicated long-timeout client: the stream stays open, and we
    // read line-by-line until the pushed diff appears.
    let sse_client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let sse_resp = sse_client
        .get(format!("{}/sse/todos?sub={}", base, sub_id))
        .send()
        .expect("open SSE stream");
    assert!(
        sse_resp.status().is_success(),
        "SSE endpoint should return 200 for a registered sub, got {}",
        sse_resp.status()
    );

    // Read the SSE stream on a background thread so the main thread can
    // enforce a hard wall-clock bound (the blocking reader itself only
    // has a per-read timeout).
    let (found_tx, found_rx) = std::sync::mpsc::channel::<bool>();
    let reader_handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(SseBody(sse_resp));
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = found_tx.send(false);
                    return;
                }
                Ok(_) => {
                    if line.starts_with("data:") && line.contains("file-changed") {
                        let _ = found_tx.send(true);
                        return;
                    }
                }
                Err(_) => {
                    let _ = found_tx.send(false);
                    return;
                }
            }
        }
    });

    // Give the SSE subscription a beat to attach, then modify the file
    // (a size change guarantees a fresh fingerprint → reparse).
    std::thread::sleep(Duration::from_millis(300));
    write(
        &org,
        "* TODO Rewritten with more bytes\n* TODO Second task\n",
    );

    // --- 3) Assert the push arrives within 10 s ------------------
    let got = found_rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or(false);
    assert!(
        got,
        "expected an SSE `data:` line containing 'file-changed' within 10s"
    );
    let _ = reader_handle.join();

    // Explicit kill (Drop would also handle it, but be tidy).
    let _ = guard.0.kill();
    let _ = guard.0.wait();
}

/// Adapter so `reqwest::blocking::Response` (which is `Read`) can be
/// wrapped in a `BufReader` for line-based SSE parsing.
struct SseBody(reqwest::blocking::Response);
impl Read for SseBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}
