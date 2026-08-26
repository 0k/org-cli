//! Coherent, versioned derivation of one org file's read model.
//!
//! [`FileSnapshot`] is the sole boundary that turns one exact content
//! value into its SHA-256 ETag, effective TODO keyword set, and parsed
//! headings. Callers must supply the fallback keyword set they would
//! otherwise use so the snapshot advertises the configuration that
//! actually produced its headings.

use std::fs;
use std::io;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::org::heading::{Heading, iter_headings_with};
use crate::org::todo_keywords::{TodoKeywords, scan_buffer};

/// One coherent read of an org file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshot {
    /// Lowercase hex SHA-256 of the exact bytes that produced this snapshot.
    pub etag: String,
    /// The effective TODO keyword set used to parse this snapshot.
    pub keywords: TodoKeywords,
    /// Every heading parsed from the same bytes as `etag` and `keywords`.
    pub headings: Vec<Heading>,
}

impl FileSnapshot {
    /// Derive a coherent snapshot from one content value.
    pub fn from_content(content: &str, fallback: &TodoKeywords) -> Self {
        let etag = sha256_hex(content.as_bytes());
        let keywords = scan_buffer(content).unwrap_or_else(|| fallback.clone());
        let headings = iter_headings_with(content, &keywords).collect();
        Self {
            etag,
            keywords,
            headings,
        }
    }

    /// Read `path` exactly once, then derive all snapshot fields from it.
    pub fn read(path: &Path, fallback: &TodoKeywords) -> io::Result<Self> {
        let content = fs::read_to_string(path)?;
        Ok(Self::from_content(&content, fallback))
    }
}

/// Return the lowercase hexadecimal SHA-256 digest of `bytes`.
///
/// Kept here so file ETags and heading-line fingerprints share one
/// encoding implementation rather than scattering hash helpers.
pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::FileSnapshot;
    use crate::org::todo_keywords::TodoKeywords;

    const SNAPSHOT_ETAG: &str = "f1c0d3be95cce5afc2ac5deb0a3da077c0cf07f1ac13fe7fd6b88cc3568e0c2e";
    const HEADING_FINGERPRINT: &str =
        "5fc78d46c13f01b31e3d5e89cdad4cfdc83475d5600c8d8d332b668a14eb4618";

    fn custom_keywords() -> TodoKeywords {
        TodoKeywords {
            active: vec!["TODO".to_string(), "WIP".to_string()],
            done: vec!["DONE".to_string(), "CANCELED".to_string()],
        }
    }

    #[test]
    fn etag_is_sha256_of_exact_snapshot_bytes() {
        let content = "* TODO [#A] x\n";
        let snapshot = FileSnapshot::from_content(content, &TodoKeywords::default());

        assert_eq!(snapshot.etag, SNAPSHOT_ETAG);
        assert_eq!(
            FileSnapshot::from_content(content, &TodoKeywords::default()).etag,
            snapshot.etag
        );
        assert_ne!(
            FileSnapshot::from_content("* TODO [#A] y\n", &TodoKeywords::default()).etag,
            snapshot.etag
        );
    }

    #[test]
    fn snapshot_advertises_declared_keywords_used_to_parse_headings() {
        let content = "#+TODO: TODO WIP | DONE CANCELED\n* WIP Working\n";
        let snapshot = FileSnapshot::from_content(content, &TodoKeywords::default());

        assert_eq!(snapshot.keywords, custom_keywords());
        assert_eq!(snapshot.headings[0].keyword.as_deref(), Some("WIP"));
    }

    #[test]
    fn snapshot_advertises_the_fallback_actually_passed() {
        let content = "* WIP Working\n";
        let default_snapshot = FileSnapshot::from_content(content, &TodoKeywords::default());
        let custom = custom_keywords();
        let custom_snapshot = FileSnapshot::from_content(content, &custom);

        assert_eq!(default_snapshot.keywords, TodoKeywords::default());
        assert_eq!(default_snapshot.headings[0].keyword, None);
        assert_eq!(custom_snapshot.keywords, custom);
        assert_eq!(custom_snapshot.headings[0].keyword.as_deref(), Some("WIP"));
    }

    #[test]
    fn fingerprint_is_sha256_of_exact_heading_line_only() {
        let original =
            FileSnapshot::from_content("* TODO [#A] x\nOriginal body\n", &TodoKeywords::default());
        let body_changed = FileSnapshot::from_content(
            "* TODO [#A] x\nChanged body with more bytes\n",
            &TodoKeywords::default(),
        );
        let heading_changed =
            FileSnapshot::from_content("* TODO [#A] y\nOriginal body\n", &TodoKeywords::default());

        assert_eq!(original.headings[0].fingerprint, HEADING_FINGERPRINT);
        assert_eq!(
            original.headings[0].fingerprint,
            body_changed.headings[0].fingerprint
        );
        assert_ne!(original.etag, body_changed.etag);
        assert_ne!(
            original.headings[0].fingerprint,
            heading_changed.headings[0].fingerprint
        );
    }

    #[test]
    fn fingerprint_excludes_lf_and_crlf_line_terminators() {
        let with_lf = FileSnapshot::from_content("* TODO x\n", &TodoKeywords::default());
        let without_terminator = FileSnapshot::from_content("* TODO x", &TodoKeywords::default());
        let with_crlf = FileSnapshot::from_content("* TODO x\r\n", &TodoKeywords::default());

        assert_eq!(
            with_lf.headings[0].fingerprint,
            without_terminator.headings[0].fingerprint
        );
        assert_eq!(
            with_crlf.headings[0].fingerprint,
            without_terminator.headings[0].fingerprint
        );
    }

    #[test]
    fn fingerprint_keeps_a_lone_carriage_return_as_line_data() {
        let plain = FileSnapshot::from_content("* TODO x", &TodoKeywords::default());
        let with_lone_cr = FileSnapshot::from_content("* TODO x\r", &TodoKeywords::default());

        assert_ne!(
            plain.headings[0].fingerprint,
            with_lone_cr.headings[0].fingerprint
        );
    }

    #[test]
    fn identical_heading_lines_have_identical_non_unique_fingerprints() {
        let snapshot = FileSnapshot::from_content(
            "* TODO [#A] x\nBody one\n* TODO [#A] x\nBody two\n",
            &TodoKeywords::default(),
        );

        assert_eq!(snapshot.headings.len(), 2);
        assert_eq!(
            snapshot.headings[0].fingerprint,
            snapshot.headings[1].fingerprint
        );
    }
}
