//! End-to-end test for the `clock` subcommand.
//!
//! Spawns the actual `org-cli` binary and exercises both
//! `clock check` (against fixture org files) and `clock fmt`
//! (against canonical inputs). This complements the unit tests in
//! `src/cli/action/clock.rs::tests` by catching CLI wiring issues.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_org-cli"))
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("clock")
}

#[test]
fn check_clean_fixture_returns_zero() {
    let out = Command::new(bin_path())
        .arg("clock")
        .arg("check")
        .arg(fixtures_root().join("clean.org"))
        .output()
        .expect("spawn org-cli");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_dirty_fixture_returns_one_with_json() {
    let out = Command::new(bin_path())
        .arg("clock")
        .arg("check")
        .arg("-j")
        .arg(fixtures_root().join("dirty.org"))
        .output()
        .expect("spawn org-cli");
    assert_eq!(out.status.code(), Some(1));

    let stdout = String::from_utf8(out.stdout).unwrap();
    let mut tags: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        tags.push(v["tag"].as_str().unwrap().to_string());
    }
    tags.sort();
    // Each malformed line in dirty.org should be reported once.
    let mut expected = vec![
        "missing-tz",
        "non-numerical-tz",
        "mixed-precision",
        "weekday-mismatch",
        "start-after-end",
    ]
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(tags, expected, "stdout was:\n{}", stdout);
}

#[test]
fn fmt_minute_with_explicit_tz_outputs_canonical_line() {
    let out = Command::new(bin_path())
        .arg("clock")
        .arg("fmt")
        .arg("2025-06-06 09:30")
        .arg("2025-06-06 11:45")
        .arg("--tz")
        .arg("+0200")
        .output()
        .expect("spawn org-cli");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        s.trim_end(),
        "CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15"
    );
}

#[test]
fn fmt_with_seconds_outputs_canonical_line() {
    let out = Command::new(bin_path())
        .arg("clock")
        .arg("fmt")
        .arg("2026-01-07 09:57:20")
        .arg("2026-01-07 12:19:25")
        .arg("--tz")
        .arg("+0100")
        .arg("--seconds")
        .output()
        .expect("spawn org-cli");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        s.trim_end(),
        "CLOCK: [2026-01-07 Wed 09:57:20 +0100]--[2026-01-07 Wed 12:19:25 +0100] =>  2:22:05"
    );
}

#[test]
fn fmt_round_trips_through_check() {
    // Generate a CLOCK line via `clock fmt`, then feed it into a
    // tiny org file that `clock check` validates clean.
    let dir = std::env::temp_dir().join(format!(
        "org-cli-clock-roundtrip-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let fmt = Command::new(bin_path())
        .arg("clock")
        .arg("fmt")
        .arg("2025-06-06 09:23")
        .arg("2025-06-06 11:47")
        .arg("--tz")
        .arg("+0200")
        .arg("--grid")
        .arg("15")
        .output()
        .expect("spawn org-cli");
    assert_eq!(fmt.status.code(), Some(0));
    let line = String::from_utf8(fmt.stdout).unwrap();
    let line = line.trim_end();
    assert_eq!(
        line,
        "CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15"
    );

    let p = dir.join("rt.org");
    std::fs::write(&p, format!("  :LOGBOOK:\n  {}\n  :END:\n", line)).unwrap();

    let chk = Command::new(bin_path())
        .arg("clock")
        .arg("check")
        .arg(&p)
        .output()
        .expect("spawn org-cli");
    assert_eq!(
        chk.status.code(),
        Some(0),
        "check failed; stdout={} stderr={}",
        String::from_utf8_lossy(&chk.stdout),
        String::from_utf8_lossy(&chk.stderr)
    );

    let _ = std::fs::remove_dir_all(&dir);
}
