use std::path::{Path, PathBuf};
use std::process::Command;

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_org-cli"))
}

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("task-ref.org")
}

#[test]
fn headings_has_property_returns_expected_set() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("--has-property")
        .arg("TASK_REF")
        .arg("-j")
        .arg(&fixture)
        .output()
        .expect("spawn org-cli");

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "stdout was:\n{}", stdout);
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();

    assert_eq!(first["file"], fixture.to_string_lossy().to_string());
    assert_eq!(first["level"], 1);
    assert_eq!(first["line"], 1);
    assert_eq!(first["title"], "TODO Billing task");
    assert_eq!(first["tags"], serde_json::json!(["urgent"]));
    assert_eq!(
        first["properties"],
        serde_json::json!({"CUSTOM_ID":"bill-248","TASK_REF":"248"})
    );

    assert_eq!(second["level"], 2);
    assert_eq!(second["line"], 15);
    assert_eq!(second["title"], "TODO Nested task");
    assert_eq!(second["tags"], serde_json::json!([]));
    assert_eq!(second["properties"], serde_json::json!({"TASK_REF":"314"}));
}

#[test]
fn headings_filter_ignore_case_returns_expected_set() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("-f")
        .arg("todo")
        .arg("-i")
        .arg("-j")
        .arg(&fixture)
        .output()
        .expect("spawn org-cli");

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let titles: Vec<String> = stdout
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["title"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        titles,
        vec![
            "TODO Billing task".to_string(),
            "TODO Nested task".to_string()
        ]
    );
}

#[test]
fn headings_combined_filter_and_property_is_anded() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("-f")
        .arg("billing")
        .arg("-i")
        .arg("--has-property")
        .arg("TASK_REF")
        .arg("-j")
        .arg(&fixture)
        .output()
        .expect("spawn org-cli");

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "stdout was:\n{}", stdout);
    let line: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(line["title"], "TODO Billing task");
}

#[test]
fn headings_nonexistent_property_returns_empty() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("--has-property")
        .arg("NONEXISTENT")
        .arg(&fixture)
        .output()
        .expect("spawn org-cli");

    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stdout.is_empty(),
        "stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn headings_json_output_matches_expected_bytes() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("--has-property")
        .arg("TASK_REF")
        .arg("-j")
        .arg(&fixture)
        .output()
        .expect("spawn org-cli");

    assert_eq!(out.status.code(), Some(0));
    let actual = String::from_utf8(out.stdout).unwrap();
    let expected = format!(
        concat!(
            "{{\"file\":\"{}\",\"level\":1,\"line\":1,\"title\":\"TODO Billing task\",\"tags\":[\"urgent\"],\"properties\":{{\"CUSTOM_ID\":\"bill-248\",\"TASK_REF\":\"248\"}}}}\n",
            "{{\"file\":\"{}\",\"level\":2,\"line\":15,\"title\":\"TODO Nested task\",\"tags\":[],\"properties\":{{\"TASK_REF\":\"314\"}}}}\n"
        ),
        fixture.display(),
        fixture.display()
    );
    assert_eq!(actual, expected);
}

#[test]
fn headings_nul_output_matches_expected_bytes() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("headings")
        .arg("--has-property")
        .arg("TASK_REF")
        .arg("-0")
        .arg(&fixture)
        .output()
        .expect("spawn org-cli");

    assert_eq!(out.status.code(), Some(0));
    let expected = format!(
        "1\0{}\0{}\0TODO Billing task\0urgent\0\02\0{}\0{}\0TODO Nested task\0\0\0",
        1,
        fixture.display(),
        15,
        fixture.display()
    )
    .into_bytes();
    assert_eq!(out.stdout, expected);
}
