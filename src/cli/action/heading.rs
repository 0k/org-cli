//! `heading` subcommand — edit org-mode headings.
//!
//! This is the orchestration layer over the pure ops in
//! [`crate::org::heading_edit`]. It resolves the target heading (by
//! [`HeadingPath`] or explicit line), applies the requested edit(s) to
//! the in-memory file content, and then either prints a unified diff
//! (dry-run, the default) or writes the result atomically.
//!
//! Output honours the project's three-format convention: human-readable
//! by default, NDJSON with `-j`, NUL-separated records with `-0`. In
//! dry-run mode the human format is a unified diff; `-j`/`-0` emit the
//! per-op [`EditRecord`]s regardless of dry-run vs write.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use colored::*;
use serde::Serialize;
use similar::TextDiff;

use crate::org::heading::{HeadingPath, resolve_path, subtree_span};
use crate::org::heading_edit::{
    AddPosition, EditRecord, InsertDest, add_heading, delete_heading, extract_subtree,
    insert_subtree, set_content, set_keyword, set_priority, set_property, set_tags, set_title,
    unset_property, write_atomic,
};
use crate::org::todo_keywords::{TodoKeywords, scan_buffer};

// ---------------------------------------------------------------------
// Shared option enums / helpers
// ---------------------------------------------------------------------

/// How the target heading is addressed on the command line.
enum Target {
    Path(String),
    Line(usize),
}

impl Target {
    /// Build from the mutually exclusive `path` / `line` args.
    fn from_args(path: Option<String>, line: Option<usize>) -> Result<Self, String> {
        match (path, line) {
            (Some(p), None) => Ok(Target::Path(p)),
            (None, Some(l)) => Ok(Target::Line(l)),
            (Some(_), Some(_)) => Err("PATH and --line are mutually exclusive".into()),
            (None, None) => Err("a heading PATH or --line N is required".into()),
        }
    }

    /// Resolve to a 1-based heading line within `content`.
    fn resolve(&self, content: &str) -> Result<usize, String> {
        match self {
            Target::Path(p) => {
                let path: HeadingPath = p.parse().map_err(|e: String| e)?;
                let heading = resolve_path(content, &path).map_err(|e| e.to_string())?;
                Ok(heading.line)
            }
            Target::Line(l) => {
                // Validate it points at a real heading via a span probe.
                subtree_span(content, *l)
                    .map(|_| *l)
                    .ok_or_else(|| format!("line {} is not a heading", l))
            }
        }
    }
}

/// Parse a `--position` string into an [`AddPosition`].
fn parse_position(s: &str) -> Result<AddPosition, String> {
    match s {
        "first" => Ok(AddPosition::First),
        "last" => Ok(AddPosition::Last),
        other => {
            if let Some(n) = other.strip_prefix("before=") {
                let n = n
                    .parse::<usize>()
                    .map_err(|_| format!("invalid position index in {:?}", other))?;
                Ok(AddPosition::Before(n))
            } else if let Some(n) = other.strip_prefix("after=") {
                let n = n
                    .parse::<usize>()
                    .map_err(|_| format!("invalid position index in {:?}", other))?;
                Ok(AddPosition::After(n))
            } else {
                Err(format!(
                    "invalid --position {:?}: expected first|last|before=N|after=N",
                    other
                ))
            }
        }
    }
}

/// Parse a `KEY=VALUE` string into a `(key, value)` pair.
fn parse_kv(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(format!("invalid property {:?}: expected KEY=VALUE", s)),
    }
}

/// Resolve a `--content STR` / `--content-file F` pair into the body
/// text. `-` on `--content` reads stdin.
fn resolve_body(
    content: Option<String>,
    content_file: Option<PathBuf>,
) -> Result<Option<String>, String> {
    match (content, content_file) {
        (Some(_), Some(_)) => Err("--content and --content-file are mutually exclusive".into()),
        (Some(s), None) => {
            if s == "-" {
                let mut buf = String::new();
                io::stdin()
                    .read_to_string(&mut buf)
                    .map_err(|e| format!("reading stdin: {}", e))?;
                Ok(Some(buf))
            } else {
                Ok(Some(s))
            }
        }
        (None, Some(f)) => {
            let s = std::fs::read_to_string(&f)
                .map_err(|e| format!("cannot read {}: {}", f.display(), e))?;
            Ok(Some(s))
        }
        (None, None) => Ok(None),
    }
}

/// Read a file into a string with a friendly error.
fn read_file(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))
}

// ---------------------------------------------------------------------
// Record output
// ---------------------------------------------------------------------

/// One record as emitted to `-j` / `-0`: the pure [`EditRecord`] plus the
/// file it applied to and whether the run actually wrote to disk.
#[derive(Serialize)]
struct OpOut<'a> {
    file: &'a Path,
    #[serde(flatten)]
    record: &'a EditRecord,
    dry_run: bool,
}

/// How to present the outcome.
#[derive(Clone, Copy)]
struct OutputMode {
    json: bool,
    nul: bool,
    write: bool,
}

/// Emit the records + (in human dry-run mode) the unified diff.
fn present(
    file: &Path,
    original: &str,
    new_content: &str,
    records: &[EditRecord],
    mode: OutputMode,
) -> Result<(), String> {
    let mut stdout = io::BufWriter::new(io::stdout());
    if mode.json {
        for rec in records {
            let out = OpOut {
                file,
                record: rec,
                dry_run: !mode.write,
            };
            let line =
                serde_json::to_string(&out).map_err(|e| format!("serialising record: {}", e))?;
            writeln!(stdout, "{}", line).map_err(|e| e.to_string())?;
        }
    } else if mode.nul {
        // <file>\0<op>\0<line>\0\0 per record.
        for rec in records {
            write!(stdout, "{}\0{}\0{}\0\0", file.display(), rec.op, rec.line)
                .map_err(|e| e.to_string())?;
        }
    } else if mode.write {
        for rec in records {
            writeln!(
                stdout,
                "{}: {} at line {}",
                file.display().to_string().bright_blue(),
                rec.op.cyan(),
                rec.line,
            )
            .map_err(|e| e.to_string())?;
        }
    } else {
        // Dry-run human: unified diff.
        print_unified_diff(&mut stdout, file, original, new_content).map_err(|e| e.to_string())?;
        for rec in records {
            writeln!(
                stdout,
                "{}",
                format!(
                    "(dry run: {} at line {}; pass --write to apply)",
                    rec.op, rec.line
                )
                .dimmed(),
            )
            .map_err(|e| e.to_string())?;
        }
    }
    stdout.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// Write a standard unified diff (with `---`/`+++` headers) of
/// `original` → `new_content`.
fn print_unified_diff<W: Write>(
    out: &mut W,
    file: &Path,
    original: &str,
    new_content: &str,
) -> io::Result<()> {
    let diff = TextDiff::from_lines(original, new_content);
    let header_old = format!("a/{}", file.display());
    let header_new = format!("b/{}", file.display());
    let text = diff
        .unified_diff()
        .header(&header_old, &header_new)
        .to_string();
    write!(out, "{}", text)
}

// ---------------------------------------------------------------------
// `heading set`
// ---------------------------------------------------------------------

/// CLI options for `heading set`.
pub struct SetOptions {
    pub file: PathBuf,
    pub path: Option<String>,
    pub line: Option<usize>,
    pub keyword: Option<String>,
    pub no_keyword: bool,
    pub priority: Option<String>,
    pub title: Option<String>,
    pub add_tag: Vec<String>,
    pub remove_tag: Vec<String>,
    pub set_tags: Option<String>,
    pub property: Vec<String>,
    pub unset_property: Vec<String>,
    pub content: Option<String>,
    pub content_file: Option<PathBuf>,
    pub write: bool,
    pub json: bool,
    pub nul: bool,
}

/// Run `heading set`. Applies keyword, priority, title, tags,
/// properties, and content sequentially in heading-anatomy order,
/// re-using the same target line (which is invariant across a heading's
/// own edits, since every edit lands on or below the heading line).
pub fn run_set(opts: SetOptions) -> Result<i32, String> {
    let target = Target::from_args(opts.path.clone(), opts.line)?;

    // Collect the desired edits, validating flag combinations up front.
    let body = resolve_body(opts.content.clone(), opts.content_file.clone())?;
    let has_keyword_edit = opts.keyword.is_some() || opts.no_keyword;
    let has_tag_edit =
        opts.set_tags.is_some() || !opts.add_tag.is_empty() || !opts.remove_tag.is_empty();
    let has_edit = has_keyword_edit
        || opts.priority.is_some()
        || opts.title.is_some()
        || has_tag_edit
        || !opts.property.is_empty()
        || !opts.unset_property.is_empty()
        || body.is_some();
    if !has_edit {
        return Err("no edit requested: pass at least one field flag".into());
    }

    let original = read_file(&opts.file)?;
    let keywords = file_keywords(&original);

    let mut content = original.clone();
    let mut records: Vec<EditRecord> = Vec::new();

    // The heading line stays stable across a heading's own edits.
    let line = target.resolve(&content)?;

    // 1. keyword
    if has_keyword_edit {
        let kw = if opts.no_keyword {
            None
        } else {
            opts.keyword.as_deref()
        };
        let (out, rec) = set_keyword(&content, line, kw, &keywords).map_err(|e| e.to_string())?;
        content = out;
        records.push(rec);
    }

    // 2. priority
    if let Some(priority) = &opts.priority {
        let priority = (priority != "none").then_some(priority.as_str());
        let (out, rec) =
            set_priority(&content, line, priority, &keywords).map_err(|e| e.to_string())?;
        content = out;
        records.push(rec);
    }

    // 3. title
    if let Some(title) = &opts.title {
        let (out, rec) = set_title(&content, line, title, &keywords).map_err(|e| e.to_string())?;
        content = out;
        records.push(rec);
    }

    // 4. tags
    if let Some(spec) = &opts.set_tags {
        let tags = parse_tag_list(spec);
        let (out, rec) = set_tags(&content, line, &tags).map_err(|e| e.to_string())?;
        content = out;
        records.push(rec);
    } else if has_tag_edit {
        // Merge add/remove into existing tags.
        let existing = current_tags(&content, line);
        let mut tags = existing;
        for t in &opts.remove_tag {
            tags.retain(|x| x != t);
        }
        for t in &opts.add_tag {
            if !tags.contains(t) {
                tags.push(t.clone());
            }
        }
        let (out, rec) = set_tags(&content, line, &tags).map_err(|e| e.to_string())?;
        content = out;
        records.push(rec);
    }

    // 5. properties (set then unset, in flag order)
    for kv in &opts.property {
        let (key, value) = parse_kv(kv)?;
        let (out, rec) = set_property(&content, line, &key, &value).map_err(|e| e.to_string())?;
        content = out;
        records.push(rec);
    }
    for key in &opts.unset_property {
        let (out, rec) = unset_property(&content, line, key).map_err(|e| e.to_string())?;
        content = out;
        records.push(rec);
    }

    // 6. content
    if let Some(b) = &body {
        let (out, rec) = set_content(&content, line, b).map_err(|e| e.to_string())?;
        content = out;
        records.push(rec);
    }

    finish(
        &opts.file, &original, &content, &records, opts.write, opts.json, opts.nul,
    )
}

// ---------------------------------------------------------------------
// `heading add`
// ---------------------------------------------------------------------

/// CLI options for `heading add`.
pub struct AddOptions {
    pub file: PathBuf,
    pub under: Option<String>,
    pub position: String,
    pub keyword: Option<String>,
    pub title: String,
    pub tag: Vec<String>,
    pub property: Vec<String>,
    pub content: Option<String>,
    pub content_file: Option<PathBuf>,
    pub write: bool,
    pub json: bool,
    pub nul: bool,
}

/// Run `heading add`.
pub fn run_add(opts: AddOptions) -> Result<i32, String> {
    let pos = parse_position(&opts.position)?;
    let body = resolve_body(opts.content.clone(), opts.content_file.clone())?;
    let properties: Vec<(String, String)> = opts
        .property
        .iter()
        .map(|kv| parse_kv(kv))
        .collect::<Result<_, _>>()?;

    let original = read_file(&opts.file)?;
    let keywords = file_keywords(&original);

    let parent_line = match &opts.under {
        Some(p) => {
            let path: HeadingPath = p.parse().map_err(|e: String| e)?;
            Some(
                resolve_path(&original, &path)
                    .map_err(|e| e.to_string())?
                    .line,
            )
        }
        None => None,
    };

    let (content, record) = add_heading(
        &original,
        parent_line,
        pos,
        opts.keyword.as_deref(),
        &opts.title,
        &opts.tag,
        &properties,
        body.as_deref(),
        &keywords,
    )
    .map_err(|e| e.to_string())?;

    finish(
        &opts.file,
        &original,
        &content,
        &[record],
        opts.write,
        opts.json,
        opts.nul,
    )
}

// ---------------------------------------------------------------------
// `heading delete`
// ---------------------------------------------------------------------

/// CLI options for `heading delete`.
pub struct DeleteOptions {
    pub file: PathBuf,
    pub path: Option<String>,
    pub line: Option<usize>,
    pub write: bool,
    pub json: bool,
    pub nul: bool,
}

/// Run `heading delete`.
pub fn run_delete(opts: DeleteOptions) -> Result<i32, String> {
    let target = Target::from_args(opts.path.clone(), opts.line)?;
    let original = read_file(&opts.file)?;
    let line = target.resolve(&original)?;
    let (content, record) = delete_heading(&original, line).map_err(|e| e.to_string())?;
    finish(
        &opts.file,
        &original,
        &content,
        &[record],
        opts.write,
        opts.json,
        opts.nul,
    )
}

// ---------------------------------------------------------------------
// `heading move`
// ---------------------------------------------------------------------

/// CLI options for `heading move`.
pub struct MoveOptions {
    pub file: PathBuf,
    pub path: Option<String>,
    pub line: Option<usize>,
    pub to_file: Option<PathBuf>,
    pub under: Option<String>,
    pub top_level: bool,
    pub position: String,
    pub write: bool,
    pub json: bool,
    pub nul: bool,
}

/// Run `heading move`.
///
/// Within one file: extract + insert on the same in-memory content, then
/// a single write. Cross-file: the destination file is written *first*,
/// then the source — so a crash between the two writes duplicates the
/// subtree rather than losing it (crash-safe ordering per the plan).
pub fn run_move(opts: MoveOptions) -> Result<i32, String> {
    let pos = parse_position(&opts.position)?;
    let target = Target::from_args(opts.path.clone(), opts.line)?;

    let src_content = read_file(&opts.file)?;
    let src_line = target.resolve(&src_content)?;

    let same_file = match &opts.to_file {
        None => true,
        Some(dst) => paths_equal(&opts.file, dst),
    };

    if same_file {
        // Extract + insert on one in-memory buffer.
        let (without, block, _extract_rec) =
            extract_subtree(&src_content, src_line).map_err(|e| e.to_string())?;
        let dest = destination(&without, &opts.under, opts.top_level, pos)?;
        let (new_content, insert_rec) =
            insert_subtree(&without, &block, dest).map_err(|e| e.to_string())?;
        finish(
            &opts.file,
            &src_content,
            &new_content,
            &[insert_rec],
            opts.write,
            opts.json,
            opts.nul,
        )
    } else {
        let dst_path = opts.to_file.clone().expect("checked not same file");
        let dst_content = read_file(&dst_path)?;

        // Extract from source (in memory).
        let (src_without, block, _extract_rec) =
            extract_subtree(&src_content, src_line).map_err(|e| e.to_string())?;
        // Insert into destination (in memory).
        let dest = destination(&dst_content, &opts.under, opts.top_level, pos)?;
        let (dst_new, insert_rec) =
            insert_subtree(&dst_content, &block, dest).map_err(|e| e.to_string())?;

        if opts.write {
            // Destination first (crash-safe: a crash duplicates, never loses).
            write_atomic(&dst_path, &dst_new).map_err(|e| e.to_string())?;
            write_atomic(&opts.file, &src_without).map_err(|e| e.to_string())?;
        }

        // Report both sides. In dry-run human mode, show the destination
        // diff (the interesting side); records cover both files.
        let src_rec = EditRecord {
            op: "move-source-removed",
            line: src_line,
            old: None,
            new: None,
            key: None,
            span: None,
        };
        // Present both sides; `present` picks the right shape per mode
        // (records for -j/-0, unified diff for dry-run, one-line summary
        // for --write).
        let mode = OutputMode {
            json: opts.json,
            nul: opts.nul,
            write: opts.write,
        };
        present(&opts.file, &src_content, &src_without, &[src_rec], mode)?;
        present(&dst_path, &dst_content, &dst_new, &[insert_rec], mode)?;
        Ok(0)
    }
}

/// Build an [`InsertDest`] for a move, honouring `--under` / `--top-level`.
fn destination(
    content: &str,
    under: &Option<String>,
    top_level: bool,
    pos: AddPosition,
) -> Result<InsertDest, String> {
    if top_level {
        return Ok(InsertDest {
            parent_line: None,
            pos,
        });
    }
    let parent_line = match under {
        Some(p) => {
            let path: HeadingPath = p.parse().map_err(|e: String| e)?;
            Some(
                resolve_path(content, &path)
                    .map_err(|e| e.to_string())?
                    .line,
            )
        }
        None => None,
    };
    Ok(InsertDest { parent_line, pos })
}

// ---------------------------------------------------------------------
// Shared finishing helper
// ---------------------------------------------------------------------

/// Apply the write (if requested) and present the outcome for the
/// single-file ops.
fn finish(
    file: &Path,
    original: &str,
    new_content: &str,
    records: &[EditRecord],
    write: bool,
    json: bool,
    nul: bool,
) -> Result<i32, String> {
    if write {
        write_atomic(file, new_content).map_err(|e| e.to_string())?;
    }
    let mode = OutputMode { json, nul, write };
    present(file, original, new_content, records, mode)?;
    Ok(0)
}

// ---------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------

/// The TODO keyword set for a file: the in-buffer `#+TODO:` declaration
/// when present, else the default `TODO`/`DONE` — matching the
/// `headings` action's semantics.
fn file_keywords(content: &str) -> TodoKeywords {
    scan_buffer(content).unwrap_or_default()
}

/// Split a `--set-tags` spec ("a,b,c") into tags, dropping empties so an
/// empty string cleanly removes all tags.
fn parse_tag_list(spec: &str) -> Vec<String> {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The current tags of the heading at `line`, via a fresh parse.
fn current_tags(content: &str, line: usize) -> Vec<String> {
    crate::org::heading::iter_headings(content)
        .find(|h| h.line == line)
        .map(|h| h.tags)
        .unwrap_or_default()
}

/// Compare two paths for "same file" purposes: canonicalize when both
/// exist, else fall back to a lexical comparison.
fn paths_equal(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}
