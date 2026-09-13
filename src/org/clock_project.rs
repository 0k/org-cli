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

use orgize::ast::Headline as OrgHeadline;
use orgize::rowan::ast::AstNode;
use orgize::{Org, SyntaxKind, SyntaxNode};

use std::collections::BTreeMap;

use crate::index::clock_blob::{Blob, FLAG_HAS_SECONDS, Interner, NO_HEADING, Record};
use crate::org::clock::parse_clock_line;
use crate::org::heading::LineIndex;

/// Bump when the *meaning* of a produced record or diagnostic changes
/// — e.g. a new flag, a changed instant epoch, a different heading
/// attribution rule. Folded into every cache key.
pub const PROJECTION_ABI: u16 = 5;

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

/// The enclosing heading of at least one CLOCK fact, with the
/// metadata a heading filter needs.
///
/// Only *clocked* headings are recorded — the same lazy rule as the
/// interned title — so this stays small on a file with thousands of
/// headings and a handful of clocks. Keyed by the [`Record`]'s
/// `heading_id`, which identifies the heading by *position* (so two
/// headings with the same title stay distinct); the title lives in
/// [`ClockedHeading::title`], not in the string table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockedHeading {
    pub level: u32,
    /// 1-based line of the headline.
    pub line: u32,
    pub title: String,
    pub tags: Vec<String>,
    pub properties: BTreeMap<String, String>,
}

/// Everything layer 1 stores for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    /// Sorted, encodable facts. `strings[0]` is always the canonical
    /// path the caller supplied, so `path_id == 0` for every record.
    pub blob: Blob,
    /// Lines that looked like a CLOCK but were rejected.
    pub diagnostics: Vec<Diagnostic>,
    /// Headings that own at least one record, keyed by the record's
    /// `heading_id`. Empty for files with no clocked heading.
    pub headings: BTreeMap<u32, ClockedHeading>,
}

impl Projection {
    /// Every source line orgize identified as a CLOCK candidate —
    /// clean facts and rejected lines alike — as 1-based line
    /// numbers in **source order**, each at most once.
    ///
    /// This is the discovery surface for the per-line commands
    /// (`check`, `list`, `fix`): they iterate these lines instead of
    /// scanning every line for `CLOCK:`, so a sample inside
    /// `#+begin_src` is never a candidate. They then re-run
    /// [`crate::org::clock::parse_clock_line`] on the exact source
    /// text, because the blob does not carry a `ParsedClockLine` and
    /// the `unaccepted-tz` rule needs live config.
    pub fn candidate_lines(&self) -> Vec<u32> {
        let mut lines: Vec<u32> = self
            .blob
            .records
            .iter()
            .map(|r| r.line)
            .chain(self.diagnostics.iter().map(|d| d.line))
            .collect();
        lines.sort_unstable();
        lines.dedup();
        lines
    }
}

/// Project `content` (the exact bytes of `canonical_path`) into
/// facts and diagnostics.
///
/// `canonical_path` is interned as string 0; every record's
/// `path_id` is 0. Layer 2 re-keys it on merge.
pub fn project(canonical_path: &str, content: &str) -> Projection {
    let mut interner = Interner::default();
    let path_id = interner.intern(canonical_path);
    debug_assert_eq!(path_id, 0);

    // A file with no `CLOCK:` substring anywhere cannot yield a fact
    // or a diagnostic; both require that literal token. Skip the
    // parse. This is a pre-filter on the *result*, not a discovery
    // rule — it never decides whether a candidate that IS present
    // counts. On the real corpus ~70% of files take this path.
    if !content.contains("CLOCK:") {
        return Projection {
            blob: Blob {
                projection_abi: PROJECTION_ABI,
                parser_abi: PARSER_ABI,
                records: Vec::new(),
                strings: interner.into_strings(),
            },
            diagnostics: Vec::new(),
            headings: BTreeMap::new(),
        };
    }

    let org = Org::parse(content);
    let root = org.document().syntax().clone();
    // One linear scan; every candidate then resolves its line by
    // binary search instead of re-counting newlines from offset 0.
    let line_index = LineIndex::new(content);

    let mut records = Vec::new();
    let mut diagnostics = Vec::new();
    let mut headings: BTreeMap<u32, ClockedHeading> = BTreeMap::new();
    // (level, headline node). The title is extracted and interned
    // lazily, only when a CLOCK is actually filed under it: a corpus
    // has vastly more headings than clocked headings, and building a
    // String per headline (let alone interning it) dominated the
    // walk before this was made lazy.
    let mut heading_stack: Vec<(u32, SyntaxNode)> = Vec::new();

    walk(
        &root,
        content,
        &line_index,
        &mut interner,
        &mut heading_stack,
        &mut records,
        &mut diagnostics,
        &mut headings,
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
        headings,
    }
}

/// Depth-first walk that stops at literal blocks.
#[allow(clippy::too_many_arguments)]
fn walk(
    node: &SyntaxNode,
    content: &str,
    line_index: &LineIndex,
    interner: &mut Interner,
    heading_stack: &mut Vec<(u32, SyntaxNode)>,
    records: &mut Vec<Record>,
    diagnostics: &mut Vec<Diagnostic>,
    headings: &mut BTreeMap<u32, ClockedHeading>,
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
                heading_stack.retain(|(l, _)| *l < level);
                heading_stack.push((level, child.clone()));
                walk(
                    &child,
                    content,
                    line_index,
                    interner,
                    heading_stack,
                    records,
                    diagnostics,
                    headings,
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
                    line_index,
                    heading_stack,
                    interner,
                    records,
                    diagnostics,
                    headings,
                );
            }

            // Paragraph text may hide a malformed CLOCK that orgize
            // refused to promote. Scan its lines, but only here —
            // never inside the literal kinds skipped above.
            SyntaxKind::PARAGRAPH => {
                let start: usize = child.text_range().start().into();
                let end: usize = child.text_range().end().into();
                // Most paragraphs hold no CLOCK at all; one substring
                // scan over the span skips the per-line split for
                // them. The per-line pass only runs on the rare hit.
                if !content[start..end].contains("CLOCK:") {
                    continue;
                }
                // A paragraph's range starts at its first character,
                // which for a list item is AFTER the bullet: the
                // `CLOCK:` token can sit at that offset while the
                // *source line* begins with `  - `. Hand the strict
                // parser the whole physical line, exactly as `check`
                // always did, so a bulleted mention is judged the
                // same way on both paths (it fails the leading
                // `CLOCK:` test and is ignored).
                let mut pos = start;
                for seg in content[start..end].split_inclusive('\n') {
                    if seg.contains("CLOCK:") {
                        let line_start = content[..pos].rfind('\n').map_or(0, |i| i + 1);
                        let line_end = pos + seg.len();
                        let raw = content[line_start..line_end].trim_end_matches(['\n', '\r']);
                        classify(
                            line_start,
                            raw,
                            line_index,
                            heading_stack,
                            interner,
                            records,
                            diagnostics,
                            headings,
                        );
                    }
                    pos += seg.len();
                }
                // Paragraphs contain no elements; nothing to descend into.
            }

            _ => walk(
                &child,
                content,
                line_index,
                interner,
                heading_stack,
                records,
                diagnostics,
                headings,
            ),
        }
    }
}

/// Run the strict grammar on one candidate line and file the result.
#[allow(clippy::too_many_arguments)]
fn classify(
    byte_offset: usize,
    raw: &str,
    line_index: &LineIndex,
    heading_stack: &[(u32, SyntaxNode)],
    interner: &mut Interner,
    records: &mut Vec<Record>,
    diagnostics: &mut Vec<Diagnostic>,
    headings: &mut BTreeMap<u32, ClockedHeading>,
) {
    let line = line_index.line_at(byte_offset) as u32;
    // Intern here, not at headline entry: only clocked headings
    // earn a string-table slot — and only they get their tags and
    // properties extracted, which is the expensive part.
    // Key the record on the heading's *position*, not its title:
    // several headings may share a title (a recurring "réunion du
    // lundi") and each must resolve to its own line and drawer. The
    // interner is asked for a string that no title can collide with.
    let heading_id = match heading_stack.last() {
        None => NO_HEADING,
        Some((level, node)) => {
            let start: usize = node.text_range().start().into();
            let id = interner.intern(&format!("\0h@{}", start));
            headings
                .entry(id)
                .or_insert_with(|| clocked_heading(node, *level, line_index));
            id
        }
    };
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

fn headline_level(node: &SyntaxNode) -> u32 {
    node.children_with_tokens()
        .find_map(|t| match t.kind() {
            SyntaxKind::HEADLINE_STARS => t.as_token().map(|tok| tok.text().len() as u32),
            _ => None,
        })
        .unwrap_or(1)
}

/// Capture what a heading filter and a `HeadingSummary` need,
/// through orgize's typed accessors (same source of truth as
/// [`crate::org::heading::Heading`]).
fn clocked_heading(node: &SyntaxNode, level: u32, line_index: &LineIndex) -> ClockedHeading {
    let start: usize = node.text_range().start().into();
    let line = line_index.line_at(start) as u32;
    let title = headline_title(node);
    let (tags, properties) = match OrgHeadline::cast(node.clone()) {
        Some(h) => (
            h.tags().map(|t| t.to_string()).collect(),
            h.properties()
                .map(|d| {
                    d.iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect::<BTreeMap<_, _>>()
                })
                .unwrap_or_default(),
        ),
        None => (Vec::new(), BTreeMap::new()),
    };
    ClockedHeading {
        level,
        line,
        title,
        tags,
        properties,
    }
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
        let title = |r: &Record| p.headings[&r.heading_id].title.clone();
        assert_eq!(p.blob.records.len(), 2);
        // orgize's HEADLINE_TITLE excludes the TODO keyword, matching
        // the `keyword` / `title` split in `crate::org::heading`.
        assert_eq!(title(&p.blob.records[0]), "The task");
        assert_eq!(title(&p.blob.records[1]), "Other");
        assert_eq!(p.headings[&p.blob.records[0].heading_id].line, 2);
        assert_eq!(p.headings[&p.blob.records[1].heading_id].line, 6);
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

    // Regression: a `CLOCK:` mention inside a list item starts a
    // paragraph AFTER the bullet, so the paragraph slice began with
    // `CLOCK:` and the strict parser accepted it — while the line
    // scanner (which sees `  - CLOCK:`) never did. Both must agree.
    #[test]
    fn bulleted_clock_mention_is_neither_fact_nor_diagnostic() {
        let c = indoc! {"
            * h
              - CLOCK: [2026-01-23 Fri 06:30 +0100]--[2026-01-23 Fri 07:00 +0100] => 0:30 (note)
        "};
        assert!(facts(c).is_empty());
        assert!(diags(c).is_empty(), "{:?}", diags(c));
    }

    #[test]
    fn same_titled_headings_stay_distinct() {
        let c = indoc! {"
            ** DONE réunion du lundi
            CLOCK: [2026-01-05 Mon 09:00 +0100]--[2026-01-05 Mon 09:30 +0100] =>  0:30
            ** DONE réunion du lundi
            CLOCK: [2026-01-12 Mon 09:00 +0100]--[2026-01-12 Mon 09:30 +0100] =>  0:30
        "};
        let p = project(P, c);
        assert_eq!(p.blob.records.len(), 2);
        let (a, b) = (&p.blob.records[0], &p.blob.records[1]);
        assert_ne!(
            a.heading_id, b.heading_id,
            "identical titles must not merge"
        );
        assert_eq!(p.headings[&a.heading_id].line, 1);
        assert_eq!(p.headings[&b.heading_id].line, 3);
    }

    #[test]
    fn candidate_lines_are_source_ordered_and_deduplicated() {
        // Line 4 is a clean fact; line 2 carries TWO violations
        // (weekday on both sides) → two diagnostics, one candidate.
        // Line 6 is in a src block → not a candidate at all.
        let c = indoc! {"
            * h
            CLOCK: [2026-01-07 Mon 10:00 +0100]--[2026-01-07 Mon 11:00 +0100] =>  1:00
            :LOGBOOK:
            CLOCK: [2026-01-07 Wed 12:00 +0100]--[2026-01-07 Wed 13:00 +0100] =>  1:00
            #+begin_src org
            CLOCK: [2026-01-07 Wed 14:00 +0100]--[2026-01-07 Wed 15:00 +0100] =>  1:00
            #+end_src
        "};
        let p = project(P, c);
        assert!(p.diagnostics.len() >= 2, "{:?}", p.diagnostics);
        assert_eq!(p.candidate_lines(), vec![2, 4]);
    }
}
