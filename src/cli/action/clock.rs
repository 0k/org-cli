//! `clock` subcommand — `check` and `fmt` for org-mode CLOCK lines.
//!
//! See `doc/plan/05-clock-subcommand.md` for the canonical format,
//! the violation taxonomy, and the design rationale.
//!
//! The pure parser/validator lives in [`crate::org::clock`]. This
//! module is the *orchestration* layer: it walks files, applies
//! `--exclude` globs, formats violations, and prints the canonical
//! line for `fmt`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(test)]
use chrono::NaiveDate;
use chrono::{Local, NaiveDateTime, NaiveTime, Timelike};
use colored::*;
use log::{debug, warn};
use regex::RegexBuilder;
use serde::Serialize;

use crate::cli::action::walk::resolve_input_files;
use crate::config::Config;
use crate::org::clock::{
    check_accepted_zones, fix_weekday_in_line, format_clock_line, parse_clock_line,
    recompute_duration_in_line,
};
use crate::org::clock_add::{HeadingQuery, apply_add};
use crate::org::filter::FilterOptions;
use crate::org::heading::{Heading, heading_for_line};

// =====================================================================
// `clock check`
// =====================================================================

/// CLI options received from `Commands::Clock { Check { … } }`.
#[derive(Debug, Clone)]
pub struct CheckOptions {
    /// Files or directories to scan. Empty means "use the agenda
    /// set" (see [`read_agenda_set`]).
    pub paths: Vec<PathBuf>,
    /// Glob patterns matched against candidate file paths;
    /// matching files are skipped.
    pub exclude_globs: Vec<String>,
    pub nul: bool,
    pub json: bool,
    pub first: bool,
}

/// One violation, format-agnostic.
#[derive(Debug, Clone, Serialize)]
pub struct Violation {
    pub file: PathBuf,
    pub line: usize,
    pub raw: String,
    pub tag: &'static str,
    pub message: String,
}

/// Run `clock check`. Returns 0 on a clean run, 1 when violations
/// were reported, `Err` on usage / I/O errors.
pub fn run_check(opts: CheckOptions) -> Result<i32, String> {
    let files = resolve_input_files(&opts.paths, &opts.exclude_globs)?;
    debug!("scanning {} org file(s)", files.len());

    // Load accepted zones once. Empty list → the
    // ``unaccepted-tz`` check is silently disabled.
    let cfg = Config::load_default()?;
    let zones = cfg.clock.resolve_zones()?;

    let mut violation_count: usize = 0;
    let mut writer = StdoutWriter::new(&opts);

    'files: for file in &files {
        let content = match fs::read_to_string(file) {
            Ok(c) => c,
            Err(e) => {
                warn!("cannot read {}: {}", file.display(), e);
                continue;
            }
        };

        for (idx, raw) in content.lines().enumerate() {
            // Cheap pre-filter so we don't allocate for every line.
            if !raw.contains("CLOCK:") {
                continue;
            }
            // Emit ONE violation record per axis. A line may carry
            // several (e.g. weekday-mismatch + duration-mismatch).
            let mut emit_one = |tag: &'static str, message: String| -> Result<bool, String> {
                violation_count += 1;
                let v = Violation {
                    file: file.clone(),
                    line: idx + 1,
                    raw: raw.to_string(),
                    tag,
                    message,
                };
                writer.emit(&v).map_err(|e| e.to_string())?;
                Ok(opts.first)
            };
            match parse_clock_line(raw) {
                Ok(None) => continue,
                Ok(Some(p)) => {
                    for v in &p.violations {
                        if emit_one(v.tag(), v.message())? {
                            break 'files;
                        }
                    }
                    // Accepted-zones check rides on top of the
                    // structural violations; emit it only when the
                    // line otherwise structurally parses (it does
                    // here — we're inside Ok(Some)).
                    if let Some(v) = check_accepted_zones(&p, &zones)
                        && emit_one(v.tag(), v.message())?
                    {
                        break 'files;
                    }
                }
                Err(err) => {
                    if emit_one(err.tag(), err.message())? {
                        break 'files;
                    }
                }
            }
        }
    }

    writer.finish(violation_count).map_err(|e| e.to_string())?;
    Ok(if violation_count == 0 { 0 } else { 1 })
}

/// Wraps stdout and selects the right serializer per CLI flags.
struct StdoutWriter<'a> {
    opts: &'a CheckOptions,
    stdout: io::BufWriter<io::Stdout>,
}

impl<'a> StdoutWriter<'a> {
    fn new(opts: &'a CheckOptions) -> Self {
        Self {
            opts,
            stdout: io::BufWriter::new(io::stdout()),
        }
    }

    fn emit(&mut self, v: &Violation) -> io::Result<()> {
        if self.opts.json {
            let line = serde_json::to_string(v).map_err(|e| io::Error::other(e.to_string()))?;
            writeln!(self.stdout, "{}", line)
        } else if self.opts.nul {
            // <file>\0<line>\0<tag>\0<message>\0<raw>\0\0
            write!(
                self.stdout,
                "{}\0{}\0{}\0{}\0{}\0\0",
                v.file.display(),
                v.line,
                v.tag,
                v.message,
                v.raw,
            )
        } else {
            writeln!(
                self.stdout,
                "{}:{}: {} {} {}\n    {}",
                v.file.display().to_string().bright_blue(),
                v.line,
                v.tag.bright_red(),
                "->".dimmed(),
                v.message.bright_yellow(),
                v.raw.trim().dimmed(),
            )
        }
    }

    fn finish(&mut self, count: usize) -> io::Result<()> {
        if !self.opts.json && !self.opts.nul {
            if count == 0 {
                writeln!(self.stdout, "{}", "All CLOCK lines OK.".green())?;
            } else {
                writeln!(
                    self.stdout,
                    "\n{} {}",
                    count.to_string().bright_red(),
                    "CLOCK violation(s) found".bright_red(),
                )?;
            }
        }
        self.stdout.flush()
    }
}

// =====================================================================
// `clock fix`
// =====================================================================
//
// Rewrites the axes of a CLOCK line that can be derived
// mechanically from canonical inputs:
//
//   * weekday — from the date
//   * duration suffix (and its Emacs spacing) — from start/end
//
// Axes that depend on human judgement (timezone, the timestamps
// themselves) are NOT touched here. See the consolidation plan.

/// CLI options for `clock fix`.
#[derive(Debug, Clone)]
pub struct FixOptions {
    /// Files or directories to scan. Empty → agenda set.
    pub paths: Vec<PathBuf>,
    pub exclude_globs: Vec<String>,
    /// When true, write changes back. Otherwise, dry-run
    /// (default).
    pub write: bool,
    /// JSON output — one record per rewrite.
    pub json: bool,
}

/// One rewrite record, format-agnostic. The `axis` field tells
/// the consumer which fixer produced this change.
#[derive(Debug, Clone, Serialize)]
pub struct FixRecord {
    pub file: PathBuf,
    pub line: usize,
    /// `"weekday"` or `"duration"`.
    pub axis: &'static str,
    /// For weekday rewrites: `Mon`, `mer.`, etc. For duration
    /// rewrites: the old `=> …` suffix.
    pub from: String,
    /// The replacement token.
    pub to: String,
    pub before: String,
    pub after: String,
}

/// Run `clock fix`. Exit 0 if there was nothing to change (or all
/// changes were applied); 1 in dry-run mode when rewrites would
/// happen.
pub fn run_fix(opts: FixOptions) -> Result<i32, String> {
    let files = resolve_input_files(&opts.paths, &opts.exclude_globs)?;
    debug!("scanning {} org file(s) for weekday-mismatch", files.len());

    let cfg = Config::load_default()?;
    let zones = cfg.clock.resolve_zones()?;

    let mut total_rewrites: usize = 0;
    let mut writer: io::BufWriter<io::Stdout> = io::BufWriter::new(io::stdout());

    for file in &files {
        let content = match fs::read_to_string(file) {
            Ok(c) => c,
            Err(e) => {
                warn!("cannot read {}: {}", file.display(), e);
                continue;
            }
        };

        let mut new_content = String::with_capacity(content.len());
        let mut file_records: Vec<FixRecord> = Vec::new();
        // `split_inclusive` preserves each line's newline state so
        // we can write the file back byte-faithfully.
        for (idx, chunk) in content.split_inclusive('\n').enumerate() {
            let (orig, eol) = match chunk.strip_suffix('\n') {
                Some(stripped) => (stripped, "\n"),
                None => (chunk, ""),
            };
            // Strict accepted-zone gate: a line whose embedded
            // offset matches no configured zone is suspect on the
            // TZ axis itself; we cannot mechanically fix the
            // weekday or duration without first agreeing what
            // the line *means*. Refuse to mutate the whole line.
            if !zones.is_empty()
                && let Ok(Some(p)) = parse_clock_line(orig)
                && check_accepted_zones(&p, &zones).is_some()
            {
                new_content.push_str(orig);
                new_content.push_str(eol);
                continue;
            }
            // Apply weekday fix first. The duration recompute
            // uses the strict parser, which only succeeds on a
            // line whose weekday already matches the date — so
            // when both axes are wrong we must fix the weekday
            // before the duration can be recomputed.
            let after_weekday = fix_weekday_in_line(orig);
            for rw in &after_weekday.rewrites {
                file_records.push(FixRecord {
                    file: file.clone(),
                    line: idx + 1,
                    axis: "weekday",
                    from: rw.old.clone(),
                    to: rw.new.to_string(),
                    before: orig.to_string(),
                    after: after_weekday.line.clone(),
                });
            }
            let after_duration = recompute_duration_in_line(&after_weekday.line);
            if let Some(ch) = &after_duration.change {
                file_records.push(FixRecord {
                    file: file.clone(),
                    line: idx + 1,
                    axis: "duration",
                    from: ch.old.clone(),
                    to: ch.new.clone(),
                    before: after_weekday.line.clone(),
                    after: after_duration.line.clone(),
                });
            }
            new_content.push_str(&after_duration.line);
            new_content.push_str(eol);
        }

        if file_records.is_empty() {
            continue;
        }

        for rec in &file_records {
            emit_fix_record(&mut writer, rec, opts.json, opts.write).map_err(|e| e.to_string())?;
        }
        total_rewrites += file_records.len();

        if opts.write {
            fs::write(file, &new_content)
                .map_err(|e| format!("cannot write {}: {}", file.display(), e))?;
        }
    }

    writer.flush().map_err(|e| e.to_string())?;

    if !opts.json {
        let mut tail = io::stdout();
        if total_rewrites == 0 {
            writeln!(tail, "{}", "No mechanical fixes to apply.".green())
                .map_err(|e| e.to_string())?;
        } else if opts.write {
            writeln!(
                tail,
                "\n{} {}",
                total_rewrites.to_string().bright_green(),
                "rewrite(s) applied".bright_green(),
            )
            .map_err(|e| e.to_string())?;
        } else {
            writeln!(
                tail,
                "\n{} {} (dry run; pass --write to apply)",
                total_rewrites.to_string().bright_yellow(),
                "rewrite(s) would be applied".bright_yellow(),
            )
            .map_err(|e| e.to_string())?;
        }
    }

    // Exit code policy:
    // - dry-run with rewrites: 1 (signals "work pending")
    // - write with rewrites: 0 (work done)
    // - no rewrites at all: 0
    Ok(if total_rewrites > 0 && !opts.write {
        1
    } else {
        0
    })
}

fn emit_fix_record(
    out: &mut io::BufWriter<io::Stdout>,
    rec: &FixRecord,
    json: bool,
    write_mode: bool,
) -> io::Result<()> {
    if json {
        let line = serde_json::to_string(rec).map_err(|e| io::Error::other(e.to_string()))?;
        writeln!(out, "{}", line)
    } else {
        let verb = if write_mode {
            "rewrote"
        } else {
            "would rewrite"
        };
        writeln!(
            out,
            "{}:{}: [{}] {} {:?} → {:?}",
            rec.file.display().to_string().bright_blue(),
            rec.line,
            rec.axis.cyan(),
            verb,
            rec.from,
            rec.to,
        )
    }
}

// =====================================================================
// `clock list`
// =====================================================================

/// CLI options for `clock list`.
#[derive(Debug, Clone)]
pub struct ListOptions {
    /// Files or directories to scan. Empty → agenda set.
    pub paths: Vec<PathBuf>,
    /// Glob patterns matched against candidate file paths;
    /// matching files are skipped.
    pub exclude_globs: Vec<String>,
    /// Match enclosing heading title against regex.
    pub filter: Option<String>,
    /// Case-insensitive enclosing heading title regex.
    pub ignore_case: bool,
    /// Keep only CLOCKs whose enclosing heading has all these properties.
    pub has_property: Vec<String>,
    /// NDJSON output (currently the only supported shape).
    pub json: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HeadingSummary {
    pub level: u32,
    pub line: usize,
    pub title: String,
    pub tags: Vec<String>,
    pub properties: BTreeMap<String, String>,
}

impl From<&Heading> for HeadingSummary {
    fn from(value: &Heading) -> Self {
        Self {
            level: value.level,
            line: value.line,
            title: value.title.clone(),
            tags: value.tags.clone(),
            properties: value.properties.clone(),
        }
    }
}

/// One emitted CLOCK record (clean lines only). All times are
/// serialised as RFC3339 with the line's embedded numerical
/// timezone preserved.
#[derive(Debug, Clone, Serialize)]
pub struct ClockRecord {
    pub file: PathBuf,
    pub line: usize,
    /// `YYYY-MM-DDTHH:MM:SS±HH:MM` (always second precision —
    /// minute-precision lines are reported with `:00` seconds).
    pub start: String,
    pub end: String,
    /// Numerical offset (signed minutes). Same on both sides
    /// (lines that disagreed would have been rejected).
    pub tz_minutes: i32,
    /// Whether the source line carried second precision.
    pub has_seconds: bool,
    /// Duration in whole seconds (`end - start` in UTC terms).
    pub duration_seconds: i64,
    /// The original line, unmodified, including leading
    /// whitespace.
    pub raw: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heading: Option<HeadingSummary>,
}

/// One rejection diagnostic, written to stderr when a CLOCK line
/// fails to pass strict canonical validation.
#[derive(Debug, Clone)]
struct ListReject {
    file: PathBuf,
    line: usize,
    raw: String,
    /// `"missing-tz"`, `"weekday-mismatch"`, … (matches the
    /// taxonomy of `clock check`).
    tag: &'static str,
    message: String,
}

/// Run `clock list`.
///
/// Hard contract: the FIRST line that fails canonical validation
/// (any structural error OR any semantic violation) is reported
/// on stderr and the run exits with code 1 immediately. No
/// further files are read, no further records are emitted. Use
/// ``clock check`` when you want the full violation inventory.
///
/// Records on stdout are byte-faithful representations of source
/// lines that pass strict validation. Because we abort on the
/// first rejection, stdout is either complete (exit 0) or
/// truncated mid-stream (exit 1) — callers should treat exit 1
/// as "fix the rejected line then re-run, the inventory is
/// incomplete".
pub fn run_list(opts: ListOptions) -> Result<i32, String> {
    let files = resolve_input_files(&opts.paths, &opts.exclude_globs)?;
    debug!("listing CLOCK lines from {} org file(s)", files.len());

    let cfg = Config::load_default()?;
    let zones = cfg.clock.resolve_zones()?;
    let heading_filter = build_heading_filter(&opts)?;

    if heading_filter.is_empty() {
        return run_list_unfiltered(&files, &zones, opts.json);
    }

    run_list_filtered(&files, &zones, opts.json, &heading_filter)
}

fn run_list_unfiltered(
    files: &[PathBuf],
    zones: &[chrono_tz::Tz],
    json: bool,
) -> Result<i32, String> {
    let mut stdout = io::BufWriter::new(io::stdout());
    let mut stderr = io::BufWriter::new(io::stderr());

    for file in files {
        let content = match fs::read_to_string(file) {
            Ok(c) => c,
            Err(e) => {
                warn!("cannot read {}: {}", file.display(), e);
                continue;
            }
        };

        for (idx, raw) in content.lines().enumerate() {
            if !raw.contains("CLOCK:") {
                continue;
            }
            match parse_clock_line(raw) {
                Ok(None) => continue,
                Ok(Some(p)) if p.violations.is_empty() => {
                    if let Some(v) = check_accepted_zones(&p, zones) {
                        let rej = ListReject {
                            file: file.clone(),
                            line: idx + 1,
                            raw: raw.to_string(),
                            tag: v.tag(),
                            message: v.message(),
                        };
                        return abort_list(&mut stdout, &mut stderr, &rej);
                    }
                    let rec = build_clock_record(file, idx + 1, raw, &p, None);
                    if json {
                        let line = serde_json::to_string(&rec)
                            .map_err(|e| format!("serializing record: {}", e))?;
                        writeln!(stdout, "{}", line).map_err(|e| e.to_string())?;
                    }
                }
                Ok(Some(p)) => {
                    let v = &p.violations[0];
                    let rej = ListReject {
                        file: file.clone(),
                        line: idx + 1,
                        raw: raw.to_string(),
                        tag: v.tag(),
                        message: v.message(),
                    };
                    return abort_list(&mut stdout, &mut stderr, &rej);
                }
                Err(err) => {
                    let rej = ListReject {
                        file: file.clone(),
                        line: idx + 1,
                        raw: raw.to_string(),
                        tag: err.tag(),
                        message: err.message(),
                    };
                    return abort_list(&mut stdout, &mut stderr, &rej);
                }
            }
        }
    }

    stdout.flush().map_err(|e| e.to_string())?;
    stderr.flush().map_err(|e| e.to_string())?;
    Ok(0)
}

fn run_list_filtered(
    files: &[PathBuf],
    zones: &[chrono_tz::Tz],
    json: bool,
    heading_filter: &FilterOptions,
) -> Result<i32, String> {
    let mut stdout = io::BufWriter::new(io::stdout());
    let mut stderr = io::BufWriter::new(io::stderr());

    for file in files {
        let content = match fs::read_to_string(file) {
            Ok(c) => c,
            Err(e) => {
                warn!("cannot read {}: {}", file.display(), e);
                continue;
            }
        };

        let records = match collect_filtered_clock_records(file, &content, zones, heading_filter) {
            Ok(records) => records,
            Err(rej) => return abort_list(&mut stdout, &mut stderr, &rej),
        };

        if json {
            for rec in records {
                let line = serde_json::to_string(&rec)
                    .map_err(|e| format!("serializing record: {}", e))?;
                writeln!(stdout, "{}", line).map_err(|e| e.to_string())?;
            }
        }
    }

    stdout.flush().map_err(|e| e.to_string())?;
    stderr.flush().map_err(|e| e.to_string())?;
    Ok(0)
}

fn collect_filtered_clock_records(
    file: &Path,
    content: &str,
    zones: &[chrono_tz::Tz],
    heading_filter: &FilterOptions,
) -> Result<Vec<ClockRecord>, ListReject> {
    let mut records = Vec::new();

    for (idx, raw) in content.lines().enumerate() {
        if !raw.contains("CLOCK:") {
            continue;
        }
        match parse_clock_line(raw) {
            Ok(None) => continue,
            Ok(Some(p)) if p.violations.is_empty() => {
                if let Some(v) = check_accepted_zones(&p, zones) {
                    return Err(ListReject {
                        file: file.to_path_buf(),
                        line: idx + 1,
                        raw: raw.to_string(),
                        tag: v.tag(),
                        message: v.message(),
                    });
                }
                let Some(heading) = heading_for_line(content, idx + 1) else {
                    continue;
                };
                if !heading_filter.matches(&heading) {
                    continue;
                }
                records.push(build_clock_record(file, idx + 1, raw, &p, Some(&heading)));
            }
            Ok(Some(p)) => {
                let v = &p.violations[0];
                return Err(ListReject {
                    file: file.to_path_buf(),
                    line: idx + 1,
                    raw: raw.to_string(),
                    tag: v.tag(),
                    message: v.message(),
                });
            }
            Err(err) => {
                return Err(ListReject {
                    file: file.to_path_buf(),
                    line: idx + 1,
                    raw: raw.to_string(),
                    tag: err.tag(),
                    message: err.message(),
                });
            }
        }
    }

    Ok(records)
}

fn build_heading_filter(opts: &ListOptions) -> Result<FilterOptions, String> {
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
        keywords: Vec::new(),
        todo_only: false,
    })
}

/// Emit the rejection diagnostic, flush both streams, and
/// return exit code 1.
fn abort_list<W1: Write, W2: Write>(
    stdout: &mut W1,
    stderr: &mut W2,
    rej: &ListReject,
) -> Result<i32, String> {
    stdout.flush().map_err(|e| e.to_string())?;
    emit_reject(stderr, rej).map_err(|e| e.to_string())?;
    writeln!(
        stderr,
        "\nstopped on first rejection. Use ``org-cli clock check`` \
         for the complete inventory.",
    )
    .map_err(|e| e.to_string())?;
    stderr.flush().map_err(|e| e.to_string())?;
    Ok(1)
}

fn build_clock_record(
    file: &Path,
    line_no: usize,
    raw: &str,
    p: &crate::org::clock::ParsedClockLine,
    heading: Option<&Heading>,
) -> ClockRecord {
    let start_iso = format_iso(
        p.line.start.date,
        p.line.start.hour,
        p.line.start.minute,
        p.line.start.second.unwrap_or(0),
        p.line.start.tz_minutes,
    );
    let end_iso = format_iso(
        p.line.end.date,
        p.line.end.hour,
        p.line.end.minute,
        p.line.end.second.unwrap_or(0),
        p.line.end.tz_minutes,
    );
    let dur_seconds = duration_seconds_of(&p.line);
    ClockRecord {
        file: file.to_path_buf(),
        line: line_no,
        start: start_iso,
        end: end_iso,
        tz_minutes: p.line.start.tz_minutes,
        has_seconds: p.line.has_seconds,
        duration_seconds: dur_seconds,
        raw: raw.to_string(),
        heading: heading.map(HeadingSummary::from),
    }
}

/// Format an instant as `YYYY-MM-DDTHH:MM:SS±HH:MM`.
fn format_iso(
    date: chrono::NaiveDate,
    hour: u32,
    minute: u32,
    second: u32,
    tz_minutes: i32,
) -> String {
    let sign = if tz_minutes < 0 { '-' } else { '+' };
    let abs = tz_minutes.abs();
    format!(
        "{}T{:02}:{:02}:{:02}{}{:02}:{:02}",
        date,
        hour,
        minute,
        second,
        sign,
        abs / 60,
        abs % 60,
    )
}

fn duration_seconds_of(line: &crate::org::clock::ClockLine) -> i64 {
    use chrono::NaiveDateTime;
    let to_naive = |t: &crate::org::clock::Timestamp| -> NaiveDateTime {
        NaiveDateTime::new(
            t.date,
            chrono::NaiveTime::from_hms_opt(t.hour, t.minute, t.second.unwrap_or(0))
                .expect("in-range time"),
        )
    };
    let s = to_naive(&line.start);
    let e = to_naive(&line.end);
    // Both sides have the same TZ at this point (parser would
    // have flagged otherwise), so naive subtraction yields the
    // wall-clock delta which equals the UTC delta.
    (e - s).num_seconds()
}

fn emit_reject<W: Write>(out: &mut W, r: &ListReject) -> io::Result<()> {
    writeln!(
        out,
        "{}:{}: rejected ({}): {}\n    {}",
        r.file.display(),
        r.line,
        r.tag,
        r.message,
        r.raw.trim(),
    )
}

// =====================================================================
// `clock add`
// =====================================================================
//
// Find a heading in an org file and insert one canonical CLOCK
// line into its `:LOGBOOK:` drawer (creating the drawer if
// necessary). Generated through the same
// `format_clock_line` code path as `clock fmt`, so the inserted
// line is by construction `clock check`-clean.

/// CLI options for `clock add`.
#[derive(Debug, Clone)]
pub struct AddOptions {
    pub file: PathBuf,
    /// Heading identifier. Digits-only → TASK_REF lookup;
    /// otherwise case-insensitive substring of the heading
    /// title.
    pub heading: String,
    pub start: String,
    pub end: String,
    pub tz: Option<String>,
    pub seconds: bool,
    pub grid: u32,
    /// Apply the change in place. Without this flag the rewrite
    /// is a dry run; the diff is printed on stdout.
    pub write: bool,
    /// NDJSON record on stdout describing the rewrite.
    pub json: bool,
}

/// Run `clock add`. Returns 0 on success.
pub fn run_add(opts: AddOptions) -> Result<i32, String> {
    if opts.seconds && opts.grid != 0 {
        return Err("--seconds and --grid are mutually exclusive".into());
    }
    if opts.grid != 0 && (opts.grid < 1 || 60 % opts.grid != 0) {
        return Err(format!(
            "--grid must be a positive divisor of 60 (got {})",
            opts.grid
        ));
    }

    // Build the canonical CLOCK line through the same path as
    // `clock fmt` — reuse helpers so behaviour stays consistent.
    let tz = match &opts.tz {
        Some(s) => parse_numerical_tz(s)?,
        None => current_local_tz_minutes(),
    };
    let tz_str = format_numerical_tz(tz);

    let start = parse_user_datetime(&opts.start, "START")?;
    let end = parse_user_datetime(&opts.end, "END")?;
    let (start, end) = if opts.grid > 0 {
        (
            round_to_grid(start, opts.grid),
            round_to_grid(end, opts.grid),
        )
    } else {
        (start, end)
    };
    if end < start {
        return Err("END is before START".into());
    }

    // Producer-side accepted-zone guard (same rule as `clock fmt`).
    let cfg = Config::load_default()?;
    let zones = cfg.clock.resolve_zones()?;
    guard_accepted_zone_at(start, tz, &zones)?;

    let clock_line = format_clock_line(start, end, &tz_str, opts.seconds);

    let content = fs::read_to_string(&opts.file)
        .map_err(|e| format!("cannot read {}: {}", opts.file.display(), e))?;

    let query = HeadingQuery::from_cli(&opts.heading);
    let (new_content, record) =
        apply_add(&content, &query, &clock_line).map_err(|e| e.to_string())?;

    if opts.write {
        fs::write(&opts.file, &new_content)
            .map_err(|e| format!("cannot write {}: {}", opts.file.display(), e))?;
    }

    if opts.json {
        #[derive(Serialize)]
        struct AddRecordOut<'a> {
            file: &'a Path,
            line: usize,
            edit_kind: &'static str,
            clock_line: &'a str,
            heading_line: usize,
            applied: bool,
        }
        let out = AddRecordOut {
            file: &opts.file,
            line: record.line,
            edit_kind: record.edit_kind,
            clock_line: &record.clock_line,
            heading_line: record.heading_line,
            applied: opts.write,
        };
        let line = serde_json::to_string(&out).map_err(|e| format!("serialising record: {}", e))?;
        println!("{}", line);
    } else if opts.write {
        println!(
            "{}: inserted at line {} ({})",
            opts.file.display().to_string().bright_blue(),
            record.line,
            record.edit_kind.cyan(),
        );
        println!("  {}", record.clock_line);
    } else {
        // Dry run: print proposed full file content.
        // Cheap & honest — no diff lib for now.
        print!("{}", new_content);
        eprintln!(
            "\n(dry run; pass --write to apply. Would insert {:?} at line {} of {})",
            record.edit_kind,
            record.line,
            opts.file.display(),
        );
    }

    Ok(0)
}

// =====================================================================
// `clock fmt`
// =====================================================================

/// CLI options for `clock fmt`.
#[derive(Debug, Clone)]
pub struct FmtOptions {
    pub start: String,
    pub end: String,
    /// Numerical zone like `+0200`. `None` means "use the current
    /// system local zone resolved at run time".
    pub tz: Option<String>,
    /// When true, emit `HH:MM:SS` precision; otherwise `HH:MM`.
    pub seconds: bool,
    /// Optional rounding grid in minutes (e.g. 15). 0 disables.
    pub grid: u32,
}

/// Run `clock fmt`. Prints the canonical CLOCK line on stdout.
pub fn run_fmt(opts: FmtOptions) -> Result<(), String> {
    if opts.seconds && opts.grid != 0 {
        return Err("--seconds and --grid are mutually exclusive".into());
    }
    if opts.grid != 0 && (opts.grid < 1 || 60 % opts.grid != 0) {
        return Err(format!(
            "--grid must be a positive divisor of 60 (got {})",
            opts.grid
        ));
    }

    let tz = match &opts.tz {
        Some(s) => parse_numerical_tz(s)?,
        None => current_local_tz_minutes(),
    };
    let tz_str = format_numerical_tz(tz);

    let start = parse_user_datetime(&opts.start, "START")?;
    let end = parse_user_datetime(&opts.end, "END")?;

    let (start, end) = if opts.grid > 0 {
        (
            round_to_grid(start, opts.grid),
            round_to_grid(end, opts.grid),
        )
    } else {
        (start, end)
    };

    if end < start {
        return Err("END is before START".into());
    }

    // Producer-side accepted-zone guard: refuse to emit a line
    // whose offset matches no configured accepted zone at the
    // start instant.  Silent when the config is empty.
    let cfg = Config::load_default()?;
    let zones = cfg.clock.resolve_zones()?;
    guard_accepted_zone_at(start, tz, &zones)?;

    let line = format_clock_line(start, end, &tz_str, opts.seconds);
    println!("{}", line);
    Ok(())
}

/// Producer-side accepted-zone guard.  Errors out if `tz_minutes`
/// (the offset we're about to emit) matches none of the
/// configured zones at `start_naive`'s instant.  No-op when
/// `zones` is empty.
fn guard_accepted_zone_at(
    start_naive: NaiveDateTime,
    tz_minutes: i32,
    zones: &[chrono_tz::Tz],
) -> Result<(), String> {
    if zones.is_empty() {
        return Ok(());
    }
    use chrono::{Offset, TimeZone};
    let any_match = zones.iter().any(|z| {
        let resolved = z
            .from_local_datetime(&start_naive)
            .single()
            .or_else(|| z.from_local_datetime(&start_naive).earliest());
        match resolved {
            Some(dt) => dt.offset().fix().local_minus_utc() / 60 == tz_minutes,
            None => false,
        }
    });
    if any_match {
        Ok(())
    } else {
        let names: Vec<&str> = zones.iter().map(|z| z.name()).collect();
        Err(format!(
            "refusing to emit CLOCK with offset {} on {}: matches no \
             configured accepted zone (configured: {})",
            format_numerical_tz(tz_minutes),
            start_naive.format("%Y-%m-%d %H:%M:%S"),
            names.join(", "),
        ))
    }
}

/// Parse `HH:MM`, `HH:MM:SS`, `YYYY-MM-DD HH:MM`, or
/// `YYYY-MM-DD HH:MM:SS`. When the date is missing, today's local
/// date is used.
fn parse_user_datetime(s: &str, what: &str) -> Result<NaiveDateTime, String> {
    let s = s.trim();
    let today = Local::now().date_naive();

    // Try the time-only variants first.
    for fmt in ["%H:%M:%S", "%H:%M"] {
        if let Ok(t) = NaiveTime::parse_from_str(s, fmt) {
            return Ok(NaiveDateTime::new(today, t));
        }
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(dt);
        }
    }
    Err(format!(
        "{}: cannot parse {:?} (expected HH:MM[:SS] or YYYY-MM-DD HH:MM[:SS])",
        what, s
    ))
}

/// Parse a `+HHMM` / `-HHMM` token to signed minutes.
fn parse_numerical_tz(s: &str) -> Result<i32, String> {
    if s.len() != 5 || !(s.starts_with('+') || s.starts_with('-')) {
        return Err(format!("--tz must look like +HHMM or -HHMM (got {:?})", s));
    }
    let sign: i32 = if s.starts_with('+') { 1 } else { -1 };
    let hh: i32 = s[1..3]
        .parse()
        .map_err(|_| format!("--tz hour part not numeric in {:?}", s))?;
    let mm: i32 = s[3..5]
        .parse()
        .map_err(|_| format!("--tz minute part not numeric in {:?}", s))?;
    if !(0..=23).contains(&hh) || !(0..=59).contains(&mm) {
        return Err(format!("--tz out of range in {:?}", s));
    }
    Ok(sign * (hh * 60 + mm))
}

/// Read the current system's local UTC offset in signed minutes.
fn current_local_tz_minutes() -> i32 {
    Local::now().offset().local_minus_utc() / 60
}

fn format_numerical_tz(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let abs = minutes.abs();
    format!("{}{:02}{:02}", sign, abs / 60, abs % 60)
}

/// Round a [`NaiveDateTime`] to the nearest `grid`-minute mark.
/// `grid` must be a positive divisor of 60.
fn round_to_grid(dt: NaiveDateTime, grid: u32) -> NaiveDateTime {
    let grid_i = grid as i64;
    // Drop seconds for the grid: org-mode CLOCK rounding works at
    // minute resolution.
    let total_min = dt.hour() as i64 * 60 + dt.minute() as i64;
    let remainder = total_min % grid_i;
    let rounded_min = if remainder * 2 >= grid_i {
        total_min + (grid_i - remainder)
    } else {
        total_min - remainder
    };
    let mut day_carry = 0i64;
    let mut new_total = rounded_min;
    while new_total >= 24 * 60 {
        new_total -= 24 * 60;
        day_carry += 1;
    }
    while new_total < 0 {
        new_total += 24 * 60;
        day_carry -= 1;
    }
    let new_date = dt.date() + chrono::Duration::days(day_carry);
    let new_time = NaiveTime::from_hms_opt((new_total / 60) as u32, (new_total % 60) as u32, 0)
        .expect("rounded time in range");
    NaiveDateTime::new(new_date, new_time)
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use std::fs;

    /// Minimal tempdir helper, mirrors the existing `check.rs` one.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            p.push(format!("org-cli-clock-test-{}-{}-{}", tag, pid, nanos));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(p: &Path, s: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, s).unwrap();
    }

    fn opts_for(paths: Vec<PathBuf>) -> CheckOptions {
        CheckOptions {
            paths,
            exclude_globs: Vec::new(),
            nul: false,
            json: true, // tests use json so output is unaffected by colors
            first: false,
        }
    }

    // ----- check ---------------------------------------------------------

    #[test]
    fn check_clean_file_returns_zero() {
        let td = TempDir::new("clean");
        write(
            &td.path().join("a.org"),
            "* h\n  :LOGBOOK:\n  CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15\n  :END:\n",
        );
        assert_eq!(
            run_check(opts_for(vec![td.path().to_path_buf()])).unwrap(),
            0
        );
    }

    #[test]
    fn check_missing_tz_is_reported() {
        let td = TempDir::new("missing-tz");
        write(
            &td.path().join("a.org"),
            "  CLOCK: [2026-01-07 Wed 09:57:20]--[2026-01-07 Wed 12:19:25] =>  2:22:05\n",
        );
        assert_eq!(
            run_check(opts_for(vec![td.path().to_path_buf()])).unwrap(),
            1
        );
    }

    #[test]
    fn check_first_short_circuits() {
        let td = TempDir::new("first");
        write(
            &td.path().join("a.org"),
            "  CLOCK: [2026-01-07 Wed 09:57]--[2026-01-07 Wed 12:19] =>  2:22\n  CLOCK: [2026-01-07 Wed 09:57]--[2026-01-07 Wed 12:19] =>  2:22\n",
        );
        let mut o = opts_for(vec![td.path().to_path_buf()]);
        o.first = true;
        assert_eq!(run_check(o).unwrap(), 1);
    }

    #[test]
    fn check_excludes_globs() {
        let td = TempDir::new("exclude");
        write(
            &td.path().join("bad.org"),
            "  CLOCK: [2026-01-07 Wed 09:57]--[2026-01-07 Wed 12:19] =>  2:22\n",
        );
        let mut o = opts_for(vec![td.path().to_path_buf()]);
        o.exclude_globs.push(format!("{}/**", td.path().display()));
        assert_eq!(run_check(o).unwrap(), 0);
    }

    // ----- fix --------------------------------------------------

    fn fix_opts(paths: Vec<PathBuf>, write: bool) -> FixOptions {
        FixOptions {
            paths,
            exclude_globs: Vec::new(),
            write,
            json: true,
        }
    }

    #[test]
    fn fix_dry_run_does_not_mutate_file() {
        let td = TempDir::new("fix-dry");
        // 2024-04-16 was a Tuesday, not Monday → two rewrites on
        // this single line.
        write(
            &td.path().join("a.org"),
            "  CLOCK: [2024-04-16 Mon 14:15 +0200]--[2024-04-16 Mon 14:30 +0200] =>  0:15\n",
        );
        let exit = run_fix(fix_opts(vec![td.path().to_path_buf()], false)).unwrap();
        assert_eq!(exit, 1, "dry-run with rewrites must exit 1");
        let after = fs::read_to_string(td.path().join("a.org")).unwrap();
        assert!(
            after.contains("Mon") && !after.contains("Tue"),
            "file was mutated in dry-run: {}",
            after
        );
    }

    #[test]
    fn fix_weekday_applied_with_write() {
        let td = TempDir::new("fix-weekday-write");
        write(
            &td.path().join("a.org"),
            "  CLOCK: [2024-04-16 Mon 14:15 +0200]--[2024-04-16 Mon 14:30 +0200] =>  0:15\n",
        );
        let exit = run_fix(fix_opts(vec![td.path().to_path_buf()], true)).unwrap();
        assert_eq!(exit, 0);
        let after = fs::read_to_string(td.path().join("a.org")).unwrap();
        assert_eq!(
            after,
            "  CLOCK: [2024-04-16 Tue 14:15 +0200]--[2024-04-16 Tue 14:30 +0200] =>  0:15\n"
        );
    }

    #[test]
    fn fix_normalises_french_abbreviations() {
        let td = TempDir::new("fix-french");
        write(
            &td.path().join("a.org"),
            "   CLOCK: [2022-05-25 mer. 08:15 +0200]--[2022-05-25 Wed 10:30 +0200] =>  2:15\n",
        );
        let exit = run_fix(fix_opts(vec![td.path().to_path_buf()], true)).unwrap();
        assert_eq!(exit, 0);
        let after = fs::read_to_string(td.path().join("a.org")).unwrap();
        assert!(after.contains("[2022-05-25 Wed 08:15"), "got: {}", after);
        assert!(!after.contains("mer."), "french token survived: {}", after);
    }

    #[test]
    fn fix_recomputes_corrupted_duration() {
        let td = TempDir::new("fix-duration");
        // 720:45 must collapse back to 0:45 (15:00..15:45).
        write(
            &td.path().join("a.org"),
            "  CLOCK: [2024-11-13 Wed 15:00 +0100]--[2024-11-13 Wed 15:45 +0100] => 720:45\n",
        );
        let exit = run_fix(fix_opts(vec![td.path().to_path_buf()], true)).unwrap();
        assert_eq!(exit, 0);
        let after = fs::read_to_string(td.path().join("a.org")).unwrap();
        assert_eq!(
            after,
            "  CLOCK: [2024-11-13 Wed 15:00 +0100]--[2024-11-13 Wed 15:45 +0100] =>  0:45\n"
        );
    }

    #[test]
    fn fix_normalises_emacs_spacing() {
        let td = TempDir::new("fix-spacing");
        // Single-digit hour, only one space between => and digits.
        write(
            &td.path().join("a.org"),
            "  CLOCK: [2022-06-03 Fri 15:00 +0200]--[2022-06-03 Fri 17:30 +0200] => 2:30\n",
        );
        let exit = run_fix(fix_opts(vec![td.path().to_path_buf()], true)).unwrap();
        assert_eq!(exit, 0);
        let after = fs::read_to_string(td.path().join("a.org")).unwrap();
        assert!(after.contains("=>  2:30"), "got: {}", after);
    }

    #[test]
    fn fix_combines_weekday_then_duration_in_one_pass() {
        let td = TempDir::new("fix-both");
        // 2024-11-13 is a Wed; line says Mon AND duration 720:45.
        // After one fix run we expect: weekday rewrite + duration
        // rewrite both applied.
        write(
            &td.path().join("a.org"),
            "  CLOCK: [2024-11-13 Mon 15:00 +0100]--[2024-11-13 Mon 15:45 +0100] => 720:45\n",
        );
        let exit = run_fix(fix_opts(vec![td.path().to_path_buf()], true)).unwrap();
        assert_eq!(exit, 0);
        let after = fs::read_to_string(td.path().join("a.org")).unwrap();
        assert_eq!(
            after,
            "  CLOCK: [2024-11-13 Wed 15:00 +0100]--[2024-11-13 Wed 15:45 +0100] =>  0:45\n"
        );
    }

    #[test]
    fn fix_clean_file_is_noop() {
        let td = TempDir::new("fix-clean");
        let original =
            "  CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15\n";
        write(&td.path().join("a.org"), original);
        let exit = run_fix(fix_opts(vec![td.path().to_path_buf()], true)).unwrap();
        assert_eq!(exit, 0);
        let after = fs::read_to_string(td.path().join("a.org")).unwrap();
        assert_eq!(after, original);
    }

    #[test]
    fn fix_preserves_unrelated_lines_byte_for_byte() {
        let td = TempDir::new("fix-preserve");
        let original = "line one\n\n* heading\n  :LOGBOOK:\n  CLOCK: [2024-04-16 Mon 14:15 +0200]--[2024-04-16 Mon 14:30 +0200] =>  0:15\n  :END:\nfinal line without newline";
        write(&td.path().join("a.org"), original);
        let exit = run_fix(fix_opts(vec![td.path().to_path_buf()], true)).unwrap();
        assert_eq!(exit, 0);
        let after = fs::read_to_string(td.path().join("a.org")).unwrap();
        // Only the day tokens changed; everything else byte-identical.
        let expected = original.replacen("Mon", "Tue", 2);
        assert_eq!(after, expected);
    }

    #[test]
    fn fix_skips_unparseable_lines_for_duration() {
        let td = TempDir::new("fix-unparseable");
        // Missing TZ: weekday is fine, duration suffix bogus.
        // Duration recomputer must NOT touch this (no TZ → cannot
        // compute reliably). Line stays unchanged.
        let original = "  CLOCK: [2026-01-07 Wed 09:57]--[2026-01-07 Wed 12:19] => 9999:99\n";
        write(&td.path().join("a.org"), original);
        let exit = run_fix(fix_opts(vec![td.path().to_path_buf()], true)).unwrap();
        assert_eq!(exit, 0, "nothing to fix mechanically");
        let after = fs::read_to_string(td.path().join("a.org")).unwrap();
        assert_eq!(after, original);
    }

    #[test]
    fn check_running_clock_is_silently_skipped() {
        let td = TempDir::new("running");
        write(
            &td.path().join("a.org"),
            "  CLOCK: [2026-01-07 Wed 09:57]\n", // running clock — out of scope
        );
        assert_eq!(
            run_check(opts_for(vec![td.path().to_path_buf()])).unwrap(),
            0
        );
    }

    // ----- list --------------------------------------------------------

    fn list_opts(paths: Vec<PathBuf>) -> ListOptions {
        ListOptions {
            paths,
            exclude_globs: Vec::new(),
            filter: None,
            ignore_case: false,
            has_property: Vec::new(),
            json: true,
        }
    }

    fn list_opts_with_filter(
        filter: Option<&str>,
        ignore_case: bool,
        has_property: &[&str],
    ) -> ListOptions {
        ListOptions {
            paths: Vec::new(),
            exclude_globs: Vec::new(),
            filter: filter.map(str::to_string),
            ignore_case,
            has_property: has_property
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            json: true,
        }
    }

    #[test]
    fn list_emits_record_for_clean_line() {
        let td = TempDir::new("list-clean");
        write(
            &td.path().join("a.org"),
            "  CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15\n",
        );
        // run_list writes JSON to stdout via println; we only
        // check exit code here (full stdout shape is exercised
        // by the integration test).
        let exit = run_list(list_opts(vec![td.path().to_path_buf()])).unwrap();
        assert_eq!(exit, 0);
    }

    #[test]
    fn list_rejects_missing_tz() {
        let td = TempDir::new("list-missing-tz");
        write(
            &td.path().join("a.org"),
            "  CLOCK: [2026-01-07 Wed 09:57]--[2026-01-07 Wed 12:19] =>  2:22\n",
        );
        let exit = run_list(list_opts(vec![td.path().to_path_buf()])).unwrap();
        assert_eq!(exit, 1, "missing-tz must hard-fail");
    }

    #[test]
    fn list_rejects_tz_mismatch() {
        let td = TempDir::new("list-tz-mismatch");
        write(
            &td.path().join("a.org"),
            "  CLOCK: [2016-08-10 Wed 12:30 +0800]--[2016-08-10 Wed 12:45 +0200] =>  0:15\n",
        );
        let exit = run_list(list_opts(vec![td.path().to_path_buf()])).unwrap();
        assert_eq!(exit, 1, "tz-mismatch must hard-fail");
    }

    #[test]
    fn list_aborts_on_first_rejection_no_subsequent_processing() {
        // Two files: the first contains a clean CLOCK followed by
        // a rejected one; the second contains another clean CLOCK.
        // Hard-fail contract: the second file must NOT be reached.
        let td = TempDir::new("list-abort");
        // ``collect_org_files`` returns paths sorted alphabetically.
        // ``a.org`` is processed before ``b.org``.
        write(
            &td.path().join("a.org"),
            concat!(
                "  CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15\n",
                "  CLOCK: [2026-01-07 Wed 09:57]--[2026-01-07 Wed 12:19] =>  2:22\n",
                "  CLOCK: [2025-06-07 Sat 09:00 +0200]--[2025-06-07 Sat 10:00 +0200] =>  1:00\n",
            ),
        );
        write(
            &td.path().join("b.org"),
            "  CLOCK: [2025-06-08 Sun 09:00 +0200]--[2025-06-08 Sun 10:00 +0200] =>  1:00\n",
        );
        let exit = run_list(list_opts(vec![td.path().to_path_buf()])).unwrap();
        assert_eq!(exit, 1, "first rejection must abort");
        // The unit test cannot easily inspect stdout here (run_list
        // writes via println-equivalent buffered IO). The integration
        // test ``list_hard_fails_on_first_rejection`` covers stdout
        // and stderr contents end-to-end.
    }

    #[test]
    fn list_running_clocks_are_silently_skipped() {
        let td = TempDir::new("list-running");
        write(
            &td.path().join("a.org"),
            "  CLOCK: [2026-01-07 Wed 09:57 +0100]\n",
        );
        let exit = run_list(list_opts(vec![td.path().to_path_buf()])).unwrap();
        assert_eq!(exit, 0);
    }

    #[test]
    fn filtered_list_emits_heading_summary() {
        let content = indoc! {"
            * Billing task
            :PROPERTIES:
            :TASK_REF: 248
            :END:
            CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15
        "};
        let filter =
            build_heading_filter(&list_opts_with_filter(None, false, &["TASK_REF"])).unwrap();

        let records =
            collect_filtered_clock_records(Path::new("a.org"), content, &[], &filter).unwrap();

        assert_eq!(records.len(), 1);
        let heading = records[0].heading.as_ref().expect("heading summary");
        assert_eq!(heading.level, 1);
        assert_eq!(heading.line, 1);
        assert_eq!(heading.title, "Billing task");
        assert_eq!(heading.properties.get("TASK_REF"), Some(&"248".to_string()));
    }

    #[test]
    fn filtered_list_applies_and_semantics_for_regex_and_property() {
        let content = indoc! {"
            * Billing task
            :PROPERTIES:
            :TASK_REF: 248
            :END:
            CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15
            * Notes
            :PROPERTIES:
            :TASK_REF: 314
            :END:
            CLOCK: [2025-06-06 Fri 12:00 +0200]--[2025-06-06 Fri 12:30 +0200] =>  0:30
        "};
        let filter =
            build_heading_filter(&list_opts_with_filter(Some("billing"), true, &["TASK_REF"]))
                .unwrap();

        let records =
            collect_filtered_clock_records(Path::new("a.org"), content, &[], &filter).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].line, 5);
        assert_eq!(
            records[0].heading.as_ref().map(|h| h.title.as_str()),
            Some("Billing task")
        );
    }

    #[test]
    fn filtered_list_skips_headings_missing_property() {
        let content = indoc! {"
            * Billing task
            CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15
            * Tracked task
            :PROPERTIES:
            :TASK_REF: 314
            :END:
            CLOCK: [2025-06-06 Fri 12:00 +0200]--[2025-06-06 Fri 12:30 +0200] =>  0:30
        "};
        let filter =
            build_heading_filter(&list_opts_with_filter(None, false, &["TASK_REF"])).unwrap();

        let records =
            collect_filtered_clock_records(Path::new("a.org"), content, &[], &filter).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].line, 7);
        assert_eq!(
            records[0].heading.as_ref().map(|h| h.title.as_str()),
            Some("Tracked task")
        );
    }

    #[test]
    fn filtered_list_drops_clocks_outside_any_heading() {
        let content = indoc! {"
            CLOCK: [2025-06-06 Fri 08:00 +0200]--[2025-06-06 Fri 08:30 +0200] =>  0:30
            * Billing task
            CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15
        "};
        let filter =
            build_heading_filter(&list_opts_with_filter(Some("billing"), true, &[])).unwrap();

        let records =
            collect_filtered_clock_records(Path::new("a.org"), content, &[], &filter).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].line, 3);
    }

    // ----- add ----------------------------------------------------------

    fn add_opts(file: PathBuf, heading: &str, write: bool) -> AddOptions {
        AddOptions {
            file,
            heading: heading.to_string(),
            start: "2025-06-06 09:30".into(),
            end: "2025-06-06 11:45".into(),
            tz: Some("+0200".into()),
            seconds: false,
            grid: 0,
            write,
            json: true,
        }
    }

    #[test]
    fn add_dry_run_leaves_file_untouched() {
        let td = TempDir::new("add-dry");
        let path = td.path().join("a.org");
        let original = "** WIP target\n  :PROPERTIES:\n  :TASK_REF: 248\n  :END:\n";
        write(&path, original);
        let exit = run_add(add_opts(path.clone(), "248", false)).unwrap();
        assert_eq!(exit, 0);
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(after, original, "dry run must not mutate the file");
    }

    #[test]
    fn add_write_inserts_clock_into_existing_logbook() {
        let td = TempDir::new("add-existing-logbook");
        let path = td.path().join("a.org");
        let original = "** WIP target\n  :LOGBOOK:\n  CLOCK: [2025-06-05 Thu 14:00 +0200]--[2025-06-05 Thu 15:00 +0200] =>  1:00\n  :END:\n";
        write(&path, original);
        let exit = run_add(add_opts(path.clone(), "target", true)).unwrap();
        assert_eq!(exit, 0);
        let after = fs::read_to_string(&path).unwrap();
        assert!(
            after.contains(
                "CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15"
            ),
            "new CLOCK line not present:\n{}",
            after
        );
        // Original CLOCK still there.
        assert!(after.contains("2025-06-05 Thu 14:00"));
    }

    #[test]
    fn add_write_creates_logbook_when_absent() {
        let td = TempDir::new("add-create-logbook");
        let path = td.path().join("a.org");
        let original = "** WIP target\n";
        write(&path, original);
        let exit = run_add(add_opts(path.clone(), "target", true)).unwrap();
        assert_eq!(exit, 0);
        let after = fs::read_to_string(&path).unwrap();
        assert!(
            after.contains(":LOGBOOK:"),
            "drawer not created:\n{}",
            after
        );
        assert!(after.contains(":END:"));
        assert!(after.contains("CLOCK: [2025-06-06 Fri 09:30 +0200]"));
    }

    #[test]
    fn add_inserted_line_round_trips_through_check() {
        // The CLOCK line generated by ``run_add`` must satisfy
        // strict canonical validation — the whole point of
        // routing through the same formatter as ``clock fmt``.
        let td = TempDir::new("add-roundtrip");
        let path = td.path().join("a.org");
        write(&path, "** WIP target\n");
        run_add(add_opts(path.clone(), "target", true)).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        // Locate the inserted CLOCK line and parse it.
        let inserted = after
            .lines()
            .find(|l| l.trim_start().starts_with("CLOCK:"))
            .expect("CLOCK line present");
        let parsed = crate::org::clock::parse_clock_line(inserted)
            .expect("structurally parseable")
            .expect("closed-range");
        assert!(parsed.violations.is_empty(), "got {:?}", parsed.violations);
    }

    #[test]
    fn add_errors_on_missing_heading() {
        let td = TempDir::new("add-missing");
        let path = td.path().join("a.org");
        write(&path, "** unrelated\n");
        let err = run_add(add_opts(path, "999", true)).unwrap_err();
        assert!(
            err.contains("not found") || err.contains(":TASK_REF:"),
            "unexpected error: {}",
            err
        );
    }

    // ----- accepted-zones producer guard --------------------------------

    fn paris_only() -> Vec<chrono_tz::Tz> {
        vec!["Europe/Paris".parse().unwrap()]
    }

    #[test]
    fn guard_silent_when_zones_empty() {
        // Even a wildly unmatching offset is allowed when the
        // user has not configured accepted zones.
        let dt = NaiveDate::from_ymd_opt(2025, 6, 6)
            .unwrap()
            .and_hms_opt(9, 30, 0)
            .unwrap();
        assert!(guard_accepted_zone_at(dt, 330, &[]).is_ok());
    }

    #[test]
    fn guard_accepts_paris_summer_offset() {
        let dt = NaiveDate::from_ymd_opt(2025, 6, 6)
            .unwrap()
            .and_hms_opt(9, 30, 0)
            .unwrap();
        assert!(guard_accepted_zone_at(dt, 120, &paris_only()).is_ok());
    }

    #[test]
    fn guard_rejects_paris_winter_with_summer_offset() {
        let dt = NaiveDate::from_ymd_opt(2026, 1, 7)
            .unwrap()
            .and_hms_opt(9, 30, 0)
            .unwrap();
        let err = guard_accepted_zone_at(dt, 120, &paris_only()).unwrap_err();
        assert!(err.contains("+0200"), "got: {}", err);
        assert!(err.contains("Europe/Paris"), "got: {}", err);
    }

    #[test]
    fn guard_rejects_unrecognised_offset() {
        let dt = NaiveDate::from_ymd_opt(2025, 6, 6)
            .unwrap()
            .and_hms_opt(9, 30, 0)
            .unwrap();
        // +0530 (India) is neither Paris nor Shanghai.
        let zones: Vec<chrono_tz::Tz> = vec![
            "Europe/Paris".parse().unwrap(),
            "Asia/Shanghai".parse().unwrap(),
        ];
        assert!(guard_accepted_zone_at(dt, 330, &zones).is_err());
    }

    // ----- fmt ----------------------------------------------------------

    #[test]
    fn fmt_minute_with_explicit_tz() {
        // Capture the function output indirectly: we exercise the
        // pure formatter, since `run_fmt` writes to stdout.
        let s = format_clock_line(
            NaiveDate::from_ymd_opt(2025, 6, 6)
                .unwrap()
                .and_hms_opt(9, 30, 0)
                .unwrap(),
            NaiveDate::from_ymd_opt(2025, 6, 6)
                .unwrap()
                .and_hms_opt(11, 45, 0)
                .unwrap(),
            "+0200",
            false,
        );
        assert_eq!(
            s,
            "CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15"
        );
    }

    #[test]
    fn fmt_second_precision() {
        let s = format_clock_line(
            NaiveDate::from_ymd_opt(2026, 1, 7)
                .unwrap()
                .and_hms_opt(9, 57, 20)
                .unwrap(),
            NaiveDate::from_ymd_opt(2026, 1, 7)
                .unwrap()
                .and_hms_opt(12, 19, 25)
                .unwrap(),
            "+0100",
            true,
        );
        assert_eq!(
            s,
            "CLOCK: [2026-01-07 Wed 09:57:20 +0100]--[2026-01-07 Wed 12:19:25 +0100] =>  2:22:05"
        );
    }

    #[test]
    fn fmt_round_to_15_minutes() {
        let dt = NaiveDate::from_ymd_opt(2025, 6, 6)
            .unwrap()
            .and_hms_opt(9, 23, 0)
            .unwrap();
        let r = round_to_grid(dt, 15);
        assert_eq!(r.hour(), 9);
        assert_eq!(r.minute(), 30);

        let dt = NaiveDate::from_ymd_opt(2025, 6, 6)
            .unwrap()
            .and_hms_opt(9, 7, 0)
            .unwrap();
        let r = round_to_grid(dt, 15);
        assert_eq!(r.minute(), 0);

        let dt = NaiveDate::from_ymd_opt(2025, 6, 6)
            .unwrap()
            .and_hms_opt(11, 53, 0)
            .unwrap();
        let r = round_to_grid(dt, 15);
        assert_eq!(r.hour(), 12);
        assert_eq!(r.minute(), 0);
    }

    #[test]
    fn fmt_round_carries_to_next_day() {
        let dt = NaiveDate::from_ymd_opt(2025, 6, 6)
            .unwrap()
            .and_hms_opt(23, 53, 0)
            .unwrap();
        let r = round_to_grid(dt, 15);
        assert_eq!(r.date(), NaiveDate::from_ymd_opt(2025, 6, 7).unwrap());
        assert_eq!(r.hour(), 0);
        assert_eq!(r.minute(), 0);
    }

    #[test]
    fn parse_numerical_tz_round_trip() {
        for (s, m) in [
            ("+0000", 0),
            ("+0200", 120),
            ("-0500", -300),
            ("+0530", 330),
        ] {
            assert_eq!(parse_numerical_tz(s).unwrap(), m, "input {}", s);
            assert_eq!(format_numerical_tz(m), s, "minutes {}", m);
        }
    }

    #[test]
    fn parse_numerical_tz_rejects_garbage() {
        for s in ["+00:00", "CEST", "0200", "+0200x", "+99:00"] {
            assert!(parse_numerical_tz(s).is_err(), "should reject {:?}", s);
        }
    }

    #[test]
    fn parse_user_datetime_today_when_date_omitted() {
        let r = parse_user_datetime("09:23", "START").unwrap();
        assert_eq!(r.date(), Local::now().date_naive());
        assert_eq!(r.hour(), 9);
        assert_eq!(r.minute(), 23);
    }

    #[test]
    fn parse_user_datetime_with_full_form() {
        let r = parse_user_datetime("2025-06-06 09:23", "START").unwrap();
        assert_eq!(r.date(), NaiveDate::from_ymd_opt(2025, 6, 6).unwrap());
    }

    #[test]
    fn fmt_seconds_and_grid_are_mutually_exclusive() {
        let opts = FmtOptions {
            start: "09:00".into(),
            end: "10:00".into(),
            tz: Some("+0200".into()),
            seconds: true,
            grid: 15,
        };
        let err = run_fmt(opts).unwrap_err();
        assert!(err.contains("mutually exclusive"), "got: {}", err);
    }
}
