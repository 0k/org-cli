//! Org-mode structural syntax defects.
//!
//! Owns one fact: whether a file's *block* and *drawer* delimiters
//! balance. This is a per-file property, deliberately unlike
//! [`crate::org::link`], whose defects are a property of the link
//! *graph* and can only be judged across the whole corpus.
//!
//! ## Why a line scanner and not `orgize`
//!
//! The same reason [`crate::org::clock`] scans lines: `orgize`'s parse
//! API is infallible. It answers "what does this text mean?" by
//! returning a tree for any input, so a malformed drawer yields a
//! *smaller* tree rather than an error. That silence is precisely the
//! defect we are hunting — an unterminated `:PROPERTIES:` drawer makes
//! its properties vanish from the parse while every existing command
//! reports a clean run.
//!
//! A defect here is therefore defined against the raw line sequence,
//! which is also what a human sees when told "line 42 is wrong".
//!
//! ## What is a defect
//!
//! Only unambiguous structural breakage, each corroborated against
//! Emacs' own `org-lint`:
//!
//! - an opener with no closer ([`DefectKind::UnterminatedBlock`],
//!   [`DefectKind::UnterminatedDrawer`]),
//! - a closer with no opener ([`DefectKind::StrayBlockEnd`],
//!   [`DefectKind::StrayDrawerEnd`]).
//!
//! Style is not a defect: indentation, unknown `#+begin_` types, and
//! property naming are all left alone.
//!
//! ## Two rules that are easy to get wrong
//!
//! 1. **Lesser blocks require comma escaping.** Inside `src`,
//!    `example`, `export`, `comment`, and `verse`, an unescaped
//!    column-0 `* ` line is a context-free headline: it dissolves the
//!    block, leaving the opener incomplete and the later `#+end_`
//!    stray. Literal payload must write `,* `. Greater blocks
//!    (`quote`, `center`, and custom special blocks) differ: they may
//!    contain ordinary Org elements and nested blocks. Headlines are
//!    context-free, however, and dissolve greater blocks too.
//! 2. **A heading ends a drawer.** Unlike a block, a drawer cannot span
//!    a headline: Emacs stops looking for `:END:` there, which is *why*
//!    the properties are lost. The next headline is the end of the
//!    evidence, so it bounds the defect.

use serde::Serialize;

/// What kind of structural breakage was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DefectKind {
    /// `#+begin_X` with no matching `#+end_X` before end of file.
    UnterminatedBlock,
    /// `#+end_X` with no `#+begin_X` open at that point.
    StrayBlockEnd,
    /// A line starts like `#+end_X` but carries non-whitespace text
    /// after the block name, so Emacs does not accept it as a closer.
    InvalidBlockEnd,
    /// A greater block directly opens another block of the same type;
    /// Org treats the inner opener as incomplete rather than nested.
    InvalidNestedBlock,
    /// A drawer opener with no `:END:` before the next headline or
    /// end of file.
    UnterminatedDrawer,
    /// `:END:` with no drawer open at that point.
    StrayDrawerEnd,
}

impl DefectKind {
    /// Stable machine-readable tag, used by the NUL and NDJSON output
    /// formats and safe to match on in a shell pipeline.
    pub fn tag(&self) -> &'static str {
        match self {
            DefectKind::UnterminatedBlock => "unterminated-block",
            DefectKind::StrayBlockEnd => "stray-block-end",
            DefectKind::InvalidBlockEnd => "invalid-block-end",
            DefectKind::InvalidNestedBlock => "invalid-nested-block",
            DefectKind::UnterminatedDrawer => "unterminated-drawer",
            DefectKind::StrayDrawerEnd => "stray-drawer-end",
        }
    }
}

/// One structural defect, located at the line that *opened* the
/// unbalanced construct (or the stray closer itself).
///
/// The opener is the useful anchor: it is where the author must add the
/// missing terminator, and it stays stable as the body below it grows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyntaxDefect {
    /// 1-based line of the opener (or of the stray closer).
    pub line: usize,
    pub kind: DefectKind,
    /// The construct's name as written, upper-cased for drawers
    /// (`PROPERTIES`, `LOGBOOK`) and lower-cased for blocks (`src`,
    /// `example`). Lets a consumer filter on construct without
    /// re-reading the file.
    pub construct: String,
    /// The opener line, trimmed. Enough to show the user what was
    /// found without opening the file.
    pub raw: String,
    /// Where the scanner gave up looking for the terminator: the
    /// structural line that bounded the search, or `None` when it ran
    /// to end of file. Set for an unterminated drawer or for any block
    /// dissolved by a context-free headline.
    pub bounded_at: Option<usize>,
}

impl SyntaxDefect {
    /// Human-readable one-line explanation, without the location
    /// prefix (the caller owns formatting `file:line:`).
    pub fn message(&self) -> String {
        match self.kind {
            DefectKind::UnterminatedBlock => match self.bounded_at {
                Some(l) => format!(
                    "unterminated `#+begin_{}` block (dissolved by the headline at line {} before `#+end_{}`)",
                    self.construct, l, self.construct
                ),
                None => format!(
                    "unterminated `#+begin_{}` block (no `#+end_{}` before end of file)",
                    self.construct, self.construct
                ),
            },
            DefectKind::StrayBlockEnd => {
                format!(
                    "stray `#+end_{}` with no matching `#+begin_{}`",
                    self.construct, self.construct
                )
            }
            DefectKind::InvalidBlockEnd => format!(
                "invalid `#+end_{}` line (only whitespace may follow the block name)",
                self.construct
            ),
            DefectKind::InvalidNestedBlock => format!(
                "invalid nested `#+begin_{}` (a greater block cannot directly contain its own type)",
                self.construct
            ),
            DefectKind::UnterminatedDrawer => match self.bounded_at {
                Some(l) => format!(
                    "unterminated `:{}:` drawer (no `:END:` before the structural boundary at line {})",
                    self.construct, l
                ),
                None => format!(
                    "unterminated `:{}:` drawer (no `:END:` before end of file)",
                    self.construct
                ),
            },
            DefectKind::StrayDrawerEnd => "stray `:END:` with no drawer open".to_string(),
        }
    }
}

/// True when the line is a headline (`*`, `**`, … followed by a space).
///
/// A bare `*` line with no trailing space is a list bullet or emphasis,
/// not a headline.
fn is_headline(line: &str) -> bool {
    let stars = line.len() - line.trim_start_matches('*').len();
    stars > 0 && line[stars..].starts_with(' ')
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BlockDelimiter {
    Begin(String),
    End(String),
    InvalidEnd(String),
}

/// Parse one block delimiter using Org's name and closing-line grammar.
///
/// Openers may carry whitespace-separated arguments (`#+begin_src
/// rust`). Closers may carry whitespace only: `#+end_src garbage` is
/// an invalid closing line, not a valid end marker.
fn block_delimiter(trimmed: &str) -> Option<BlockDelimiter> {
    let lower = trimmed.strip_prefix("#+")?.to_ascii_lowercase();
    let (is_end, after) = match lower.strip_prefix("begin_") {
        Some(a) => (false, a),
        None => (true, lower.strip_prefix("end_")?),
    };
    let name_len = after.find(char::is_whitespace).unwrap_or(after.len());
    if name_len == 0 {
        return None;
    }
    let (name, rest) = after.split_at(name_len);
    if !is_end {
        return Some(BlockDelimiter::Begin(name.to_string()));
    }
    if rest.trim().is_empty() {
        Some(BlockDelimiter::End(name.to_string()))
    } else {
        Some(BlockDelimiter::InvalidEnd(name.to_string()))
    }
}

/// Whether `name` denotes one of Org's lesser block types.
///
/// Lesser blocks cannot contain elements. In particular, a column-0
/// headline is context-free and dissolves the block. Every other
/// `#+begin_NAME` form is a greater (special) block, which may contain
/// ordinary elements and nested blocks until its matching terminator.
fn is_lesser_block(name: &str) -> bool {
    matches!(name, "src" | "example" | "export" | "comment" | "verse")
}

/// Extract a drawer opener's name, upper-cased.
///
/// A drawer opener is a line whose entire content is `:NAME:`. That
/// shape is also how a valueless node-property is written, so the
/// caller must only consult this when not already inside a drawer.
/// `:END:` is excluded: it is a terminator, never an opener.
fn drawer_opener(trimmed: &str) -> Option<String> {
    let inner = trimmed.strip_prefix(':')?.strip_suffix(':')?;
    if inner.is_empty()
        || !inner
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
    {
        return None;
    }
    if inner.eq_ignore_ascii_case("END") {
        return None;
    }
    Some(inner.to_ascii_uppercase())
}

/// Scan `content` and report every structural defect, ordered by the
/// line that opened each unbalanced construct.
///
/// ```
/// use org_cli::org::syntax::{scan, DefectKind};
///
/// let defects = scan("* H\n:PROPERTIES:\n:A: 1\n* Next\n");
/// assert_eq!(defects.len(), 1);
/// assert_eq!(defects[0].kind, DefectKind::UnterminatedDrawer);
/// assert_eq!(defects[0].line, 2);
///
/// // Comma escaping makes a headline-shaped line literal payload.
/// assert!(scan("#+begin_example\n,* not a heading\n#+end_example\n").is_empty());
/// ```
pub fn scan(content: &str) -> Vec<SyntaxDefect> {
    let mut defects = Vec::new();

    // Open blocks: (type, opening line, raw opener). Greater blocks
    // may contain nested blocks, so this is a stack rather than one
    // opaque state.
    let mut blocks: Vec<(String, usize, String)> = Vec::new();
    // Open drawer: (name, opening line, raw opener).
    let mut drawer: Option<(String, usize, String)> = None;

    for (idx, line) in content.lines().enumerate() {
        let lineno = idx + 1;
        let trimmed = line.trim();

        // A dissolved lesser block needs this same line processed
        // again as ordinary structure, so one physical line may take
        // two passes through the context state machine.
        loop {
            // Headlines are context-free: they dissolve every open
            // block, lesser or greater, one stack frame at a time.
            if is_headline(line)
                && let Some((name, open_line, raw)) = blocks.pop()
            {
                defects.push(SyntaxDefect {
                    line: open_line,
                    kind: DefectKind::UnterminatedBlock,
                    construct: name,
                    raw,
                    bounded_at: Some(lineno),
                });
                continue;
            }

            if let Some((open_type, _, _)) = blocks.last()
                && is_lesser_block(open_type)
            {
                match block_delimiter(trimmed) {
                    Some(BlockDelimiter::End(end_type)) if &end_type == open_type => {
                        blocks.pop();
                        break;
                    }
                    Some(BlockDelimiter::InvalidEnd(end_type)) if &end_type == open_type => {
                        defects.push(SyntaxDefect {
                            line: lineno,
                            kind: DefectKind::InvalidBlockEnd,
                            construct: end_type,
                            raw: trimmed.to_string(),
                            bounded_at: None,
                        });
                        break;
                    }
                    _ => break, // opaque lesser-block payload
                }
            }

            // Drawers are opaque until `:END:` or a headline. Raw
            // closing-block lines inside a broken drawer are still
            // useful lint findings, but cannot close an outer block.
            if drawer.is_some() {
                if trimmed.eq_ignore_ascii_case(":END:") {
                    drawer = None;
                } else if is_headline(line) {
                    let (name, open_line, raw) = drawer.take().expect("drawer is open");
                    defects.push(SyntaxDefect {
                        line: open_line,
                        kind: DefectKind::UnterminatedDrawer,
                        construct: name,
                        raw,
                        bounded_at: Some(lineno),
                    });
                    continue;
                } else {
                    match block_delimiter(trimmed) {
                        Some(BlockDelimiter::End(name))
                            if blocks.last().is_some_and(|(open, _, _)| open == &name) =>
                        {
                            let (drawer_name, open_line, raw) =
                                drawer.take().expect("drawer is open");
                            defects.push(SyntaxDefect {
                                line: open_line,
                                kind: DefectKind::UnterminatedDrawer,
                                construct: drawer_name,
                                raw,
                                bounded_at: Some(lineno),
                            });
                            // Re-process this line as the valid end of
                            // the containing greater block.
                            continue;
                        }
                        Some(BlockDelimiter::End(name)) => defects.push(SyntaxDefect {
                            line: lineno,
                            kind: DefectKind::StrayBlockEnd,
                            construct: name,
                            raw: trimmed.to_string(),
                            bounded_at: None,
                        }),
                        Some(BlockDelimiter::InvalidEnd(name)) => defects.push(SyntaxDefect {
                            line: lineno,
                            kind: DefectKind::InvalidBlockEnd,
                            construct: name,
                            raw: trimmed.to_string(),
                            bounded_at: None,
                        }),
                        _ => {}
                    }
                }
                break;
            }

            match block_delimiter(trimmed) {
                Some(BlockDelimiter::Begin(name)) => {
                    if !is_lesser_block(&name)
                        && blocks.last().is_some_and(|(open, _, _)| open == &name)
                    {
                        defects.push(SyntaxDefect {
                            line: lineno,
                            kind: DefectKind::InvalidNestedBlock,
                            construct: name,
                            raw: trimmed.to_string(),
                            bounded_at: None,
                        });
                    } else {
                        blocks.push((name, lineno, trimmed.to_string()));
                    }
                    break;
                }
                Some(BlockDelimiter::End(name)) => {
                    if blocks.last().is_some_and(|(open, _, _)| open == &name) {
                        blocks.pop();
                    } else {
                        defects.push(SyntaxDefect {
                            line: lineno,
                            kind: DefectKind::StrayBlockEnd,
                            construct: name,
                            raw: trimmed.to_string(),
                            bounded_at: None,
                        });
                    }
                    break;
                }
                Some(BlockDelimiter::InvalidEnd(name)) => {
                    defects.push(SyntaxDefect {
                        line: lineno,
                        kind: DefectKind::InvalidBlockEnd,
                        construct: name,
                        raw: trimmed.to_string(),
                        bounded_at: None,
                    });
                    break;
                }
                None => {}
            }

            if is_headline(line) {
                break;
            }

            if trimmed.eq_ignore_ascii_case(":END:") {
                defects.push(SyntaxDefect {
                    line: lineno,
                    kind: DefectKind::StrayDrawerEnd,
                    construct: "END".to_string(),
                    raw: trimmed.to_string(),
                    bounded_at: None,
                });
                break;
            }

            if let Some(name) = drawer_opener(trimmed) {
                drawer = Some((name, lineno, trimmed.to_string()));
            }
            break;
        }
    }

    for (name, open_line, raw) in blocks {
        defects.push(SyntaxDefect {
            line: open_line,
            kind: DefectKind::UnterminatedBlock,
            construct: name,
            raw,
            bounded_at: None,
        });
    }
    if let Some((name, open_line, raw)) = drawer {
        defects.push(SyntaxDefect {
            line: open_line,
            kind: DefectKind::UnterminatedDrawer,
            construct: name,
            raw,
            bounded_at: None,
        });
    }

    defects.sort_by_key(|d| d.line);
    defects
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    fn kinds(content: &str) -> Vec<DefectKind> {
        scan(content).into_iter().map(|d| d.kind).collect()
    }

    // -----------------------------------------------------------------
    // Clean input must stay clean. These guard against a scanner that
    // reports defects on ordinary documents.
    // -----------------------------------------------------------------

    #[test]
    fn test_scan_accepts_balanced_drawer_and_block() {
        let content = indoc! {"
            * Heading
            :PROPERTIES:
            :AUTHOR: X
            :END:
            body

            #+begin_src rust
            fn main() {}
            #+end_src
        "};
        assert!(scan(content).is_empty(), "{:?}", scan(content));
    }

    #[test]
    fn test_scan_accepts_empty_and_link_only_content() {
        assert!(scan("").is_empty());
        assert!(scan("just prose\n").is_empty());
        assert!(scan("* H\n[[file:a.org][a]]\n").is_empty());
    }

    #[test]
    fn test_scan_accepts_indented_drawer_and_block() {
        // Emacs accepts indented delimiters; indentation is style.
        let content = indoc! {"
            * Heading
              :PROPERTIES:
              :A: 1
              :END:
              #+begin_src sh
              echo hi
              #+end_src
        "};
        assert!(scan(content).is_empty(), "{:?}", scan(content));
    }

    #[test]
    fn test_scan_is_case_insensitive_on_delimiters() {
        let content = indoc! {"
            #+BEGIN_SRC sh
            echo hi
            #+END_SRC
            * H
            :PROPERTIES:
            :A: 1
            :end:
        "};
        assert!(scan(content).is_empty(), "{:?}", scan(content));
    }

    #[test]
    fn test_scan_accepts_comma_escaped_org_syntax_in_lesser_block() {
        // Lesser-block payload that begins with `*` must use Org's
        // comma escape. Org strips the comma when reading the value.
        let content = indoc! {"
            #+begin_example
            ,* This is not a heading
            :PROPERTIES:
            :A: 1
            #+end_example
        "};
        assert!(
            scan(content).is_empty(),
            "block content is literal: {:?}",
            scan(content)
        );
    }

    #[test]
    fn test_scan_unescaped_headline_dissolves_lesser_block() {
        // Emacs parses the headline as a context-free element, not as
        // src payload. The original opener is incomplete; the drawer
        // below is incomplete; and the later end marker is stray.
        let content = indoc! {"
            #+begin_src org
            * Unescaped heading
            :PROPERTIES:
            :A: 1
            #+end_src
        "};
        let d = scan(content);
        assert_eq!(
            d.iter().map(|x| (x.line, x.kind)).collect::<Vec<_>>(),
            vec![
                (1, DefectKind::UnterminatedBlock),
                (3, DefectKind::UnterminatedDrawer),
                (5, DefectKind::StrayBlockEnd),
            ]
        );
        assert_eq!(d[0].bounded_at, Some(2));
        assert!(d[0].message().contains("headline at line 2"));
    }

    #[test]
    fn test_scan_headline_dissolves_greater_block_too() {
        // Greater blocks contain ordinary elements, but headlines are
        // context-free and dissolve them just as they dissolve lesser
        // blocks.
        let content = indoc! {"
            #+begin_quote
            * Headline outside the quote
            #+end_quote
        "};
        assert_eq!(
            scan(content)
                .iter()
                .map(|x| (x.line, x.kind))
                .collect::<Vec<_>>(),
            vec![
                (1, DefectKind::UnterminatedBlock),
                (3, DefectKind::StrayBlockEnd),
            ]
        );
    }

    #[test]
    fn test_scan_reports_malformed_children_inside_greater_block() {
        // Emacs reports all five raw structural failures: both open
        // blocks, the drawer, and both closing lines. A greater block
        // contains ordinary Org elements; it must not suppress checks
        // inside itself.
        let content = indoc! {"
            #+begin_quote
            #+begin_src org
            * Unescaped heading
            :PROPERTIES:
            :A: 1
            #+end_src
            #+end_quote
        "};
        let d = scan(content);
        assert_eq!(
            d.iter().map(|x| (x.line, x.kind)).collect::<Vec<_>>(),
            vec![
                (1, DefectKind::UnterminatedBlock),
                (2, DefectKind::UnterminatedBlock),
                (4, DefectKind::UnterminatedDrawer),
                (6, DefectKind::StrayBlockEnd),
                (7, DefectKind::StrayBlockEnd),
            ]
        );
    }

    #[test]
    fn test_scan_accepts_balanced_children_inside_greater_block() {
        let content = indoc! {"
            #+begin_quote
            prose inside quote
            :PROPERTIES:
            :A: 1
            :END:
            #+begin_src sh
            echo hi
            #+end_src
            #+end_quote
        "};
        assert!(scan(content).is_empty(), "{:?}", scan(content));
    }

    #[test]
    fn test_scan_parent_greater_end_bounds_broken_drawer_then_closes_parent() {
        let content = indoc! {"
            #+begin_quote
            :PROPERTIES:
            :A: 1
            #+end_quote
        "};
        let d = scan(content);
        assert_eq!(
            d.iter().map(|x| (x.line, x.kind)).collect::<Vec<_>>(),
            vec![(2, DefectKind::UnterminatedDrawer)]
        );
        assert_eq!(d[0].bounded_at, Some(4));
    }

    #[test]
    fn test_scan_rejects_direct_same_type_greater_block_nesting() {
        let content = indoc! {"
            #+begin_quote
            #+begin_quote
            inner
            #+end_quote
            #+end_quote
        "};
        assert_eq!(
            scan(content)
                .iter()
                .map(|x| (x.line, x.kind))
                .collect::<Vec<_>>(),
            vec![
                (2, DefectKind::InvalidNestedBlock),
                (5, DefectKind::StrayBlockEnd),
            ]
        );
    }

    #[test]
    fn test_scan_accepts_punctuation_in_custom_block_name() {
        assert!(scan("#+begin_foo.bar\nbody\n#+end_foo.bar\n").is_empty());
        let d = scan("#+begin_foo.bar\nbody\n");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, DefectKind::UnterminatedBlock);
        assert_eq!(d[0].construct, "foo.bar");
    }

    #[test]
    fn test_scan_accepts_unicode_word_drawer_name() {
        assert!(scan("* H\n:ÉTAT:\n:A: 1\n:END:\n").is_empty());
    }

    #[test]
    fn test_scan_does_not_treat_punctuation_property_as_drawer() {
        // Org drawer names use word characters, `-`, and `_`. A
        // punctuation-bearing line is ordinary prose, not an opener.
        assert!(scan("* H\n:not.a.drawer:\n").is_empty());
    }

    #[test]
    fn test_scan_does_not_treat_mismatched_nested_end_as_closing() {
        // `#+end_quote` does not close a `src` block; the src block is
        // still open at EOF and that is the one defect.
        let content = "#+begin_src\n#+end_quote\n";
        assert_eq!(kinds(content), vec![DefectKind::UnterminatedBlock]);
    }

    #[test]
    fn test_scan_ignores_property_lines_that_look_like_drawers() {
        // `:A: 1` has a value, so it is a property, not a drawer
        // opener. Only a bare `:NAME:` line opens a drawer.
        let content = "* H\n:PROPERTIES:\n:A: 1\n:B: 2\n:END:\n";
        assert!(scan(content).is_empty(), "{:?}", scan(content));
    }

    #[test]
    fn test_scan_ignores_bare_star_without_space() {
        // `*bold*` is emphasis, not a headline, so it must not bound
        // the drawer.
        let content = "* H\n:PROPERTIES:\n*emphasis*\n:END:\n";
        assert!(scan(content).is_empty(), "{:?}", scan(content));
    }

    // -----------------------------------------------------------------
    // The two defects this work was filed for.
    // -----------------------------------------------------------------

    #[test]
    fn test_scan_detects_unterminated_drawer_bounded_by_heading() {
        let content = indoc! {"
            * First
            :PROPERTIES:
            :AUTHOR: X
            :SCOPE: important

            ** Second
            body
        "};
        let d = scan(content);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, DefectKind::UnterminatedDrawer);
        assert_eq!(d[0].line, 2, "anchored at the opener");
        assert_eq!(d[0].construct, "PROPERTIES");
        assert_eq!(d[0].bounded_at, Some(6), "the headline that ended it");
    }

    #[test]
    fn test_scan_detects_unterminated_drawer_at_eof() {
        let content = "* H\n:LOGBOOK:\n- a note\n";
        let d = scan(content);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, DefectKind::UnterminatedDrawer);
        assert_eq!(d[0].construct, "LOGBOOK");
        assert_eq!(d[0].bounded_at, None);
    }

    #[test]
    fn test_scan_detects_unterminated_src_block_at_eof() {
        let content = "* H\n#+begin_src rust\nfn main() {}\n";
        let d = scan(content);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, DefectKind::UnterminatedBlock);
        assert_eq!(d[0].line, 2);
        assert_eq!(d[0].construct, "src");
        assert_eq!(d[0].raw, "#+begin_src rust");
    }

    #[test]
    fn test_scan_detects_stray_terminators() {
        assert_eq!(kinds("#+end_src\n"), vec![DefectKind::StrayBlockEnd]);
        assert_eq!(kinds("* H\n:END:\n"), vec![DefectKind::StrayDrawerEnd]);
    }

    #[test]
    fn test_scan_reports_every_defect_in_line_order() {
        let content = indoc! {"
            * One
            :PROPERTIES:
            :A: 1

            * Two
            :LOGBOOK:
            - note

            * Three
            #+begin_src sh
        "};
        let d = scan(content);
        assert_eq!(
            d.iter().map(|x| (x.line, x.kind)).collect::<Vec<_>>(),
            vec![
                (2, DefectKind::UnterminatedDrawer),
                (6, DefectKind::UnterminatedDrawer),
                (10, DefectKind::UnterminatedBlock),
            ]
        );
    }

    #[test]
    fn test_scan_recovers_after_a_defect() {
        // A bounded unterminated drawer must not swallow the rest of
        // the file: the balanced drawer below is still clean.
        let content = indoc! {"
            * One
            :PROPERTIES:
            :A: 1

            * Two
            :PROPERTIES:
            :B: 2
            :END:
            body
        "};
        let d = scan(content);
        assert_eq!(d.len(), 1, "only the first drawer is broken: {:?}", d);
        assert_eq!(d[0].line, 2);
    }

    #[test]
    fn test_defect_kind_tags_are_stable() {
        assert_eq!(DefectKind::UnterminatedBlock.tag(), "unterminated-block");
        assert_eq!(DefectKind::StrayBlockEnd.tag(), "stray-block-end");
        assert_eq!(DefectKind::InvalidBlockEnd.tag(), "invalid-block-end");
        assert_eq!(DefectKind::InvalidNestedBlock.tag(), "invalid-nested-block");
        assert_eq!(DefectKind::UnterminatedDrawer.tag(), "unterminated-drawer");
        assert_eq!(DefectKind::StrayDrawerEnd.tag(), "stray-drawer-end");
    }

    #[test]
    fn test_message_names_the_bounding_line_when_known() {
        let d = scan("* H\n:PROPERTIES:\n:A: 1\n* Next\n");
        assert!(
            d[0].message().contains("boundary at line 4"),
            "got: {}",
            d[0].message()
        );
    }
}
