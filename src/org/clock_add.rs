//! Heading lookup + LOGBOOK insertion helpers used by
//! ``org-cli clock add``.
//!
//! All functions in this module are pure (string in / string out
//! or string + index in / index out). The CLI orchestration
//! lives in [`crate::cli::action::clock::run_add`].
//!
//! The mental model:
//!
//! 1. The caller supplies a [`HeadingQuery`] (TASK_REF or
//!    substring). [`find_heading`] returns the 1-based line
//!    number of the matched heading line — or an error if the
//!    match is missing or ambiguous.
//! 2. [`find_logbook`] inspects the section directly under that
//!    heading and reports either an existing `:LOGBOOK: / :END:`
//!    drawer, or `None`.
//! 3. [`find_logbook_insert_point`] picks the line *after* which
//!    a freshly-created LOGBOOK should be placed (after
//!    `:PROPERTIES:` and any metadata lines, before body text).
//! 4. [`apply_add`] composes the result: returns the rewritten
//!    file content + a structured [`ClockAddRecord`].
//!
//! This module deliberately knows nothing about the canonical
//! CLOCK format — it just gets handed a `clock_line` string by
//! the caller (who built it through [`crate::org::clock::format_clock_line`]).

use serde::Serialize;

use crate::org::heading::{Heading, iter_headings};

/// What to look up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadingQuery {
    /// Match the first heading whose section contains a
    /// `:TASK_REF: <number>` property line.
    TaskRef(u64),
    /// Case-insensitive substring of the heading's title text
    /// (i.e. the part after `***` and any TODO keyword).
    Substring(String),
}

impl HeadingQuery {
    /// Build a query from a CLI argument: digit-only string →
    /// `TaskRef`, anything else → `Substring`.
    pub fn from_cli(s: &str) -> Self {
        if !s.is_empty()
            && s.chars().all(|c| c.is_ascii_digit())
            && let Ok(n) = s.parse::<u64>()
        {
            return HeadingQuery::TaskRef(n);
        }
        HeadingQuery::Substring(s.to_string())
    }
}

/// Heading lookup outcome on success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadingHit {
    /// 1-based line number of the heading line itself.
    pub line: usize,
    /// The full heading line as it appears in the source (incl.
    /// leading stars).
    pub raw: String,
    /// Number of leading `*` characters (level).
    pub level: usize,
}

/// What can go wrong looking up a heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindError {
    NotFound { what: String },
    Ambiguous { what: String, hits: Vec<HeadingHit> },
}

impl std::fmt::Display for FindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FindError::NotFound { what } => write!(f, "heading not found: {}", what),
            FindError::Ambiguous { what, hits } => {
                write!(f, "{} heading match(es) for {:?}:", hits.len(), what)?;
                for h in hits {
                    write!(f, "\n  line {}: {}", h.line, h.raw.trim_end())?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for FindError {}

/// Locate the heading specified by `query` in `content`.
///
/// `TaskRef`: scans the file for the literal property line
/// `:TASK_REF: <n>` (any leading whitespace allowed). If found,
/// walks back to the nearest preceding heading. The matched
/// heading is the one whose section *contains* the property,
/// not the property line itself.
///
/// `Substring`: collects every heading whose title text
/// (post-stars, post-TODO-keyword, pre-tags) contains `query`
/// as a case-insensitive substring. A single match wins;
/// multiple matches surface as `Ambiguous`.
pub fn find_heading(content: &str, query: &HeadingQuery) -> Result<HeadingHit, FindError> {
    match query {
        HeadingQuery::TaskRef(n) => iter_headings(content)
            .find(|heading| task_ref_matches(heading, *n))
            .and_then(|heading| heading_hit(content, &heading))
            .ok_or_else(|| FindError::NotFound {
                what: format!(":TASK_REF: {}", n),
            }),
        HeadingQuery::Substring(needle) => {
            let lower = needle.to_lowercase();
            let hits = iter_headings(content)
                .filter(|heading| heading.title.to_lowercase().contains(&lower))
                .filter_map(|heading| heading_hit(content, &heading))
                .collect::<Vec<_>>();
            match hits.len() {
                0 => Err(FindError::NotFound {
                    what: needle.to_string(),
                }),
                1 => Ok(hits.into_iter().next().ok_or_else(|| FindError::NotFound {
                    what: needle.to_string(),
                })?),
                _ => Err(FindError::Ambiguous {
                    what: needle.to_string(),
                    hits,
                }),
            }
        }
    }
}

fn task_ref_matches(heading: &Heading, n: u64) -> bool {
    heading
        .properties
        .get("TASK_REF")
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| value == n)
        .unwrap_or(false)
}

fn heading_hit(content: &str, heading: &Heading) -> Option<HeadingHit> {
    let line_index = heading.line.checked_sub(1)?;
    let level = usize::try_from(heading.level).ok()?;
    let raw = content.lines().nth(line_index)?.to_string();

    Some(HeadingHit {
        line: heading.line,
        raw,
        level,
    })
}

/// Parse a single line as a heading. Returns `None` for non-headings.
fn parse_heading_line(line: &str, line_no: usize) -> Option<HeadingHit> {
    let bytes = line.as_bytes();
    let mut level = 0;
    while level < bytes.len() && bytes[level] == b'*' {
        level += 1;
    }
    if level == 0 {
        return None;
    }
    // Heading marker requires a space after the stars.
    if level >= bytes.len() || bytes[level] != b' ' {
        return None;
    }
    Some(HeadingHit {
        line: line_no,
        raw: line.to_string(),
        level,
    })
}

/// What [`find_logbook`] returns when the heading already has a
/// LOGBOOK drawer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogbookSpan {
    /// 1-based line number of the `:LOGBOOK:` line.
    pub start: usize,
    /// 1-based line number of the matching `:END:` line.
    pub end: usize,
    /// Indentation prefix of the `:LOGBOOK:` line (whitespace
    /// before the colon). New CLOCK lines use the same prefix.
    pub indent: String,
}

/// Find the LOGBOOK drawer attached *directly* to the heading
/// at `heading_line` (1-based). The drawer must appear before
/// any sub-heading or further heading — a LOGBOOK that lives
/// inside a child sub-heading is the child's, not the parent's,
/// and is correctly returned as `None` here.
pub fn find_logbook(content: &str, heading_line: usize) -> Option<LogbookSpan> {
    let lines: Vec<&str> = content.lines().collect();
    let _h = parse_heading_line(lines.get(heading_line - 1)?, heading_line)?;
    let mut start: Option<usize> = None;
    let mut indent: String = String::new();
    for (i, line) in lines.iter().enumerate().skip(heading_line) {
        // Stop at the next heading at any level — the parent's
        // drawers can only sit *between* the heading line and
        // the first sub-heading.
        if parse_heading_line(line, i + 1).is_some() {
            break;
        }
        let trimmed = line.trim_start();
        if start.is_none() && trimmed.eq_ignore_ascii_case(":LOGBOOK:") {
            start = Some(i + 1);
            indent = line[..line.len() - trimmed.len()].to_string();
            continue;
        }
        if let Some(s) = start
            && trimmed.eq_ignore_ascii_case(":END:")
        {
            return Some(LogbookSpan {
                start: s,
                end: i + 1,
                indent,
            });
        }
    }
    None
}

/// Pick the line **after which** a freshly-created LOGBOOK
/// drawer should be inserted under the heading at `heading_line`
/// (1-based). The chosen line is the last of:
///
/// * the heading line itself (always),
/// * a `:PROPERTIES: … :END:` drawer immediately under it,
/// * any number of `CLOSED:` / `SCHEDULED:` / `DEADLINE:` lines.
///
/// Stops at body text, sub-headings, or end of file.
pub fn find_logbook_insert_point(content: &str, heading_line: usize) -> usize {
    let lines: Vec<&str> = content.lines().collect();
    let Some(h) = lines
        .get(heading_line - 1)
        .and_then(|l| parse_heading_line(l, heading_line))
    else {
        return heading_line;
    };
    let mut after: usize = heading_line;
    let mut i = heading_line; // 0-based index into lines, points to NEXT line below heading
    while i < lines.len() {
        let line = lines[i];
        // Stop at next heading.
        if parse_heading_line(line, i + 1).is_some() {
            break;
        }
        let trimmed = line.trim_start();
        if trimmed.eq_ignore_ascii_case(":PROPERTIES:") {
            // Walk to the matching :END:.
            after = i + 1;
            i += 1;
            while i < lines.len() {
                let inner = lines[i].trim_start();
                if inner.eq_ignore_ascii_case(":END:") {
                    after = i + 1;
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        // Metadata lines we step over.
        let upper = trimmed.split_whitespace().next().unwrap_or("");
        if matches!(upper, "CLOSED:" | "SCHEDULED:" | "DEADLINE:") {
            after = i + 1;
            i += 1;
            continue;
        }
        // Empty line directly after the heading is part of the
        // heading's prelude; skip it but don't advance `after`,
        // so the new drawer lands snug to the heading.
        if trimmed.is_empty() && after == heading_line {
            i += 1;
            continue;
        }
        // Anything else is body content; we stop here.
        break;
    }
    let _ = h; // level not currently needed beyond initial validation
    after
}

/// Output of [`apply_add`].
#[derive(Debug, Clone, Serialize)]
pub struct ClockAddRecord {
    /// 1-based line number where the new CLOCK line ended up.
    pub line: usize,
    /// `"created-logbook"` if a fresh `:LOGBOOK:` drawer was
    /// created, `"appended-to-logbook"` if the line was inserted
    /// into a pre-existing drawer.
    pub edit_kind: &'static str,
    /// The CLOCK line that was inserted (no leading whitespace,
    /// no trailing newline).
    pub clock_line: String,
    /// The line found by the heading lookup (1-based).
    pub heading_line: usize,
}

/// Compose the rewrite. Returns `(new_content, record)`.
///
/// `clock_line` should be the canonical CLOCK line as produced
/// by [`crate::org::clock::format_clock_line`] — it is inserted
/// verbatim, with the appropriate indentation prepended.
pub fn apply_add(
    content: &str,
    query: &HeadingQuery,
    clock_line: &str,
) -> Result<(String, ClockAddRecord), FindError> {
    let heading = find_heading(content, query)?;
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let trailing_newline = content.ends_with('\n');

    let (insert_index, indent, edit_kind) = match find_logbook(content, heading.line) {
        Some(span) => {
            // Insert AFTER the :LOGBOOK: line (line number `span.start`).
            // 0-based vec index for "after that line" is `span.start`.
            (span.start, span.indent, "appended-to-logbook")
        }
        None => {
            // No drawer yet: build one. We produce three lines
            // (`:LOGBOOK:`, the CLOCK, `:END:`) right after the
            // chosen insert point.
            let after = find_logbook_insert_point(content, heading.line);
            let indent = "  ".to_string(); // canonical 2 spaces
            // Splice three lines starting at index `after`.
            let to_insert = vec![
                format!("{}:LOGBOOK:", indent),
                format!("{}{}", indent, clock_line),
                format!("{}:END:", indent),
            ];
            for (k, l) in to_insert.into_iter().enumerate() {
                lines.insert(after + k, l);
            }
            // Reassemble and return early; line number of the
            // CLOCK is `after + 2` (1-based).
            let mut out = lines.join("\n");
            if trailing_newline {
                out.push('\n');
            }
            return Ok((
                out,
                ClockAddRecord {
                    line: after + 2,
                    edit_kind: "created-logbook",
                    clock_line: clock_line.to_string(),
                    heading_line: heading.line,
                },
            ));
        }
    };

    let new_line = format!("{}{}", indent, clock_line);
    lines.insert(insert_index, new_line);
    let mut out = lines.join("\n");
    if trailing_newline {
        out.push('\n');
    }
    Ok((
        out,
        ClockAddRecord {
            line: insert_index + 1,
            edit_kind,
            clock_line: clock_line.to_string(),
            heading_line: heading.line,
        },
    ))
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    // ----- HeadingQuery::from_cli -------------------------------------

    #[test]
    fn from_cli_recognises_task_ref() {
        assert_eq!(HeadingQuery::from_cli("248"), HeadingQuery::TaskRef(248));
        assert_eq!(HeadingQuery::from_cli("0"), HeadingQuery::TaskRef(0));
    }

    #[test]
    fn from_cli_falls_back_to_substring() {
        assert_eq!(
            HeadingQuery::from_cli("fix login"),
            HeadingQuery::Substring("fix login".into())
        );
        assert_eq!(
            HeadingQuery::from_cli("248-A"),
            HeadingQuery::Substring("248-A".into())
        );
    }

    // ----- find_heading -----------------------------------------------

    const SAMPLE_TASK: &str = indoc! {"
        * Logs

        ** WIP fix login bug
          :PROPERTIES:
          :TASK_REF: 248
          :END:

          some body text

        ** DONE refactor cron
          :PROPERTIES:
          :TASK_REF: 314
          :END:
    "};

    #[test]
    fn find_heading_by_task_ref() {
        let h = find_heading(SAMPLE_TASK, &HeadingQuery::TaskRef(248)).unwrap();
        assert_eq!(h.line, 3);
        assert!(h.raw.contains("fix login"));
        assert_eq!(h.level, 2);
    }

    #[test]
    fn find_heading_by_task_ref_not_found() {
        let err = find_heading(SAMPLE_TASK, &HeadingQuery::TaskRef(999)).unwrap_err();
        matches!(err, FindError::NotFound { .. });
    }

    #[test]
    fn find_heading_by_substring_case_insensitive() {
        let h = find_heading(SAMPLE_TASK, &HeadingQuery::Substring("LOGIN".into())).unwrap();
        assert!(h.raw.contains("fix login"));
    }

    #[test]
    fn find_heading_by_substring_ambiguous() {
        let content = indoc! {"
            * fix one
            * fix two
        "};
        let err = find_heading(content, &HeadingQuery::Substring("fix".into())).unwrap_err();
        if let FindError::Ambiguous { hits, .. } = err {
            assert_eq!(hits.len(), 2);
        } else {
            panic!("expected Ambiguous, got {:?}", err);
        }
    }

    #[test]
    fn find_heading_strips_trailing_tags() {
        let content = "* WIP my task :work:foo:\n";
        let h = find_heading(content, &HeadingQuery::Substring("my task".into())).unwrap();
        assert_eq!(h.line, 1);
    }

    #[test]
    fn find_heading_handles_todo_keyword() {
        let content = indoc! {"
            * TODO build feature
            * implementing build feature
        "};
        // Substring "build feature" matches both: it appears in
        // both titles (the second one is just "build feature"
        // verbatim, the first has TODO stripped). Should be
        // Ambiguous.
        let err =
            find_heading(content, &HeadingQuery::Substring("build feature".into())).unwrap_err();
        if let FindError::Ambiguous { hits, .. } = err {
            assert_eq!(hits.len(), 2);
        } else {
            panic!("expected Ambiguous, got {:?}", err);
        }
    }

    // ----- find_logbook -----------------------------------------------

    #[test]
    fn find_logbook_existing() {
        let content = indoc! {"
            ** WIP my task
              :PROPERTIES:
              :TASK_REF: 1
              :END:
              :LOGBOOK:
              CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15
              :END:

              body
        "};
        let lb = find_logbook(content, 1).expect("logbook");
        assert_eq!(lb.start, 5);
        assert_eq!(lb.end, 7);
        assert_eq!(lb.indent, "  ");
    }

    #[test]
    fn find_logbook_absent() {
        let content = indoc! {"
            ** WIP empty task
              :PROPERTIES:
              :TASK_REF: 2
              :END:

              body
        "};
        assert!(find_logbook(content, 1).is_none());
    }

    #[test]
    fn find_logbook_does_not_pierce_into_subheading() {
        let content = indoc! {"
            ** parent
            *** child
              :LOGBOOK:
              CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15
              :END:
        "};
        // The LOGBOOK belongs to *** child, not ** parent.
        // find_logbook(content, 1) should return None because
        // the next heading at line 2 is shallower (level 3 < we
        // require >= parent's level 2 for "still in section").
        // Wait — for level 2 parent, level 3 IS deeper, so the
        // scan should continue. We deliberately stop at deeper
        // sub-headings to avoid stealing their drawers.
        assert!(
            find_logbook(content, 1).is_none(),
            "must not pierce into ***"
        );
    }

    // ----- find_logbook_insert_point ----------------------------------

    #[test]
    fn insert_point_after_properties_drawer() {
        let content = indoc! {"
            ** WIP my task
              :PROPERTIES:
              :TASK_REF: 1
              :END:

              body content
        "};
        // PROPERTIES :END: is line 4; new drawer goes after it.
        assert_eq!(find_logbook_insert_point(content, 1), 4);
    }

    #[test]
    fn insert_point_after_closed_line() {
        let content = indoc! {"
            ** DONE my task
              CLOSED: [2025-06-06 Fri 11:45 +0200]
              :PROPERTIES:
              :TASK_REF: 1
              :END:

              body
        "};
        // CLOSED comes BEFORE PROPERTIES; Emacs writes either
        // order. Either way we should land after both.
        assert_eq!(find_logbook_insert_point(content, 1), 5);
    }

    #[test]
    fn insert_point_for_bare_heading() {
        let content = indoc! {"
            ** bare
        "};
        // No properties, no metadata: drawer goes right after
        // the heading line.
        assert_eq!(find_logbook_insert_point(content, 1), 1);
    }

    // ----- apply_add --------------------------------------------------

    #[test]
    fn apply_add_appends_to_existing_logbook() {
        let content = indoc! {"
            ** WIP my task
              :LOGBOOK:
              CLOCK: [2025-06-06 Fri 14:00 +0200]--[2025-06-06 Fri 15:00 +0200] =>  1:00
              :END:
        "};
        let clock = "CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15";
        let (new, rec) =
            apply_add(content, &HeadingQuery::Substring("my task".into()), clock).unwrap();
        assert_eq!(rec.edit_kind, "appended-to-logbook");
        // New line should be at line 3 (after :LOGBOOK: at line 2).
        assert_eq!(rec.line, 3);
        let want = indoc! {"
            ** WIP my task
              :LOGBOOK:
              CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15
              CLOCK: [2025-06-06 Fri 14:00 +0200]--[2025-06-06 Fri 15:00 +0200] =>  1:00
              :END:
        "};
        assert_eq!(new, want);
    }

    #[test]
    fn apply_add_creates_logbook_after_properties() {
        let content = indoc! {"
            ** WIP fresh
              :PROPERTIES:
              :TASK_REF: 248
              :END:

              body
        "};
        let clock = "CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15";
        let (new, rec) = apply_add(content, &HeadingQuery::TaskRef(248), clock).unwrap();
        assert_eq!(rec.edit_kind, "created-logbook");
        let want = indoc! {"
            ** WIP fresh
              :PROPERTIES:
              :TASK_REF: 248
              :END:
              :LOGBOOK:
              CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15
              :END:

              body
        "};
        assert_eq!(new, want);
    }

    #[test]
    fn apply_add_creates_logbook_for_bare_heading() {
        let content = indoc! {"
            ** WIP bare task
        "};
        let clock = "CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15";
        let (new, _rec) =
            apply_add(content, &HeadingQuery::Substring("bare task".into()), clock).unwrap();
        let want = indoc! {"
            ** WIP bare task
              :LOGBOOK:
              CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15
              :END:
        "};
        assert_eq!(new, want);
    }

    #[test]
    fn apply_add_round_trips_through_parse_clock_line() {
        // The line we insert must pass strict canonical
        // validation. We build it via the canonical formatter
        // and assert the result of `apply_add` still parses
        // clean when re-read.
        use crate::org::clock::{format_clock_line, parse_clock_line};
        use chrono::{NaiveDate, NaiveDateTime};
        let s = NaiveDateTime::new(
            NaiveDate::from_ymd_opt(2025, 6, 6).unwrap(),
            chrono::NaiveTime::from_hms_opt(9, 30, 0).unwrap(),
        );
        let e = NaiveDateTime::new(
            NaiveDate::from_ymd_opt(2025, 6, 6).unwrap(),
            chrono::NaiveTime::from_hms_opt(11, 45, 0).unwrap(),
        );
        let clock = format_clock_line(s, e, "+0200", false);

        let (new, rec) = apply_add(
            "** WIP target\n",
            &HeadingQuery::Substring("target".into()),
            &clock,
        )
        .unwrap();
        // Re-parse the inserted line.
        let inserted = new.lines().nth(rec.line - 1).unwrap();
        let parsed = parse_clock_line(inserted)
            .expect("structurally parseable")
            .expect("closed-range");
        assert!(parsed.violations.is_empty(), "got {:?}", parsed.violations);
    }

    #[test]
    fn apply_add_preserves_indent_of_existing_logbook() {
        // 3-space indented LOGBOOK in older files.
        let content = indoc! {"
            ** WIP my task
               :LOGBOOK:
               CLOCK: [2025-06-06 Fri 14:00 +0200]--[2025-06-06 Fri 15:00 +0200] =>  1:00
               :END:
        "};
        let clock = "CLOCK: [2025-06-06 Fri 09:30 +0200]--[2025-06-06 Fri 11:45 +0200] =>  2:15";
        let (new, _rec) =
            apply_add(content, &HeadingQuery::Substring("my task".into()), clock).unwrap();
        // Inserted line keeps the 3-space indent of its drawer.
        assert!(
            new.contains("   CLOCK: [2025-06-06 Fri 09:30"),
            "indent not preserved:\n{}",
            new
        );
    }

    #[test]
    fn apply_add_returns_error_on_missing_heading() {
        let content = "** unrelated\n";
        let err = apply_add(content, &HeadingQuery::TaskRef(999), "CLOCK: [...]").unwrap_err();
        matches!(err, FindError::NotFound { .. });
    }
}
