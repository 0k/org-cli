//! End-to-end tests for `org-cli clock list-filed` and `check-filed`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use indoc::{formatdoc, indoc};
use similar::TextDiff;

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_org-cli"))
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/clock")
        .join(name)
}

fn run_clock(args: &[&str], path: &Path) -> Output {
    let cache = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    Command::new(bin_path())
        .arg("clock")
        .args(args)
        .arg(path)
        .env("ORG_CLI_CLOCK_CACHE", cache.path().join("clock.sqlite"))
        .env("XDG_CONFIG_HOME", config.path())
        .output()
        .expect("spawn org-cli clock command")
}

fn assert_output_eq(actual: &str, expected: &str) {
    if actual != expected {
        eprintln!("{}", TextDiff::from_lines(expected, actual).unified_diff());
        panic!("output mismatch");
    }
}

fn expected_json(path: &Path) -> String {
    let file = path.canonicalize().unwrap();
    formatdoc! {r#"
        {{"file":"{file}","line":4,"start":1787556600,"end":1787560200,"tag":"lok"}}
        {{"file":"{file}","line":5,"start":1787562005,"end":1787566510,"tag":"acme_2"}}
    "#, file = file.display()}
}

#[test]
fn list_filed_json_matches_exact_output() {
    let path = fixture("filed.org");
    let output = run_clock(&["list-filed", "-j", "--no-cache"], &path);
    assert_eq!(output.status.code(), Some(0));
    assert_output_eq(
        &String::from_utf8(output.stdout).unwrap(),
        &expected_json(&path),
    );
    assert_output_eq(&String::from_utf8(output.stderr).unwrap(), "");
}

#[test]
fn list_filed_filters_by_customer() {
    let path = fixture("filed.org");
    let output = run_clock(
        &["list-filed", "-j", "--no-cache", "--customer", "lok"],
        &path,
    );
    assert_eq!(output.status.code(), Some(0));
    let expected = expected_json(&path).lines().next().unwrap().to_string() + "\n";
    assert_output_eq(&String::from_utf8(output.stdout).unwrap(), &expected);
}

#[test]
fn list_filed_nul_has_five_fixed_fields_per_record() {
    let path = fixture("filed.org");
    let output = run_clock(&["list-filed", "-0", "--no-cache"], &path);
    assert_eq!(output.status.code(), Some(0));
    let file = path.canonicalize().unwrap();
    let expected = format!(
        "{}\04\01787556600\01787560200\0lok\0\0{}\05\01787562005\01787566510\0acme_2\0\0",
        file.display(),
        file.display()
    );
    assert_eq!(output.stdout, expected.as_bytes());
}

#[test]
fn list_filed_human_output_is_one_line_per_record() {
    let path = fixture("filed.org");
    let output = run_clock(&["list-filed", "--no-cache"], &path);
    assert_eq!(output.status.code(), Some(0));
    let file = path.canonicalize().unwrap();
    let expected = format!(
        "{}:4  2026-08-24T09:30:00+02:00 -- 2026-08-24T10:30:00+02:00  lok\n{}:5  2026-08-24T11:00:05+02:00 -- 2026-08-24T12:15:10+02:00  acme_2\n",
        file.display(),
        file.display()
    );
    assert_output_eq(&String::from_utf8(output.stdout).unwrap(), &expected);
}

#[test]
fn check_filed_accepts_clean_and_reports_dirty_lines() {
    let clean = run_clock(&["check-filed"], &fixture("filed.org"));
    assert_eq!(clean.status.code(), Some(0));
    assert_output_eq(
        &String::from_utf8(clean.stdout).unwrap(),
        "All FILED lines OK.\n",
    );

    let dirty = run_clock(&["check-filed", "--no-color"], &fixture("filed-dirty.org"));
    assert_eq!(dirty.status.code(), Some(1));
    let stdout = String::from_utf8(dirty.stdout).unwrap();
    assert!(stdout.contains("missing-tag"), "{stdout}");
    assert!(stdout.contains("invalid-tag"), "{stdout}");
}

#[test]
fn list_filed_rejects_invalid_lines_on_stderr() {
    let output = run_clock(
        &["list-filed", "-j", "--no-cache"],
        &fixture("filed-dirty.org"),
    );
    assert_eq!(output.status.code(), Some(1));
    assert_output_eq(&String::from_utf8(output.stdout).unwrap(), "");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("rejected (missing-tag)"), "{stderr}");
    assert!(stderr.contains("clock check-filed"), "{stderr}");
}

#[test]
fn clock_commands_ignore_filed_lines() {
    let path = fixture("filed.org");
    let list = run_clock(&["list", "-j", "--no-cache"], &path);
    assert_eq!(list.status.code(), Some(0));
    let stdout = String::from_utf8(list.stdout).unwrap();
    assert!(!stdout.contains("FILED:"), "{stdout}");
    assert_eq!(stdout.lines().count(), 1);

    let check = run_clock(&["check", "--no-color", "--no-cache"], &path);
    assert_eq!(check.status.code(), Some(0));
    assert_output_eq(
        &String::from_utf8(check.stdout).unwrap(),
        "All CLOCK lines OK.\n",
    );

    let overlap = run_clock(&["overlap", "-j", "--no-cache"], &path);
    assert_eq!(overlap.status.code(), Some(0));
    assert_output_eq(&String::from_utf8(overlap.stdout).unwrap(), "");
}

#[test]
fn clock_fix_leaves_filed_lines_byte_identical() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("filed.org");
    let before = indoc! {"
        * Frozen declaration
        FILED: [2024-04-16 Mon 14:15 +0200]--[2024-04-16 Mon 14:30 +0200] => 99:99  # lok
    "};
    std::fs::write(&path, before).unwrap();
    let output = run_clock(&["fix", "--write", "--no-color"], &path);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(std::fs::read_to_string(path).unwrap(), before);
}

#[test]
fn second_list_filed_run_is_a_cache_hit() {
    let cache = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let path = fixture("filed.org");
    let invoke = || {
        Command::new(bin_path())
            .args(["clock", "list-filed", "-j", "--timing"])
            .arg(&path)
            .env("ORG_CLI_CLOCK_CACHE", cache.path().join("clock.sqlite"))
            .env("XDG_CONFIG_HOME", config.path())
            .output()
            .unwrap()
    };
    let cold = invoke();
    let warm = invoke();
    assert_eq!(cold.status.code(), Some(0));
    assert_eq!(warm.status.code(), Some(0));
    assert_output_eq(
        &String::from_utf8(warm.stdout).unwrap(),
        &expected_json(&path),
    );
    let stderr = String::from_utf8(warm.stderr).unwrap();
    assert!(stderr.contains("l1-hits            1"), "{stderr}");
    assert!(stderr.contains("l1-misses          0"), "{stderr}");
}
