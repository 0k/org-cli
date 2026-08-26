use std::path::{Path, PathBuf};
use std::process::Command;

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_org-cli"))
}

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let mut path = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or(0);
        path.push(format!(
            "org-cli-headings-it-{tag}-{}-{nanos}",
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

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("task-ref.org")
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
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

#[test]
fn headings_has_property_returns_expected_set() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("--has-property")
        .arg("TASK_REF")
        .arg("-j")
        .arg(&fixture)
        .output()
        .expect("spawn org-cli");

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "stdout was:\n{}", stdout);
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();

    assert_eq!(first["file"], fixture.to_string_lossy().to_string());
    assert_eq!(first["level"], 1);
    assert_eq!(first["line"], 1);
    assert_eq!(first["title"], "TODO Billing task");
    assert_eq!(first["tags"], serde_json::json!(["urgent"]));
    assert_eq!(
        first["properties"],
        serde_json::json!({"CUSTOM_ID":"bill-248","TASK_REF":"248"})
    );

    assert_eq!(second["level"], 2);
    assert_eq!(second["line"], 15);
    assert_eq!(second["title"], "TODO Nested task");
    assert_eq!(second["tags"], serde_json::json!([]));
    assert_eq!(second["properties"], serde_json::json!({"TASK_REF":"314"}));
}

#[test]
fn headings_filter_ignore_case_returns_expected_set() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("-f")
        .arg("todo")
        .arg("-i")
        .arg("-j")
        .arg(&fixture)
        .output()
        .expect("spawn org-cli");

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let titles: Vec<String> = stdout
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["title"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        titles,
        vec![
            "TODO Billing task".to_string(),
            "TODO Nested task".to_string()
        ]
    );
}

#[test]
fn headings_combined_filter_and_property_is_anded() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("-f")
        .arg("billing")
        .arg("-i")
        .arg("--has-property")
        .arg("TASK_REF")
        .arg("-j")
        .arg(&fixture)
        .output()
        .expect("spawn org-cli");

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "stdout was:\n{}", stdout);
    let line: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(line["title"], "TODO Billing task");
}

#[test]
fn headings_nonexistent_property_returns_empty() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("--has-property")
        .arg("NONEXISTENT")
        .arg(&fixture)
        .output()
        .expect("spawn org-cli");

    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stdout.is_empty(),
        "stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn headings_json_output_matches_expected_bytes() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("--has-property")
        .arg("TASK_REF")
        .arg("-j")
        .arg(&fixture)
        .output()
        .expect("spawn org-cli");

    assert_eq!(out.status.code(), Some(0));
    let actual = String::from_utf8(out.stdout).unwrap();
    let expected = format!(
        concat!(
            "{{\"file\":\"{}\",\"level\":1,\"line\":1,\"keyword\":\"TODO\",\"done\":false,\"title\":\"TODO Billing task\",\"tags\":[\"urgent\"],\"properties\":{{\"CUSTOM_ID\":\"bill-248\",\"TASK_REF\":\"248\"}},\"priority\":null,\"fingerprint\":\"e9191080a819d962d2462483ff76f169f411864cbbdf29caa63ff14837999699\",\"etag\":\"fd65efd2c9dc107b6becaecb687a566413d29aee33ca457e234dcb59b1d5400b\",\"todo_keywords\":{{\"active\":[\"TODO\"],\"done\":[\"DONE\"]}}}}\n",
            "{{\"file\":\"{}\",\"level\":2,\"line\":15,\"keyword\":\"TODO\",\"done\":false,\"title\":\"TODO Nested task\",\"tags\":[],\"properties\":{{\"TASK_REF\":\"314\"}},\"priority\":null,\"fingerprint\":\"1ab6c5c7142ef465aa548053748ba2cea1cc975952c3635f1c01b54bf166995a\",\"etag\":\"fd65efd2c9dc107b6becaecb687a566413d29aee33ca457e234dcb59b1d5400b\",\"todo_keywords\":{{\"active\":[\"TODO\"],\"done\":[\"DONE\"]}}}}\n"
        ),
        fixture.display(),
        fixture.display()
    );
    assert_eq!(actual, expected);
}

#[test]
fn headings_todo_flag_keeps_only_keyworded_headings() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("--todo")
        .arg("-j")
        .arg(&fixture)
        .output()
        .expect("spawn org-cli");

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let keywords: Vec<String> = stdout
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["keyword"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        keywords,
        vec!["TODO".to_string(), "TODO".to_string(), "DONE".to_string()]
    );
}

#[test]
fn headings_keyword_flag_exact_matches_only() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("--keyword")
        .arg("DONE")
        .arg("-j")
        .arg(&fixture)
        .output()
        .expect("spawn org-cli");

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "stdout was:\n{}", stdout);
    let record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(record["keyword"], "DONE");
    assert_eq!(record["title"], "Archived task");
}

#[test]
fn headings_keyword_flag_is_repeatable() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("--keyword")
        .arg("TODO")
        .arg("--keyword")
        .arg("DONE")
        .arg("-j")
        .arg(&fixture)
        .output()
        .expect("spawn org-cli");

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let keywords: Vec<String> = stdout
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["keyword"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        keywords,
        vec!["TODO".to_string(), "TODO".to_string(), "DONE".to_string()]
    );
}

#[test]
fn headings_nul_output_matches_expected_bytes() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("--has-property")
        .arg("TASK_REF")
        .arg("-0")
        .arg(&fixture)
        .output()
        .expect("spawn org-cli");

    assert_eq!(out.status.code(), Some(0));
    let expected = format!(
        "1\0{}\0{}\0TODO\00\0TODO Billing task\0urgent\0\0e9191080a819d962d2462483ff76f169f411864cbbdf29caa63ff14837999699\0fd65efd2c9dc107b6becaecb687a566413d29aee33ca457e234dcb59b1d5400b\0{{\"active\":[\"TODO\"],\"done\":[\"DONE\"]}}\0\02\0{}\0{}\0TODO\00\0TODO Nested task\0\0\01ab6c5c7142ef465aa548053748ba2cea1cc975952c3635f1c01b54bf166995a\0fd65efd2c9dc107b6becaecb687a566413d29aee33ca457e234dcb59b1d5400b\0{{\"active\":[\"TODO\"],\"done\":[\"DONE\"]}}\0\0",
        1,
        fixture.display(),
        15,
        fixture.display()
    )
    .into_bytes();
    assert_eq!(out.stdout, expected);
}

#[test]
fn headings_in_buffer_declaration_recognizes_custom_keywords() {
    let file = fixture("custom-todo.org");
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("--todo")
        .arg("-j")
        .arg(&file)
        .output()
        .expect("spawn org-cli");

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let records: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    // --todo keeps only the three keyworded headings (WIP/CANCELED are
    // now recognized thanks to the in-buffer #+TODO: declaration).
    let keywords: Vec<&str> = records
        .iter()
        .map(|r| r["keyword"].as_str().unwrap())
        .collect();
    assert_eq!(keywords, vec!["TODO", "WIP", "CANCELED"]);

    // done reflects the file's effective done set (DONE, CANCELED).
    let by_keyword = |kw: &str| -> &serde_json::Value {
        records
            .iter()
            .find(|r| r["keyword"] == kw)
            .unwrap_or_else(|| panic!("no record with keyword {kw}"))
    };
    assert_eq!(by_keyword("WIP")["done"], serde_json::json!(false));
    assert_eq!(by_keyword("CANCELED")["done"], serde_json::json!(true));
    assert_eq!(by_keyword("TODO")["done"], serde_json::json!(false));
}

#[test]
fn headings_keyword_flag_exact_matches_custom_keyword() {
    let file = fixture("custom-todo.org");
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("--keyword")
        .arg("WIP")
        .arg("-j")
        .arg(&file)
        .output()
        .expect("spawn org-cli");

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "stdout was:\n{}", stdout);
    let record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(record["keyword"], "WIP");
    assert_eq!(record["title"], "Implement feature");
    assert_eq!(record["done"], serde_json::json!(false));
}

#[test]
fn headings_todo_keywords_flag_recognizes_custom_keyword() {
    let file = fixture("no-todo-decl.org");
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("--todo-keywords")
        .arg("TODO WIP | DONE")
        .arg("--todo")
        .arg("-j")
        .arg(&file)
        .output()
        .expect("spawn org-cli");

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let records: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        titles_of(&stdout),
        vec![
            "Plan the sprint".to_string(),
            "Implement feature".to_string()
        ]
    );
    assert!(records.iter().all(|record| {
        record["todo_keywords"] == serde_json::json!({"active":["TODO","WIP"],"done":["DONE"]})
    }));
    let wip = records
        .iter()
        .find(|record| record["keyword"] == "WIP")
        .expect("custom CLI fallback must parse WIP");
    assert_eq!(
        wip["todo_keywords"]["active"],
        serde_json::json!(["TODO", "WIP"])
    );
}

#[test]
fn headings_versioned_formats_match_exact_bytes_and_priority_placement() {
    const ETAG: &str = "733410184441c09822f04a5d3aaf3c4262ea3c26810a5d26033097cd05e1a1f9";
    const FINGERPRINT: &str = "13a8ef44885266cff793163ac7bfecab5d5fec09bf28839dcabe368d31955789";

    let td = TempDir::new("versioned-formats");
    let file = td.path().join("versioned.org");
    std::fs::write(
        &file,
        "#+TODO: TODO WIP | DONE CANCELED\n* WIP [#A] Buy milk :home:\n",
    )
    .unwrap();

    let json = Command::new(bin_path())
        .arg("headings")
        .arg("-j")
        .arg(&file)
        .output()
        .expect("spawn org-cli");
    assert_eq!(json.status.code(), Some(0));
    let expected_json = format!(
        "{{\"file\":\"{}\",\"level\":1,\"line\":2,\"keyword\":\"WIP\",\"done\":false,\"title\":\"Buy milk\",\"tags\":[\"home\"],\"properties\":{{}},\"priority\":\"A\",\"fingerprint\":\"{FINGERPRINT}\",\"etag\":\"{ETAG}\",\"todo_keywords\":{{\"active\":[\"TODO\",\"WIP\"],\"done\":[\"DONE\",\"CANCELED\"]}}}}\n",
        file.display()
    );
    assert_eq!(String::from_utf8(json.stdout).unwrap(), expected_json);

    let nul = Command::new(bin_path())
        .arg("headings")
        .arg("-0")
        .arg(&file)
        .output()
        .expect("spawn org-cli");
    assert_eq!(nul.status.code(), Some(0));
    let expected_nul = format!(
        "1\02\0{}\0WIP\00\0Buy milk\0home\0A\0{FINGERPRINT}\0{ETAG}\0{{\"active\":[\"TODO\",\"WIP\"],\"done\":[\"DONE\",\"CANCELED\"]}}\0\0",
        file.display()
    )
    .into_bytes();
    assert_eq!(nul.stdout, expected_nul);

    let human = Command::new(bin_path())
        .arg("headings")
        .arg(&file)
        .output()
        .expect("spawn org-cli");
    assert_eq!(human.status.code(), Some(0));
    let expected_human = format!("{}:\n   2: * WIP [#A] Buy milk :home:\n", file.display());
    assert_eq!(String::from_utf8(human.stdout).unwrap(), expected_human);
}

#[test]
fn headings_without_declaration_or_flag_keeps_wip_in_title() {
    let file = fixture("no-todo-decl.org");
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("-j")
        .arg(&file)
        .output()
        .expect("spawn org-cli");

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let records: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    // WIP is NOT a default keyword: it stays folded into the title.
    let wip = records
        .iter()
        .find(|r| r["title"] == "WIP Implement feature")
        .expect("WIP heading should keep keyword in title");
    assert_eq!(wip["keyword"], serde_json::Value::Null);
    assert_eq!(wip["done"], serde_json::Value::Null);
}

#[test]
fn headings_in_buffer_declaration_wins_over_flag() {
    // File declares TODO WIP | DONE CANCELED. A conflicting flag that
    // omits WIP must be ignored: the in-buffer declaration wins, so WIP
    // is still recognized as a keyword.
    let file = fixture("custom-todo.org");
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("--todo-keywords")
        .arg("TODO | DONE")
        .arg("--keyword")
        .arg("WIP")
        .arg("-j")
        .arg(&file)
        .output()
        .expect("spawn org-cli");

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "stdout was:\n{}", stdout);
    let record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(record["keyword"], "WIP");
    assert_eq!(record["title"], "Implement feature");
}

#[test]
fn headings_skips_unreadable_file_and_continues() {
    // A directory containing one valid org file and one non-UTF-8
    // file. The run must skip the unreadable file with a warning
    // instead of aborting: exit 0, valid heading present, invalid
    // file's content absent.
    let dir = std::env::temp_dir().join(format!(
        "org-cli-headings-skip-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let good = dir.join("good.org");
    std::fs::write(&good, "* Valid heading\n").unwrap();

    // Non-UTF-8 bytes: 0xE9 is `é` in ISO-8859-1, invalid UTF-8.
    let bad = dir.join("bad.org");
    std::fs::write(&bad, b"* Caf\xe9\n").unwrap();

    let out = Command::new(bin_path())
        .arg("headings")
        .arg("-j")
        .arg(&dir)
        .output()
        .expect("spawn org-cli");

    let stdout = String::from_utf8(out.stdout).unwrap();

    assert_eq!(
        out.status.code(),
        Some(0),
        "run must not abort; stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        stdout
    );

    let titles: Vec<String> = stdout
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["title"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();

    assert_eq!(
        titles,
        vec!["Valid heading".to_string()],
        "only the valid file's heading must appear; stdout={}",
        stdout
    );
    assert!(
        !stdout.contains("Caf"),
        "the unreadable file's headings must be absent; stdout={}",
        stdout
    );

    let _ = std::fs::remove_dir_all(&dir);
}
