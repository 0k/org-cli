//! End-to-end specification tests for `org-cli link move`.
//!
//! Every fixture is created under a temporary root. The tests exercise the
//! real binary and compare complete file contents so path rewrites cannot
//! accidentally alter unrelated org bytes.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use indoc::indoc;
use similar::TextDiff;

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_org-cli"))
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn run_move(old: &Path, new: &Path, root: &Path, write: bool) -> Output {
    let mut command = Command::new(bin_path());
    command
        .arg("link")
        .arg("move")
        .arg(old)
        .arg(new)
        .arg("--root")
        .arg(root);
    if write {
        command.arg("--write");
    }
    command.output().expect("spawn org-cli link move")
}

fn assert_ok(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected success; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_output_eq(actual: &str, expected: &str) {
    if actual != expected {
        let diff = TextDiff::from_lines(expected, actual);
        eprintln!("{}", diff.unified_diff().header("expected", "actual"));
        panic!("output mismatch");
    }
}

#[test]
fn dry_run_prints_deterministic_summary_and_diff_without_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("old.org");
    let new = root.join("new.org");
    let source = root.join("source.org");
    let source_original = "See [[file:old.org][old]].\n";
    write(&old, "* Old\n");
    write(&source, source_original);

    let output = run_move(&old, &new, root, false);
    assert_ok(&output);

    assert!(old.exists());
    assert!(!new.exists());
    assert_output_eq(&fs::read_to_string(&source).unwrap(), source_original);
    let expected = format!(
        "Move: {} -> {}\nRewrites: 1 link(s) in 1 org file(s)\n--- a/{}\n+++ b/{}\n@@ -1 +1 @@\n-See [[file:old.org][old]].\n+See [[file:new.org][old]].\n(dry run; pass --write to apply)\n",
        old.display(),
        new.display(),
        source.display(),
        source.display(),
    );
    assert_output_eq(&String::from_utf8(output.stdout).unwrap(), &expected);
}

#[test]
fn file_move_rewrites_inbound_path_only_and_preserves_anchor_description() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("topic/old file.org");
    let new = root.join("topic/new file.org");
    let source = root.join("index.org");
    write(&old, "* Destination\n");
    let original = indoc! {"
        #+TITLE: Unchanged
        Before [[file:topic/old file.org::*Destination][A useful description]] after.
        Raw old file.org text is not a link.
    "};
    write(&source, original);

    let output = run_move(&old, &new, root, true);
    assert_ok(&output);

    assert!(!old.exists());
    assert!(new.exists());
    assert_output_eq(&fs::read_to_string(&new).unwrap(), "* Destination\n");
    assert_output_eq(
        &fs::read_to_string(&source).unwrap(),
        indoc! {"
            #+TITLE: Unchanged
            Before [[file:topic/new file.org::*Destination][A useful description]] after.
            Raw old file.org text is not a link.
        "},
    );
}

#[test]
fn directory_move_rewrites_inbound_outbound_and_both_moving_edges() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("old");
    let destination_parent = root.join("relocated/deeper");
    let new = destination_parent.join("new");
    fs::create_dir_all(&destination_parent).unwrap();
    write(&root.join("outside.org"), "* Outside\n");
    write(&old.join("b.org"), "* B\n");
    write(
        &old.join("a.org"),
        indoc! {"
            [[file:../outside.org][outbound]]
            [[file:b.org][both move]]
        "},
    );
    write(&root.join("index.org"), "[[file:old/b.org][inbound]]\n");

    let output = run_move(&old, &new, root, true);
    assert_ok(&output);

    assert!(!old.exists());
    assert_output_eq(
        &fs::read_to_string(new.join("a.org")).unwrap(),
        indoc! {"
            [[file:../../../outside.org][outbound]]
            [[file:b.org][both move]]
        "},
    );
    assert_output_eq(
        &fs::read_to_string(root.join("index.org")).unwrap(),
        "[[file:relocated/deeper/new/b.org][inbound]]\n",
    );
}

#[test]
fn move_rewrites_all_followable_origins_and_preserves_other_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("old.org");
    let new = root.join("new.org");
    let source = root.join("source.org");
    write(&old, "* Old\n");
    write(
        &source,
        indoc! {"
            * H
            :PROPERTIES:
            :POINTER: [[file:old.org][property]]
            :END:
            Body [[file:old.org][body]].
            :LOGBOOK:
            - note [[file:old.org][drawer]]
            :END:
        "},
    );

    assert_ok(&run_move(&old, &new, root, true));
    assert_output_eq(
        &fs::read_to_string(&source).unwrap(),
        indoc! {"
            * H
            :PROPERTIES:
            :POINTER: [[file:new.org][property]]
            :END:
            Body [[file:new.org][body]].
            :LOGBOOK:
            - note [[file:new.org][drawer]]
            :END:
        "},
    );
}

#[test]
fn absolute_links_stay_absolute_and_directory_links_keep_trailing_slash() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("old-dir");
    let new = root.join("new-dir");
    fs::create_dir(&old).unwrap();
    let source = root.join("source.org");
    write(
        &source,
        &format!("[[file:{}/][absolute directory]]\n", old.display()),
    );

    assert_ok(&run_move(&old, &new, root, true));
    assert_output_eq(
        &fs::read_to_string(&source).unwrap(),
        &format!("[[file:{}/][absolute directory]]\n", new.display()),
    );
}

#[test]
fn generated_ambiguous_targets_round_trip_to_the_moved_file() {
    for (destination_name, encoded_target) in [
        ("%2E%2E", "%252E%252E"),
        ("name::anchor", "name%3A%3Aanchor"),
        ("~", "%7E"),
        ("name [with space]", "name %5Bwith space%5D"),
        ("control\nname", "control%0Aname"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let old = root.join("old.org");
        let new = root.join(destination_name);
        let source = root.join("source.org");
        write(&old, "* H\n");
        write(&source, "[[file:old.org::*H][kept description]]\n");

        let output = run_move(&old, &new, root, true);
        assert_ok(&output);
        assert!(new.exists(), "destination {destination_name:?}");
        assert_output_eq(
            &fs::read_to_string(&source).unwrap(),
            &format!("[[file:{}::*H][kept description]]\n", encoded_target),
        );

        let check = Command::new(bin_path())
            .args(["link", "check"])
            .arg(root)
            .output()
            .unwrap();
        assert_eq!(
            check.status.code(),
            Some(0),
            "generated target {destination_name:?} did not round-trip: {}",
            String::from_utf8_lossy(&check.stdout)
        );
    }
}

#[test]
fn authored_symlink_with_dotdot_resolves_before_root_containment_check() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let outside = temp.path().join("outside");
    fs::create_dir_all(outside.join("nested")).unwrap();
    fs::create_dir(&root).unwrap();
    write(&root.join("victim"), "inside\n");
    write(&outside.join("victim"), "outside\n");
    symlink(outside.join("nested"), root.join("portal")).unwrap();

    let authored_old = root.join("portal/../victim");
    let new = root.join("moved");
    let output = run_move(&authored_old, &new, &root, true);

    assert_ne!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stderr).contains("outside ROOT"));
    assert_output_eq(
        &fs::read_to_string(root.join("victim")).unwrap(),
        "inside\n",
    );
    assert_output_eq(
        &fs::read_to_string(outside.join("victim")).unwrap(),
        "outside\n",
    );
    assert!(!new.exists());
}

#[test]
fn inbound_link_to_symlink_inside_moved_directory_tracks_authored_location() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let outside = temp.path().join("outside.org");
    let old = root.join("old");
    let new = root.join("new");
    fs::create_dir(&old).unwrap();
    write(&outside, "* Outside\n");
    symlink(&outside, old.join("alias")).unwrap();
    write(&root.join("source.org"), "[[file:old/alias][outside]]\n");

    assert_ok(&run_move(&old, &new, root, true));
    assert_output_eq(
        &fs::read_to_string(root.join("source.org")).unwrap(),
        "[[file:new/alias][outside]]\n",
    );
    assert!(new.join("alias").exists());
    assert!(!old.exists());
}

#[test]
fn affected_relative_terminal_symlink_is_refused_before_relocation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("old");
    let destination_parent = root.join("relocated");
    let new = destination_parent.join("new");
    fs::create_dir(&old).unwrap();
    fs::create_dir(&destination_parent).unwrap();
    write(&root.join("outside.org"), "* Original target\n");
    write(
        &destination_parent.join("outside.org"),
        "* Different target\n",
    );
    symlink("../outside.org", old.join("alias")).unwrap();
    let source = root.join("source.org");
    let source_original = "[[file:old/alias][must not retarget]]\n";
    write(&source, source_original);

    for write_mode in [false, true] {
        let output = run_move(&old, &new, root, write_mode);
        assert_ne!(output.status.code(), Some(0));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("relative terminal symlink under OLD"),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(old.exists());
        assert!(!new.exists());
        assert_eq!(
            fs::read_link(old.join("alias")).unwrap(),
            PathBuf::from("../outside.org")
        );
        assert_output_eq(&fs::read_to_string(&source).unwrap(), source_original);
        assert_output_eq(
            &fs::read_to_string(root.join("outside.org")).unwrap(),
            "* Original target\n",
        );
        assert_output_eq(
            &fs::read_to_string(destination_parent.join("outside.org")).unwrap(),
            "* Different target\n",
        );
    }
}

#[test]
fn affected_relative_intermediate_symlink_is_refused_before_relocation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("old");
    let destination_parent = root.join("relocated");
    let new = destination_parent.join("new");
    fs::create_dir(&old).unwrap();
    fs::create_dir_all(root.join("outside-dir")).unwrap();
    fs::create_dir_all(destination_parent.join("outside-dir")).unwrap();
    write(&root.join("outside-dir/target.org"), "* Original target\n");
    write(
        &destination_parent.join("outside-dir/target.org"),
        "* Different target\n",
    );
    symlink("../outside-dir", old.join("alias")).unwrap();
    let source = root.join("source.org");
    let source_original = "[[file:old/alias/target.org][must not retarget]]\n";
    write(&source, source_original);

    for write_mode in [false, true] {
        let output = run_move(&old, &new, root, write_mode);
        assert_ne!(output.status.code(), Some(0));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("relative symlink component under OLD"),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(old.exists());
        assert!(!new.exists());
        assert_eq!(
            fs::read_link(old.join("alias")).unwrap(),
            PathBuf::from("../outside-dir")
        );
        assert_output_eq(&fs::read_to_string(&source).unwrap(), source_original);
        assert_output_eq(
            &fs::read_to_string(root.join("outside-dir/target.org")).unwrap(),
            "* Original target\n",
        );
        assert_output_eq(
            &fs::read_to_string(destination_parent.join("outside-dir/target.org")).unwrap(),
            "* Different target\n",
        );
    }
}

#[test]
fn affected_absolute_intermediate_symlink_remains_supported() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("old");
    let new = root.join("new");
    let outside = temp.path().join("outside-dir");
    fs::create_dir(&old).unwrap();
    fs::create_dir(&outside).unwrap();
    write(&outside.join("target.org"), "* Absolute target\n");
    symlink(&outside, old.join("alias")).unwrap();
    let source = root.join("source.org");
    write(&source, "[[file:old/alias/target.org][absolute]]\n");

    assert_ok(&run_move(&old, &new, root, true));
    assert_output_eq(
        &fs::read_to_string(source).unwrap(),
        "[[file:new/alias/target.org][absolute]]\n",
    );
    assert_output_eq(
        &fs::read_to_string(outside.join("target.org")).unwrap(),
        "* Absolute target\n",
    );
    assert!(new.join("alias").is_symlink());
    assert!(!old.exists());
}

#[test]
fn affected_link_with_symlink_then_parent_is_refused_without_retargeting() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let outside = temp.path().join("outside");
    let old = root.join("old");
    let new = root.join("new");
    fs::create_dir_all(outside.join("nested")).unwrap();
    fs::create_dir_all(&old).unwrap();
    write(&outside.join("target.org"), "* Outside target\n");
    write(&old.join("target.org"), "* Local target\n");
    symlink(outside.join("nested"), old.join("portal")).unwrap();
    let source = root.join("source.org");
    let source_original = "[[file:old/portal/../target.org][must stay outside]]\n";
    write(&source, source_original);

    for write_mode in [false, true] {
        let output = run_move(&old, &new, &root, write_mode);
        assert_ne!(output.status.code(), Some(0));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("symlink component followed by '..'"),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(old.exists());
        assert!(!new.exists());
        assert_output_eq(&fs::read_to_string(&source).unwrap(), source_original);
        assert_output_eq(
            &fs::read_to_string(outside.join("target.org")).unwrap(),
            "* Outside target\n",
        );
        assert_output_eq(
            &fs::read_to_string(old.join("target.org")).unwrap(),
            "* Local target\n",
        );
    }
}

#[test]
fn destination_collision_and_invalid_boundaries_are_refused_without_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("old");
    let collision = root.join("collision");
    fs::create_dir(&old).unwrap();
    fs::create_dir(&collision).unwrap();

    let collision_output = run_move(&old, &collision, root, true);
    assert_ne!(collision_output.status.code(), Some(0));
    assert!(old.exists());
    assert!(collision.exists());

    let outside = root.parent().unwrap().join("outside-org-cli-move");
    let outside_output = run_move(&old, &outside, root, true);
    assert_ne!(outside_output.status.code(), Some(0));
    assert!(old.exists());
    assert!(!outside.exists());

    let outside_old = root.parent().unwrap().join("outside-old-org-cli-move");
    fs::create_dir(&outside_old).unwrap();
    let outside_old_output = run_move(&outside_old, &root.join("inside"), root, true);
    assert_ne!(outside_old_output.status.code(), Some(0));
    assert!(outside_old.exists());
    fs::remove_dir(&outside_old).unwrap();

    let inside = old.join("child");
    let inside_output = run_move(&old, &inside, root, true);
    assert_ne!(inside_output.status.code(), Some(0));
    assert!(old.exists());
    assert!(!inside.exists());
}

#[test]
fn destination_parent_must_already_exist() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("old.org");
    let new = root.join("missing/new.org");
    write(&old, "* Old\n");

    let output = run_move(&old, &new, root, true);
    assert_ne!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stderr).contains("destination parent"));
    assert!(old.exists());
    assert!(!new.exists());
}

#[test]
fn missing_old_and_unreadable_corpus_source_are_rejected_before_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("old.org");
    let new = root.join("new.org");

    let missing = run_move(&old, &new, root, true);
    assert_ne!(missing.status.code(), Some(0));
    assert!(!new.exists());

    write(&old, "* Old\n");
    fs::write(root.join("unreadable.org"), [0xff, 0xfe]).unwrap();
    let unreadable = run_move(&old, &new, root, true);
    assert_ne!(unreadable.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&unreadable.stderr).contains("cannot read"));
    assert!(old.exists());
    assert!(!new.exists());
}

#[test]
fn initially_broken_corpus_is_rejected_before_preview_or_write_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("old.org");
    let new = root.join("new.org");
    let source = root.join("source.org");
    let old_original = "* Old\n";
    let source_original = "[[file:old.org][affected]]\n";
    let unrelated_original = "[[file:missing.org][already broken]]\n";
    write(&old, old_original);
    write(&source, source_original);
    write(&root.join("unrelated.org"), unrelated_original);

    let dry_run = run_move(&old, &new, root, false);
    assert_ne!(dry_run.status.code(), Some(0));
    assert!(
        dry_run.stdout.is_empty(),
        "no success preview on invalid corpus"
    );
    assert!(old.exists());
    assert!(!new.exists());
    assert_output_eq(&fs::read_to_string(&old).unwrap(), old_original);
    assert_output_eq(&fs::read_to_string(&source).unwrap(), source_original);

    let write_run = run_move(&old, &new, root, true);
    assert_ne!(write_run.status.code(), Some(0));
    assert!(old.exists());
    assert!(!new.exists());
    assert_output_eq(&fs::read_to_string(&old).unwrap(), old_original);
    assert_output_eq(&fs::read_to_string(&source).unwrap(), source_original);
    assert_output_eq(
        &fs::read_to_string(root.join("unrelated.org")).unwrap(),
        unrelated_original,
    );
}

#[test]
fn move_succeeds_with_preexisting_broken_internal_link_and_preserves_it() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("old.org");
    let new = root.join("new.org");
    let source = root.join("source.org");
    write(&old, "* Old\n");
    write(&source, "* Source\n[[file:old.org][target]] [[*Nope]]\n");

    assert_ok(&run_move(&old, &new, root, true));
    assert_output_eq(
        &fs::read_to_string(&source).unwrap(),
        "* Source\n[[file:new.org][target]] [[*Nope]]\n",
    );
    assert!(!old.exists());
    assert!(new.exists());
}

#[test]
fn successful_write_leaves_the_all_followable_link_check_clean() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("old.org");
    let new = root.join("new.org");
    write(&old, "* H\n");
    write(&root.join("source.org"), "[[file:old.org::*H][target]]\n");

    assert_ok(&run_move(&old, &new, root, true));
    let check = Command::new(bin_path())
        .arg("link")
        .arg("check")
        .arg(root)
        .output()
        .unwrap();
    assert_eq!(
        check.status.code(),
        Some(0),
        "link check failed: stdout={} stderr={}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
}

#[test]
fn help_documents_dry_run_root_and_write_contract() {
    let output = Command::new(bin_path())
        .args(["link", "move", "--help"])
        .output()
        .unwrap();
    assert_ok(&output);
    let help = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "OLD",
        "NEW",
        "--root",
        "--write",
        "dry-run",
        "quiescent",
        "Cooperating",
        "same canonical ROOT",
    ] {
        assert!(help.contains(expected), "missing {expected:?} in:\n{help}");
    }
}

/// Fixture from the bug report: `a/note.org` links out to `b/target.org`
/// and is linked from `index.org`, so moving it into `b/` must rewrite
/// both its own outbound link and the inbound one.
fn moved_org_with_outbound_link_fixture(root: &Path) -> (PathBuf, PathBuf) {
    write(
        &root.join("a/note.org"),
        "* A\n[[file:../b/target.org][t]]\n",
    );
    write(&root.join("b/target.org"), "* T\n");
    write(&root.join("index.org"), "* Idx\n[[file:a/note.org][n]]\n");
    (root.join("a/note.org"), root.join("b/note.org"))
}

#[test]
fn dry_run_names_moved_org_file_without_trailing_slash() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (old, new) = moved_org_with_outbound_link_fixture(root);

    let output = run_move(&old, &new, root, false);
    assert_ok(&output);

    let expected = format!(
        "Move: {old} -> {new}\nRewrites: 2 link(s) in 2 org file(s)\n--- a/{old}\n+++ b/{new}\n@@ -1,2 +1,2 @@\n * A\n-[[file:../b/target.org][t]]\n+[[file:target.org][t]]\n--- a/{index}\n+++ b/{index}\n@@ -1,2 +1,2 @@\n * Idx\n-[[file:a/note.org][n]]\n+[[file:b/note.org][n]]\n(dry run; pass --write to apply)\n",
        old = old.display(),
        new = new.display(),
        index = root.join("index.org").display(),
    );
    assert_output_eq(&String::from_utf8(output.stdout).unwrap(), &expected);
}

#[test]
fn org_file_moved_to_another_directory_rewrites_its_own_outbound_links() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (old, new) = moved_org_with_outbound_link_fixture(root);

    let output = run_move(&old, &new, root, true);
    assert_ok(&output);

    assert!(!old.exists());
    assert_output_eq(
        &fs::read_to_string(&new).unwrap(),
        "* A\n[[file:target.org][t]]\n",
    );
    assert_output_eq(
        &fs::read_to_string(root.join("index.org")).unwrap(),
        "* Idx\n[[file:b/note.org][n]]\n",
    );
}

#[test]
fn moved_org_file_that_is_link_source_and_target_keeps_every_edge() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("a/note.org");
    let new = root.join("b/deeper/note.org");
    fs::create_dir_all(root.join("b/deeper")).unwrap();
    write(
        &old,
        indoc! {"
            * A
            [[file:../b/target.org][out]]
            [[file:note.org::*A][self]]
        "},
    );
    write(
        &root.join("b/target.org"),
        "* T\n[[file:../a/note.org][back]]\n",
    );

    let output = run_move(&old, &new, root, true);
    assert_ok(&output);

    assert!(!old.exists());
    assert_output_eq(
        &fs::read_to_string(&new).unwrap(),
        indoc! {"
            * A
            [[file:../target.org][out]]
            [[file:note.org::*A][self]]
        "},
    );
    assert_output_eq(
        &fs::read_to_string(root.join("b/target.org")).unwrap(),
        "* T\n[[file:deeper/note.org][back]]\n",
    );
}

#[test]
fn absolute_link_to_moved_file_is_rewritten_without_trailing_slash() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let old = root.join("a/note.org");
    let new = root.join("b/note.org");
    fs::create_dir_all(root.join("b")).unwrap();
    write(&old, "* T\n");
    let source = root.join("index.org");
    write(&source, &format!("[[file:{}][abs]]\n", old.display()));

    assert_ok(&run_move(&old, &new, root, true));
    assert_output_eq(
        &fs::read_to_string(&source).unwrap(),
        &format!("[[file:{}][abs]]\n", new.display()),
    );
}
