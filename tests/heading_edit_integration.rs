//! Integration tests for the `org-cli heading <set|add|delete|move>`
//! subcommands. Each test spawns the real binary against a temp file.
//!
//! Byte-level file assertions use full-content comparison; no private
//! corpus is involved (fixtures are constructed inline via `indoc`).

use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::PathBuf;
use std::process::Command;

use indoc::indoc;
use similar::TextDiff;

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_org-cli"))
}

/// Write `content` to a fresh temp `.org` file and return the handle.
fn temp_org(content: &str) -> tempfile::NamedTempFile {
    use std::io::Write;
    let mut f = tempfile::Builder::new()
        .suffix(".org")
        .tempfile()
        .expect("create temp org");
    f.write_all(content.as_bytes()).expect("write temp org");
    f.flush().expect("flush");
    f
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin_path())
        .args(args)
        .output()
        .expect("spawn org-cli")
}

fn read(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).expect("read file")
}

fn assert_ok(out: &std::process::Output) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "expected success; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_output_eq(actual: &str, expected: &str) {
    if actual != expected {
        let diff = TextDiff::from_lines(expected, actual);
        eprintln!();
        for line in diff
            .unified_diff()
            .header("expected", "actual")
            .to_string()
            .lines()
        {
            if line.starts_with('-') {
                eprintln!("\x1b[31m{}\x1b[0m", line);
            } else if line.starts_with('+') {
                eprintln!("\x1b[32m{}\x1b[0m", line);
            } else if line.starts_with('@') {
                eprintln!("\x1b[36m{}\x1b[0m", line);
            } else {
                eprintln!("{}", line);
            }
        }
        panic!("Output mismatch - see diff above");
    }
}

// =====================================================================
// set
// =====================================================================

#[test]
fn set_keyword_by_path_write() {
    let f = temp_org(indoc! {"
        * Parent
        ** Child task
    "});
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "Parent/Child task",
        "--keyword",
        "TODO",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(
        read(f.path()),
        indoc! {"
            * Parent
            ** TODO Child task
        "}
    );
}

#[test]
fn set_title_by_line_write() {
    let f = temp_org(indoc! {"
        * First
        * Second
    "});
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "2",
        "--title",
        "Renamed",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(
        read(f.path()),
        indoc! {"
            * First
            * Renamed
        "}
    );
}

#[test]
fn set_no_keyword_removes_it() {
    let f = temp_org("* TODO Task\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--no-keyword",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(read(f.path()), "* Task\n");
}

#[test]
fn set_add_and_remove_tags() {
    let f = temp_org("* Task :keep:drop:\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--add-tag",
        "fresh",
        "--remove-tag",
        "drop",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(read(f.path()), "* Task :keep:fresh:\n");
}

#[test]
fn set_set_tags_replaces_all() {
    let f = temp_org("* Task :a:b:\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--set-tags",
        "x,y",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(read(f.path()), "* Task :x:y:\n");
}

#[test]
fn set_set_tags_empty_removes_all() {
    let f = temp_org("* Task :a:b:\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--set-tags",
        "",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(read(f.path()), "* Task\n");
}

#[test]
fn set_property_and_unset_property() {
    let f = temp_org("* Task\nbody\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--property",
        "K=v",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(
        read(f.path()),
        indoc! {"
            * Task
              :PROPERTIES:
              :K: v
              :END:
            body
        "}
    );

    // Now unset it → back to original.
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--unset-property",
        "K",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(read(f.path()), "* Task\nbody\n");
}

#[test]
fn set_content_from_stdin() {
    let f = temp_org(indoc! {"
        * Task
        old body
        * Next
    "});
    let out = Command::new(bin_path())
        .args([
            "heading",
            "set",
            f.path().to_str().unwrap(),
            "--line",
            "1",
            "--content",
            "-",
            "--write",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(b"new body line\n")?;
            child.wait_with_output()
        })
        .expect("run with stdin");
    assert_ok(&out);
    assert_eq!(
        read(f.path()),
        indoc! {"
            * Task
            new body line
            * Next
        "}
    );
}

#[test]
fn set_uses_in_buffer_todo_keywords() {
    // The file declares WIP; --keyword WIP must be accepted.
    let f = temp_org(indoc! {"
        #+TODO: TODO WIP | DONE
        * Task
    "});
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "2",
        "--keyword",
        "WIP",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(
        read(f.path()),
        indoc! {"
            #+TODO: TODO WIP | DONE
            * WIP Task
        "}
    );
}

#[test]
fn set_multiple_fields_sequential() {
    let f = temp_org("* Old :t:\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--keyword",
        "TODO",
        "--title",
        "New",
        "--set-tags",
        "x",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(read(f.path()), "* TODO New :x:\n");
}

#[test]
fn set_priority_by_line_write() {
    let f = temp_org("* Task\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--priority",
        "A",
        "--write",
    ]);
    assert_ok(&out);
    assert_output_eq(&read(f.path()), "* [#A] Task\n");
}

#[test]
fn set_priority_none_removes_cookie_without_constructing_none_cookie() {
    let f = temp_org("* [#A] Task\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--priority",
        "none",
        "--write",
    ]);
    assert_ok(&out);
    let content = read(f.path());
    assert_output_eq(&content, "* Task\n");
    assert!(!content.contains("[#none]"));
}

#[test]
fn set_priority_and_title_preserve_custom_keyword_in_one_invocation() {
    let f = temp_org(indoc! {"
        #+TODO: TODO WIP | DONE
        * WIP Old
    "});
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "2",
        "--priority",
        "B",
        "--title",
        "New",
        "--write",
    ]);
    assert_ok(&out);
    assert_output_eq(
        &read(f.path()),
        indoc! {"
            #+TODO: TODO WIP | DONE
            * WIP [#B] New
        "},
    );
}

#[test]
fn set_priority_alone_is_a_valid_edit_request() {
    let f = temp_org("* Task\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--priority",
        "C",
        "-j",
    ]);
    assert_ok(&out);
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["op"], "set-priority");
}

#[test]
fn set_priority_invalid_value_is_rejected_before_file_mutation() {
    let original = b"* Task\n";
    let f = temp_org(std::str::from_utf8(original).unwrap());
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--priority",
        "D",
        "--write",
    ]);
    assert_ne!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid value 'D'"));
    assert_eq!(fs::read(f.path()).unwrap(), original);
}

#[test]
fn set_priority_dry_run_prints_full_diff_and_keeps_file_unchanged() {
    let original = "* Task\n";
    let f = temp_org(original);
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--priority",
        "A",
    ]);
    assert_ok(&out);
    assert_output_eq(&read(f.path()), original);
    let expected = format!(
        "--- a/{0}\n+++ b/{0}\n@@ -1 +1 @@\n-* Task\n+* [#A] Task\n(dry run: set-priority at line 1; pass --write to apply)\n",
        f.path().display()
    );
    assert_output_eq(&String::from_utf8(out.stdout).unwrap(), &expected);
}

#[test]
fn set_priority_json_record_uses_letter_values() {
    let f = temp_org("* [#A] Task\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--priority",
        "C",
        "-j",
    ]);
    assert_ok(&out);
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["op"], "set-priority");
    assert_eq!(record["old"], "A");
    assert_eq!(record["new"], "C");
}

#[test]
fn set_priority_nul_record_uses_existing_shape() {
    let f = temp_org("* Task\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--priority",
        "A",
        "-0",
    ]);
    assert_ok(&out);
    let expected = format!("{}\0set-priority\01\0\0", f.path().display());
    assert_output_eq(&String::from_utf8(out.stdout).unwrap(), &expected);
}

#[test]
fn heading_set_rejects_crlf_without_changing_raw_bytes() {
    let original = b"* Task\r\nbody\r\n";
    let mut f = tempfile::Builder::new().suffix(".org").tempfile().unwrap();
    use std::io::Write;
    f.write_all(original).unwrap();
    f.flush().unwrap();

    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--priority",
        "A",
        "--write",
    ]);

    assert_ne!(out.status.code(), Some(0));
    assert_output_eq(
        &String::from_utf8(out.stderr).unwrap(),
        "Error: unsupported CRLF line ending at line 1; heading edits accept LF-only text\n",
    );
    assert_eq!(fs::read(f.path()).unwrap(), original);
}

#[test]
fn heading_set_write_through_symlink_preserves_link() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.org");
    let link = dir.path().join("link.org");
    fs::write(&target, "* Task\n").unwrap();
    symlink("target.org", &link).unwrap();
    let link_before = fs::symlink_metadata(&link).unwrap();

    let out = run(&[
        "heading",
        "set",
        link.to_str().unwrap(),
        "--line",
        "1",
        "--priority",
        "A",
        "--write",
    ]);

    assert_ok(&out);
    assert_output_eq(&fs::read_to_string(&target).unwrap(), "* [#A] Task\n");
    let link_after = fs::symlink_metadata(&link).unwrap();
    assert!(link_after.file_type().is_symlink());
    assert_eq!(link_after.ino(), link_before.ino());
    assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from("target.org"));
}

#[test]
fn heading_set_write_refuses_hard_link_and_preserves_both_names() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.org");
    let second = dir.path().join("second.org");
    fs::write(&first, "* Task\n").unwrap();
    fs::hard_link(&first, &second).unwrap();
    let inode = fs::metadata(&first).unwrap().ino();

    let out = run(&[
        "heading",
        "set",
        first.to_str().unwrap(),
        "--line",
        "1",
        "--priority",
        "A",
        "--write",
    ]);

    assert_ne!(out.status.code(), Some(0));
    assert_output_eq(&fs::read_to_string(&first).unwrap(), "* Task\n");
    assert_output_eq(&fs::read_to_string(&second).unwrap(), "* Task\n");
    assert_eq!(fs::metadata(&first).unwrap().ino(), inode);
    assert_eq!(fs::metadata(&second).unwrap().ino(), inode);
}

// =====================================================================
// dry-run / -j / -0
// =====================================================================

#[test]
fn dry_run_prints_diff_and_does_not_modify() {
    let original = "* Task\n";
    let f = temp_org(original);
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--keyword",
        "TODO",
    ]);
    assert_ok(&out);
    // File is unchanged.
    assert_eq!(read(f.path()), original);
    // Diff appears on stdout with unified headers and the changed lines.
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("---"), "missing --- header:\n{stdout}");
    assert!(stdout.contains("+++"), "missing +++ header:\n{stdout}");
    assert!(
        stdout.contains("-* Task"),
        "missing removed line:\n{stdout}"
    );
    assert!(
        stdout.contains("+* TODO Task"),
        "missing added line:\n{stdout}"
    );
}

#[test]
fn dry_run_json_emits_records() {
    let f = temp_org("* Task\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--keyword",
        "TODO",
        "-j",
    ]);
    assert_ok(&out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "expected one record:\n{stdout}");
    let rec: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSON");
    assert_eq!(rec["op"], "set-keyword");
    assert_eq!(rec["line"], 1);
    assert_eq!(rec["dry_run"], true);
    assert_eq!(
        rec["file"],
        f.path().to_string_lossy().to_string(),
        "file field"
    );
    assert_eq!(rec["new"], "TODO");
}

#[test]
fn write_json_reports_applied() {
    let f = temp_org("* Task\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--keyword",
        "TODO",
        "-j",
        "--write",
    ]);
    assert_ok(&out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let rec: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert_eq!(rec["dry_run"], false);
    assert_eq!(read(f.path()), "* TODO Task\n");
}

#[test]
fn nul_output_format() {
    let f = temp_org("* Task\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--keyword",
        "TODO",
        "-0",
        "--write",
    ]);
    assert_ok(&out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Record layout: <file>\0<op>\0<line>\0\0
    let expected = format!("{}\0{}\0{}\0\0", f.path().display(), "set-keyword", 1);
    assert_eq!(stdout, expected);
}

// =====================================================================
// add
// =====================================================================

#[test]
fn add_under_parent_last() {
    let f = temp_org(indoc! {"
        * Parent
        ** A
        * Other
    "});
    let out = run(&[
        "heading",
        "add",
        f.path().to_str().unwrap(),
        "--under",
        "Parent",
        "--position",
        "last",
        "--title",
        "B",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(
        read(f.path()),
        indoc! {"
            * Parent
            ** A
            ** B
            * Other
        "}
    );
}

#[test]
fn add_top_level_with_keyword_and_tags() {
    let f = temp_org("* One\n");
    let out = run(&[
        "heading",
        "add",
        f.path().to_str().unwrap(),
        "--position",
        "last",
        "--keyword",
        "TODO",
        "--title",
        "Two",
        "--tag",
        "x",
        "--tag",
        "y",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(read(f.path()), "* One\n* TODO Two :x:y:\n");
}

#[test]
fn add_before_index() {
    let f = temp_org(indoc! {"
        * P
        ** A
        ** C
    "});
    let out = run(&[
        "heading",
        "add",
        f.path().to_str().unwrap(),
        "--under",
        "P",
        "--position",
        "before=2",
        "--title",
        "B",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(
        read(f.path()),
        indoc! {"
            * P
            ** A
            ** B
            ** C
        "}
    );
}

#[test]
fn add_with_property_and_content() {
    let f = temp_org("* Parent\n");
    let out = run(&[
        "heading",
        "add",
        f.path().to_str().unwrap(),
        "--under",
        "Parent",
        "--title",
        "Child",
        "--property",
        "K=v",
        "--content",
        "body text",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(
        read(f.path()),
        indoc! {"
            * Parent
            ** Child
              :PROPERTIES:
              :K: v
              :END:
            body text
        "}
    );
}

// =====================================================================
// delete
// =====================================================================

#[test]
fn delete_by_path_removes_subtree() {
    let f = temp_org(indoc! {"
        * One
        ** Child
        body
        * Two
    "});
    let out = run(&[
        "heading",
        "delete",
        f.path().to_str().unwrap(),
        "One",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(read(f.path()), "* Two\n");
}

#[test]
fn delete_dry_run_keeps_file() {
    let original = indoc! {"
        * One
        * Two
    "};
    let f = temp_org(original);
    let out = run(&["heading", "delete", f.path().to_str().unwrap(), "One"]);
    assert_ok(&out);
    assert_eq!(read(f.path()), original);
}

// =====================================================================
// move
// =====================================================================

#[test]
fn move_within_file_after_index() {
    let f = temp_org(indoc! {"
        * P
        ** A
        ** B
        ** C
    "});
    // Move B (currently 2nd child) to after C. After B is extracted the
    // surviving children are [A, C], so C is at index 2 in the
    // destination — insert `after=2`.
    let out = run(&[
        "heading",
        "move",
        f.path().to_str().unwrap(),
        "P/B",
        "--under",
        "P",
        "--position",
        "after=2",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(
        read(f.path()),
        indoc! {"
            * P
            ** A
            ** C
            ** B
        "}
    );
}

#[test]
fn move_within_file_to_top_level() {
    let f = temp_org(indoc! {"
        * P
        ** Child
        body
        * Other
    "});
    let out = run(&[
        "heading",
        "move",
        f.path().to_str().unwrap(),
        "P/Child",
        "--top-level",
        "--position",
        "last",
        "--write",
    ]);
    assert_ok(&out);
    assert_eq!(
        read(f.path()),
        indoc! {"
            * P
            * Other
            * Child
            body
        "}
    );
}

#[test]
fn move_to_another_file_under_path() {
    let src = temp_org(indoc! {"
        * Keep
        * Moving
        body
    "});
    let dst = temp_org(indoc! {"
        * Dest
        ** Existing
    "});
    let out = run(&[
        "heading",
        "move",
        src.path().to_str().unwrap(),
        "Moving",
        "--to-file",
        dst.path().to_str().unwrap(),
        "--under",
        "Dest",
        "--position",
        "last",
        "--write",
    ]);
    assert_ok(&out);
    // Source no longer has Moving.
    assert_eq!(read(src.path()), "* Keep\n");
    // Destination gained Moving as a level-2 child (re-leveled).
    assert_eq!(
        read(dst.path()),
        indoc! {"
            * Dest
            ** Existing
            ** Moving
            body
        "}
    );
}

// =====================================================================
// error handling
// =====================================================================

#[test]
fn ambiguous_path_errors_with_candidates() {
    let f = temp_org(indoc! {"
        * Dup
        * Dup
    "});
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "Dup",
        "--keyword",
        "TODO",
    ]);
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("match"),
        "expected ambiguity message:\n{stderr}"
    );
    assert!(stderr.contains("line 1"), "expected candidates:\n{stderr}");
    assert!(stderr.contains("line 2"), "expected candidates:\n{stderr}");
}

#[test]
fn not_found_path_errors_with_prefix() {
    let f = temp_org(indoc! {"
        * Alpha
        ** One
    "});
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "Alpha/Missing",
        "--keyword",
        "TODO",
    ]);
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("Alpha"),
        "expected deepest-prefix message:\n{stderr}"
    );
}

#[test]
fn line_pointing_at_non_heading_errors() {
    let f = temp_org(indoc! {"
        * Task
        body
    "});
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "2",
        "--keyword",
        "TODO",
    ]);
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("not a heading"),
        "expected not-a-heading message:\n{stderr}"
    );
}

#[test]
fn invalid_keyword_errors_with_allowed() {
    let f = temp_org("* Task\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "--line",
        "1",
        "--keyword",
        "NOPE",
    ]);
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("TODO"),
        "expected allowed-keyword list:\n{stderr}"
    );
}

#[test]
fn set_requires_at_least_one_edit() {
    let f = temp_org("* Task\n");
    let out = run(&["heading", "set", f.path().to_str().unwrap(), "--line", "1"]);
    assert_ne!(out.status.code(), Some(0));
}

#[test]
fn path_and_line_conflict() {
    let f = temp_org("* Task\n");
    let out = run(&[
        "heading",
        "set",
        f.path().to_str().unwrap(),
        "Task",
        "--line",
        "1",
        "--keyword",
        "TODO",
    ]);
    assert_ne!(out.status.code(), Some(0));
}

#[test]
fn move_top_level_and_under_conflict() {
    let f = temp_org(indoc! {"
        * P
        ** Child
    "});
    let out = run(&[
        "heading",
        "move",
        f.path().to_str().unwrap(),
        "P/Child",
        "--top-level",
        "--under",
        "P",
        "--write",
    ]);
    assert_ne!(out.status.code(), Some(0));
}
