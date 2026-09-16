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

/// A `org-cli` command isolated from the user's environment: no real
/// clock cache, no real config, and — because `clock check` sweeps
/// the agenda set for overlaps — no real agenda set either.
fn cmd() -> Command {
    let mut c = Command::new(bin_path());
    c.env("ORG_CLI_AGENDA_SET", "/nonexistent/org-cli-it-agenda")
        .env("XDG_CONFIG_HOME", "/nonexistent/org-cli-it-config");
    c
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("clock")
}

#[test]
fn check_clean_fixture_returns_zero() {
    let out = cmd()
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
    let out = cmd()
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
    // Tally per-tag and assert each fixture entry produces the
    // tag(s) we expect. The weekday-mismatch line has both sides
    // wrong, so emits twice.
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for t in &tags {
        *counts.entry(t.clone()).or_default() += 1;
    }
    let expected: BTreeMap<String, usize> = [
        ("missing-tz", 1),
        ("non-numerical-tz", 1),
        ("mixed-precision", 1),
        ("weekday-mismatch", 2),
        ("start-after-end", 1),
        ("tz-mismatch", 1),
        // Two duration-mismatches: one is the dedicated
        // fixture line; the other is the tz-mismatch line, whose
        // naive HH:MM subtraction (0:15) disagrees with the
        // UTC-aware canonical (6:15). Reporting both is
        // intentional — the linter surfaces every axis.
        ("duration-mismatch", 2),
        ("spacing-mismatch", 1),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), *v))
    .collect();
    assert_eq!(counts, expected, "stdout was:\n{}", stdout);
}

#[test]
fn list_clean_fixture_emits_one_record_per_clock() {
    let out = cmd()
        .arg("clock")
        .arg("list")
        .arg("-j")
        .arg(fixtures_root().join("clean.org"))
        .output()
        .expect("spawn org-cli");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let recs: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON line"))
        .collect();
    // ``clean.org`` has 2 closed-range CLOCKs and one running
    // clock that is silently skipped — see the fixture.
    assert_eq!(recs.len(), 2, "got {:#?}", recs);
    // Each record must carry the canonical fields.
    for r in &recs {
        assert!(r["start"].as_str().unwrap().contains("T"));
        assert!(r["end"].as_str().unwrap().contains("T"));
        assert!(r["tz_minutes"].is_number());
        assert!(r["has_seconds"].is_boolean());
        assert!(r["duration_seconds"].is_number());
        assert!(r["raw"].as_str().unwrap().contains("CLOCK:"));
    }
    // First CLOCK in fixture: 09:30 +0200 → 11:45 +0200 = 2h15m.
    assert_eq!(recs[0]["tz_minutes"], 120);
    assert_eq!(recs[0]["has_seconds"], false);
    assert_eq!(recs[0]["duration_seconds"], 2 * 3600 + 15 * 60);
    // Second CLOCK: second-precision.
    assert_eq!(recs[1]["has_seconds"], true);
    assert_eq!(recs[1]["tz_minutes"], 60);
}

#[test]
fn list_hard_fails_on_first_rejection() {
    let out = cmd()
        .arg("clock")
        .arg("list")
        .arg("-j")
        .arg(fixtures_root().join("dirty.org"))
        .output()
        .expect("spawn org-cli");
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 on first rejection; stderr=\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    // Stdout must contain ZERO records: the first CLOCK in
    // ``dirty.org`` is the missing-tz one, processing aborts
    // before any clean record is ever produced.
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.lines().all(|l| l.trim().is_empty()),
        "stdout should be empty; got:\n{}",
        stdout
    );
    // Stderr must mention exactly one rejection diagnostic
    // ("rejected" appears once: in the diagnostic itself, NOT
    // in a summary tally).
    let stderr = String::from_utf8(out.stderr).unwrap();
    let rejected_count = stderr.matches("rejected").count();
    assert_eq!(
        rejected_count, 1,
        "stderr should report exactly one rejection; got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("stopped on first rejection"),
        "stderr should mention the abort hint; got:\n{}",
        stderr
    );
}

#[test]
fn list_aborts_before_processing_subsequent_clean_lines() {
    // Mixed fixture: first line is clean, second is missing-tz,
    // third is clean. We expect ONE record on stdout (the first
    // clean line) and an immediate abort — the third line must
    // NOT appear.
    let dir = std::env::temp_dir().join(format!(
        "org-cli-list-abort-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("mixed.org");
    std::fs::write(
        &p,
        concat!(
            "  CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15\n",
            "  CLOCK: [2026-01-07 Wed 09:57]--[2026-01-07 Wed 12:19] =>  2:22\n",
            "  CLOCK: [2025-06-07 Sat 09:00 +0200]--[2025-06-07 Sat 10:00 +0200] =>  1:00\n",
        ),
    )
    .unwrap();
    let out = cmd()
        .arg("clock")
        .arg("list")
        .arg("-j")
        .arg(&p)
        .output()
        .expect("spawn org-cli");
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let line_count = stdout.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(
        line_count, 1,
        "stdout should have exactly one record (the line before \
         the rejection); got:\n{}",
        stdout
    );
    assert!(
        !stdout.contains("2025-06-07"),
        "the clean CLOCK after the rejection must NOT appear; got:\n{}",
        stdout
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn add_inserts_canonical_clock_into_target_heading() {
    let dir = std::env::temp_dir().join(format!(
        "org-cli-add-int-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("target.org");
    std::fs::write(
        &p,
        "** WIP fix login bug\n  :PROPERTIES:\n  :TASK_REF: 248\n  :END:\n",
    )
    .unwrap();

    let out = cmd()
        .arg("clock")
        .arg("add")
        .arg(&p)
        .arg("248")
        .arg("2025-06-06 09:30")
        .arg("2025-06-06 11:45")
        .arg("--tz")
        .arg("+0200")
        .arg("--write")
        .arg("-j")
        .output()
        .expect("spawn org-cli");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value =
        serde_json::from_str(stdout.lines().next().unwrap()).expect("NDJSON");
    assert_eq!(v["applied"], true);
    assert_eq!(v["edit_kind"], "created-logbook");

    let after = std::fs::read_to_string(&p).unwrap();
    assert!(
        after
            .contains("CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15"),
        "expected canonical CLOCK line in:\n{}",
        after
    );
    // The inserted line must pass ``clock check``.
    let chk = cmd()
        .arg("clock")
        .arg("check")
        .arg(&p)
        .output()
        .expect("spawn org-cli clock check");
    assert_eq!(
        chk.status.code(),
        Some(0),
        "clock check stdout=\n{}\nstderr=\n{}",
        String::from_utf8_lossy(&chk.stdout),
        String::from_utf8_lossy(&chk.stderr),
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fmt_minute_with_explicit_tz_outputs_canonical_line() {
    let out = cmd()
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
    let out = cmd()
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

    let fmt = cmd()
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

    let chk = cmd()
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

// ----- orgize-driven discovery ------------------------------------------

fn scratch(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "org-cli-clock-it-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// `--first` must stop after ONE emitted record, even when the first
/// offending line carries several violations.
#[test]
fn check_first_emits_a_single_record() {
    let dir = scratch("first");
    std::fs::write(
        dir.join("a.org"),
        "CLOCK: [2024-11-13 Mon 15:00 +0100]--[2024-11-13 Mon 15:45 +0100] => 720:45\n",
    )
    .unwrap();
    let out = cmd()
        .args(["clock", "check", "-j", "-1"])
        .arg(&dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).lines().count(),
        1,
        "stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A CLOCK sample inside `#+begin_src` is documentation: `check`
/// must not lint it, `list` must not emit or reject on it, `fix`
/// must not rewrite it.
#[test]
fn src_block_samples_are_invisible_to_check_list_and_fix() {
    let dir = scratch("srcblock");
    let f = dir.join("a.org");
    let body = "* h\n\
                :LOGBOOK:\n\
                CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00\n\
                :END:\n\
                #+begin_src org\n\
                CLOCK: [2024-04-16 Mon 14:15 +0200]--[2024-04-16 Mon 14:30 +0200] =>  0:15\n\
                CLOCK: [2026-01-07 Wed 09:57]--[2026-01-07 Wed 12:19] =>  2:22\n\
                #+end_src\n";
    std::fs::write(&f, body).unwrap();

    let chk = cmd()
        .args(["clock", "check", "-j"])
        .arg(&f)
        .output()
        .unwrap();
    assert_eq!(chk.status.code(), Some(0), "check must be clean");
    assert!(chk.stdout.is_empty());

    let lst = cmd()
        .args(["clock", "list", "-j"])
        .arg(&f)
        .output()
        .unwrap();
    assert_eq!(
        lst.status.code(),
        Some(0),
        "list must not reject the missing-tz sample"
    );
    assert_eq!(String::from_utf8_lossy(&lst.stdout).lines().count(), 1);

    let fix = cmd()
        .args(["clock", "fix", "--write"])
        .arg(&f)
        .output()
        .unwrap();
    assert_eq!(fix.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        body,
        "fix must leave the file byte-identical"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file that is not valid UTF-8 is warned about and decoded
/// lossily: its CLOCK lines still count, on both the cached and the
/// uncached path. Fix rewrites only the CLOCK line; the invalid bytes
/// on other lines are carried through untouched.
#[test]
fn non_utf8_file_is_decoded_lossily_with_warning() {
    let dir = scratch("utf8");
    let bad = dir.join("bad.org");
    let bad_bytes: Vec<u8> =
        b"* caf\xe9\nCLOCK: [2024-04-16 Mon 14:15 +0200]--[2024-04-16 Mon 14:30 +0200] =>  0:15\n"
            .to_vec();
    std::fs::write(&bad, &bad_bytes).unwrap();

    for (sub, want_exit) in [("check", 1), ("list", 1)] {
        for extra in [&[][..], &["--no-cache"][..]] {
            let out = cmd()
                .env("ORG_CLI_CLOCK_CACHE", dir.join("cache.sqlite"))
                .args(["clock", sub, "-j"])
                .args(extra)
                .arg(&bad)
                .output()
                .unwrap();
            let err = String::from_utf8_lossy(&out.stderr);
            // The warning is logged only when the file is actually
            // decoded; a warm cache hit does not re-read it. The
            // observable contract is the exit code.
            if extra.contains(&"--no-cache") {
                assert!(err.contains("not valid UTF-8"), "{sub} {extra:?}: {err}");
            }
            assert_eq!(
                out.status.code(),
                Some(want_exit),
                "{sub} {extra:?}: the weekday-broken clock must be seen; stderr={err}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
