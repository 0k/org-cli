//! Real-binary specifications for strict YAML batch `link move` plans.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

fn run_plan(plan: &Path, root: &Path, write_mode: bool, cwd: Option<&Path>) -> Output {
    let mut command = Command::new(bin_path());
    command
        .args(["link", "move", "--plan"])
        .arg(plan)
        .arg("--root")
        .arg(root);
    if write_mode {
        command.arg("--write");
    }
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.output().unwrap()
}

fn assert_output_eq(actual: &str, expected: &str) {
    if actual != expected {
        eprintln!(
            "{}",
            TextDiff::from_lines(expected, actual)
                .unified_diff()
                .header("expected", "actual")
        );
        panic!("output mismatch");
    }
}

fn batch_fixture(root: &Path) -> PathBuf {
    write(&root.join("outside.org"), "* Outside\n");
    write(
        &root.join("a-old/a.org"),
        "[[file:../b-old/b.org][cross-b]]\n[[file:../outside.org][out]]\n",
    );
    write(
        &root.join("b-old/b.org"),
        "[[file:../a-old/a.org][cross-a]]\n",
    );
    write(
        &root.join("index.org"),
        "[[file:a-old/a.org][a]] [[file:b-old/b.org][b]]\n",
    );
    let plan = root.join("renames.yaml");
    write(
        &plan,
        "moves:\n  - from: a-old\n    to: a new\n  - from: b-old\n    to: b new\n",
    );
    plan
}

#[test]
fn batch_dry_run_is_simultaneous_deterministic_and_non_mutating() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let plan = batch_fixture(root);

    let output = run_plan(&plan, root, false, None);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.join("a-old").exists());
    assert!(root.join("b-old").exists());
    assert!(!root.join("a new").exists());
    assert!(!root.join("b new").exists());

    let expected = format!(
        "Moves:\n  1: {0}/a-old -> {0}/a new\n  2: {0}/b-old -> {0}/b new\nRewrites: 4 link(s) in 3 org file(s)\n--- a/{0}/a-old/a.org\n+++ b/{0}/a new/a.org\n@@ -1,2 +1,2 @@\n-[[file:../b-old/b.org][cross-b]]\n+[[file:../b new/b.org][cross-b]]\n [[file:../outside.org][out]]\n--- a/{0}/b-old/b.org\n+++ b/{0}/b new/b.org\n@@ -1 +1 @@\n-[[file:../a-old/a.org][cross-a]]\n+[[file:../a new/a.org][cross-a]]\n--- a/{0}/index.org\n+++ b/{0}/index.org\n@@ -1 +1 @@\n-[[file:a-old/a.org][a]] [[file:b-old/b.org][b]]\n+[[file:a new/a.org][a]] [[file:b new/b.org][b]]\n(dry run; pass --write to apply)\n",
        root.display()
    );
    assert_output_eq(&String::from_utf8(output.stdout).unwrap(), &expected);
}

#[test]
fn batch_write_preserves_cross_move_graph() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let plan = batch_fixture(root);
    let output = run_plan(&plan, root, true, None);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.join("a-old").exists());
    assert!(!root.join("b-old").exists());
    assert!(root.join("a new").exists());
    assert!(root.join("b new").exists());
    assert!(
        fs::read_to_string(root.join("index.org"))
            .unwrap()
            .contains("[[file:a new/a.org][a]]")
    );
    let check = Command::new(bin_path())
        .args(["link", "check"])
        .arg(root)
        .output()
        .unwrap();
    assert_eq!(
        check.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&check.stdout)
    );
}

#[test]
fn batch_paths_resolve_from_root_not_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let elsewhere = temp.path().join("elsewhere");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&elsewhere).unwrap();
    write(&root.join("old.org"), "* H\n");
    let plan = temp.path().join("renames.yaml");
    write(&plan, "moves:\n  - from: old.org\n    to: new.org\n");
    let output = run_plan(&plan, &root, false, Some(&elsewhere));
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains(&root.join("old.org").display().to_string())
    );
}

#[test]
fn batch_rejects_invalid_modes_and_yaml() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(&root.join("old"), "x\n");
    for (name, yaml) in [
        ("malformed", "moves: ["),
        ("unknown-top", "moves: []\nextra: true\n"),
        (
            "unknown-move",
            "moves:\n  - from: old\n    to: new\n    extra: true\n",
        ),
        ("empty", "moves: []\n"),
        ("empty-path", "moves:\n  - from: ''\n    to: new\n"),
        (
            "duplicate",
            "moves:\n  - from: old\n    to: new\n  - from: old\n    to: other\n",
        ),
    ] {
        let plan = root.join(format!("{name}.yaml"));
        write(&plan, yaml);
        let output = run_plan(&plan, root, false, None);
        assert_ne!(output.status.code(), Some(0), "case {name}");
    }
    for args in [
        vec!["link", "move", "--root", root.to_str().unwrap()],
        vec!["link", "move", "old", "--root", root.to_str().unwrap()],
        vec![
            "link",
            "move",
            "old",
            "new",
            "--plan",
            "x.yaml",
            "--root",
            root.to_str().unwrap(),
        ],
    ] {
        assert_ne!(
            Command::new(bin_path())
                .args(args)
                .output()
                .unwrap()
                .status
                .code(),
            Some(0)
        );
    }
    let help = Command::new(bin_path())
        .args(["link", "move", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--plan <YAML>"));
    assert!(help.contains("exactly one mode"));
}

#[test]
fn batch_rejects_overlaps_collisions_and_chains() {
    for (name, setup, yaml, expected_error) in [
        (
            "source-overlap",
            vec!["a", "a/child"],
            "moves:\n  - from: a\n    to: x\n  - from: a/child\n    to: y\n",
            "overlapping FROM paths",
        ),
        (
            "destination-overlap",
            vec!["a", "b", "x"],
            "moves:\n  - from: a\n    to: x\n  - from: b\n    to: x/child\n",
            "overlapping TO paths",
        ),
        (
            "chain",
            vec!["a", "b"],
            "moves:\n  - from: a\n    to: b\n  - from: b\n    to: c\n",
            "FROM/TO cross-collision",
        ),
        (
            "collision",
            vec!["a", "occupied"],
            "moves:\n  - from: a\n    to: occupied\n",
            "TO already exists",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for path in setup {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        let plan = root.join(format!("{name}.yaml"));
        write(&plan, yaml);
        let output = run_plan(&plan, root, true, None);
        assert_ne!(output.status.code(), Some(0), "case {name}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected_error),
            "case {name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::create_dir(temp.path().join("outside")).unwrap();
    let plan = temp.path().join("outside.yaml");
    write(&plan, "moves:\n  - from: ../outside\n    to: inside\n");
    assert_ne!(run_plan(&plan, &root, false, None).status.code(), Some(0));
}

#[test]
fn batch_moves_org_file_whose_own_links_must_be_rewritten() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(
        &root.join("a/note.org"),
        "* A\n[[file:../b/target.org][t]]\n",
    );
    write(&root.join("b/target.org"), "* T\n");
    write(&root.join("index.org"), "* Idx\n[[file:a/note.org][n]]\n");
    let plan = root.join("renames.yaml");
    write(&plan, "moves:\n  - from: a/note.org\n    to: b/note.org\n");

    let output = run_plan(&plan, root, true, None);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.join("a/note.org").exists());
    assert_output_eq(
        &fs::read_to_string(root.join("b/note.org")).unwrap(),
        "* A\n[[file:target.org][t]]\n",
    );
    assert_output_eq(
        &fs::read_to_string(root.join("index.org")).unwrap(),
        "* Idx\n[[file:b/note.org][n]]\n",
    );
}
