//! End-to-end tests for the `syntax check` subcommand.
//!
//! Spawns the actual `org-cli` binary against the fixtures at
//! `tests/fixtures/syntax/` and asserts the *specification*: block and
//! drawer delimiters must balance, block content is literal, and a
//! defect is anchored at the line that opened the unbalanced
//! construct.
//!
//! `broken.org` holds one of each defect shape; `clean.org` is the
//! false-positive guard and uses Org's comma escape when quoting a
//! headline inside a lesser block. `dissolved-lesser-block.org`
//! captures Emacs' treatment of the same headline without that escape.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("syntax")
}

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_org-cli"))
}

/// Run `syntax check` over one fixture, returning `(exit_code, stdout)`.
fn run_syntax(args: &[&str], target: &Path) -> (Option<i32>, String) {
    let output = Command::new(bin_path())
        .arg("syntax")
        .arg("check")
        .args(args)
        .arg(target)
        .output()
        .expect("failed to spawn org-cli");
    let stdout = String::from_utf8(output.stdout).expect("stdout not UTF-8");
    (output.status.code(), stdout)
}

fn broken() -> PathBuf {
    fixture_root().join("broken.org")
}

fn clean() -> PathBuf {
    fixture_root().join("clean.org")
}

/// Every NDJSON record emitted for `broken.org`.
fn json_records() -> Vec<serde_json::Value> {
    let (code, stdout) = run_syntax(&["-j"], &broken());
    assert_eq!(code, Some(1), "fixture has defects → exit 1");
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("not valid JSON: {e}\nline={l}"))
        })
        .collect()
}

// ---------------------------------------------------------------------
// The false-positive guard comes first: a checker that cries wolf on
// ordinary documents is worse than no checker, because it trains its
// consumers to bypass the gate.
// ---------------------------------------------------------------------

#[test]
fn test_syntax_check_clean_fixture_exits_zero() {
    let (code, stdout) = run_syntax(&[], &clean());
    assert_eq!(code, Some(0), "clean fixture must pass; got:\n{stdout}");
    assert!(stdout.contains("All org syntax OK."), "got:\n{stdout}");
}

#[test]
fn test_syntax_check_accepts_comma_escaped_lesser_block_payload() {
    // `clean.org` contains a `#+begin_example` holding a comma-escaped
    // `,* heading` and an unclosed-looking `:PROPERTIES:`. Both are
    // payload text, not structure.
    let (code, stdout) = run_syntax(&["-j"], &clean());
    assert_eq!(code, Some(0), "quoted org syntax is not a defect: {stdout}");
    assert!(stdout.trim().is_empty(), "no records expected: {stdout}");
}

#[test]
fn test_syntax_check_reports_dissolved_lesser_block_like_emacs() {
    let target = fixture_root().join("dissolved-lesser-block.org");
    let (code, stdout) = run_syntax(&["-j"], &target);
    assert_eq!(code, Some(1));

    let got: Vec<(u64, String)> = stdout
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .map(|v| {
            (
                v["line"].as_u64().unwrap(),
                v["reason_tag"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (1, "unterminated-block".to_string()),
            (3, "unterminated-drawer".to_string()),
            (5, "stray-block-end".to_string()),
        ],
        "must match Emacs org-lint's three findings for this shape"
    );
}

#[test]
fn test_syntax_check_reports_nested_failures_inside_greater_block() {
    let target = fixture_root().join("greater-with-broken-children.org");
    let (code, stdout) = run_syntax(&["-j"], &target);
    assert_eq!(code, Some(1));
    let got: Vec<(u64, String)> = stdout
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .map(|v| {
            (
                v["line"].as_u64().unwrap(),
                v["reason_tag"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (1, "unterminated-block".to_string()),
            (2, "unterminated-block".to_string()),
            (4, "unterminated-drawer".to_string()),
            (6, "stray-block-end".to_string()),
            (7, "stray-block-end".to_string()),
        ]
    );
}

#[test]
fn test_syntax_check_keeps_parent_greater_end_valid_around_broken_drawer() {
    let target = fixture_root().join("quote-with-unclosed-drawer.org");
    let (code, stdout) = run_syntax(&["-j"], &target);
    assert_eq!(code, Some(1));
    let rows: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 1, "only the drawer is incomplete: {rows:?}");
    assert_eq!(rows[0]["reason_tag"], "unterminated-drawer");
    assert_eq!(rows[0]["line"], 2);
    assert_eq!(rows[0]["bounded_at"], 4);
}

#[test]
fn test_syntax_check_rejects_trailing_garbage_on_end_marker() {
    let target = fixture_root().join("invalid-end-marker.org");
    let (code, stdout) = run_syntax(&["-j"], &target);
    assert_eq!(code, Some(1));
    let tags: Vec<String> = stdout
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .map(|v| v["reason_tag"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(tags, vec!["unterminated-block", "invalid-block-end"]);
}

// ---------------------------------------------------------------------
// Defect detection.
// ---------------------------------------------------------------------

#[test]
fn test_syntax_check_reports_every_defect_shape() {
    let records = json_records();

    let got: BTreeSet<(String, u64)> = records
        .iter()
        .map(|v| {
            (
                v["reason_tag"].as_str().unwrap_or("").to_string(),
                v["line"].as_u64().unwrap_or(0),
            )
        })
        .collect();

    let expected: BTreeSet<(String, u64)> = [
        // opener line of the drawer that is never closed
        ("unterminated-drawer", 4),
        // the stray `:END:` itself
        ("stray-drawer-end", 12),
        // opener line of the block that runs to EOF
        ("unterminated-block", 15),
    ]
    .into_iter()
    .map(|(t, l)| (t.to_string(), l))
    .collect();

    assert_eq!(
        got, expected,
        "must report exactly one of each defect shape, anchored at its opener"
    );
}

#[test]
fn test_syntax_check_names_the_bounding_line() {
    let records = json_records();
    let drawer = records
        .iter()
        .find(|v| v["reason_tag"] == "unterminated-drawer")
        .expect("drawer defect present");

    assert_eq!(drawer["construct"], "PROPERTIES");
    assert_eq!(
        drawer["bounded_at"], 8,
        "the structural line that ended the search for :END:"
    );
    assert!(
        drawer["reason_message"]
            .as_str()
            .expect("reason_message is a string")
            .contains("boundary at line 8"),
    );
}

#[test]
fn test_syntax_check_omits_bounded_at_when_search_ran_to_eof() {
    let records = json_records();
    let block = records
        .iter()
        .find(|v| v["reason_tag"] == "unterminated-block")
        .expect("block defect present");

    assert_eq!(block["construct"], "src");
    assert!(
        block.get("bounded_at").is_none(),
        "absent field, not a null: {block}"
    );
}

#[test]
fn test_syntax_check_human_output_shows_location_and_opener() {
    // Output is piped, so `colored` emits no escape codes.
    let (code, stdout) = run_syntax(&[], &broken());
    assert_eq!(code, Some(1));

    assert!(
        stdout.contains("broken.org:4: unterminated-drawer"),
        "human output must locate the opener; got:\n{stdout}"
    );
    assert!(
        stdout.contains(":PROPERTIES:"),
        "human output must echo the opening line; got:\n{stdout}"
    );
    assert!(
        stdout.contains("3 syntax defect(s) found"),
        "human output must summarise; got:\n{stdout}"
    );
}

// ---------------------------------------------------------------------
// Machine-readable formats. The NUL contract mirrors `link check`:
// fixed field count, and never an ambiguous empty trailing field.
// ---------------------------------------------------------------------

#[test]
fn test_syntax_check_nul_records_have_six_fields() {
    let (code, stdout) = run_syntax(&["-0"], &broken());
    assert_eq!(code, Some(1));

    let records: Vec<&str> = stdout.split("\0\0").filter(|r| !r.is_empty()).collect();
    assert_eq!(records.len(), 3, "one record per defect");

    for r in &records {
        let fields: Vec<&str> = r.split('\0').collect();
        assert_eq!(fields.len(), 6, "every record has 6 fields: {fields:?}");
        assert!(!fields[5].is_empty(), "bounded_at is never empty");
    }
}

#[test]
fn test_syntax_check_nul_records_never_emit_three_consecutive_nuls() {
    // A record ends at `\0\0`. An empty trailing field would emit a
    // third NUL and desynchronize a reader splitting on the terminator.
    let (_, stdout) = run_syntax(&["-0"], &broken());
    assert!(
        !stdout.contains("\0\0\0"),
        "no record may contain three consecutive NUL bytes"
    );
}

#[test]
fn test_syntax_check_nul_uses_dash_for_absent_bounded_at() {
    let (_, stdout) = run_syntax(&["-0"], &broken());
    let record = stdout
        .split("\0\0")
        .find(|r| r.contains("unterminated-block"))
        .expect("block record present");
    let fields: Vec<&str> = record.split('\0').collect();
    assert_eq!(fields[2], "unterminated-block");
    assert_eq!(fields[3], "src", "construct in position 4");
    assert_eq!(fields[5], "-", "absent bounded_at is a dash, not empty");
}

// ---------------------------------------------------------------------
// Flags and exit codes.
// ---------------------------------------------------------------------

#[test]
fn test_syntax_check_first_stops_after_one_defect() {
    let (code, stdout) = run_syntax(&["-j", "-1"], &broken());
    assert_eq!(code, Some(1));
    assert_eq!(
        stdout.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "--first emits exactly one record"
    );
}

#[test]
fn test_syntax_check_scans_a_directory_recursively() {
    // Pointed at the fixture dir, it must find every malformed file
    // while clean.org contributes none.
    let (code, stdout) = run_syntax(&["-j"], &fixture_root());
    assert_eq!(code, Some(1));
    assert_eq!(
        stdout.lines().filter(|l| !l.trim().is_empty()).count(),
        14,
        "all malformed fixtures contribute their full diagnostics"
    );
}

#[test]
fn test_old_top_level_link_commands_are_rejected() {
    for command in ["check", "orphans", "backlinks"] {
        let output = Command::new(bin_path())
            .arg(command)
            .output()
            .expect("failed to spawn org-cli");
        assert_eq!(
            output.status.code(),
            Some(2),
            "old top-level command {command:?} must not remain as an alias"
        );
    }
}

#[cfg(unix)]
#[test]
fn test_syntax_check_nul_output_preserves_non_utf8_path_bytes() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "org-cli-non-utf8-path-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let name = OsString::from_vec(b"bad-\xff.org".to_vec());
    let path = dir.join(&name);
    std::fs::write(&path, "#+begin_src sh\n").unwrap();

    let output = Command::new(bin_path())
        .arg("syntax")
        .arg("check")
        .arg("-0")
        .arg(&path)
        .output()
        .expect("failed to spawn org-cli");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(output.status.code(), Some(1));
    let expected_path = path.as_os_str().as_encoded_bytes();
    assert!(
        output.stdout.starts_with(expected_path),
        "NUL output must preserve the pathname bytes exactly"
    );
    assert_eq!(output.stdout[expected_path.len()], 0);
}

#[test]
fn test_syntax_check_nul_output_escapes_nul_inside_source_line() {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "org-cli-nul-content-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bad.org");
    std::fs::write(&path, b"#+begin_src sh\0arg\n").unwrap();

    let output = Command::new(bin_path())
        .arg("syntax")
        .arg("check")
        .arg("-0")
        .arg(&path)
        .output()
        .expect("failed to spawn org-cli");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        output.stdout.iter().filter(|&&b| b == 0).count(),
        7,
        "six fields require five separators plus the double-NUL terminator"
    );
    assert!(
        output.stdout.windows(5).any(|w| w == br"sh\0a"),
        "the embedded NUL must be escaped as two printable bytes"
    );
}

#[test]
fn test_syntax_check_explicit_non_utf8_file_is_a_hard_error() {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "org-cli-invalid-utf8-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("binary.org");
    std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();

    let output = Command::new(bin_path())
        .arg("syntax")
        .arg("check")
        .arg(&path)
        .output()
        .expect("failed to spawn org-cli");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        output.stdout.is_empty(),
        "must not print a false clean summary"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("cannot read"), "got: {stderr}");
    assert!(stderr.contains("UTF-8"), "got: {stderr}");
}
