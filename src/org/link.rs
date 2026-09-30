//! Org-mode link domain type.
//!
//! Owns the parsing and structural representation of `[[file:...]]`
//! and same-file anchor links found in an org document, plus the indexing of anchorable
//! targets (headings, `:CUSTOM_ID:` properties) inside an org file.
//!
//! Higher layers (`crate::cli::action::link::check`,
//! `crate::cli::action::link::orphans`,
//! `crate::cli::action::link::backlinks`) consume these to validate cross-file
//! link integrity (forward) or detect unreferenced files (reverse).
//!
//! # Where links live: body, drawers, and property values
//!
//! A `file:` link is not only found in ordinary body text. It also
//! occurs inside metadata — a `:PROPERTIES:` entry recording
//! provenance, or a free-form note in a `:LOGBOOK:` drawer. Those
//! links are *followable in Emacs* but reach this module through three
//! different parser paths, and consumers do not all want them:
//!
//! - **Body** — `orgize`'s `traverse()` emits a `Link` container.
//! - **Drawer content** — `orgize` builds the `LINK` node but
//!   `traverse()` never descends into `DRAWER_CONTENT`, so the walker
//!   misses it. The node is recovered here by walking drawer
//!   descendants directly.
//! - **Canonical property value** — `orgize` stores a node-property
//!   value as one raw `TEXT` token, so no `LINK` node exists at all
//!   (this mirrors `org-element`, which likewise exposes `:value` as a
//!   string). The value is recovered by re-parsing it *with this same
//!   extractor* — no second link parser is introduced.
//!
//! A `:PROPERTIES:` drawer only becomes a typed `PROPERTY_DRAWER` when
//! it sits immediately after its headline; anywhere else the identical
//! text parses as a plain `DRAWER`. Both shapes are handled, and both
//! report a [`LinkOrigin::Property`] when the drawer line has
//! `:KEY: value` shape, so callers see one consistent origin regardless
//! of which parser path produced the link.
//!
//! Each link therefore carries a [`LinkOrigin`], and each consumer
//! selects the origins it wants via an [`OriginPolicy`]. See that type
//! for the per-command rationale.

use std::collections::HashSet;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use log::warn;
use orgize::{
    Org,
    ast::{Drawer, Headline, Link as OrgLink, PropertyDrawer},
    export::{Container, Event, from_fn},
    rowan::ast::AstNode,
};

use crate::org::property::parse_property_line;

/// A link extracted from an org document.
///
/// [`extract_file_links`] produces only `file:` links for graph and rewrite
/// consumers. [`extract_checkable_links`] also produces same-file `[[*Heading]]`
/// and `[[#custom-id]]` anchors for `link check`; other link types are skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Raw link text as it appears in the source, e.g.
    /// `"[[file:foo.org::*Heading][My desc]]"`.
    pub raw: String,
    /// Path bytes exactly as authored between `file:` and any `::`
    /// anchor. Unlike [`Link::target_path`], this is not percent-decoded.
    pub raw_target: String,
    /// Absolute byte range of [`Link::raw_target`] in the complete source
    /// document. This lets editors replace the path without reconstructing
    /// descriptions, anchors, or surrounding org syntax.
    pub target_span: Range<usize>,
    /// Path part, with the `file:` prefix stripped and percent-escapes
    /// decoded. May be relative (resolved against the containing org
    /// file's directory by callers) or absolute.
    pub target_path: PathBuf,
    /// `true` when the original path ended in `/`, marking it as a
    /// directory citation (e.g. `[[file:photos/]]`). The slash is
    /// stripped by `PathBuf` construction, so this flag preserves
    /// the semantic information for downstream consumers.
    pub target_is_directory: bool,
    /// Same-file anchor links have no path to rewrite. Such links are only
    /// returned by [`extract_checkable_links`]; `target_span` is empty.
    pub target_is_same_file: bool,
    /// Optional anchor following `::`. `None` when the link is just
    /// `[[file:path]]` or `[[file:path][desc]]`.
    pub anchor: Option<Anchor>,
    /// 1-based line number of the link's opening `[[` in the source.
    pub line: usize,
    /// 1-based column number of the link's opening `[[` in the source.
    pub column: usize,
    /// Where in the document structure the link was found. Consumers
    /// filter on this via [`OriginPolicy`]; `link check` also reports it so
    /// a broken metadata pointer is distinguishable from a broken
    /// prose link.
    pub origin: LinkOrigin,
}

/// Where in an org document's structure a [`Link`] was found.
///
/// This is the axis the three link-graph commands disagree on, so it
/// is modelled explicitly rather than being flattened into a boolean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkOrigin {
    /// Ordinary document content — paragraphs, list items, tables,
    /// quote blocks. The links `org-cli` has always seen.
    Body,
    /// Inside a drawer, but not on a `:KEY: value` line: free-form
    /// text such as an `org-add-note` entry in a `:LOGBOOK:` drawer.
    Drawer {
        /// Drawer name as written, upper-cased for stability
        /// (`"LOGBOOK"`, `"PROPERTIES"`, …).
        drawer: String,
    },
    /// The value of a node property, i.e. a `:KEY: value` line. Carries
    /// the key so diagnostics can name the exact metadata field.
    Property {
        /// Drawer name as written, upper-cased (`"PROPERTIES"` for the
        /// common case; a property-shaped line inside another drawer
        /// keeps that drawer's name).
        drawer: String,
        /// Property key, exactly as written in the source.
        key: String,
        /// `true` when the property value is *nothing but* the link —
        /// a pure pointer such as `:PREDECESSOR: [[file:prev.org][…]]`.
        /// `false` when the link is embedded in surrounding narrative
        /// prose, where it reads as a citation rather than a relation.
        pure_pointer: bool,
    },
}

impl LinkOrigin {
    /// Short machine-friendly tag for output formats.
    pub fn tag(&self) -> &'static str {
        match self {
            LinkOrigin::Body => "body",
            LinkOrigin::Drawer { .. } => "drawer",
            LinkOrigin::Property { .. } => "property",
        }
    }

    /// `true` for links that are document content rather than metadata.
    pub fn is_body(&self) -> bool {
        matches!(self, LinkOrigin::Body)
    }

    /// `true` for a property value consisting solely of the link.
    pub fn is_pure_pointer(&self) -> bool {
        matches!(
            self,
            LinkOrigin::Property {
                pure_pointer: true,
                ..
            }
        )
    }

    /// Human-readable origin suffix used in `link check` diagnostics, e.g.
    /// `" in :PROPERTIES: :SOURCE_AUDIO:"`. Empty for body links, whose
    /// line/column already say everything.
    pub fn describe(&self) -> String {
        match self {
            LinkOrigin::Body => String::new(),
            LinkOrigin::Drawer { drawer } => format!(" in :{}: drawer", drawer),
            LinkOrigin::Property { drawer, key, .. } => {
                format!(" in :{}: drawer, property :{}:", drawer, key)
            }
        }
    }
}

/// Which [`LinkOrigin`]s a consumer counts as graph edges.
///
/// The three link-graph commands make different promises, so they take
/// different rows here rather than sharing one edge set:
///
/// - `link check` promises *followability*: Emacs opens a link in a drawer
///   or a property value, so a broken one is a defect a user can hit.
///   It takes [`OriginPolicy::AllFollowable`].
/// - `link backlinks` promises *meaningful reverse navigation*. A property
///   whose value IS a link asserts a relation; a link buried in a long
///   narrative property value is a citation inside prose, and promoting
///   it to an edge inflates the reverse index. It takes
///   [`OriginPolicy::BodyAndPurePointers`].
/// - `link orphans` promises to *surface unreferenced files*. Every added
///   edge can only suppress an orphan report, never produce one, so
///   widening the edge set here can only hide a genuinely forgotten
///   document. It keeps [`OriginPolicy::BodyOnly`], leaving its
///   semantics unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginPolicy {
    /// Body links only — the historical edge set.
    BodyOnly,
    /// Body links, plus property values that are pure pointers.
    BodyAndPurePointers,
    /// Every link a user can follow: body, drawer text, and all
    /// property values.
    AllFollowable,
}

impl OriginPolicy {
    /// `true` when a link with this origin counts under this policy.
    pub fn admits(&self, origin: &LinkOrigin) -> bool {
        match self {
            OriginPolicy::BodyOnly => origin.is_body(),
            OriginPolicy::BodyAndPurePointers => origin.is_body() || origin.is_pure_pointer(),
            OriginPolicy::AllFollowable => true,
        }
    }
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
                && let Some((_, cid)) = properties
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case("CUSTOM_ID"))
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

/// One file-link target after relative-to-source resolution and
/// (best-effort) canonicalization.
///
/// Two links pointing at the same file from different relative
/// positions canonicalize to the same `path`, so [`ResolvedTarget`]s
/// can be compared by set membership across the whole archive.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResolvedTarget {
    /// Absolute path. Canonicalized when the path exists on disk;
    /// otherwise the lexically-joined path (still usable for
    /// existence checks and for diagnostics).
    pub path: PathBuf,
    /// `true` when the original link's path part ended in `/`,
    /// marking it as a directory citation (e.g. `[[file:photos/]]`).
    /// Callers decide whether to expand a directory link to cover
    /// its contents.
    pub is_directory_link: bool,
}

/// One exact file-link path replacement in a source document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkPathRewrite {
    /// Absolute byte range occupied by the old raw target.
    pub span: Range<usize>,
    /// New raw target bytes, excluding `file:` and any anchor.
    pub replacement: String,
}

/// Apply non-overlapping file-target rewrites without changing any other byte.
///
/// Rewrites are validated and applied from the end of the document towards
/// the start so each recorded span remains valid. Callers obtain spans from
/// [`extract_file_links`]; this function deliberately does no link parsing.
pub fn rewrite_file_link_paths(
    content: &str,
    rewrites: &[LinkPathRewrite],
) -> Result<String, String> {
    let mut ordered = rewrites.to_vec();
    ordered.sort_by_key(|rewrite| std::cmp::Reverse(rewrite.span.start));

    let mut previous_start = content.len();
    for rewrite in &ordered {
        if rewrite.span.start > rewrite.span.end
            || rewrite.span.end > content.len()
            || !content.is_char_boundary(rewrite.span.start)
            || !content.is_char_boundary(rewrite.span.end)
        {
            return Err(format!(
                "invalid link target byte span {}..{} for {}-byte source",
                rewrite.span.start,
                rewrite.span.end,
                content.len()
            ));
        }
        if rewrite.span.end > previous_start {
            return Err(format!(
                "overlapping link target byte spans near {}..{}",
                rewrite.span.start, rewrite.span.end
            ));
        }
        previous_start = rewrite.span.start;
    }

    let mut out = content.to_string();
    for rewrite in ordered {
        out.replace_range(rewrite.span, &rewrite.replacement);
    }
    Ok(out)
}

/// Resolve a single [`Link`]'s target against a source directory.
///
/// Used by both `link check` (forward link validation) and `link orphans`
/// (reverse, unreferenced-file detection). Anchors are *not*
/// considered here — the caller validates them separately if needed.
///
/// The returned path is canonicalized when possible (target exists),
/// otherwise lexically joined. This matches the historical behavior
/// of `cli::action::link::check::resolve_path`.
pub fn resolve_target(source_dir: &Path, link: &Link) -> ResolvedTarget {
    let joined = authored_target(source_dir, link);
    let path = fs::canonicalize(&joined).unwrap_or(joined);
    ResolvedTarget {
        path,
        is_directory_link: link.target_is_directory,
    }
}

/// Resolve a link as authored, before following filesystem symlinks.
///
/// This preserves the lexical location of an entry such as `old/alias`
/// when `alias` is a symlink to a target outside `old`. Relocation logic
/// needs both this path and [`resolve_target`]'s canonical identity.
pub fn authored_target(source_dir: &Path, link: &Link) -> PathBuf {
    let expanded = expand_tilde(link);
    if expanded.is_absolute() {
        expanded
    } else {
        source_dir.join(expanded)
    }
}

/// Expand a leading `~` (bare, or `~/rest`) to the user's home
/// directory, as Emacs does when following a link.
///
/// Without this, `[[file:~/notes.org]]` is not absolute and is joined
/// under the source file's directory, producing a bogus
/// `<source-dir>/~/notes.org` that never exists — reporting a
/// perfectly valid link as broken.
///
/// Only a leading `~` is expanded. `~user` forms are left untouched
/// (resolving another user's home needs the password database, which
/// this tool does not consult), and so is a `~` anywhere but the first
/// component, which is an ordinary filename character.
fn expand_tilde(link: &Link) -> PathBuf {
    let path = &link.target_path;
    let authored_tilde = link.raw_target == "~" || link.raw_target.starts_with("~/");
    if !authored_tilde {
        return path.to_path_buf();
    }
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    let rest = if s == "~" {
        ""
    } else if let Some(r) = s.strip_prefix("~/") {
        r
    } else {
        return path.to_path_buf();
    };
    match dirs::home_dir() {
        Some(home) if rest.is_empty() => home,
        Some(home) => home.join(rest),
        None => {
            warn!(
                "cannot determine home directory; leaving {:?} unexpanded",
                s
            );
            path.to_path_buf()
        }
    }
}

/// Read every org file in `org_files`, extract the `[[file:...]]`
/// links admitted by `policy`, resolve each target relative to its
/// source file's directory, and return the de-duplicated set of
/// resolved targets.
///
/// I/O failures on a single org file are logged via `log::warn!` and
/// the file is skipped — same defensive policy as
/// [`crate::cli::action::link::check`].
///
/// This is the **graph-walk core** behind `link orphans` (reverse
/// integrity: which on-disk files are *not* in this set?). `link orphans`
/// passes [`OriginPolicy::BodyOnly`]: an extra edge there can only
/// suppress a warning, never raise one.
pub fn collect_referenced_paths(
    org_files: &[PathBuf],
    policy: OriginPolicy,
) -> HashSet<ResolvedTarget> {
    let mut out = HashSet::new();
    for source_file in org_files {
        let content = match fs::read_to_string(source_file) {
            Ok(c) => c,
            Err(e) => {
                warn!("cannot read {}: {}", source_file.display(), e);
                continue;
            }
        };
        let source_dir = source_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        for link in extract_file_links(&content) {
            if !policy.admits(&link.origin) {
                continue;
            }
            out.insert(resolve_target(&source_dir, &link));
        }
    }
    out
}

/// Extract every `[[file:...]]` link from the given org content,
/// including links inside drawers and node-property values.
///
/// The returned [`Link`]s have their `file:` prefix stripped and
/// their path part percent-decoded, and each carries the
/// [`LinkOrigin`] it was found at. Path resolution against the
/// containing file's directory is the caller's responsibility, as is
/// filtering by [`OriginPolicy`] — this function reports everything it
/// finds.
///
/// Links are returned in source order.
pub fn extract_file_links(content: &str) -> Vec<Link> {
    extract_links(content, false)
}

/// Extract file links and same-file heading/CUSTOM_ID links for validation.
/// Other schemes and unprefixed fuzzy links are not included.
pub fn extract_checkable_links(content: &str) -> Vec<Link> {
    extract_links(content, true)
}

fn extract_links(content: &str, include_same_file: bool) -> Vec<Link> {
    let mut out = extract_body_links(content, include_same_file);
    out.extend(extract_drawer_links(content, include_same_file));
    out.sort_by_key(|l| (l.line, l.column));
    out
}

/// Extract the `file:` links `orgize`'s traversal reaches: ordinary
/// document content (paragraphs, lists, tables, quote blocks).
///
/// Deliberately uses `traverse()` rather than a raw descendant walk:
/// the walker's coverage *is* the definition of "body" here, and a
/// descendant walk would additionally pick up links in headline
/// titles, silently widening the historical edge set.
fn extract_body_links(content: &str, include_same_file: bool) -> Vec<Link> {
    let mut out: Vec<Link> = Vec::new();
    let mut handler = from_fn(|event| {
        let Event::Enter(Container::Link(link)) = event else {
            return;
        };
        if let Some(l) = build_link(content, &link, LinkOrigin::Body, include_same_file) {
            out.push(l);
        }
    });
    Org::parse(content).traverse(&mut handler);
    out
}

/// Extract the `file:` links that live in metadata: drawer content and
/// node-property values.
///
/// Two parser shapes are covered, and both are reached from the *same*
/// `orgize` parse used for body links — no separate org parser exists
/// in this crate:
///
/// - A plain `DRAWER` (including a `:PROPERTIES:` drawer that is not
///   positioned immediately after its headline) *does* contain `LINK`
///   nodes; they are collected by walking its descendants. Whether a
///   given link is a property value is then decided by re-reading its
///   source line through
///   [`crate::org::property::parse_property_line`].
/// - A canonical `PROPERTY_DRAWER` contains no `LINK` nodes at all —
///   each value is one raw token. Every value is re-parsed *through
///   this module's own body extractor*, and the resulting links are
///   re-anchored to their absolute position in the document.
fn extract_drawer_links(content: &str, include_same_file: bool) -> Vec<Link> {
    let org = Org::parse(content);
    let mut out: Vec<Link> = Vec::new();

    // Plain drawers: LINK nodes exist, traverse() just never visits
    // them. Blocks nested inside a drawer (`#+begin_src`) still yield
    // no LINK node, so code samples stay excluded exactly as before.
    for drawer in org
        .document()
        .syntax()
        .descendants()
        .filter_map(Drawer::cast)
    {
        let name = drawer.name().to_string().trim().to_uppercase();
        for link in drawer.syntax().descendants().filter_map(OrgLink::cast) {
            let offset: usize = link.start().into();
            let origin = classify_drawer_link(content, offset, &name);
            if let Some(l) = build_link(content, &link, origin, include_same_file) {
                out.push(l);
            }
        }
    }

    // Canonical property drawers: values are opaque text, so re-parse
    // each value with the body extractor and shift the results back to
    // document coordinates.
    for pd in org
        .document()
        .syntax()
        .descendants()
        .filter_map(PropertyDrawer::cast)
    {
        let drawer_name = property_drawer_name(&pd);
        for np in pd.node_properties() {
            let raw = np.raw();
            let np_offset: usize = np.start().into();
            let Some(parsed) = parse_property_line(&raw) else {
                continue;
            };
            // Locate the value inside the raw property line so nested
            // link offsets can be mapped back to the document.
            let Some(value_in_line) = raw.find(&parsed.value) else {
                continue;
            };
            let value_offset = np_offset + value_in_line;
            for mut link in extract_body_links(&parsed.value, include_same_file) {
                let pure_pointer = parsed.value.trim() == link.raw.trim();
                link.origin = LinkOrigin::Property {
                    drawer: drawer_name.clone(),
                    key: parsed.key.clone(),
                    pure_pointer,
                };
                let abs = value_offset + offset_of(&parsed.value, link.line, link.column);
                let (line, column) = offset_to_line_col(content, abs);
                link.target_span =
                    (value_offset + link.target_span.start)..(value_offset + link.target_span.end);
                link.line = line;
                link.column = column;
                out.push(link);
            }
        }
    }

    out
}

/// Name of a canonical property drawer, read from its `DRAWER_BEGIN`
/// text so an unusual spelling is reported as written (upper-cased).
/// Falls back to `PROPERTIES`, which is what the parser requires for
/// this node to exist in the first place.
fn property_drawer_name(pd: &PropertyDrawer) -> String {
    pd.syntax()
        .children()
        .find(|n| n.kind() == orgize::SyntaxKind::DRAWER_BEGIN)
        .map(|n| n.text().to_string())
        .map(|s| s.trim().trim_matches(':').trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "PROPERTIES".to_string())
}

/// Decide whether a link found in a plain drawer sits on a
/// `:KEY: value` line (a property) or in free-form drawer text.
///
/// This is what makes a *degraded* `:PROPERTIES:` drawer — one not
/// placed immediately after its headline, which the parser downgrades
/// to a plain drawer — still report [`LinkOrigin::Property`] with its
/// key, matching the canonical case.
fn classify_drawer_link(content: &str, link_offset: usize, drawer_name: &str) -> LinkOrigin {
    let line_start = content[..link_offset.min(content.len())]
        .rfind('\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    let line_end = content[line_start..]
        .find('\n')
        .map(|p| line_start + p)
        .unwrap_or(content.len());
    let line = &content[line_start..line_end];

    match parse_property_line(line) {
        Some(p) => {
            // A property line whose value is exactly one link is a
            // pure pointer; anything else is prose around a citation.
            let pure_pointer = p.value.starts_with("[[") && p.value.ends_with("]]");
            LinkOrigin::Property {
                drawer: drawer_name.to_string(),
                key: p.key,
                pure_pointer,
            }
        }
        None => LinkOrigin::Drawer {
            drawer: drawer_name.to_string(),
        },
    }
}

/// Byte offset of a 1-based (line, column) position in `s`. Inverse of
/// [`offset_to_line_col`], used to re-anchor links found in a re-parsed
/// property value back into the whole document.
fn offset_of(s: &str, line: usize, column: usize) -> usize {
    let mut start = 0;
    for _ in 1..line {
        match s[start..].find('\n') {
            Some(p) => start += p + 1,
            None => return s.len(),
        }
    }
    (start + column - 1).min(s.len())
}

/// Build a [`Link`] from the shared orgize link node for the admitted kinds.
fn build_link(
    content: &str,
    link: &OrgLink,
    origin: LinkOrigin,
    include_same_file: bool,
) -> Option<Link> {
    let raw_path = link.path().to_string();
    let offset: usize = link.start().into();
    let (line, column) = offset_to_line_col(content, offset);
    let raw = link.raw();
    let (path_str, target_start, target_path, target_is_directory, target_is_same_file, anchor) =
        if let Some(after_scheme) = raw_path.strip_prefix("file:") {
            let (path_str, anchor) = split_anchor(after_scheme);
            let decoded = percent_decode(path_str);
            let target_in_raw = raw.strip_prefix("[[file:")?;
            if !target_in_raw.starts_with(path_str) {
                return None;
            }
            (
                path_str,
                offset + "[[file:".len(),
                PathBuf::from(&decoded),
                decoded.ends_with('/'),
                false,
                anchor,
            )
        } else if include_same_file && (raw_path.starts_with('*') || raw_path.starts_with('#')) {
            let anchor = classify_anchor(&raw_path);
            ("", offset, PathBuf::new(), false, true, Some(anchor))
        } else {
            return None;
        };

    Some(Link {
        raw,
        raw_target: path_str.to_string(),
        target_span: target_start..target_start + path_str.len(),
        target_path,
        target_is_directory,
        target_is_same_file,
        anchor,
        line,
        column,
        origin,
    })
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
        Anchor::Heading(normalize_heading(&unescape_link(rest)))
    } else if let Some(rest) = raw.strip_prefix('#') {
        Anchor::CustomId(unescape_link(rest))
    } else if let Ok(n) = raw.parse::<usize>() {
        Anchor::LineNumber(n)
    } else {
        Anchor::Other(unescape_link(raw))
    }
}

/// Remove org's backslash escapes from a link's search option.
///
/// Emacs writes `[` and `]` as `\[` and `\]` inside a link (see
/// `org-link-escape`), and strips those backslashes with
/// `org-link-unescape` before searching for the target. Anchors must
/// therefore be compared against the *unescaped* text: a headline
/// titled `[2026-07-15] Note` is linked as `::*\[2026-07-15\] Note`.
///
/// A backslash escapes `[`, `]`, and another backslash. Before any
/// other character it is not an escape and is kept verbatim, so a
/// heading containing `\n` still round-trips.
///
/// Applied to anchors only. The path keeps its authored bytes because
/// [`Link::target_span`] rewrites are byte-offset based.
fn unescape_link(input: &str) -> String {
    if !input.contains('\\') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.clone().next() {
            Some(next @ ('[' | ']' | '\\')) => {
                out.push(next);
                chars.next();
            }
            // Not an escape sequence: keep the backslash as authored.
            _ => out.push('\\'),
        }
    }
    out
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
    fn test_link_target_span_is_exact_for_body_and_property_links() {
        let src = indoc! {"
            Préface [[file:body old.org::*H][body]].
            * Node
            :PROPERTIES:
            :POINTER: [[file:property old.org::#id][property]]
            :END:
        "};
        let links = extract_file_links(src);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].raw_target, "body old.org");
        assert_eq!(&src[links[0].target_span.clone()], "body old.org");
        assert_eq!(links[1].raw_target, "property old.org");
        assert_eq!(&src[links[1].target_span.clone()], "property old.org");
    }

    #[test]
    fn test_rewrite_file_link_paths_changes_only_exact_target_spans() {
        let src = "Before [[file:old.org::*H][old.org description]] after old.org.\n";
        let link = extract_file_links(src).remove(0);
        let rewritten = rewrite_file_link_paths(
            src,
            &[LinkPathRewrite {
                span: link.target_span,
                replacement: "new.org".to_string(),
            }],
        )
        .unwrap();
        assert_eq!(
            rewritten,
            "Before [[file:new.org::*H][old.org description]] after old.org.\n"
        );
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

    /// Emacs escapes `[` and `]` in a link's search option, and strips
    /// those backslashes with `org-link-unescape` before searching. A
    /// heading titled `[2026-07-15] Note` is linked as
    /// `::*\[2026-07-15\] Note`, so the anchor must compare equal to
    /// the unescaped title, not to the escaped source bytes.
    #[test]
    fn test_heading_anchor_unescapes_brackets() {
        let src = r"[[file:a.org::*\[2026-07-15\] Note][d]]".to_string() + "\n";
        let links = extract_file_links(&src);
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].anchor,
            Some(Anchor::Heading("[2026-07-15] Note".to_string()))
        );
    }

    #[test]
    fn test_custom_id_anchor_unescapes_brackets() {
        let src = r"[[file:a.org::#id\[1\]][d]]".to_string() + "\n";
        let links = extract_file_links(&src);
        assert_eq!(links[0].anchor, Some(Anchor::CustomId("id[1]".to_string())));
    }

    /// `\\` denotes a single literal backslash.
    #[test]
    fn test_heading_anchor_unescapes_double_backslash() {
        let src = r"[[file:a.org::*A\\B][d]]".to_string() + "\n";
        let links = extract_file_links(&src);
        assert_eq!(links[0].anchor, Some(Anchor::Heading(r"A\B".to_string())));
    }

    /// A backslash before any other character is not an escape and is
    /// preserved, matching `org-link-unescape`.
    #[test]
    fn test_heading_anchor_keeps_non_escape_backslash() {
        let src = r"[[file:a.org::*A\nB][d]]".to_string() + "\n";
        let links = extract_file_links(&src);
        assert_eq!(links[0].anchor, Some(Anchor::Heading(r"A\nB".to_string())));
    }

    /// Unescaping must not disturb the path span used by `link move`:
    /// the rewrite is byte-offset based, so `raw_target` stays verbatim.
    #[test]
    fn test_escaped_anchor_leaves_path_span_intact() {
        let src = r"[[file:a.org::*\[X\] T][d]]".to_string() + "\n";
        let links = extract_file_links(&src);
        assert_eq!(links[0].raw_target, "a.org");
        assert_eq!(&src[links[0].target_span.clone()], "a.org");
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

    // ---------------------------------------------------------------
    // Tests for the shared resolver / collector used by both `link check`
    // and `link orphans`.
    // ---------------------------------------------------------------

    /// Same tempdir helper as `check.rs::tests` — duplicated here on
    /// purpose to keep `org::link` self-contained for testing. If a
    /// third test module needs it we'll lift it.
    mod tempdir_helper {
        use std::fs;
        use std::path::PathBuf;
        pub struct TempDir(pub PathBuf);
        impl TempDir {
            pub fn new(tag: &str) -> Self {
                let mut p = std::env::temp_dir();
                let pid = std::process::id();
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0);
                p.push(format!("org-cli-link-test-{}-{}-{}", tag, pid, nanos));
                fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }

    fn write_file(p: &Path, s: &str) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn test_extract_marks_directory_link() {
        let src = "[[file:photos/][shots]] [[file:foo.org][f]]\n";
        let links = extract_file_links(src);
        assert_eq!(links.len(), 2);
        // Order matches source order.
        assert!(links[0].target_is_directory);
        assert!(!links[1].target_is_directory);
    }

    #[test]
    fn test_resolve_target_strips_anchor_concept() {
        // Anchors don't reach `resolve_target` — they live in
        // `Link::anchor`. The resolver only cares about the path.
        // This test just documents that contract.
        let src = "[[file:foo.org::*Heading][f]]\n";
        let link = &extract_file_links(src)[0];
        let r = resolve_target(Path::new("/tmp"), link);
        assert!(r.path.ends_with("foo.org"));
    }

    #[test]
    fn test_resolve_target_canonicalizes_when_path_exists() {
        let td = tempdir_helper::TempDir::new("canon");
        // Create the same target reachable from two different
        // source dirs via different relative paths.
        write_file(&td.path().join("topic/foo.org"), "* H\n");
        std::fs::create_dir_all(td.path().join("visit")).unwrap();

        let from_root = "[[file:topic/foo.org][f]]\n";
        let link_a = &extract_file_links(from_root)[0];
        let resolved_a = resolve_target(td.path(), link_a);

        let from_visit = "[[file:../topic/foo.org][f]]\n";
        let link_b = &extract_file_links(from_visit)[0];
        let resolved_b = resolve_target(&td.path().join("visit"), link_b);

        assert_eq!(
            resolved_a, resolved_b,
            "two relative paths to the same file must canonicalize equal"
        );
        assert!(!resolved_a.is_directory_link);
    }

    #[test]
    fn test_resolve_target_keeps_directory_flag_through_canonicalization() {
        let td = tempdir_helper::TempDir::new("dir-flag");
        std::fs::create_dir_all(td.path().join("sub")).unwrap();
        let src = "[[file:sub/][s]]\n";
        let link = &extract_file_links(src)[0];
        let r = resolve_target(td.path(), link);
        assert!(r.is_directory_link, "trailing-slash semantic preserved");
    }

    #[test]
    fn test_resolve_target_falls_back_when_path_missing() {
        // No filesystem state; canonicalize fails; we expect the
        // lexically-joined path back so callers can still reason
        // about it (existence checks, error messages).
        let src = "[[file:does/not/exist.org][x]]\n";
        let link = &extract_file_links(src)[0];
        let r = resolve_target(Path::new("/tmp"), link);
        assert_eq!(r.path, PathBuf::from("/tmp/does/not/exist.org"));
    }

    // ---------------------------------------------------------------
    // Link origin: body vs drawer vs property value.
    // ---------------------------------------------------------------

    fn origins(src: &str) -> Vec<(String, LinkOrigin)> {
        extract_file_links(src)
            .into_iter()
            .map(|l| (l.target_path.display().to_string(), l.origin))
            .collect()
    }

    #[test]
    fn test_body_link_origin_is_body() {
        let src = "* H\nsee [[file:a.org][A]]\n";
        assert_eq!(origins(src), vec![("a.org".to_string(), LinkOrigin::Body)]);
    }

    #[test]
    fn test_canonical_property_value_link_is_a_pure_pointer() {
        let src = indoc! {"
            * H
            :PROPERTIES:
            :SOURCE_AUDIO: [[file:a.org][A]]
            :END:
        "};
        assert_eq!(
            origins(src),
            vec![(
                "a.org".to_string(),
                LinkOrigin::Property {
                    drawer: "PROPERTIES".to_string(),
                    key: "SOURCE_AUDIO".to_string(),
                    pure_pointer: true,
                }
            )]
        );
    }

    #[test]
    fn test_narrative_property_value_link_is_not_a_pure_pointer() {
        let src = indoc! {"
            * H
            :PROPERTIES:
            :TIMESTAMP_NOTE: le bilan cite [[file:a.org][A]] sans conclure
            :END:
        "};
        assert_eq!(
            origins(src),
            vec![(
                "a.org".to_string(),
                LinkOrigin::Property {
                    drawer: "PROPERTIES".to_string(),
                    key: "TIMESTAMP_NOTE".to_string(),
                    pure_pointer: false,
                }
            )]
        );
    }

    #[test]
    fn test_degraded_property_drawer_still_reports_property_origin() {
        // A `:PROPERTIES:` drawer not placed immediately after its
        // headline parses as a plain DRAWER, but is the same thing to
        // the user and must classify identically.
        let src = indoc! {"
            * H

            :PROPERTIES:
            :PREDECESSOR: [[file:a.org][A]]
            :END:
        "};
        assert_eq!(
            origins(src),
            vec![(
                "a.org".to_string(),
                LinkOrigin::Property {
                    drawer: "PROPERTIES".to_string(),
                    key: "PREDECESSOR".to_string(),
                    pure_pointer: true,
                }
            )]
        );
    }

    #[test]
    fn test_free_form_drawer_text_link_is_drawer_origin() {
        let src = indoc! {"
            * H
            :LOGBOOK:
            - Note taken on [2026-08-17 Mon] see [[file:a.org][A]]
            :END:
        "};
        assert_eq!(
            origins(src),
            vec![(
                "a.org".to_string(),
                LinkOrigin::Drawer {
                    drawer: "LOGBOOK".to_string()
                }
            )]
        );
    }

    #[test]
    fn test_clock_line_yields_no_link() {
        let src = indoc! {"
            * H
            :LOGBOOK:
            CLOCK: [2026-08-17 Mon 10:00]--[2026-08-17 Mon 11:00] =>  1:00
            :END:
        "};
        assert!(extract_file_links(src).is_empty());
    }

    #[test]
    fn test_blocks_stay_excluded_inside_and_outside_drawers() {
        // Code and examples are quoted material, not links — true
        // before this change and it must stay true, including when the
        // block sits inside a drawer.
        let src = indoc! {"
            * H
            #+begin_src text
            [[file:a.org][A]]
            #+end_src
            :MYDRAWER:
            #+begin_example
            [[file:b.org][B]]
            #+end_example
            :END:
        "};
        assert!(extract_file_links(src).is_empty(), "got {:?}", origins(src));
    }

    #[test]
    fn test_headline_title_link_is_not_extracted() {
        // Guards the body extractor against silently widening: a
        // descendant walk would pick this up, `traverse()` does not,
        // and this change is not authorized to alter that.
        let src = "* Head [[file:a.org][A]]\nbody\n";
        assert!(extract_file_links(src).is_empty());
    }

    #[test]
    fn test_links_are_returned_in_source_order_across_origins() {
        let src = indoc! {"
            * H
            :PROPERTIES:
            :K: [[file:prop.org][P]]
            :END:
            body [[file:body.org][B]]
            :LOGBOOK:
            - note [[file:log.org][L]]
            :END:
        "};
        let paths: Vec<String> = extract_file_links(src)
            .into_iter()
            .map(|l| l.target_path.display().to_string())
            .collect();
        assert_eq!(paths, vec!["prop.org", "body.org", "log.org"]);
    }

    #[test]
    fn test_drawer_link_line_and_column_point_at_the_link() {
        let src = indoc! {"
            * H
            :PROPERTIES:
            :K: [[file:a.org][A]]
            :END:
        "};
        let l = &extract_file_links(src)[0];
        assert_eq!(l.line, 3, "1-based line of the property entry");
        assert_eq!(l.column, 5, "column of the opening `[[`");
        assert_eq!(l.raw, "[[file:a.org][A]]");
    }

    #[test]
    fn test_property_value_link_keeps_anchor_and_directory_semantics() {
        let src = indoc! {"
            * H
            :PROPERTIES:
            :K: [[file:a.org::*Some Heading][A]]
            :DIR: [[file:photos/][p]]
            :END:
        "};
        let links = extract_file_links(src);
        assert_eq!(
            links[0].anchor,
            Some(Anchor::Heading("Some Heading".to_string()))
        );
        assert_eq!(links[0].target_path, PathBuf::from("a.org"));
        assert!(links[1].target_is_directory);
    }

    #[test]
    fn test_property_shaped_line_in_a_non_properties_drawer_names_that_drawer() {
        let src = indoc! {"
            * H
            :LOGBOOK:
            :PRED: [[file:a.org][A]]
            :END:
        "};
        assert_eq!(
            origins(src),
            vec![(
                "a.org".to_string(),
                LinkOrigin::Property {
                    drawer: "LOGBOOK".to_string(),
                    key: "PRED".to_string(),
                    pure_pointer: true,
                }
            )]
        );
    }

    #[test]
    fn test_origin_describe_names_the_metadata_field() {
        assert_eq!(LinkOrigin::Body.describe(), "");
        assert_eq!(
            LinkOrigin::Drawer {
                drawer: "LOGBOOK".into()
            }
            .describe(),
            " in :LOGBOOK: drawer"
        );
        assert_eq!(
            LinkOrigin::Property {
                drawer: "PROPERTIES".into(),
                key: "SOURCE_AUDIO".into(),
                pure_pointer: true,
            }
            .describe(),
            " in :PROPERTIES: drawer, property :SOURCE_AUDIO:"
        );
    }

    #[test]
    fn test_origin_tags_are_stable_for_machine_output() {
        assert_eq!(LinkOrigin::Body.tag(), "body");
        assert_eq!(LinkOrigin::Drawer { drawer: "X".into() }.tag(), "drawer");
        assert_eq!(
            LinkOrigin::Property {
                drawer: "PROPERTIES".into(),
                key: "K".into(),
                pure_pointer: false,
            }
            .tag(),
            "property"
        );
    }

    // ---------------------------------------------------------------
    // Per-command policy.
    // ---------------------------------------------------------------

    #[test]
    fn test_policy_body_only_rejects_all_metadata_links() {
        let p = OriginPolicy::BodyOnly;
        assert!(p.admits(&LinkOrigin::Body));
        assert!(!p.admits(&LinkOrigin::Drawer {
            drawer: "LOGBOOK".into()
        }));
        assert!(!p.admits(&LinkOrigin::Property {
            drawer: "PROPERTIES".into(),
            key: "K".into(),
            pure_pointer: true,
        }));
    }

    #[test]
    fn test_policy_body_and_pure_pointers_splits_pointer_from_prose() {
        let p = OriginPolicy::BodyAndPurePointers;
        assert!(p.admits(&LinkOrigin::Body));
        assert!(p.admits(&LinkOrigin::Property {
            drawer: "PROPERTIES".into(),
            key: "PREDECESSOR".into(),
            pure_pointer: true,
        }));
        assert!(
            !p.admits(&LinkOrigin::Property {
                drawer: "PROPERTIES".into(),
                key: "TIMESTAMP_NOTE".into(),
                pure_pointer: false,
            }),
            "a link inside narrative prose is a citation, not a relation"
        );
        assert!(!p.admits(&LinkOrigin::Drawer {
            drawer: "LOGBOOK".into()
        }));
    }

    #[test]
    fn test_policy_all_followable_admits_every_origin() {
        let p = OriginPolicy::AllFollowable;
        assert!(p.admits(&LinkOrigin::Body));
        assert!(p.admits(&LinkOrigin::Drawer {
            drawer: "LOGBOOK".into()
        }));
        assert!(p.admits(&LinkOrigin::Property {
            drawer: "PROPERTIES".into(),
            key: "K".into(),
            pure_pointer: false,
        }));
    }

    // ---------------------------------------------------------------
    // `~/` expansion.
    // ---------------------------------------------------------------

    #[test]
    fn test_resolve_target_expands_leading_tilde_to_home() {
        let home = dirs::home_dir().expect("home dir required for this test");
        let src = "[[file:~/some-file-that-need-not-exist.org][x]]\n";
        let link = &extract_file_links(src)[0];
        let r = resolve_target(Path::new("/tmp"), link);
        assert_eq!(r.path, home.join("some-file-that-need-not-exist.org"));
    }

    #[test]
    fn test_resolve_target_expands_bare_tilde() {
        let home = dirs::home_dir().expect("home dir required for this test");
        let src = "[[file:~][home]]\n";
        let link = &extract_file_links(src)[0];
        let r = resolve_target(Path::new("/tmp"), link);
        assert_eq!(r.path, fs::canonicalize(&home).unwrap_or(home));
    }

    #[test]
    fn test_resolve_target_expands_tilde_in_property_value() {
        // The 5 real archive failures were `~/`-prefixed links inside
        // `:TIMESTAMP_NOTE:` values: both fixes must compose.
        let home = dirs::home_dir().expect("home dir required for this test");
        let src = indoc! {"
            * H
            :PROPERTIES:
            :TIMESTAMP_NOTE: cf [[file:~/notes/x.org][x]]
            :END:
        "};
        let link = &extract_file_links(src)[0];
        let r = resolve_target(Path::new("/tmp"), link);
        assert_eq!(r.path, home.join("notes/x.org"));
    }

    #[test]
    fn test_resolve_target_leaves_non_leading_and_user_tildes_alone() {
        // `~backup.org` is an ordinary filename; `~user/` needs the
        // password database and is out of scope.
        for (raw, expected) in [
            ("[[file:~backup.org][b]]\n", "/tmp/~backup.org"),
            ("[[file:notes/~/x.org][x]]\n", "/tmp/notes/~/x.org"),
        ] {
            let link = &extract_file_links(raw)[0];
            let r = resolve_target(Path::new("/tmp"), link);
            assert_eq!(r.path, PathBuf::from(expected));
        }
    }

    #[test]
    fn test_collect_referenced_paths_dedups_across_files() {
        let td = tempdir_helper::TempDir::new("collect-dedup");
        write_file(&td.path().join("target.org"), "* H\n");
        write_file(&td.path().join("a.org"), "[[file:target.org][t]]\n");
        write_file(
            &td.path().join("nested/b.org"),
            "[[file:../target.org][t]]\n",
        );

        let org_files = vec![td.path().join("a.org"), td.path().join("nested/b.org")];
        let set = collect_referenced_paths(&org_files, OriginPolicy::BodyOnly);
        assert_eq!(
            set.len(),
            1,
            "the same target reached via different relative paths must dedup"
        );
        let only = set.iter().next().unwrap();
        assert!(only.path.ends_with("target.org"));
        assert!(!only.is_directory_link);
    }

    #[test]
    fn test_collect_referenced_paths_skips_unreadable_org() {
        let td = tempdir_helper::TempDir::new("collect-unreadable");
        // Path that doesn't exist — `read_to_string` errors are
        // logged and the file is skipped, not propagated.
        let bogus = td.path().join("ghost.org");
        let set = collect_referenced_paths(&[bogus], OriginPolicy::BodyOnly);
        assert!(set.is_empty());
    }

    #[test]
    fn test_collect_referenced_paths_skips_non_file_links() {
        let td = tempdir_helper::TempDir::new("collect-nonfile");
        write_file(
            &td.path().join("a.org"),
            "[[https://example.com][site]] [[id:abc][by-id]]\n",
        );
        let set = collect_referenced_paths(&[td.path().join("a.org")], OriginPolicy::BodyOnly);
        assert!(
            set.is_empty(),
            "only file: links contribute, got: {:?}",
            set
        );
    }

    #[test]
    fn test_collect_referenced_paths_marks_directory_links() {
        let td = tempdir_helper::TempDir::new("collect-dir");
        std::fs::create_dir_all(td.path().join("photos")).unwrap();
        write_file(&td.path().join("a.org"), "[[file:photos/][p]]\n");
        let set = collect_referenced_paths(&[td.path().join("a.org")], OriginPolicy::BodyOnly);
        assert_eq!(set.len(), 1);
        assert!(set.iter().next().unwrap().is_directory_link);
    }

    #[test]
    fn test_collect_referenced_paths_honors_the_requested_policy() {
        let td = tempdir_helper::TempDir::new("collect-policy");
        write_file(&td.path().join("body.org"), "* H\n");
        write_file(&td.path().join("pointer.org"), "* H\n");
        write_file(&td.path().join("prose.org"), "* H\n");
        write_file(&td.path().join("note.org"), "* H\n");
        write_file(
            &td.path().join("a.org"),
            indoc! {"
                * H
                :PROPERTIES:
                :PREDECESSOR: [[file:pointer.org][p]]
                :NOTE: mentions [[file:prose.org][x]] in passing
                :END:
                body [[file:body.org][b]]
                :LOGBOOK:
                - note [[file:note.org][n]]
                :END:
            "},
        );

        let names = |policy| -> Vec<String> {
            let mut v: Vec<String> = collect_referenced_paths(&[td.path().join("a.org")], policy)
                .into_iter()
                .map(|t| t.path.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };

        assert_eq!(
            names(OriginPolicy::BodyOnly),
            vec!["body.org"],
            "orphans keeps its historical edge set"
        );
        assert_eq!(
            names(OriginPolicy::BodyAndPurePointers),
            vec!["body.org", "pointer.org"],
            "pure pointers count; prose-embedded citations do not"
        );
        assert_eq!(
            names(OriginPolicy::AllFollowable),
            vec!["body.org", "note.org", "pointer.org", "prose.org"],
        );
    }
}
