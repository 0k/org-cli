//! Integration tests for `PATCH /api/heading` on the real daemon binary.

#![cfg(feature = "server")]

use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use indoc::indoc;
use reqwest::StatusCode;
use serde_json::{Value, json};
use similar::TextDiff;

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_org-cli"))
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let mut path = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        path.push(format!(
            "org-cli-heading-mutation-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
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

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct DaemonFixture {
    _child: ChildGuard,
    temp: TempDir,
    org: PathBuf,
    base: String,
    http: reqwest::blocking::Client,
}

impl DaemonFixture {
    fn start(tag: &str, content: &str) -> Self {
        let temp = TempDir::new(tag);
        let org = temp.path().join("indexed.org");
        std::fs::write(&org, content).unwrap();
        let db = temp.path().join("todos.sqlite");
        let port = free_port();
        let addr = format!("127.0.0.1:{port}");
        let child = Command::new(bin_path())
            .arg("daemon")
            .arg(&org)
            .arg("--addr")
            .arg(&addr)
            .arg("--db")
            .arg(&db)
            .spawn()
            .expect("spawn org-cli daemon");
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let fixture = Self {
            _child: ChildGuard(child),
            temp,
            org,
            base: format!("http://{addr}"),
            http,
        };
        fixture.wait_until_ready();
        fixture
    }

    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "daemon did not become ready");
            match self.http.get(format!("{}/api/todos", self.base)).send() {
                Ok(response) if response.status().is_success() => return,
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    fn snapshot(&self) -> (String, Vec<Value>) {
        let response = self
            .http
            .get(format!("{}/api/todos", self.base))
            .send()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let sub = response
            .headers()
            .get("x-sub-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let records = serde_json::from_str(&response.text().unwrap()).unwrap();
        (sub, records)
    }

    fn locator(&self) -> Locator {
        let (_, records) = self.snapshot();
        assert_eq!(records.len(), 1, "expected one TODO record: {records:?}");
        Locator::from_record(&records[0])
    }

    fn patch(
        &self,
        requested_path: &Path,
        locator: &Locator,
        if_match: Option<&str>,
        fields: Value,
    ) -> PatchResponse {
        send_patch(
            &self.http,
            &self.base,
            requested_path,
            locator,
            if_match,
            fields,
        )
    }
}

#[derive(Clone, Debug)]
struct Locator {
    path: String,
    line: u64,
    fingerprint: String,
    etag: String,
}

impl Locator {
    fn from_record(record: &Value) -> Self {
        Self {
            path: record["path"].as_str().unwrap().to_string(),
            line: record["line"].as_u64().unwrap(),
            fingerprint: record["fingerprint"].as_str().unwrap().to_string(),
            etag: record["etag"].as_str().unwrap().to_string(),
        }
    }

    fn from_patch_body(body: &Value) -> Self {
        let record = &body["todos"][0];
        Self {
            path: body["path"].as_str().unwrap().to_string(),
            line: record["line"].as_u64().unwrap(),
            fingerprint: record["fingerprint"].as_str().unwrap().to_string(),
            etag: body["etag"].as_str().unwrap().to_string(),
        }
    }
}

#[derive(Debug)]
struct PatchResponse {
    status: StatusCode,
    etag_header: Option<String>,
    body: Value,
}

fn send_patch(
    http: &reqwest::blocking::Client,
    base: &str,
    requested_path: &Path,
    locator: &Locator,
    if_match: Option<&str>,
    fields: Value,
) -> PatchResponse {
    let mut body = json!({
        "path": requested_path,
        "line": locator.line,
        "fingerprint": locator.fingerprint,
    });
    let body_object = body.as_object_mut().unwrap();
    for (key, value) in fields.as_object().unwrap() {
        body_object.insert(key.clone(), value.clone());
    }

    let mut request = http
        .patch(format!("{base}/api/heading"))
        .header("content-type", "application/json")
        .body(body.to_string());
    if let Some(value) = if_match {
        request = request.header("if-match", value);
    }
    let response = request.send().unwrap();
    let status = response.status();
    let etag_header = response
        .headers()
        .get("etag")
        .map(|value| value.to_str().unwrap().to_string());
    let text = response.text().unwrap();
    let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
    PatchResponse {
        status,
        etag_header,
        body,
    }
}

fn quoted(etag: &str) -> String {
    format!("\"{etag}\"")
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn assert_output_eq(actual: &str, expected: &str) {
    if actual != expected {
        let diff = TextDiff::from_lines(expected, actual);
        eprintln!();
        for line in diff
            .unified_diff()
            .header("expected", "actual")
            .to_string()
            .lines()
        {
            if line.starts_with('-') {
                eprintln!("\x1b[31m{line}\x1b[0m");
            } else if line.starts_with('+') {
                eprintln!("\x1b[32m{line}\x1b[0m");
            } else if line.starts_with('@') {
                eprintln!("\x1b[36m{line}\x1b[0m");
            } else {
                eprintln!("{line}");
            }
        }
        panic!("Output mismatch - see diff above");
    }
}

#[test]
fn patch_with_matching_etag_updates_only_target_heading_line() {
    let before = indoc! {"
        #+TODO: TODO WIP | DONE CANCELED
        Preface stays exactly here.
        * TODO [#B] First task :home:
        Body line stays untouched.
        * WIP Second task :work:
        Tail stays untouched.
    "};
    let after = indoc! {"
        #+TODO: TODO WIP | DONE CANCELED
        Preface stays exactly here.
        * DONE [#A] First task :home:
        Body line stays untouched.
        * WIP Second task :work:
        Tail stays untouched.
    "};
    let fixture = DaemonFixture::start("success", before);
    let (_, records) = fixture.snapshot();
    let locator = Locator::from_record(&records[0]);

    let response = fixture.patch(
        &fixture.org,
        &locator,
        Some(&quoted(&locator.etag)),
        json!({"priority": "A", "keyword": "DONE"}),
    );

    assert_eq!(response.status, StatusCode::OK);
    assert_output_eq(&std::fs::read_to_string(&fixture.org).unwrap(), after);
    assert_eq!(response.body["path"], locator.path);
    assert_eq!(response.body["todos"].as_array().unwrap().len(), 2);
    assert_eq!(response.body["todos"][0]["priority"], "A");
    assert_eq!(response.body["todos"][0]["keyword"], "DONE");
    assert_eq!(
        response.body["todo_keywords"],
        json!({"active": ["TODO", "WIP"], "done": ["DONE", "CANCELED"]})
    );
    assert_eq!(
        response.etag_header,
        Some(quoted(response.body["etag"].as_str().unwrap()))
    );
}

#[test]
fn stale_if_match_returns_authoritative_snapshot_without_writing() {
    let content = "* TODO [#B] Current\n* DONE Existing\n";
    let fixture = DaemonFixture::start("stale", content);
    let (_, records) = fixture.snapshot();
    let locator = Locator::from_record(&records[0]);

    let response = fixture.patch(
        &fixture.org,
        &locator,
        Some("\"stale-etag\""),
        json!({"priority": "A"}),
    );

    assert_eq!(response.status, StatusCode::PRECONDITION_FAILED);
    assert_output_eq(&std::fs::read_to_string(&fixture.org).unwrap(), content);
    assert_eq!(response.body["path"], locator.path);
    assert_eq!(response.body["etag"], locator.etag);
    assert_eq!(response.body["todos"].as_array().unwrap().len(), 2);
    assert_eq!(response.body["todos"][0]["priority"], "B");
    assert_eq!(response.etag_header, Some(quoted(&locator.etag)));
}

#[test]
fn empty_patch_is_unprocessable_without_writing() {
    let content = "* TODO Empty patch\n";
    let fixture = DaemonFixture::start("empty", content);
    let locator = fixture.locator();

    let response = fixture.patch(
        &fixture.org,
        &locator,
        Some(&quoted(&locator.etag)),
        json!({}),
    );

    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        response.body,
        Value::String("empty heading patch".to_string())
    );
    assert_output_eq(&std::fs::read_to_string(&fixture.org).unwrap(), content);
}

#[test]
fn mismatched_locator_is_conflict_without_writing() {
    let content = "* TODO Locator\n";
    let fixture = DaemonFixture::start("locator", content);
    let mut locator = fixture.locator();
    locator.fingerprint = "wrong-fingerprint".to_string();

    let response = fixture.patch(
        &fixture.org,
        &locator,
        Some(&quoted(&locator.etag)),
        json!({"priority": "A"}),
    );

    assert_eq!(response.status, StatusCode::CONFLICT);
    assert_output_eq(&std::fs::read_to_string(&fixture.org).unwrap(), content);
}

#[test]
fn missing_and_malformed_if_match_are_rejected_without_writing() {
    let content = "* TODO [#B] Untouched\n";
    let fixture = DaemonFixture::start("preconditions", content);
    let locator = fixture.locator();

    let missing = fixture.patch(&fixture.org, &locator, None, json!({"priority": "A"}));
    assert_eq!(missing.status, StatusCode::PRECONDITION_REQUIRED);
    assert_output_eq(&std::fs::read_to_string(&fixture.org).unwrap(), content);

    for malformed in ["*", "W/\"x\"", "a, b", ""] {
        let response = fixture.patch(
            &fixture.org,
            &locator,
            Some(malformed),
            json!({"priority": "A"}),
        );
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "If-Match {malformed:?}"
        );
        assert_output_eq(&std::fs::read_to_string(&fixture.org).unwrap(), content);
    }
}

#[test]
fn canonical_membership_accepts_dotdot_and_symlink_but_rejects_unindexed_file() {
    let fixture = DaemonFixture::start("membership", "* TODO [#C] Indexed\n");
    let locator = fixture.locator();
    let outside = fixture.temp.path().join("outside.org");
    std::fs::write(&outside, "* TODO Outside\n").unwrap();

    let rejected = fixture.patch(
        &outside,
        &locator,
        Some(&quoted(&locator.etag)),
        json!({"priority": "A"}),
    );
    assert_eq!(rejected.status, StatusCode::NOT_FOUND);
    assert_output_eq(
        &std::fs::read_to_string(&outside).unwrap(),
        "* TODO Outside\n",
    );
    assert_output_eq(
        &std::fs::read_to_string(&fixture.org).unwrap(),
        "* TODO [#C] Indexed\n",
    );

    let unindexed_dir = fixture.temp.path().join("unindexed-dir");
    std::fs::create_dir(&unindexed_dir).unwrap();
    let traversing = unindexed_dir.join("..").join("indexed.org");
    let traversed = fixture.patch(
        &traversing,
        &locator,
        Some(&quoted(&locator.etag)),
        json!({"priority": "A"}),
    );
    assert_eq!(traversed.status, StatusCode::OK);
    assert_eq!(traversed.body["path"], locator.path);
    assert_output_eq(
        &std::fs::read_to_string(&fixture.org).unwrap(),
        "* TODO [#A] Indexed\n",
    );

    let link = fixture.temp.path().join("alias.org");
    std::os::unix::fs::symlink("indexed.org", &link).unwrap();
    let current = Locator::from_patch_body(&traversed.body);
    let linked = fixture.patch(
        &link,
        &current,
        Some(&quoted(&current.etag)),
        json!({"priority": "B"}),
    );
    assert_eq!(linked.status, StatusCode::OK);
    assert_eq!(linked.body["path"], locator.path);
    assert_output_eq(
        &std::fs::read_to_string(&fixture.org).unwrap(),
        "* TODO [#B] Indexed\n",
    );
}

#[test]
fn concurrent_same_file_patches_serialize_with_one_winner() {
    let initial = "* TODO Concurrent\n";
    let fixture = DaemonFixture::start("concurrent", initial);
    let locator = fixture.locator();
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();

    for priority in ["A", "B"] {
        let http = fixture.http.clone();
        let base = fixture.base.clone();
        let path = fixture.org.clone();
        let locator = locator.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let response = send_patch(
                &http,
                &base,
                &path,
                &locator,
                Some(&quoted(&locator.etag)),
                json!({"priority": priority}),
            );
            (priority, response)
        }));
    }

    barrier.wait();
    let results: Vec<(&str, PatchResponse)> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let mut statuses: Vec<u16> = results
        .iter()
        .map(|(_, response)| response.status.as_u16())
        .collect();
    statuses.sort_unstable();
    assert_eq!(statuses, vec![200, 412]);

    let (winning_priority, winning_response) = results
        .iter()
        .find(|(_, response)| response.status == StatusCode::OK)
        .unwrap();
    let expected = format!("* TODO [#{winning_priority}] Concurrent\n");
    assert_output_eq(&std::fs::read_to_string(&fixture.org).unwrap(), &expected);
    assert_eq!(
        winning_response.body["todos"][0]["priority"],
        *winning_priority
    );
    assert_eq!(
        winning_response.body["etag"],
        winning_response.body["todos"][0]["etag"]
    );
}

#[test]
fn omitted_patch_fields_preserve_keyword_priority_and_tags() {
    let fixture = DaemonFixture::start(
        "omitted",
        "#+TODO: TODO WIP | DONE\n* WIP [#B] Keep anatomy :one:two:\n",
    );
    let locator = fixture.locator();

    let priority_only = fixture.patch(
        &fixture.org,
        &locator,
        Some(&quoted(&locator.etag)),
        json!({"priority": "A"}),
    );
    assert_eq!(priority_only.status, StatusCode::OK);
    assert_output_eq(
        &std::fs::read_to_string(&fixture.org).unwrap(),
        "#+TODO: TODO WIP | DONE\n* WIP [#A] Keep anatomy :one:two:\n",
    );

    let current = Locator::from_patch_body(&priority_only.body);
    let keyword_only = fixture.patch(
        &fixture.org,
        &current,
        Some(&quoted(&current.etag)),
        json!({"keyword": "DONE"}),
    );
    assert_eq!(keyword_only.status, StatusCode::OK);
    assert_output_eq(
        &std::fs::read_to_string(&fixture.org).unwrap(),
        "#+TODO: TODO WIP | DONE\n* DONE [#A] Keep anatomy :one:two:\n",
    );
}

#[test]
fn priority_patch_supports_set_null_none_and_rejects_invalid() {
    let fixture = DaemonFixture::start("priority", "* TODO Task\n");
    let mut locator = fixture.locator();

    for priority in ["A", "B", "C"] {
        let response = fixture.patch(
            &fixture.org,
            &locator,
            Some(&quoted(&locator.etag)),
            json!({"priority": priority}),
        );
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body["todos"][0]["priority"], priority);
        locator = Locator::from_patch_body(&response.body);
    }

    for removal in [Value::Null, Value::String("none".to_string())] {
        let response = fixture.patch(
            &fixture.org,
            &locator,
            Some(&quoted(&locator.etag)),
            json!({"priority": removal}),
        );
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body["todos"][0]["priority"], Value::Null);
        locator = Locator::from_patch_body(&response.body);

        let reset = fixture.patch(
            &fixture.org,
            &locator,
            Some(&quoted(&locator.etag)),
            json!({"priority": "A"}),
        );
        assert_eq!(reset.status, StatusCode::OK);
        locator = Locator::from_patch_body(&reset.body);
    }

    let before_invalid = std::fs::read_to_string(&fixture.org).unwrap();
    let invalid = fixture.patch(
        &fixture.org,
        &locator,
        Some(&quoted(&locator.etag)),
        json!({"priority": "D"}),
    );
    assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        invalid.body,
        Value::String("invalid priority \"D\"; expected A, B, or C".to_string())
    );
    assert_output_eq(
        &std::fs::read_to_string(&fixture.org).unwrap(),
        &before_invalid,
    );
}

#[test]
fn removing_keyword_returns_file_wide_todos_without_heading() {
    let fixture = DaemonFixture::start("remove-keyword", "* TODO Remove me\n* DONE Keep me\n");
    let (_, records) = fixture.snapshot();
    let locator = Locator::from_record(&records[0]);

    let response = fixture.patch(
        &fixture.org,
        &locator,
        Some(&quoted(&locator.etag)),
        json!({"keyword": null}),
    );

    assert_eq!(response.status, StatusCode::OK);
    assert_output_eq(
        &std::fs::read_to_string(&fixture.org).unwrap(),
        "* Remove me\n* DONE Keep me\n",
    );
    let todos = response.body["todos"].as_array().unwrap();
    assert_eq!(todos.len(), 1);
    assert_eq!(todos[0]["title"], "Keep me");
}

#[test]
fn no_op_patch_preserves_etag_inode_and_mtime() {
    let content = "* TODO [#A] Already set\n";
    let fixture = DaemonFixture::start("noop", content);
    let locator = fixture.locator();
    let before = std::fs::metadata(&fixture.org).unwrap();

    let response = fixture.patch(
        &fixture.org,
        &locator,
        Some(&quoted(&locator.etag)),
        json!({"priority": "A"}),
    );

    let after = std::fs::metadata(&fixture.org).unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["etag"], locator.etag);
    assert_eq!(response.etag_header, Some(quoted(&locator.etag)));
    assert_eq!(before.ino(), after.ino());
    assert_eq!(
        (before.mtime(), before.mtime_nsec()),
        (after.mtime(), after.mtime_nsec())
    );
    assert_output_eq(&std::fs::read_to_string(&fixture.org).unwrap(), content);
}

#[test]
fn crlf_patch_is_refused_without_writing() {
    let content = "* TODO [#B] CRLF\r\nBody\r\n";
    let fixture = DaemonFixture::start("crlf", content);
    let locator = fixture.locator();

    let response = fixture.patch(
        &fixture.org,
        &locator,
        Some(&quoted(&locator.etag)),
        json!({"priority": "A"}),
    );

    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        response.body,
        Value::String(
            "unsupported CRLF line ending at line 1; heading edits accept LF-only text".to_string()
        )
    );
    assert_eq!(std::fs::read(&fixture.org).unwrap(), content.as_bytes());
}

#[test]
fn daemon_patch_round_trips_through_sse() {
    let fixture = DaemonFixture::start("sse", "* TODO Round trip\n");
    let (sub_id, records) = fixture.snapshot();
    let locator = Locator::from_record(&records[0]);
    let sse_client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let sse_response = sse_client
        .get(format!("{}/sse/todos?sub={sub_id}", fixture.base))
        .send()
        .unwrap();
    assert_eq!(sse_response.status(), StatusCode::OK);

    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(SseBody(sse_response));
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    let _ = event_tx.send(None);
                    return;
                }
                Ok(_) if line.starts_with("data:") && line.contains("file-changed") => {
                    let json = line.trim_start_matches("data:").trim();
                    let _ = event_tx.send(serde_json::from_str::<Value>(json).ok());
                    return;
                }
                Ok(_) => {}
            }
        }
    });

    std::thread::sleep(Duration::from_millis(100));
    let patched = fixture.patch(
        &fixture.org,
        &locator,
        Some(&quoted(&locator.etag)),
        json!({"priority": "A"}),
    );
    assert_eq!(patched.status, StatusCode::OK);

    let event = event_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("file-changed event within timeout")
        .expect("valid file-changed JSON");
    assert_eq!(event["type"], "file-changed");
    assert_eq!(event["path"], locator.path);
    assert_eq!(event["etag"], patched.body["etag"]);
    assert_eq!(event["todos"][0]["priority"], "A");
    assert_output_eq(
        &std::fs::read_to_string(&fixture.org).unwrap(),
        "* TODO [#A] Round trip\n",
    );
    reader.join().unwrap();
}

struct SseBody(reqwest::blocking::Response);

impl Read for SseBody {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buffer)
    }
}
