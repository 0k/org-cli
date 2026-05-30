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
fn clock_list_has_property_filters_and_embeds_heading() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("clock")
        .arg("list")
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

    assert_eq!(first["line"], 7);
    assert_eq!(
        first["heading"],
        serde_json::json!({
            "level": 1,
            "line": 1,
            "title": "TODO Billing task",
            "tags": ["urgent"],
            "properties": {"CUSTOM_ID": "bill-248", "TASK_REF": "248"}
        })
    );

    assert_eq!(second["line"], 20);
    assert_eq!(
        second["heading"],
        serde_json::json!({
            "level": 2,
            "line": 15,
            "title": "TODO Nested task",
            "tags": [],
            "properties": {"TASK_REF": "314"}
        })
    );
}

#[test]
fn clock_list_without_filter_is_byte_identical_for_task_ref_fixture() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("clock")
        .arg("list")
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
    let actual = String::from_utf8(out.stdout).unwrap();
    let expected = format!(
        concat!(
            "{{\"file\":\"{}\",\"line\":7,\"start\":\"2025-06-06T09:30:00+02:00\",\"end\":\"2025-06-06T11:45:00+02:00\",\"tz_minutes\":120,\"has_seconds\":false,\"duration_seconds\":8100,\"raw\":\"CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15\"}}\n",
            "{{\"file\":\"{}\",\"line\":12,\"start\":\"2025-06-06T12:00:00+02:00\",\"end\":\"2025-06-06T12:30:00+02:00\",\"tz_minutes\":120,\"has_seconds\":false,\"duration_seconds\":1800,\"raw\":\"CLOCK: [2025-06-06 Fri 12:00 +0200]--[2025-06-06 Fri 12:30 +0200] =>  0:30\"}}\n",
            "{{\"file\":\"{}\",\"line\":20,\"start\":\"2025-06-06T13:00:00+02:00\",\"end\":\"2025-06-06T14:00:00+02:00\",\"tz_minutes\":120,\"has_seconds\":false,\"duration_seconds\":3600,\"raw\":\"CLOCK: [2025-06-06 Fri 13:00 +0200]--[2025-06-06 Fri 14:00 +0200] =>  1:00\"}}\n",
            "{{\"file\":\"{}\",\"line\":28,\"start\":\"2025-06-06T15:00:00+02:00\",\"end\":\"2025-06-06T15:15:00+02:00\",\"tz_minutes\":120,\"has_seconds\":false,\"duration_seconds\":900,\"raw\":\"CLOCK: [2025-06-06 Fri 15:00 +0200]--[2025-06-06 Fri 15:15 +0200] =>  0:15\"}}\n"
        ),
        fixture.display(),
        fixture.display(),
        fixture.display(),
        fixture.display()
    );
    assert_eq!(actual, expected);
}

#[test]
fn clock_list_title_filter_ignore_case_works() {
    let fixture = fixture_path();
    let out = Command::new(bin_path())
        .arg("clock")
        .arg("list")
        .arg("-f")
        .arg("billing")
        .arg("-i")
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
    assert_eq!(lines.len(), 1, "stdout was:\n{}", stdout);
    let record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(record["line"], 7);
    assert_eq!(record["heading"]["title"], "TODO Billing task");
}
