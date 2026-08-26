use std::collections::BTreeMap;
use std::ops::Range;
use std::str::FromStr;

use orgize::{
    ParseConfig,
    ast::Headline as OrgHeadline,
    rowan::{TextSize, ast::AstNode},
};

use crate::org::snapshot::sha256_hex;
use crate::org::todo_keywords::TodoKeywords;

/// A heading extracted from an org-mode file, including its property drawer.
///
/// `fingerprint` is not unique: identical heading lines have identical
/// fingerprints. The snapshot-scoped locator is `(path, line, fingerprint,
/// etag)`, never the fingerprint alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    pub level: u32,
    pub line: usize,
    /// The org-mode TODO keyword (e.g. `TODO`, `DONE`), if present.
    ///
    /// Which words count as keywords depends on the [`TodoKeywords`] set
    /// passed to [`iter_headings_with`]. The plain [`iter_headings`]
    /// wrapper uses the default set (`TODO`/`DONE`); a custom set — from
    /// an in-buffer `#+TODO:` declaration or a CLI spec — extends this to
    /// recognize words like `WIP` or `CANCELED`. `None` when the heading
    /// carries no recognized keyword.
    pub keyword: Option<String>,
    /// The heading's priority as the bare character, e.g. `A` rather than
    /// the `[#A]` cookie. `None` means not yet evaluated, not low priority.
    pub priority: Option<String>,
    /// Lowercase hex SHA-256 of this exact heading line, terminator excluded.
    pub fingerprint: String,
    pub title: String,
    pub tags: Vec<String>,
    pub properties: BTreeMap<String, String>,
}

/// Precomputed newline positions enabling O(log n) line-number lookups.
///
/// Building the index is a single linear scan over the content; each
/// subsequent query is a binary search. This replaces the naive
/// count-newlines-from-the-start approach, which was O(bytes) per query and
/// quadratic when resolving line numbers for every heading in a document.
///
/// Offsets are byte offsets into the original content; returned line numbers
/// are 1-based.
struct LineIndex {
    /// Byte offsets of every `\n` in the content, in ascending order.
    newline_offsets: Vec<usize>,
}

impl LineIndex {
    /// Build an index over `content` by recording the byte offset of every
    /// newline.
    fn new(content: &str) -> Self {
        let newline_offsets = content
            .bytes()
            .enumerate()
            .filter(|(_, byte)| *byte == b'\n')
            .map(|(offset, _)| offset)
            .collect();
        Self { newline_offsets }
    }

    /// Return the 1-based line number containing byte `offset`.
    ///
    /// An offset that lands exactly on a `\n` byte belongs to the line that
    /// the newline terminates (i.e. the line before the break). Offsets past
    /// the end of the content resolve to the final line without panicking.
    fn line_at(&self, offset: usize) -> usize {
        self.newline_offsets.partition_point(|&pos| pos < offset) + 1
    }

    /// Return the byte range of 1-based `line`, excluding its terminator.
    ///
    /// A trailing LF is excluded. When that LF is immediately preceded by
    /// CR, the full CRLF pair is excluded. A lone CR without LF remains data.
    fn line_range(&self, content: &str, line: usize) -> Option<Range<usize>> {
        if line == 0 {
            return None;
        }
        let start = if line == 1 {
            0
        } else {
            self.newline_offsets.get(line - 2)?.saturating_add(1)
        };
        if start > content.len() {
            return None;
        }
        let end = match self.newline_offsets.get(line - 1).copied() {
            Some(newline) if newline > start && content.as_bytes()[newline - 1] == b'\r' => {
                newline - 1
            }
            Some(newline) => newline,
            None => content.len(),
        };
        Some(start..end)
    }
}

/// Walk every heading in `content`, in document order.
///
/// Property keys are preserved exactly as written. Lookups against
/// `Heading::properties` are therefore case-sensitive, matching orgize's
/// `Headline::properties().get(key)` behavior.
///
/// Uses the default TODO keyword set (`TODO`/`DONE`). Call
/// [`iter_headings_with`] to recognize custom keywords.
pub fn iter_headings(content: &str) -> impl Iterator<Item = Heading> + '_ {
    iter_headings_with(content, &TodoKeywords::default())
}

/// Walk every heading in `content`, recognizing `keywords` as TODO
/// states.
///
/// Behaves like [`iter_headings`] but builds a [`ParseConfig`] whose
/// `todo_keywords` are taken from `keywords`, so custom active/done
/// words (e.g. `WIP`, `CANCELED`) populate [`Heading::keyword`] instead
/// of being folded into the title.
pub fn iter_headings_with<'a>(
    content: &'a str,
    keywords: &TodoKeywords,
) -> impl Iterator<Item = Heading> + use<'a> {
    let config = ParseConfig {
        todo_keywords: (keywords.active.clone(), keywords.done.clone()),
        ..Default::default()
    };
    let org = config.parse(content);
    let line_index = LineIndex::new(content);
    org.document()
        .syntax()
        .descendants()
        .filter_map(OrgHeadline::cast)
        .filter_map(move |headline| build_heading(content, &line_index, headline))
        .collect::<Vec<_>>()
        .into_iter()
}

/// Return the enclosing heading for a given 1-based line number, if any.
///
/// The containing heading is the most recent heading whose starting line is
/// less than or equal to `line`, up to (but not including) the next heading at
/// the same or shallower level.
pub fn heading_for_line(content: &str, line: usize) -> Option<Heading> {
    if line == 0 {
        return None;
    }

    let headings: Vec<_> = iter_headings(content).collect();
    for (index, heading) in headings.iter().enumerate().rev() {
        let end_line = headings
            .iter()
            .skip(index + 1)
            .find(|next| next.level <= heading.level)
            .map(|next| next.line.saturating_sub(1))
            .unwrap_or(usize::MAX);

        if heading.line <= line && line <= end_line {
            return Some(heading.clone());
        }
    }

    None
}

fn build_heading(content: &str, line_index: &LineIndex, headline: OrgHeadline) -> Option<Heading> {
    let level = u32::try_from(headline.level()).ok()?;
    let line = line_index.line_at(byte_offset(content, headline.start()));
    let keyword = headline.todo_keyword().map(|token| token.to_string());
    let priority = headline.priority().map(|token| token.to_string());
    let fingerprint = sha256_hex(
        content
            .get(line_index.line_range(content, line)?)?
            .as_bytes(),
    );
    let title = headline.title_raw().trim().to_string();
    let tags = headline.tags().map(|tag| tag.to_string()).collect();
    let properties = headline
        .properties()
        .map(|drawer| {
            drawer
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    Some(Heading {
        level,
        line,
        keyword,
        priority,
        fingerprint,
        title,
        tags,
        properties,
    })
}

/// Convert an orgize `TextSize` to a clamped byte offset into `content`.
///
/// Defensively clamps to `content.len()` so an out-of-range offset can never
/// index past the end of the string.
fn byte_offset(content: &str, offset: TextSize) -> usize {
    let offset_u32: u32 = offset.into();
    usize::try_from(offset_u32)
        .unwrap_or(content.len())
        .min(content.len())
}

// ---------------------------------------------------------------------
// Span primitives
// ---------------------------------------------------------------------

/// A 1-based, inclusive line range within an org document.
///
/// `start` and `end` are both 1-based line numbers; a single-line span
/// has `start == end`. Produced by [`subtree_span`] and [`section_span`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubtreeSpan {
    /// 1-based line number of the first line (always the heading line).
    pub start: usize,
    /// 1-based line number of the last line, inclusive.
    pub end: usize,
}

/// Count the content lines of `content` as 1-based line numbers.
///
/// A trailing newline does not add a phantom empty line: both `"a\nb\n"`
/// and `"a\nb"` report `2`. Empty content reports `0`.
fn total_lines(content: &str) -> usize {
    content.lines().count()
}

/// Return the full subtree span of the heading at `heading_line` (1-based).
///
/// The span runs from the heading line down to the last line *before* the
/// next heading of the same or a shallower level — i.e. it includes the
/// heading's own body plus every descendant heading and their bodies. When
/// no such following heading exists, the span extends to the last line of
/// the file.
///
/// Returns `None` if `heading_line` does not point at a heading line.
pub fn subtree_span(content: &str, heading_line: usize) -> Option<SubtreeSpan> {
    let headings: Vec<Heading> = iter_headings(content).collect();
    let index = headings.iter().position(|h| h.line == heading_line)?;
    let level = headings[index].level;

    let end = headings
        .iter()
        .skip(index + 1)
        .find(|next| next.level <= level)
        .map(|next| next.line.saturating_sub(1))
        .unwrap_or_else(|| total_lines(content));

    Some(SubtreeSpan {
        start: heading_line,
        end,
    })
}

/// Return the *section* span of the heading at `heading_line` (1-based):
/// the heading line plus its own body, stopping before the first child
/// heading.
///
/// Unlike [`subtree_span`], this excludes descendant headings. The span
/// runs from the heading line to the last line before the *next heading of
/// any level* — whether a deeper child or a same/shallower sibling. When no
/// following heading exists, it extends to the last line of the file.
///
/// Returns `None` if `heading_line` does not point at a heading line.
pub fn section_span(content: &str, heading_line: usize) -> Option<SubtreeSpan> {
    let headings: Vec<Heading> = iter_headings(content).collect();
    let index = headings.iter().position(|h| h.line == heading_line)?;

    let end = headings
        .get(index + 1)
        .map(|next| next.line.saturating_sub(1))
        .unwrap_or_else(|| total_lines(content));

    Some(SubtreeSpan {
        start: heading_line,
        end,
    })
}

/// Return the exact source text of `span`, terminators included.
///
/// The slice starts at the first byte of `span.start` and ends after the
/// line terminator of `span.end` (or at end-of-content when the last line
/// is unterminated). It is therefore a *verbatim* substring of `content`:
/// re-parsing it yields the same nodes the whole document produced for
/// those lines, with byte offsets rebased to the slice.
///
/// Returns `None` when the span is empty or out of range (`start == 0`,
/// or `end < start`).
pub fn span_text(content: &str, span: SubtreeSpan) -> Option<&str> {
    if span.start == 0 || span.end < span.start {
        return None;
    }
    let index = LineIndex::new(content);
    let start = index.line_range(content, span.start)?.start;
    let end = match index.newline_offsets.get(span.end - 1) {
        // Include the terminator: the slice must remain a faithful
        // substring, and a body whose final newline was trimmed would
        // re-parse with its last element glued to whatever follows.
        Some(&newline) => newline.saturating_add(1).min(content.len()),
        None => content.len(),
    };
    if end <= start {
        return None;
    }
    Some(&content[start..end])
}

/// Return the *internal content* of the heading at `heading_line`: every
/// line of its subtree except the heading line itself.
///
/// This is the subtree minus its own first line — planning, drawers, body
/// text *and* descendant headings. It is deliberately not
/// [`section_span`] minus one line: a heading's internal content includes
/// its children, which is what a reader expanding a task expects to see.
///
/// Returns `None` when `heading_line` is not a heading line, and
/// `Some("")` when the heading has no content of its own.
pub fn heading_body(content: &str, heading_line: usize) -> Option<&str> {
    let span = subtree_span(content, heading_line)?;
    if span.end <= span.start {
        return Some("");
    }
    Some(
        span_text(
            content,
            SubtreeSpan {
                start: span.start + 1,
                end: span.end,
            },
        )
        .unwrap_or(""),
    )
}

// ---------------------------------------------------------------------
// Heading paths
// ---------------------------------------------------------------------

/// A `/`-separated path of exact heading titles addressing one heading.
///
/// Each segment is matched against [`Heading::title`] with an exact string
/// comparison (the parser has already stripped the TODO keyword and tags
/// from the title). A literal `/` inside a title is written `\/`; a literal
/// backslash is written `\\`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadingPath(pub Vec<String>);

impl FromStr for HeadingPath {
    type Err = String;

    /// Parse a `/`-separated path, honoring `\/` and `\\` escapes.
    ///
    /// Errors on an empty path or on any empty segment (e.g. `a//b`,
    /// a leading `/`, or a trailing `/`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err("empty heading path".to_string());
        }

        let mut segments: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some(escaped @ ('/' | '\\')) => current.push(escaped),
                    Some(other) => {
                        // Preserve unknown escapes verbatim (backslash +
                        // the following character) rather than silently
                        // swallowing the backslash.
                        current.push('\\');
                        current.push(other);
                    }
                    None => return Err("trailing backslash in heading path".to_string()),
                },
                '/' => {
                    segments.push(std::mem::take(&mut current));
                }
                other => current.push(other),
            }
        }
        segments.push(current);

        if segments.iter().any(|seg| seg.is_empty()) {
            return Err(format!("empty segment in heading path: {:?}", s));
        }

        Ok(HeadingPath(segments))
    }
}

/// What can go wrong resolving a [`HeadingPath`] to a single heading.
#[derive(Debug)]
pub enum PathError {
    /// No heading matched a segment. `deepest_matched` lists the segment
    /// titles that *did* resolve before the failure, deepest last.
    NotFound { deepest_matched: Vec<String> },
    /// A segment matched more than one heading. `candidates` lists every
    /// ambiguous heading so the caller can report line numbers.
    Ambiguous { candidates: Vec<Heading> },
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::NotFound { deepest_matched } => {
                if deepest_matched.is_empty() {
                    write!(f, "heading path not found (no segment matched)")
                } else {
                    write!(
                        f,
                        "heading path not found; deepest matched prefix: {}",
                        deepest_matched.join("/")
                    )
                }
            }
            PathError::Ambiguous { candidates } => {
                write!(f, "{} headings match this path:", candidates.len())?;
                for h in candidates {
                    write!(f, "\n  line {}: {}", h.line, h.title)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for PathError {}

/// Return the direct children of `headings[parent_index]`.
///
/// A heading `H` is a direct child of parent `P` when it lies inside `P`'s
/// subtree and no heading strictly between `P` and `H` has a level shallower
/// than `H` (i.e. a level `< H.level`). This makes level-skips org-idiomatic:
/// a level-3 heading placed directly under a level-1 heading is treated as a
/// direct child, since nothing shallower intervenes.
fn direct_children(headings: &[Heading], parent_index: usize) -> Vec<usize> {
    let parent = &headings[parent_index];
    let mut children = Vec::new();

    for (offset, candidate) in headings.iter().enumerate().skip(parent_index + 1) {
        // Leaving the parent's subtree: a heading at the parent's level or
        // shallower ends it.
        if candidate.level <= parent.level {
            break;
        }
        // A direct child is the shallowest heading reachable without
        // stepping through anything shallower than itself. If some heading
        // between the parent and this candidate is strictly shallower than
        // the candidate, the candidate is a grandchild, not a direct child.
        let intervening_shallower = headings[parent_index + 1..offset]
            .iter()
            .any(|between| between.level < candidate.level);
        if !intervening_shallower {
            children.push(offset);
        }
    }

    children
}

/// Return the 1-based heading lines of the *direct children* of the heading
/// at `parent_line`, in document order.
///
/// A direct child follows the same rule as [`direct_children`]: a heading
/// inside the parent's subtree with no shallower heading intervening. The
/// result is empty when the heading has no children; it is also empty when
/// `parent_line` does not point at a heading line (callers that need to
/// distinguish the two cases should validate the heading line first, e.g.
/// via [`subtree_span`]).
pub fn direct_child_lines(content: &str, parent_line: usize) -> Vec<usize> {
    let headings: Vec<Heading> = iter_headings(content).collect();
    let Some(parent_index) = headings.iter().position(|h| h.line == parent_line) else {
        return Vec::new();
    };
    direct_children(&headings, parent_index)
        .into_iter()
        .map(|idx| headings[idx].line)
        .collect()
}

/// Resolve `path` to exactly one heading in `content`.
///
/// The first segment matches among all level-1 headings. Each subsequent
/// segment matches among the *direct children* (see [`direct_children`]) of
/// the previously matched heading. Every segment must match exactly one
/// heading:
///
/// * zero matches → [`PathError::NotFound`] carrying the deepest prefix that
///   did resolve;
/// * two or more matches → [`PathError::Ambiguous`] carrying every candidate.
pub fn resolve_path(content: &str, path: &HeadingPath) -> Result<Heading, PathError> {
    let headings: Vec<Heading> = iter_headings(content).collect();

    // Candidate indices at the current level, and the titles matched so far.
    let level_one: Vec<usize> = headings
        .iter()
        .enumerate()
        .filter(|(_, h)| h.level == 1)
        .map(|(i, _)| i)
        .collect();

    let mut current_pool = level_one;
    let mut matched_index: Option<usize> = None;
    let mut deepest_matched: Vec<String> = Vec::new();

    for segment in &path.0 {
        let hits: Vec<usize> = current_pool
            .iter()
            .copied()
            .filter(|&i| headings[i].title == *segment)
            .collect();

        match hits.len() {
            0 => return Err(PathError::NotFound { deepest_matched }),
            1 => {
                let idx = hits[0];
                matched_index = Some(idx);
                deepest_matched.push(headings[idx].title.clone());
                current_pool = direct_children(&headings, idx);
            }
            _ => {
                let candidates = hits.into_iter().map(|i| headings[i].clone()).collect();
                return Err(PathError::Ambiguous { candidates });
            }
        }
    }

    // `path` is non-empty (guaranteed by `HeadingPath::from_str`), so at
    // least one segment was consumed and `matched_index` is set.
    matched_index
        .map(|idx| headings[idx].clone())
        .ok_or(PathError::NotFound { deepest_matched })
}

#[cfg(test)]
mod tests {
    use super::{
        Heading, HeadingPath, LineIndex, PathError, SubtreeSpan, heading_body, heading_for_line,
        iter_headings, iter_headings_with, resolve_path, section_span, span_text, subtree_span,
    };
    use crate::org::todo_keywords::TodoKeywords;
    use indoc::indoc;
    use std::collections::BTreeMap;
    use std::str::FromStr;

    #[test]
    fn line_index_empty_content_is_line_one() {
        let index = LineIndex::new("");
        assert_eq!(index.line_at(0), 1);
    }

    #[test]
    fn line_index_offset_zero_is_line_one() {
        let index = LineIndex::new(indoc! {"
            first
            second
        "});
        assert_eq!(index.line_at(0), 1);
    }

    #[test]
    fn line_index_offset_past_end_clamps_to_last_line() {
        let content = indoc! {"
            first
            second
        "};
        let index = LineIndex::new(content);
        // Offset far beyond the end must not panic and yields the last line.
        assert_eq!(
            index.line_at(content.len() + 100),
            index.line_at(content.len())
        );
    }

    #[test]
    fn line_index_offset_at_newline_byte_belongs_to_current_line() {
        // content: "a\nb\n" -> newline bytes at offsets 1 and 3.
        let index = LineIndex::new("a\nb\n");
        // Offset 1 is the `\n` terminating line 1: still line 1.
        assert_eq!(index.line_at(1), 1);
        // Offset 3 is the `\n` terminating line 2: still line 2.
        assert_eq!(index.line_at(3), 2);
    }

    #[test]
    fn line_index_offset_just_after_newline_starts_next_line() {
        // content: "a\nb\n" -> offset 2 is the `b` on line 2.
        let index = LineIndex::new("a\nb\n");
        assert_eq!(index.line_at(2), 2);
        // Offset 4 is one past the final `\n`: line 3.
        assert_eq!(index.line_at(4), 3);
    }

    #[test]
    fn line_index_content_without_trailing_newline() {
        // content: "a\nb" -> single newline at offset 1.
        let index = LineIndex::new("a\nb");
        assert_eq!(index.line_at(0), 1);
        assert_eq!(index.line_at(2), 2);
    }

    #[test]
    fn line_index_multibyte_utf8_offsets_are_byte_based() {
        // "é" is two bytes (0xC3 0xA9). Line 1: "héllo\n" occupies bytes 0..=5
        // (h=0, é=1..2, l=3, l=4, o=5, \n=6). Line 2 starts at byte 7.
        let content = "héllo\nworld";
        let index = LineIndex::new(content);
        // Byte offset of the newline (6) is still line 1.
        assert_eq!(index.line_at(6), 1);
        // Byte offset 7 (`w`) is line 2.
        assert_eq!(index.line_at(7), 2);
    }

    fn props(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn iter_headings_extracts_level_line_title_tags_and_properties() {
        let content = indoc! {"
            * TODO Parent heading :work:client:
            :PROPERTIES:
            :TASK_REF: 248
            :CUSTOM_ID: parent-id
            :END:
            Parent body.
            ** Child heading
            Child body.
            * DONE Finished sibling
            * Sibling
        "};

        let headings: Vec<_> = iter_headings(content).collect();
        assert_eq!(
            headings,
            vec![
                Heading {
                    level: 1,
                    line: 1,
                    keyword: Some("TODO".to_string()),
                    priority: None,
                    fingerprint: "e640c2d8c38f4bda691fcf8d8cba30e4fd2a53f1478a072f81c244ea688562d9"
                        .to_string(),
                    title: "Parent heading".to_string(),
                    tags: vec!["work".to_string(), "client".to_string()],
                    properties: props(&[("CUSTOM_ID", "parent-id"), ("TASK_REF", "248")]),
                },
                Heading {
                    level: 2,
                    line: 7,
                    keyword: None,
                    priority: None,
                    fingerprint: "8b38c0d7ac803c4571af11def1e523fd313339b9fd8a194650b9e6536db45c6d"
                        .to_string(),
                    title: "Child heading".to_string(),
                    tags: vec![],
                    properties: BTreeMap::new(),
                },
                Heading {
                    level: 1,
                    line: 9,
                    keyword: Some("DONE".to_string()),
                    priority: None,
                    fingerprint: "1bfe6956f9548c11f122f6866a6ae224c23c5fbde735e193ba8ed3107cd04fd7"
                        .to_string(),
                    title: "Finished sibling".to_string(),
                    tags: vec![],
                    properties: BTreeMap::new(),
                },
                Heading {
                    level: 1,
                    line: 10,
                    keyword: None,
                    priority: None,
                    fingerprint: "1f4a9b6bdeb8a7999a8ab4e83789a4b8995681d13ace5462ff8b9024454af3d5"
                        .to_string(),
                    title: "Sibling".to_string(),
                    tags: vec![],
                    properties: BTreeMap::new(),
                },
            ]
        );
    }

    #[test]
    fn iter_headings_with_custom_set_extracts_wip_and_canceled() {
        let content = indoc! {"
            * WIP Working task
            * CANCELED Dropped task
            * Plain heading
        "};
        let keywords = TodoKeywords {
            active: vec!["TODO".to_string(), "WIP".to_string()],
            done: vec!["DONE".to_string(), "CANCELED".to_string()],
        };

        let keywords_seen: Vec<Option<String>> = iter_headings_with(content, &keywords)
            .map(|heading| heading.keyword)
            .collect();

        assert_eq!(
            keywords_seen,
            vec![Some("WIP".to_string()), Some("CANCELED".to_string()), None,]
        );
    }

    #[test]
    fn iter_headings_reports_priority_without_restricting_cookie_values() {
        let content = indoc! {"
            * TODO [#A] Latin priority
            * TODO No priority
            * TODO [#破] Non-Latin priority
        "};

        let priorities: Vec<Option<String>> = iter_headings(content)
            .map(|heading| heading.priority)
            .collect();

        assert_eq!(
            priorities,
            vec![Some("A".to_string()), None, Some("破".to_string())]
        );
    }

    #[test]
    fn heading_for_line_returns_enclosing_heading_across_boundaries() {
        let content = indoc! {"
            * Parent
            :PROPERTIES:
            :TASK_REF: 248
            :END:
            Parent body.
            ** Child
            Child body.
            * Sibling
            Sibling body.
        "};

        assert_eq!(heading_for_line(content, 0), None);
        assert_eq!(
            heading_for_line(content, 1).map(|h| h.title),
            Some("Parent".to_string())
        );
        assert_eq!(
            heading_for_line(content, 2).map(|h| h.title),
            Some("Parent".to_string())
        );
        assert_eq!(
            heading_for_line(content, 5).map(|h| h.title),
            Some("Parent".to_string())
        );
        assert_eq!(
            heading_for_line(content, 6).map(|h| h.title),
            Some("Child".to_string())
        );
        assert_eq!(
            heading_for_line(content, 7).map(|h| h.title),
            Some("Child".to_string())
        );
        assert_eq!(
            heading_for_line(content, 8).map(|h| h.title),
            Some("Sibling".to_string())
        );
        assert_eq!(
            heading_for_line(content, 9).map(|h| h.title),
            Some("Sibling".to_string())
        );
        assert_eq!(
            heading_for_line(content, 10).map(|h| h.title),
            Some("Sibling".to_string())
        );
    }

    // ----- subtree_span -----------------------------------------------

    #[test]
    fn test_subtree_span_includes_descendants() {
        let content = indoc! {"
            * Parent
            body of parent
            ** Child
            child body
            *** Grandchild
            grandchild body
            * Sibling
        "};
        // Parent (line 1) subtree runs to line 6 (before Sibling at 7).
        assert_eq!(
            subtree_span(content, 1),
            Some(SubtreeSpan { start: 1, end: 6 })
        );
    }

    #[test]
    fn test_subtree_span_child_stops_before_shallower_sibling() {
        let content = indoc! {"
            * Parent
            ** Child
            child body
            ** Second child
        "};
        // Child (line 2) subtree stops before the next level-2 sibling.
        assert_eq!(
            subtree_span(content, 2),
            Some(SubtreeSpan { start: 2, end: 3 })
        );
    }

    #[test]
    fn test_subtree_span_last_heading_extends_to_last_line() {
        let content = indoc! {"
            * First
            ** Child
            * Last
            last body
            more body
        "};
        // Last (line 3) has no following heading -> extends to line 5.
        assert_eq!(
            subtree_span(content, 3),
            Some(SubtreeSpan { start: 3, end: 5 })
        );
    }

    #[test]
    fn test_subtree_span_last_heading_no_trailing_newline() {
        // No trailing newline: line count must still include the final line.
        let content = "* First\n* Last\nlast body";
        assert_eq!(
            subtree_span(content, 2),
            Some(SubtreeSpan { start: 2, end: 3 })
        );
    }

    #[test]
    fn test_subtree_span_single_line_heading() {
        let content = "* Only\n";
        assert_eq!(
            subtree_span(content, 1),
            Some(SubtreeSpan { start: 1, end: 1 })
        );
    }

    #[test]
    fn test_subtree_span_level_skip_one_to_three() {
        // A level-3 heading directly under a level-1 heading is still a
        // descendant, so the parent subtree spans across it.
        let content = indoc! {"
            * Parent
            *** Deep child
            deep body
            * Sibling
        "};
        assert_eq!(
            subtree_span(content, 1),
            Some(SubtreeSpan { start: 1, end: 3 })
        );
    }

    #[test]
    fn test_subtree_span_bold_line_is_not_a_heading() {
        // `*bold*` at line start has no space after the star: not a heading.
        let content = indoc! {"
            * Heading
            *bold* text on its own line
            more body
            * Next
        "};
        assert_eq!(
            subtree_span(content, 1),
            Some(SubtreeSpan { start: 1, end: 3 })
        );
        // The `*bold*` line is not addressable as a heading.
        assert_eq!(subtree_span(content, 2), None);
    }

    #[test]
    fn test_subtree_span_non_heading_line_returns_none() {
        let content = indoc! {"
            * Heading
            body line
        "};
        assert_eq!(subtree_span(content, 2), None);
    }

    #[test]
    fn test_subtree_span_line_out_of_range_returns_none() {
        let content = "* Heading\n";
        assert_eq!(subtree_span(content, 99), None);
    }

    // ----- section_span -----------------------------------------------

    #[test]
    fn test_section_span_stops_before_immediate_child() {
        let content = indoc! {"
            * Parent
            parent body
            ** Child
            child body
        "};
        // Parent's own section is lines 1..=2, stopping before the child.
        assert_eq!(
            section_span(content, 1),
            Some(SubtreeSpan { start: 1, end: 2 })
        );
    }

    #[test]
    fn test_section_span_heading_immediately_followed_by_child() {
        let content = indoc! {"
            * Parent
            ** Child
            child body
        "};
        // No body between Parent and Child: section is the heading line only.
        assert_eq!(
            section_span(content, 1),
            Some(SubtreeSpan { start: 1, end: 1 })
        );
    }

    #[test]
    fn test_section_span_last_heading_extends_to_last_line() {
        let content = indoc! {"
            * First
            ** Child
            * Last
            body
        "};
        assert_eq!(
            section_span(content, 3),
            Some(SubtreeSpan { start: 3, end: 4 })
        );
    }

    #[test]
    fn test_section_span_stops_before_deeper_and_sibling_alike() {
        // section stops at the FIRST following heading of ANY level.
        let content = indoc! {"
            * Parent
            body
            * Sibling
        "};
        assert_eq!(
            section_span(content, 1),
            Some(SubtreeSpan { start: 1, end: 2 })
        );
    }

    #[test]
    fn test_section_span_non_heading_returns_none() {
        let content = "* Heading\nbody\n";
        assert_eq!(section_span(content, 2), None);
    }

    // ----- span_text / heading_body -----------------------------------

    #[test]
    fn test_span_text_is_a_verbatim_substring_with_terminator() {
        let content = indoc! {"
            * Parent
            body one
            body two
            * Next
        "};
        let span = SubtreeSpan { start: 2, end: 3 };
        let text = span_text(content, span).unwrap();

        assert_eq!(text, "body one\nbody two\n");
        // Verbatim: the slice must occur literally in the source.
        assert!(content.contains(text));
    }

    #[test]
    fn test_span_text_last_unterminated_line_reaches_end_of_content() {
        let content = "* Parent\nbody";
        assert_eq!(
            span_text(content, SubtreeSpan { start: 2, end: 2 }),
            Some("body")
        );
    }

    #[test]
    fn test_span_text_rejects_empty_or_out_of_range_spans() {
        let content = "* Parent\nbody\n";
        assert_eq!(span_text(content, SubtreeSpan { start: 0, end: 2 }), None);
        assert_eq!(span_text(content, SubtreeSpan { start: 3, end: 2 }), None);
        assert_eq!(span_text(content, SubtreeSpan { start: 9, end: 9 }), None);
    }

    #[test]
    fn test_heading_body_excludes_the_heading_line_but_keeps_descendants() {
        let content = indoc! {"
            * Parent
            :PROPERTIES:
            :ID: x
            :END:
            parent body
            ** Child
            child body
            * Sibling
            sibling body
        "};

        assert_eq!(
            heading_body(content, 1).unwrap(),
            indoc! {"
                :PROPERTIES:
                :ID: x
                :END:
                parent body
                ** Child
                child body
            "}
        );
        assert_eq!(heading_body(content, 6).unwrap(), "child body\n");
    }

    #[test]
    fn test_heading_body_of_a_bare_heading_is_empty_not_missing() {
        let content = "* Alone\n* Next\n";
        assert_eq!(heading_body(content, 1), Some(""));
        assert_eq!(heading_body(content, 2), Some(""));
    }

    #[test]
    fn test_heading_body_of_a_non_heading_line_is_none() {
        let content = "* Alone\nbody\n";
        assert_eq!(heading_body(content, 2), None);
        assert_eq!(heading_body(content, 99), None);
    }

    // ----- HeadingPath::from_str --------------------------------------

    #[test]
    fn test_heading_path_parses_simple_segments() {
        assert_eq!(
            HeadingPath::from_str("Parent/Child").unwrap(),
            HeadingPath(vec!["Parent".to_string(), "Child".to_string()])
        );
    }

    #[test]
    fn test_heading_path_single_segment() {
        assert_eq!(
            HeadingPath::from_str("Only").unwrap(),
            HeadingPath(vec!["Only".to_string()])
        );
    }

    #[test]
    fn test_heading_path_escaped_slash_stays_in_segment() {
        // `a\/b` is a single segment containing a literal slash.
        assert_eq!(
            HeadingPath::from_str(r"a\/b").unwrap(),
            HeadingPath(vec!["a/b".to_string()])
        );
    }

    #[test]
    fn test_heading_path_escaped_slash_between_segments() {
        assert_eq!(
            HeadingPath::from_str(r"Top/a\/b/Leaf").unwrap(),
            HeadingPath(vec![
                "Top".to_string(),
                "a/b".to_string(),
                "Leaf".to_string(),
            ])
        );
    }

    #[test]
    fn test_heading_path_escaped_backslash() {
        assert_eq!(
            HeadingPath::from_str(r"a\\b").unwrap(),
            HeadingPath(vec![r"a\b".to_string()])
        );
    }

    #[test]
    fn test_heading_path_empty_string_is_error() {
        assert!(HeadingPath::from_str("").is_err());
    }

    #[test]
    fn test_heading_path_empty_segment_is_error() {
        assert!(HeadingPath::from_str("a//b").is_err());
        assert!(HeadingPath::from_str("/a").is_err());
        assert!(HeadingPath::from_str("a/").is_err());
    }

    #[test]
    fn test_heading_path_trailing_backslash_is_error() {
        assert!(HeadingPath::from_str(r"a\").is_err());
    }

    // ----- resolve_path -----------------------------------------------

    const RESOLVE_SAMPLE: &str = indoc! {"
        * Alpha
        ** One
        ** Two
        *** Deep
        * Beta
        ** One
    "};

    #[test]
    fn test_resolve_path_single_top_level() {
        let path = HeadingPath::from_str("Alpha").unwrap();
        let h = resolve_path(RESOLVE_SAMPLE, &path).unwrap();
        assert_eq!(h.line, 1);
        assert_eq!(h.title, "Alpha");
    }

    #[test]
    fn test_resolve_path_nested() {
        let path = HeadingPath::from_str("Alpha/Two").unwrap();
        let h = resolve_path(RESOLVE_SAMPLE, &path).unwrap();
        assert_eq!(h.line, 3);
        assert_eq!(h.title, "Two");
    }

    #[test]
    fn test_resolve_path_deep() {
        let path = HeadingPath::from_str("Alpha/Two/Deep").unwrap();
        let h = resolve_path(RESOLVE_SAMPLE, &path).unwrap();
        assert_eq!(h.line, 4);
        assert_eq!(h.title, "Deep");
    }

    #[test]
    fn test_resolve_path_duplicate_titles_disambiguated_by_path() {
        // "One" appears under both Alpha and Beta; the path disambiguates.
        let under_alpha =
            resolve_path(RESOLVE_SAMPLE, &HeadingPath::from_str("Alpha/One").unwrap()).unwrap();
        assert_eq!(under_alpha.line, 2);

        let under_beta =
            resolve_path(RESOLVE_SAMPLE, &HeadingPath::from_str("Beta/One").unwrap()).unwrap();
        assert_eq!(under_beta.line, 6);
    }

    #[test]
    fn test_resolve_path_ambiguous_at_same_level() {
        let content = indoc! {"
            * Root
            ** Dup
            ** Dup
        "};
        let path = HeadingPath::from_str("Root/Dup").unwrap();
        match resolve_path(content, &path).unwrap_err() {
            PathError::Ambiguous { candidates } => {
                assert_eq!(candidates.len(), 2);
                assert_eq!(candidates[0].line, 2);
                assert_eq!(candidates[1].line, 3);
            }
            other => panic!("expected Ambiguous, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_path_ambiguous_at_top_level() {
        let content = indoc! {"
            * Dup
            * Dup
        "};
        let path = HeadingPath::from_str("Dup").unwrap();
        match resolve_path(content, &path).unwrap_err() {
            PathError::Ambiguous { candidates } => assert_eq!(candidates.len(), 2),
            other => panic!("expected Ambiguous, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_path_not_found_reports_deepest_prefix() {
        let path = HeadingPath::from_str("Alpha/Missing").unwrap();
        match resolve_path(RESOLVE_SAMPLE, &path).unwrap_err() {
            PathError::NotFound { deepest_matched } => {
                assert_eq!(deepest_matched, vec!["Alpha".to_string()]);
            }
            other => panic!("expected NotFound, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_path_not_found_at_top_level_empty_prefix() {
        let path = HeadingPath::from_str("Nope").unwrap();
        match resolve_path(RESOLVE_SAMPLE, &path).unwrap_err() {
            PathError::NotFound { deepest_matched } => assert!(deepest_matched.is_empty()),
            other => panic!("expected NotFound, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_path_level_skip_direct_child() {
        // Level 1 followed directly by level 3: the level-3 heading is a
        // direct child of the level-1 parent (nothing shallower intervenes).
        let content = indoc! {"
            * Parent
            *** Skipped
            * Sibling
        "};
        let path = HeadingPath::from_str("Parent/Skipped").unwrap();
        let h = resolve_path(content, &path).unwrap();
        assert_eq!(h.line, 2);
        assert_eq!(h.title, "Skipped");
    }

    #[test]
    fn test_resolve_path_grandchild_not_a_direct_child() {
        // "Deep" (level 3) is NOT a direct child of Alpha because "Two"
        // (level 2) intervenes and is shallower than Deep.
        let path = HeadingPath::from_str("Alpha/Deep").unwrap();
        match resolve_path(RESOLVE_SAMPLE, &path).unwrap_err() {
            PathError::NotFound { deepest_matched } => {
                assert_eq!(deepest_matched, vec!["Alpha".to_string()]);
            }
            other => panic!("expected NotFound, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_path_title_matches_after_keyword_and_tags_stripped() {
        // The parser strips the TODO keyword and tags from the title, so
        // the path segment matches the bare title text.
        let content = indoc! {"
            * TODO Task title :work:urgent:
            ** DONE Subtask :home:
        "};
        let h = resolve_path(
            content,
            &HeadingPath::from_str("Task title/Subtask").unwrap(),
        )
        .unwrap();
        assert_eq!(h.line, 2);
        assert_eq!(h.title, "Subtask");
    }

    #[test]
    fn test_resolve_path_display_ambiguous_lists_candidates() {
        let content = indoc! {"
            * Dup
            * Dup
        "};
        let err = resolve_path(content, &HeadingPath::from_str("Dup").unwrap()).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("2 headings match"));
        assert!(rendered.contains("line 1: Dup"));
        assert!(rendered.contains("line 2: Dup"));
    }

    #[test]
    fn test_resolve_path_display_not_found_shows_prefix() {
        let err = resolve_path(
            RESOLVE_SAMPLE,
            &HeadingPath::from_str("Alpha/Missing").unwrap(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("deepest matched prefix: Alpha"));
    }
}
