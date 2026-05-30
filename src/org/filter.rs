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

        pattern_matches && properties_match
    }

    pub fn is_empty(&self) -> bool {
        self.pattern.is_none() && self.has_properties.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::FilterOptions;
    use crate::org::heading::Heading;
    use regex::{Regex, RegexBuilder};
    use std::collections::BTreeMap;

    fn heading(title: &str, properties: &[(&str, &str)]) -> Heading {
        Heading {
            level: 1,
            line: 1,
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
        };

        assert!(filter.matches(&heading("Fix LOGIN bug", &[])));
    }
}
