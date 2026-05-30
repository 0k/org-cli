use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use regex::RegexBuilder;
use serde::Serialize;

use crate::cli::action::clock::resolve_input_files;
use crate::org::filter::FilterOptions;
use crate::org::heading::iter_headings;

#[derive(Debug, Clone)]
pub struct HeadingsOptions {
    pub paths: Vec<PathBuf>,
    pub exclude_globs: Vec<String>,
    pub filter: Option<String>,
    pub ignore_case: bool,
    pub has_property: Vec<String>,
    pub nul: bool,
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct HeadingRecord {
    file: PathBuf,
    level: u32,
    line: usize,
    title: String,
    tags: Vec<String>,
    properties: BTreeMap<String, String>,
}

pub fn run(opts: HeadingsOptions) -> Result<i32, String> {
    let files = resolve_input_files(&opts.paths, &opts.exclude_globs)?;
    let filter = build_filter(&opts)?;
    let mut stdout = io::BufWriter::new(io::stdout());

    let mut records = Vec::new();
    for file in &files {
        let content = fs::read_to_string(file)
            .map_err(|e| format!("cannot read {}: {}", file.display(), e))?;
        records.extend(
            iter_headings(&content)
                .filter(|heading| filter.matches(heading))
                .map(|heading| HeadingRecord {
                    file: file.clone(),
                    level: heading.level,
                    line: heading.line,
                    title: heading.title,
                    tags: heading.tags,
                    properties: heading.properties,
                }),
        );
    }

    write_records(&mut stdout, &records, &opts)?;
    stdout.flush().map_err(|e| e.to_string())?;
    Ok(0)
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
        write!(
            out,
            "{}\0{}\0{}\0{}\0{}\0\0",
            record.level,
            record.line,
            record.file.display(),
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
        writeln!(
            out,
            "{:>4}: {} {}{}",
            record.line, stars, record.title, tags
        )
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
        HeadingRecord {
            file: PathBuf::from(file),
            level,
            line,
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

        assert_eq!(out, b"2\x0017\x00/tmp/a.org\x00Nested\x00ops,low\x00\x00");
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
        let expected = "{\"file\":\"/tmp/a.org\",\"level\":2,\"line\":17,\"title\":\"Nested\",\"tags\":[\"ops\"],\"properties\":{\"CUSTOM_ID\":\"cid\",\"TASK_REF\":\"248\"}}\n";
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
            nul: false,
            json: false,
        };

        let err = build_filter(&opts).unwrap_err();
        assert!(err.contains("invalid --filter regex"), "got: {}", err);
    }
}
