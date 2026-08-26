//! Integration tests for the `todos` subcommand: spawn the real
//! binary, sync a fixture directory into a temp sqlite index, and
//! assert on the three output modes plus sync semantics.

use std::path::{Path, PathBuf};
use std::process::Command;

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
        p.push(format!("org-cli-todos-it-{}-{}-{}", tag, pid, nanos));
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

fn write(p: &Path, s: &str) {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, s).unwrap();
}

/// Run `todos` against `dir`, indexing into `db`. Extra args are
/// appended (e.g. `--search`, `-j`).
fn run_todos(dir: &Path, db: &Path, extra: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(bin_path());
    cmd.arg("todos").arg(dir).arg("--db").arg(db);
    for a in extra {
        cmd.arg(a);
    }
    cmd.output().expect("spawn org-cli")
}

/// Collect the `title` field of every NDJSON record on stdout.
fn titles_of(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["title"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

/// A small fixture tree with a mix of keyworded and plain headings.
fn make_fixture(td: &TempDir) -> PathBuf {
    let dir = td.path().join("org");
    write(
        &dir.join("a.org"),
        "* TODO Buy milk :home:\n* Plain heading\n* DONE Ship it\n",
    );
    write(
        &dir.join("b.org"),
        "* TODO Write report\n** TODO Draft outline\n",
    );
    dir
}

#[test]
fn todos_sync_and_list_json_shape() {
    let td = TempDir::new("json");
    let dir = make_fixture(&td);
    let db = td.path().join("todos.sqlite");

    let out = run_todos(&dir, &db, &["-j"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let records: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // Only keyworded headings, ordered by path then line.
    assert_eq!(
        titles_of(&stdout),
        vec![
            "Buy milk".to_string(),
            "Ship it".to_string(),
            "Write report".to_string(),
            "Draft outline".to_string(),
        ]
    );

    let first = &records[0];
    assert_eq!(
        first["path"],
        dir.join("a.org").to_string_lossy().to_string()
    );
    assert_eq!(first["line"], 1);
    assert_eq!(first["level"], 1);
    assert_eq!(first["keyword"], "TODO");
    assert_eq!(first["title"], "Buy milk");
    assert_eq!(first["tags"], serde_json::json!(["home"]));
    assert_eq!(first["is_done"], false);

    let ship = records.iter().find(|r| r["title"] == "Ship it").unwrap();
    assert_eq!(ship["keyword"], "DONE");
    assert_eq!(ship["is_done"], true);
}

#[test]
fn todos_search_filters_title_substring() {
    let td = TempDir::new("search");
    let dir = make_fixture(&td);
    let db = td.path().join("todos.sqlite");

    let out = run_todos(&dir, &db, &["--search", "report", "-j"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(titles_of(&stdout), vec!["Write report".to_string()]);
}

#[test]
fn todos_keyword_filters_exact_match() {
    let td = TempDir::new("keyword");
    let dir = make_fixture(&td);
    let db = td.path().join("todos.sqlite");

    let out = run_todos(&dir, &db, &["--keyword", "DONE", "-j"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(titles_of(&stdout), vec!["Ship it".to_string()]);
}

#[test]
fn todos_second_run_unchanged_returns_same_results() {
    let td = TempDir::new("idempotent");
    let dir = make_fixture(&td);
    let db = td.path().join("todos.sqlite");

    let first = run_todos(&dir, &db, &["-j"]);
    assert_eq!(first.status.code(), Some(0));
    let first_out = String::from_utf8(first.stdout).unwrap();

    // Second run: files unchanged → identical query results.
    let second = run_todos(&dir, &db, &["-j"]);
    assert_eq!(second.status.code(), Some(0));
    let second_out = String::from_utf8(second.stdout).unwrap();

    assert_eq!(first_out, second_out);
    assert_eq!(titles_of(&second_out).len(), 4);
}

#[test]
fn todos_no_sync_on_fresh_db_returns_empty() {
    let td = TempDir::new("no-sync");
    let dir = make_fixture(&td);
    let db = td.path().join("fresh.sqlite");

    // --no-sync on a brand-new db: the index is empty, so no rows.
    let out = run_todos(&dir, &db, &["--no-sync", "-j"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "stdout should be empty, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn todos_versioned_formats_match_exact_bytes_and_priority_placement() {
    const ETAG: &str = "733410184441c09822f04a5d3aaf3c4262ea3c26810a5d26033097cd05e1a1f9";
    const FINGERPRINT: &str = "13a8ef44885266cff793163ac7bfecab5d5fec09bf28839dcabe368d31955789";

    let td = TempDir::new("versioned-formats");
    let file = td.path().join("versioned.org");
    write(
        &file,
        "#+TODO: TODO WIP | DONE CANCELED\n* WIP [#A] Buy milk :home:\n",
    );
    let db = td.path().join("todos.sqlite");

    let json = run_todos(&file, &db, &["-j"]);
    assert_eq!(json.status.code(), Some(0));
    let expected_json = format!(
        "{{\"path\":\"{}\",\"line\":2,\"level\":1,\"keyword\":\"WIP\",\"title\":\"Buy milk\",\"tags\":[\"home\"],\"is_done\":false,\"priority\":\"A\",\"fingerprint\":\"{FINGERPRINT}\",\"etag\":\"{ETAG}\",\"todo_keywords\":{{\"active\":[\"TODO\",\"WIP\"],\"done\":[\"DONE\",\"CANCELED\"]}}}}\n",
        file.display()
    );
    assert_eq!(String::from_utf8(json.stdout).unwrap(), expected_json);

    let nul = run_todos(&file, &db, &["-0"]);
    assert_eq!(nul.status.code(), Some(0));
    let expected_nul = format!(
        "{}\02\01\0WIP\0Buy milk\0home\00\0A\0{FINGERPRINT}\0{ETAG}\0{{\"active\":[\"TODO\",\"WIP\"],\"done\":[\"DONE\",\"CANCELED\"]}}\0\0",
        file.display()
    )
    .into_bytes();
    assert_eq!(nul.stdout, expected_nul);

    let human = Command::new(bin_path())
        .arg("todos")
        .arg(&file)
        .arg("--db")
        .arg(&db)
        .output()
        .expect("spawn org-cli");
    assert_eq!(human.status.code(), Some(0));
    let expected_human = format!("{}:2: WIP [#A] Buy milk :home:\n", file.display());
    assert_eq!(String::from_utf8(human.stdout).unwrap(), expected_human);
}
