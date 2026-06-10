//! End-to-end test for the `backlinks` subcommand.
//!
//! Spawns the actual `org-cli` binary against the fixture tree at
//! `tests/fixtures/backlinks/` and asserts:
//!
//! - querying backlinks to `target.org` finds exactly the two
//!   sources that link it (`source-a.org` directly and
//!   `nested/source-b.org` via `../target.org`), proving the
//!   canonicalization makes relative paths match,
//! - `unrelated.org` (which links elsewhere) is NOT reported,
//! - exit code is 0 when backlinks are found,
//! - exit code is 1 when a target has no backlinks.
//!
//! These tests complement the unit tests in
//! `src/cli/action/backlinks.rs::tests` by exercising the real
//! binary with a real fixture tree, catching wiring/regression
//! issues that unit tests don't see (clap parsing, dispatch,
//! output streams).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Absolute path to the fixture root, regardless of cwd.
fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("backlinks")
}

/// Path to the freshly-built debug binary chosen by Cargo.
fn bin_path() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by Cargo for integration tests.
    PathBuf::from(env!("CARGO_BIN_EXE_org-cli"))
}

/// Parse one NDJSON backlink line into its `source_file` field.
fn parse_source_file(line: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(line)
        .unwrap_or_else(|e| panic!("not valid JSON: {:?}\nline={}", e, line));
    v["source_file"]
        .as_str()
        .expect("source_file field missing or not a string")
        .to_string()
}

#[test]
fn test_backlinks_finds_both_referencing_sources() {
    let fixture = fixture_root();
    let target = fixture.join("target.org");
    let output = Command::new(bin_path())
        .arg("backlinks")
        .arg("-j")
        .arg(&target)
        .arg(&fixture)
        .output()
        .expect("failed to spawn org-cli");

    // Two backlinks present → exit code 0 (found).
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit 0 (backlinks found); stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout not UTF-8");
    let sources: BTreeSet<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(parse_source_file)
        .collect();

    let expected: BTreeSet<String> = [
        fixture.join("source-a.org"),
        fixture.join("nested").join("source-b.org"),
    ]
    .iter()
    .map(|p| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|e| panic!("canonicalize {:?}: {}", p, e))
            .to_string_lossy()
            .into_owned()
    })
    .collect();

    assert_eq!(sources, expected, "unexpected backlink source set");
}

#[test]
fn test_backlinks_target_with_no_backlinks_exits_one() {
    let fixture = fixture_root();
    // unrelated.org is linked by nobody in the tree.
    let target = fixture.join("unrelated.org");
    let output = Command::new(bin_path())
        .arg("backlinks")
        .arg("-j")
        .arg(&target)
        .arg(&fixture)
        .output()
        .expect("failed to spawn org-cli");

    // No backlinks → exit code 1.
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1 (no backlinks); stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout not UTF-8");
    assert!(
        stdout.lines().all(|l| l.is_empty()),
        "expected no NDJSON records; got: {}",
        stdout
    );
}

#[test]
fn test_backlinks_first_flag_stops_early() {
    let fixture = fixture_root();
    let target = fixture.join("target.org");
    let output = Command::new(bin_path())
        .arg("backlinks")
        .arg("-j")
        .arg("--first")
        .arg(&target)
        .arg(&fixture)
        .output()
        .expect("failed to spawn org-cli");

    // At least one backlink exists → exit 0, and exactly one record.
    assert_eq!(output.status.code(), Some(0));

    let stdout = String::from_utf8(output.stdout).expect("stdout not UTF-8");
    let count = stdout.lines().filter(|l| !l.is_empty()).count();
    assert_eq!(count, 1, "expected exactly one record with --first");
}
