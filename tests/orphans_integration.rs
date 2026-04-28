//! End-to-end test for the `orphans` subcommand.
//!
//! Spawns the actual `org-cli` binary against the fixture tree at
//! `tests/fixtures/orphans/` and asserts:
//!
//! - exit code is 1 (orphans found),
//! - the NDJSON stream contains exactly the three expected orphans,
//! - `--directory-references-all-children` removes
//!   `ref-folder/child.org` from the orphan set,
//! - `--filter-path sub` narrows the report to the `sub/` subtree.
//!
//! These tests complement the unit tests in
//! `src/cli/action/orphans.rs::tests` by exercising the real binary
//! with a real fixture tree, catching wiring/regression issues that
//! unit tests don't see (clap parsing, dispatch, output streams).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Absolute path to the fixture root, regardless of cwd.
fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("orphans")
}

/// Path to the freshly-built debug binary chosen by Cargo.
fn bin_path() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by Cargo for integration tests.
    PathBuf::from(env!("CARGO_BIN_EXE_org-cli"))
}

/// Parse one NDJSON orphan line into its `path` field.
fn parse_orphan_path(line: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(line)
        .unwrap_or_else(|e| panic!("not valid JSON: {:?}\nline={}", e, line));
    assert_eq!(v["type"], "unreferenced", "unexpected type: {}", line);
    v["path"]
        .as_str()
        .expect("path field missing or not a string")
        .to_string()
}

#[test]
fn test_orphans_default_run_reports_three_orphans() {
    let fixture = fixture_root();
    let output = Command::new(bin_path())
        .arg("orphans")
        .arg("-j")
        .arg(&fixture)
        .output()
        .expect("failed to spawn org-cli");

    // Three orphans are present → exit code 1.
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1; stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout not UTF-8");
    let paths: BTreeSet<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(parse_orphan_path)
        .collect();

    let expected: BTreeSet<String> = [
        fixture.join("orphan.org"),
        fixture.join("ref-folder").join("child.org"),
        fixture.join("sub").join("unvisited.org"),
    ]
    .iter()
    .map(|p| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|e| panic!("canonicalize {:?}: {}", p, e))
            .to_string_lossy()
            .into_owned()
    })
    .collect();

    assert_eq!(paths, expected, "unexpected orphan set");
}

#[test]
fn test_orphans_directory_flag_absorbs_folder_children() {
    let fixture = fixture_root();
    let output = Command::new(bin_path())
        .arg("orphans")
        .arg("-j")
        .arg("--directory-references-all-children")
        .arg(&fixture)
        .output()
        .expect("failed to spawn org-cli");

    // ref-folder/child.org is now covered by the directory link;
    // orphan.org and sub/unvisited.org remain.
    assert_eq!(output.status.code(), Some(1));

    let stdout = String::from_utf8(output.stdout).expect("stdout not UTF-8");
    let paths: BTreeSet<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(parse_orphan_path)
        .collect();

    let expected: BTreeSet<String> = [
        fixture.join("orphan.org"),
        fixture.join("sub").join("unvisited.org"),
    ]
    .iter()
    .map(|p| {
        std::fs::canonicalize(p)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    })
    .collect();

    assert_eq!(paths, expected);
}

#[test]
fn test_orphans_filter_path_narrows_to_subtree() {
    let fixture = fixture_root();
    let output = Command::new(bin_path())
        .arg("orphans")
        .arg("-j")
        .arg("--filter-path")
        .arg(fixture.join("sub"))
        .arg(&fixture)
        .output()
        .expect("failed to spawn org-cli");

    // Only sub/unvisited.org passes the filter.
    assert_eq!(output.status.code(), Some(1));

    let stdout = String::from_utf8(output.stdout).expect("stdout not UTF-8");
    let paths: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(parse_orphan_path)
        .collect();

    assert_eq!(paths.len(), 1, "got: {:?}", paths);
    let expected = std::fs::canonicalize(fixture.join("sub").join("unvisited.org"))
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(paths[0], expected);
}
