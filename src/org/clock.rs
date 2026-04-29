//! Parser and validator for org-mode `CLOCK` lines.
//!
//! A *closed-range* CLOCK line is the only thing this module
//! recognises. Its canonical shape is:
//!
//! ```text
//! CLOCK: [YYYY-MM-DD Day HH:MM[:SS] ±ZZZZ]--[YYYY-MM-DD Day HH:MM[:SS] ±ZZZZ] =>  H:MM[:SS]
//! ```
//!
//! Mandatory invariants (see `doc/plan/05-clock-subcommand.md`):
//!
//! 1. Numerical timezone (`±HHMM`) on both timestamps.
//! 2. Both timestamps share the same precision (`HH:MM` or
//!    `HH:MM:SS`); no mixing.
//! 3. The weekday token agrees with the date.
//! 4. Start ≤ End.
//! 5. The line parses as a closed range with the duration suffix
//!    in shape (`=> …`). Running clocks are not in scope.
//!
//! [`parse_clock_line`] returns `Ok(None)` when the line does not
//! look like a CLOCK closed range at all (caller should ignore),
//! `Ok(Some(ClockLine))` when it is fully valid, and
//! `Err(ClockError)` when it *is* a closed-range CLOCK line but
//! violates one of the invariants above.

use std::fmt;
use std::sync::OnceLock;

use chrono::{Datelike, NaiveDate, Weekday};
use regex::Regex;

/// One successfully-parsed closed-range CLOCK line.
///
/// "Parsed" only means the structural shape is recoverable
/// (`[…]--[…] => …` with valid date/time/numerical-TZ on both
/// sides). The line may still carry semantic violations; see
/// [`ParsedClockLine`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockLine {
    pub start: Timestamp,
    pub end: Timestamp,
    /// Whether both timestamps carry seconds.
    pub has_seconds: bool,
}

/// A parsed CLOCK line plus the list of semantic violations
/// observed on it (possibly empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedClockLine {
    pub line: ClockLine,
    /// The exact suffix the line carries, starting at the `=>`
    /// arrow and ending at end-of-line (trailing whitespace
    /// stripped). Used to assess [`ClockViolation::SpacingMismatch`]
    /// and [`ClockViolation::DurationMismatch`].
    pub raw_duration_suffix: String,
    pub violations: Vec<ClockViolation>,
}

/// One bracketed `[YYYY-MM-DD Day HH:MM[:SS] ±ZZZZ]` instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timestamp {
    pub date: NaiveDate,
    pub weekday_token: String,
    pub hour: u32,
    pub minute: u32,
    pub second: Option<u32>,
    /// Numerical offset from UTC in minutes (signed).
    pub tz_minutes: i32,
}

/// Structural error: the line cannot be parsed into a
/// [`ClockLine`] at all. Once any of these is hit, no semantic
/// invariants can be checked because the timestamps themselves
/// are not recoverable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClockError {
    /// One of the bracketed timestamps has no `±HHMM` suffix.
    MissingTz {
        /// `"start"` or `"end"`.
        side: &'static str,
    },
    /// A timezone is present but not numerical (e.g. `CEST`).
    NonNumericalTz { side: &'static str, token: String },
    /// Looks like a closed-range CLOCK but cannot be parsed.
    Unparseable { reason: String },
}

impl ClockError {
    /// A short stable tag for machine-readable output.
    pub fn tag(&self) -> &'static str {
        match self {
            ClockError::MissingTz { .. } => "missing-tz",
            ClockError::NonNumericalTz { .. } => "non-numerical-tz",
            ClockError::Unparseable { .. } => "unparseable",
        }
    }

    /// Human-readable message.
    pub fn message(&self) -> String {
        match self {
            ClockError::MissingTz { side } => {
                format!("{} timestamp has no numerical timezone (±HHMM)", side)
            }
            ClockError::NonNumericalTz { side, token } => {
                format!(
                    "{} timestamp has non-numerical timezone {:?} (use ±HHMM)",
                    side, token
                )
            }
            ClockError::Unparseable { reason } => format!("unparseable CLOCK line: {}", reason),
        }
    }

    /// Structural errors are never auto-fixable.
    pub fn is_fixable(&self) -> bool {
        false
    }
}

impl fmt::Display for ClockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ClockError {}

/// Semantic violation: the line parses cleanly into a
/// [`ClockLine`] but breaks one of the canonical invariants.
/// A line may carry several of these at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClockViolation {
    /// Weekday token disagrees with the date.
    WeekdayMismatch {
        side: &'static str,
        date: NaiveDate,
        token: String,
        expected: String,
    },
    /// End instant strictly precedes start instant (UTC-wise).
    StartAfterEnd,
    /// One side has seconds, the other does not.
    MixedPrecision,
    /// Start and end carry different numerical timezones.
    TzMismatch {
        start_tz_minutes: i32,
        end_tz_minutes: i32,
    },
    /// The `=> H:MM[:SS]` suffix's numeric value disagrees with
    /// the (start,end) instants.
    DurationMismatch { found: String, expected: String },
    /// The spacing of the `=> H:MM[:SS]` suffix doesn't match
    /// Emacs' canonical `printf("=> %2d:%02d")` form.
    SpacingMismatch { found: String, expected: String },
}

impl ClockViolation {
    pub fn tag(&self) -> &'static str {
        match self {
            ClockViolation::WeekdayMismatch { .. } => "weekday-mismatch",
            ClockViolation::StartAfterEnd => "start-after-end",
            ClockViolation::MixedPrecision => "mixed-precision",
            ClockViolation::TzMismatch { .. } => "tz-mismatch",
            ClockViolation::DurationMismatch { .. } => "duration-mismatch",
            ClockViolation::SpacingMismatch { .. } => "spacing-mismatch",
        }
    }

    pub fn message(&self) -> String {
        match self {
            ClockViolation::WeekdayMismatch {
                side,
                date,
                token,
                expected,
            } => format!(
                "{} weekday token {:?} disagrees with {} (expected {:?})",
                side, token, date, expected
            ),
            ClockViolation::StartAfterEnd => "end instant precedes start instant".into(),
            ClockViolation::MixedPrecision => {
                "start and end use different precision (one HH:MM, the other HH:MM:SS)".into()
            }
            ClockViolation::TzMismatch {
                start_tz_minutes,
                end_tz_minutes,
            } => format!(
                "start and end timezones differ ({} vs {})",
                format_tz_signed(*start_tz_minutes),
                format_tz_signed(*end_tz_minutes),
            ),
            ClockViolation::DurationMismatch { found, expected } => format!(
                "duration suffix {:?} disagrees with timestamps (expected {:?})",
                found, expected
            ),
            ClockViolation::SpacingMismatch { found, expected } => format!(
                "duration suffix has non-canonical spacing {:?} (expected {:?})",
                found, expected
            ),
        }
    }

    /// Whether [`recompute_duration_in_line`] /
    /// [`fix_weekday_in_line`] can heal this violation safely.
    /// The fixer must refuse to mutate any line that carries a
    /// non-fixable violation, even if other (fixable) violations
    /// are also present — because we cannot tell which axis
    /// carries the truth.
    pub fn is_fixable(&self) -> bool {
        matches!(
            self,
            ClockViolation::WeekdayMismatch { .. }
                | ClockViolation::DurationMismatch { .. }
                | ClockViolation::SpacingMismatch { .. }
        )
    }
}

impl fmt::Display for ClockViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

fn format_tz_signed(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let abs = minutes.abs();
    format!("{}{:02}{:02}", sign, abs / 60, abs % 60)
}

/// Parse one line.
///
/// * `Ok(None)` — the line is not a closed-range CLOCK (running
///   clock, comment, body text mentioning `CLOCK:`, …); the
///   caller should ignore it silently.
/// * `Ok(Some(parsed))` — the line *is* a closed-range CLOCK and
///   its structural shape is recoverable. `parsed.violations`
///   may still be non-empty: any subset of
///   `weekday-mismatch / mixed-precision / tz-mismatch /
///   start-after-end / duration-mismatch / spacing-mismatch`.
/// * `Err(_)` — the line cannot be parsed structurally
///   (`missing-tz`, `non-numerical-tz`, `unparseable`). No
///   timestamps are recoverable, so no semantic violations can
///   be reported on the same line.
pub fn parse_clock_line(line: &str) -> Result<Option<ParsedClockLine>, ClockError> {
    // Find the `CLOCK:` prefix, accepting arbitrary leading
    // whitespace (LOGBOOK indentation varies in real files).
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix("CLOCK:") else {
        return Ok(None);
    };
    let rest = rest.trim_start();

    // Closed-range silhouette: must start with `[`.
    if !rest.starts_with('[') {
        return Ok(None);
    }

    // Match `[…]--[…] => …`. Anything missing the duration arrow
    // doesn't qualify as a closed range we should validate.
    let (start_inner, after_start) = bracketed(rest).ok_or(ClockError::Unparseable {
        reason: "missing closing bracket on start timestamp".into(),
    })?;
    // No `--` after the first bracket → running clock.
    let Some(after_dashes) = after_start.strip_prefix("--") else {
        return Ok(None);
    };
    let (end_inner, after_end) = bracketed(after_dashes).ok_or(ClockError::Unparseable {
        reason: "missing closing bracket on end timestamp".into(),
    })?;
    let after_end_trim = after_end.trim_start();
    if !after_end_trim.starts_with("=>") {
        return Err(ClockError::Unparseable {
            reason: "missing `=>` duration arrow".into(),
        });
    }

    // Capture the raw `=> …` suffix exactly as it appears so we
    // can report SpacingMismatch / DurationMismatch precisely.
    let raw_duration_suffix = line
        .rfind("=>")
        .map(|i| line[i..].trim_end().to_string())
        .unwrap_or_default();

    // Structural parse of both timestamps. Failures here are
    // `ClockError` (return Err and bail; semantic checks impossible).
    let (start, start_extras) = parse_timestamp(start_inner, "start")?;
    let (end, end_extras) = parse_timestamp(end_inner, "end")?;

    // Collect semantic violations.
    let mut violations: Vec<ClockViolation> = Vec::new();
    violations.extend(start_extras);
    violations.extend(end_extras);

    // Mixed precision.
    if start.second.is_some() != end.second.is_some() {
        violations.push(ClockViolation::MixedPrecision);
    }

    // Mismatched timezones.
    if start.tz_minutes != end.tz_minutes {
        violations.push(ClockViolation::TzMismatch {
            start_tz_minutes: start.tz_minutes,
            end_tz_minutes: end.tz_minutes,
        });
    }

    // Start after end (UTC-wise).
    if cmp_instants(&start, &end) == std::cmp::Ordering::Greater {
        violations.push(ClockViolation::StartAfterEnd);
    }

    let clock = ClockLine {
        has_seconds: start.second.is_some() && end.second.is_some(),
        start,
        end,
    };

    // Duration / spacing checks — only meaningful if start ≤ end
    // and the precisions match (otherwise the canonical suffix
    // is undefined).
    let can_check_duration = !violations.iter().any(|v| {
        matches!(
            v,
            ClockViolation::StartAfterEnd | ClockViolation::MixedPrecision
        )
    });
    if can_check_duration {
        let canonical = canonical_duration_suffix(&clock);
        if raw_duration_suffix != canonical {
            // Distinguish a pure-spacing diff from a numeric-value
            // diff. Strip whitespace inside the suffix and compare
            // the digit pattern: if the digits match, only the
            // spacing differs.
            let normalise_spacing =
                |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
            if normalise_spacing(&raw_duration_suffix) == normalise_spacing(&canonical) {
                violations.push(ClockViolation::SpacingMismatch {
                    found: raw_duration_suffix.clone(),
                    expected: canonical,
                });
            } else {
                violations.push(ClockViolation::DurationMismatch {
                    found: raw_duration_suffix.clone(),
                    expected: canonical,
                });
            }
        }
    }

    Ok(Some(ParsedClockLine {
        line: clock,
        raw_duration_suffix,
        violations,
    }))
}

/// Find the closing `]` matching a leading `[`. Returns `(inner,
/// rest_after_close)`.
fn bracketed(s: &str) -> Option<(&str, &str)> {
    let s = s.strip_prefix('[')?;
    let close = s.find(']')?;
    Some((&s[..close], &s[close + 1..]))
}

/// Parse one bracketed `YYYY-MM-DD Day HH:MM[:SS] ±ZZZZ` token.
///
/// Returns the timestamp plus any *semantic* violations
/// observed on this side (currently: `WeekdayMismatch`).
/// `side` is `"start"` or `"end"` for error reporting.
fn parse_timestamp(
    inner: &str,
    side: &'static str,
) -> Result<(Timestamp, Vec<ClockViolation>), ClockError> {
    let parts: Vec<&str> = inner.split_whitespace().collect();
    // 4 parts is the canonical shape: DATE DAY TIME TZ.
    // 3 parts means the TZ is missing. Anything else is unparseable.
    let (date_s, day_s, time_s, tz_opt) = match parts.as_slice() {
        [d, day, t, tz] => (*d, *day, *t, Some(*tz)),
        [d, day, t] => (*d, *day, *t, None),
        _ => {
            return Err(ClockError::Unparseable {
                reason: format!(
                    "{} timestamp has {} space-separated tokens, expected 3 or 4",
                    side,
                    parts.len()
                ),
            });
        }
    };

    let date = parse_date(date_s).map_err(|e| ClockError::Unparseable {
        reason: format!("{} date: {}", side, e),
    })?;

    let (hour, minute, second) = parse_time(time_s).map_err(|e| ClockError::Unparseable {
        reason: format!("{} time: {}", side, e),
    })?;

    // Timezone handling — distinguish "missing entirely" (3 tokens)
    // from "present but non-numerical" (4 tokens, garbage).
    let tz_minutes = match tz_opt {
        None => return Err(ClockError::MissingTz { side }),
        Some(tok) => match parse_tz(tok) {
            Some(m) => m,
            None => {
                return Err(ClockError::NonNumericalTz {
                    side,
                    token: tok.to_string(),
                });
            }
        },
    };

    // Weekday agreement — reported as a semantic violation, not a
    // structural failure, so the rest of the line still gets
    // analysed.
    let mut violations = Vec::new();
    let expected = weekday_short(date.weekday());
    if day_s != expected {
        violations.push(ClockViolation::WeekdayMismatch {
            side,
            date,
            token: day_s.to_string(),
            expected: expected.to_string(),
        });
    }

    Ok((
        Timestamp {
            date,
            weekday_token: day_s.to_string(),
            hour,
            minute,
            second,
            tz_minutes,
        },
        violations,
    ))
}

fn parse_date(s: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|e| e.to_string())
}

/// Parse `HH:MM` or `HH:MM:SS`. Returns `(hour, minute, second_opt)`.
fn parse_time(s: &str) -> Result<(u32, u32, Option<u32>), String> {
    let parts: Vec<&str> = s.split(':').collect();
    let parse_u = |p: &str, what: &str| -> Result<u32, String> {
        p.parse::<u32>()
            .map_err(|e| format!("invalid {} {:?}: {}", what, p, e))
    };
    match parts.as_slice() {
        [h, m] => {
            let h = parse_u(h, "hour")?;
            let m = parse_u(m, "minute")?;
            range_check(h, 0, 23, "hour")?;
            range_check(m, 0, 59, "minute")?;
            Ok((h, m, None))
        }
        [h, m, sec] => {
            let h = parse_u(h, "hour")?;
            let m = parse_u(m, "minute")?;
            let sec = parse_u(sec, "second")?;
            range_check(h, 0, 23, "hour")?;
            range_check(m, 0, 59, "minute")?;
            range_check(sec, 0, 59, "second")?;
            Ok((h, m, Some(sec)))
        }
        _ => Err(format!("expected HH:MM or HH:MM:SS, got {:?}", s)),
    }
}

fn range_check(v: u32, lo: u32, hi: u32, what: &str) -> Result<(), String> {
    if v < lo || v > hi {
        Err(format!("{} {} out of range {}..={}", what, v, lo, hi))
    } else {
        Ok(())
    }
}

/// Parse `+HHMM` / `-HHMM`. Returns total signed minutes, or `None`
/// if the token does not match that shape.
fn parse_tz(s: &str) -> Option<i32> {
    if s.len() != 5 {
        return None;
    }
    let sign = match s.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let hh: i32 = s[1..3].parse().ok()?;
    let mm: i32 = s[3..5].parse().ok()?;
    if !(0..=23).contains(&hh) || !(0..=59).contains(&mm) {
        return None;
    }
    Some(sign * (hh * 60 + mm))
}

/// Three-letter English weekday short name as org-mode emits.
pub fn weekday_short(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

/// Result of [`fix_weekday_in_line`]: a mutated line plus the list
/// of rewrites that took place. Empty `rewrites` means the input
/// is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeekdayFix {
    /// The (possibly mutated) line.
    pub line: String,
    /// `(old_token, new_token, date)` tuples, in order of
    /// occurrence. Empty when the line was already correct.
    pub rewrites: Vec<WeekdayRewrite>,
}

/// One token-replacement record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeekdayRewrite {
    pub date: NaiveDate,
    pub old: String,
    pub new: &'static str,
}

/// On a line that looks like a closed-range CLOCK, normalise both
/// `[YYYY-MM-DD Day ...]` weekday tokens to the English
/// three-letter abbreviation that matches the date.
///
/// **Strict gate:** the line is only rewritten when it parses
/// structurally AND every other violation it carries is itself
/// auto-fixable. A line with `tz-mismatch`, `start-after-end`,
/// `mixed-precision`, missing-tz, etc. is left untouched — even
/// if its weekday is wrong — because the line as a whole is
/// suspect and we cannot guarantee the date is the canonical
/// axis.
///
/// Lines that don't match the closed-range silhouette are left
/// alone (returned as-is with empty `rewrites`).
pub fn fix_weekday_in_line(line: &str) -> WeekdayFix {
    if !is_closed_range_clock(line) {
        return WeekdayFix {
            line: line.to_string(),
            rewrites: Vec::new(),
        };
    }
    // Strict gate: refuse to touch a line that has any
    // non-fixable violation, or that fails structural parse.
    match parse_clock_line(line) {
        Ok(Some(p)) if !p.violations.iter().any(|v| !v.is_fixable()) => {}
        _ => {
            return WeekdayFix {
                line: line.to_string(),
                rewrites: Vec::new(),
            };
        }
    }
    let re = closed_range_token_regex();
    let mut rewrites = Vec::new();
    let new_line = re
        .replace_all(line, |caps: &regex::Captures<'_>| {
            let date_s = &caps[1];
            let day_s = &caps[2];
            match NaiveDate::parse_from_str(date_s, "%Y-%m-%d") {
                Ok(d) => {
                    let expected = weekday_short(d.weekday());
                    if day_s != expected {
                        rewrites.push(WeekdayRewrite {
                            date: d,
                            old: day_s.to_string(),
                            new: expected,
                        });
                        format!("[{} {}", date_s, expected)
                    } else {
                        caps[0].to_string()
                    }
                }
                Err(_) => caps[0].to_string(),
            }
        })
        .into_owned();
    WeekdayFix {
        line: new_line,
        rewrites,
    }
}

/// True when `line` looks like a closed-range CLOCK (`CLOCK:` +
/// `[…]--[…]` silhouette + `=>` arrow). Used as a cheap pre-filter
/// so we don't run the rewrite regex on every line of every file.
fn is_closed_range_clock(line: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^\s*CLOCK:\s*\[[^\]]+\]--\[[^\]]+\]\s*=>").unwrap());
    re.is_match(line)
}

/// Result of [`recompute_duration_in_line`]: a mutated line plus
/// the (old, new) duration suffix when something changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurationFix {
    pub line: String,
    pub change: Option<DurationChange>,
}

/// One duration-suffix replacement record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurationChange {
    /// The full old suffix as it appeared on the line, starting
    /// at the `=>` arrow (e.g. `"=>  720:45"`). Trailing
    /// whitespace stripped.
    pub old: String,
    /// The replacement suffix (e.g. `"=>  0:45"`).
    pub new: String,
}

/// On a line that looks like a closed-range CLOCK and parses
/// successfully, recompute the `=> H:MM[:SS]` duration suffix
/// from the bracketed start/end timestamps, applying the
/// canonical Emacs spacing rule (`printf("=> %2d:%02d")`: a
/// leading space pads single-digit hours so the hour field is
/// always at least two characters wide, hence `=>  2:15` but
/// `=> 12:30`).
///
/// **Strict gate:** the line is only rewritten when *every*
/// remaining violation it carries is itself auto-fixable
/// ([`ClockViolation::is_fixable`]). A line that also has
/// `tz-mismatch`, `start-after-end`, `mixed-precision`, or any
/// structural error is left untouched — because we cannot tell
/// which axis carries the truth on a line that is otherwise
/// suspect.
pub fn recompute_duration_in_line(line: &str) -> DurationFix {
    if !is_closed_range_clock(line) {
        return DurationFix {
            line: line.to_string(),
            change: None,
        };
    }
    let parsed = match parse_clock_line(line) {
        Ok(Some(p)) => p,
        _ => {
            return DurationFix {
                line: line.to_string(),
                change: None,
            };
        }
    };
    if parsed.violations.iter().any(|v| !v.is_fixable()) {
        return DurationFix {
            line: line.to_string(),
            change: None,
        };
    }
    let new_suffix = canonical_duration_suffix(&parsed.line);

    let arrow = match line.rfind("=>") {
        Some(i) => i,
        None => {
            return DurationFix {
                line: line.to_string(),
                change: None,
            };
        }
    };
    let head = &line[..arrow];
    let old_suffix = line[arrow..].trim_end().to_string();
    if old_suffix == new_suffix {
        return DurationFix {
            line: line.to_string(),
            change: None,
        };
    }
    let new_line = format!("{}{}", head, new_suffix);
    DurationFix {
        line: new_line,
        change: Some(DurationChange {
            old: old_suffix,
            new: new_suffix,
        }),
    }
}

/// Build the canonical `"=> H:MM"` / `"=> H:MM:SS"` suffix for a
/// parsed CLOCK line, applying the Emacs spacing rule.
pub fn canonical_duration_suffix(c: &ClockLine) -> String {
    let secs = duration_seconds(c).max(0);
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if c.has_seconds {
        format!("=> {:>2}:{:02}:{:02}", h, m, s)
    } else {
        format!("=> {:>2}:{:02}", h, m)
    }
}

/// `end - start` in whole seconds, taking each side's timezone
/// into account.
fn duration_seconds(c: &ClockLine) -> i64 {
    utc_seconds(&c.end) - utc_seconds(&c.start)
}

/// Convert a [`Timestamp`] to seconds since the chrono epoch in
/// UTC. Used for ordering and duration computation.
fn utc_seconds(t: &Timestamp) -> i64 {
    let local_min =
        t.date.num_days_from_ce() as i64 * 24 * 60 + t.hour as i64 * 60 + t.minute as i64;
    local_min * 60 + t.second.unwrap_or(0) as i64 - t.tz_minutes as i64 * 60
}

/// Regex that captures `[YYYY-MM-DD Day` (without the trailing
/// space) inside a CLOCK timestamp, with `Day` being the longest
/// non-whitespace token after the date. Both the start and end
/// timestamps are matched.
fn closed_range_token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[(\d{4}-\d{2}-\d{2}) (\S+)").unwrap())
}

/// Compare two timestamps as wall-clock instants in UTC.
fn cmp_instants(a: &Timestamp, b: &Timestamp) -> std::cmp::Ordering {
    utc_seconds(a).cmp(&utc_seconds(b))
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    // ----- happy path ----------------------------------------------------

    /// Helper: assert no violations on a parse.
    fn parsed_clean(line: &str) -> ParsedClockLine {
        let p = parse_clock_line(line).unwrap().expect("closed-range");
        assert!(
            p.violations.is_empty(),
            "unexpected violations on line {:?}: {:?}",
            line,
            p.violations
        );
        p
    }

    /// Helper: collect violation tags.
    fn tags_of(line: &str) -> Vec<&'static str> {
        let p = parse_clock_line(line).unwrap().expect("closed-range");
        p.violations.iter().map(|v| v.tag()).collect()
    }

    #[test]
    fn parses_canonical_minute_precision() {
        let line = "  CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15";
        let parsed = parsed_clean(line);
        assert!(!parsed.line.has_seconds);
        assert_eq!(parsed.line.start.tz_minutes, 120);
        assert_eq!(parsed.line.end.tz_minutes, 120);
        assert_eq!(parsed.line.start.hour, 9);
        assert_eq!(parsed.line.end.hour, 11);
    }

    #[test]
    fn parses_canonical_second_precision() {
        let line =
            "CLOCK: [2026-01-07 Wed 09:57:20 +0100]--[2026-01-07 Wed 12:19:25 +0100] =>  2:22:05";
        let parsed = parsed_clean(line);
        assert!(parsed.line.has_seconds);
        assert_eq!(parsed.line.start.second, Some(20));
        assert_eq!(parsed.line.end.second, Some(25));
    }

    #[test]
    fn parses_negative_tz() {
        let line = "CLOCK: [2025-12-15 Mon 09:00 -0500]--[2025-12-15 Mon 10:00 -0500] =>  1:00";
        let parsed = parsed_clean(line);
        assert_eq!(parsed.line.start.tz_minutes, -300);
    }

    // ----- non-clock lines are silently skipped --------------------------

    #[test]
    fn ignores_running_clock() {
        let line = "  CLOCK: [2025-06-06 Fri 09:30 +0200]";
        assert_eq!(parse_clock_line(line).unwrap(), None);
    }

    #[test]
    fn ignores_unrelated_line() {
        for s in [
            "* heading",
            "Some prose mentioning CLOCK: in passing.",
            "",
            "   ",
            ":LOGBOOK:",
        ] {
            assert_eq!(parse_clock_line(s).unwrap(), None, "input {:?}", s);
        }
    }

    // ----- structural errors --------------------------------------------

    #[test]
    fn rejects_missing_tz_on_start() {
        let line = "CLOCK: [2026-01-07 Wed 09:57:20]--[2026-01-07 Wed 12:19:25 +0100] =>  2:22:05";
        let err = parse_clock_line(line).unwrap_err();
        assert_eq!(err.tag(), "missing-tz");
        match err {
            ClockError::MissingTz { side } => assert_eq!(side, "start"),
            other => panic!("wrong err: {:?}", other),
        }
    }

    #[test]
    fn rejects_missing_tz_on_end() {
        let line = "CLOCK: [2026-01-07 Wed 09:57:20 +0100]--[2026-01-07 Wed 12:19:25] =>  2:22:05";
        let err = parse_clock_line(line).unwrap_err();
        match err {
            ClockError::MissingTz { side } => assert_eq!(side, "end"),
            other => panic!("wrong err: {:?}", other),
        }
    }

    #[test]
    fn rejects_named_tz() {
        let line = "CLOCK: [2025-06-06 Fri 09:30 CEST]--[2025-06-06 Fri 11:45 CEST] =>  2:15";
        let err = parse_clock_line(line).unwrap_err();
        assert_eq!(err.tag(), "non-numerical-tz");
    }

    #[test]
    fn rejects_unparseable() {
        for line in [
            "CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200]",
            "CLOCK: [2025-13-40 Fri 09:30 +0200]--[2025-13-40 Fri 11:45 +0200] =>  0:00",
            "CLOCK: [2025-06-06 Fri xx:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15",
        ] {
            let r = parse_clock_line(line);
            assert!(
                matches!(r, Err(ClockError::Unparseable { .. })),
                "expected unparseable for {:?}, got {:?}",
                line,
                r
            );
        }
    }

    // ----- semantic violations -----------------------------------------

    #[test]
    fn flags_mixed_precision_as_violation() {
        let line = "CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45:30 +0200] =>  2:15";
        let tags = tags_of(line);
        assert!(tags.contains(&"mixed-precision"), "got {:?}", tags);
    }

    #[test]
    fn flags_weekday_mismatch_as_violation() {
        // 2025-06-06 was a Friday, not Monday.
        let line = "CLOCK: [2025-06-06 Mon 09:30 +0200]--[2025-06-06 Mon 11:45 +0200] =>  2:15";
        let tags = tags_of(line);
        // Both sides wrong → two weekday-mismatch entries.
        assert_eq!(
            tags.iter().filter(|t| **t == "weekday-mismatch").count(),
            2,
            "got {:?}",
            tags
        );
    }

    #[test]
    fn flags_start_after_end_as_violation() {
        let line = "CLOCK: [2025-06-06 Fri 12:00 +0200]--[2025-06-06 Fri 11:00 +0200] =>  -1:00";
        let tags = tags_of(line);
        assert!(tags.contains(&"start-after-end"), "got {:?}", tags);
    }

    #[test]
    fn flags_tz_mismatch_as_violation() {
        // The real-life case.
        let line =
            "     CLOCK: [2016-08-10 Wed 12:30 +0800]--[2016-08-10 Wed 12:45 +0200] =>  0:15";
        let tags = tags_of(line);
        assert!(tags.contains(&"tz-mismatch"), "got {:?}", tags);
    }

    #[test]
    fn flags_duration_mismatch_as_violation() {
        // 09:30..11:45 is 2:15, not 2:30.
        let line = "CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:30";
        let tags = tags_of(line);
        assert!(tags.contains(&"duration-mismatch"), "got {:?}", tags);
    }

    #[test]
    fn flags_spacing_mismatch_as_violation() {
        // Single space + 1-digit hour: should be 2 spaces.
        let line = "CLOCK: [2022-06-03 Fri 15:00 +0200]--[2022-06-03 Fri 17:30 +0200] => 2:30";
        let tags = tags_of(line);
        assert!(tags.contains(&"spacing-mismatch"), "got {:?}", tags);
        // Spacing-only diff should NOT be reported as duration-mismatch.
        assert!(!tags.contains(&"duration-mismatch"), "got {:?}", tags);
    }

    #[test]
    fn two_digit_hour_uses_single_space() {
        // 12:30 duration is canonical with 1 space.
        let line = "CLOCK: [2026-04-28 Tue 02:00 +0200]--[2026-04-28 Tue 14:30 +0200] => 12:30";
        let p = parsed_clean(line);
        assert!(p.violations.is_empty(), "got {:?}", p.violations);
    }

    #[test]
    fn equal_start_and_end_is_clean() {
        let line = "CLOCK: [2025-06-06 Fri 12:00 +0200]--[2025-06-06 Fri 12:00 +0200] =>  0:00";
        parsed_clean(line);
    }

    #[test]
    fn cross_tz_ordering_uses_utc() {
        // 09:00 +0200 = 07:00 UTC; 08:00 +0000 = 08:00 UTC.
        // End > Start when TZs are honoured. Duration suffix matches.
        let line = "CLOCK: [2025-06-06 Fri 09:00 +0200]--[2025-06-06 Fri 08:00 +0000] =>  1:00";
        let tags = tags_of(line);
        // tz-mismatch is now a violation, but no start-after-end
        // (because UTC-wise the end is after the start).
        assert!(tags.contains(&"tz-mismatch"), "got {:?}", tags);
        assert!(!tags.contains(&"start-after-end"), "got {:?}", tags);
    }

    // ----- weekday rewriter --------------------------------------------

    #[test]
    fn fix_weekday_noop_on_clean_line() {
        let line = "  CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15";
        let r = fix_weekday_in_line(line);
        assert_eq!(r.line, line);
        assert!(r.rewrites.is_empty());
    }

    #[test]
    fn fix_weekday_corrects_both_sides_when_token_wrong() {
        // 2024-04-16 was a Tuesday, not a Monday.
        let line = "  CLOCK: [2024-04-16 Mon 14:15 +0200]--[2024-04-16 Mon 14:30 +0200] =>  0:15";
        let r = fix_weekday_in_line(line);
        assert_eq!(
            r.line,
            "  CLOCK: [2024-04-16 Tue 14:15 +0200]--[2024-04-16 Tue 14:30 +0200] =>  0:15"
        );
        assert_eq!(r.rewrites.len(), 2);
        assert_eq!(r.rewrites[0].old, "Mon");
        assert_eq!(r.rewrites[0].new, "Tue");
    }

    #[test]
    fn fix_weekday_normalises_french_to_english() {
        // 2022-05-25 was a Wednesday.
        let line = "   CLOCK: [2022-05-25 mer. 08:15 +0200]--[2022-05-25 Wed 10:30 +0200] =>  2:15";
        let r = fix_weekday_in_line(line);
        assert_eq!(
            r.line,
            "   CLOCK: [2022-05-25 Wed 08:15 +0200]--[2022-05-25 Wed 10:30 +0200] =>  2:15"
        );
        // Only the start side rewrote; end was already "Wed".
        assert_eq!(r.rewrites.len(), 1);
        assert_eq!(r.rewrites[0].old, "mer.");
        assert_eq!(r.rewrites[0].new, "Wed");
    }

    #[test]
    fn fix_weekday_only_one_side_wrong() {
        // 2024-02-19 was a Monday. Start ok, end says Tue.
        let line = "CLOCK: [2024-02-19 Mon 14:00 +0100]--[2024-02-19 Tue 16:00 +0100] =>  2:00";
        let r = fix_weekday_in_line(line);
        assert_eq!(
            r.line,
            "CLOCK: [2024-02-19 Mon 14:00 +0100]--[2024-02-19 Mon 16:00 +0100] =>  2:00"
        );
        assert_eq!(r.rewrites.len(), 1);
        assert_eq!(r.rewrites[0].old, "Tue");
    }

    #[test]
    fn fix_weekday_refuses_missing_tz_line() {
        // Strict gate: missing-tz makes the line untouchable.
        let line = "  CLOCK: [2026-01-07 Tue 09:57:20]--[2026-01-07 Tue 12:19:25] =>  2:22:05";
        let r = fix_weekday_in_line(line);
        assert_eq!(r.line, line);
        assert!(r.rewrites.is_empty());
    }

    #[test]
    fn fix_weekday_refuses_tz_mismatch_line() {
        // The real-life case: weekday axis would be "correct"
        // here, but tz-mismatch makes the whole line suspect.
        let line =
            "     CLOCK: [2016-08-10 Wed 12:30 +0800]--[2016-08-10 Wed 12:45 +0200] =>  0:15";
        let r = fix_weekday_in_line(line);
        assert_eq!(r.line, line);
        assert!(r.rewrites.is_empty());
    }

    #[test]
    fn duration_refuses_tz_mismatch_line() {
        // The actual real-life regression test: previously the
        // fixer would silently rewrite the duration to the
        // UTC-aware computation. With the strict gate, no.
        let line =
            "     CLOCK: [2016-08-10 Wed 12:30 +0800]--[2016-08-10 Wed 12:45 +0200] =>  0:15";
        let r = recompute_duration_in_line(line);
        assert_eq!(r.line, line);
        assert!(r.change.is_none());
    }

    #[test]
    fn fix_weekday_skips_non_clock_lines() {
        for line in [
            "* heading [2024-04-16 Mon] something",
            "prose mentioning [2024-04-16 Mon] in passing",
            "  CLOCK: [2024-04-16 Mon 14:15 +0200]", // running clock
            "",
        ] {
            let r = fix_weekday_in_line(line);
            assert_eq!(r.line, line, "input {:?}", line);
            assert!(r.rewrites.is_empty(), "input {:?}", line);
        }
    }

    #[test]
    fn fix_weekday_is_idempotent() {
        let line = "  CLOCK: [2024-04-16 Mon 14:15 +0200]--[2024-04-16 Mon 14:30 +0200] =>  0:15";
        let once = fix_weekday_in_line(line);
        let twice = fix_weekday_in_line(&once.line);
        assert_eq!(twice.line, once.line);
        assert!(twice.rewrites.is_empty());
    }

    // ----- duration recomputer -----------------------------------------

    #[test]
    fn duration_noop_on_correct_line() {
        let line = "  CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15";
        let r = recompute_duration_in_line(line);
        assert_eq!(r.line, line);
        assert!(r.change.is_none());
    }

    #[test]
    fn duration_recomputed_when_corrupted() {
        // 2024-11-13 15:00..15:45 = 0:45, but the line says 720:45.
        let line = "  CLOCK: [2024-11-13 Wed 15:00 +0100]--[2024-11-13 Wed 15:45 +0100] => 720:45";
        let r = recompute_duration_in_line(line);
        assert_eq!(
            r.line,
            "  CLOCK: [2024-11-13 Wed 15:00 +0100]--[2024-11-13 Wed 15:45 +0100] =>  0:45"
        );
        let ch = r.change.unwrap();
        assert_eq!(ch.old, "=> 720:45");
        assert_eq!(ch.new, "=>  0:45");
    }

    #[test]
    fn duration_normalises_emacs_spacing() {
        // Single-digit hour written without the padding space
        // ("=> 2:30" instead of "=>  2:30") gets re-padded.
        let line = "  CLOCK: [2022-06-03 Fri 15:00 +0200]--[2022-06-03 Fri 17:30 +0200] => 2:30";
        let r = recompute_duration_in_line(line);
        assert_eq!(
            r.line,
            "  CLOCK: [2022-06-03 Fri 15:00 +0200]--[2022-06-03 Fri 17:30 +0200] =>  2:30"
        );
    }

    #[test]
    fn duration_keeps_two_digit_hour_with_single_space() {
        // 12-hour duration written as "=> 12:30" is canonical;
        // must remain unchanged.
        let line = "  CLOCK: [2026-04-28 Tue 02:00 +0200]--[2026-04-28 Tue 14:30 +0200] => 12:30";
        let r = recompute_duration_in_line(line);
        assert_eq!(r.line, line);
        assert!(r.change.is_none());
    }

    #[test]
    fn duration_handles_seconds_precision() {
        let line =
            "  CLOCK: [2026-01-07 Wed 09:57:20 +0100]--[2026-01-07 Wed 12:19:25 +0100] => bogus";
        let r = recompute_duration_in_line(line);
        assert_eq!(
            r.line,
            "  CLOCK: [2026-01-07 Wed 09:57:20 +0100]--[2026-01-07 Wed 12:19:25 +0100] =>  2:22:05"
        );
    }

    #[test]
    fn duration_skips_unparseable_lines() {
        // Missing TZ → don't touch.
        let line = "  CLOCK: [2026-01-07 Wed 09:57]--[2026-01-07 Wed 12:19] =>  2:22";
        let r = recompute_duration_in_line(line);
        assert_eq!(r.line, line);
        assert!(r.change.is_none());
    }

    #[test]
    fn duration_idempotent() {
        let line = "  CLOCK: [2024-11-13 Wed 15:00 +0100]--[2024-11-13 Wed 15:45 +0100] => 720:45";
        let once = recompute_duration_in_line(line);
        let twice = recompute_duration_in_line(&once.line);
        assert_eq!(twice.line, once.line);
        assert!(twice.change.is_none());
    }

    #[test]
    fn fix_weekday_leaves_unparseable_date_alone() {
        let line = "  CLOCK: [2025-13-40 Foo 09:30 +0200]--[2025-13-40 Foo 11:45 +0200] =>  2:15";
        let r = fix_weekday_in_line(line);
        assert_eq!(r.line, line);
        assert!(r.rewrites.is_empty());
    }

    #[test]
    fn looks_like_clock_but_truncated_is_treated_as_running() {
        // No `--` and no second bracket → silently skipped.
        for line in ["CLOCK: [garbage", "CLOCK: [2025-06-06 Fri 09:30 +0200]"] {
            let r = parse_clock_line(line);
            assert!(
                matches!(r, Ok(None)) || matches!(r, Err(ClockError::Unparseable { .. })),
                "input {:?} produced {:?}",
                line,
                r
            );
        }
    }

    // ----- multi-line walker -------------------------------------------

    /// Smoke test: the public parse function operates on one line; a
    /// caller iterating over a file's lines can collect violations.
    #[test]
    fn walks_a_logbook() {
        let content = indoc! {"
            **** Some heading
              :LOGBOOK:
              CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15
              CLOCK: [2026-01-07 Wed 09:57:20]--[2026-01-07 Wed 12:19:25] =>  2:22:05
              :END:
        "};
        let mut ok = 0usize;
        let mut errs = 0usize;
        for line in content.lines() {
            match parse_clock_line(line) {
                Ok(Some(_)) => ok += 1,
                Ok(None) => {}
                Err(_) => errs += 1,
            }
        }
        assert_eq!(ok, 1);
        assert_eq!(errs, 1);
    }
}
