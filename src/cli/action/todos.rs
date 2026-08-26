//! `todos` subcommand — sync org TODO headings into a sqlite index
//! and list/query them.
//!
//! Orchestration only: file resolution is delegated to
//! [`crate::cli::action::walk::resolve_input_files`] (agenda fallback),
//! persistence and querying to [`crate::index`]. Output follows the
//! established three-mode per-action pattern (human / `-0` / `-j`).
//!
//! Version fields are appended after the historical seven NUL fields.
//! Existing indices remain stable, but consumers that assert the old exact
//! field count must be updated for the deliberate larger record shape.

use std::io::{self, Write};

use colored::*;

use crate::cli::action::walk::resolve_input_files;
use crate::config::Config;
use crate::index::{TodoQuery, TodoRecord, TodoStore};

/// CLI options for `todos`.
#[derive(Debug, Clone)]
pub struct TodosOptions {
    /// Files or directories to scan. Empty → agenda set.
    pub paths: Vec<std::path::PathBuf>,
    /// Case-insensitive title substring filter.
    pub search: Option<String>,
    /// Exact-match TODO keyword filter.
    pub keyword: Option<String>,
    /// Skip the incremental sync before querying.
    pub no_sync: bool,
    /// Explicit index path, overriding config + default.
    pub db: Option<std::path::PathBuf>,
    pub nul: bool,
    pub json: bool,
}

/// Run `todos`. Returns 0 on success.
pub fn run(opts: TodosOptions) -> Result<i32, String> {
    let db_path = match &opts.db {
        Some(p) => p.clone(),
        None => {
            let cfg = Config::load_default()?;
            cfg.todos.resolve_db_path()?
        }
    };

    let mut store = TodoStore::open(&db_path)?;

    if !opts.no_sync {
        let files = resolve_input_files(&opts.paths, &[])?;
        store.sync(&files)?;
    }

    let query = TodoQuery {
        search: opts.search.clone(),
        keyword: opts.keyword.clone(),
    };
    let records = store.query(&query)?;

    let mut stdout = io::BufWriter::new(io::stdout());
    write_records(&mut stdout, &records, &opts)?;
    stdout.flush().map_err(|e| e.to_string())?;
    Ok(0)
}

fn write_records<W: Write>(
    out: &mut W,
    records: &[TodoRecord],
    opts: &TodosOptions,
) -> Result<(), String> {
    if opts.json {
        write_json(out, records)
    } else if opts.nul {
        write_nul(out, records)
    } else {
        write_human(out, records)
    }
}

/// NDJSON: one serialised [`TodoRecord`] per line.
fn write_json<W: Write>(out: &mut W, records: &[TodoRecord]) -> Result<(), String> {
    for record in records {
        let line =
            serde_json::to_string(record).map_err(|e| format!("serializing record: {}", e))?;
        writeln!(out, "{}", line).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// NUL preserves the historical first seven fields and appends priority,
/// fingerprint, ETag, and compact-JSON TODO keywords.
fn write_nul<W: Write>(out: &mut W, records: &[TodoRecord]) -> Result<(), String> {
    for record in records {
        let tags = record.tags.join(",");
        let is_done = if record.is_done { "1" } else { "0" };
        let priority = record.priority.as_deref().unwrap_or("");
        let todo_keywords = serde_json::to_string(&record.todo_keywords)
            .map_err(|e| format!("serializing TODO keywords: {}", e))?;
        write!(
            out,
            "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0\0",
            record.path,
            record.line,
            record.level,
            record.keyword,
            record.title,
            tags,
            is_done,
            priority,
            record.fingerprint,
            record.etag,
            todo_keywords,
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Human: `path:line: KEYWORD title :tags:`, coloured.
fn write_human<W: Write>(out: &mut W, records: &[TodoRecord]) -> Result<(), String> {
    for record in records {
        let keyword = if record.is_done {
            record.keyword.green().to_string()
        } else {
            record.keyword.bright_red().to_string()
        };
        let tags = if record.tags.is_empty() {
            String::new()
        } else {
            format!(" {}", format!(":{}:", record.tags.join(":")).cyan())
        };
        let priority = record
            .priority
            .as_ref()
            .map(|priority| format!(" [#{}]", priority))
            .unwrap_or_default();
        writeln!(
            out,
            "{}:{}: {}{} {}{}",
            record.path.bright_blue(),
            record.line,
            keyword,
            priority,
            record.title,
            tags,
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::TodoRecord;
    use crate::org::todo_keywords::TodoKeywords;

    fn record(
        path: &str,
        line: usize,
        level: u32,
        keyword: &str,
        title: &str,
        tags: &[&str],
        is_done: bool,
    ) -> TodoRecord {
        TodoRecord {
            path: path.to_string(),
            line,
            level,
            keyword: keyword.to_string(),
            title: title.to_string(),
            tags: tags.iter().map(|t| (*t).to_string()).collect(),
            is_done,
            priority: None,
            fingerprint: "fingerprint".to_string(),
            etag: "etag".to_string(),
            todo_keywords: TodoKeywords::default(),
        }
    }

    fn opts(nul: bool, json: bool) -> TodosOptions {
        TodosOptions {
            paths: Vec::new(),
            search: None,
            keyword: None,
            no_sync: false,
            db: None,
            nul,
            json,
        }
    }

    #[test]
    fn nul_formatter_renders_expected_bytes() {
        let records = vec![record(
            "/tmp/a.org",
            1,
            1,
            "TODO",
            "Buy milk",
            &["home", "urgent"],
            false,
        )];
        let mut out = Vec::new();
        write_records(&mut out, &records, &opts(true, false)).unwrap();
        assert_eq!(
            out,
            b"/tmp/a.org\x001\x001\x00TODO\x00Buy milk\x00home,urgent\x000\x00\x00fingerprint\x00etag\x00{\"active\":[\"TODO\"],\"done\":[\"DONE\"]}\x00\x00"
        );
    }

    #[test]
    fn nul_formatter_marks_done() {
        let records = vec![record("/tmp/a.org", 2, 1, "DONE", "Ship it", &[], true)];
        let mut out = Vec::new();
        write_records(&mut out, &records, &opts(true, false)).unwrap();
        assert_eq!(
            out,
            b"/tmp/a.org\x002\x001\x00DONE\x00Ship it\x00\x001\x00\x00fingerprint\x00etag\x00{\"active\":[\"TODO\"],\"done\":[\"DONE\"]}\x00\x00"
        );
    }

    #[test]
    fn ndjson_formatter_renders_expected_bytes() {
        let records = vec![record(
            "/tmp/a.org",
            1,
            1,
            "TODO",
            "Buy milk",
            &["home"],
            false,
        )];
        let mut out = Vec::new();
        write_records(&mut out, &records, &opts(false, true)).unwrap();
        let actual = String::from_utf8(out).unwrap();
        let expected = "{\"path\":\"/tmp/a.org\",\"line\":1,\"level\":1,\"keyword\":\"TODO\",\"title\":\"Buy milk\",\"tags\":[\"home\"],\"is_done\":false,\"priority\":null,\"fingerprint\":\"fingerprint\",\"etag\":\"etag\",\"todo_keywords\":{\"active\":[\"TODO\"],\"done\":[\"DONE\"]}}\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn human_formatter_renders_path_line_keyword_title_tags() {
        // Force colour off so the assertion is stable regardless of
        // the test runner's TTY state.
        colored::control::set_override(false);
        let mut priority = record("/tmp/a.org", 1, 1, "TODO", "Buy milk", &["home"], false);
        priority.priority = Some("A".to_string());
        let records = vec![
            priority,
            record("/tmp/a.org", 2, 1, "DONE", "Ship it", &[], true),
        ];
        let mut out = Vec::new();
        write_records(&mut out, &records, &opts(false, false)).unwrap();
        let actual = String::from_utf8(out).unwrap();
        let expected = "/tmp/a.org:1: TODO [#A] Buy milk :home:\n/tmp/a.org:2: DONE Ship it\n";
        assert_eq!(actual, expected);
        colored::control::unset_override();
    }

    #[test]
    fn empty_results_render_empty_stdout_in_all_formats() {
        for option in [opts(false, false), opts(true, false), opts(false, true)] {
            let mut out = Vec::new();
            write_records(&mut out, &[], &option).unwrap();
            assert!(out.is_empty());
        }
    }
}
