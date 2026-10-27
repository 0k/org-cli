//! Requested CLOCK properties inherit from headings, independently of filters.

use std::path::{Path, PathBuf};
use std::process::Command;

use indoc::indoc;
use similar::TextDiff;

const CLOCK: &str = "CLOCK: [2026-01-07 Wed 09:00 +0100]--[2026-01-07 Wed 10:00 +0100] =>  1:00";
const LINES: [usize; 8] = [3, 15, 20, 28, 31, 37, 40, 43];

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/clock/inherited-properties.org")
}

fn run(path: &Path, state: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_org-cli"))
        .args(["clock", "list", "-j"])
        .args(args)
        .arg(path)
        .env("ORG_CLI_CLOCK_CACHE", state.join("clock.sqlite"))
        .env("XDG_CONFIG_HOME", state)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stderr, b"");
    String::from_utf8(output.stdout).unwrap()
}

fn assert_output_eq(actual: &str, expected: &str) {
    if actual != expected {
        panic!("{}", TextDiff::from_lines(expected, actual).unified_diff());
    }
}

fn expected(path: &Path, maps: Option<&[&str; 8]>) -> String {
    let file = serde_json::to_string(&path.canonicalize().unwrap()).unwrap();
    LINES.iter().enumerate().map(|(i, line)| {
        let properties = maps.map(|maps| format!(",\"properties\":{}", maps[i])).unwrap_or_default();
        format!("{{\"file\":{file},\"line\":{line},\"start\":\"2026-01-07T09:00:00+01:00\",\"end\":\"2026-01-07T10:00:00+01:00\",\"tz_minutes\":60,\"has_seconds\":false,\"duration_seconds\":3600,\"raw\":\"{CLOCK}\"{properties}}}\n")
    }).collect()
}

#[test]
fn inherited_properties_match_full_output_in_all_cache_modes() {
    let state = tempfile::tempdir().unwrap();
    let maps = [
        "{}",
        r#"{"TASK_REF":"lok/42"}"#,
        r#"{"TASK_REF":"elabore/6623#0123456789abcdef"}"#,
        r#"{"TASK_REF":"lok/99#fedcba9876543210"}"#,
        r#"{"TASK_REF":"elabore/6623#0123456789abcdef"}"#,
        r#"{"TASK_REF":"elabore/6623#0123456789abcdef"}"#,
        "{}",
        "{}",
    ];
    // Cold, warm, verified, and bypassed runs must all satisfy the same contract.
    for mode in [None, None, Some("--verify"), Some("--no-cache")] {
        let mut args = vec!["--inherit-property", "TASK_REF"];
        args.extend(mode);
        assert_output_eq(
            &run(&fixture(), state.path(), &args),
            &expected(&fixture(), Some(&maps)),
        );
    }
}

#[test]
fn repeated_names_match_case_insensitively_and_preserve_values() {
    let state = tempfile::tempdir().unwrap();
    let maps = [
        "{}",
        r#"{"OWNER":"Alice; Bob=team","task_ref":"lok/42"}"#,
        r#"{"OWNER":"Alice; Bob=team","task_ref":"elabore/6623#0123456789abcdef"}"#,
        r#"{"OWNER":"Alice; Bob=team","task_ref":"lok/99#fedcba9876543210"}"#,
        r#"{"OWNER":"Alice; Bob=team","task_ref":"elabore/6623#0123456789abcdef"}"#,
        r#"{"OWNER":"Alice; Bob=team","task_ref":"elabore/6623#0123456789abcdef"}"#,
        "{}",
        "{}",
    ];
    let args = [
        "--inherit-property",
        "task_ref",
        "--inherit-property",
        "OWNER",
        "--inherit-property",
        "MISSING",
    ];
    for _ in 0..2 {
        assert_output_eq(
            &run(&fixture(), state.path(), &args),
            &expected(&fixture(), Some(&maps)),
        );
    }
}

#[test]
fn no_option_is_byte_identical_even_after_inheritance_warms_cache() {
    let state = tempfile::tempdir().unwrap();
    run(
        &fixture(),
        state.path(),
        &["--inherit-property", "TASK_REF"],
    );
    for args in [vec![], vec!["--verify"], vec!["--no-cache"]] {
        assert_output_eq(
            &run(&fixture(), state.path(), &args),
            &expected(&fixture(), None),
        );
    }
}

#[test]
fn missing_requested_name_emits_empty_maps() {
    let state = tempfile::tempdir().unwrap();
    assert_output_eq(
        &run(&fixture(), state.path(), &["--inherit-property", "MISSING"]),
        &expected(&fixture(), Some(&["{}"; 8])),
    );
}

#[test]
fn ancestor_edits_invalidate_cache_without_changing_direct_property_filter() {
    let state = tempfile::tempdir().unwrap();
    let file = state.path().join("edited.org");
    let content = indoc! {"
        * Parent
          :PROPERTIES:
          :TASK_REF: lok/1
          :END:
        ** Child
    "};
    std::fs::write(&file, format!("{content}{CLOCK}\n")).unwrap();
    let args = ["--inherit-property", "TASK_REF"];
    run(&file, state.path(), &args);
    std::fs::write(
        &file,
        format!("{}{CLOCK}\n", content.replace("lok/1", "elabore/123")),
    )
    .unwrap();
    let cached = run(&file, state.path(), &args);
    let bypass = run(
        &file,
        state.path(),
        &["--inherit-property", "TASK_REF", "--no-cache"],
    );
    assert_output_eq(&cached, &bypass);
    let record: serde_json::Value = serde_json::from_str(cached.trim()).unwrap();
    assert_eq!(
        record["properties"],
        serde_json::json!({"TASK_REF": "elabore/123"})
    );
    assert_output_eq(
        &run(
            &file,
            state.path(),
            &[
                "--inherit-property",
                "TASK_REF",
                "--has-property",
                "TASK_REF",
            ],
        ),
        "",
    );
}
