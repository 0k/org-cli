use regex::Regex;

use crate::org::heading::Heading;

/// Compiled predicate over a [`Heading`]. Multiple options AND together.
#[derive(Debug, Clone)]
pub struct FilterOptions {
    pub pattern: Option<Regex>,
    /// Case-insensitive matching is applied when compiling `pattern`
    /// (for example via `RegexBuilder::case_insensitive(true)`).
    pub ignore_case: bool,
    pub has_properties: Vec<String>,
    /// Exact-match set of TODO keywords. When non-empty, a heading
    /// passes only if its keyword is one of these (case-sensitive).
    pub keywords: Vec<String>,
    /// When true, a heading passes only if it carries any TODO keyword.
    pub todo_only: bool,
}

impl FilterOptions {
    pub fn matches(&self, heading: &Heading) -> bool {
        let pattern_matches = self
            .pattern
            .as_ref()
            .map(|pattern| pattern.is_match(&heading.title))
            .unwrap_or(true);
        let properties_match = self
            .has_properties
            .iter()
            .all(|name| heading.properties.contains_key(name));
        let keyword_match = self.keywords.is_empty()
            || heading
                .keyword
                .as_ref()
                .is_some_and(|keyword| self.keywords.contains(keyword));
        let todo_match = !self.todo_only || heading.keyword.is_some();

        pattern_matches && properties_match && keyword_match && todo_match
    }

    pub fn is_empty(&self) -> bool {
        self.pattern.is_none()
            && self.has_properties.is_empty()
            && self.keywords.is_empty()
            && !self.todo_only
    }
}

#[cfg(test)]
mod tests {
    use super::FilterOptions;
    use crate::org::heading::Heading;
    use regex::{Regex, RegexBuilder};
    use std::collections::BTreeMap;

    fn heading(title: &str, properties: &[(&str, &str)]) -> Heading {
        heading_kw(title, None, properties)
    }

    fn heading_kw(title: &str, keyword: Option<&str>, properties: &[(&str, &str)]) -> Heading {
        Heading {
            level: 1,
            line: 1,
            keyword: keyword.map(str::to_string),
            priority: None,
            fingerprint: String::new(),
            title: title.to_string(),
            tags: vec![],
            properties: properties
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn empty_filter_is_identity() {
        let filter = FilterOptions {
            pattern: None,
            ignore_case: false,
            has_properties: vec![],
            keywords: vec![],
            todo_only: false,
        };

        assert!(filter.is_empty());
        assert!(filter.matches(&heading("Anything", &[])));
    }

    #[test]
    fn matches_title_pattern_and_required_properties() {
        let filter = FilterOptions {
            pattern: Some(Regex::new("task").unwrap()),
            ignore_case: false,
            has_properties: vec!["TASK_REF".to_string(), "CUSTOM_ID".to_string()],
            keywords: vec![],
            todo_only: false,
        };

        assert!(filter.matches(&heading(
            "task runner",
            &[("TASK_REF", "248"), ("CUSTOM_ID", "abc")]
        )));
        assert!(!filter.matches(&heading("task runner", &[("TASK_REF", "248")])));
        assert!(!filter.matches(&heading(
            "note",
            &[("TASK_REF", "248"), ("CUSTOM_ID", "abc")]
        )));
    }

    #[test]
    fn matches_uses_case_insensitive_compiled_pattern() {
        let filter = FilterOptions {
            pattern: Some(
                RegexBuilder::new("login")
                    .case_insensitive(true)
                    .build()
                    .unwrap(),
            ),
            ignore_case: true,
            has_properties: vec![],
            keywords: vec![],
            todo_only: false,
        };

        assert!(filter.matches(&heading("Fix LOGIN bug", &[])));
    }

    #[test]
    fn matches_exact_keyword_set() {
        let filter = FilterOptions {
            pattern: None,
            ignore_case: false,
            has_properties: vec![],
            keywords: vec!["TODO".to_string(), "DONE".to_string()],
            todo_only: false,
        };

        assert!(!filter.is_empty());
        assert!(filter.matches(&heading_kw("Buy milk", Some("TODO"), &[])));
        assert!(filter.matches(&heading_kw("Ship it", Some("DONE"), &[])));
        assert!(!filter.matches(&heading_kw("Waiting", Some("WAIT"), &[])));
        assert!(!filter.matches(&heading_kw("Plain", None, &[])));
    }

    #[test]
    fn matches_todo_only_requires_any_keyword() {
        let filter = FilterOptions {
            pattern: None,
            ignore_case: false,
            has_properties: vec![],
            keywords: vec![],
            todo_only: true,
        };

        assert!(!filter.is_empty());
        assert!(filter.matches(&heading_kw("Buy milk", Some("TODO"), &[])));
        assert!(filter.matches(&heading_kw("Ship it", Some("DONE"), &[])));
        assert!(!filter.matches(&heading_kw("Plain", None, &[])));
    }

    #[test]
    fn keyword_predicates_and_combine_with_title_pattern() {
        let filter = FilterOptions {
            pattern: Some(Regex::new("milk").unwrap()),
            ignore_case: false,
            has_properties: vec![],
            keywords: vec!["TODO".to_string()],
            todo_only: false,
        };

        assert!(filter.matches(&heading_kw("Buy milk", Some("TODO"), &[])));
        // Right keyword, wrong title.
        assert!(!filter.matches(&heading_kw("Buy bread", Some("TODO"), &[])));
        // Right title, wrong keyword.
        assert!(!filter.matches(&heading_kw("Buy milk", Some("DONE"), &[])));
    }
}
