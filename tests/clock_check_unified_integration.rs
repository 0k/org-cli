//! End-to-end tests for the unified `org-cli clock check`.
//!
//! `clock check FILE…` is the single gate for "is this file's time
//! bookkeeping sound?". It runs three rule sets and folds them into
//! one exit code:
//!
//! 1. CLOCK per-line format (the historical `check`),
//! 2. FILED grammar + customer tag (formerly `check-filed`),
//! 3. overlap — the one cross-entry rule (also exposed as the
//!    pair-oriented `clock overlap` view).
//!
//! The overlap pass is corpus-wide: a CLOCK just written to one file
//! may collide with a CLOCK in another agenda file, so the sweep
//! covers `PATHS ∪ agenda set` and reports only pairs that touch
//! the named PATHS. Every test here pins `HOME`, the clock cache and
//! the config dir to a scratch directory so the user's real agenda
//! set and cache are never read.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use indoc::indoc;

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_org-cli"))
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("clock")
}

/// One isolated world: a fake `$HOME` (so the agenda-set fallback
/// resolves inside it), a private clock cache and a private config
/// dir. Dropped with the test.
struct World {
    root: tempfile::TempDir,
}

impl World {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let p = self.path(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, content).unwrap();
        p
    }

    /// Declare the agenda set: one absolute path per line, at the
    /// location `clock check` reads when no PATHS are given and for
    /// the corpus-wide overlap sweep.
    fn agenda(&self, files: &[&Path]) {
        let body: String = files.iter().map(|f| format!("{}\n", f.display())).collect();
        self.write(".cache/emacs/org-collect/org-agenda-files.tmp", &body);
    }

    fn config(&self, yaml: &str) {
        self.write(".config/org-cli/config.yml", yaml);
    }

    /// Run `org-cli clock check ARGS…` inside this world.
    fn check(&self, args: &[&str]) -> (Option<i32>, String, String) {
        self.run_clock("check", args)
    }

    fn run_clock(&self, sub: &str, args: &[&str]) -> (Option<i32>, String, String) {
        let out = Command::new(bin_path())
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.path(".config"))
            .env("XDG_CACHE_HOME", self.path(".cache"))
            .env(
                "ORG_CLI_CLOCK_CACHE",
                self.path(".cache/org-cli/clock.sqlite"),
            )
            .env("NO_COLOR", "1")
            .arg("clock")
            .arg(sub)
            .args(args)
            .output()
            .expect("spawn org-cli");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn json_lines(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad JSON {l:?}: {e}")))
        .collect()
}

const CLEAN_CLOCK_A: &str = indoc! {"
    * A
    :LOGBOOK:
    CLOCK: [2026-03-02 Mon 09:00 +0100]--[2026-03-02 Mon 10:00 +0100] =>  1:00
    :END:
"};

// ---------------------------------------------------------------------
// Pass 2: FILED lines are linted by `check`
// ---------------------------------------------------------------------

#[test]
fn check_reports_dirty_filed_lines_alongside_clean_clocks() {
    let w = World::new();
    let f = fixtures_root().join("filed-dirty.org");
    let (code, stdout, _) = w.check(&["-j", s(&f)]);
    assert_eq!(code, Some(1));
    let recs = json_lines(&stdout);
    let has = |tag: &str| recs.iter().any(|r| r["tag"] == tag);
    assert!(has("missing-tag"), "{stdout}");
    assert!(has("invalid-tag"), "{stdout}");
}

#[test]
fn check_clean_file_with_valid_filed_lines_passes_with_three_ok_lines() {
    let w = World::new();
    let f = fixtures_root().join("filed.org");
    let (code, stdout, stderr) = w.check(&[s(&f)]);
    assert_eq!(code, Some(0), "stderr={stderr}");
    assert_eq!(
        stdout,
        "All CLOCK lines OK.\nAll FILED lines OK.\nNo overlapping CLOCK entries.\n"
    );
}

#[test]
fn check_filed_subcommand_no_longer_exists() {
    let w = World::new();
    let f = fixtures_root().join("filed.org");
    let (code, _, stderr) = w.run_clock("check-filed", &[s(&f)]);
    assert_ne!(code, Some(0));
    assert!(
        stderr.contains("unrecognized subcommand"),
        "check-filed must be gone; stderr={stderr}"
    );
}

// ---------------------------------------------------------------------
// Pass 3: overlap inside the named file
// ---------------------------------------------------------------------

#[test]
fn check_reports_overlapping_clocks_within_one_file() {
    let w = World::new();
    let f = fixtures_root().join("overlap-dirty.org");
    let (code, stdout, stderr) = w.check(&["-j", s(&f)]);
    assert_eq!(code, Some(1), "stderr={stderr}");
    let recs = json_lines(&stdout);
    // Every line is individually canonical, so ONLY overlap
    // violations may appear — one per pair, four pairs in the fixture.
    assert!(
        recs.iter().all(|r| r["tag"] == "overlap"),
        "non-overlap tags leaked: {stdout}"
    );
    assert_eq!(recs.len(), 4, "{stdout}");
    // Human summary must not claim the corpus is overlap-free.
    let (_, human, _) = w.check(&[s(&f)]);
    assert!(human.contains("All CLOCK lines OK."), "{human}");
    assert!(human.contains("All FILED lines OK."), "{human}");
    assert!(!human.contains("No overlapping"), "{human}");
    assert!(human.contains("4 overlapping pair(s) found"), "{human}");
}

#[test]
fn overlap_violation_record_names_both_sides_and_the_shared_span() {
    let w = World::new();
    let f = w.write(
        "dup.org",
        indoc! {"
            * Partial overlap
            :LOGBOOK:
            CLOCK: [2026-02-02 Mon 10:00 +0100]--[2026-02-02 Mon 12:00 +0100] =>  2:00
            CLOCK: [2026-02-02 Mon 11:00 +0100]--[2026-02-02 Mon 13:00 +0100] =>  2:00
            :END:
        "},
    );
    let (code, stdout, _) = w.check(&["-j", s(&f)]);
    assert_eq!(code, Some(1));
    let recs = json_lines(&stdout);
    assert_eq!(recs.len(), 1, "{stdout}");
    let r = &recs[0];
    assert_eq!(r["tag"], "overlap");
    // Anchored on the earlier-starting line, like `clock overlap`.
    assert_eq!(r["line"], 3);
    assert!(r["raw"].as_str().unwrap().contains("10:00 +0100]--"), "{r}");
    let other = &r["other"];
    assert_eq!(other["line"], 4, "{r}");
    assert_eq!(
        Path::new(other["file"].as_str().unwrap()),
        f.canonicalize().unwrap()
    );
    assert!(
        other["raw"].as_str().unwrap().contains("11:00 +0100]--"),
        "{r}"
    );
    assert_eq!(r["overlap_seconds"], 3600, "{r}");
    let msg = r["message"].as_str().unwrap();
    assert!(msg.contains("1:00:00"), "shared span in message: {msg}");
    assert!(msg.contains(":4"), "other side's line in message: {msg}");
}

#[test]
fn per_line_violations_do_not_carry_an_other_side() {
    let w = World::new();
    let f = fixtures_root().join("dirty.org");
    let (_, stdout, _) = w.check(&["-j", s(&f)]);
    for r in json_lines(&stdout) {
        assert_ne!(r["tag"], "overlap", "{r}");
        assert!(
            r.get("other").is_none() || r["other"].is_null(),
            "per-line record must not carry `other`: {r}"
        );
    }
}

#[test]
fn adjacent_and_zone_shifted_clocks_are_not_overlaps() {
    let w = World::new();
    let f = fixtures_root().join("overlap-clean.org");
    let (code, stdout, stderr) = w.check(&[s(&f)]);
    assert_eq!(code, Some(0), "stdout={stdout} stderr={stderr}");
    assert!(stdout.contains("No overlapping CLOCK entries."), "{stdout}");
}

// ---------------------------------------------------------------------
// Pass 3: overlap against the rest of the agenda set
// ---------------------------------------------------------------------

#[test]
fn check_of_one_file_catches_overlap_with_another_agenda_file() {
    let w = World::new();
    let mine = w.write("mine.org", CLEAN_CLOCK_A);
    let theirs = w.write(
        "theirs.org",
        indoc! {"
            * B
            :LOGBOOK:
            CLOCK: [2026-03-02 Mon 09:30 +0100]--[2026-03-02 Mon 10:30 +0100] =>  1:00
            :END:
        "},
    );
    w.agenda(&[&mine, &theirs]);

    let (code, stdout, stderr) = w.check(&["-j", s(&mine)]);
    assert_eq!(code, Some(1), "stderr={stderr}");
    let recs = json_lines(&stdout);
    assert_eq!(recs.len(), 1, "{stdout}");
    let r = &recs[0];
    assert_eq!(r["tag"], "overlap");
    // Anchored on the NAMED file even though `theirs` sorts later.
    assert_eq!(
        Path::new(r["file"].as_str().unwrap()),
        mine.canonicalize().unwrap()
    );
    assert_eq!(
        Path::new(r["other"]["file"].as_str().unwrap()),
        theirs.canonicalize().unwrap()
    );
}

#[test]
fn overlaps_between_two_unnamed_agenda_files_are_not_reported() {
    let w = World::new();
    let mine = w.write("mine.org", CLEAN_CLOCK_A);
    let x = w.write(
        "x.org",
        indoc! {"
            * X
            :LOGBOOK:
            CLOCK: [2026-04-01 Wed 09:00 +0200]--[2026-04-01 Wed 10:00 +0200] =>  1:00
            :END:
        "},
    );
    let y = w.write(
        "y.org",
        indoc! {"
            * Y
            :LOGBOOK:
            CLOCK: [2026-04-01 Wed 09:00 +0200]--[2026-04-01 Wed 10:00 +0200] =>  1:00
            :END:
        "},
    );
    w.agenda(&[&mine, &x, &y]);

    // Scoped: x/y's problem is not mine.org's problem.
    let (code, stdout, stderr) = w.check(&[s(&mine)]);
    assert_eq!(code, Some(0), "stdout={stdout} stderr={stderr}");

    // Unscoped (no PATHS): the whole agenda set is in scope.
    let (code, stdout, _) = w.check(&["-j"]);
    assert_eq!(code, Some(1));
    let recs = json_lines(&stdout);
    assert_eq!(recs.len(), 1, "{stdout}");
    assert_eq!(recs[0]["tag"], "overlap");
}

#[test]
fn named_path_given_relative_is_matched_against_the_agenda_entry() {
    let w = World::new();
    let mine = w.write("sub/mine.org", CLEAN_CLOCK_A);
    let theirs = w.write(
        "theirs.org",
        indoc! {"
            * B
            :LOGBOOK:
            CLOCK: [2026-03-02 Mon 09:30 +0100]--[2026-03-02 Mon 10:30 +0100] =>  1:00
            :END:
        "},
    );
    w.agenda(&[&mine, &theirs]);

    // Same file named two ways: relative from cwd, absolute in the
    // agenda. Must count as ONE file (one pair), not two.
    let out = Command::new(bin_path())
        .current_dir(w.path("sub"))
        .env("HOME", w.root.path())
        .env("XDG_CONFIG_HOME", w.path(".config"))
        .env("XDG_CACHE_HOME", w.path(".cache"))
        .env("ORG_CLI_CLOCK_CACHE", w.path(".cache/org-cli/clock.sqlite"))
        .args(["clock", "check", "-j", "mine.org"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(json_lines(&stdout).len(), 1, "{stdout}");
}

#[test]
fn missing_agenda_set_degrades_to_named_paths_with_a_warning() {
    let w = World::new();
    // No agenda file written at all.
    let a = w.write(
        "a.org",
        indoc! {"
            * A
            :LOGBOOK:
            CLOCK: [2026-02-04 Wed 14:00 +0100]--[2026-02-04 Wed 15:00 +0100] =>  1:00
            CLOCK: [2026-02-04 Wed 14:00 +0100]--[2026-02-04 Wed 15:00 +0100] =>  1:00
            :END:
        "},
    );
    let (code, stdout, stderr) = w.check(&["-j", s(&a)]);
    // In-file overlap is still found…
    assert_eq!(code, Some(1), "stderr={stderr}");
    assert_eq!(json_lines(&stdout).len(), 1, "{stdout}");
    // …and the narrowed sweep is announced, not silent.
    assert!(
        stderr.contains("agenda set") && stderr.contains("named paths only"),
        "stderr={stderr}"
    );

    // Without PATHS there is nothing to check: hard error, as before.
    let (code, _, stderr) = w.check(&[]);
    assert_ne!(code, Some(0));
    assert!(stderr.contains("agenda set"), "stderr={stderr}");
}

// ---------------------------------------------------------------------
// Exclusions
// ---------------------------------------------------------------------

#[test]
fn configured_overlap_exclude_globs_hide_evidence_files_from_the_sweep() {
    let w = World::new();
    let mine = w.write("mine.org", CLEAN_CLOCK_A);
    let evidence = w.write(
        "keep/when/2026-03-02.org",
        indoc! {"
            * Segment
            :LOGBOOK:
            CLOCK: [2026-03-02 Mon 09:00 +0100]--[2026-03-02 Mon 12:00 +0100] =>  3:00
            :END:
        "},
    );
    w.agenda(&[&mine, &evidence]);

    // Without the exclusion the evidence CLOCK collides with mine.
    let (code, _, stderr) = w.check(&[s(&mine)]);
    assert_eq!(code, Some(1), "stderr={stderr}");

    w.config(&format!(
        "clock:\n  overlap:\n    exclude-globs:\n      - \"{}/keep/when/**\"\n",
        w.root.path().display()
    ));
    let (code, stdout, stderr) = w.check(&[s(&mine)]);
    assert_eq!(code, Some(0), "stdout={stdout} stderr={stderr}");
}

#[test]
fn command_line_exclude_narrows_both_lint_and_overlap() {
    let w = World::new();
    let bad = w.write(
        "bad.org",
        "CLOCK: [2026-01-07 Wed 09:57]--[2026-01-07 Wed 12:19] =>  2:22\n",
    );
    let dup = w.write(
        "dup.org",
        indoc! {"
            * D
            :LOGBOOK:
            CLOCK: [2026-02-04 Wed 14:00 +0100]--[2026-02-04 Wed 15:00 +0100] =>  1:00
            CLOCK: [2026-02-04 Wed 14:00 +0100]--[2026-02-04 Wed 15:00 +0100] =>  1:00
            :END:
        "},
    );
    w.agenda(&[&bad, &dup]);
    let (code, _, _) = w.check(&[s(w.root.path())]);
    assert_eq!(code, Some(1));
    let (code, stdout, stderr) = w.check(&[
        "--exclude",
        "**/bad.org",
        "--exclude",
        "**/dup.org",
        s(w.root.path()),
    ]);
    assert_eq!(code, Some(0), "stdout={stdout} stderr={stderr}");
}

// ---------------------------------------------------------------------
// Ordering, --first, output formats
// ---------------------------------------------------------------------

#[test]
fn first_stops_after_one_record_even_when_only_the_overlap_pass_hits() {
    let w = World::new();
    let f = fixtures_root().join("overlap-dirty.org");
    let (code, stdout, _) = w.check(&["-j", "-1", s(&f)]);
    assert_eq!(code, Some(1));
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert_eq!(json_lines(&stdout)[0]["tag"], "overlap");
}

#[test]
fn non_canonical_lines_are_reported_once_not_also_as_sweep_warnings() {
    let w = World::new();
    let f = w.write(
        "mixed.org",
        indoc! {"
            * M
            :LOGBOOK:
            CLOCK: [2026-01-07 Wed 09:57]--[2026-01-07 Wed 12:19] =>  2:22
            CLOCK: [2026-02-04 Wed 14:00 +0100]--[2026-02-04 Wed 15:00 +0100] =>  1:00
            CLOCK: [2026-02-04 Wed 14:00 +0100]--[2026-02-04 Wed 15:00 +0100] =>  1:00
            :END:
        "},
    );
    let (code, stdout, stderr) = w.check(&["-j", s(&f)]);
    assert_eq!(code, Some(1));
    let recs = json_lines(&stdout);
    // Per-line hits first (source order), then the overlap pair.
    let tags: Vec<&str> = recs.iter().map(|r| r["tag"].as_str().unwrap()).collect();
    assert_eq!(tags.last().copied(), Some("overlap"), "{stdout}");
    assert!(tags.iter().any(|t| *t == "missing-tz"), "{stdout}");
    assert!(
        !stderr.contains("skipping non-canonical"),
        "pass 1 already reported the line; stderr={stderr}"
    );
}

#[test]
fn nul_output_appends_the_other_side_as_extra_fields() {
    let w = World::new();
    let f = w.write(
        "dup.org",
        indoc! {"
            * D
            :LOGBOOK:
            CLOCK: [2026-02-04 Wed 14:00 +0100]--[2026-02-04 Wed 15:00 +0100] =>  1:00
            CLOCK: [2026-02-04 Wed 14:00 +0100]--[2026-02-04 Wed 15:00 +0100] =>  1:00
            :END:
        "},
    );
    let out = Command::new(bin_path())
        .env("HOME", w.root.path())
        .env("XDG_CONFIG_HOME", w.path(".config"))
        .env("XDG_CACHE_HOME", w.path(".cache"))
        .env("ORG_CLI_CLOCK_CACHE", w.path(".cache/org-cli/clock.sqlite"))
        .args(["clock", "check", "-0"])
        .arg(&f)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    // One record. The standard five fields, then the other side
    // appended so existing field indices stay stable:
    // <file>\0<line>\0<tag>\0<message>\0<raw>\0<ofile>\0<oline>\0<oraw>\0\0
    // Splitting on NUL yields the 8 fields, one empty string for the
    // record terminator, and one trailing empty string.
    let fields: Vec<&[u8]> = out.stdout.split(|b| *b == 0).collect();
    assert_eq!(fields.len(), 10, "fields={}", fields.len());
    assert_eq!(fields[2], b"overlap");
    assert_eq!(fields[6], b"4");
    assert!(fields[7].starts_with(b"CLOCK:"));
    assert!(fields[8].is_empty() && fields[9].is_empty());
}

#[test]
fn stale_hint_in_list_abort_points_at_check_not_check_filed() {
    let w = World::new();
    let f = fixtures_root().join("filed-dirty.org");
    let (code, _, stderr) = w.run_clock("list-filed", &["-j", "--no-cache", s(&f)]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("org-cli clock check``"), "{stderr}");
    assert!(!stderr.contains("check-filed"), "{stderr}");
}
