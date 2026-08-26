//! Pure, byte-faithful heading edit operations used by
//! ``org-cli heading …``.
//!
//! Every operation follows the same shape established by
//! [`crate::org::clock_add::apply_add`]: it takes the whole file
//! `content` plus a 1-based `heading_line` addressing the target
//! heading, and returns `Result<(String, EditRecord), EditError>` — the
//! rewritten content and a structured record of what changed. Callers
//! resolve a [`crate::org::heading::HeadingPath`] to a line number
//! *before* calling into this module; path resolution is not repeated
//! here.
//!
//! # Vital invariant
//!
//! Every line **outside** the edited region stays byte-for-byte
//! identical, and the presence or absence of a trailing newline is
//! preserved. Operations are designed so that an op and its inverse
//! round-trip to the original bytes (e.g. `set_keyword(Some) →
//! set_keyword(None)`, `set_property → unset_property`,
//! `extract_subtree → insert_subtree` at the same place).
//!
//! # Line-ending contract
//!
//! Heading edits accept LF-only text. Any `\r\n` (CRLF) terminator is
//! rejected with an error naming its first 1-based line, before an edit
//! can normalize bytes elsewhere in the file. A lone `\r` not followed
//! by `\n` is ordinary data and is preserved byte-for-byte on untouched
//! lines.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::org::heading::{direct_child_lines, section_span, subtree_span};
use crate::org::property::parse_property_line;
use crate::org::todo_keywords::TodoKeywords;

// ---------------------------------------------------------------------
// Records + errors
// ---------------------------------------------------------------------

/// Structured description of a single edit, suitable for `-j` output.
///
/// `op` names the operation (`"set-keyword"`, `"set-title"`, …); `line`
/// is the 1-based heading line that was targeted. The remaining fields
/// are populated only when relevant to the operation, and skipped from
/// JSON when `None`/empty so each record stays terse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EditRecord {
    /// The operation kind.
    pub op: &'static str,
    /// 1-based heading line the edit targeted (for `add`, the line of the
    /// newly inserted heading).
    pub line: usize,
    /// Previous scalar value, when the op replaces one (keyword, title,
    /// property value, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    /// New scalar value, when the op sets one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
    /// Property key, for property operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Affected span `[start, end]` (1-based inclusive), for delete /
    /// extract / content operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<[usize; 2]>,
}

impl EditRecord {
    /// Build a minimal record carrying just the op name and line.
    fn new(op: &'static str, line: usize) -> Self {
        Self {
            op,
            line,
            old: None,
            new: None,
            key: None,
            span: None,
        }
    }
}

/// Everything that can go wrong applying a heading edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// The source or caller-supplied text contains a CRLF terminator.
    UnsupportedCrlf { line: usize },
    /// `heading_line` (or a referenced parent line) is not a heading.
    NotAHeading { line: usize },
    /// A keyword was rejected because it is not in the active∪done set.
    InvalidKeyword {
        keyword: String,
        allowed: Vec<String>,
    },
    /// A priority was not one of the three supported org letters.
    InvalidPriority { priority: String },
    /// A tag was empty or contained a `:`.
    InvalidTag { tag: String },
    /// `unset_property` referenced a key that is not present.
    PropertyNotFound { key: String, line: usize },
    /// A sibling position index was out of range for the destination.
    PositionOutOfRange { index: usize, sibling_count: usize },
    /// Re-leveling a subtree during insert would drive a heading below
    /// level 1.
    LevelUnderflow { requested_shift: i64 },
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::UnsupportedCrlf { line } => {
                write!(
                    f,
                    "unsupported CRLF line ending at line {}; heading edits accept LF-only text",
                    line
                )
            }
            EditError::NotAHeading { line } => {
                write!(f, "line {} is not a heading", line)
            }
            EditError::InvalidKeyword { keyword, allowed } => {
                write!(
                    f,
                    "invalid TODO keyword {:?}; allowed: {}",
                    keyword,
                    if allowed.is_empty() {
                        "(none)".to_string()
                    } else {
                        allowed.join(", ")
                    }
                )
            }
            EditError::InvalidPriority { priority } => {
                write!(f, "invalid priority {:?}; expected A, B, or C", priority)
            }
            EditError::InvalidTag { tag } => {
                write!(
                    f,
                    "invalid tag {:?}: must be non-empty and contain no ':'",
                    tag
                )
            }
            EditError::PropertyNotFound { key, line } => {
                write!(
                    f,
                    "property {:?} not found under heading at line {}",
                    key, line
                )
            }
            EditError::PositionOutOfRange {
                index,
                sibling_count,
            } => {
                write!(
                    f,
                    "sibling position {} out of range (there are {} sibling(s))",
                    index, sibling_count
                )
            }
            EditError::LevelUnderflow { requested_shift } => {
                write!(
                    f,
                    "re-leveling by {} would move a heading above the top level",
                    requested_shift
                )
            }
        }
    }
}

impl std::error::Error for EditError {}

// ---------------------------------------------------------------------
// Line-vector helpers (shared shape with clock_add)
// ---------------------------------------------------------------------

/// Reject the first CRLF terminator in `content` and report its 1-based
/// line number. Lone carriage returns remain ordinary data.
fn ensure_lf_only(content: &str) -> Result<(), EditError> {
    if let Some(byte_index) = content
        .as_bytes()
        .windows(2)
        .position(|pair| pair == b"\r\n")
    {
        let line = content.as_bytes()[..byte_index]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count()
            + 1;
        return Err(EditError::UnsupportedCrlf { line });
    }
    Ok(())
}

/// Split LF-only `content` into owned lines plus a flag recording
/// whether the original ended with a newline.
fn split_lines(content: &str) -> Result<(Vec<String>, bool), EditError> {
    ensure_lf_only(content)?;
    let lines = content.lines().map(str::to_string).collect();
    Ok((lines, content.ends_with('\n')))
}

/// Reassemble lines into a single string, restoring the trailing newline
/// when `trailing_newline` is set.
fn join_lines(lines: &[String], trailing_newline: bool) -> String {
    let mut out = lines.join("\n");
    if trailing_newline && (!lines.is_empty() || out.is_empty()) {
        // Preserve the trailing newline. When `lines` is empty the source
        // was either "" (no trailing) or "\n" — `lines()` drops the latter
        // to zero lines, so we only reach here for a genuine "\n"-only file
        // if trailing_newline is true and out is "".
        out.push('\n');
    }
    out
}

/// Decide the result's trailing-newline after deleting the 1-based line
/// range `[start, end]` from an original with `original_count` lines.
///
/// If the deleted range reached the original last line, the new last
/// line was `\n`-terminated in the original (it was followed by the
/// removed lines), so the result ends with a newline. Otherwise the
/// original tail survives and keeps the original's trailing-newline
/// status.
fn trailing_after_removal(
    original_count: usize,
    original_trailing_newline: bool,
    end: usize,
) -> bool {
    if end >= original_count {
        true
    } else {
        original_trailing_newline
    }
}

/// Number of leading `*` characters if `line` is a heading (stars
/// followed by a space), else `None`.
fn heading_level(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut level = 0;
    while level < bytes.len() && bytes[level] == b'*' {
        level += 1;
    }
    if level == 0 || level >= bytes.len() || bytes[level] != b' ' {
        return None;
    }
    Some(level)
}

/// Validate that `heading_line` (1-based) points at a real heading and
/// return the raw line. Uses the orgize-backed [`subtree_span`] so
/// `*bold*` and other non-headings are rejected consistently with the
/// parser.
fn heading_raw(lines: &[String], heading_line: usize) -> Option<String> {
    let raw = lines.get(heading_line.checked_sub(1)?)?;
    heading_level(raw)?;
    Some(raw.clone())
}

// ---------------------------------------------------------------------
// Heading-line model (byte-faithful parse + render)
// ---------------------------------------------------------------------

/// A parsed org heading line, decomposed so each component can be
/// rewritten independently while preserving the untouched ones.
///
/// The rendered form is:
/// `stars SP [keyword SP] [priority SP] title [align tags]`
/// where `tag_col` records the 0-based character column at which the tag
/// block started in the source, so alignment can be reproduced.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HeadingLineModel {
    /// Number of leading stars (heading level).
    stars: usize,
    /// TODO keyword, when the first word is a recognized state.
    keyword: Option<String>,
    /// Priority cookie including brackets, e.g. `[#A]`.
    priority: Option<String>,
    /// Title text with surrounding whitespace trimmed.
    title: String,
    /// Tags, without the surrounding/separating colons.
    tags: Vec<String>,
    /// 0-based char column where the tag block began, when tags existed.
    tag_col: Option<usize>,
}

/// True when `word` looks like a priority cookie: `[#X]` for a single
/// non-space, non-`]` character `X`.
fn is_priority_cookie(word: &str) -> bool {
    let bytes = word.as_bytes();
    word.len() == 4
        && bytes[0] == b'['
        && bytes[1] == b'#'
        && bytes[3] == b']'
        && bytes[2] != b' '
        && bytes[2] != b']'
}

/// Split a trailing tag block off `text`, returning `(head, tags,
/// tag_block_byte_start)`.
///
/// A tag block is a trailing run matching `:(word:)+` preceded by
/// whitespace (or the start of the meaningful text). `head` is the text
/// before the tag block with its own trailing whitespace trimmed;
/// `tag_block_byte_start` is the byte offset **into `text`** at which the
/// tag block's leading `:` sits (so callers can convert it to an absolute
/// column). Returns `tags` empty and `None` when there is no tag block.
fn split_tags(text: &str) -> (String, Vec<String>, Option<usize>) {
    let trimmed_end = text.trim_end();
    // The tag block is the final whitespace-delimited token, and must be
    // shaped `:a:b:` — at least two colons, no interior whitespace, and
    // every colon-separated field non-empty.
    let last_ws = trimmed_end
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8());
    let candidate_start = last_ws.unwrap_or(0);
    let candidate = &trimmed_end[candidate_start..];

    if is_tag_block(candidate) {
        let tags: Vec<String> = candidate
            .trim_matches(':')
            .split(':')
            .map(str::to_string)
            .collect();
        let head = trimmed_end[..candidate_start].trim_end().to_string();
        (head, tags, Some(candidate_start))
    } else {
        (trimmed_end.to_string(), Vec::new(), None)
    }
}

/// True when `s` is a well-formed org tag block: `:a:b:c:` with at least
/// one tag and no empty fields or interior whitespace.
fn is_tag_block(s: &str) -> bool {
    if s.len() < 2 || !s.starts_with(':') || !s.ends_with(':') {
        return false;
    }
    if s.chars().any(char::is_whitespace) {
        return false;
    }
    let inner = &s[1..s.len() - 1];
    if inner.is_empty() {
        return false;
    }
    inner.split(':').all(|field| !field.is_empty())
}

impl HeadingLineModel {
    /// Parse a raw heading line. `keywords` decides which leading word is
    /// treated as a TODO keyword. The caller must have already ensured
    /// `raw` is a heading line.
    fn parse(raw: &str, keywords: &TodoKeywords) -> Self {
        let stars = heading_level(raw).unwrap_or(0);
        // Byte offset within `raw` where the non-star text begins (after
        // the stars and their following spaces).
        let after_stars_offset =
            stars + raw[stars..].len() - raw[stars..].trim_start_matches(' ').len();
        let after_stars = &raw[after_stars_offset..];
        let (head, tags, tag_block_offset) = split_tags(after_stars);

        // Convert the tag block's byte offset within `after_stars` into an
        // absolute character column within the full rendered line — but
        // only when the tags were *deliberately aligned*. A single space
        // (or none) before the tag block is the org default, not
        // alignment: recording a column there would make round-trips add
        // padding. We treat a gap of two or more whitespace characters as
        // intentional column alignment worth preserving.
        let tag_col = tag_block_offset.and_then(|off| {
            let gap = after_stars[head.len()..off].chars().count();
            if gap >= 2 {
                Some(raw[..after_stars_offset + off].chars().count())
            } else {
                None
            }
        });

        let mut rest = head.as_str();
        let mut keyword = None;
        let mut priority = None;

        // Keyword: first whitespace-delimited word if recognized.
        if let Some((first, remainder)) = split_first_word(rest)
            && is_known_keyword(first, keywords)
        {
            keyword = Some(first.to_string());
            rest = remainder;
        }

        // Priority cookie: next word if shaped `[#X]`.
        if let Some((first, remainder)) = split_first_word(rest)
            && is_priority_cookie(first)
        {
            priority = Some(first.to_string());
            rest = remainder;
        }

        let title = rest.trim().to_string();

        HeadingLineModel {
            stars,
            keyword,
            priority,
            title,
            tags,
            tag_col,
        }
    }

    /// Render back to a single line, reproducing the tag column when
    /// tags are present (falling back to a single space when the prefix
    /// is already at/over the recorded column).
    fn render(&self) -> String {
        let mut prefix = String::new();
        prefix.push_str(&"*".repeat(self.stars));
        prefix.push(' ');
        if let Some(kw) = &self.keyword {
            prefix.push_str(kw);
            prefix.push(' ');
        }
        if let Some(pri) = &self.priority {
            prefix.push_str(pri);
            prefix.push(' ');
        }
        prefix.push_str(&self.title);

        if self.tags.is_empty() {
            return prefix;
        }

        let tag_block = format!(":{}:", self.tags.join(":"));
        match self.tag_col {
            Some(col) => {
                let current = prefix.chars().count();
                if col > current {
                    // Pad with spaces to reach the original column.
                    let pad = col - current;
                    prefix.push_str(&" ".repeat(pad));
                } else {
                    // Title grew past the column: single-space fallback.
                    prefix.push(' ');
                }
            }
            None => prefix.push(' '),
        }
        prefix.push_str(&tag_block);
        prefix
    }
}

/// Split `s` into its first whitespace-delimited word and the remainder
/// (with leading whitespace of the remainder trimmed). Returns `None`
/// when `s` is empty/whitespace-only.
fn split_first_word(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    match s.find(char::is_whitespace) {
        Some(idx) => {
            let word = &s[..idx];
            let rest = s[idx..].trim_start();
            Some((word, rest))
        }
        None => Some((s, "")),
    }
}

/// True when `word` is in the active∪done keyword set.
fn is_known_keyword(word: &str, keywords: &TodoKeywords) -> bool {
    keywords.active.iter().any(|k| k == word) || keywords.done.iter().any(|k| k == word)
}

/// The union of active and done keywords, active first, for error
/// messages and validation.
fn allowed_keywords(keywords: &TodoKeywords) -> Vec<String> {
    let mut all = keywords.active.clone();
    all.extend(keywords.done.iter().cloned());
    all
}

// ---------------------------------------------------------------------
// Heading-line editing ops
// ---------------------------------------------------------------------

/// Set or remove the heading's TODO keyword.
///
/// `Some(kw)` sets the keyword (validated against `keywords`); `None`
/// removes it. Only the heading line is rewritten; the keyword/priority/
/// tag layout and tag column are preserved.
pub fn set_keyword(
    content: &str,
    heading_line: usize,
    keyword: Option<&str>,
    keywords: &TodoKeywords,
) -> Result<(String, EditRecord), EditError> {
    let (mut lines, trailing_newline) = split_lines(content)?;
    let raw =
        heading_raw(&lines, heading_line).ok_or(EditError::NotAHeading { line: heading_line })?;

    if let Some(kw) = keyword
        && !is_known_keyword(kw, keywords)
    {
        return Err(EditError::InvalidKeyword {
            keyword: kw.to_string(),
            allowed: allowed_keywords(keywords),
        });
    }

    let mut model = HeadingLineModel::parse(&raw, keywords);
    let old = model.keyword.clone();
    model.keyword = keyword.map(str::to_string);
    let rendered = model.render();
    lines[heading_line - 1] = rendered.clone();

    let mut record = EditRecord::new("set-keyword", heading_line);
    record.old = old;
    record.new = keyword.map(str::to_string);
    Ok((join_lines(&lines, trailing_newline), record))
}

/// Set or remove the heading's priority cookie.
///
/// `Some("A" | "B" | "C")` stores the matching org cookie; `None`
/// removes it. No priority means the heading has not yet been evaluated,
/// not that it has low priority. `keywords` determines the keyword/title
/// boundary so custom TODO states remain before the cookie.
pub fn set_priority(
    content: &str,
    heading_line: usize,
    priority: Option<&str>,
    keywords: &TodoKeywords,
) -> Result<(String, EditRecord), EditError> {
    if let Some(priority) = priority
        && !matches!(priority, "A" | "B" | "C")
    {
        return Err(EditError::InvalidPriority {
            priority: priority.to_string(),
        });
    }

    let (mut lines, trailing_newline) = split_lines(content)?;
    let raw =
        heading_raw(&lines, heading_line).ok_or(EditError::NotAHeading { line: heading_line })?;
    let mut model = HeadingLineModel::parse(&raw, keywords);
    let old = model
        .priority
        .as_deref()
        .and_then(|cookie| cookie.strip_prefix("[#"))
        .and_then(|letter| letter.strip_suffix(']'))
        .map(str::to_string);
    model.priority = priority.map(|letter| format!("[#{}]", letter));
    lines[heading_line - 1] = model.render();

    let mut record = EditRecord::new("set-priority", heading_line);
    record.old = old;
    record.new = priority.map(str::to_string);
    Ok((join_lines(&lines, trailing_newline), record))
}

/// Replace the heading's title, preserving keyword, priority cookie,
/// tags, and the tag column when possible.
pub fn set_title(
    content: &str,
    heading_line: usize,
    title: &str,
    keywords: &TodoKeywords,
) -> Result<(String, EditRecord), EditError> {
    let (mut lines, trailing_newline) = split_lines(content)?;
    let raw =
        heading_raw(&lines, heading_line).ok_or(EditError::NotAHeading { line: heading_line })?;

    let mut model = HeadingLineModel::parse(&raw, keywords);
    let old = model.title.clone();
    model.title = title.trim().to_string();
    lines[heading_line - 1] = model.render();

    let mut record = EditRecord::new("set-title", heading_line);
    record.old = Some(old);
    record.new = Some(title.trim().to_string());
    Ok((join_lines(&lines, trailing_newline), record))
}

/// Replace the heading's tag block.
///
/// An empty `tags` slice removes the tag block entirely (and the spacing
/// that only served to align it). Adding tags to a line that had none
/// uses a single-space separator; replacing existing tags keeps the
/// original tag column. Each tag must be non-empty and free of `:`.
pub fn set_tags(
    content: &str,
    heading_line: usize,
    tags: &[String],
) -> Result<(String, EditRecord), EditError> {
    for tag in tags {
        if tag.is_empty() || tag.contains(':') {
            return Err(EditError::InvalidTag { tag: tag.clone() });
        }
    }

    let (mut lines, trailing_newline) = split_lines(content)?;
    let raw =
        heading_raw(&lines, heading_line).ok_or(EditError::NotAHeading { line: heading_line })?;

    let mut model = HeadingLineModel::parse(&raw, &TodoKeywords::default());
    let old = model.tags.clone();
    if tags.is_empty() {
        model.tags = Vec::new();
        model.tag_col = None;
    } else {
        model.tags = tags.to_vec();
        // Keep existing column; if the line had none, render() uses a
        // single space (tag_col stays None).
    }
    lines[heading_line - 1] = model.render();

    let mut record = EditRecord::new("set-tags", heading_line);
    record.old = Some(old.join(":"));
    record.new = Some(tags.join(":"));
    Ok((join_lines(&lines, trailing_newline), record))
}

// ---------------------------------------------------------------------
// Property drawer editing
// ---------------------------------------------------------------------

/// Detect the `:PROPERTIES: … :END:` drawer attached directly to the
/// heading at `heading_line`, returning `(start_line, end_line, indent)`
/// (1-based inclusive). Scans only the heading's own section, stopping at
/// the first child heading.
fn find_properties_drawer(lines: &[String], heading_line: usize) -> Option<(usize, usize, String)> {
    let mut start: Option<(usize, String)> = None;
    for (i, line) in lines.iter().enumerate().skip(heading_line) {
        if heading_level(line).is_some() {
            break;
        }
        let trimmed = line.trim_start();
        if start.is_none() && trimmed.eq_ignore_ascii_case(":PROPERTIES:") {
            let indent = line[..line.len() - trimmed.len()].to_string();
            start = Some((i + 1, indent));
            continue;
        }
        if let Some((s, indent)) = &start
            && trimmed.eq_ignore_ascii_case(":END:")
        {
            return Some((*s, i + 1, indent.clone()));
        }
    }
    None
}

/// Pick the 1-based line *after which* a freshly-created `:PROPERTIES:`
/// drawer should be inserted: after the heading line and any planning
/// line (`CLOSED:`/`SCHEDULED:`/`DEADLINE:`), before body text.
fn properties_insert_point(lines: &[String], heading_line: usize) -> usize {
    let mut after = heading_line;
    let mut i = heading_line; // 0-based index of the line below the heading.
    while i < lines.len() {
        let line = &lines[i];
        if heading_level(line).is_some() {
            break;
        }
        let first = line.split_whitespace().next().unwrap_or("");
        if matches!(first, "CLOSED:" | "SCHEDULED:" | "DEADLINE:") {
            after = i + 1;
            i += 1;
            continue;
        }
        break;
    }
    after
}

/// Set a property `key = value` under the heading at `heading_line`.
///
/// Updates the value in place when the key already exists (preserving the
/// drawer's indentation), otherwise appends the entry before `:END:`.
/// Creates a `:PROPERTIES:` drawer when none exists.
pub fn set_property(
    content: &str,
    heading_line: usize,
    key: &str,
    value: &str,
) -> Result<(String, EditRecord), EditError> {
    let (mut lines, trailing_newline) = split_lines(content)?;
    heading_raw(&lines, heading_line).ok_or(EditError::NotAHeading { line: heading_line })?;

    let old_value;
    match find_properties_drawer(&lines, heading_line) {
        Some((start, end, indent)) => {
            // Look for an existing entry for `key`.
            let mut found: Option<usize> = None;
            for (i, line) in lines.iter().enumerate().take(end - 1).skip(start) {
                if let Some(p) = parse_property_line(line)
                    && p.key == key
                {
                    found = Some(i);
                    break;
                }
            }
            match found {
                Some(i) => {
                    old_value = parse_property_line(&lines[i]).map(|p| p.value);
                    lines[i] = format!("{}:{}: {}", indent, key, value);
                }
                None => {
                    old_value = None;
                    // Insert before the :END: line (0-based end-1).
                    lines.insert(end - 1, format!("{}:{}: {}", indent, key, value));
                }
            }
        }
        None => {
            old_value = None;
            let after = properties_insert_point(&lines, heading_line);
            let indent = drawer_indent();
            let block = vec![
                format!("{}:PROPERTIES:", indent),
                format!("{}:{}: {}", indent, key, value),
                format!("{}:END:", indent),
            ];
            for (k, l) in block.into_iter().enumerate() {
                lines.insert(after + k, l);
            }
        }
    }

    let mut record = EditRecord::new("set-property", heading_line);
    record.key = Some(key.to_string());
    record.old = old_value;
    record.new = Some(value.to_string());
    Ok((join_lines(&lines, trailing_newline), record))
}

/// Remove a property `key` under the heading at `heading_line`.
///
/// Errors when the key is absent. Removing the last entry drops the whole
/// `:PROPERTIES:` drawer.
pub fn unset_property(
    content: &str,
    heading_line: usize,
    key: &str,
) -> Result<(String, EditRecord), EditError> {
    let (mut lines, trailing_newline) = split_lines(content)?;
    heading_raw(&lines, heading_line).ok_or(EditError::NotAHeading { line: heading_line })?;

    let (start, end, _indent) = find_properties_drawer(&lines, heading_line).ok_or_else(|| {
        EditError::PropertyNotFound {
            key: key.to_string(),
            line: heading_line,
        }
    })?;

    // Find the entry line for `key` within (start, end) exclusive of the
    // :PROPERTIES:/:END: markers.
    let mut target: Option<usize> = None;
    for (i, line) in lines.iter().enumerate().take(end - 1).skip(start) {
        if let Some(p) = parse_property_line(line)
            && p.key == key
        {
            target = Some(i);
            break;
        }
    }
    let target = target.ok_or_else(|| EditError::PropertyNotFound {
        key: key.to_string(),
        line: heading_line,
    })?;

    let old_value = parse_property_line(&lines[target]).map(|p| p.value);

    // Entry count in the drawer (between markers).
    let entry_count = end - 1 - start;
    if entry_count <= 1 {
        // Removing the only entry: drop the whole drawer (start..=end).
        lines.drain((start - 1)..end);
    } else {
        lines.remove(target);
    }

    let mut record = EditRecord::new("unset-property", heading_line);
    record.key = Some(key.to_string());
    record.old = old_value;
    Ok((join_lines(&lines, trailing_newline), record))
}

/// The indentation for a freshly created `:PROPERTIES:` drawer.
///
/// Uses the canonical two-space indent, matching the existing
/// `:LOGBOOK:` drawer creation in [`crate::org::clock_add`]. This keeps
/// drawer creation predictable and consistent across the editing layer
/// regardless of how the heading's body happens to be indented.
fn drawer_indent() -> String {
    "  ".to_string()
}

// ---------------------------------------------------------------------
// Section body editing
// ---------------------------------------------------------------------

/// Replace the section body of the heading at `heading_line`.
///
/// The body is everything after the heading line, its planning lines
/// (`CLOSED:`/`SCHEDULED:`/`DEADLINE:`) and its `:PROPERTIES:` /
/// `:LOGBOOK:` drawers, up to (exclusive) the first child heading or the
/// end of the section. Drawers and planning lines are preserved. A
/// `body` without a trailing newline is inserted as given (each line
/// joined by `\n`); an empty `body` clears the section body.
pub fn set_content(
    content: &str,
    heading_line: usize,
    body: &str,
) -> Result<(String, EditRecord), EditError> {
    let (lines, trailing_newline) = split_lines(content)?;
    ensure_lf_only(body)?;
    heading_raw(&lines, heading_line).ok_or(EditError::NotAHeading { line: heading_line })?;

    let section =
        section_span(content, heading_line).ok_or(EditError::NotAHeading { line: heading_line })?;
    // First body line (1-based) = after heading + planning + drawers.
    let body_start = section_body_start(&lines, heading_line);
    // Body occupies body_start..=section.end (1-based inclusive).
    let mut new_lines: Vec<String> = Vec::new();
    new_lines.extend_from_slice(&lines[..body_start - 1]);
    if !body.is_empty() {
        new_lines.extend(body.lines().map(str::to_string));
    }
    // Everything from section.end + 1 onward is untouched.
    if section.end < lines.len() {
        new_lines.extend_from_slice(&lines[section.end..]);
    }

    let result_trailing = trailing_after_removal(lines.len(), trailing_newline, section.end);

    let mut record = EditRecord::new("set-content", heading_line);
    record.span = Some([body_start, section.end]);
    Ok((join_lines(&new_lines, result_trailing), record))
}

/// The 1-based line number of the first body line under a heading:
/// after the heading line, planning lines, and any leading
/// `:PROPERTIES:` / `:LOGBOOK:` drawers.
fn section_body_start(lines: &[String], heading_line: usize) -> usize {
    let mut after = heading_line;
    let mut i = heading_line;
    while i < lines.len() {
        let line = &lines[i];
        if heading_level(line).is_some() {
            break;
        }
        let trimmed = line.trim_start();
        let first = trimmed.split_whitespace().next().unwrap_or("");
        if matches!(first, "CLOSED:" | "SCHEDULED:" | "DEADLINE:") {
            after = i + 1;
            i += 1;
            continue;
        }
        if trimmed.eq_ignore_ascii_case(":PROPERTIES:") || trimmed.eq_ignore_ascii_case(":LOGBOOK:")
        {
            // Skip to matching :END:.
            i += 1;
            while i < lines.len() {
                let inner = lines[i].trim_start();
                let is_end = inner.eq_ignore_ascii_case(":END:");
                i += 1;
                if is_end {
                    break;
                }
            }
            after = i;
            continue;
        }
        break;
    }
    after + 1
}

// ---------------------------------------------------------------------
// Add / delete / extract / insert
// ---------------------------------------------------------------------

/// Where to place a new or inserted heading among its siblings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddPosition {
    /// Before all existing siblings.
    First,
    /// After all existing siblings.
    Last,
    /// Immediately before the sibling at this 1-based index.
    Before(usize),
    /// Immediately after the sibling at this 1-based index.
    After(usize),
}

/// Build the text block for a new heading (heading line + optional
/// property drawer + optional body), each element as its own line.
fn render_new_heading(
    level: usize,
    keyword: Option<&str>,
    title: &str,
    tags: &[String],
    properties: &[(String, String)],
    body: Option<&str>,
) -> Vec<String> {
    let model = HeadingLineModel {
        stars: level,
        keyword: keyword.map(str::to_string),
        priority: None,
        title: title.trim().to_string(),
        tags: tags.to_vec(),
        tag_col: None,
    };
    let mut out = vec![model.render()];
    if !properties.is_empty() {
        out.push("  :PROPERTIES:".to_string());
        for (k, v) in properties {
            out.push(format!("  :{}: {}", k, v));
        }
        out.push("  :END:".to_string());
    }
    if let Some(b) = body
        && !b.is_empty()
    {
        out.extend(b.lines().map(str::to_string));
    }
    out
}

/// Compute the 0-based insertion index (into the line vector) for a new
/// sibling under `parent_line` at `pos`.
///
/// `parent_line` `None` addresses the top level (level-1 siblings). The
/// insertion point is placed after the full subtree of the sibling the
/// position references, so a new heading lands between complete subtrees.
fn sibling_insert_index(
    content: &str,
    lines: &[String],
    parent_line: Option<usize>,
    pos: AddPosition,
) -> Result<usize, EditError> {
    let sibling_lines: Vec<usize> = match parent_line {
        Some(p) => direct_child_lines(content, p),
        None => top_level_lines(content),
    };
    let count = sibling_lines.len();

    // Resolve to "insert after this sibling line" (Option: None = before
    // the first sibling / at section start).
    let after_sibling: Option<usize> = match pos {
        AddPosition::First => None,
        AddPosition::Last => sibling_lines.last().copied(),
        AddPosition::Before(n) => {
            if n < 1 || n > count {
                return Err(EditError::PositionOutOfRange {
                    index: n,
                    sibling_count: count,
                });
            }
            if n == 1 {
                None
            } else {
                Some(sibling_lines[n - 2])
            }
        }
        AddPosition::After(n) => {
            if n < 1 || n > count {
                return Err(EditError::PositionOutOfRange {
                    index: n,
                    sibling_count: count,
                });
            }
            Some(sibling_lines[n - 1])
        }
    };

    let insert_at = match after_sibling {
        Some(sib) => {
            // After the sibling's full subtree.
            let span = subtree_span(content, sib).ok_or(EditError::NotAHeading { line: sib })?;
            span.end // 0-based index == 1-based end line (insert after it).
        }
        None => match parent_line {
            // Before the first child: right after the parent's own
            // section prelude (heading + planning + drawers) so the new
            // heading precedes existing children but follows the parent's
            // own metadata.
            Some(p) => section_body_first_child_boundary(content, lines, p),
            // Top level, First: immediately before the first existing
            // top-level heading, so any document preamble (`#+TITLE:`,
            // `#+FILETAGS:`, …) that precedes it is preserved. When there
            // is no heading at all, fall back to the very start.
            None => sibling_lines.first().map(|&first| first - 1).unwrap_or(0),
        },
    };
    Ok(insert_at)
}

/// 0-based index just before the first child heading of `parent_line`
/// (or, when there is no child, the end of the parent's subtree). Used to
/// place a `First`/`Before(1)` child.
fn section_body_first_child_boundary(content: &str, lines: &[String], parent_line: usize) -> usize {
    let children = direct_child_lines(content, parent_line);
    match children.first() {
        Some(&first_child) => first_child - 1, // insert before that line
        None => subtree_span(content, parent_line)
            .map(|s| s.end)
            .unwrap_or(lines.len()),
    }
}

/// The 1-based lines of all top-level (level-1) headings.
fn top_level_lines(content: &str) -> Vec<usize> {
    crate::org::heading::iter_headings(content)
        .filter(|h| h.level == 1)
        .map(|h| h.line)
        .collect()
}

/// Insert a new heading under `parent_line` (or top level when `None`).
///
/// The new heading's level matches the parent's existing direct children
/// (see [`destination_child_level`]) — falling back to `parent.level + 1`
/// (or 1 at the top level) when there are none. Positioning is among the
/// parent's direct children per `pos`.
///
/// The wide parameter list mirrors the org heading's own anatomy
/// (keyword, title, tags, properties, body) and the plan's fixed API
/// shape; grouping them into a struct would only move the fan-out to the
/// call site. Same `#[allow]` precedent as
/// `crate::cli::action::headings`.
#[allow(clippy::too_many_arguments)]
pub fn add_heading(
    content: &str,
    parent_line: Option<usize>,
    pos: AddPosition,
    keyword: Option<&str>,
    title: &str,
    tags: &[String],
    properties: &[(String, String)],
    body: Option<&str>,
    keywords: &TodoKeywords,
) -> Result<(String, EditRecord), EditError> {
    for tag in tags {
        if tag.is_empty() || tag.contains(':') {
            return Err(EditError::InvalidTag { tag: tag.clone() });
        }
    }
    if let Some(kw) = keyword
        && !is_known_keyword(kw, keywords)
    {
        return Err(EditError::InvalidKeyword {
            keyword: kw.to_string(),
            allowed: allowed_keywords(keywords),
        });
    }

    let (mut lines, trailing_newline) = split_lines(content)?;
    if let Some(body) = body {
        ensure_lf_only(body)?;
    }

    // A new heading adopts the level of the parent's existing direct
    // children (org siblings share a level, including legal level-skips);
    // when there are none it falls back to `parent.level + 1`.
    let level = destination_child_level(content, &lines, parent_line)?;

    let insert_at = sibling_insert_index(content, &lines, parent_line, pos)?;
    let block = render_new_heading(level, keyword, title, tags, properties, body);
    let block_len = block.len();
    for (k, l) in block.into_iter().enumerate() {
        lines.insert(insert_at + k, l);
    }

    let new_line = insert_at + 1; // 1-based line of the new heading.
    let mut record = EditRecord::new("add-heading", new_line);
    record.new = Some(title.trim().to_string());
    record.span = Some([new_line, new_line + block_len - 1]);
    Ok((join_lines(&lines, trailing_newline), record))
}

/// Delete the full subtree rooted at `heading_line`.
pub fn delete_heading(
    content: &str,
    heading_line: usize,
) -> Result<(String, EditRecord), EditError> {
    let (mut lines, trailing_newline) = split_lines(content)?;
    heading_raw(&lines, heading_line).ok_or(EditError::NotAHeading { line: heading_line })?;
    let span =
        subtree_span(content, heading_line).ok_or(EditError::NotAHeading { line: heading_line })?;

    let original_count = lines.len();
    lines.drain((span.start - 1)..span.end);
    let result_trailing = trailing_after_removal(original_count, trailing_newline, span.end);

    let mut record = EditRecord::new("delete-heading", heading_line);
    record.span = Some([span.start, span.end]);
    Ok((join_lines(&lines, result_trailing), record))
}

/// Split the subtree at `heading_line` out of `content`.
///
/// Returns `(content_without, subtree_block, record)`: the content with
/// the subtree removed, and the extracted block **verbatim** (the exact
/// source lines, joined by `\n`, with a trailing newline). Extracting and
/// then re-inserting at the same position with an unchanged level
/// round-trips byte-identically.
pub fn extract_subtree(
    content: &str,
    heading_line: usize,
) -> Result<(String, String, EditRecord), EditError> {
    let (mut lines, trailing_newline) = split_lines(content)?;
    heading_raw(&lines, heading_line).ok_or(EditError::NotAHeading { line: heading_line })?;
    let span =
        subtree_span(content, heading_line).ok_or(EditError::NotAHeading { line: heading_line })?;

    let block_lines: Vec<String> = lines[(span.start - 1)..span.end].to_vec();
    let mut block = block_lines.join("\n");
    block.push('\n');

    let original_count = lines.len();
    lines.drain((span.start - 1)..span.end);
    let result_trailing = trailing_after_removal(original_count, trailing_newline, span.end);

    let mut record = EditRecord::new("extract-subtree", heading_line);
    record.span = Some([span.start, span.end]);
    Ok((join_lines(&lines, result_trailing), block, record))
}

/// Destination for [`insert_subtree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsertDest {
    /// Parent heading line, or `None` for the top level.
    pub parent_line: Option<usize>,
    /// Position among the parent's direct children.
    pub pos: AddPosition,
}

/// The level a new direct child should take under `parent_line` (or the
/// top level when `None`).
///
/// When the parent already has direct children, the new child adopts the
/// level of the **first existing child** — org headings that share a
/// parent share a level, and real files legally *skip* levels (a level-4
/// parent with level-6 children). Matching the existing sibling level is
/// what makes an extract-then-insert-at-the-same-place round trip
/// byte-identical. Only when there are no existing children (nothing to
/// match) does it fall back to `parent.level + 1` (or 1 at the top).
fn destination_child_level(
    content: &str,
    lines: &[String],
    parent_line: Option<usize>,
) -> Result<usize, EditError> {
    let siblings: Vec<usize> = match parent_line {
        Some(p) => direct_child_lines(content, p),
        None => top_level_lines(content),
    };
    if let Some(&first) = siblings.first()
        && let Some(raw) = lines.get(first - 1)
        && let Some(level) = heading_level(raw)
    {
        return Ok(level);
    }
    match parent_line {
        Some(p) => {
            let raw = heading_raw(lines, p).ok_or(EditError::NotAHeading { line: p })?;
            Ok(heading_level(&raw).unwrap_or(1) + 1)
        }
        None => Ok(1),
    }
}

/// Insert a subtree `block` into `content` at `dest`, re-leveling the
/// block so its root sits at the destination depth.
///
/// The destination depth matches the level of the parent's existing
/// direct children when any exist (preserving legal level-skips), else
/// `parent.level + 1` (or 1 at the top level). Every heading star run in
/// the block is shifted by `dest_level - block_root_level`. Re-leveling
/// that would push any heading above the top level (< 1) is rejected.
pub fn insert_subtree(
    content: &str,
    block: &str,
    dest: InsertDest,
) -> Result<(String, EditRecord), EditError> {
    let (mut lines, trailing_newline) = split_lines(content)?;
    ensure_lf_only(block)?;

    let dest_level = destination_child_level(content, &lines, dest.parent_line)?;

    let block_lines: Vec<String> = block.lines().map(str::to_string).collect();
    let root_level = block_lines
        .iter()
        .find_map(|l| heading_level(l))
        .ok_or(EditError::NotAHeading { line: 0 })?;

    let shift: i64 = dest_level as i64 - root_level as i64;
    // Validate all headings stay >= 1.
    for l in &block_lines {
        if let Some(lvl) = heading_level(l) {
            let new_lvl = lvl as i64 + shift;
            if new_lvl < 1 {
                return Err(EditError::LevelUnderflow {
                    requested_shift: shift,
                });
            }
        }
    }

    let releveled: Vec<String> = block_lines
        .iter()
        .map(|l| match heading_level(l) {
            Some(lvl) => {
                let new_lvl = (lvl as i64 + shift) as usize;
                let rest = &l[lvl..]; // everything after the original stars
                format!("{}{}", "*".repeat(new_lvl), rest)
            }
            None => l.clone(),
        })
        .collect();

    let insert_at = sibling_insert_index(content, &lines, dest.parent_line, dest.pos)?;
    let block_len = releveled.len();
    for (k, l) in releveled.into_iter().enumerate() {
        lines.insert(insert_at + k, l);
    }

    let new_line = insert_at + 1;
    let mut record = EditRecord::new("insert-subtree", new_line);
    record.span = Some([new_line, new_line + block_len - 1]);
    Ok((join_lines(&lines, trailing_newline), record))
}

// ---------------------------------------------------------------------
// Atomic write
// ---------------------------------------------------------------------

/// A refusal or I/O failure while preparing an atomic replacement.
#[derive(Debug)]
pub enum AtomicWriteError {
    /// Existing-file metadata cannot be verified on this platform.
    UnsupportedPlatform { path: PathBuf },
    /// The requested path is a symlink whose final target is absent.
    DanglingSymlink { path: PathBuf },
    /// The resolved target is not a regular file.
    NotRegularFile { path: PathBuf },
    /// The target has names other than the one being replaced.
    HardLinked { path: PathBuf, nlink: u64 },
    /// The target carries xattrs or ACLs that replacement would discard.
    ExtendedMetadataPresent { path: PathBuf },
    /// Extended metadata could not be inspected reliably.
    CannotInspectExtendedMetadata { path: PathBuf, source: io::Error },
    /// The replacement could not receive the target's owner and group.
    CannotPreserveOwnership {
        path: PathBuf,
        uid: u32,
        gid: u32,
        source: io::Error,
    },
    /// The replacement could not receive the target's permission mode.
    CannotPreserveMode {
        path: PathBuf,
        mode: u32,
        source: io::Error,
    },
    /// A post-application stat did not prove exact metadata preservation.
    PreservationVerificationFailed { path: PathBuf, detail: String },
    /// An ordinary filesystem operation failed.
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl std::fmt::Display for AtomicWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform { path } => write!(
                f,
                "refusing atomic replacement of \"{}\": this platform cannot verify preservation of mode, ownership, and ACLs",
                path.display()
            ),
            Self::DanglingSymlink { path } => write!(
                f,
                "refusing atomic replacement of \"{}\": symlink target does not exist",
                path.display()
            ),
            Self::NotRegularFile { path } => write!(
                f,
                "refusing atomic replacement of \"{}\": target is not a regular file",
                path.display()
            ),
            Self::HardLinked { path, nlink } => write!(
                f,
                "refusing atomic replacement of \"{}\": target has {} hard links; replacement would leave the other links unchanged",
                path.display(),
                nlink
            ),
            Self::ExtendedMetadataPresent { path } => write!(
                f,
                "refusing atomic replacement of \"{}\": target has extended attributes or ACLs that this writer does not preserve",
                path.display()
            ),
            Self::CannotInspectExtendedMetadata { path, source } => write!(
                f,
                "refusing atomic replacement of \"{}\": cannot verify extended attributes or ACLs: {}",
                path.display(),
                source
            ),
            Self::CannotPreserveOwnership {
                path,
                uid,
                gid,
                source,
            } => write!(
                f,
                "refusing atomic replacement of \"{}\": cannot preserve uid {} and gid {}: {}",
                path.display(),
                uid,
                gid,
                source
            ),
            Self::CannotPreserveMode { path, mode, source } => write!(
                f,
                "refusing atomic replacement of \"{}\": cannot preserve mode {:#06o}: {}",
                path.display(),
                mode,
                source
            ),
            Self::PreservationVerificationFailed { path, detail } => write!(
                f,
                "refusing atomic replacement of \"{}\": replacement metadata verification failed: {}",
                path.display(),
                detail
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "{} \"{}\": {}", operation, path.display(), source),
        }
    }
}

impl std::error::Error for AtomicWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CannotInspectExtendedMetadata { source, .. }
            | Self::CannotPreserveOwnership { source, .. }
            | Self::CannotPreserveMode { source, .. }
            | Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn inspect_extended_metadata_with(
    path: &Path,
    list_xattrs_fn: impl Fn(&Path) -> io::Result<Vec<OsString>>,
) -> Result<(), AtomicWriteError> {
    let attributes =
        list_xattrs_fn(path).map_err(|source| AtomicWriteError::CannotInspectExtendedMetadata {
            path: path.to_path_buf(),
            source,
        })?;
    if attributes.is_empty() {
        Ok(())
    } else {
        Err(AtomicWriteError::ExtendedMetadataPresent {
            path: path.to_path_buf(),
        })
    }
}

#[cfg(unix)]
fn prepare_replacement_metadata_with(
    display_path: &Path,
    replacement_path: &Path,
    target_metadata: &std::fs::Metadata,
    preservation_supported: bool,
    chown_fn: impl Fn(&Path, Option<u32>, Option<u32>) -> io::Result<()>,
    metadata_fn: impl Fn(&Path) -> io::Result<std::fs::Metadata>,
) -> Result<(), AtomicWriteError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if !preservation_supported {
        return Err(AtomicWriteError::UnsupportedPlatform {
            path: display_path.to_path_buf(),
        });
    }

    let uid = target_metadata.uid();
    let gid = target_metadata.gid();
    let mode = target_metadata.mode() & 0o7777;
    chown_fn(replacement_path, Some(uid), Some(gid)).map_err(|source| {
        AtomicWriteError::CannotPreserveOwnership {
            path: display_path.to_path_buf(),
            uid,
            gid,
            source,
        }
    })?;
    std::fs::set_permissions(replacement_path, std::fs::Permissions::from_mode(mode)).map_err(
        |source| AtomicWriteError::CannotPreserveMode {
            path: display_path.to_path_buf(),
            mode,
            source,
        },
    )?;

    let replacement_metadata = metadata_fn(replacement_path).map_err(|source| {
        AtomicWriteError::PreservationVerificationFailed {
            path: display_path.to_path_buf(),
            detail: format!("cannot stat prepared replacement: {}", source),
        }
    })?;
    let actual_mode = replacement_metadata.mode() & 0o7777;
    if actual_mode != mode {
        return Err(AtomicWriteError::PreservationVerificationFailed {
            path: display_path.to_path_buf(),
            detail: format!("mode is {:#06o}, expected {:#06o}", actual_mode, mode),
        });
    }
    if replacement_metadata.uid() != uid || replacement_metadata.gid() != gid {
        return Err(AtomicWriteError::PreservationVerificationFailed {
            path: display_path.to_path_buf(),
            detail: format!(
                "uid/gid are {}/{}, expected {}/{}",
                replacement_metadata.uid(),
                replacement_metadata.gid(),
                uid,
                gid
            ),
        });
    }
    Ok(())
}

/// Atomically replace `path`'s contents with `content` while preserving
/// the supported metadata contract.
///
/// Existing symlinks are followed and their final regular-file target is
/// replaced, leaving the link itself intact. Existing regular files must
/// have one link, no extended attributes or ACLs, and metadata that can
/// be copied and verified exactly before rename. Rename installs a new
/// inode, so already-open file descriptors continue to observe the old
/// inode. Missing destinations are created with the tempfile's secure
/// mode and process ownership.
///
/// The guarantee is atomic visibility: readers see either the complete
/// old file or the complete new file. This function deliberately does
/// not call `fsync`/`sync_all`, so it does not promise power-loss
/// durability.
pub fn write_atomic(path: &Path, content: &str) -> Result<(), AtomicWriteError> {
    use std::io::Write;

    let requested_path = path.to_path_buf();
    let (replacement_path, target_metadata) = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let resolved =
                std::fs::canonicalize(path).map_err(|_| AtomicWriteError::DanglingSymlink {
                    path: requested_path.clone(),
                })?;
            let target_metadata =
                std::fs::metadata(&resolved).map_err(|source| AtomicWriteError::Io {
                    operation: "inspect target",
                    path: requested_path.clone(),
                    source,
                })?;
            (resolved, Some(target_metadata))
        }
        Ok(metadata) => (path.to_path_buf(), Some(metadata)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => (path.to_path_buf(), None),
        Err(source) => {
            return Err(AtomicWriteError::Io {
                operation: "inspect target",
                path: requested_path,
                source,
            });
        }
    };

    if let Some(metadata) = &target_metadata {
        if !metadata.file_type().is_file() {
            return Err(AtomicWriteError::NotRegularFile {
                path: requested_path,
            });
        }

        #[cfg(not(target_os = "linux"))]
        return Err(AtomicWriteError::UnsupportedPlatform {
            path: requested_path,
        });

        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() > 1 {
                return Err(AtomicWriteError::HardLinked {
                    path: requested_path.clone(),
                    nlink: metadata.nlink(),
                });
            }
            inspect_extended_metadata_with(&requested_path, |_| {
                xattr::list(&replacement_path).map(|attrs| attrs.collect())
            })?;
        }
    }

    let directory = replacement_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary =
        tempfile::NamedTempFile::new_in(directory).map_err(|source| AtomicWriteError::Io {
            operation: "create temporary file for",
            path: requested_path.clone(),
            source,
        })?;
    temporary
        .write_all(content.as_bytes())
        .map_err(|source| AtomicWriteError::Io {
            operation: "write temporary file for",
            path: requested_path.clone(),
            source,
        })?;
    temporary.flush().map_err(|source| AtomicWriteError::Io {
        operation: "flush temporary file for",
        path: requested_path.clone(),
        source,
    })?;

    #[cfg(target_os = "linux")]
    {
        let attributes: Vec<OsString> = xattr::list(temporary.path())
            .map_err(|source| AtomicWriteError::CannotInspectExtendedMetadata {
                path: requested_path.clone(),
                source,
            })?
            .collect();
        for attribute in attributes {
            xattr::remove(temporary.path(), &attribute).map_err(|source| AtomicWriteError::Io {
                operation: "clear temporary-file extended metadata for",
                path: requested_path.clone(),
                source,
            })?;
        }
        inspect_extended_metadata_with(&requested_path, |_| {
            xattr::list(temporary.path()).map(|attrs| attrs.collect())
        })?;
    }

    if let Some(metadata) = &target_metadata {
        #[cfg(unix)]
        prepare_replacement_metadata_with(
            &requested_path,
            temporary.path(),
            metadata,
            cfg!(target_os = "linux"),
            |path, uid, gid| std::os::unix::fs::chown(path, uid, gid),
            |path| std::fs::metadata(path),
        )?;
    }

    temporary
        .persist(&replacement_path)
        .map_err(|error| AtomicWriteError::Io {
            operation: "rename replacement over",
            path: requested_path,
            source: error.error,
        })?;
    Ok(())
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use similar::TextDiff;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    fn assert_output_eq(actual: &str, expected: &str) {
        if actual != expected {
            let diff = TextDiff::from_lines(expected, actual);
            eprintln!();
            for line in diff
                .unified_diff()
                .header("expected", "actual")
                .to_string()
                .lines()
            {
                if line.starts_with('-') {
                    eprintln!("\x1b[31m{}\x1b[0m", line);
                } else if line.starts_with('+') {
                    eprintln!("\x1b[32m{}\x1b[0m", line);
                } else if line.starts_with('@') {
                    eprintln!("\x1b[36m{}\x1b[0m", line);
                } else {
                    eprintln!("{}", line);
                }
            }
            panic!("Output mismatch - see diff above");
        }
    }

    fn kw() -> TodoKeywords {
        TodoKeywords {
            active: vec!["TODO".to_string(), "WIP".to_string()],
            done: vec!["DONE".to_string(), "CANCELED".to_string()],
        }
    }

    fn directory_entries(path: &Path) -> Vec<OsString> {
        let mut entries: Vec<OsString> = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        entries.sort();
        entries
    }

    #[derive(Debug, PartialEq, Eq)]
    struct FileSnapshot {
        content: Vec<u8>,
        mode: u32,
        uid: u32,
        gid: u32,
        ino: u64,
        nlink: u64,
    }

    fn snapshot(path: &Path) -> FileSnapshot {
        let metadata = fs::metadata(path).unwrap();
        FileSnapshot {
            content: fs::read(path).unwrap(),
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            ino: metadata.ino(),
            nlink: metadata.nlink(),
        }
    }

    // ----- heading-line model -----------------------------------------

    #[test]
    fn model_parses_full_heading() {
        let m = HeadingLineModel::parse("** TODO [#A] Title text :work:home:", &kw());
        assert_eq!(m.stars, 2);
        assert_eq!(m.keyword.as_deref(), Some("TODO"));
        assert_eq!(m.priority.as_deref(), Some("[#A]"));
        assert_eq!(m.title, "Title text");
        assert_eq!(m.tags, vec!["work".to_string(), "home".to_string()]);
    }

    #[test]
    fn model_render_round_trips_plain() {
        let raw = "* Just a title";
        let m = HeadingLineModel::parse(raw, &kw());
        assert_output_eq(&m.render(), raw);
    }

    #[test]
    fn model_render_preserves_tag_column() {
        // Tags aligned at a wide column.
        let raw = "* Short title                                                       :tag:";
        let m = HeadingLineModel::parse(raw, &kw());
        assert_output_eq(&m.render(), raw);
    }

    #[test]
    fn model_bold_star_word_is_not_keyword_or_priority() {
        let m = HeadingLineModel::parse("* *bold* intro", &kw());
        assert_eq!(m.keyword, None);
        assert_eq!(m.priority, None);
        assert_eq!(m.title, "*bold* intro");
    }

    // ----- set_keyword ------------------------------------------------

    #[test]
    fn set_keyword_adds_to_plain_heading() {
        let content = "* Plain\n";
        let (out, rec) = set_keyword(content, 1, Some("TODO"), &kw()).unwrap();
        assert_output_eq(&out, "* TODO Plain\n");
        assert_eq!(rec.op, "set-keyword");
        assert_eq!(rec.new.as_deref(), Some("TODO"));
        assert_eq!(rec.old, None);
    }

    #[test]
    fn set_keyword_removes_existing() {
        let content = "* TODO Plain\n";
        let (out, rec) = set_keyword(content, 1, None, &kw()).unwrap();
        assert_output_eq(&out, "* Plain\n");
        assert_eq!(rec.old.as_deref(), Some("TODO"));
    }

    #[test]
    fn set_keyword_invalid_is_error() {
        let content = "* Plain\n";
        let err = set_keyword(content, 1, Some("NOPE"), &kw()).unwrap_err();
        match err {
            EditError::InvalidKeyword { keyword, allowed } => {
                assert_eq!(keyword, "NOPE");
                assert!(allowed.contains(&"TODO".to_string()));
                assert!(allowed.contains(&"CANCELED".to_string()));
            }
            other => panic!("expected InvalidKeyword, got {:?}", other),
        }
    }

    #[test]
    fn set_keyword_non_heading_line_is_error() {
        let content = "* Head\nbody\n";
        assert_eq!(
            set_keyword(content, 2, Some("TODO"), &kw()).unwrap_err(),
            EditError::NotAHeading { line: 2 }
        );
    }

    #[test]
    fn set_keyword_round_trips() {
        let original = "** [#B] A task :x:\n";
        let (with_kw, _) = set_keyword(original, 1, Some("WIP"), &kw()).unwrap();
        let (back, _) = set_keyword(&with_kw, 1, None, &kw()).unwrap();
        assert_output_eq(&back, original);
    }

    #[test]
    fn set_keyword_preserves_other_lines_byte_identical() {
        let content = indoc! {"
            * First
            body one
            * Second
            body two
        "};
        let (out, _) = set_keyword(content, 3, Some("TODO"), &kw()).unwrap();
        let want = indoc! {"
            * First
            body one
            * TODO Second
            body two
        "};
        assert_output_eq(&out, want);
    }

    // ----- set_title --------------------------------------------------

    #[test]
    fn set_title_preserves_keyword_priority_tags() {
        let content = "* TODO [#A] Old title :work:\n";
        let (out, rec) = set_title(content, 1, "New title", &kw()).unwrap();
        assert_output_eq(&out, "* TODO [#A] New title :work:\n");
        assert_eq!(rec.old.as_deref(), Some("Old title"));
        assert_eq!(rec.new.as_deref(), Some("New title"));
    }

    #[test]
    fn set_title_preserves_tag_column_when_short_enough() {
        let content = "* Short                                                             :t:\n";
        // The tag column here is preserved because the new title is shorter.
        let (out, _) = set_title(content, 1, "Tiny", &kw()).unwrap();
        // Recompute expected: same column, tags moved to same char column.
        // The column of ":t:" in the input:
        let col = content.find(":t:").unwrap();
        let title_col = "* Tiny".chars().count();
        let pad = col - title_col;
        let want = format!("* Tiny{}{}\n", " ".repeat(pad), ":t:");
        assert_output_eq(&out, &want);
    }

    #[test]
    fn set_title_falls_back_to_single_space_when_too_long() {
        let content = "* Hi :t:\n";
        let long = "A very very long replacement title that exceeds the column";
        let (out, _) = set_title(content, 1, long, &kw()).unwrap();
        assert_output_eq(&out, &format!("* {} :t:\n", long));
    }

    #[test]
    fn set_title_round_trips() {
        let original = "* TODO [#A] Original :a:b:\n";
        let (changed, _) = set_title(original, 1, "Temporary", &kw()).unwrap();
        let (back, _) = set_title(&changed, 1, "Original", &kw()).unwrap();
        assert_output_eq(&back, original);
    }

    #[test]
    fn set_title_preserves_custom_keyword_and_priority() {
        let content = "* WIP [#A] Old\n";
        let (out, _) = set_title(content, 1, "New", &kw()).unwrap();
        assert_output_eq(&out, "* WIP [#A] New\n");
    }

    // ----- set_priority -----------------------------------------------

    #[test]
    fn set_priority_sets_each_supported_letter_and_records_letters() {
        for priority in ["A", "B", "C"] {
            let (out, record) = set_priority("* Task\n", 1, Some(priority), &kw()).unwrap();
            assert_output_eq(&out, &format!("* [#{}] Task\n", priority));
            assert_eq!(record.op, "set-priority");
            assert_eq!(record.old, None);
            assert_eq!(record.new.as_deref(), Some(priority));
        }
    }

    #[test]
    fn set_priority_changes_and_removes_cookie_without_spacing_residue() {
        let original = "* Task\n";
        let (changed, record) = set_priority("* [#A] Task\n", 1, Some("C"), &kw()).unwrap();
        assert_output_eq(&changed, "* [#C] Task\n");
        assert_eq!(record.old.as_deref(), Some("A"));
        assert_eq!(record.new.as_deref(), Some("C"));

        let (without, record) = set_priority("* [#C] Task\n", 1, None, &kw()).unwrap();
        assert_output_eq(&without, original);
        assert_eq!(record.old.as_deref(), Some("C"));
        assert_eq!(record.new, None);

        let (with, _) = set_priority(original, 1, Some("B"), &kw()).unwrap();
        let (round_tripped, _) = set_priority(&with, 1, None, &kw()).unwrap();
        assert_output_eq(&round_tripped, original);
    }

    #[test]
    fn set_priority_rejects_values_outside_exact_uppercase_letters() {
        for priority in ["D", "", "AA", "a"] {
            assert_eq!(
                set_priority("* Task\n", 1, Some(priority), &kw()).unwrap_err(),
                EditError::InvalidPriority {
                    priority: priority.to_string()
                }
            );
        }
    }

    #[test]
    fn set_priority_preserves_heading_anatomy_and_wide_tag_column() {
        let content =
            "* TODO [#A] Short                                             :wide:column:\n";
        let (out, _) = set_priority(content, 1, Some("B"), &kw()).unwrap();
        assert_output_eq(
            &out,
            "* TODO [#B] Short                                             :wide:column:\n",
        );
    }

    #[test]
    fn set_priority_uses_custom_keyword_set_for_cookie_position() {
        let (out, _) = set_priority("* WIP Task\n", 1, Some("A"), &kw()).unwrap();
        assert_output_eq(&out, "* WIP [#A] Task\n");
    }

    #[test]
    fn set_priority_preserves_other_lines_and_trailing_newline_state() {
        let content = indoc! {"
            preamble
            * First
            body
            * Second
            tail
        "};
        let (out, _) = set_priority(content, 4, Some("A"), &kw()).unwrap();
        assert_output_eq(
            &out,
            indoc! {"
                preamble
                * First
                body
                * [#A] Second
                tail
            "},
        );

        let (without_newline, _) = set_priority("* Task", 1, Some("C"), &kw()).unwrap();
        assert_output_eq(&without_newline, "* [#C] Task");
    }

    #[test]
    fn set_priority_rejects_non_heading_line() {
        assert_eq!(
            set_priority("* Task\nbody\n", 2, Some("A"), &kw()).unwrap_err(),
            EditError::NotAHeading { line: 2 }
        );
    }

    #[test]
    fn all_heading_edit_ops_reject_crlf_source_at_first_occurrence() {
        let content = "* One\nbody\r\n* Two\r\n";
        let expected = EditError::UnsupportedCrlf { line: 2 };
        assert_eq!(
            set_keyword(content, 1, Some("TODO"), &kw()).unwrap_err(),
            expected
        );
        assert_eq!(
            set_priority(content, 1, Some("A"), &kw()).unwrap_err(),
            expected
        );
        assert_eq!(set_title(content, 1, "Title", &kw()).unwrap_err(), expected);
        assert_eq!(
            set_tags(content, 1, &["tag".to_string()]).unwrap_err(),
            expected
        );
        assert_eq!(set_property(content, 1, "K", "v").unwrap_err(), expected);
        assert_eq!(unset_property(content, 1, "K").unwrap_err(), expected);
        assert_eq!(set_content(content, 1, "body").unwrap_err(), expected);
        assert_eq!(
            add_heading(
                content,
                None,
                AddPosition::Last,
                None,
                "New",
                &[],
                &[],
                None,
                &kw()
            )
            .unwrap_err(),
            expected
        );
        assert_eq!(delete_heading(content, 1).unwrap_err(), expected);
        assert_eq!(extract_subtree(content, 1).unwrap_err(), expected);
        assert_eq!(
            insert_subtree(
                content,
                "* Added\n",
                InsertDest {
                    parent_line: None,
                    pos: AddPosition::Last,
                },
            )
            .unwrap_err(),
            expected
        );
    }

    #[test]
    fn caller_supplied_multiline_text_rejects_crlf_at_its_own_line() {
        assert_eq!(
            set_content("* Task\n", 1, "first\nsecond\r\nthird").unwrap_err(),
            EditError::UnsupportedCrlf { line: 2 }
        );
        assert_eq!(
            add_heading(
                "* Task\n",
                None,
                AddPosition::Last,
                None,
                "New",
                &[],
                &[],
                Some("first\nsecond\r\n"),
                &kw(),
            )
            .unwrap_err(),
            EditError::UnsupportedCrlf { line: 2 }
        );
        assert_eq!(
            insert_subtree(
                "* Task\n",
                "* First\nbody\r\n",
                InsertDest {
                    parent_line: None,
                    pos: AddPosition::Last,
                },
            )
            .unwrap_err(),
            EditError::UnsupportedCrlf { line: 2 }
        );
    }

    #[test]
    fn lone_carriage_return_is_data_and_survives_untouched() {
        let content = "note\rdata\n* Task\n";
        let (out, _) = set_priority(content, 2, Some("A"), &kw()).unwrap();
        assert_output_eq(&out, "note\rdata\n* [#A] Task\n");
    }

    // ----- set_tags ---------------------------------------------------

    #[test]
    fn set_tags_adds_to_bare_heading_single_space() {
        let content = "* Plain\n";
        let (out, _) = set_tags(content, 1, &["work".to_string(), "home".to_string()]).unwrap();
        assert_output_eq(&out, "* Plain :work:home:\n");
    }

    #[test]
    fn set_tags_replaces_preserving_column() {
        let content = "* Title                                                             :old:\n";
        let (out, _) = set_tags(content, 1, &["new".to_string()]).unwrap();
        let col = content.find(":old:").unwrap();
        let title_col = "* Title".chars().count();
        let pad = col - title_col;
        let want = format!("* Title{}{}\n", " ".repeat(pad), ":new:");
        assert_output_eq(&out, &want);
    }

    #[test]
    fn set_tags_empty_removes_block() {
        let content = "* Title :work:home:\n";
        let (out, _) = set_tags(content, 1, &[]).unwrap();
        assert_output_eq(&out, "* Title\n");
    }

    #[test]
    fn set_tags_rejects_colon_in_tag() {
        let content = "* Title\n";
        let err = set_tags(content, 1, &["a:b".to_string()]).unwrap_err();
        assert_eq!(err, EditError::InvalidTag { tag: "a:b".into() });
    }

    #[test]
    fn set_tags_rejects_empty_tag() {
        let content = "* Title\n";
        let err = set_tags(content, 1, &["".to_string()]).unwrap_err();
        assert_eq!(err, EditError::InvalidTag { tag: "".into() });
    }

    #[test]
    fn set_tags_round_trips() {
        let original = "* Title :a:b:\n";
        let (changed, _) = set_tags(original, 1, &["x".to_string()]).unwrap();
        let (back, _) = set_tags(&changed, 1, &["a".to_string(), "b".to_string()]).unwrap();
        assert_output_eq(&back, original);
    }

    // ----- set_property / unset_property ------------------------------

    #[test]
    fn set_property_creates_drawer_after_heading() {
        let content = "* Task\nbody\n";
        let (out, rec) = set_property(content, 1, "TASK_REF", "248").unwrap();
        let want = indoc! {"
            * Task
              :PROPERTIES:
              :TASK_REF: 248
              :END:
            body
        "};
        assert_output_eq(&out, want);
        assert_eq!(rec.key.as_deref(), Some("TASK_REF"));
        assert_eq!(rec.new.as_deref(), Some("248"));
    }

    #[test]
    fn set_property_creates_drawer_after_planning_line() {
        let content = indoc! {"
            * DONE Task
            CLOSED: [2025-06-06 Fri 11:45]
            body
        "};
        let (out, _) = set_property(content, 1, "K", "v").unwrap();
        let want = indoc! {"
            * DONE Task
            CLOSED: [2025-06-06 Fri 11:45]
              :PROPERTIES:
              :K: v
              :END:
            body
        "};
        assert_output_eq(&out, want);
    }

    #[test]
    fn set_property_updates_existing_in_place() {
        let content = indoc! {"
            * Task
              :PROPERTIES:
              :TASK_REF: 100
              :END:
        "};
        let (out, rec) = set_property(content, 1, "TASK_REF", "200").unwrap();
        let want = indoc! {"
            * Task
              :PROPERTIES:
              :TASK_REF: 200
              :END:
        "};
        assert_output_eq(&out, want);
        assert_eq!(rec.old.as_deref(), Some("100"));
    }

    #[test]
    fn set_property_appends_when_key_absent() {
        let content = indoc! {"
            * Task
              :PROPERTIES:
              :A: 1
              :END:
        "};
        let (out, _) = set_property(content, 1, "B", "2").unwrap();
        let want = indoc! {"
            * Task
              :PROPERTIES:
              :A: 1
              :B: 2
              :END:
        "};
        assert_output_eq(&out, want);
    }

    #[test]
    fn set_property_mixed_case_drawer_markers() {
        let content = indoc! {"
            * Task
              :properties:
              :A: 1
              :end:
        "};
        let (out, _) = set_property(content, 1, "B", "2").unwrap();
        let want = indoc! {"
            * Task
              :properties:
              :A: 1
              :B: 2
              :end:
        "};
        assert_output_eq(&out, want);
    }

    #[test]
    fn unset_property_removes_line() {
        let content = indoc! {"
            * Task
              :PROPERTIES:
              :A: 1
              :B: 2
              :END:
        "};
        let (out, rec) = unset_property(content, 1, "A").unwrap();
        let want = indoc! {"
            * Task
              :PROPERTIES:
              :B: 2
              :END:
        "};
        assert_output_eq(&out, want);
        assert_eq!(rec.old.as_deref(), Some("1"));
    }

    #[test]
    fn unset_last_property_removes_drawer() {
        let content = indoc! {"
            * Task
              :PROPERTIES:
              :A: 1
              :END:
            body
        "};
        let (out, _) = unset_property(content, 1, "A").unwrap();
        let want = indoc! {"
            * Task
            body
        "};
        assert_output_eq(&out, want);
    }

    #[test]
    fn unset_missing_property_is_error() {
        let content = indoc! {"
            * Task
              :PROPERTIES:
              :A: 1
              :END:
        "};
        let err = unset_property(content, 1, "Z").unwrap_err();
        assert_eq!(
            err,
            EditError::PropertyNotFound {
                key: "Z".into(),
                line: 1
            }
        );
    }

    #[test]
    fn unset_property_no_drawer_is_error() {
        let content = "* Task\nbody\n";
        let err = unset_property(content, 1, "A").unwrap_err();
        assert_eq!(
            err,
            EditError::PropertyNotFound {
                key: "A".into(),
                line: 1
            }
        );
    }

    #[test]
    fn property_set_then_unset_round_trips() {
        let original = "* Task\nbody\n";
        let (with_prop, _) = set_property(original, 1, "K", "v").unwrap();
        let (back, _) = unset_property(&with_prop, 1, "K").unwrap();
        assert_output_eq(&back, original);
    }

    // ----- set_content ------------------------------------------------

    #[test]
    fn set_content_replaces_body_preserving_drawers() {
        let content = indoc! {"
            * Task
              :PROPERTIES:
              :A: 1
              :END:
            old body line 1
            old body line 2
            ** Child
            child body
        "};
        let (out, _) = set_content(content, 1, "new body").unwrap();
        let want = indoc! {"
            * Task
              :PROPERTIES:
              :A: 1
              :END:
            new body
            ** Child
            child body
        "};
        assert_output_eq(&out, want);
    }

    #[test]
    fn set_content_empty_clears_body() {
        let content = indoc! {"
            * Task
            body a
            body b
            * Next
        "};
        let (out, _) = set_content(content, 1, "").unwrap();
        let want = indoc! {"
            * Task
            * Next
        "};
        assert_output_eq(&out, want);
    }

    #[test]
    fn set_content_multiline_body() {
        let content = indoc! {"
            * Task
            old
        "};
        let (out, _) = set_content(content, 1, "line1\nline2").unwrap();
        let want = indoc! {"
            * Task
            line1
            line2
        "};
        assert_output_eq(&out, want);
    }

    // ----- add_heading ------------------------------------------------

    #[test]
    fn add_heading_top_level_last() {
        let content = indoc! {"
            * One
            * Two
        "};
        let (out, rec) = add_heading(
            content,
            None,
            AddPosition::Last,
            None,
            "Three",
            &[],
            &[],
            None,
            &kw(),
        )
        .unwrap();
        let want = indoc! {"
            * One
            * Two
            * Three
        "};
        assert_output_eq(&out, want);
        assert_eq!(rec.line, 3);
    }

    #[test]
    fn add_heading_top_level_first() {
        let content = indoc! {"
            * One
            * Two
        "};
        let (out, _) = add_heading(
            content,
            None,
            AddPosition::First,
            None,
            "Zero",
            &[],
            &[],
            None,
            &kw(),
        )
        .unwrap();
        let want = indoc! {"
            * Zero
            * One
            * Two
        "};
        assert_output_eq(&out, want);
    }

    #[test]
    fn add_heading_under_parent_last_derives_level() {
        let content = indoc! {"
            * Parent
            ** A
            * Other
        "};
        let (out, _) = add_heading(
            content,
            Some(1),
            AddPosition::Last,
            Some("TODO"),
            "B",
            &["t".to_string()],
            &[],
            None,
            &kw(),
        )
        .unwrap();
        let want = indoc! {"
            * Parent
            ** A
            ** TODO B :t:
            * Other
        "};
        assert_output_eq(&out, want);
    }

    #[test]
    fn add_heading_under_parent_with_no_children() {
        let content = indoc! {"
            * Parent
            parent body
            * Other
        "};
        let (out, _) = add_heading(
            content,
            Some(1),
            AddPosition::Last,
            None,
            "First child",
            &[],
            &[],
            None,
            &kw(),
        )
        .unwrap();
        let want = indoc! {"
            * Parent
            parent body
            ** First child
            * Other
        "};
        assert_output_eq(&out, want);
    }

    #[test]
    fn add_heading_before_and_after_index() {
        let content = indoc! {"
            * P
            ** A
            ** C
        "};
        let (out, _) = add_heading(
            content,
            Some(1),
            AddPosition::After(1),
            None,
            "B",
            &[],
            &[],
            None,
            &kw(),
        )
        .unwrap();
        let want = indoc! {"
            * P
            ** A
            ** B
            ** C
        "};
        assert_output_eq(&out, want);
    }

    #[test]
    fn add_heading_with_properties_and_body() {
        let content = "* Parent\n";
        let (out, _) = add_heading(
            content,
            Some(1),
            AddPosition::Last,
            None,
            "Child",
            &[],
            &[("K".to_string(), "v".to_string())],
            Some("body text"),
            &kw(),
        )
        .unwrap();
        let want = indoc! {"
            * Parent
            ** Child
              :PROPERTIES:
              :K: v
              :END:
            body text
        "};
        assert_output_eq(&out, want);
    }

    #[test]
    fn add_heading_position_out_of_range_is_error() {
        let content = indoc! {"
            * P
            ** A
        "};
        let err = add_heading(
            content,
            Some(1),
            AddPosition::Before(5),
            None,
            "X",
            &[],
            &[],
            None,
            &kw(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            EditError::PositionOutOfRange {
                index: 5,
                sibling_count: 1
            }
        );
    }

    // ----- delete_heading ---------------------------------------------

    #[test]
    fn delete_heading_removes_full_subtree() {
        let content = indoc! {"
            * One
            ** Child
            body
            * Two
        "};
        let (out, rec) = delete_heading(content, 1).unwrap();
        let want = indoc! {"
            * Two
        "};
        assert_output_eq(&out, want);
        assert_eq!(rec.span, Some([1, 3]));
    }

    #[test]
    fn delete_last_heading_no_trailing_newline() {
        let content = "* One\n* Two\nbody";
        let (out, _) = delete_heading(content, 2).unwrap();
        assert_output_eq(&out, "* One\n");
    }

    // ----- extract_subtree / insert_subtree ---------------------------

    #[test]
    fn extract_subtree_returns_verbatim_block() {
        let content = indoc! {"
            * One
            ** Child
            body
            * Two
        "};
        let (without, block, rec) = extract_subtree(content, 1).unwrap();
        assert_output_eq(&without, "* Two\n");
        assert_output_eq(&block, "* One\n** Child\nbody\n");
        assert_eq!(rec.span, Some([1, 3]));
    }

    #[test]
    fn extract_then_insert_same_place_round_trips() {
        let content = indoc! {"
            * One
            ** Child
            body
            * Two
        "};
        let (without, block, _) = extract_subtree(content, 1).unwrap();
        // Re-insert at top level, first position (its original place).
        let (restored, _) = insert_subtree(
            &without,
            &block,
            InsertDest {
                parent_line: None,
                pos: AddPosition::First,
            },
        )
        .unwrap();
        assert_output_eq(&restored, content);
    }

    #[test]
    fn insert_subtree_relevels_to_destination_depth() {
        // A level-1 block inserted under a level-1 parent becomes level 2.
        let content = indoc! {"
            * Dest
            * Other
        "};
        let block = "* Moved\n** Sub\nbody\n";
        let (out, _) = insert_subtree(
            content,
            block,
            InsertDest {
                parent_line: Some(1),
                pos: AddPosition::Last,
            },
        )
        .unwrap();
        let want = indoc! {"
            * Dest
            ** Moved
            *** Sub
            body
            * Other
        "};
        assert_output_eq(&out, want);
    }

    #[test]
    fn insert_subtree_deeper_block_kept_relative() {
        // Block whose root is level 2; inserting under level-1 parent
        // (dest level 2) keeps it unchanged.
        let content = "* Dest\n";
        let block = "** Root\n*** Child\n";
        let (out, _) = insert_subtree(
            content,
            block,
            InsertDest {
                parent_line: Some(1),
                pos: AddPosition::Last,
            },
        )
        .unwrap();
        let want = indoc! {"
            * Dest
            ** Root
            *** Child
        "};
        assert_output_eq(&out, want);
    }

    #[test]
    fn insert_subtree_underflow_is_error() {
        // Root is level 3; inserting at top level (dest 1) shifts by -2,
        // which is fine (3 -> 1). Force underflow: block with a level-1
        // heading inserted where shift would go negative is impossible at
        // top level, so construct dest level 1 with root level 2 and a
        // nested level-1... which cannot happen in a valid subtree. Instead
        // test the guard directly with a block whose min level is 1 under a
        // top-level insert after negative shift: use root level 2, nested
        // level 1 (malformed but exercises the guard).
        let content = "* Dest\n";
        let block = "** Root\n* Shallower\n";
        let err = insert_subtree(
            content,
            block,
            InsertDest {
                parent_line: None,
                pos: AddPosition::Last,
            },
        )
        .unwrap_err();
        assert_eq!(
            err,
            EditError::LevelUnderflow {
                requested_shift: -1
            }
        );
    }

    // ----- write_atomic -----------------------------------------------

    #[test]
    fn atomic_write_error_display_contract_is_exact() {
        let path = PathBuf::from("target.org");
        let denied = || io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let cases = [
            (
                AtomicWriteError::UnsupportedPlatform { path: path.clone() },
                "refusing atomic replacement of \"target.org\": this platform cannot verify preservation of mode, ownership, and ACLs",
            ),
            (
                AtomicWriteError::DanglingSymlink { path: path.clone() },
                "refusing atomic replacement of \"target.org\": symlink target does not exist",
            ),
            (
                AtomicWriteError::NotRegularFile { path: path.clone() },
                "refusing atomic replacement of \"target.org\": target is not a regular file",
            ),
            (
                AtomicWriteError::HardLinked {
                    path: path.clone(),
                    nlink: 2,
                },
                "refusing atomic replacement of \"target.org\": target has 2 hard links; replacement would leave the other links unchanged",
            ),
            (
                AtomicWriteError::ExtendedMetadataPresent { path: path.clone() },
                "refusing atomic replacement of \"target.org\": target has extended attributes or ACLs that this writer does not preserve",
            ),
            (
                AtomicWriteError::CannotInspectExtendedMetadata {
                    path: path.clone(),
                    source: denied(),
                },
                "refusing atomic replacement of \"target.org\": cannot verify extended attributes or ACLs: denied",
            ),
            (
                AtomicWriteError::CannotPreserveOwnership {
                    path: path.clone(),
                    uid: 1000,
                    gid: 1001,
                    source: denied(),
                },
                "refusing atomic replacement of \"target.org\": cannot preserve uid 1000 and gid 1001: denied",
            ),
            (
                AtomicWriteError::CannotPreserveMode {
                    path: path.clone(),
                    mode: 0o640,
                    source: denied(),
                },
                "refusing atomic replacement of \"target.org\": cannot preserve mode 0o0640: denied",
            ),
            (
                AtomicWriteError::PreservationVerificationFailed {
                    path: path.clone(),
                    detail: "mode mismatch".to_string(),
                },
                "refusing atomic replacement of \"target.org\": replacement metadata verification failed: mode mismatch",
            ),
            (
                AtomicWriteError::Io {
                    operation: "write",
                    path,
                    source: denied(),
                },
                "write \"target.org\": denied",
            ),
        ];

        for (error, expected) in cases {
            assert_output_eq(&error.to_string(), expected);
        }
    }

    #[test]
    fn write_atomic_preserves_mode_ownership_and_replaces_inode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.org");
        fs::write(&path, "old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let before = snapshot(&path);

        write_atomic(&path, "new\n").unwrap();

        let after = snapshot(&path);
        assert_eq!(after.content, b"new\n");
        assert_eq!(after.mode & 0o7777, 0o640);
        assert_eq!((after.uid, after.gid), (before.uid, before.gid));
        assert_ne!(after.ino, before.ino);
    }

    #[test]
    fn write_atomic_preserves_symlink_identity_and_updates_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.org");
        let link = dir.path().join("link.org");
        fs::write(&target, "old\n").unwrap();
        symlink("target.org", &link).unwrap();
        let link_before = fs::symlink_metadata(&link).unwrap();
        let destination_before = fs::read_link(&link).unwrap();

        write_atomic(&link, "new\n").unwrap();

        let link_after = fs::symlink_metadata(&link).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new\n");
        assert_eq!(fs::read_link(&link).unwrap(), destination_before);
        assert!(link_after.file_type().is_symlink());
        assert_eq!(link_after.ino(), link_before.ino());
    }

    #[test]
    fn write_atomic_refuses_hard_links_without_any_change_or_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.org");
        let second = dir.path().join("second.org");
        fs::write(&first, "original\n").unwrap();
        fs::hard_link(&first, &second).unwrap();
        let before_first = snapshot(&first);
        let before_second = snapshot(&second);
        let entries_before = directory_entries(dir.path());

        let error = write_atomic(&first, "replacement\n").unwrap_err();

        assert!(matches!(
            error,
            AtomicWriteError::HardLinked { nlink: 2, .. }
        ));
        assert_eq!(snapshot(&first), before_first);
        assert_eq!(snapshot(&second), before_second);
        assert_eq!(directory_entries(dir.path()), entries_before);
    }

    #[test]
    fn write_atomic_refuses_dangling_symlink_without_changing_it() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("dangling.org");
        symlink("missing.org", &link).unwrap();
        let metadata_before = fs::symlink_metadata(&link).unwrap();
        let target_before = fs::read_link(&link).unwrap();
        let entries_before = directory_entries(dir.path());

        let error = write_atomic(&link, "replacement\n").unwrap_err();

        assert!(matches!(error, AtomicWriteError::DanglingSymlink { .. }));
        let metadata_after = fs::symlink_metadata(&link).unwrap();
        assert!(metadata_after.file_type().is_symlink());
        assert_eq!(metadata_after.ino(), metadata_before.ino());
        assert_eq!(fs::read_link(&link).unwrap(), target_before);
        assert_eq!(directory_entries(dir.path()), entries_before);
    }

    #[test]
    fn write_atomic_refuses_directory_without_changing_metadata_or_entries() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let metadata_before = fs::metadata(&target).unwrap();
        let entries_before = directory_entries(dir.path());

        let error = write_atomic(&target, "replacement\n").unwrap_err();

        assert!(matches!(error, AtomicWriteError::NotRegularFile { .. }));
        let metadata_after = fs::metadata(&target).unwrap();
        assert_eq!(metadata_after.ino(), metadata_before.ino());
        assert_eq!(metadata_after.mode(), metadata_before.mode());
        assert_eq!(directory_entries(dir.path()), entries_before);
    }

    #[test]
    fn write_atomic_creates_missing_destination_with_secure_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.org");
        write_atomic(&path, "new\n").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(metadata.mode() & 0o7777, 0o600);
    }

    #[test]
    fn inspect_extended_metadata_seam_reports_presence_and_inspection_failure() {
        let path = Path::new("target.org");
        let present =
            inspect_extended_metadata_with(path, |_| Ok(vec![OsString::from("user.test")]))
                .unwrap_err();
        assert!(matches!(
            present,
            AtomicWriteError::ExtendedMetadataPresent { .. }
        ));

        let failure = inspect_extended_metadata_with(path, |_| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
        })
        .unwrap_err();
        assert!(matches!(
            failure,
            AtomicWriteError::CannotInspectExtendedMetadata { .. }
        ));
    }

    #[test]
    fn prepare_replacement_metadata_seam_reports_chown_failure() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.org");
        let replacement = dir.path().join("replacement.org");
        fs::write(&source, "source\n").unwrap();
        fs::write(&replacement, "replacement\n").unwrap();
        let metadata = fs::metadata(&source).unwrap();

        let error = prepare_replacement_metadata_with(
            &source,
            &replacement,
            &metadata,
            true,
            |_, _, _| Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied")),
            |path| fs::metadata(path),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            AtomicWriteError::CannotPreserveOwnership { .. }
        ));
    }

    #[test]
    fn prepare_replacement_metadata_seam_reports_post_apply_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.org");
        let replacement = dir.path().join("replacement.org");
        let mismatch = dir.path().join("mismatch.org");
        fs::write(&source, "source\n").unwrap();
        fs::write(&replacement, "replacement\n").unwrap();
        fs::write(&mismatch, "mismatch\n").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        fs::set_permissions(&mismatch, fs::Permissions::from_mode(0o600)).unwrap();
        let metadata = fs::metadata(&source).unwrap();

        let error = prepare_replacement_metadata_with(
            &source,
            &replacement,
            &metadata,
            true,
            |_, _, _| Ok(()),
            |_| fs::metadata(&mismatch),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            AtomicWriteError::PreservationVerificationFailed { .. }
        ));
    }

    #[test]
    fn prepare_replacement_metadata_seam_reports_unsupported_platform() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.org");
        let replacement = dir.path().join("replacement.org");
        fs::write(&source, "source\n").unwrap();
        fs::write(&replacement, "replacement\n").unwrap();
        let metadata = fs::metadata(&source).unwrap();

        let error = prepare_replacement_metadata_with(
            &source,
            &replacement,
            &metadata,
            false,
            |_, _, _| Ok(()),
            |path| fs::metadata(path),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            AtomicWriteError::UnsupportedPlatform { .. }
        ));
    }

    #[test]
    fn write_atomic_creates_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.org");
        write_atomic(&path, "* One\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "* One\n");
        // Overwrite.
        write_atomic(&path, "* Two\nbody\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "* Two\nbody\n");
    }

    #[test]
    fn write_atomic_preserves_no_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.org");
        write_atomic(&path, "* One\nbody").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "* One\nbody");
    }

    // ----- trailing-newline preservation across ops -------------------

    #[test]
    fn ops_preserve_absent_trailing_newline() {
        let content = "* TODO Task :t:"; // no trailing newline
        let (out, _) = set_title(content, 1, "Renamed", &kw()).unwrap();
        assert_output_eq(&out, "* TODO Renamed :t:");
    }
}
