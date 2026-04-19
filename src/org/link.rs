//! Org-mode link domain type.
//!
//! Owns the parsing and structural representation of `[[file:...]]`
//! links found in an org document, plus the indexing of anchorable
//! targets (headings, `:CUSTOM_ID:` properties) inside an org file.
//!
//! Higher layers (`crate::cli::action::check`) consume these to
//! validate cross-file link integrity.

use std::path::PathBuf;

use orgize::{
    Org,
    ast::Headline,
    export::{Container, Event, from_fn},
    rowan::ast::AstNode,
};

/// A link extracted from an org document.
///
/// Only `file:` links are produced by [`extract_file_links`]; other
/// link types (`http:`, `id:`, `[[*Same-File-Heading]]`, …) are
/// out of scope for v1 and are silently skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Raw link text as it appears in the source, e.g.
    /// `"[[file:foo.org::*Heading][My desc]]"`.
    pub raw: String,
    /// Path part, with the `file:` prefix stripped and percent-escapes
    /// decoded. May be relative (resolved against the containing org
    /// file's directory by callers) or absolute.
    pub target_path: PathBuf,
    /// Optional anchor following `::`. `None` when the link is just
    /// `[[file:path]]` or `[[file:path][desc]]`.
    pub anchor: Option<Anchor>,
    /// 1-based line number of the link's opening `[[` in the source.
    pub line: usize,
    /// 1-based column number of the link's opening `[[` in the source.
    pub column: usize,
}

/// The kind of in-file target referenced by a link's anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    /// `::*Heading` — match against a headline's plain title.
    Heading(String),
    /// `::#custom-id` — match against a `:CUSTOM_ID:` property value.
    CustomId(String),
    /// `::N` — a 1-based line number.
    LineNumber(usize),
    /// Anything else after `::` we don't validate (e.g. arbitrary
    /// search strings). Stored verbatim for reporting.
    Other(String),
}

/// Why a link is considered broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakReason {
    /// Resolved target path does not exist on disk.
    PathMissing,
    /// Path exists but the requested heading was not found.
    HeadingNotFound { heading: String },
    /// Path exists but no headline carries that `:CUSTOM_ID:`.
    CustomIdNotFound { id: String },
    /// Path exists but is shorter than the requested line number.
    LineOutOfRange { asked: usize, max: usize },
    /// `::N` anchor on a non-org file (we only enforce in `--strict`).
    LineAnchorOnNonOrg { asked: usize },
}

impl BreakReason {
    /// Short machine-friendly tag (used in NUL/JSON output).
    pub fn tag(&self) -> &'static str {
        match self {
            BreakReason::PathMissing => "path-missing",
            BreakReason::HeadingNotFound { .. } => "heading-not-found",
            BreakReason::CustomIdNotFound { .. } => "custom-id-not-found",
            BreakReason::LineOutOfRange { .. } => "line-out-of-range",
            BreakReason::LineAnchorOnNonOrg { .. } => "line-anchor-on-non-org",
        }
    }

    /// Human-readable explanation.
    pub fn message(&self) -> String {
        match self {
            BreakReason::PathMissing => "target path does not exist".to_string(),
            BreakReason::HeadingNotFound { heading } => {
                format!("heading not found: {:?}", heading)
            }
            BreakReason::CustomIdNotFound { id } => {
                format!("CUSTOM_ID not found: {:?}", id)
            }
            BreakReason::LineOutOfRange { asked, max } => {
                format!("line {} out of range (file has {} lines)", asked, max)
            }
            BreakReason::LineAnchorOnNonOrg { asked } => {
                format!("line anchor ::{} on non-org target", asked)
            }
        }
    }
}

/// Index of anchorable targets inside an org file. Built once per
/// linked-to file and reused for every anchor lookup.
#[derive(Debug, Default, Clone)]
pub struct AnchorIndex {
    /// Trimmed plain titles of every headline in the file (any depth).
    pub headings: Vec<String>,
    /// Values of every `:CUSTOM_ID:` property drawer entry.
    pub custom_ids: Vec<String>,
    /// Number of newline-separated lines in the file (used by `::N`).
    pub line_count: usize,
}

impl AnchorIndex {
    /// Parse an org file's contents and build its anchor index.
    pub fn from_org_content(content: &str) -> Self {
        let org = Org::parse(content);
        let mut headings = Vec::new();
        let mut custom_ids = Vec::new();

        for hdl in org
            .document()
            .syntax()
            .descendants()
            .filter_map(Headline::cast)
        {
            headings.push(normalize_heading(&hdl.title_raw()));
            if let Some(properties) = hdl.properties()
                && let Some(cid) = properties.get("CUSTOM_ID")
            {
                custom_ids.push(cid.to_string());
            }
        }

        AnchorIndex {
            headings,
            custom_ids,
            line_count: line_count(content),
        }
    }

    /// `true` if the given heading text exactly matches one of the
    /// file's headings, after whitespace-collapsing both sides. This
    /// is the strict matcher used when `--strict-headings` is on.
    pub fn has_heading_exact(&self, heading: &str) -> bool {
        let needle = normalize_heading(heading);
        self.headings.iter().any(|h| h == &needle)
    }

    /// `true` if the given heading text matches any headline using
    /// org-mode's default lenient `[[*Heading]]` resolution, which
    /// is a substring match against the headline's plain title
    /// (after whitespace-collapse). This mirrors Emacs' actual
    /// link-following behavior for `[[*Heading]]` anchors.
    pub fn has_heading_substring(&self, heading: &str) -> bool {
        let needle = normalize_heading(heading);
        if needle.is_empty() {
            return false;
        }
        self.headings.iter().any(|h| h.contains(&needle))
    }

    /// `true` if the given custom id matches any `:CUSTOM_ID:` value.
    pub fn has_custom_id(&self, id: &str) -> bool {
        self.custom_ids.iter().any(|c| c == id)
    }
}

/// Extract every `[[file:...]]` link from the given org content.
///
/// The returned [`Link`]s have their `file:` prefix stripped and
/// their path part percent-decoded. Path resolution against the
/// containing file's directory is the caller's responsibility.
pub fn extract_file_links(content: &str) -> Vec<Link> {
    let mut out: Vec<Link> = Vec::new();
    let mut handler = from_fn(|event| {
        let Event::Enter(Container::Link(link)) = event else {
            return;
        };
        let raw_path = link.path().to_string();
        let Some(after_scheme) = raw_path.strip_prefix("file:") else {
            return; // skip http:, id:, etc.
        };

        let (path_str, anchor) = split_anchor(after_scheme);
        let decoded = percent_decode(path_str);
        let target_path = PathBuf::from(decoded);

        let offset: usize = link.start().into();
        let (line, column) = offset_to_line_col(content, offset);

        out.push(Link {
            raw: link.raw(),
            target_path,
            anchor,
            line,
            column,
        });
    });
    Org::parse(content).traverse(&mut handler);
    out
}

/// Whitespace-collapse and trim a heading title for matching.
///
/// Two headings are considered equal iff their normalized forms are
/// byte-equal. We deliberately preserve case and accents; only
/// whitespace runs are collapsed and edge whitespace stripped.
fn normalize_heading(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.trim().chars() {
        if ch.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out
}

/// Split a path payload (the part after `file:`) into `(path, anchor)`.
fn split_anchor(after_scheme: &str) -> (&str, Option<Anchor>) {
    let Some(idx) = after_scheme.find("::") else {
        return (after_scheme, None);
    };
    let path = &after_scheme[..idx];
    let raw_anchor = &after_scheme[idx + 2..];
    let anchor = classify_anchor(raw_anchor);
    (path, Some(anchor))
}

fn classify_anchor(raw: &str) -> Anchor {
    if let Some(rest) = raw.strip_prefix('*') {
        Anchor::Heading(normalize_heading(rest))
    } else if let Some(rest) = raw.strip_prefix('#') {
        Anchor::CustomId(rest.to_string())
    } else if let Ok(n) = raw.parse::<usize>() {
        Anchor::LineNumber(n)
    } else {
        Anchor::Other(raw.to_string())
    }
}

/// Decode percent-escapes in a path. Invalid sequences are left as-is.
fn percent_decode(input: &str) -> String {
    percent_encoding::percent_decode_str(input)
        .decode_utf8_lossy()
        .into_owned()
}

/// 1-based (line, column) for a byte offset into `content`.
fn offset_to_line_col(content: &str, offset: usize) -> (usize, usize) {
    let clamped = offset.min(content.len());
    let before = &content[..clamped];
    let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
    let col = match before.rfind('\n') {
        Some(p) => clamped - p,
        None => clamped + 1,
    };
    (line, col)
}

/// Number of lines in `content`, counted as in `wc -l`-style "lines
/// the user can address by `::N`": we count the maximum 1-based line
/// number the file exposes, i.e. `count('\n') + (1 if last line is
/// non-empty else 0)`.
fn line_count(content: &str) -> usize {
    if content.is_empty() {
        return 0;
    }
    let nl = content.bytes().filter(|&b| b == b'\n').count();
    if content.ends_with('\n') { nl } else { nl + 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn test_extract_simple_file_link() {
        let src = indoc! {"
            * Top
            See [[file:foo.org][Foo]] for details.
        "};
        let links = extract_file_links(src);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_path, PathBuf::from("foo.org"));
        assert_eq!(links[0].anchor, None);
        assert_eq!(links[0].line, 2);
    }

    #[test]
    fn test_extract_link_with_heading_anchor() {
        let src = "Body [[file:bar.org::*My Heading][bar]] end.\n";
        let links = extract_file_links(src);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_path, PathBuf::from("bar.org"));
        assert_eq!(
            links[0].anchor,
            Some(Anchor::Heading("My Heading".to_string()))
        );
    }

    #[test]
    fn test_extract_link_with_custom_id_anchor() {
        let src = "[[file:foo.org::#sec-intro][intro]]\n";
        let links = extract_file_links(src);
        assert_eq!(
            links[0].anchor,
            Some(Anchor::CustomId("sec-intro".to_string()))
        );
    }

    #[test]
    fn test_extract_link_with_line_number_anchor() {
        let src = "[[file:foo.txt::42][line 42]]\n";
        let links = extract_file_links(src);
        assert_eq!(links[0].anchor, Some(Anchor::LineNumber(42)));
    }

    #[test]
    fn test_skips_non_file_links() {
        let src = indoc! {"
            See [[https://example.com][site]] and [[*Heading]].
        "};
        let links = extract_file_links(src);
        assert!(links.is_empty(), "got: {:?}", links);
    }

    #[test]
    fn test_percent_decoded_path() {
        let src = "[[file:my%20folder/doc.org][doc]]\n";
        let links = extract_file_links(src);
        assert_eq!(links[0].target_path, PathBuf::from("my folder/doc.org"));
    }

    #[test]
    fn test_absolute_path() {
        let src = "[[file:/etc/hosts][hosts]]\n";
        let links = extract_file_links(src);
        assert_eq!(links[0].target_path, PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn test_anchor_index_collects_headings_and_custom_ids() {
        let src = indoc! {"
            * First
              :PROPERTIES:
              :CUSTOM_ID: first-id
              :END:
              body
            ** Second   Sub
            * TODO Third
        "};
        let idx = AnchorIndex::from_org_content(src);
        assert_eq!(
            idx.headings,
            vec!["First", "Second Sub", "Third"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(idx.custom_ids, vec!["first-id"]);
    }

    #[test]
    fn test_anchor_index_has_heading_exact_collapses_whitespace() {
        let src = "* My  Long\tHeading\n";
        let idx = AnchorIndex::from_org_content(src);
        assert!(idx.has_heading_exact("My Long Heading"));
        assert!(idx.has_heading_exact("My  Long  Heading"));
        assert!(
            !idx.has_heading_exact("my long heading"),
            "case must matter"
        );
    }

    #[test]
    fn test_anchor_index_substring_matches_org_mode_lenient_rule() {
        let src = indoc! {"
            * [2026-04-06 Mon] Témoignage — antécédents : asthme, appendicectomie
            * Another One
        "};
        let idx = AnchorIndex::from_org_content(src);
        // Real archive pattern: anchor is a substring of the full
        // timestamped heading.
        assert!(idx.has_heading_substring("Témoignage — antécédents"));
        // Exact match would fail for the same anchor.
        assert!(!idx.has_heading_exact("Témoignage — antécédents"));
        // Unrelated substring still rejected.
        assert!(!idx.has_heading_substring("No such thing"));
        // Empty needle never matches (avoid vacuous match).
        assert!(!idx.has_heading_substring(""));
    }

    #[test]
    fn test_line_count_counts_addressable_lines() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("a"), 1);
        assert_eq!(line_count("a\n"), 1);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count("a\nb\n"), 2);
    }

    #[test]
    fn test_offset_to_line_col_basics() {
        let s = "abc\ndef\n";
        assert_eq!(offset_to_line_col(s, 0), (1, 1));
        assert_eq!(offset_to_line_col(s, 3), (1, 4));
        assert_eq!(offset_to_line_col(s, 4), (2, 1));
        assert_eq!(offset_to_line_col(s, 7), (2, 4));
    }

    #[test]
    fn test_break_reason_tag_and_message() {
        assert_eq!(BreakReason::PathMissing.tag(), "path-missing");
        let r = BreakReason::HeadingNotFound {
            heading: "X".into(),
        };
        assert!(r.message().contains("X"));
    }
}
