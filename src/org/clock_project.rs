//! Project one org file's content into its CLOCK facts and diagnostics.
//!
//! This is the layer-1 producer: given exact bytes, it yields the
//! sorted records the cache stores, plus the diagnostics that
//! `clock check` would have raised. It exists so that every global
//! CLOCK query derives its data through **one** parse of the document
//! structure, rather than each command growing its own line scanner.
//!
//! # Where structure comes from
//!
//! `orgize` decides what is and is not a CLOCK. Two consequences:
//!
//! - A `CLOCK:` line inside `#+begin_src`, `#+begin_example`, or any
//!   other literal block is **not** a CLOCK. It is documentation about
//!   a CLOCK, and orgize never emits a `CLOCK` node for it. This module
//!   inherits that judgement rather than re-deriving block grammar.
//! - A malformed `CLOCK:` line (missing timezone, bad weekday, …)
//!   does *not* become a `CLOCK` node either — orgize degrades it to
//!   paragraph text. Such lines must still surface as diagnostics, or
//!   a warm cache would silently hide what a cold `clock check` would
//!   report. They are found by scanning the text of **non-literal**
//!   nodes only, so a `CLOCK:` inside a source block stays invisible
//!   on both paths.
//!
//! # Where CLOCK semantics come from
//!
//! [`crate::org::clock::parse_clock_line`] remains the only CLOCK
//! grammar in the crate. This module hands it the exact source line
//! orgize identified; it never re-implements timestamp parsing.

use orgize::rowan::ast::AstNode;
use orgize::{Org, SyntaxKind, SyntaxNode};

use crate::index::clock_blob::{Blob, FLAG_HAS_SECONDS, Interner, NO_HEADING, Record};
use crate::org::clock::parse_clock_line;

/// Bump when the *meaning* of a produced record or diagnostic changes
/// — e.g. a new flag, a changed instant epoch, a different heading
/// attribution rule. Folded into every cache key.
pub const PROJECTION_ABI: u16 = 2;

/// Bump when the patched `orgize` fork changes in a way that could
/// alter which lines become `CLOCK` nodes. Folded into every cache
/// key. Tracked by hand because the fork is a path dependency with no
/// version of its own.
pub const PARSER_ABI: u16 = 1;

/// One CLOCK line that failed strict validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// 1-based line number.
    pub line: u32,
    /// Byte offset of the line's first byte.
    pub byte_offset: u32,
    /// Stable tag from [`crate::org::clock`] (`missing-tz`, …).
    pub tag: &'static str,
    /// Human message from the same source.
    pub message: String,
    /// The original line, unmodified.
    pub raw: String,
}

/// Everything layer 1 stores for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    /// Sorted, encodable facts. `strings[0]` is always the canonical
    /// path the caller supplied, so `path_id == 0` for every record.
    pub blob: Blob,
    /// Lines that looked like a CLOCK but were rejected.
    pub diagnostics: Vec<Diagnostic>,
}

/// Project `content` (the exact bytes of `canonical_path`) into
/// facts and diagnostics.
///
/// `canonical_path` is interned as string 0; every record's
/// `path_id` is 0. Layer 2 re-keys it on merge.
pub fn project(canonical_path: &str, content: &str) -> Projection {
    let org = Org::parse(content);
    let root = org.document().syntax().clone();

    let mut interner = Interner::default();
    let path_id = interner.intern(canonical_path);
    debug_assert_eq!(path_id, 0);

    let mut records = Vec::new();
    let mut diagnostics = Vec::new();
    // (level, title). Titles are interned lazily, only when a CLOCK
    // is actually filed under them: a corpus has vastly more
    // headings than clocked headings, and interning every title
    // would make the string table dwarf the records it serves.
    let mut heading_stack: Vec<(u32, String)> = Vec::new();

    walk(
        &root,
        content,
        &mut interner,
        &mut heading_stack,
        &mut records,
        &mut diagnostics,
    );

    records.sort_by_key(Record::sort_key);
    diagnostics.sort_by_key(|d| d.byte_offset);

    Projection {
        blob: Blob {
            projection_abi: PROJECTION_ABI,
            parser_abi: PARSER_ABI,
            records,
            strings: interner.into_strings(),
        },
        diagnostics,
    }
}

/// Depth-first walk that stops at literal blocks.
fn walk(
    node: &SyntaxNode,
    content: &str,
    interner: &mut Interner,
    heading_stack: &mut Vec<(u32, String)>,
    records: &mut Vec<Record>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for child in node.children() {
        match child.kind() {
            // Literal content: nothing inside is a CLOCK, on either
            // path. Do not descend, do not scan text.
            SyntaxKind::SOURCE_BLOCK
            | SyntaxKind::EXAMPLE_BLOCK
            | SyntaxKind::EXPORT_BLOCK
            | SyntaxKind::COMMENT_BLOCK
            | SyntaxKind::VERSE_BLOCK
            | SyntaxKind::COMMENT
            | SyntaxKind::FIXED_WIDTH => continue,

            SyntaxKind::HEADLINE => {
                let level = headline_level(&child);
                let title = headline_title(&child);
                heading_stack.retain(|(l, _)| *l < level);
                heading_stack.push((level, title));
                walk(
                    &child,
                    content,
                    interner,
                    heading_stack,
                    records,
                    diagnostics,
                );
                heading_stack.retain(|(l, _)| *l < level);
            }

            // A well-formed CLOCK. Re-validate through the strict
            // grammar so that `clock check`-level rules (weekday,
            // duration, spacing, accepted zones) still apply.
            SyntaxKind::CLOCK => {
                let (offset, raw) = node_line(&child, content);
                classify(
                    offset,
                    raw,
                    content,
                    heading_stack,
                    interner,
                    records,
                    diagnostics,
                );
            }

            // Paragraph text may hide a malformed CLOCK that orgize
            // refused to promote. Scan its lines, but only here —
            // never inside the literal kinds skipped above.
            SyntaxKind::PARAGRAPH => {
                let start: usize = child.text_range().start().into();
                let end: usize = child.text_range().end().into();
                let mut pos = start;
                for line in content[start..end].split_inclusive('\n') {
                    if line.contains("CLOCK:") {
                        let raw = line.trim_end_matches(['\n', '\r']);
                        classify(
                            pos,
                            raw,
                            content,
                            heading_stack,
                            interner,
                            records,
                            diagnostics,
                        );
                    }
                    pos += line.len();
                }
                // Paragraphs contain no elements; nothing to descend into.
            }

            _ => walk(
                &child,
                content,
                interner,
                heading_stack,
                records,
                diagnostics,
            ),
        }
    }
}

/// Run the strict grammar on one candidate line and file the result.
fn classify(
    byte_offset: usize,
    raw: &str,
    content: &str,
    heading_stack: &[(u32, String)],
    interner: &mut Interner,
    records: &mut Vec<Record>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let line = line_number_at(content, byte_offset);
    // Intern here, not at headline entry: only clocked headings
    // earn a string-table slot.
    let heading_id = heading_stack
        .last()
        .map(|(_, title)| interner.intern(title))
        .unwrap_or(NO_HEADING);
    match parse_clock_line(raw) {
        // Running clock or not a closed range at all: neither fact
        // nor diagnostic, exactly as `clock check` treats it.
        Ok(None) => {}
        Ok(Some(p)) if p.violations.is_empty() => {
            records.push(Record {
                start: p.line.start_utc_seconds(),
                end: p.line.end_utc_seconds(),
                path_id: 0,
                heading_id,
                byte_offset: byte_offset as u32,
                line,
                tz_minutes: p.line.start.tz_minutes as i16,
                flags: if p.line.has_seconds {
                    FLAG_HAS_SECONDS
                } else {
                    0
                },
            });
        }
        Ok(Some(p)) => {
            for v in &p.violations {
                diagnostics.push(Diagnostic {
                    line,
                    byte_offset: byte_offset as u32,
                    tag: v.tag(),
                    message: v.message(),
                    raw: raw.to_owned(),
                });
            }
        }
        Err(e) => diagnostics.push(Diagnostic {
            line,
            byte_offset: byte_offset as u32,
            tag: e.tag(),
            message: e.message(),
            raw: raw.to_owned(),
        }),
    }
}

/// `(byte offset of the line, the line without its terminator)` for a
/// node that is known to occupy exactly one source line.
fn node_line<'a>(node: &SyntaxNode, content: &'a str) -> (usize, &'a str) {
    let start: usize = node.text_range().start().into();
    let end: usize = node.text_range().end().into();
    let raw = content[start..end].trim_end_matches(['\n', '\r']);
    (start, raw)
}

fn line_number_at(content: &str, byte_offset: usize) -> u32 {
    (content[..byte_offset]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
        + 1) as u32
}

fn headline_level(node: &SyntaxNode) -> u32 {
    node.children_with_tokens()
        .find_map(|t| match t.kind() {
            SyntaxKind::HEADLINE_STARS => t.as_token().map(|tok| tok.text().len() as u32),
            _ => None,
        })
        .unwrap_or(1)
}

fn headline_title(node: &SyntaxNode) -> String {
    node.children()
        .find(|c| c.kind() == SyntaxKind::HEADLINE_TITLE)
        .map(|t| t.text().to_string().trim().to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    const P: &str = "/t.org";

    fn facts(content: &str) -> Vec<Record> {
        project(P, content).blob.records
    }

    fn diags(content: &str) -> Vec<&'static str> {
        project(P, content)
            .diagnostics
            .iter()
            .map(|d| d.tag)
            .collect()
    }

    // P4
    #[test]
    fn logbook_clock_is_a_fact() {
        let c = indoc! {"
            * h
            :LOGBOOK:
            CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00
            :END:
        "};
        let r = facts(c);
        assert_eq!(r.len(), 1, "{:?}", r);
        assert_eq!(r[0].end - r[0].start, 7200);
        assert_eq!(r[0].tz_minutes, 60);
        assert_eq!(r[0].line, 3);
        assert_eq!(r[0].flags & FLAG_HAS_SECONDS, 0);
        assert!(diags(c).is_empty());
    }

    // P1
    #[test]
    fn src_block_is_not_a_clock() {
        let c = indoc! {"
            * h
            #+begin_src org
            CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00
            #+end_src
        "};
        assert!(facts(c).is_empty(), "src block content must not be a fact");
        assert!(diags(c).is_empty(), "…nor a diagnostic");
    }

    // P2
    #[test]
    fn example_and_quote_blocks_are_not_clocks() {
        let c = indoc! {"
            * h
            #+begin_example
            CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00
            #+end_example
            #+begin_quote
            CLOCK: [2026-01-07 Wed 13:00 +0100]--[2026-01-07 Wed 14:00 +0100] =>  1:00
            #+end_quote
        "};
        // `example` is literal: invisible. `quote` is a greater block
        // whose content is ordinary org — a CLOCK inside it IS a
        // CLOCK, as Emacs itself treats it. This pins that distinction.
        let r = facts(c);
        assert_eq!(r.len(), 1, "{:?}", r);
        assert_eq!(r[0].line, 6);
    }

    // P3 — the todo.org:2634 regression
    #[test]
    fn headline_dissolves_unterminated_block() {
        let c = indoc! {"
            * doc
            #+begin_quote
            #+BEGIN_SRC elisp
            #+end_quote
            * real work
            :LOGBOOK:
            CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00
            :END:
        "};
        let r = facts(c);
        assert_eq!(
            r.len(),
            1,
            "the headline must terminate the broken block; got {:?}",
            r
        );
        assert_eq!(r[0].line, 7);
    }

    // P5
    #[test]
    fn malformed_clock_is_a_diagnostic() {
        let c = indoc! {"
            * h
            CLOCK: [2026-01-07 Wed 09:57]--[2026-01-07 Wed 12:19] =>  2:22
        "};
        assert!(facts(c).is_empty());
        assert_eq!(diags(c), vec!["missing-tz"]);
    }

    // P5: a malformed CLOCK in a src block is neither
    #[test]
    fn malformed_clock_in_src_block_is_invisible() {
        let c = indoc! {"
            * h
            #+begin_src org
            CLOCK: [2026-01-07 Wed 09:57]--[2026-01-07 Wed 12:19] =>  2:22
            #+end_src
        "};
        assert!(facts(c).is_empty());
        assert!(diags(c).is_empty());
    }

    // P6
    #[test]
    fn running_clock_is_ignored() {
        let c = indoc! {"
            * h
            CLOCK: [2026-01-07 Wed 10:00 +0100]
        "};
        assert!(facts(c).is_empty());
        assert!(diags(c).is_empty());
    }

    // P7
    #[test]
    fn fact_carries_heading() {
        let c = indoc! {"
            * Parent
            ** DONE The task
            :LOGBOOK:
            CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00
            :END:
            * Other
            CLOCK: [2026-01-08 Thu 10:00 +0100]--[2026-01-08 Thu 12:00 +0100] =>  2:00
        "};
        let p = project(P, c);
        let title = |r: &Record| p.blob.strings[r.heading_id as usize].clone();
        assert_eq!(p.blob.records.len(), 2);
        // orgize's HEADLINE_TITLE excludes the TODO keyword, matching
        // the `keyword` / `title` split in `crate::org::heading`.
        assert_eq!(title(&p.blob.records[0]), "The task");
        assert_eq!(title(&p.blob.records[1]), "Other");
    }

    #[test]
    fn clock_before_any_heading_has_no_heading() {
        let c = "CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00\n";
        let r = facts(c);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].heading_id, NO_HEADING);
    }

    // P8
    #[test]
    fn facts_are_sorted() {
        let c = indoc! {"
            * h
            CLOCK: [2026-01-07 Wed 14:00 +0100]--[2026-01-07 Wed 15:00 +0100] =>  1:00
            CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00
        "};
        let r = facts(c);
        assert_eq!(r.len(), 2);
        assert!(r[0].start < r[1].start);
        assert_eq!(r[0].line, 3, "the earlier instant is on the later line");
    }

    #[test]
    fn path_is_string_zero() {
        let p = project(
            P,
            "CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00\n",
        );
        assert_eq!(p.blob.strings[0], P);
        assert!(p.blob.records.iter().all(|r| r.path_id == 0));
    }

    #[test]
    fn projection_is_encodable() {
        let p = project(
            P,
            indoc! {"
                * h
                CLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00
            "},
        );
        p.blob.encode().unwrap();
    }

    #[test]
    fn seconds_precision_sets_flag() {
        let c =
            "CLOCK: [2026-01-07 Wed 09:57:20 +0100]--[2026-01-07 Wed 12:19:25 +0100] =>  2:22:05\n";
        let r = facts(c);
        assert_eq!(r.len(), 1);
        assert_ne!(r[0].flags & FLAG_HAS_SECONDS, 0);
    }
}
