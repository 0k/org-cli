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

/// Walk every heading in `content`, in document order.
///
/// Property keys are preserved exactly as written. Lookups against
/// `Heading::properties` are therefore case-sensitive, matching orgize's
/// `Headline::properties().get(key)` behavior.
pub fn iter_headings(content: &str) -> impl Iterator<Item = Heading> + '_ {
    let org = Org::parse(content);
    org.document()
        .syntax()
        .descendants()
        .filter_map(OrgHeadline::cast)
        .filter_map(|headline| build_heading(content, headline))
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

fn build_heading(content: &str, headline: OrgHeadline) -> Option<Heading> {
    let level = u32::try_from(headline.level()).ok()?;
    let line = line_number(content, headline.start());
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

fn line_number(content: &str, offset: TextSize) -> usize {
    let offset_u32: u32 = offset.into();
    let byte_offset = usize::try_from(offset_u32).unwrap_or(content.len());
    content[..byte_offset.min(content.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

#[cfg(test)]
mod tests {
    use super::{Heading, heading_for_line, iter_headings};
    use indoc::indoc;
    use std::collections::BTreeMap;

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
