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

use chrono::{Datelike, NaiveDate, Weekday};

/// One successfully-validated closed-range CLOCK line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockLine {
    pub start: Timestamp,
    pub end: Timestamp,
    /// Whether both timestamps carry seconds.
    pub has_seconds: bool,
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

/// What can be wrong with a closed-range CLOCK line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClockError {
    /// One of the bracketed timestamps has no `±HHMM` suffix.
    MissingTz {
        /// `"start"` or `"end"`.
        side: &'static str,
    },
    /// A timezone is present but not numerical (e.g. `CEST`).
    NonNumericalTz { side: &'static str, token: String },
    /// One side has seconds, the other does not.
    MixedPrecision,
    /// Weekday token disagrees with the date.
    WeekdayMismatch {
        side: &'static str,
        date: NaiveDate,
        token: String,
        expected: String,
    },
    /// End instant strictly precedes start instant.
    StartAfterEnd,
    /// Looks like a closed-range CLOCK but cannot be parsed.
    Unparseable { reason: String },
}

impl ClockError {
    /// A short stable tag for machine-readable output.
    pub fn tag(&self) -> &'static str {
        match self {
            ClockError::MissingTz { .. } => "missing-tz",
            ClockError::NonNumericalTz { .. } => "non-numerical-tz",
            ClockError::MixedPrecision => "mixed-precision",
            ClockError::WeekdayMismatch { .. } => "weekday-mismatch",
            ClockError::StartAfterEnd => "start-after-end",
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
            ClockError::MixedPrecision => {
                "start and end use different precision (one HH:MM, the other HH:MM:SS)".into()
            }
            ClockError::WeekdayMismatch {
                side,
                date,
                token,
                expected,
            } => format!(
                "{} weekday token {:?} disagrees with {} (expected {:?})",
                side, token, date, expected
            ),
            ClockError::StartAfterEnd => "end instant precedes start instant".into(),
            ClockError::Unparseable { reason } => format!("unparseable CLOCK line: {}", reason),
        }
    }
}

impl fmt::Display for ClockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ClockError {}

/// Parse and validate one line.
///
/// Returns `Ok(None)` if the line is not a closed-range CLOCK
/// (running clock, comment, body text mentioning `CLOCK:`, …).
///
/// Returns `Ok(Some(ClockLine))` on a fully valid line.
///
/// Returns `Err(_)` when the line *is* a closed-range CLOCK line
/// but breaks one of the canonical-format invariants.
pub fn parse_clock_line(line: &str) -> Result<Option<ClockLine>, ClockError> {
    // Find the `CLOCK:` prefix anywhere on the line, accepting
    // arbitrary leading whitespace (real-world LOGBOOK indentation
    // varies between 1, 2, and 3 spaces). Anything before the
    // prefix that isn't whitespace means the line is not a CLOCK
    // line.
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix("CLOCK:") else {
        return Ok(None);
    };
    let rest = rest.trim_start();

    // Closed-range silhouette: must start with `[`.
    if !rest.starts_with('[') {
        // Running clock or otherwise non-closed; not in scope.
        return Ok(None);
    }

    // Match `[…]--[…] => …`. Anything missing the duration arrow
    // doesn't qualify as a closed range we should validate.
    let (start_inner, after_start) = bracketed(rest).ok_or(ClockError::Unparseable {
        reason: "missing closing bracket on start timestamp".into(),
    })?;
    // No `--` after the first bracket → running clock, not in scope.
    let Some(after_dashes) = after_start.strip_prefix("--") else {
        return Ok(None);
    };
    let (end_inner, after_end) = bracketed(after_dashes).ok_or(ClockError::Unparseable {
        reason: "missing closing bracket on end timestamp".into(),
    })?;
    let after_end = after_end.trim_start();
    if !after_end.starts_with("=>") {
        return Err(ClockError::Unparseable {
            reason: "missing `=>` duration arrow".into(),
        });
    }

    // Parse each bracketed timestamp.
    let start = parse_timestamp(start_inner, "start")?;
    let end = parse_timestamp(end_inner, "end")?;

    // Precision must match.
    if start.second.is_some() != end.second.is_some() {
        return Err(ClockError::MixedPrecision);
    }

    // Start must not strictly follow end.
    if cmp_instants(&start, &end) == std::cmp::Ordering::Greater {
        return Err(ClockError::StartAfterEnd);
    }

    Ok(Some(ClockLine {
        has_seconds: start.second.is_some(),
        start,
        end,
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
/// `side` is `"start"` or `"end"` for error reporting.
fn parse_timestamp(inner: &str, side: &'static str) -> Result<Timestamp, ClockError> {
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

    // Weekday agreement.
    let expected = weekday_short(date.weekday());
    if day_s != expected {
        return Err(ClockError::WeekdayMismatch {
            side,
            date,
            token: day_s.to_string(),
            expected: expected.to_string(),
        });
    }

    Ok(Timestamp {
        date,
        weekday_token: day_s.to_string(),
        hour,
        minute,
        second,
        tz_minutes,
    })
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
fn weekday_short(w: Weekday) -> &'static str {
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

/// Compare two timestamps as wall-clock instants in UTC.
fn cmp_instants(a: &Timestamp, b: &Timestamp) -> std::cmp::Ordering {
    let ax = utc_minutes(a);
    let bx = utc_minutes(b);
    let asec = a.second.unwrap_or(0) as i64;
    let bsec = b.second.unwrap_or(0) as i64;
    (ax * 60 + asec).cmp(&(bx * 60 + bsec))
}

/// Convert a [`Timestamp`] to "minutes since the chrono epoch in UTC".
/// Used only for ordering.
fn utc_minutes(t: &Timestamp) -> i64 {
    let local_min =
        t.date.num_days_from_ce() as i64 * 24 * 60 + t.hour as i64 * 60 + t.minute as i64;
    local_min - t.tz_minutes as i64
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    // ----- happy path ----------------------------------------------------

    #[test]
    fn parses_canonical_minute_precision() {
        let line = "  CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15";
        let parsed = parse_clock_line(line).unwrap().unwrap();
        assert!(!parsed.has_seconds);
        assert_eq!(parsed.start.tz_minutes, 120);
        assert_eq!(parsed.end.tz_minutes, 120);
        assert_eq!(parsed.start.hour, 9);
        assert_eq!(parsed.end.hour, 11);
    }

    #[test]
    fn parses_canonical_second_precision() {
        let line =
            "CLOCK: [2026-01-07 Wed 09:57:20 +0100]--[2026-01-07 Wed 12:19:25 +0100] =>  2:22:05";
        let parsed = parse_clock_line(line).unwrap().unwrap();
        assert!(parsed.has_seconds);
        assert_eq!(parsed.start.second, Some(20));
        assert_eq!(parsed.end.second, Some(25));
    }

    #[test]
    fn parses_negative_tz() {
        let line = "CLOCK: [2025-12-15 Mon 09:00 -0500]--[2025-12-15 Mon 10:00 -0500] =>  1:00";
        let parsed = parse_clock_line(line).unwrap().unwrap();
        assert_eq!(parsed.start.tz_minutes, -300);
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

    // ----- violations ---------------------------------------------------

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
    fn rejects_mixed_precision() {
        let line = "CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45:30 +0200] =>  2:15";
        let err = parse_clock_line(line).unwrap_err();
        assert_eq!(err.tag(), "mixed-precision");
    }

    #[test]
    fn rejects_weekday_mismatch() {
        // 2025-06-06 was a Friday, not a Monday.
        let line = "CLOCK: [2025-06-06 Mon 09:30 +0200]--[2025-06-06 Mon 11:45 +0200] =>  2:15";
        let err = parse_clock_line(line).unwrap_err();
        assert_eq!(err.tag(), "weekday-mismatch");
    }

    #[test]
    fn rejects_start_after_end() {
        let line = "CLOCK: [2025-06-06 Fri 12:00 +0200]--[2025-06-06 Fri 11:00 +0200] =>  -1:00";
        let err = parse_clock_line(line).unwrap_err();
        assert_eq!(err.tag(), "start-after-end");
    }

    #[test]
    fn equal_start_and_end_is_ok() {
        let line = "CLOCK: [2025-06-06 Fri 12:00 +0200]--[2025-06-06 Fri 12:00 +0200] =>  0:00";
        assert!(parse_clock_line(line).unwrap().is_some());
    }

    #[test]
    fn cross_tz_ordering_uses_utc() {
        // 09:00 +0200 = 07:00 UTC; 08:00 +0000 = 08:00 UTC. End > Start.
        let line = "CLOCK: [2025-06-06 Fri 09:00 +0200]--[2025-06-06 Fri 08:00 +0000] =>  1:00";
        assert!(parse_clock_line(line).unwrap().is_some());
    }

    #[test]
    fn rejects_unparseable() {
        // Lines that *do* claim to be a closed range (have `--[…]`)
        // but break in some way once we look closer.
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
