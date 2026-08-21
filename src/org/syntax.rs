//! Org-mode structural syntax defects.
//!
//! Owns one fact: whether a file's blocks and drawers follow Emacs
//! Org's structural grammar. This is a per-file property, deliberately unlike
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
//!   [`DefectKind::StrayDrawerEnd`]),
//! - a misplaced or malformed property drawer
//!   ([`DefectKind::MisplacedPropertyDrawer`],
//!   [`DefectKind::InvalidPropertyDrawerContent`]),
//! - forbidden drawer containment ([`DefectKind::NestedDrawer`],
//!   [`DefectKind::HeadlineInDrawer`]).
//!
//! Style is not a defect: indentation, unknown `#+begin_` types, and
//! property-key spelling and case are left alone.
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

use crate::org::property::parse_property_line;

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
    /// `:PROPERTIES:` is closed but does not occupy the privileged
    /// position immediately after a heading and optional planning line.
    MisplacedPropertyDrawer,
    /// A recognized property drawer contains a non-property line.
    InvalidPropertyDrawerContent,
    /// A generic drawer contains another drawer opener.
    NestedDrawer,
    /// A context-free headline interrupts a generic drawer.
    HeadlineInDrawer,
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
            DefectKind::MisplacedPropertyDrawer => "misplaced-property-drawer",
            DefectKind::InvalidPropertyDrawerContent => "invalid-property-drawer-content",
            DefectKind::NestedDrawer => "nested-drawer",
            DefectKind::HeadlineInDrawer => "headline-in-drawer",
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
            DefectKind::MisplacedPropertyDrawer =>
                "misplaced `:PROPERTIES:` drawer (must immediately follow a heading and optional planning line)".to_string(),
            DefectKind::InvalidPropertyDrawerContent => match self.bounded_at {
                Some(l) => format!(
                    "invalid `:PROPERTIES:` drawer content at line {} (only node-property lines are allowed)",
                    l
                ),
                None => "invalid `:PROPERTIES:` drawer content (only node-property lines are allowed)".to_string(),
            },
            DefectKind::NestedDrawer => format!(
                "nested `:{}` drawer (generic drawers cannot contain another drawer)",
                self.construct
            ),
            DefectKind::HeadlineInDrawer => match self.bounded_at {
                Some(l) => format!(
                    "headline at line {} interrupts `:{}` drawer (drawers cannot contain headlines)",
                    l, self.construct
                ),
                None => format!("headline interrupts `:{}` drawer", self.construct),
            },
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
    if rest.trim_matches([' ', '\t']).is_empty() {
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
    Some(inner.to_uppercase())
}

fn is_planning_line(trimmed: &str) -> bool {
    let mut rest = trimmed;
    let mut found = false;
    while let Some(keyword_len) = ["CLOSED:", "DEADLINE:", "SCHEDULED:"]
        .into_iter()
        .find(|keyword| rest.to_ascii_uppercase().starts_with(keyword))
        .map(str::len)
    {
        let after_keyword = rest[keyword_len..].trim_start_matches([' ', '\t']);
        let Some(open) = after_keyword.chars().next() else {
            return false;
        };
        let close = match open {
            '[' => ']',
            '<' => '>',
            _ => return false,
        };
        let Some(end) = after_keyword.find(close) else {
            return false;
        };
        found = true;
        rest = after_keyword[end + close.len_utf8()..].trim_start_matches([' ', '\t']);
    }
    found && rest.is_empty()
}

fn is_root_comment(trimmed: &str) -> bool {
    trimmed.starts_with("# ") || trimmed == "#"
}

fn has_future_drawer_end_before_headline(lines: &[&str], start: usize) -> bool {
    for line in &lines[start..] {
        let trimmed = line.trim_matches([' ', '\t']);
        if is_headline(line) {
            return false;
        }
        if trimmed.eq_ignore_ascii_case(":END:") {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrawerKind {
    Generic,
    MisplacedProperty,
    RecoveredProperty,
    Property,
}

#[derive(Debug, Clone)]
struct OpenDrawer {
    name: String,
    line: usize,
    raw: String,
    kind: DrawerKind,
    /// Block stack depth when the drawer opened. A matching block end
    /// above this depth closes a block contained by the drawer; at this
    /// depth it bounds an unterminated drawer inside a greater block.
    block_depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionPrefix {
    Root,
    RootComment,
    Heading,
    Planning,
    Body,
}

/// Scan `content` and report every structural defect, ordered by the
/// line that opened each unbalanced construct.
///
/// ```
/// use org_cli::org::syntax::{scan, DefectKind};
///
/// let defects = scan("* H\n:PROPERTIES:\n:A: 1\n* Next\n");
/// assert_eq!(defects.len(), 1);
/// assert_eq!(defects[0].kind, DefectKind::InvalidPropertyDrawerContent);
/// assert_eq!(defects[0].line, 2);
///
/// // Comma escaping makes a headline-shaped line literal payload.
/// assert!(scan("#+begin_example\n,* not a heading\n#+end_example\n").is_empty());
/// ```
pub fn scan(content: &str) -> Vec<SyntaxDefect> {
    let mut defects = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    // Open blocks: (type, opening line, raw opener). Greater blocks
    // may contain nested blocks, so this is a stack rather than one
    // opaque state.
    let mut blocks: Vec<(String, usize, String)> = Vec::new();
    let mut drawer: Option<OpenDrawer> = None;
    let mut section_prefix = SectionPrefix::Root;

    for (idx, line) in lines.iter().copied().enumerate() {
        let lineno = idx + 1;
        let trimmed = line.trim_matches([' ', '\t']);

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

            if matches!(drawer.as_ref().map(|d| d.kind), Some(DrawerKind::Property)) {
                if trimmed.eq_ignore_ascii_case(":END:") {
                    drawer = None;
                    section_prefix = SectionPrefix::Body;
                } else if is_headline(line) {
                    let open = drawer.take().expect("property drawer is open");
                    defects.push(SyntaxDefect {
                        line: open.line,
                        kind: DefectKind::InvalidPropertyDrawerContent,
                        construct: open.name,
                        raw: open.raw,
                        bounded_at: Some(lineno),
                    });
                    continue;
                } else if parse_property_line(line).is_none() {
                    let open = drawer.as_mut().expect("property drawer is open");
                    defects.push(SyntaxDefect {
                        line: open.line,
                        kind: DefectKind::InvalidPropertyDrawerContent,
                        construct: open.name.clone(),
                        raw: open.raw.clone(),
                        bounded_at: Some(lineno),
                    });
                    // Emacs falls back to a generic drawer when the
                    // privileged property grammar fails. Re-process
                    // the offending line under that recovery grammar
                    // so balanced blocks inside it stay balanced.
                    open.kind = DrawerKind::RecoveredProperty;
                    continue;
                }
                break;
            }

            if drawer.is_some() {
                if trimmed.eq_ignore_ascii_case(":END:") {
                    drawer = None;
                    break;
                }
                if is_headline(line) {
                    let open = drawer.take().expect("generic drawer is open");
                    defects.push(SyntaxDefect {
                        line: open.line,
                        kind: DefectKind::HeadlineInDrawer,
                        construct: open.name,
                        raw: open.raw,
                        bounded_at: Some(lineno),
                    });
                    continue;
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
                    Some(BlockDelimiter::End(name))
                        if blocks.last().is_some_and(|(open, _, _)| open == &name) =>
                    {
                        let depth = drawer.as_ref().expect("generic drawer is open").block_depth;
                        if blocks.len() > depth {
                            blocks.pop();
                            break;
                        }
                        let open = drawer.take().expect("generic drawer is open");
                        defects.push(SyntaxDefect {
                            line: open.line,
                            kind: DefectKind::UnterminatedDrawer,
                            construct: open.name,
                            raw: open.raw,
                            bounded_at: Some(lineno),
                        });
                        continue;
                    }
                    Some(BlockDelimiter::End(name)) => {
                        defects.push(SyntaxDefect {
                            line: lineno,
                            kind: DefectKind::StrayBlockEnd,
                            construct: name,
                            raw: trimmed.to_string(),
                            bounded_at: None,
                        });
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

                if matches!(drawer.as_ref().map(|d| d.kind), Some(DrawerKind::Generic))
                    && let Some(name) = drawer_opener(trimmed)
                {
                    defects.push(SyntaxDefect {
                        line: lineno,
                        kind: DefectKind::NestedDrawer,
                        construct: name,
                        raw: trimmed.to_string(),
                        bounded_at: None,
                    });
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
                    section_prefix = SectionPrefix::Body;
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
                    section_prefix = SectionPrefix::Body;
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
                    section_prefix = SectionPrefix::Body;
                    break;
                }
                None => {}
            }

            if is_headline(line) {
                section_prefix = SectionPrefix::Heading;
                break;
            }

            if trimmed.is_empty() {
                if matches!(
                    section_prefix,
                    SectionPrefix::RootComment | SectionPrefix::Heading | SectionPrefix::Planning
                ) {
                    section_prefix = SectionPrefix::Body;
                }
                break;
            }

            if section_prefix == SectionPrefix::Heading && is_planning_line(trimmed) {
                section_prefix = SectionPrefix::Planning;
                break;
            }

            if section_prefix == SectionPrefix::Planning && is_planning_line(trimmed) {
                section_prefix = SectionPrefix::Body;
                break;
            }

            if matches!(
                section_prefix,
                SectionPrefix::Root | SectionPrefix::RootComment
            ) && is_root_comment(trimmed)
            {
                section_prefix = SectionPrefix::RootComment;
                break;
            }

            if trimmed.eq_ignore_ascii_case(":END:") {
                if has_future_drawer_end_before_headline(&lines, idx + 1) {
                    drawer = Some(OpenDrawer {
                        name: "END".to_string(),
                        line: lineno,
                        raw: trimmed.to_string(),
                        kind: DrawerKind::Generic,
                        block_depth: blocks.len(),
                    });
                    section_prefix = SectionPrefix::Body;
                } else {
                    defects.push(SyntaxDefect {
                        line: lineno,
                        kind: DefectKind::StrayDrawerEnd,
                        construct: "END".to_string(),
                        raw: trimmed.to_string(),
                        bounded_at: None,
                    });
                }
                break;
            }

            if let Some(name) = drawer_opener(trimmed) {
                let kind = if name.eq_ignore_ascii_case("PROPERTIES") {
                    if matches!(
                        section_prefix,
                        SectionPrefix::Root
                            | SectionPrefix::RootComment
                            | SectionPrefix::Heading
                            | SectionPrefix::Planning
                    ) {
                        DrawerKind::Property
                    } else {
                        defects.push(SyntaxDefect {
                            line: lineno,
                            kind: DefectKind::MisplacedPropertyDrawer,
                            construct: name.clone(),
                            raw: trimmed.to_string(),
                            bounded_at: None,
                        });
                        DrawerKind::MisplacedProperty
                    }
                } else {
                    DrawerKind::Generic
                };
                drawer = Some(OpenDrawer {
                    name,
                    line: lineno,
                    raw: trimmed.to_string(),
                    kind,
                    block_depth: blocks.len(),
                });
                section_prefix = SectionPrefix::Body;
                break;
            }

            section_prefix = SectionPrefix::Body;
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
    if let Some(open) = drawer {
        defects.push(SyntaxDefect {
            line: open.line,
            kind: DefectKind::UnterminatedDrawer,
            construct: open.name,
            raw: open.raw,
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

    fn tag_lines(content: &str) -> Vec<(usize, &'static str)> {
        scan(content)
            .into_iter()
            .map(|d| (d.line, d.kind.tag()))
            .collect()
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
                (3, DefectKind::InvalidPropertyDrawerContent),
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
                (4, DefectKind::InvalidPropertyDrawerContent),
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
            :LOGBOOK:
            - note
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
            :LOGBOOK:
            - note
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
    fn test_scan_rejects_non_property_markup_inside_property_drawer() {
        // `*bold*` is not a headline, but it is still not a node
        // property and therefore invalid inside `:PROPERTIES:`.
        let content = "* H\n:PROPERTIES:\n*emphasis*\n:END:\n";
        assert_eq!(
            tag_lines(content),
            vec![(2, "invalid-property-drawer-content")]
        );
    }

    // -----------------------------------------------------------------
    // Drawer semantics. These follow Emacs Org, not orgize's narrower
    // generic-drawer name grammar or its silent fallback behavior.
    // -----------------------------------------------------------------

    #[test]
    fn test_scan_accepts_properties_then_logbook_with_optional_planning() {
        let content = indoc! {"
            * Direct
            :PROPERTIES:
            :A: one
            :END:
            :LOGBOOK:
            - note
            :END:

            * With planning
            CLOSED: [2026-08-20 Thu]
            :PROPERTIES:
            :A: two
            :END:

            :LOGBOOK:
            - note
            :END:
        "};
        assert!(scan(content).is_empty(), "{:?}", scan(content));
    }

    #[test]
    fn test_scan_accepts_case_insensitive_single_planning_line() {
        let content = indoc! {"
            * H
            scheduled: <2026-08-20 Thu>
            :PROPERTIES:
            :A: one
            :END:
        "};
        assert!(scan(content).is_empty(), "{:?}", scan(content));
    }

    #[test]
    fn test_scan_rejects_properties_after_second_planning_line() {
        let content = indoc! {"
            * H
            SCHEDULED: <2026-08-20 Thu>
            DEADLINE: <2026-08-21 Fri>
            :PROPERTIES:
            :A: one
            :END:
        "};
        assert_eq!(tag_lines(content), vec![(4, "misplaced-property-drawer")]);
    }

    #[test]
    fn test_scan_root_properties_allow_one_contiguous_comment_element() {
        let valid = indoc! {"
            # comment one
            # comment two
            :PROPERTIES:
            :A: one
            :END:
        "};
        assert!(scan(valid).is_empty(), "{:?}", scan(valid));

        let separated = indoc! {"
            # first comment group

            # second comment group
            :PROPERTIES:
            :A: two
            :END:
        "};
        assert_eq!(tag_lines(separated), vec![(4, "misplaced-property-drawer")]);
    }

    #[test]
    fn test_scan_reports_every_misplaced_property_drawer() {
        let content = indoc! {"
            * Blank before properties

            :PROPERTIES:
            :A: one
            :END:

            * Logbook before properties
            :LOGBOOK:
            - note
            :END:
            :PROPERTIES:
            :A: two
            :END:

            * Duplicate properties
            :PROPERTIES:
            :A: three
            :END:
            :PROPERTIES:
            :B: four
            :END:
        "};
        assert_eq!(
            tag_lines(content),
            vec![
                (3, "misplaced-property-drawer"),
                (11, "misplaced-property-drawer"),
                (19, "misplaced-property-drawer"),
            ]
        );
    }

    #[test]
    fn test_scan_reports_invalid_property_drawer_contents_once_per_drawer() {
        let content = indoc! {"
            * Blank inside
            :PROPERTIES:
            :A: one

            :END:

            * Prose inside
            :PROPERTIES:
            not a property
            :END:

            * Block inside
            :PROPERTIES:
            #+begin_src sh
            echo hi
            #+end_src
            :END:

            * Drawer inside
            :PROPERTIES:
            :LOGBOOK:
            - note
            :END:
            :END:
        "};
        assert_eq!(
            tag_lines(content),
            vec![
                (2, "invalid-property-drawer-content"),
                (8, "invalid-property-drawer-content"),
                (13, "invalid-property-drawer-content"),
                (20, "invalid-property-drawer-content"),
                (24, "stray-drawer-end"),
            ]
        );
    }

    #[test]
    fn test_scan_accepts_blocks_inside_generic_drawer() {
        let content = indoc! {"
            :LOGBOOK:
            - note
            #+begin_quote
            quoted
            #+end_quote
            :END:
        "};
        assert!(scan(content).is_empty(), "{:?}", scan(content));
    }

    #[test]
    fn test_scan_reports_headline_inside_generic_drawer() {
        let content = indoc! {"
            :LOGBOOK:
            - note
            * Heading
            :END:
        "};
        assert_eq!(
            tag_lines(content),
            vec![(1, "headline-in-drawer"), (4, "stray-drawer-end")]
        );
    }

    #[test]
    fn test_scan_reports_nested_generic_drawer() {
        let content = indoc! {"
            :OUTER:
            :INNER:
            :END:
            :END:
        "};
        assert_eq!(
            tag_lines(content),
            vec![(2, "nested-drawer"), (4, "stray-drawer-end")]
        );
    }

    #[test]
    fn test_scan_accepts_paired_drawer_named_end() {
        assert!(scan(":END:\ncontent\n:END:\n").is_empty());
    }

    #[test]
    fn test_scan_treats_punctuation_bearing_colon_line_as_prose() {
        assert!(scan(":BAD.NAME:\n").is_empty());
        assert_eq!(
            tag_lines(":BAD.NAME:\n:END:\n"),
            vec![(2, "stray-drawer-end")]
        );
    }

    #[test]
    fn test_scan_recovered_property_does_not_invent_nested_drawer() {
        let content = indoc! {"
            * H
            :PROPERTIES:
            bad content
            :EMPTY:
            :END:
        "};
        assert_eq!(
            tag_lines(content),
            vec![(2, "invalid-property-drawer-content")]
        );
    }

    #[test]
    fn test_scan_does_not_accept_unicode_drawer_indentation() {
        assert_eq!(
            tag_lines("\u{a0}:LOGBOOK:\n:END:\n"),
            vec![(2, "stray-drawer-end")]
        );
    }

    #[test]
    fn test_scan_normalizes_unicode_drawer_name_in_diagnostic() {
        let defects = scan(":état:\n* Heading\n");
        assert_eq!(defects[0].construct, "ÉTAT");
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
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].kind, DefectKind::InvalidPropertyDrawerContent);
        assert_eq!(d[0].line, 2, "anchored at the opener");
        assert_eq!(d[0].construct, "PROPERTIES");
        assert_eq!(d[0].bounded_at, Some(5), "the blank line invalidated it");
        assert_eq!(d[1].kind, DefectKind::HeadlineInDrawer);
        assert_eq!(d[1].bounded_at, Some(6), "the headline ended recovery");
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
                (2, DefectKind::InvalidPropertyDrawerContent),
                (2, DefectKind::HeadlineInDrawer),
                (6, DefectKind::HeadlineInDrawer),
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
        assert_eq!(d.len(), 2, "only the first drawer is broken: {:?}", d);
        assert_eq!(d[0].line, 2);
        assert_eq!(d[1].line, 2);
    }

    #[test]
    fn test_defect_kind_tags_are_stable() {
        assert_eq!(DefectKind::UnterminatedBlock.tag(), "unterminated-block");
        assert_eq!(DefectKind::StrayBlockEnd.tag(), "stray-block-end");
        assert_eq!(DefectKind::InvalidBlockEnd.tag(), "invalid-block-end");
        assert_eq!(DefectKind::InvalidNestedBlock.tag(), "invalid-nested-block");
        assert_eq!(DefectKind::UnterminatedDrawer.tag(), "unterminated-drawer");
        assert_eq!(DefectKind::StrayDrawerEnd.tag(), "stray-drawer-end");
        assert_eq!(
            DefectKind::MisplacedPropertyDrawer.tag(),
            "misplaced-property-drawer"
        );
        assert_eq!(
            DefectKind::InvalidPropertyDrawerContent.tag(),
            "invalid-property-drawer-content"
        );
        assert_eq!(DefectKind::NestedDrawer.tag(), "nested-drawer");
        assert_eq!(DefectKind::HeadlineInDrawer.tag(), "headline-in-drawer");
    }

    #[test]
    fn test_message_names_the_bounding_line_when_known() {
        let d = scan("* H\n:LOGBOOK:\n- note\n* Next\n");
        assert!(
            d[0].message().contains("headline at line 4"),
            "got: {}",
            d[0].message()
        );
    }
}
