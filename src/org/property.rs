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
/// Leading indentation is ignored; the value keeps everything after the
/// closing colon, trimmed.
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
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix(':')?;
    let close = rest.find(':')?;
    let key = &rest[..close];
    if key.is_empty() || key.eq_ignore_ascii_case("END") || key.eq_ignore_ascii_case("PROPERTIES") {
        return None;
    }
    Some(PropertyLine {
        key: key.to_string(),
        value: rest[close + 1..].trim().to_string(),
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
    fn test_parse_property_line_rejects_drawer_delimiters() {
        assert!(parse_property_line(":PROPERTIES:").is_none());
        assert!(parse_property_line(":END:").is_none());
        assert!(parse_property_line("  :end:").is_none());
    }

    #[test]
    fn test_parse_property_line_rejects_non_property_lines() {
        assert!(parse_property_line("plain text").is_none());
        assert!(parse_property_line("- list item").is_none());
        assert!(parse_property_line(":unterminated").is_none());
        assert!(parse_property_line("::").is_none());
    }
}
