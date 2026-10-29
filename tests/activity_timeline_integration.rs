//! Activity-timeline configuration and its shared CLOCK command scope.

use std::path::Path;
use std::process::{Command, Output};

use chrono::NaiveDate;
use indoc::indoc;
use org_cli::org::clock::{ClockKeyword, format_clock_line};

struct Corpus {
    state: tempfile::TempDir,
}

impl Corpus {
    fn new() -> Self {
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(state.path().join("org-cli")).unwrap();
        let start = NaiveDate::from_ymd_opt(2026, 1, 7)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        let clock = format_clock_line(
            ClockKeyword::Clock,
            start,
            start + chrono::Duration::hours(1),
            "+0100",
            false,
            None,
        );
        let mut agenda = String::new();
        for name in ["work.org", "sport.org", "evidence.org"] {
            let path = state.path().join(name);
            std::fs::write(&path, format!("* Activity\n{clock}\n")).unwrap();
            agenda.push_str(&format!("{}\n", path.display()));
        }
        std::fs::write(state.path().join("agenda"), agenda).unwrap();
        Self { state }
    }

    fn path(&self) -> &Path {
        self.state.path()
    }

    fn config(&self, body: &str) {
        std::fs::write(self.path().join("org-cli/config.yml"), body).unwrap();
    }

    fn run(&self, command: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_org-cli"))
            .args(["clock", command, "-j"])
            .args(args)
            .env("HOME", self.path())
            .env("XDG_CONFIG_HOME", self.path())
            .env("ORG_CLI_AGENDA_SET", self.path().join("agenda"))
            .env("ORG_CLI_CLOCK_CACHE", self.path().join("clock.sqlite"))
            .output()
            .unwrap()
    }

    fn listed_files(&self, args: &[&str]) -> Vec<String> {
        let output = self.run("list", args);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let mut files: Vec<_> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| {
                let record: serde_json::Value = serde_json::from_str(line).unwrap();
                Path::new(record["file"].as_str().unwrap())
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        files.sort();
        files
    }
}

#[test]
fn activity_timeline_list_is_opt_in_and_cli_exclusions_narrow_further() {
    let corpus = Corpus::new();
    corpus.config(indoc! {"
        clock:
          activity-timeline:
            exclude-globs:
              - ~/evidence.org
    "});
    for mode in [vec!["--no-cache"], vec![], vec!["--verify"]] {
        assert_eq!(
            corpus.listed_files(&mode),
            ["evidence.org", "sport.org", "work.org"]
        );
        let mut args = mode;
        args.push("--activity-timeline");
        assert_eq!(corpus.listed_files(&args), ["sport.org", "work.org"]);
        args.extend(["--exclude", "**/sport.org"]);
        assert_eq!(corpus.listed_files(&args), ["work.org"]);
    }
}

#[test]
fn activity_timeline_list_accepts_deprecated_overlap_alias_and_warns_once() {
    let corpus = Corpus::new();
    corpus.config("clock:\n  overlap:\n    exclude-globs: ['**/evidence.org']\n");
    let output = corpus.run("list", &["--activity-timeline"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr
            .matches(
                "config: `clock.overlap` is deprecated, rename it to `clock.activity-timeline`"
            )
            .count(),
        1,
        "{stderr}"
    );
    assert_eq!(
        corpus.listed_files(&["--activity-timeline"]),
        ["sport.org", "work.org"]
    );
}

#[test]
fn activity_timeline_config_rejects_both_keys_even_when_empty() {
    let corpus = Corpus::new();
    corpus.config("clock:\n  activity-timeline: {}\n  overlap: {}\n");
    let output = corpus.run("list", &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("clock.activity-timeline"), "{stderr}");
    assert!(stderr.contains("clock.overlap"), "{stderr}");
}

#[test]
fn activity_timeline_overlap_and_check_preserve_alias_results() {
    let corpus = Corpus::new();
    for command in ["overlap", "check"] {
        corpus.config("clock:\n  overlap:\n    exclude-globs: ['**/evidence.org']\n");
        let old = corpus.run(command, &[]);
        corpus.config("clock:\n  activity-timeline:\n    exclude-globs: ['**/evidence.org']\n");
        let new = corpus.run(command, &[]);
        assert_eq!(old.status.code(), Some(1), "{old:?}");
        assert_eq!(new.status.code(), old.status.code(), "{new:?}");
        assert_eq!(new.stdout, old.stdout);
        assert_eq!(String::from_utf8(new.stdout).unwrap().lines().count(), 1);
        let narrowed = corpus.run(command, &["--exclude", "**/sport.org"]);
        assert_eq!(narrowed.status.code(), Some(0), "{narrowed:?}");
        assert_eq!(narrowed.stdout, b"");
    }
}
