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

use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use colored::*;
use glob::Pattern;
use log::{debug, warn};
use serde::Serialize;

use crate::cli::action::walk::collect_org_files;
use crate::org::clock::parse_clock_line;

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
    let files = if opts.paths.is_empty() {
        let path = default_agenda_set_path()?;
        debug!("no PATHS given; using agenda set {}", path.display());
        let listed = read_agenda_set(&path)?;
        // Each entry in the agenda set is already a file. We still
        // pipe through `collect_org_files` so the same walking
        // semantics apply to any directories that may appear.
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
                path.display()
            ));
        }
        collect_org_files(&inputs)?
    } else {
        collect_org_files(&opts.paths)?
    };

    let patterns = compile_globs(&opts.exclude_globs)?;
    let files: Vec<PathBuf> = files
        .into_iter()
        .filter(|p| !path_excluded(p, &patterns))
        .collect();
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
            match parse_clock_line(raw) {
                Ok(_) => continue,
                Err(err) => {
                    violation_count += 1;
                    let v = Violation {
                        file: file.clone(),
                        line: idx + 1,
                        raw: raw.to_string(),
                        tag: err.tag(),
                        message: err.message(),
                    };
                    writer.emit(&v).map_err(|e| e.to_string())?;
                    if opts.first {
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

fn format_clock_line(start: NaiveDateTime, end: NaiveDateTime, tz: &str, seconds: bool) -> String {
    let s_ts = format_timestamp(start, tz, seconds);
    let e_ts = format_timestamp(end, tz, seconds);
    let dur = format_duration(end - start, seconds);
    format!("CLOCK: [{}]--[{}] => {}", s_ts, e_ts, dur)
}

fn format_timestamp(dt: NaiveDateTime, tz: &str, seconds: bool) -> String {
    let day = weekday_short(dt.date());
    if seconds {
        format!(
            "{} {} {:02}:{:02}:{:02} {}",
            dt.date(),
            day,
            dt.hour(),
            dt.minute(),
            dt.second(),
            tz
        )
    } else {
        format!(
            "{} {} {:02}:{:02} {}",
            dt.date(),
            day,
            dt.hour(),
            dt.minute(),
            tz
        )
    }
}

fn weekday_short(d: NaiveDate) -> &'static str {
    use chrono::Weekday::*;
    match d.weekday() {
        Mon => "Mon",
        Tue => "Tue",
        Wed => "Wed",
        Thu => "Thu",
        Fri => "Fri",
        Sat => "Sat",
        Sun => "Sun",
    }
}

/// Format a [`chrono::Duration`] as the standard org-mode CLOCK
/// suffix. With seconds: `H:MM:SS` zero-padded on minute and
/// second; without: `H:MM`. The leading hour count is right-padded
/// to one or two digits for visual alignment with org's typical
/// `=> ` followed by two leading spaces.
fn format_duration(d: chrono::Duration, seconds: bool) -> String {
    let secs = d.num_seconds().max(0);
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if seconds {
        format!(" {}:{:02}:{:02}", h, m, s)
    } else {
        format!(" {}:{:02}", h, m)
    }
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
