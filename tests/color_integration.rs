//! End-to-end tests for global color control.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use indoc::indoc;

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_org-cli"))
}

fn org_fixture(content: &str) -> tempfile::NamedTempFile {
    let mut fixture = tempfile::Builder::new()
        .prefix("org-cli-color-")
        .suffix(".org")
        .tempfile()
        .unwrap();
    fixture.write_all(content.as_bytes()).unwrap();
    fixture
}

fn run(args: &[&str], target: &Path, environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new(bin_path());
    command
        .args(args)
        .arg(target)
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("NO_COLOR")
        .envs(environment.iter().copied());
    command.output().expect("failed to spawn org-cli")
}

fn assert_success_output(output: Output, expected_stdout: &str) {
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, expected_stdout.as_bytes());
    assert_eq!(output.stderr, b"");
}

#[test]
fn color_forces_ansi_in_captured_human_output_before_command() {
    let fixture = org_fixture(indoc! {"
        * Clean
    "});
    let output = run(&["--color", "syntax", "check"], fixture.path(), &[]);

    assert_success_output(output, "\x1b[32mAll org syntax OK.\x1b[0m\n");
}

#[test]
fn no_color_overrides_forced_ansi_after_nested_command() {
    let fixture = org_fixture(indoc! {"
        * Clean
    "});
    let environment = [("CLICOLOR_FORCE", "1")];

    let control = run(&["syntax", "check"], fixture.path(), &environment);
    assert_success_output(control, "\x1b[32mAll org syntax OK.\x1b[0m\n");

    let output = run(
        &["syntax", "check", "--no-color"],
        fixture.path(),
        &environment,
    );
    assert_success_output(output, "All org syntax OK.\n");
}

#[test]
fn automatic_color_is_plain_for_captured_output_and_no_color_environment() {
    let fixture = org_fixture(indoc! {"
        * Clean
    "});

    let captured = run(&["syntax", "check"], fixture.path(), &[]);
    assert_success_output(captured, "All org syntax OK.\n");

    let no_color = run(&["syntax", "check"], fixture.path(), &[("NO_COLOR", "1")]);
    assert_success_output(no_color, "All org syntax OK.\n");
}

#[test]
fn color_overrides_no_color_environment() {
    let fixture = org_fixture(indoc! {"
        * Clean
    "});
    let output = run(
        &["syntax", "check", "--color"],
        fixture.path(),
        &[("NO_COLOR", "1")],
    );

    assert_success_output(output, "\x1b[32mAll org syntax OK.\x1b[0m\n");
}

#[test]
fn simultaneous_color_flags_are_rejected_before_dispatch() {
    let fixture = org_fixture(indoc! {"
        * Clean
    "});
    let output = run(
        &["--color", "syntax", "check", "--no-color"],
        fixture.path(),
        &[],
    );

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    assert_eq!(
        output.stderr,
        b"Error: Cannot use both --color and --no-color\n"
    );
}

#[test]
fn forced_color_does_not_change_json_or_nul_records() {
    let fixture = org_fixture(indoc! {"
        #+begin_src
        body
    "});
    let path_json = serde_json::to_string(&fixture.path().to_string_lossy()).unwrap();

    let json = run(
        &["--color", "syntax", "check", "--json"],
        fixture.path(),
        &[],
    );
    let expected_json = format!(
        concat!(
            "{{\"source_file\":{},\"line\":1,",
            "\"reason_tag\":\"unterminated-block\",",
            "\"reason_message\":\"unterminated `#+begin_src` block (no `#+end_src` before end of file)\",",
            "\"construct\":\"src\",\"raw\":\"#+begin_src\"}}\n"
        ),
        path_json
    );
    assert_eq!(json.status.code(), Some(1));
    assert_eq!(json.stdout, expected_json.as_bytes());
    assert_eq!(json.stderr, b"");

    let nul = run(&["syntax", "check", "-0", "--color"], fixture.path(), &[]);
    let mut expected_nul = fixture.path().as_os_str().as_encoded_bytes().to_vec();
    expected_nul.extend_from_slice(b"\x001\0unterminated-block\0src\0#+begin_src\0-\0\0");
    assert_eq!(nul.status.code(), Some(1));
    assert_eq!(nul.stdout, expected_nul);
    assert_eq!(nul.stderr, b"");
}
