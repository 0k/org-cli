//! Integration tests for `GET /api/heading` on the real daemon binary.
//!
//! The detail endpoint exists so heading bodies stay *out* of the hot
//! `/api/todos` snapshot and its SSE stream. These tests pin the two
//! halves of that bargain: the snapshot must not grow a body field, and
//! the on-demand read must return the heading's own bytes plus the AST
//! shape the shared renderer consumes.

#![cfg(feature = "server")]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use indoc::indoc;
use reqwest::StatusCode;
use serde_json::Value;
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
            "org-cli-heading-detail-{tag}-{}-{nanos}",
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

    fn records(&self) -> Vec<Value> {
        let response = self
            .http
            .get(format!("{}/api/todos", self.base))
            .send()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_str(&response.text().unwrap()).unwrap()
    }

    fn detail(&self, requested_path: &Path, line: u64, fingerprint: &str) -> DetailResponse {
        let response = self
            .http
            .get(format!("{}/api/heading", self.base))
            .query(&[
                ("path", requested_path.to_string_lossy().to_string()),
                ("line", line.to_string()),
                ("fingerprint", fingerprint.to_string()),
            ])
            .send()
            .unwrap();
        let status = response.status();
        let etag_header = response
            .headers()
            .get("etag")
            .map(|value| value.to_str().unwrap().to_string());
        let text = response.text().unwrap();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        DetailResponse {
            status,
            etag_header,
            body,
        }
    }

    /// Fetch the detail for the record at `index` in the snapshot.
    fn detail_of_record(&self, index: usize) -> (Value, DetailResponse) {
        let records = self.records();
        let record = records[index].clone();
        let detail = self.detail(
            &self.org,
            record["line"].as_u64().unwrap(),
            record["fingerprint"].as_str().unwrap(),
        );
        (record, detail)
    }
}

#[derive(Debug)]
struct DetailResponse {
    status: StatusCode,
    etag_header: Option<String>,
    body: Value,
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

/// Every node in an AST tree, depth-first.
fn all_nodes(value: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    fn visit<'a>(value: &'a Value, out: &mut Vec<&'a Value>) {
        match value {
            Value::Array(items) => items.iter().for_each(|item| visit(item, out)),
            Value::Object(map) => {
                if map.contains_key("type") && map.contains_key("source") {
                    out.push(value);
                }
                map.values().for_each(|item| visit(item, out));
            }
            _ => {}
        }
    }
    visit(value, &mut out);
    out
}

fn nodes_of_type<'a>(value: &'a Value, kind: &str) -> Vec<&'a Value> {
    all_nodes(value)
        .into_iter()
        .filter(|node| node["type"] == kind)
        .collect()
}

const RICH: &str = indoc! {"
    #+TODO: TODO WIP | DONE CANCELED
    Preface belongs to no heading.
    * TODO [#B] First task :home:
    :PROPERTIES:
    :EFFORT: 2:00
    :END:
    :LOGBOOK:
    CLOCK: [2026-01-07 Wed 09:57:20 +0100]--[2026-01-07 Wed 12:19:25 +0100] =>  2:22:05
    :END:
    Body with a [[file:/tmp/x.org][link]], ~code~ and =verbatim=.

    - [ ] a checkbox item
    ** WIP Child task
    child body
    * DONE Second task
    tail body
"};

#[test]
fn detail_returns_the_headings_own_bytes_and_the_shared_ast_shape() {
    let fixture = DaemonFixture::start("rich", RICH);
    let (record, detail) = fixture.detail_of_record(0);

    assert_eq!(detail.status, StatusCode::OK);
    // Identity echoed back: the daemon's indexed path, not the client's
    // spelling, and the exact locator that was matched.
    assert_eq!(detail.body["path"], record["path"]);
    assert_eq!(detail.body["line"], record["line"]);
    assert_eq!(detail.body["fingerprint"], record["fingerprint"]);
    // The read is versioned by the same content hash the snapshot used.
    assert_eq!(detail.body["etag"], record["etag"]);
    assert_eq!(
        detail.etag_header,
        Some(quoted(record["etag"].as_str().unwrap()))
    );

    // The body is the heading's own lines, verbatim: heading line
    // excluded, descendants included, preface and siblings excluded.
    assert_output_eq(
        detail.body["body"].as_str().unwrap(),
        indoc! {"
            :PROPERTIES:
            :EFFORT: 2:00
            :END:
            :LOGBOOK:
            CLOCK: [2026-01-07 Wed 09:57:20 +0100]--[2026-01-07 Wed 12:19:25 +0100] =>  2:22:05
            :END:
            Body with a [[file:/tmp/x.org][link]], ~code~ and =verbatim=.

            - [ ] a checkbox item
            ** WIP Child task
            child body
        "},
    );
    // And it really is a substring of the file, not a re-serialization.
    assert!(RICH.contains(detail.body["body"].as_str().unwrap()));

    let ast = &detail.body["ast"];
    assert_eq!(ast["type"], "document");

    // The clock is the fact the patched `orgize` fork exists to deliver:
    // unpatched, a numerical UTC offset fails to parse and the CLOCK
    // line silently degrades to a paragraph.
    let clocks = nodes_of_type(ast, "clock");
    assert_eq!(
        clocks.len(),
        1,
        "zero clocks means org-cli is building against unpatched orgize"
    );
    assert_eq!(clocks[0]["start"]["second"], "20");
    assert_eq!(clocks[0]["start"]["tz_offset"], "+0100");
    assert_eq!(clocks[0]["duration"], "2:22:05");
    assert_eq!(clocks[0]["running"], false);

    assert_eq!(nodes_of_type(ast, "link").len(), 1);
    assert_eq!(nodes_of_type(ast, "code").len(), 1);
    assert_eq!(nodes_of_type(ast, "verbatim").len(), 1);
    assert_eq!(nodes_of_type(ast, "list-item").len(), 1);
    assert_eq!(nodes_of_type(ast, "property-drawer").len(), 1);
    assert_eq!(nodes_of_type(ast, "node-property").len(), 1);
    // The descendant heading is present as a headline, not flattened.
    let headlines = nodes_of_type(ast, "headline");
    assert_eq!(headlines.len(), 1);
    assert_eq!(headlines[0]["title_raw"], "WIP Child task");

    // Every node carries a well-formed range into `body`.
    let body_len = detail.body["body"].as_str().unwrap().len() as u64;
    for node in all_nodes(ast) {
        let start = node["source"]["start"].as_u64().unwrap();
        let end = node["source"]["end"].as_u64().unwrap();
        assert!(start <= end, "inverted range on {}", node["type"]);
        assert!(end <= body_len, "range past body end on {}", node["type"]);
    }
}

#[test]
fn a_heading_without_content_reports_an_empty_body_and_no_ast() {
    let fixture = DaemonFixture::start("empty", "* TODO Bare\n* DONE Next\n");
    let (_, detail) = fixture.detail_of_record(0);

    assert_eq!(detail.status, StatusCode::OK);
    assert_eq!(detail.body["body"], "");
    // `null`, not an empty document: a client can tell "nothing to show"
    // from "a document that rendered to nothing".
    assert_eq!(detail.body["ast"], Value::Null);
}

#[test]
fn the_hot_snapshot_still_carries_no_body_or_ast() {
    // The whole reason this endpoint exists. A regression that folds
    // bodies into the snapshot would be invisible to every other test.
    let fixture = DaemonFixture::start("hot-path", RICH);

    for record in fixture.records() {
        let object = record.as_object().unwrap();
        assert!(
            !object.contains_key("body") && !object.contains_key("ast"),
            "snapshot record grew a body/ast field: {record}"
        );
    }
}

#[test]
fn a_stale_locator_is_a_conflict_rather_than_the_wrong_heading() {
    let fixture = DaemonFixture::start("stale", "* TODO [#B] Original\nbody\n");
    let records = fixture.records();
    let line = records[0]["line"].as_u64().unwrap();

    // Right line, wrong heading: exactly the case where returning the
    // body anyway would silently show a reader someone else's task.
    let stale = fixture.detail(&fixture.org, line, "0".repeat(64).as_str());
    assert_eq!(stale.status, StatusCode::CONFLICT);

    // Right fingerprint, wrong line.
    let fingerprint = records[0]["fingerprint"].as_str().unwrap();
    let displaced = fixture.detail(&fixture.org, line + 1, fingerprint);
    assert_eq!(displaced.status, StatusCode::CONFLICT);

    // And after a real edit, the old fingerprint stops resolving.
    std::fs::write(&fixture.org, "* TODO [#A] Original\nbody\n").unwrap();
    let after_edit = fixture.detail(&fixture.org, line, fingerprint);
    assert_eq!(after_edit.status, StatusCode::CONFLICT);
}

#[test]
fn detail_reads_are_confined_to_indexed_files() {
    let fixture = DaemonFixture::start("membership", "* TODO [#C] Indexed\nbody\n");
    let records = fixture.records();
    let line = records[0]["line"].as_u64().unwrap();
    let fingerprint = records[0]["fingerprint"].as_str().unwrap();

    // A readable file the daemon does not index is not readable through
    // this endpoint: it is an org-file reader, not a file server.
    let outside = fixture.temp.path().join("outside.org");
    std::fs::write(&outside, "* TODO [#C] Indexed\nsecret\n").unwrap();
    assert_eq!(
        fixture.detail(&outside, line, fingerprint).status,
        StatusCode::NOT_FOUND
    );

    assert_eq!(
        fixture
            .detail(Path::new("/nonexistent/nope.org"), line, fingerprint)
            .status,
        StatusCode::NOT_FOUND
    );

    // Canonicalisation still admits an equivalent spelling of an
    // indexed file, matching the mutation endpoint.
    let unindexed_dir = fixture.temp.path().join("unindexed-dir");
    std::fs::create_dir(&unindexed_dir).unwrap();
    let traversing = unindexed_dir.join("..").join("indexed.org");
    let traversed = fixture.detail(&traversing, line, fingerprint);
    assert_eq!(traversed.status, StatusCode::OK);
    assert_eq!(traversed.body["path"], records[0]["path"]);
    assert_eq!(traversed.body["body"], "body\n");

    let link = fixture.temp.path().join("alias.org");
    std::os::unix::fs::symlink("indexed.org", &link).unwrap();
    let linked = fixture.detail(&link, line, fingerprint);
    assert_eq!(linked.status, StatusCode::OK);
    assert_eq!(linked.body["path"], records[0]["path"]);
}

#[test]
fn reading_a_heading_never_writes_the_file() {
    let content = "* TODO [#B] Read me\nbody\n";
    let fixture = DaemonFixture::start("readonly", content);
    let before = std::fs::metadata(&fixture.org).unwrap();

    let (_, detail) = fixture.detail_of_record(0);
    assert_eq!(detail.status, StatusCode::OK);

    let after = std::fs::metadata(&fixture.org).unwrap();
    assert_output_eq(&std::fs::read_to_string(&fixture.org).unwrap(), content);
    // Atomic replacement would change the inode; a plain write would
    // change the length. Neither may happen on a read.
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(&before),
        std::os::unix::fs::MetadataExt::ino(&after)
    );
    assert_eq!(before.len(), after.len());
}
