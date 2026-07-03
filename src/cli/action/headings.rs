use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use log::warn;
use regex::RegexBuilder;
use serde::Serialize;

use crate::cli::action::walk::resolve_input_files;
use crate::org::filter::FilterOptions;
use crate::org::heading::iter_headings_with;
use crate::org::todo_keywords::{TodoKeywords, parse_spec, scan_buffer};

#[derive(Debug, Clone)]
pub struct HeadingsOptions {
    pub paths: Vec<PathBuf>,
    pub exclude_globs: Vec<String>,
    pub filter: Option<String>,
    pub ignore_case: bool,
    pub has_property: Vec<String>,
    /// Exact-match TODO keyword filter (repeatable `--keyword`).
    pub keyword: Vec<String>,
    /// Keep only headings that carry any TODO keyword (`--todo`).
    pub todo: bool,
    /// Raw `--todo-keywords` spec string applied to files that lack an
    /// in-buffer `#+TODO:` declaration. `None` uses the default set.
    pub todo_keywords: Option<String>,
    pub nul: bool,
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct HeadingRecord {
    file: PathBuf,
    level: u32,
    line: usize,
    /// The org-mode TODO keyword, if any (serialized as `null` when absent).
    keyword: Option<String>,
    /// Whether `keyword` is a *done* (terminal) state for the file's
    /// effective keyword set. `None` when the heading carries no keyword.
    done: Option<bool>,
    title: String,
    tags: Vec<String>,
    properties: BTreeMap<String, String>,
}

pub fn run(opts: HeadingsOptions) -> Result<i32, String> {
    let files = resolve_input_files(&opts.paths, &opts.exclude_globs)?;
    let filter = build_filter(&opts)?;
    let cli_keywords = resolve_cli_keywords(&opts)?;
    let mut stdout = io::BufWriter::new(io::stdout());

    let mut records = Vec::new();
    for file in &files {
        let content = match fs::read_to_string(file) {
            Ok(c) => c,
            Err(e) => {
                warn!("cannot read {}: {}", file.display(), e);
                continue;
            }
        };
        // Per-file resolution: an in-buffer declaration overrides the
        // CLI value, which overrides the default set (Emacs semantics).
        let keywords = scan_buffer(&content).unwrap_or_else(|| cli_keywords.clone());
        records.extend(
            iter_headings_with(&content, &keywords)
                .filter(|heading| filter.matches(heading))
                .map(|heading| {
                    let done = heading
                        .keyword
                        .as_ref()
                        .map(|kw| keywords.done.contains(kw));
                    HeadingRecord {
                        file: file.clone(),
                        level: heading.level,
                        line: heading.line,
                        keyword: heading.keyword,
                        done,
                        title: heading.title,
                        tags: heading.tags,
                        properties: heading.properties,
                    }
                }),
        );
    }

    write_records(&mut stdout, &records, &opts)?;
    stdout.flush().map_err(|e| e.to_string())?;
    Ok(0)
}

/// Resolve the CLI-provided `--todo-keywords` spec into a
/// [`TodoKeywords`], falling back to the default set when unset.
///
/// This is the file-independent baseline; an in-buffer `#+TODO:`
/// declaration takes precedence over it per file (see [`run`]).
fn resolve_cli_keywords(opts: &HeadingsOptions) -> Result<TodoKeywords, String> {
    match &opts.todo_keywords {
        Some(spec) => parse_spec(spec),
        None => Ok(TodoKeywords::default()),
    }
}

fn build_filter(opts: &HeadingsOptions) -> Result<FilterOptions, String> {
    let pattern = match &opts.filter {
        Some(pattern) => Some(
            RegexBuilder::new(pattern)
                .case_insensitive(opts.ignore_case)
                .build()
                .map_err(|e| format!("invalid --filter regex {:?}: {}", pattern, e))?,
        ),
        None => None,
    };

    Ok(FilterOptions {
        pattern,
        ignore_case: opts.ignore_case,
        has_properties: opts.has_property.clone(),
        keywords: opts.keyword.clone(),
        todo_only: opts.todo,
    })
}

fn write_records<W: Write>(
    out: &mut W,
    records: &[HeadingRecord],
    opts: &HeadingsOptions,
) -> Result<(), String> {
    if opts.json {
        write_json(out, records)
    } else if opts.nul {
        write_nul(out, records)
    } else {
        write_human(out, records)
    }
}

fn write_json<W: Write>(out: &mut W, records: &[HeadingRecord]) -> Result<(), String> {
    for record in records {
        let line =
            serde_json::to_string(record).map_err(|e| format!("serializing record: {}", e))?;
        writeln!(out, "{}", line).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn write_nul<W: Write>(out: &mut W, records: &[HeadingRecord]) -> Result<(), String> {
    for record in records {
        let tags = record.tags.join(",");
        let keyword = record.keyword.as_deref().unwrap_or("");
        // `done`: `1`/`0` when a keyword is present, empty otherwise.
        let done = match record.done {
            Some(true) => "1",
            Some(false) => "0",
            None => "",
        };
        write!(
            out,
            "{}\0{}\0{}\0{}\0{}\0{}\0{}\0\0",
            record.level,
            record.line,
            record.file.display(),
            keyword,
            done,
            record.title,
            tags,
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn write_human<W: Write>(out: &mut W, records: &[HeadingRecord]) -> Result<(), String> {
    let mut current_file: Option<&Path> = None;
    for record in records {
        let file = record.file.as_path();
        if current_file != Some(file) {
            if current_file.is_some() {
                writeln!(out).map_err(|e| e.to_string())?;
            }
            writeln!(out, "{}:", file.display()).map_err(|e| e.to_string())?;
            current_file = Some(file);
        }
        let stars =
            "*".repeat(usize::try_from(record.level).map_err(|_| "heading level too large")?);
        let tags = if record.tags.is_empty() {
            String::new()
        } else {
            format!(" :{}:", record.tags.join(":"))
        };
        let title = match &record.keyword {
            Some(keyword) => format!("{} {}", keyword, record.title),
            None => record.title.clone(),
        };
        writeln!(out, "{:>4}: {} {}{}", record.line, stars, title, tags)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{HeadingRecord, HeadingsOptions, build_filter, write_records};
    use indoc::indoc;
    use similar::TextDiff;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

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

    fn record(
        file: &str,
        level: u32,
        line: usize,
        title: &str,
        tags: &[&str],
        properties: &[(&str, &str)],
    ) -> HeadingRecord {
        record_kw(file, level, line, None, None, title, tags, properties)
    }

    #[allow(clippy::too_many_arguments)]
    fn record_kw(
        file: &str,
        level: u32,
        line: usize,
        keyword: Option<&str>,
        done: Option<bool>,
        title: &str,
        tags: &[&str],
        properties: &[(&str, &str)],
    ) -> HeadingRecord {
        HeadingRecord {
            file: PathBuf::from(file),
            level,
            line,
            keyword: keyword.map(str::to_string),
            done,
            title: title.to_string(),
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
            properties: properties
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn opts(nul: bool, json: bool) -> HeadingsOptions {
        HeadingsOptions {
            paths: Vec::new(),
            exclude_globs: Vec::new(),
            filter: None,
            ignore_case: false,
            has_property: Vec::new(),
            keyword: Vec::new(),
            todo: false,
            todo_keywords: None,
            nul,
            json,
        }
    }

    #[test]
    fn human_formatter_renders_expected_bytes() {
        let records = vec![
            record(
                "/tmp/a.org",
                1,
                1,
                "Heading One",
                &["urgent"],
                &[("TASK_REF", "248")],
            ),
            record("/tmp/a.org", 2, 17, "Nested", &[], &[]),
            record("/tmp/b.org", 1, 3, "Other", &["ops", "low"], &[]),
        ];
        let mut out = Vec::new();

        write_records(&mut out, &records, &opts(false, false)).unwrap();

        let actual = String::from_utf8(out).unwrap();
        let expected = indoc! {"
            /tmp/a.org:
               1: * Heading One :urgent:
              17: ** Nested

            /tmp/b.org:
               3: * Other :ops:low:
        "};
        assert_output_eq(&actual, expected);
    }

    #[test]
    fn human_formatter_renders_keyword_between_stars_and_title() {
        let records = vec![
            record_kw(
                "/tmp/a.org",
                1,
                1,
                Some("TODO"),
                Some(false),
                "Buy milk",
                &["home"],
                &[],
            ),
            record_kw(
                "/tmp/a.org",
                1,
                2,
                Some("DONE"),
                Some(true),
                "Ship it",
                &[],
                &[],
            ),
            record("/tmp/a.org", 1, 3, "Plain", &[], &[]),
        ];
        let mut out = Vec::new();

        write_records(&mut out, &records, &opts(false, false)).unwrap();

        let actual = String::from_utf8(out).unwrap();
        let expected = indoc! {"
            /tmp/a.org:
               1: * TODO Buy milk :home:
               2: * DONE Ship it
               3: * Plain
        "};
        assert_output_eq(&actual, expected);
    }

    #[test]
    fn nul_formatter_renders_expected_bytes() {
        let records = vec![record(
            "/tmp/a.org",
            2,
            17,
            "Nested",
            &["ops", "low"],
            &[("TASK_REF", "248")],
        )];
        let mut out = Vec::new();

        write_records(&mut out, &records, &opts(true, false)).unwrap();

        assert_eq!(
            out,
            b"2\x0017\x00/tmp/a.org\x00\x00\x00Nested\x00ops,low\x00\x00"
        );
    }

    #[test]
    fn nul_formatter_renders_keyword_field_when_present() {
        let records = vec![record_kw(
            "/tmp/a.org",
            1,
            1,
            Some("TODO"),
            Some(false),
            "Buy milk",
            &["home"],
            &[],
        )];
        let mut out = Vec::new();

        write_records(&mut out, &records, &opts(true, false)).unwrap();

        assert_eq!(
            out,
            b"1\x001\x00/tmp/a.org\x00TODO\x000\x00Buy milk\x00home\x00\x00"
        );
    }

    #[test]
    fn ndjson_formatter_renders_expected_bytes() {
        let records = vec![record(
            "/tmp/a.org",
            2,
            17,
            "Nested",
            &["ops"],
            &[("CUSTOM_ID", "cid"), ("TASK_REF", "248")],
        )];
        let mut out = Vec::new();

        write_records(&mut out, &records, &opts(false, true)).unwrap();

        let actual = String::from_utf8(out).unwrap();
        let expected = "{\"file\":\"/tmp/a.org\",\"level\":2,\"line\":17,\"keyword\":null,\"done\":null,\"title\":\"Nested\",\"tags\":[\"ops\"],\"properties\":{\"CUSTOM_ID\":\"cid\",\"TASK_REF\":\"248\"}}\n";
        assert_output_eq(&actual, expected);
    }

    #[test]
    fn ndjson_formatter_renders_keyword_field_when_present() {
        let records = vec![record_kw(
            "/tmp/a.org",
            1,
            1,
            Some("TODO"),
            Some(false),
            "Buy milk",
            &[],
            &[],
        )];
        let mut out = Vec::new();

        write_records(&mut out, &records, &opts(false, true)).unwrap();

        let actual = String::from_utf8(out).unwrap();
        let expected = "{\"file\":\"/tmp/a.org\",\"level\":1,\"line\":1,\"keyword\":\"TODO\",\"done\":false,\"title\":\"Buy milk\",\"tags\":[],\"properties\":{}}\n";
        assert_output_eq(&actual, expected);
    }

    #[test]
    fn empty_results_render_empty_stdout_in_all_formats() {
        for option in [opts(false, false), opts(true, false), opts(false, true)] {
            let mut out = Vec::new();
            write_records(&mut out, &[], &option).unwrap();
            assert!(out.is_empty());
        }
    }

    #[test]
    fn build_filter_compiles_case_insensitive_regex_and_properties() {
        let opts = HeadingsOptions {
            paths: Vec::new(),
            exclude_globs: Vec::new(),
            filter: Some("todo".to_string()),
            ignore_case: true,
            has_property: vec!["TASK_REF".to_string()],
            keyword: Vec::new(),
            todo: false,
            todo_keywords: None,
            nul: false,
            json: false,
        };

        let filter = build_filter(&opts).unwrap();
        assert!(filter.ignore_case);
        assert_eq!(filter.has_properties, vec!["TASK_REF".to_string()]);
        assert!(
            filter
                .pattern
                .as_ref()
                .is_some_and(|pattern| pattern.is_match("TODO item"))
        );
    }

    #[test]
    fn build_filter_reports_invalid_regex() {
        let opts = HeadingsOptions {
            paths: Vec::new(),
            exclude_globs: Vec::new(),
            filter: Some("(".to_string()),
            ignore_case: false,
            has_property: Vec::new(),
            keyword: Vec::new(),
            todo: false,
            todo_keywords: None,
            nul: false,
            json: false,
        };

        let err = build_filter(&opts).unwrap_err();
        assert!(err.contains("invalid --filter regex"), "got: {}", err);
    }
}
