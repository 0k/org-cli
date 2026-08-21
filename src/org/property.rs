//! Org-mode node-property line shape.
//!
//! Owns one fact: what a `:KEY: value` line *is*. Two independent
//! consumers need that fact and previously each carried their own copy
//! of it:
//!
//! - [`crate::org::heading_edit`] edits property drawers line by line
//!   (`set-property` / `unset-property`) and must recognize an existing
//!   entry to replace or remove it.
//! - [`crate::org::link`] extracts `[[file:...]]` links from *degraded*
//!   property drawers, where `orgize` no longer builds `NODE_PROPERTY`
//!   nodes (see the `E3` note there) and the key/value split must be
//!   recovered from the raw line.
//!
//! Both change for the same reason — a change in what org-mode accepts
//! as a node-property line — so the definition lives here once.

/// The `:KEY: value` split of a single node-property line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyLine {
    /// Property key, exactly as written (case preserved).
    pub key: String,
    /// Property value, trimmed of surrounding whitespace.
    pub value: String,
}

/// Parse a `:KEY: value` node-property line.
///
/// Returns `None` when the line is not a property entry, including for
/// the drawer delimiters `:PROPERTIES:` and `:END:`, which are
/// syntactically indistinguishable from a valueless property but are
/// never entries.
///
/// Leading indentation is ignored. Keys may contain internal colons
/// (`header-args:clojure`) but no whitespace. A non-empty value must be
/// separated from the final colon by horizontal whitespace; an empty
/// value (`:KEY:`) is valid. The returned value is trimmed.
///
/// ```
/// use org_cli::org::property::parse_property_line;
///
/// let p = parse_property_line("  :SOURCE_AUDIO: [[file:a.org][A]]").unwrap();
/// assert_eq!(p.key, "SOURCE_AUDIO");
/// assert_eq!(p.value, "[[file:a.org][A]]");
///
/// assert!(parse_property_line(":END:").is_none());
/// assert!(parse_property_line("not a property").is_none());
/// ```
pub fn parse_property_line(line: &str) -> Option<PropertyLine> {
    let trimmed = line.trim_start_matches([' ', '\t']);
    let rest = trimmed.strip_prefix(':')?;
    // The delimiter is the first colon followed by horizontal
    // whitespace or end-of-line. Property keys may themselves contain
    // colons (`header-args:clojure`), while a non-empty value must be
    // separated from the delimiter (`:KEY:VALUE` is not a property).
    let close = rest.match_indices(':').find_map(|(i, _)| {
        let after = &rest[i + 1..];
        (after.is_empty() || after.starts_with([' ', '\t'])).then_some(i)
    })?;
    let key = &rest[..close];
    let value = rest[close + 1..].trim_matches([' ', '\t']);
    if key.is_empty() || key.chars().any(char::is_whitespace) {
        return None;
    }
    // `:END:` is the drawer terminator only in its exact valueless
    // form. `:END: value` is a valid node property inside a property
    // drawer. `:PROPERTIES:` is also a valid valueless property when
    // it appears inside an already-open property drawer; outer opener
    // detection belongs to the caller's context.
    if key.eq_ignore_ascii_case("END") && value.is_empty() {
        return None;
    }
    Some(PropertyLine {
        key: key.to_string(),
        value: value.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_property_line_splits_key_and_value() {
        let p = parse_property_line(":PREDECESSOR: [[file:a.org][A]]").unwrap();
        assert_eq!(p.key, "PREDECESSOR");
        assert_eq!(p.value, "[[file:a.org][A]]");
    }

    #[test]
    fn test_parse_property_line_ignores_indentation_and_trims_value() {
        let p = parse_property_line("    :KEY:   spaced value   ").unwrap();
        assert_eq!(p.key, "KEY");
        assert_eq!(p.value, "spaced value");
    }

    #[test]
    fn test_parse_property_line_preserves_key_case() {
        assert_eq!(
            parse_property_line(":MixedCase: v").unwrap().key,
            "MixedCase"
        );
    }

    #[test]
    fn test_parse_property_line_accepts_empty_value() {
        let p = parse_property_line(":KEY:").unwrap();
        assert_eq!(p.key, "KEY");
        assert_eq!(p.value, "");
    }

    #[test]
    fn test_parse_property_line_accepts_internal_colon_in_key() {
        let p = parse_property_line(":header-args:clojure: :session *x*").unwrap();
        assert_eq!(p.key, "header-args:clojure");
        assert_eq!(p.value, ":session *x*");
    }

    #[test]
    fn test_parse_property_line_accepts_append_key() {
        let p = parse_property_line(":var+: second").unwrap();
        assert_eq!(p.key, "var+");
        assert_eq!(p.value, "second");
    }

    #[test]
    fn test_parse_property_line_rejects_nonempty_value_without_separator_space() {
        assert!(parse_property_line(":KEY:VALUE").is_none());
    }

    #[test]
    fn test_parse_property_line_rejects_whitespace_in_key() {
        assert!(parse_property_line(":BAD KEY: value").is_none());
    }

    #[test]
    fn test_parse_property_line_rejects_drawer_delimiters() {
        assert!(parse_property_line(":END:").is_none());
        assert!(parse_property_line("  :end:").is_none());
    }

    #[test]
    fn test_parse_property_line_accepts_reserved_looking_keys_in_property_context() {
        assert_eq!(parse_property_line(":END: value").unwrap().value, "value");
        assert_eq!(
            parse_property_line(":PROPERTIES: value").unwrap().value,
            "value"
        );
        assert_eq!(parse_property_line(":PROPERTIES:").unwrap().value, "");
    }

    #[test]
    fn test_parse_property_line_rejects_unicode_indentation() {
        assert!(parse_property_line("\u{a0}:KEY: value").is_none());
    }

    #[test]
    fn test_parse_property_line_rejects_non_property_lines() {
        assert!(parse_property_line("plain text").is_none());
        assert!(parse_property_line("- list item").is_none());
        assert!(parse_property_line(":unterminated").is_none());
        assert!(parse_property_line("::").is_none());
    }
}
