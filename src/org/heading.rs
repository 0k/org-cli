use std::collections::BTreeMap;

use orgize::{
    Org,
    ast::Headline as OrgHeadline,
    rowan::{TextSize, ast::AstNode},
};

/// A heading extracted from an org-mode file, including its property drawer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    pub level: u32,
    pub line: usize,
    /// The org-mode TODO keyword (e.g. `TODO`, `DONE`), if present.
    ///
    /// Only the keywords recognized by orgize's default `ParseConfig`
    /// (namely `TODO` and `DONE`) are extracted; `None` otherwise.
    pub keyword: Option<String>,
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
}

/// Walk every heading in `content`, in document order.
///
/// Property keys are preserved exactly as written. Lookups against
/// `Heading::properties` are therefore case-sensitive, matching orgize's
/// `Headline::properties().get(key)` behavior.
pub fn iter_headings(content: &str) -> impl Iterator<Item = Heading> + '_ {
    let org = Org::parse(content);
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

#[cfg(test)]
mod tests {
    use super::{Heading, LineIndex, heading_for_line, iter_headings};
    use indoc::indoc;
    use std::collections::BTreeMap;

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
                    title: "Parent heading".to_string(),
                    tags: vec!["work".to_string(), "client".to_string()],
                    properties: props(&[("CUSTOM_ID", "parent-id"), ("TASK_REF", "248")]),
                },
                Heading {
                    level: 2,
                    line: 7,
                    keyword: None,
                    title: "Child heading".to_string(),
                    tags: vec![],
                    properties: BTreeMap::new(),
                },
                Heading {
                    level: 1,
                    line: 9,
                    keyword: Some("DONE".to_string()),
                    title: "Finished sibling".to_string(),
                    tags: vec![],
                    properties: BTreeMap::new(),
                },
                Heading {
                    level: 1,
                    line: 10,
                    keyword: None,
                    title: "Sibling".to_string(),
                    tags: vec![],
                    properties: BTreeMap::new(),
                },
            ]
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
}
