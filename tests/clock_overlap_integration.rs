//! End-to-end tests for `org-cli clock overlap`.
//!
//! Spawns the actual binary against fixture org files. This
//! complements the pure-algorithm tests in
//! `src/org/clock_overlap.rs::tests` and the orchestration tests in
//! `src/cli/action/clock.rs::tests` by catching CLI wiring issues:
//! flag parsing, exit codes, and the three output formats.

use std::cmp::Ordering;
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

/// Minimal tempdir helper, mirroring the one in the unit tests.
struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        p.push(format!("org-cli-overlap-it-{}-{}-{}", tag, pid, nanos));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(p: &Path, s: &str) {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, s).unwrap();
}

/// Run `clock overlap` with the given extra args, returning
/// `(exit_code, stdout, stderr)`.
///
/// Every invocation gets its own throw-away cache file so tests
/// never read the user's real cache nor each other's.
fn run(args: &[&str]) -> (Option<i32>, String, String) {
    let cache = TempDir::new("cache");
    run_with_cache(&cache, args)
}

fn run_with_cache(cache: &TempDir, args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(bin_path())
        .env("ORG_CLI_CLOCK_CACHE", cache.path().join("clock.sqlite"))
        .env("XDG_CONFIG_HOME", cache.path().join("config"))
        .env("ORG_CLI_AGENDA_SET", "/nonexistent/org-cli-it-agenda")
        .arg("clock")
        .arg("overlap")
        .args(args)
        .output()
        .expect("spawn org-cli");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn write_sort_fixture(td: &TempDir) -> PathBuf {
    let file = td.path().join("sort.org");
    write(
        &file,
        indoc! {"
            * Nested clocks
            CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 18:00 +0100] =>  8:00
            CLOCK: [2026-01-07 Wed 11:00 +0100]--[2026-01-07 Wed 14:00 +0100] =>  3:00
            CLOCK: [2026-01-07 Wed 12:00 +0100]--[2026-01-07 Wed 17:00 +0100] =>  5:00
            CLOCK: [2026-01-07 Wed 13:00 +0100]--[2026-01-07 Wed 15:00 +0100] =>  2:00
        "},
    );
    file
}

fn json_records(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON line"))
        .collect()
}

fn pair_ids(records: &[serde_json::Value]) -> Vec<(u64, u64)> {
    records
        .iter()
        .map(|record| {
            (
                record["a"]["line"].as_u64().unwrap(),
                record["b"]["line"].as_u64().unwrap(),
            )
        })
        .collect()
}

fn record_field_cmp(a: &serde_json::Value, b: &serde_json::Value, field: &str) -> Ordering {
    match field {
        "start" => a["b"]["start"]
            .as_str()
            .unwrap()
            .cmp(b["b"]["start"].as_str().unwrap()),
        "end" => {
            let a_end = a["a"]["end"]
                .as_str()
                .unwrap()
                .min(a["b"]["end"].as_str().unwrap());
            let b_end = b["a"]["end"]
                .as_str()
                .unwrap()
                .min(b["b"]["end"].as_str().unwrap());
            a_end.cmp(b_end)
        }
        "duration" => a["overlap_seconds"]
            .as_i64()
            .unwrap()
            .cmp(&b["overlap_seconds"].as_i64().unwrap()),
        _ => unreachable!("test field is known"),
    }
}

// ----- fixtures ---------------------------------------------------------

#[test]
fn clean_fixture_returns_zero() {
    let f = fixtures_root().join("overlap-clean.org");
    let (code, stdout, stderr) = run(&[f.to_str().unwrap(), "-j"]);
    assert_eq!(code, Some(0), "stderr={}", stderr);
    assert!(
        stdout.trim().is_empty(),
        "clean fixture must emit no records, got: {}",
        stdout
    );
}

#[test]
fn clean_fixture_also_passes_clock_check() {
    // Guards the premise of the clean fixture: it must be
    // canonical, otherwise `clock overlap` would be silent merely
    // because every line got skipped.
    let out = Command::new(bin_path())
        .env("ORG_CLI_AGENDA_SET", "/nonexistent/org-cli-it-agenda")
        .env("XDG_CONFIG_HOME", "/nonexistent/org-cli-it-config")
        .arg("clock")
        .arg("check")
        .arg(fixtures_root().join("overlap-clean.org"))
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
fn dirty_fixture_also_passes_per_line_clock_check() {
    // The dirty fixture is dirty *only* in the overlap sense; each
    // individual line must still be canonical. `clock check` now
    // runs the overlap rule too, so look at the per-line records
    // rather than the exit code.
    let out = Command::new(bin_path())
        .env("ORG_CLI_AGENDA_SET", "/nonexistent/org-cli-it-agenda")
        .env("XDG_CONFIG_HOME", "/nonexistent/org-cli-it-config")
        .arg("clock")
        .arg("check")
        .arg("-j")
        .arg(fixtures_root().join("overlap-dirty.org"))
        .output()
        .expect("spawn org-cli");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["tag"], "overlap", "per-line violation leaked: {line}");
    }
}

#[test]
fn dirty_fixture_returns_one_and_reports_every_pair() {
    let f = fixtures_root().join("overlap-dirty.org");
    let (code, stdout, stderr) = run(&[f.to_str().unwrap(), "-j"]);
    assert_eq!(code, Some(1), "stderr={}", stderr);

    let mut spans: Vec<i64> = Vec::new();
    for line in stdout.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        spans.push(v["overlap_seconds"].as_i64().unwrap());
    }
    spans.sort_unstable();

    // Four independent conflicts in the fixture:
    //   partial overlap  → 1h    (3600)
    //   full containment → 1h    (3600)
    //   exact duplicate  → 1h    (3600)
    //   cross-timezone   → 1h30  (5400)
    assert_eq!(spans, vec![3600, 3600, 3600, 5400], "stdout={}", stdout);
}

// ----- output formats ---------------------------------------------------

#[test]
fn json_record_carries_both_sides() {
    let f = fixtures_root().join("overlap-dirty.org");
    let (_, stdout, _) = run(&[f.to_str().unwrap(), "-j"]);
    let first = stdout.lines().next().expect("at least one record");
    let v: serde_json::Value = serde_json::from_str(first).unwrap();

    for side in ["a", "b"] {
        assert!(v[side]["file"].is_string(), "missing {}.file", side);
        assert!(v[side]["line"].is_u64(), "missing {}.line", side);
        assert!(v[side]["start"].is_string(), "missing {}.start", side);
        assert!(v[side]["end"].is_string(), "missing {}.end", side);
        assert!(v[side]["raw"].is_string(), "missing {}.raw", side);
    }
    assert!(v["overlap_seconds"].is_i64());

    // `a` is the earlier-starting side.
    let a_start = v["a"]["start"].as_str().unwrap();
    let b_start = v["b"]["start"].as_str().unwrap();
    assert!(
        a_start <= b_start,
        "a={} must sort before b={}",
        a_start,
        b_start
    );
}

/// A file that is not valid UTF-8 (a stray Latin-1 byte in prose) is
/// indexed with a lossy decode, so its pairs are found. Rendering
/// must read the source lines back the same way: the `raw` field
/// carries the CLOCK text, not an empty string.
#[test]
fn non_utf8_file_still_renders_raw_clock_lines() {
    let td = TempDir::new("latin1");
    let file = td.path().join("latin1.org");
    let mut bytes = b"* Op\xe9rationnelle\n".to_vec();
    bytes.extend_from_slice(
        indoc! {"
            CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00
            CLOCK: [2026-01-07 Wed 11:00 +0100]--[2026-01-07 Wed 13:00 +0100] =>  2:00
        "}
        .as_bytes(),
    );
    fs::write(&file, bytes).unwrap();

    let (code, stdout, _) = run(&[file.to_str().unwrap(), "-j"]);
    assert_eq!(code, Some(1));
    let records = json_records(&stdout);
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["a"]["raw"],
        "CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00"
    );
    assert_eq!(
        records[0]["b"]["raw"],
        "CLOCK: [2026-01-07 Wed 11:00 +0100]--[2026-01-07 Wed 13:00 +0100] =>  2:00"
    );
}

#[test]
fn json_instants_are_rfc3339_utc() {
    let td = TempDir::new("rfc3339");
    write(
        td.path().join("a.org").as_path(),
        "* h\n\
         CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00\n\
         CLOCK: [2026-01-07 Wed 11:00 +0100]--[2026-01-07 Wed 13:00 +0100] =>  2:00\n",
    );
    let (_, stdout, _) = run(&[td.path().to_str().unwrap(), "-j"]);
    let v: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    // 10:00 +0100 is 09:00 UTC — guards the era/Unix epoch offset.
    assert_eq!(v["a"]["start"].as_str().unwrap(), "2026-01-07T09:00:00Z");
    assert_eq!(v["a"]["end"].as_str().unwrap(), "2026-01-07T11:00:00Z");
}

#[test]
fn nul_output_has_seven_fields_per_record() {
    let td = TempDir::new("nul");
    write(
        td.path().join("a.org").as_path(),
        "* h\n\
         CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00\n\
         CLOCK: [2026-01-07 Wed 11:00 +0100]--[2026-01-07 Wed 13:00 +0100] =>  2:00\n",
    );
    let (code, stdout, _) = run(&[td.path().to_str().unwrap(), "-0"]);
    assert_eq!(code, Some(1));

    // Records are terminated by a double NUL.
    let records: Vec<&str> = stdout.split("\0\0").filter(|s| !s.is_empty()).collect();
    assert_eq!(records.len(), 1, "expected exactly one record");
    let fields: Vec<&str> = records[0].split('\0').collect();
    assert_eq!(fields.len(), 7, "fields={:?}", fields);
    assert!(fields[0].ends_with("a.org"));
    assert_eq!(fields[1], "2"); // a.line
    assert!(fields[2].ends_with("a.org"));
    assert_eq!(fields[3], "3"); // b.line
    assert_eq!(fields[4], "3600"); // overlap_seconds
    assert!(fields[5].contains("10:00"));
    assert!(fields[6].contains("11:00"));
}

#[test]
fn human_output_names_both_locations() {
    let f = fixtures_root().join("overlap-dirty.org");
    let (code, stdout, _) = run(&[f.to_str().unwrap()]);
    assert_eq!(code, Some(1));
    assert!(stdout.contains("overlap"), "stdout={}", stdout);
    assert!(
        stdout.contains("overlap-dirty.org"),
        "must name the file: {}",
        stdout
    );
    assert!(
        stdout.contains("overlapping CLOCK pair(s) found"),
        "must summarise: {}",
        stdout
    );
}

#[test]
fn human_output_reports_clean_corpus_explicitly() {
    let f = fixtures_root().join("overlap-clean.org");
    let (code, stdout, _) = run(&[f.to_str().unwrap()]);
    assert_eq!(code, Some(0));
    assert!(
        stdout.contains("No overlapping CLOCK lines."),
        "stdout={}",
        stdout
    );
}

#[test]
fn nul_and_json_are_mutually_exclusive() {
    let f = fixtures_root().join("overlap-clean.org");
    let (code, _, stderr) = run(&[f.to_str().unwrap(), "-0", "-j"]);
    assert_eq!(code, Some(2), "clap usage error expected: {}", stderr);
}

// ----- behaviour --------------------------------------------------------

#[test]
fn detects_overlap_across_two_files() {
    let td = TempDir::new("cross-file");
    write(
        td.path().join("a.org").as_path(),
        "* a\nCLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00\n",
    );
    write(
        td.path().join("b.org").as_path(),
        "* b\nCLOCK: [2026-01-07 Wed 11:00 +0100]--[2026-01-07 Wed 13:00 +0100] =>  2:00\n",
    );
    let (code, stdout, _) = run(&[td.path().to_str().unwrap(), "-j"]);
    assert_eq!(code, Some(1));
    let v: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert!(v["a"]["file"].as_str().unwrap().ends_with("a.org"));
    assert!(v["b"]["file"].as_str().unwrap().ends_with("b.org"));
}

#[test]
fn exclude_glob_suppresses_the_pair() {
    let td = TempDir::new("exclude");
    write(
        td.path().join("keep/a.org").as_path(),
        "* a\nCLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00\n",
    );
    write(
        td.path().join("drop/b.org").as_path(),
        "* b\nCLOCK: [2026-01-07 Wed 11:00 +0100]--[2026-01-07 Wed 13:00 +0100] =>  2:00\n",
    );

    let (code, _, _) = run(&[td.path().to_str().unwrap(), "-j"]);
    assert_eq!(code, Some(1), "sanity: the two files do overlap");

    let glob = format!("{}/drop/**", td.path().display());
    let (code, stdout, _) = run(&[td.path().to_str().unwrap(), "-j", "--exclude", &glob]);
    assert_eq!(
        code,
        Some(0),
        "excluded file must not contribute: {}",
        stdout
    );
}

#[test]
fn first_flag_emits_a_single_record() {
    let td = TempDir::new("first");
    // Three mutually overlapping clocks → three pairs.
    write(
        td.path().join("a.org").as_path(),
        "* h\n\
         CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 13:00 +0100] =>  3:00\n\
         CLOCK: [2026-01-07 Wed 11:00 +0100]--[2026-01-07 Wed 14:00 +0100] =>  3:00\n\
         CLOCK: [2026-01-07 Wed 12:00 +0100]--[2026-01-07 Wed 15:00 +0100] =>  3:00\n",
    );

    let (_, stdout, _) = run(&[td.path().to_str().unwrap(), "-j"]);
    assert_eq!(
        stdout.lines().count(),
        3,
        "expected 3 pairs without --first"
    );

    let (code, stdout, _) = run(&[td.path().to_str().unwrap(), "-j", "-1"]);
    assert_eq!(code, Some(1));
    assert_eq!(stdout.lines().count(), 1, "--first must emit one record");
}

#[test]
fn no_sort_preserves_detector_order() {
    let td = TempDir::new("sort-default");
    let file = write_sort_fixture(&td);
    let (code, stdout, stderr) = run(&[file.to_str().unwrap(), "-j"]);

    assert_eq!(code, Some(1), "stderr={stderr}");
    assert_eq!(
        pair_ids(&json_records(&stdout)),
        vec![(2, 3), (2, 4), (3, 4), (2, 5), (3, 5), (4, 5)]
    );
}

#[test]
fn sort_fields_support_implicit_and_explicit_directions() {
    let td = TempDir::new("sort-fields");
    let file = write_sort_fixture(&td);
    let path = file.to_str().unwrap();

    for field in ["start", "end", "duration"] {
        let implicit = run(&[path, "-j", "--sort", field]);
        let explicit_asc = run(&[path, "-j", "--sort", &format!("{field}:asc")]);
        assert_eq!(implicit.0, Some(1), "field={field}: {}", implicit.2);
        assert_eq!(implicit.1, explicit_asc.1, "field={field}");
        let records = json_records(&implicit.1);
        assert!(
            records
                .windows(2)
                .all(|pair| record_field_cmp(&pair[0], &pair[1], field) != Ordering::Greater),
            "{field}:asc output={}",
            implicit.1
        );

        let descending = format!("{field}:desc");
        let desc = run(&[path, "-j", "--sort", &descending]);
        assert_eq!(desc.0, Some(1), "field={field}: {}", desc.2);
        let records = json_records(&desc.1);
        assert!(
            records
                .windows(2)
                .all(|pair| record_field_cmp(&pair[0], &pair[1], field) != Ordering::Less),
            "{field}:desc output={}",
            desc.1
        );
    }
}

#[test]
fn repeated_sort_keys_are_lexicographic_and_stable() {
    let td = TempDir::new("sort-multi");
    let file = write_sort_fixture(&td);
    let (_, stdout, _) = run(&[
        file.to_str().unwrap(),
        "-j",
        "--sort",
        "start",
        "--sort",
        "duration:desc",
    ]);

    assert_eq!(
        pair_ids(&json_records(&stdout)),
        vec![(2, 3), (2, 4), (3, 4), (2, 5), (4, 5), (3, 5)]
    );
}

#[test]
fn first_is_applied_after_sorting() {
    let td = TempDir::new("sort-first");
    let file = write_sort_fixture(&td);
    let (code, stdout, stderr) = run(&[
        file.to_str().unwrap(),
        "-j",
        "--sort",
        "duration",
        "--first",
    ]);

    assert_eq!(code, Some(1), "stderr={stderr}");
    assert_eq!(pair_ids(&json_records(&stdout)), vec![(3, 5)]);
}

#[test]
fn sorted_order_is_consistent_across_output_modes() {
    let td = TempDir::new("sort-formats");
    let file = write_sort_fixture(&td);
    let path = file.to_str().unwrap();
    let expected = vec![(3, 5), (3, 4), (2, 5), (4, 5), (2, 3), (2, 4)];

    let (_, json, _) = run(&[path, "-j", "--sort", "duration"]);
    assert_eq!(pair_ids(&json_records(&json)), expected);

    let (_, nul, _) = run(&[path, "-0", "--sort", "duration"]);
    let nul_pairs: Vec<(u64, u64)> = nul
        .split("\0\0")
        .filter(|record| !record.is_empty())
        .map(|record| {
            let fields: Vec<&str> = record.split('\0').collect();
            (fields[1].parse().unwrap(), fields[3].parse().unwrap())
        })
        .collect();
    assert_eq!(nul_pairs, expected);

    let (_, human, _) = run(&[path, "--sort", "duration"]);
    let location_prefix = format!("{}:", file.display());
    let locations: Vec<u64> = human
        .lines()
        .filter_map(|line| line.trim().strip_prefix(&location_prefix))
        .map(|line| line.parse().unwrap())
        .collect();
    let human_pairs: Vec<(u64, u64)> = locations
        .chunks_exact(2)
        .map(|pair| (pair[0], pair[1]))
        .collect();
    assert_eq!(human_pairs, expected);
}

#[test]
fn invalid_sort_field_and_direction_are_actionable_clap_errors() {
    let td = TempDir::new("sort-invalid");
    let file = write_sort_fixture(&td);
    let path = file.to_str().unwrap();

    let (code, _, stderr) = run(&[path, "--sort", "when"]);
    assert_eq!(code, Some(2), "stderr={stderr}");
    for expected in ["when", "start", "end", "duration"] {
        assert!(stderr.contains(expected), "missing {expected:?}: {stderr}");
    }

    let (code, _, stderr) = run(&[path, "--sort", "start:up"]);
    assert_eq!(code, Some(2), "stderr={stderr}");
    for expected in ["up", "asc", "desc"] {
        assert!(stderr.contains(expected), "missing {expected:?}: {stderr}");
    }
}

#[test]
fn non_canonical_lines_are_skipped_not_fatal() {
    let td = TempDir::new("skip");
    // The first line has no timezone and cannot be placed on a
    // timeline; the remaining two genuinely overlap.
    write(
        td.path().join("a.org").as_path(),
        "* h\n\
         CLOCK: [2026-01-07 Wed 09:57]--[2026-01-07 Wed 12:19] =>  2:22\n\
         CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00\n\
         CLOCK: [2026-01-07 Wed 11:00 +0100]--[2026-01-07 Wed 13:00 +0100] =>  2:00\n",
    );
    let (code, stdout, _) = run(&[td.path().to_str().unwrap(), "-j"]);
    assert_eq!(code, Some(1));
    assert_eq!(
        stdout.lines().count(),
        1,
        "the unparseable line must not produce a pair: {}",
        stdout
    );
}

#[test]
fn adjacent_clocks_are_not_reported() {
    let td = TempDir::new("adjacent");
    write(
        td.path().join("a.org").as_path(),
        "* h\n\
         CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 11:00 +0100] =>  1:00\n\
         CLOCK: [2026-01-07 Wed 11:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  1:00\n",
    );
    let (code, _, _) = run(&[td.path().to_str().unwrap(), "-j"]);
    assert_eq!(code, Some(0), "half-open intervals must allow adjacency");
}

#[test]
fn empty_corpus_is_clean_not_an_error() {
    let td = TempDir::new("empty");
    write(td.path().join("a.org").as_path(), "* just a heading\n");
    let (code, stdout, stderr) = run(&[td.path().to_str().unwrap(), "-j"]);
    assert_eq!(code, Some(0), "stderr={}", stderr);
    assert!(stdout.trim().is_empty());
}

#[test]
fn missing_path_is_a_hard_error() {
    let (code, _, stderr) = run(&["/no/such/place/nowhere.org", "-j"]);
    assert_eq!(code, Some(1), "stderr={}", stderr);
    assert!(
        stderr.contains("path not found"),
        "expected a clear diagnostic, got: {}",
        stderr
    );
}

// ----- cache ----------------------------------------------------------

// O3: cached result equals uncached result, and src-block samples are
// excluded on BOTH paths now that discovery goes through orgize.
#[test]
fn cache_matches_uncached() {
    let td = TempDir::new("o3");
    write(
        td.path().join("a.org").as_path(),
        indoc! {"
            * h
            CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00
            CLOCK: [2026-01-07 Wed 11:00 +0100]--[2026-01-07 Wed 13:00 +0100] =>  2:00
            #+begin_src org
            CLOCK: [2026-01-07 Wed 11:30 +0100]--[2026-01-07 Wed 11:45 +0100] =>  0:15
            #+end_src
        "},
    );
    let dir = td.path().to_str().unwrap();
    let cache = TempDir::new("o3-cache");
    let (cached_code, cached, _) = run_with_cache(&cache, &[dir, "-j"]);
    let (uncached_code, uncached, _) = run_with_cache(&cache, &[dir, "-j", "--no-cache"]);
    assert_eq!(uncached_code, cached_code);
    assert_eq!(uncached, cached);
    assert_eq!(cached_code, Some(1));
    assert_eq!(cached.lines().count(), 1, "cached: {}", cached);
    let v: serde_json::Value = serde_json::from_str(cached.lines().next().unwrap()).unwrap();
    assert_eq!(v["overlap_seconds"], 3600);
    assert!(
        v["a"]["raw"].as_str().unwrap().contains("10:00"),
        "raw line must be fetched for rendering: {}",
        v
    );
}

#[test]
fn structural_clock_discovery_is_identical_in_every_cache_mode() {
    let td = TempDir::new("structural");
    write(
        td.path().join("external.org").as_path(),
        indoc! {"
            * External work
            CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00
        "},
    );
    write(
        td.path().join("samples-and-work.org").as_path(),
        indoc! {"
            * Literal samples
            #+begin_example
            CLOCK: [2026-01-07 Wed 10:15 +0100]--[2026-01-07 Wed 10:30 +0100] =>  0:15
            #+end_example
            #+begin_quote
              #+BeGiN_SrC org
              CLOCK: [2026-01-07 Wed 10:30 +0100]--[2026-01-07 Wed 10:45 +0100] =>  0:15
              #+eNd_SrC
              #+begin_example
              CLOCK: [2026-01-07 Wed 10:45 +0100]--[2026-01-07 Wed 11:00 +0100] =>  0:15
              #+end_example
            #+end_quote
            * Broken block sample
            #+begin_quote
            #+BEGIN_SRC org
            #+end_quote
            * Real work after heading dissolves the broken block
            :LOGBOOK:
            CLOCK: [2026-01-07 Wed 11:00 +0100]--[2026-01-07 Wed 13:00 +0100] =>  2:00
            :END:
        "},
    );

    let dir = td.path().to_str().unwrap();
    let cache = TempDir::new("structural-cache");
    let cold = run_with_cache(&cache, &[dir, "-j"]);
    let warm = run_with_cache(&cache, &[dir, "-j"]);
    let verified = run_with_cache(&cache, &[dir, "-j", "--verify"]);

    let no_cache_storage = TempDir::new("structural-no-cache");
    let cache_file = no_cache_storage.path().join("clock.sqlite");
    let uncached = run_with_cache(&no_cache_storage, &[dir, "-j", "--no-cache"]);

    for (label, actual) in [
        ("warm", &warm),
        ("verified", &verified),
        ("uncached", &uncached),
    ] {
        assert_eq!(actual.0, cold.0, "{label} exit status: stderr={}", actual.2);
        assert_eq!(actual.1, cold.1, "{label} stdout");
    }
    assert_eq!(cold.0, Some(1), "stderr={}", cold.2);
    assert_eq!(cold.1.lines().count(), 1, "stdout={}", cold.1);
    assert!(
        !cache_file.exists(),
        "--no-cache must not create or open {}",
        cache_file.display()
    );

    let record: serde_json::Value = serde_json::from_str(cold.1.trim()).unwrap();
    assert_eq!(record["a"]["line"], 2);
    assert_eq!(record["b"]["line"], 19);
    assert_eq!(record["overlap_seconds"], 3600);
    assert!(
        record["a"]["file"]
            .as_str()
            .unwrap()
            .ends_with("external.org")
    );
    assert!(
        record["b"]["file"]
            .as_str()
            .unwrap()
            .ends_with("samples-and-work.org")
    );
}

#[cfg(unix)]
#[test]
fn canonical_alias_and_cross_file_tie_order_are_cache_invariant() {
    use std::os::unix::fs::symlink;

    let td = TempDir::new("canonical-alias");
    let real = td.path().join("a.org");
    let alias = td.path().join("alias.org");
    let other = td.path().join("z.org");
    let clock = indoc! {"
        * Work
        CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 11:00 +0100] =>  1:00
    "};
    write(&real, clock);
    write(&other, clock);
    symlink(&real, &alias).unwrap();

    let cache = TempDir::new("canonical-alias-cache");
    let args = [alias.to_str().unwrap(), other.to_str().unwrap(), "-j"];
    let cold = run_with_cache(&cache, &args);
    let warm = run_with_cache(&cache, &args);
    let mut uncached_args = args.to_vec();
    uncached_args.push("--no-cache");
    let uncached = run_with_cache(&cache, &uncached_args);

    assert_eq!(warm.0, cold.0, "warm stderr={}", warm.2);
    assert_eq!(warm.1, cold.1);
    assert_eq!(uncached.0, cold.0, "uncached stderr={}", uncached.2);
    assert_eq!(uncached.1, cold.1);
    assert_eq!(cold.0, Some(1), "stderr={}", cold.2);
    assert_eq!(cold.1.lines().count(), 1, "stdout={}", cold.1);

    let record: serde_json::Value = serde_json::from_str(cold.1.trim()).unwrap();
    let canonical_real = real.canonicalize().unwrap();
    let files = [
        PathBuf::from(record["a"]["file"].as_str().unwrap()),
        PathBuf::from(record["b"]["file"].as_str().unwrap()),
    ];
    assert!(files.contains(&canonical_real), "record={record}");
    assert!(!files.contains(&alias), "record={record}");
}

// Second run against the same cache file must be a root hit.
#[test]
fn warm_run_hits_root() {
    let td = TempDir::new("warm");
    let cache = TempDir::new("warm-cache");
    write(
        td.path().join("a.org").as_path(),
        "* h\nCLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00\n",
    );
    let dir = td.path().to_str().unwrap();
    let (_, _, cold) = run_with_cache(&cache, &[dir, "-j", "--timing"]);
    let (_, _, warm) = run_with_cache(&cache, &[dir, "-j", "--timing"]);
    assert!(cold.contains("root-hit       false"), "cold: {}", cold);
    assert!(warm.contains("root-hit        true"), "warm: {}", warm);
    assert!(warm.contains("l1-misses          0"), "warm: {}", warm);
}

// T1
#[test]
fn timing_has_all_phases() {
    let td = TempDir::new("t1");
    write(td.path().join("a.org").as_path(), "* h\n");
    let (_, _, stderr) = run(&[td.path().to_str().unwrap(), "--timing"]);
    for phase in [
        "validate", "l1-miss", "l2", "sweep", "render", "total", "facts", "pairs",
    ] {
        assert!(
            stderr.contains(phase),
            "missing phase {}: {}",
            phase,
            stderr
        );
    }
}

// --verify catches an edit that preserves size and mtime.
#[test]
fn verify_catches_metadata_preserving_edit() {
    let td = TempDir::new("verify");
    let cache = TempDir::new("verify-cache");
    let f = td.path().join("a.org");
    let original = "* h\nCLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00\n\
         CLOCK: [2026-01-07 Wed 14:00 +0100]--[2026-01-07 Wed 15:00 +0100] =>  1:00\n";
    write(&f, original);
    let dir = td.path().to_str().unwrap();
    let (c, _, _) = run_with_cache(&cache, &[dir, "-j"]);
    assert_eq!(c, Some(0), "sanity: no overlap in the original");

    // Same byte length, now overlapping; restore the mtime.
    let mtime = fs::metadata(&f).unwrap().modified().unwrap();
    let edited = original.replace(
        "14:00 +0100]--[2026-01-07 Wed 15:00",
        "11:00 +0100]--[2026-01-07 Wed 12:00",
    );
    assert_eq!(edited.len(), original.len());
    write(&f, &edited);
    fs::File::open(&f).unwrap().set_modified(mtime).unwrap();

    let (fooled, _, _) = run_with_cache(&cache, &[dir, "-j"]);
    let (caught, out, _) = run_with_cache(&cache, &[dir, "-j", "--verify"]);
    assert_eq!(fooled, Some(0), "metadata mode is expected to be fooled");
    assert_eq!(caught, Some(1), "--verify must see the edit: {}", out);
}
