//! `clock` subcommand — `check` and `fmt` for org-mode CLOCK lines.
//!
//! See `doc/plan/05-clock-subcommand.md` for the canonical format,
//! the violation taxonomy, and the design rationale.
//!
//! The pure parser/validator lives in [`crate::org::clock`]. This
//! module is the *orchestration* layer: it walks files, applies
//! `--exclude` globs, formats violations, and prints the canonical
//! line for `fmt`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{Local, NaiveDateTime, NaiveTime, Timelike};
#[cfg(test)]
use chrono::NaiveDate;
use colored::*;
use glob::Pattern;
use log::{debug, warn};
use serde::Serialize;

use crate::cli::action::walk::collect_org_files;
use crate::org::clock::{
    fix_weekday_in_line, format_clock_line, parse_clock_line, recompute_duration_in_line,
};

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

/// Default location of the agenda-set file (the org-collect output).
fn default_agenda_set_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or("cannot determine home directory")?;
    Ok(home.join(".cache/emacs/org-collect/org-agenda-files.tmp"))
}

/// Read an agenda-set file: one absolute path per line, blank lines
/// and lines beginning with `#` are ignored.
pub fn read_agenda_set(p: &Path) -> Result<Vec<PathBuf>, String> {
    let content = fs::read_to_string(p)
        .map_err(|e| format!("cannot read agenda set {}: {}", p.display(), e))?;
    let mut out = Vec::new();
    for line in content.lines() {
        let s = line.trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        out.push(PathBuf::from(s));
    }
    Ok(out)
}

fn compile_globs(raw: &[String]) -> Result<Vec<Pattern>, String> {
    raw.iter()
        .map(|g| Pattern::new(g).map_err(|e| format!("invalid --exclude glob {:?}: {}", g, e)))
        .collect()
}

fn path_excluded(p: &Path, patterns: &[Pattern]) -> bool {
    let s = p.to_string_lossy();
    patterns.iter().any(|pat| pat.matches(&s))
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

/// Shared input resolver used by `check` and `fix`.
fn resolve_input_files(
    paths: &[PathBuf],
    exclude_globs: &[String],
) -> Result<Vec<PathBuf>, String> {
    let raw = if paths.is_empty() {
        let agenda = default_agenda_set_path()?;
        debug!("no PATHS given; using agenda set {}", agenda.display());
        let listed = read_agenda_set(&agenda)?;
        let inputs: Vec<PathBuf> = listed
            .into_iter()
            .filter(|p| {
                if !p.exists() {
                    warn!("agenda set entry not found, skipping: {}", p.display());
                    false
                } else {
                    true
                }
            })
            .collect();
        if inputs.is_empty() {
            return Err(format!(
                "agenda set at {} resolved to zero existing paths",
                agenda.display()
            ));
        }
        collect_org_files(&inputs)?
    } else {
        collect_org_files(paths)?
    };

    let patterns = compile_globs(exclude_globs)?;
    Ok(raw
        .into_iter()
        .filter(|p| !path_excluded(p, &patterns))
        .collect())
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

    let line = format_clock_line(start, end, &tz_str, opts.seconds);
    println!("{}", line);
    Ok(())
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

    // ----- agenda set loader --------------------------------------------

    #[test]
    fn read_agenda_set_skips_blank_and_comments() {
        let td = TempDir::new("agenda");
        let f = td.path().join("agenda.tmp");
        write(&f, "/tmp/one.org\n\n# a comment\n/tmp/two.org\n   \n");
        let v = read_agenda_set(&f).unwrap();
        assert_eq!(
            v,
            vec![PathBuf::from("/tmp/one.org"), PathBuf::from("/tmp/two.org")]
        );
    }

    #[test]
    fn read_agenda_set_errors_on_missing_file() {
        let td = TempDir::new("agenda-missing");
        let bogus = td.path().join("nope.tmp");
        let err = read_agenda_set(&bogus).unwrap_err();
        assert!(err.contains("cannot read"), "got: {}", err);
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
