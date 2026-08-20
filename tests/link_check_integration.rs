//! End-to-end tests for the `link check` action.
//!
//! Spawns the actual `org-cli` binary against the fixture tree at
//! `tests/fixtures/check/` and asserts the *specification* of what
//! `check` promises: it validates every link a user can follow —
//! including links written inside `:PROPERTIES:` values and
//! `:LOGBOOK:` notes — and names where each broken one came from.
//!
//! The fixture holds four broken links (one per origin shape) and two
//! valid ones, plus a `#+begin_src` block and a `CLOCK:` line that
//! must contribute no links at all.
//!
//! These complement the unit tests in `src/cli/action/check.rs::tests`
//! by exercising clap parsing, dispatch, and the three output writers
//! through the real binary.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Absolute path to the fixture root, regardless of cwd.
fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("check")
}

/// Path to the freshly-built binary chosen by Cargo.
fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_org-cli"))
}

/// Run `check` over the fixture with the given extra flags, returning
/// `(exit_code, stdout)`.
fn run_check(args: &[&str]) -> (Option<i32>, String) {
    let output = Command::new(bin_path())
        .arg("link")
        .arg("check")
        .args(args)
        .arg(fixture_root())
        .output()
        .expect("failed to spawn org-cli");
    let stdout = String::from_utf8(output.stdout).expect("stdout not UTF-8");
    (output.status.code(), stdout)
}

/// Every NDJSON record emitted by a `check -j` run over the fixture.
fn json_records() -> Vec<serde_json::Value> {
    let (code, stdout) = run_check(&["-j"]);
    assert_eq!(code, Some(1), "fixture has broken links → exit 1");
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("not valid JSON: {e}\nline={l}"))
        })
        .collect()
}

/// The `(origin_tag, origin_drawer, origin_property, raw)` tuple of a
/// record, with absent fields rendered as the empty string.
fn origin_of(v: &serde_json::Value) -> (String, String, String, String) {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    (
        s("origin_tag"),
        s("origin_drawer"),
        s("origin_property"),
        s("raw"),
    )
}

#[test]
fn test_check_reports_every_followable_broken_link_with_its_origin() {
    let records = json_records();

    let got: BTreeSet<(String, String, String, String)> = records.iter().map(origin_of).collect();

    let expected: BTreeSet<(String, String, String, String)> = [
        (
            "property",
            "PROPERTIES",
            "SOURCE_AUDIO",
            "[[file:missing-audio.ogg][recording]]",
        ),
        (
            "property",
            "PROPERTIES",
            "TIMESTAMP_NOTE",
            "[[file:missing-prose.org][ce document]]",
        ),
        (
            "drawer",
            "LOGBOOK",
            "",
            "[[file:missing-note.org][the note]]",
        ),
        ("body", "", "", "[[file:missing-body.org][gone]]"),
    ]
    .into_iter()
    .map(|(a, b, c, d)| (a.to_string(), b.to_string(), c.to_string(), d.to_string()))
    .collect();

    assert_eq!(
        got, expected,
        "check must report exactly the four broken links, each naming its origin"
    );
}

#[test]
fn test_check_does_not_report_valid_links_or_quoted_syntax() {
    let raws: Vec<String> = json_records()
        .iter()
        .map(|v| v["raw"].as_str().unwrap_or("").to_string())
        .collect();

    for must_not in [
        "target.org",          // valid, reachable from body and property
        "not-a-real-link.org", // inside #+begin_src, quoted syntax
    ] {
        assert!(
            !raws.iter().any(|r| r.contains(must_not)),
            "{must_not} must not be reported; got {raws:?}"
        );
    }
}

#[test]
fn test_check_records_carry_line_and_reason_for_metadata_links() {
    let records = json_records();
    let audio = records
        .iter()
        .find(|v| v["origin_property"] == "SOURCE_AUDIO")
        .expect("SOURCE_AUDIO record present");

    assert_eq!(audio["reason_tag"], "path-missing");
    assert_eq!(
        audio["line"], 5,
        "line must point at the property entry in the fixture"
    );
    assert!(
        audio["source_file"]
            .as_str()
            .expect("source_file is a string")
            .ends_with("source.org"),
    );
}

#[test]
fn test_check_human_output_names_the_metadata_field() {
    // Output is piped, so the `colored` crate emits no escape codes.
    let (code, stdout) = run_check(&[]);
    assert_eq!(code, Some(1));

    assert!(
        stdout.contains(" in :PROPERTIES: drawer, property :SOURCE_AUDIO:"),
        "human output must name the property; got:\n{stdout}"
    );
    assert!(
        stdout.contains(" in :LOGBOOK: drawer"),
        "human output must name the drawer; got:\n{stdout}"
    );
}

#[test]
fn test_check_human_output_leaves_body_links_unannotated() {
    let (_, stdout) = run_check(&[]);
    let body_line = stdout
        .lines()
        .find(|l| l.contains("[[file:missing-body.org][gone]]"))
        .expect("body link reported");
    assert_eq!(
        body_line.trim(),
        "[[file:missing-body.org][gone]]",
        "a body link's line/column already locate it; no origin suffix"
    );
}

#[test]
fn test_check_nul_output_appends_origin_after_the_historical_fields() {
    let (code, stdout) = run_check(&["-0"]);
    assert_eq!(code, Some(1));

    let record = stdout
        .split("\0\0")
        .find(|r| r.contains("missing-audio.ogg"))
        .expect("SOURCE_AUDIO record present");
    let fields: Vec<&str> = record.split('\0').collect();

    assert_eq!(fields.len(), 7, "six historical fields plus one origin");
    assert_eq!(fields[3], "path-missing", "reason_tag stays in position 4");
    assert_eq!(fields[5], "[[file:missing-audio.ogg][recording]]");
    assert_eq!(fields[6], "property:PROPERTIES:SOURCE_AUDIO");
}

#[test]
fn test_check_nul_records_never_emit_an_ambiguous_empty_trailing_field() {
    // A record ends at `\0\0`. An empty trailing field would emit a
    // third consecutive NUL and desynchronize a reader splitting on
    // the terminator, so a body link must still carry a non-empty
    // origin field.
    let (_, stdout) = run_check(&["-0"]);
    assert!(
        !stdout.contains("\0\0\0"),
        "no record may contain three consecutive NUL bytes"
    );

    let records: Vec<&str> = stdout.split("\0\0").filter(|r| !r.is_empty()).collect();
    assert_eq!(records.len(), 4, "one record per broken link");
    for r in &records {
        let fields: Vec<&str> = r.split('\0').collect();
        assert_eq!(fields.len(), 7, "every record has 7 fields: {fields:?}");
        assert!(!fields[6].is_empty(), "origin is never empty");
    }

    let body = records
        .iter()
        .find(|r| r.contains("missing-body.org"))
        .expect("body record present");
    assert_eq!(
        body.split('\0').nth(6),
        Some("body"),
        "a body link's origin is the bare tag"
    );
}

#[test]
fn test_check_clean_tree_reports_no_broken_links() {
    // Guards the fixture assertions from passing vacuously: a tree
    // whose only file has no links must exit 0.
    let output = Command::new(bin_path())
        .arg("link")
        .arg("check")
        .arg(fixture_root().join("target.org"))
        .output()
        .expect("failed to spawn org-cli");
    assert_eq!(output.status.code(), Some(0));
}
