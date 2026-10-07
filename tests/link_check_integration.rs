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

use indoc::indoc;

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

#[test]
fn test_check_internal_links_report_all_formats_and_origins() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.org");
    std::fs::write(
        &source,
        indoc! {"
            * Existing
            :PROPERTIES:
            :CUSTOM_ID: existing-id
            :REFERENCE: [[#nope][missing id]]
            :END:
            [[*Missing][missing heading]]
            :LOGBOOK:
            - [[*Absent][drawer heading]]
            :END:
        "},
    )
    .unwrap();
    let invoke = |format: &str| {
        Command::new(bin_path())
            .args(["link", "check", format])
            .arg(&source)
            .output()
            .unwrap()
    };
    let json = invoke("-j");
    assert_eq!(json.status.code(), Some(1));
    let records: Vec<serde_json::Value> = String::from_utf8(json.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["source_file"], source.to_str().unwrap());
    assert_eq!(records[0]["resolved_target"], source.to_str().unwrap());
    assert_eq!(records[0]["reason_tag"], "custom-id-not-found");
    assert_eq!(records[0]["origin_tag"], "property");
    assert_eq!(records[0]["origin_property"], "REFERENCE");
    assert_eq!(records[1]["reason_tag"], "heading-not-found");
    assert_eq!(records[1]["origin_tag"], "body");
    assert_eq!(records[2]["origin_tag"], "drawer");
    assert_eq!(records[2]["origin_drawer"], "LOGBOOK");

    let nul = invoke("-0");
    assert_eq!(nul.status.code(), Some(1));
    let bytes = String::from_utf8(nul.stdout).unwrap();
    let rows: Vec<&str> = bytes.split("\0\0").filter(|row| !row.is_empty()).collect();
    assert_eq!(rows.len(), 3);
    let fields: Vec<&str> = rows[0].split('\0').collect();
    assert_eq!(
        fields,
        vec![
            source.to_str().unwrap(),
            "4",
            "13",
            "custom-id-not-found",
            source.to_str().unwrap(),
            "[[#nope][missing id]]",
            "property:PROPERTIES:REFERENCE"
        ]
    );

    let human = invoke("--no-color");
    assert_eq!(human.status.code(), Some(1));
    let output = String::from_utf8(human.stdout).unwrap();
    assert!(output.contains("CUSTOM_ID not found: \"nope\""));
    assert!(output.contains("[[*Missing][missing heading]]"));
    assert!(output.contains("3 broken link(s) found"));
}

#[test]
fn test_check_internal_links_resolve_markup_unicode_and_ignore_other_link_kinds() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.org");
    std::fs::write(
        &source,
        indoc! {"
        ** odoo_base / build de l'image Élabore en échec — erreur sur un paquet =apt=
        :PROPERTIES:
        :CUSTOM_ID: existing-id
        :END:
        [[*Élabore en échec — erreur sur un paquet =apt=][heading]]
        [[#existing-id]]
        [[Some text]] [[https://example.com]] [[id:unknown]]
        #+begin_src org
        [[*Not a link]]
        #+end_src
        ** Headline [[*Not extracted]]
    "},
    )
    .unwrap();
    let output = Command::new(bin_path())
        .args(["link", "check", "-j"])
        .arg(&source)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(output.stdout.is_empty());
    let strict = Command::new(bin_path())
        .args(["link", "check", "-j", "--strict-headings"])
        .arg(&source)
        .output()
        .unwrap();
    assert_eq!(strict.status.code(), Some(1));
    let record: serde_json::Value = serde_json::from_slice(&strict.stdout).unwrap();
    assert_eq!(record["reason_tag"], "heading-not-found");
}

#[test]
fn test_internal_links_do_not_change_orphans_or_backlinks() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target.org");
    let source = temp.path().join("source.org");
    std::fs::write(&target, "* Target\n").unwrap();
    std::fs::write(&source, "* Source\n[[file:target.org][target]]\n").unwrap();
    let run = |action: &str| {
        let mut cmd = Command::new(bin_path());
        cmd.args(["link", action, "-j"]);
        if action == "backlinks" {
            cmd.arg(&target);
        }
        cmd.arg(temp.path()).output().unwrap()
    };
    let before_orphans = run("orphans");
    let before_backlinks = run("backlinks");
    std::fs::write(
        &source,
        "* Source\n[[file:target.org][target]]\n[[*Source]] [[#nope]]\n",
    )
    .unwrap();
    let after_orphans = run("orphans");
    let after_backlinks = run("backlinks");
    assert_eq!(after_orphans.status, before_orphans.status);
    assert_eq!(after_orphans.stdout, before_orphans.stdout);
    assert_eq!(after_backlinks.status, before_backlinks.status);
    assert_eq!(after_backlinks.stdout, before_backlinks.stdout);
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

#[test]
fn test_check_first_stops_before_reading_later_corpus_sources() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("a-first.org"),
        "[[file:missing.org][first broken]]\n",
    )
    .unwrap();
    std::fs::write(temp.path().join("z-unreadable.org"), [0xff, 0xfe]).unwrap();

    let output = Command::new(bin_path())
        .args(["link", "check", "--first", "--json"])
        .arg(temp.path())
        .output()
        .expect("failed to spawn org-cli");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let records: Vec<&str> = stdout.lines().collect();
    assert_eq!(records.len(), 1);
    assert!(
        output.stderr.is_empty(),
        "later unreadable source must not be inspected: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A heading whose title contains `[` / `]` is linked by Emacs with
/// those brackets backslash-escaped (`org-store-link` emits
/// `::*\[2026-07-15\] Note`). Two properties must hold together:
/// the escaped `\]` must not truncate the link — otherwise `check`
/// silently drops it from the graph and reports a clean tree — and
/// the anchor must be compared unescaped against the real title.
#[test]
fn test_check_follows_heading_anchor_with_escaped_brackets() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("target.org"),
        "* [2026-07-15] Referral letter\n\nBody.\n\n* Plain\n\nBody.\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("source.org"),
        concat!(
            r"- ok   [[file:target.org::*\[2026-07-15\] Referral letter][resolves]]",
            "\n",
            r"- bad  [[file:target.org::*\[2026-07-99\] Absent heading][broken]]",
            "\n",
        ),
    )
    .unwrap();

    let output = Command::new(bin_path())
        .args(["link", "check", "--json"])
        .arg(temp.path())
        .output()
        .expect("failed to spawn org-cli");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let records: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // Exactly the absent heading is reported: the escaped link that
    // does resolve must not be flagged, and must not be skipped either.
    assert_eq!(
        records.len(),
        1,
        "expected only the unresolvable escaped anchor, got: {stdout}"
    );
    assert_eq!(records[0]["reason_tag"], "heading-not-found");
    assert_eq!(
        records[0]["reason_message"], "heading not found: \"[2026-07-99] Absent heading\"",
        "anchor must be reported unescaped"
    );
    assert_eq!(output.status.code(), Some(1));
}

/// A description containing `[` or `]` must not make the link vanish.
///
/// This is the shape the archive's own house style produces — a
/// description ending in a bracketed ISO date, `… ; versé
/// [2026-08-30]` — and the parser used to stop the description at any
/// bracket, so the whole link failed to parse and was absent from the
/// graph. `check` then reported a clean tree over broken links, which
/// is the one failure a link checker must never have.
///
/// Every row below is a link *Emacs follows*, verified against
/// `org-element-parse-buffer`. They are therefore positive cases: each
/// must be seen, and the ones whose target is missing must be
/// reported. `[[file:present.org][…]]` variants must stay silent.
#[test]
fn test_check_sees_links_whose_description_contains_brackets() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("present.org"), "* Here\n").unwrap();
    std::fs::write(
        temp.path().join("source.org"),
        concat!(
            // Reported: target absent, description carries brackets.
            "- [[file:gone-date.org][2010-04-14 — consultation ; versé [2026-08-30]]]\n",
            "- [[file:gone-pair.org][has [bracket] pair]]\n",
            "- [[file:gone-open.org][open [ only]]\n",
            "- [[file:gone-close.org][close ] only]]\n",
            // Silent: same shapes, but the target exists.
            "- [[file:present.org][ok ; versé [2026-08-30]]]\n",
            "- [[file:present.org][ok [bracket] pair]]\n",
        ),
    )
    .unwrap();

    let output = Command::new(bin_path())
        .args(["link", "check", "--json"])
        .arg(temp.path())
        .output()
        .expect("failed to spawn org-cli");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let targets: BTreeSet<String> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .map(|v| {
            Path::new(v["resolved_target"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
        })
        .collect();

    let expected: BTreeSet<String> = [
        "gone-date.org",
        "gone-pair.org",
        "gone-open.org",
        "gone-close.org",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    assert_eq!(
        targets, expected,
        "every bracketed-description link must be seen; \
         a missing target must be reported and a present one must not. got: {stdout}"
    );
    assert_eq!(output.status.code(), Some(1));
}

/// The bracketed-description shapes must stay excluded inside literal
/// contexts, where Emacs does not follow links either.
///
/// Paired deliberately with the test above: widening the description
/// scanner could plausibly have started claiming link-shaped text in
/// `#+begin_src` / `#+begin_example` / `=verbatim=`, and a checker that
/// reports a broken link inside a code sample is as wrong as one that
/// misses a real link.
#[test]
fn test_check_ignores_bracketed_descriptions_in_literal_contexts() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("source.org"),
        concat!(
            "#+begin_src org\n",
            "[[file:in-src.org][sample ; versé [2026-08-30]]]\n",
            "#+end_src\n\n",
            "#+begin_example\n",
            "[[file:in-example.org][sample [bracket] pair]]\n",
            "#+end_example\n\n",
            "Inline verbatim: =[[file:in-verbatim.org][quoted [x] here]]=\n",
        ),
    )
    .unwrap();

    let output = Command::new(bin_path())
        .args(["link", "check", "--json"])
        .arg(temp.path())
        .output()
        .expect("failed to spawn org-cli");
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(
        stdout.trim(),
        "",
        "link-shaped text in a literal context must yield no link, got: {stdout}"
    );
    assert_eq!(output.status.code(), Some(0));
}

/// `[[path][]]` is a link with no description, not a parse failure.
///
/// Emacs parses this form, so dropping it would delete a real edge.
/// Pinned because "the description is one-or-more characters" is the
/// intuitive reading and it is wrong.
#[test]
fn test_check_sees_link_with_empty_description() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("source.org"),
        "- [[file:gone-empty.org][]]\n",
    )
    .unwrap();

    let output = Command::new(bin_path())
        .args(["link", "check", "--json"])
        .arg(temp.path())
        .output()
        .expect("failed to spawn org-cli");
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        stdout.contains("gone-empty.org"),
        "an empty description must not make the link vanish, got: {stdout}"
    );
    assert_eq!(output.status.code(), Some(1));
}

/// A bracketed description must not let one link swallow the next.
///
/// The scanner stops at the FIRST `]]`, so two links on one line stay
/// two links. A greedy scan would merge them and silently drop the
/// second target from the graph.
#[test]
fn test_check_does_not_merge_adjacent_bracketed_links() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("source.org"),
        "See [[file:first-gone.org][a [x] b]] and [[file:second-gone.org][c [y] d]].\n",
    )
    .unwrap();

    let output = Command::new(bin_path())
        .args(["link", "check", "--json"])
        .arg(temp.path())
        .output()
        .expect("failed to spawn org-cli");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let targets: BTreeSet<String> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .map(|v| {
            Path::new(v["resolved_target"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
        })
        .collect();

    let expected: BTreeSet<String> = ["first-gone.org", "second-gone.org"]
        .into_iter()
        .map(String::from)
        .collect();

    assert_eq!(targets, expected, "both links must survive; got: {stdout}");
}

/// `link backlinks` shares the extraction path, so an escaped anchor
/// must make the source appear as a backlink rather than vanish.
#[test]
fn test_backlinks_sees_links_with_escaped_brackets() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("target.org"),
        "* [2026-07-15] Referral letter\n\nBody.\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("source.org"),
        concat!(
            r"[[file:target.org::*\[2026-07-15\] Referral letter][escaped]]",
            "\n",
        ),
    )
    .unwrap();

    let output = Command::new(bin_path())
        .args(["link", "backlinks"])
        .arg(temp.path().join("target.org"))
        .arg(temp.path())
        .output()
        .expect("failed to spawn org-cli");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("source.org"),
        "escaped link must be visible to the link graph, got: {stdout}"
    );
}

/// Corpus where a regular file `b/note.org` exists and is linked once
/// correctly, once with a trailing slash, once with a path continuing
/// past it, and once at a location that truly does not exist.
fn not_a_directory_fixture(root: &Path) {
    std::fs::create_dir(root.join("b")).unwrap();
    std::fs::write(root.join("b/note.org"), "* T\n").unwrap();
    std::fs::write(
        root.join("index.org"),
        indoc! {"
            [[file:b/note.org][ok]]
            [[file:b/note.org/][slash]]
            [[file:b/note.org/x.org][past]]
            [[file:a/note.org][missing]]
        "},
    )
    .unwrap();
}

/// Run `link check .` from `root` so reported paths are `./`-relative.
fn run_check_in(root: &Path, args: &[&str]) -> (Option<i32>, String) {
    let output = Command::new(bin_path())
        .args(["link", "check"])
        .args(args)
        .arg(".")
        .current_dir(root)
        .output()
        .expect("failed to spawn org-cli");
    (
        output.status.code(),
        String::from_utf8(output.stdout).expect("stdout not UTF-8"),
    )
}

#[test]
fn test_check_reports_path_through_regular_file_as_not_a_directory() {
    let temp = tempfile::tempdir().unwrap();
    not_a_directory_fixture(temp.path());

    let (code, stdout) = run_check_in(temp.path(), &[]);
    assert_eq!(code, Some(1));
    assert_eq!(
        stdout,
        indoc! {"
            ./index.org:2:1: broken -> ./b/note.org/ (not a directory: ./b/note.org is a regular file)
                [[file:b/note.org/][slash]]
            ./index.org:3:1: broken -> ./b/note.org/x.org (not a directory: ./b/note.org is a regular file)
                [[file:b/note.org/x.org][past]]
            ./index.org:4:1: broken -> ./a/note.org (target path does not exist)
                [[file:a/note.org][missing]]

            3 broken link(s) found
        "}
    );
}

#[test]
fn test_check_machine_formats_tag_not_a_directory() {
    let temp = tempfile::tempdir().unwrap();
    not_a_directory_fixture(temp.path());

    let (_, json) = run_check_in(temp.path(), &["-j"]);
    let tags: Vec<(String, String)> = json
        .lines()
        .map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            (
                v["reason_tag"].as_str().unwrap().to_string(),
                v["reason_message"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        tags,
        [
            (
                "not-a-directory".to_string(),
                "not a directory: ./b/note.org is a regular file".to_string()
            ),
            (
                "not-a-directory".to_string(),
                "not a directory: ./b/note.org is a regular file".to_string()
            ),
            (
                "path-missing".to_string(),
                "target path does not exist".to_string()
            ),
        ]
    );

    let (_, nul) = run_check_in(temp.path(), &["-0"]);
    let nul_tags: Vec<&str> = nul
        .split("\0\0")
        .filter(|r| !r.is_empty())
        .map(|r| r.split('\0').nth(3).unwrap())
        .collect();
    assert_eq!(
        nul_tags,
        ["not-a-directory", "not-a-directory", "path-missing"]
    );
}
