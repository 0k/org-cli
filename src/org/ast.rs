//! Semantic, loss-aware JSON AST for complete Org documents.
//!
//! This is the *presentation* projection: a total, side-effect-free
//! rendering of a whole Org document for clients that display it.  It is a
//! sibling of the query/edit types in this module tree, not a replacement
//! for them — [`super::heading::Heading`] answers "which headings exist and
//! what are their fields", while [`parse_ast`] answers "what does this
//! entire document contain, structurally".  It owns no cache, performs no
//! I/O, and never mutates a file.
//!
//! ## Ownership
//!
//! This converter previously lived in `vigil-watch`
//! (`src/data_flow/org_ast.rs`), which made the ecosystem carry two
//! divergent Org parsers.  It now lives here, with `org-cli` as the single
//! owner of Org parsing, and `vigil-watch` depends on this crate rather
//! than duplicating it.  Consumers today:
//!
//! * `vigil-watch` serves it from its Org AST endpoint;
//! * `vigil-ui` renders it, and mirrors this exact shape in TypeScript at
//!   `frontend/src/types/orgAst.ts`.
//!
//! The serialized shape is therefore a *cross-language contract*.
//! `tests/fixtures/ast-contract.org` and its companion
//! `tests/fixtures/ast-contract.json` pin it so the Rust and
//! TypeScript shapes cannot drift apart; see
//! `tests/ast_contract.rs`.  Changing a field name or a `type`
//! discriminator here is a breaking change for the renderer.
//!
//! ## Patched `orgize` dependency
//!
//! `Cargo.toml` patches `orgize` to the `0k/orgize` fork.
//! That fork fixes native parsing of timezone-bearing clocks and introduces
//! `TIMESTAMP_SECOND` and `TIMESTAMP_TZ_OFFSET`.  This walker consumes those
//! native rowan nodes and tokens directly.  In particular, clocks inside
//! `:LOGBOOK:` drawers are ordinary [`orgize::SyntaxKind::CLOCK`] children;
//! there is intentionally no regex or supplementary clock grammar here.
//!
//! Without that patch a canonical Emacs `CLOCK:` line — which carries a
//! numerical UTC offset, and often seconds — silently degrades to a plain
//! paragraph.  That failure is *silent*, so it is pinned by an explicit
//! test rather than left to be noticed downstream.
//!
//! ## Loss and payload policy
//!
//! Every JSON node carries a rowan byte range under `source`.  Known Org
//! delimiters (headline stars, emphasis markers, drawer borders, and similar
//! markup) disappear from the semantic payload but remain recoverable from
//! the endpoint's original `content` using that range.  `raw` text is emitted
//! only for [`OrgAstNode::Raw`] fallbacks and [`OrgAstNode::Timestamp`], whose
//! exact spelling matters for repeaters, delays, and diary forms.  Repeating
//! raw source on every semantic node would substantially inflate item
//! responses without adding information because the response already carries
//! the original content.
//! Affiliated keywords are attached to their owning element as ordered
//! `{key, value}` entries, rather than emitted as separate keyword nodes.
//! Inline scripts retain their original bracing, verse exposes parsed inline
//! children alongside its raw value, and timestamp cookies keep their raw
//! spelling while date/time endpoints remain structured.
//!
//! Unknown or unsupported rowan syntax is never skipped: the whole node or
//! token becomes a `raw` node containing its exact source text.  This is the
//! forward-compatibility boundary for new `orgize` syntax kinds.

use orgize::ast::{Clock, Entity, Headline, InlineSrc, List, SourceBlock, Timestamp, TodoType};
use orgize::rowan::{NodeOrToken, ast::AstNode};
use orgize::{Org, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};
use serde::Serialize;

/// Half-open UTF-8 byte range in the original Org input.
///
/// Rowan offsets are bytes, not Unicode scalar indices.  Consumers can safely
/// recover a node with `content.slice(source.start, source.end)` only when the
/// slicing API also uses UTF-8 byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SourceRange {
    pub start: u32,
    pub end: u32,
}

/// Structured date/time fields for one end of a timestamp range.
///
/// Every field is nullable because Org timestamps may omit time fields, and
/// diary timestamps carry no ordinary calendar tokens at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimestampValue {
    pub year: Option<String>,
    pub month: Option<String>,
    pub day: Option<String>,
    pub dayname: Option<String>,
    pub hour: Option<String>,
    pub minute: Option<String>,
    pub second: Option<String>,
    pub tz_offset: Option<String>,
}

/// TODO state attached to a headline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OrgTodoType {
    Todo,
    Done,
}

/// Planning keyword associated with a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanningKind {
    Scheduled,
    Deadline,
    Closed,
}

/// One planning entry in source order.
///
/// A vector is used rather than three optional fields so repeated planning
/// keywords and their original interleaving cannot be silently collapsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanningEntry {
    pub source: SourceRange,
    pub kind: PlanningKind,
    pub timestamp: Box<OrgAstNode>,
}

/// An affiliated Org keyword attached to the following element, in source order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AffiliatedKeyword {
    pub key: String,
    pub value: String,
}

/// Complete semantic Org AST node.
///
/// The enum is internally tagged, so every serialized node has a kebab-case
/// `type` discriminator followed by the fields documented on its variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum OrgAstNode {
    Document {
        source: SourceRange,
        children: Vec<OrgAstNode>,
    },
    Section {
        source: SourceRange,
        children: Vec<OrgAstNode>,
    },
    Headline {
        source: SourceRange,
        level: usize,
        title: Vec<OrgAstNode>,
        title_raw: String,
        todo_keyword: Option<String>,
        todo_type: Option<OrgTodoType>,
        priority: Option<String>,
        tags: Vec<String>,
        commented: bool,
        archived: bool,
        children: Vec<OrgAstNode>,
    },
    PropertyDrawer {
        source: SourceRange,
        properties: Vec<OrgAstNode>,
    },
    NodeProperty {
        source: SourceRange,
        key: String,
        value: String,
    },
    Drawer {
        source: SourceRange,
        name: String,
        children: Vec<OrgAstNode>,
    },
    Planning {
        source: SourceRange,
        entries: Vec<PlanningEntry>,
    },
    Clock {
        source: SourceRange,
        start: TimestampValue,
        end: Option<TimestampValue>,
        duration: Option<String>,
        running: bool,
    },
    Timestamp {
        source: SourceRange,
        start: TimestampValue,
        end: Option<TimestampValue>,
        active: bool,
        range: bool,
        raw: String,
        /// Raw repeater cookie (for example `+1w`), or null.
        repeater: Option<String>,
        /// Raw warning/delay cookie (for example `-3d`), or null.
        warning: Option<String>,
    },
    Keyword {
        source: SourceRange,
        key: String,
        value: String,
    },
    Paragraph {
        source: SourceRange,
        affiliated: Vec<AffiliatedKeyword>,
        children: Vec<OrgAstNode>,
    },
    PlainList {
        source: SourceRange,
        affiliated: Vec<AffiliatedKeyword>,
        ordered: bool,
        descriptive: bool,
        children: Vec<OrgAstNode>,
    },
    ListItem {
        source: SourceRange,
        indent: usize,
        bullet: String,
        checkbox: Option<String>,
        counter: Option<String>,
        tag: Vec<OrgAstNode>,
        children: Vec<OrgAstNode>,
    },
    Table {
        source: SourceRange,
        affiliated: Vec<AffiliatedKeyword>,
        has_header: bool,
        children: Vec<OrgAstNode>,
    },
    TableRow {
        source: SourceRange,
        rule: bool,
        children: Vec<OrgAstNode>,
    },
    TableCell {
        source: SourceRange,
        children: Vec<OrgAstNode>,
    },
    SrcBlock {
        source: SourceRange,
        affiliated: Vec<AffiliatedKeyword>,
        language: Option<String>,
        switches: Option<String>,
        parameters: Option<String>,
        value: String,
    },
    QuoteBlock {
        source: SourceRange,
        affiliated: Vec<AffiliatedKeyword>,
        children: Vec<OrgAstNode>,
    },
    ExampleBlock {
        source: SourceRange,
        affiliated: Vec<AffiliatedKeyword>,
        value: String,
    },
    VerseBlock {
        source: SourceRange,
        affiliated: Vec<AffiliatedKeyword>,
        value: String,
        /// Inline Org objects in the verse body; line breaks remain text.
        children: Vec<OrgAstNode>,
    },
    CenterBlock {
        source: SourceRange,
        affiliated: Vec<AffiliatedKeyword>,
        children: Vec<OrgAstNode>,
    },
    ExportBlock {
        source: SourceRange,
        affiliated: Vec<AffiliatedKeyword>,
        backend: Option<String>,
        value: String,
    },
    SpecialBlock {
        source: SourceRange,
        affiliated: Vec<AffiliatedKeyword>,
        name: String,
        children: Vec<OrgAstNode>,
    },
    Comment {
        source: SourceRange,
        value: String,
    },
    CommentBlock {
        source: SourceRange,
        affiliated: Vec<AffiliatedKeyword>,
        value: String,
    },
    FixedWidth {
        source: SourceRange,
        affiliated: Vec<AffiliatedKeyword>,
        value: String,
    },
    HorizontalRule {
        source: SourceRange,
    },
    Link {
        source: SourceRange,
        path: String,
        link_type: String,
        description: Option<Vec<OrgAstNode>>,
    },
    Bold {
        source: SourceRange,
        children: Vec<OrgAstNode>,
    },
    Italic {
        source: SourceRange,
        children: Vec<OrgAstNode>,
    },
    Underline {
        source: SourceRange,
        children: Vec<OrgAstNode>,
    },
    StrikeThrough {
        source: SourceRange,
        children: Vec<OrgAstNode>,
    },
    Code {
        source: SourceRange,
        value: String,
    },
    Verbatim {
        source: SourceRange,
        value: String,
    },
    Text {
        source: SourceRange,
        text: String,
    },
    LineBreak {
        source: SourceRange,
    },
    Entity {
        source: SourceRange,
        name: String,
        value: String,
    },
    FootnoteReference {
        source: SourceRange,
        label: Option<String>,
        definition: Vec<OrgAstNode>,
    },
    FootnoteDefinition {
        source: SourceRange,
        affiliated: Vec<AffiliatedKeyword>,
        label: String,
        children: Vec<OrgAstNode>,
    },
    LatexFragment {
        source: SourceRange,
        value: String,
    },
    Superscript {
        source: SourceRange,
        /// Whether the source used `^{…}` rather than `^x`.
        braced: bool,
        children: Vec<OrgAstNode>,
    },
    Subscript {
        source: SourceRange,
        /// Whether the source used `_{…}` rather than `_x`.
        braced: bool,
        children: Vec<OrgAstNode>,
    },
    Target {
        source: SourceRange,
        value: String,
    },
    RadioTarget {
        source: SourceRange,
        value: String,
    },
    Macro {
        source: SourceRange,
        name: String,
        arguments: Option<String>,
    },
    InlineSrc {
        source: SourceRange,
        language: String,
        parameters: Option<String>,
        value: String,
    },
    StatisticsCookie {
        source: SourceRange,
        value: String,
    },
    Raw {
        source: SourceRange,
        text: String,
    },
}

/// Root type returned by [`parse_ast`].
///
/// The value is always [`OrgAstNode::Document`]; the alias keeps the public
/// signature explicit without introducing a second serialization shape.
pub type OrgAstDocument = OrgAstNode;

/// Parse any UTF-8 string into a total, side-effect-free semantic AST.
///
/// `orgize` is permissive and produces a rowan tree even for incomplete Org
/// input.  This function preserves that totality: malformed recognized nodes
/// degrade to [`OrgAstNode::Raw`] instead of panicking or returning an error.
pub fn parse_ast(s: &str) -> OrgAstDocument {
    let org = Org::parse(s);
    node_to_ast(org.document().syntax())
}

fn source_of_node(node: &SyntaxNode) -> SourceRange {
    let range = node.text_range();
    SourceRange {
        start: u32::from(range.start()),
        end: u32::from(range.end()),
    }
}

fn source_of_token(token: &SyntaxToken) -> SourceRange {
    let range = token.text_range();
    SourceRange {
        start: u32::from(range.start()),
        end: u32::from(range.end()),
    }
}

fn raw_node(node: &SyntaxNode) -> OrgAstNode {
    OrgAstNode::Raw {
        source: source_of_node(node),
        text: node.to_string(),
    }
}

fn raw_token(token: &SyntaxToken) -> OrgAstNode {
    OrgAstNode::Raw {
        source: source_of_token(token),
        text: token.text().to_string(),
    }
}

fn text_or_raw_token(token: &SyntaxToken) -> OrgAstNode {
    match token.kind() {
        SyntaxKind::TEXT
        | SyntaxKind::WHITESPACE
        | SyntaxKind::NEW_LINE
        | SyntaxKind::BLANK_LINE => OrgAstNode::Text {
            source: source_of_token(token),
            text: token.text().to_string(),
        },
        _ => raw_token(token),
    }
}

fn element_to_ast(element: SyntaxElement) -> OrgAstNode {
    match element {
        NodeOrToken::Node(node) => node_to_ast(&node),
        NodeOrToken::Token(token) => text_or_raw_token(&token),
    }
}

fn children_to_ast(node: &SyntaxNode) -> Vec<OrgAstNode> {
    node.children_with_tokens()
        .filter(|element| element.kind() != SyntaxKind::AFFILIATED_KEYWORD)
        .map(element_to_ast)
        .collect()
}

fn affiliated(node: &SyntaxNode) -> Vec<AffiliatedKeyword> {
    node.children()
        .filter(|child| child.kind() == SyntaxKind::AFFILIATED_KEYWORD)
        .filter_map(|child| {
            let texts = token_texts(&child, SyntaxKind::TEXT);
            Some(AffiliatedKeyword {
                key: texts.first()?.to_ascii_uppercase(),
                value: texts.get(1)?.trim().to_string(),
            })
        })
        .collect()
}

fn node_to_ast(node: &SyntaxNode) -> OrgAstNode {
    let source = source_of_node(node);
    match node.kind() {
        SyntaxKind::DOCUMENT => OrgAstNode::Document {
            source,
            children: children_to_ast(node),
        },
        SyntaxKind::SECTION => OrgAstNode::Section {
            source,
            children: children_to_ast(node),
        },
        SyntaxKind::HEADLINE => headline_node(node).unwrap_or_else(|| raw_node(node)),
        SyntaxKind::PROPERTY_DRAWER => property_drawer_node(node),
        SyntaxKind::NODE_PROPERTY => node_property_node(node).unwrap_or_else(|| raw_node(node)),
        SyntaxKind::DRAWER => drawer_node(node).unwrap_or_else(|| raw_node(node)),
        SyntaxKind::PLANNING => planning_node(node).unwrap_or_else(|| raw_node(node)),
        SyntaxKind::CLOCK => clock_node(node).unwrap_or_else(|| raw_node(node)),
        SyntaxKind::TIMESTAMP_ACTIVE
        | SyntaxKind::TIMESTAMP_INACTIVE
        | SyntaxKind::TIMESTAMP_DIARY => timestamp_node(node).unwrap_or_else(|| raw_node(node)),
        SyntaxKind::KEYWORD | SyntaxKind::AFFILIATED_KEYWORD => {
            keyword_node(node).unwrap_or_else(|| raw_node(node))
        }
        SyntaxKind::PARAGRAPH => OrgAstNode::Paragraph {
            source,
            affiliated: affiliated(node),
            children: children_to_ast(node),
        },
        SyntaxKind::LIST => list_node(node).unwrap_or_else(|| raw_node(node)),
        SyntaxKind::LIST_ITEM => list_item_node(node).unwrap_or_else(|| raw_node(node)),
        SyntaxKind::ORG_TABLE => table_node(node),
        SyntaxKind::FN_DEF => footnote_definition_node(node).unwrap_or_else(|| raw_node(node)),
        SyntaxKind::ORG_TABLE_STANDARD_ROW | SyntaxKind::ORG_TABLE_RULE_ROW => table_row_node(node),
        SyntaxKind::ORG_TABLE_CELL => OrgAstNode::TableCell {
            source,
            children: children_to_ast(node),
        },
        SyntaxKind::SOURCE_BLOCK => source_block_node(node).unwrap_or_else(|| raw_node(node)),
        SyntaxKind::QUOTE_BLOCK => OrgAstNode::QuoteBlock {
            source,
            affiliated: affiliated(node),
            children: block_children(node),
        },
        SyntaxKind::EXAMPLE_BLOCK => OrgAstNode::ExampleBlock {
            source,
            affiliated: affiliated(node),
            value: block_value(node),
        },
        SyntaxKind::VERSE_BLOCK => OrgAstNode::VerseBlock {
            source,
            affiliated: affiliated(node),
            value: block_value(node),
            children: block_inline_children(node),
        },
        SyntaxKind::CENTER_BLOCK => OrgAstNode::CenterBlock {
            source,
            affiliated: affiliated(node),
            children: block_children(node),
        },
        SyntaxKind::EXPORT_BLOCK => export_block_node(node),
        SyntaxKind::SPECIAL_BLOCK => special_block_node(node).unwrap_or_else(|| raw_node(node)),
        SyntaxKind::COMMENT => OrgAstNode::Comment {
            source,
            value: direct_text(node),
        },
        SyntaxKind::COMMENT_BLOCK => OrgAstNode::CommentBlock {
            source,
            affiliated: affiliated(node),
            value: block_value(node),
        },
        SyntaxKind::FIXED_WIDTH => OrgAstNode::FixedWidth {
            source,
            affiliated: affiliated(node),
            value: direct_text(node),
        },
        SyntaxKind::RULE => OrgAstNode::HorizontalRule { source },
        SyntaxKind::LINK => link_node(node).unwrap_or_else(|| raw_node(node)),
        SyntaxKind::BOLD => OrgAstNode::Bold {
            source,
            children: inline_children_without(node, &[SyntaxKind::STAR]),
        },
        SyntaxKind::ITALIC => OrgAstNode::Italic {
            source,
            children: inline_children_without(node, &[SyntaxKind::SLASH]),
        },
        SyntaxKind::UNDERLINE => OrgAstNode::Underline {
            source,
            children: inline_children_without(node, &[SyntaxKind::UNDERSCORE]),
        },
        SyntaxKind::STRIKE => OrgAstNode::StrikeThrough {
            source,
            children: inline_children_without(node, &[SyntaxKind::PLUS]),
        },
        SyntaxKind::CODE => OrgAstNode::Code {
            source,
            value: direct_text(node),
        },
        SyntaxKind::VERBATIM => OrgAstNode::Verbatim {
            source,
            value: direct_text(node),
        },
        SyntaxKind::LINE_BREAK => OrgAstNode::LineBreak { source },
        SyntaxKind::ENTITY => entity_node(node).unwrap_or_else(|| raw_node(node)),
        SyntaxKind::FN_REF => footnote_reference_node(node),
        SyntaxKind::LATEX_FRAGMENT => OrgAstNode::LatexFragment {
            source,
            value: direct_text(node),
        },
        SyntaxKind::SUPERSCRIPT => OrgAstNode::Superscript {
            source,
            braced: first_direct_token(node, SyntaxKind::L_CURLY).is_some(),
            children: inline_children_without(
                node,
                &[SyntaxKind::CARET, SyntaxKind::L_CURLY, SyntaxKind::R_CURLY],
            ),
        },
        SyntaxKind::SUBSCRIPT => OrgAstNode::Subscript {
            source,
            braced: first_direct_token(node, SyntaxKind::L_CURLY).is_some(),
            children: inline_children_without(
                node,
                &[
                    SyntaxKind::UNDERSCORE,
                    SyntaxKind::L_CURLY,
                    SyntaxKind::R_CURLY,
                ],
            ),
        },
        SyntaxKind::TARGET => OrgAstNode::Target {
            source,
            value: direct_text(node),
        },
        SyntaxKind::RADIO_TARGET => OrgAstNode::RadioTarget {
            source,
            value: direct_text(node),
        },
        SyntaxKind::MACROS => macro_node(node).unwrap_or_else(|| raw_node(node)),
        SyntaxKind::INLINE_SRC => inline_src_node(node).unwrap_or_else(|| raw_node(node)),
        SyntaxKind::COOKIE => OrgAstNode::StatisticsCookie {
            source,
            value: cookie_value(node),
        },
        _ => raw_node(node),
    }
}

fn headline_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let headline = Headline::cast(node.clone())?;
    let title_node = node
        .children()
        .find(|child| child.kind() == SyntaxKind::HEADLINE_TITLE);
    let title = title_node.as_ref().map(children_to_ast).unwrap_or_default();
    let children = node
        .children()
        .filter(|child| {
            matches!(
                child.kind(),
                SyntaxKind::PLANNING
                    | SyntaxKind::PROPERTY_DRAWER
                    | SyntaxKind::SECTION
                    | SyntaxKind::HEADLINE
            )
        })
        .map(|child| node_to_ast(&child))
        .collect();
    let todo_type = match headline.todo_type() {
        Some(TodoType::Todo) => Some(OrgTodoType::Todo),
        Some(TodoType::Done) => Some(OrgTodoType::Done),
        None => None,
    };

    Some(OrgAstNode::Headline {
        source: source_of_node(node),
        level: headline.level(),
        title,
        title_raw: headline.title_raw(),
        todo_keyword: headline.todo_keyword().map(|token| token.to_string()),
        todo_type,
        priority: headline.priority().map(|token| token.to_string()),
        tags: headline.tags().map(|token| token.to_string()).collect(),
        commented: headline.is_commented(),
        archived: headline.is_archived(),
        children,
    })
}

fn property_drawer_node(node: &SyntaxNode) -> OrgAstNode {
    let properties = node
        .children()
        .filter(|child| child.kind() == SyntaxKind::NODE_PROPERTY)
        .map(|child| node_to_ast(&child))
        .collect();
    OrgAstNode::PropertyDrawer {
        source: source_of_node(node),
        properties,
    }
}

fn node_property_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let texts = token_texts(node, SyntaxKind::TEXT);
    Some(OrgAstNode::NodeProperty {
        source: source_of_node(node),
        key: texts.first()?.clone(),
        value: texts.get(1)?.clone(),
    })
}

fn drawer_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let begin = node
        .children()
        .find(|child| child.kind() == SyntaxKind::DRAWER_BEGIN)?;
    let name = token_texts(&begin, SyntaxKind::TEXT).first()?.clone();
    let children = node
        .children()
        .find(|child| child.kind() == SyntaxKind::DRAWER_CONTENT)
        .map(|content| children_to_ast(&content))
        .unwrap_or_default();
    Some(OrgAstNode::Drawer {
        source: source_of_node(node),
        name,
        children,
    })
}

fn planning_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let mut entries = Vec::new();
    for child in node.children() {
        let kind = match child.kind() {
            SyntaxKind::PLANNING_SCHEDULED => PlanningKind::Scheduled,
            SyntaxKind::PLANNING_DEADLINE => PlanningKind::Deadline,
            SyntaxKind::PLANNING_CLOSED => PlanningKind::Closed,
            _ => return None,
        };
        let timestamp = child.children().find(|candidate| {
            matches!(
                candidate.kind(),
                SyntaxKind::TIMESTAMP_ACTIVE
                    | SyntaxKind::TIMESTAMP_INACTIVE
                    | SyntaxKind::TIMESTAMP_DIARY
            )
        })?;
        entries.push(PlanningEntry {
            source: source_of_node(&child),
            kind,
            timestamp: Box::new(timestamp_node(&timestamp)?),
        });
    }
    Some(OrgAstNode::Planning {
        source: source_of_node(node),
        entries,
    })
}

fn clock_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let clock = Clock::cast(node.clone())?;
    let timestamp = clock.value()?;
    let (start, end, _) = timestamp_values(&timestamp);
    Some(OrgAstNode::Clock {
        source: source_of_node(node),
        start,
        end,
        duration: clock.duration().map(|token| token.to_string()),
        running: clock.is_running(),
    })
}

fn timestamp_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let timestamp = Timestamp::cast(node.clone())?;
    let (start, end, range) = timestamp_values(&timestamp);
    Some(OrgAstNode::Timestamp {
        source: source_of_node(node),
        start,
        end,
        active: timestamp.is_active(),
        range,
        raw: node.to_string(),
        repeater: timestamp_cookie(node, SyntaxKind::TIMESTAMP_REPEATER_MARK),
        warning: timestamp_cookie(node, SyntaxKind::TIMESTAMP_DELAY_MARK),
    })
}

fn timestamp_cookie(node: &SyntaxNode, mark: SyntaxKind) -> Option<String> {
    let tokens: Vec<_> = node
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .collect();
    tokens
        .windows(3)
        .find(|window| {
            window[0].kind() == mark
                && window[1].kind() == SyntaxKind::TIMESTAMP_VALUE
                && window[2].kind() == SyntaxKind::TIMESTAMP_UNIT
        })
        .map(|window| window.iter().map(|token| token.text()).collect())
}

fn timestamp_values(timestamp: &Timestamp) -> (TimestampValue, Option<TimestampValue>, bool) {
    let syntax = timestamp.syntax();
    let range = timestamp.is_range();
    let start = timestamp_value(syntax, false);
    let end = range.then(|| timestamp_value(syntax, true));
    (start, end, range)
}

fn timestamp_value(node: &SyntaxNode, end: bool) -> TimestampValue {
    TimestampValue {
        year: endpoint_token(node, SyntaxKind::TIMESTAMP_YEAR, end),
        month: endpoint_token(node, SyntaxKind::TIMESTAMP_MONTH, end),
        day: endpoint_token(node, SyntaxKind::TIMESTAMP_DAY, end),
        dayname: endpoint_token(node, SyntaxKind::TIMESTAMP_DAYNAME, end),
        hour: endpoint_token(node, SyntaxKind::TIMESTAMP_HOUR, end),
        minute: endpoint_token(node, SyntaxKind::TIMESTAMP_MINUTE, end),
        second: endpoint_token(node, SyntaxKind::TIMESTAMP_SECOND, end),
        tz_offset: endpoint_token(node, SyntaxKind::TIMESTAMP_TZ_OFFSET, end),
    }
}

fn endpoint_token(node: &SyntaxNode, kind: SyntaxKind, end: bool) -> Option<String> {
    let values = token_texts(node, kind);
    if end {
        values.last().cloned()
    } else {
        values.first().cloned()
    }
}

fn keyword_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let texts = token_texts(node, SyntaxKind::TEXT);
    Some(OrgAstNode::Keyword {
        source: source_of_node(node),
        key: texts.first()?.clone(),
        value: texts.get(1)?.clone(),
    })
}

fn list_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let list = List::cast(node.clone())?;
    Some(OrgAstNode::PlainList {
        source: source_of_node(node),
        affiliated: affiliated(node),
        ordered: list.is_ordered(),
        descriptive: list.is_descriptive(),
        children: node
            .children()
            .filter(|child| child.kind() == SyntaxKind::LIST_ITEM)
            .map(|child| node_to_ast(&child))
            .collect(),
    })
}

fn list_item_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let indent = first_direct_token(node, SyntaxKind::LIST_ITEM_INDENT)?;
    let bullet = first_direct_token(node, SyntaxKind::LIST_ITEM_BULLET)?;
    let checkbox = child_text(node, SyntaxKind::LIST_ITEM_CHECK_BOX);
    let counter = child_text(node, SyntaxKind::LIST_ITEM_COUNTER);
    let tag = node
        .children()
        .find(|child| child.kind() == SyntaxKind::LIST_ITEM_TAG)
        .map(|tag| {
            tag.children_with_tokens()
                .filter(|element| element.kind() != SyntaxKind::COLON2)
                .map(element_to_ast)
                .collect()
        })
        .unwrap_or_default();
    let children = node
        .children()
        .find(|child| child.kind() == SyntaxKind::LIST_ITEM_CONTENT)
        .map(|content| children_to_ast(&content))
        .unwrap_or_default();
    Some(OrgAstNode::ListItem {
        source: source_of_node(node),
        indent: indent.len(),
        bullet,
        checkbox,
        counter,
        tag,
        children,
    })
}

fn table_node(node: &SyntaxNode) -> OrgAstNode {
    let has_header =
        orgize::ast::OrgTable::cast(node.clone()).is_some_and(|table| table.has_header());
    OrgAstNode::Table {
        source: source_of_node(node),
        affiliated: affiliated(node),
        has_header,
        children: node
            .children()
            .filter(|child| {
                matches!(
                    child.kind(),
                    SyntaxKind::ORG_TABLE_STANDARD_ROW | SyntaxKind::ORG_TABLE_RULE_ROW
                )
            })
            .map(|child| node_to_ast(&child))
            .collect(),
    }
}

fn table_row_node(node: &SyntaxNode) -> OrgAstNode {
    OrgAstNode::TableRow {
        source: source_of_node(node),
        rule: node.kind() == SyntaxKind::ORG_TABLE_RULE_ROW,
        children: node
            .children()
            .filter(|child| child.kind() == SyntaxKind::ORG_TABLE_CELL)
            .map(|child| node_to_ast(&child))
            .collect(),
    }
}

fn source_block_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let block = SourceBlock::cast(node.clone())?;
    Some(OrgAstNode::SrcBlock {
        source: source_of_node(node),
        affiliated: affiliated(node),
        language: block.language().map(|token| token.to_string()),
        switches: block.switches().map(|token| token.to_string()),
        parameters: block.parameters().map(|token| token.to_string()),
        value: block.value(),
    })
}

fn block_children(node: &SyntaxNode) -> Vec<OrgAstNode> {
    node.children()
        .find(|child| child.kind() == SyntaxKind::BLOCK_CONTENT)
        .map(|content| children_to_ast(&content))
        .unwrap_or_default()
}

fn block_inline_children(node: &SyntaxNode) -> Vec<OrgAstNode> {
    node.children()
        .find(|child| child.kind() == SyntaxKind::BLOCK_CONTENT)
        .map(|content| {
            content
                .children_with_tokens()
                .filter(|element| element.kind() != SyntaxKind::COMMA)
                .map(element_to_ast)
                .collect()
        })
        .unwrap_or_default()
}

/// Concatenate a lesser block's content as its **semantic** value:
/// the escaping commas Org requires on `,*` / `,#+` lines are
/// removed.
///
/// Org makes this the parser's job, not the consumer's: "parser
/// removes all the commas in `,*` and `,#+` lines"
/// (`org-syntax.html` § Blocks), and the manual promises Org
/// "transparently strips these additional commas whenever it
/// accesses the contents of the block".  Emacs implements it in
/// `org-element` itself — `org-element-example-block-parser` builds
/// `:value` through `org-unescape-code-in-string`.
///
/// `orgize` already does the separation for us: its block parser
/// emits each escaping comma as its own [`SyntaxKind::COMMA`] token
/// rather than folding it into the surrounding text, which is why
/// its own [`orgize::ast::SourceBlock::value`] is documented to
/// "return unescaped source code string".  Keeping only `TEXT`
/// tokens here mirrors that accessor.
///
/// Stringifying the whole `BLOCK_CONTENT` node instead would
/// re-serialize those `COMMA` tokens and hand callers the raw
/// concrete syntax — a literal `,#+end_example` where the author
/// wrote `#+end_example`.
///
/// [`escape_org_literal_lines`] is the inverse: it adds exactly the
/// commas this function removes.  The two must agree on which lines
/// carry an escape, and a round-trip test pins them together.
fn block_value(node: &SyntaxNode) -> String {
    node.children()
        .find(|child| child.kind() == SyntaxKind::BLOCK_CONTENT)
        .map(|content| {
            content
                .descendants_with_tokens()
                .filter_map(|element| element.into_token())
                .filter(|token| token.kind() != SyntaxKind::COMMA)
                .map(|token| token.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Comma-escape the lines of `text` so it can sit inside a lesser
/// block (`#+begin_example`, `#+begin_verse`, `#+begin_src`) without
/// being reinterpreted as markup.  The exact inverse of
/// [`block_value`].
///
/// Org's literal-block escape is a leading comma, and it applies to
/// exactly one line shape: a run of commas at **column 0** followed
/// by `*` or `#+` — the two prefixes that would end the block or
/// open a structure inside it.  Every other line is left
/// byte-identical.
///
/// This escape is a LESSER-block mechanism.  A greater block
/// (`#+begin_quote`, `#+begin_center`) parses its contents as
/// elements rather than text, so `orgize` never tokenizes a comma
/// there and [`block_value`] cannot strip one back off: escaping
/// into a quote block leaks a literal comma to the reader.  Wrap
/// arbitrary text in a lesser block, not a quote.
///
/// The condition mirrors the unescaper rather than merely being
/// sufficient, because escaping is only correct if it is reversible.
/// Two asymmetries would otherwise corrupt the payload:
///
/// * **An ordinary leading comma is not an escape.**  Escaping
///   `,serde: expected value` yields `,,serde: …`, which no
///   unescaper undoes, so the reader sees an extra comma.
/// * **The escape is column-0 only.**  `orgize`'s block parser
///   tests `s[i..]` against `",*"` / `",#+"` at each line start with
///   no leading-whitespace allowance, so a comma written before an
///   indent (`,  * x`) is never recognised as an escape and survives
///   into the value verbatim.
/// * **Only a single-comma run is stripped.**  `,* x` unescapes to
///   `* x`, but `,,* x` stays `,,* x` (Emacs would strip one comma
///   here; `orgize` does not).  So a line that ALREADY begins with a
///   comma run followed by a sigil cannot be escaped reversibly, and
///   is left untouched: it is inert inside the block either way,
///   since only a bare `*` / `#+` at column 0 threatens the fence.
///
/// An indented sigil needs no escape in the first place: an indented
/// `* x` is not a heading.  An indented `#+end_example` DOES still
/// close the block in Emacs (though not in `orgize`), and cannot be
/// protected by a comma, so callers must not indent a payload they
/// then escape — escape the un-indented text instead.
pub fn escape_org_literal_lines(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if line.starts_with('*') || line.starts_with("#+") {
            escaped.push(',');
        }
        escaped.push_str(line);
    }
    escaped
}

fn export_block_node(node: &SyntaxNode) -> OrgAstNode {
    let backend = node
        .children()
        .find(|child| child.kind() == SyntaxKind::BLOCK_BEGIN)
        .and_then(|begin| first_direct_token(&begin, SyntaxKind::EXPORT_BLOCK_TYPE));
    OrgAstNode::ExportBlock {
        source: source_of_node(node),
        affiliated: affiliated(node),
        backend,
        value: block_value(node),
    }
}

fn special_block_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let begin = node
        .children()
        .find(|child| child.kind() == SyntaxKind::BLOCK_BEGIN)?;
    let name = token_texts(&begin, SyntaxKind::TEXT).get(1)?.clone();
    Some(OrgAstNode::SpecialBlock {
        source: source_of_node(node),
        affiliated: affiliated(node),
        name,
        children: block_children(node),
    })
}

fn link_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let path = first_direct_token(node, SyntaxKind::LINK_PATH)?;
    let mut in_description = false;
    let mut description = Vec::new();
    for element in node.children_with_tokens() {
        if element.kind() == SyntaxKind::L_BRACKET {
            in_description = true;
            continue;
        }
        if element.kind() == SyntaxKind::R_BRACKET2 {
            break;
        }
        if in_description {
            description.push(element_to_ast(element));
        }
    }
    let link_type = path
        .split_once(':')
        .filter(|(prefix, _)| {
            !prefix.is_empty()
                && prefix
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        })
        .map(|(prefix, _)| prefix.to_ascii_lowercase())
        .unwrap_or_else(|| "fuzzy".to_string());
    Some(OrgAstNode::Link {
        source: source_of_node(node),
        path,
        link_type,
        description: in_description.then_some(description),
    })
}

fn inline_children_without(node: &SyntaxNode, delimiters: &[SyntaxKind]) -> Vec<OrgAstNode> {
    node.children_with_tokens()
        .filter(|element| !delimiters.contains(&element.kind()))
        .map(element_to_ast)
        .collect()
}

fn entity_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let entity = Entity::cast(node.clone())?;
    Some(OrgAstNode::Entity {
        source: source_of_node(node),
        name: entity.name().to_string(),
        value: entity.utf8().to_string(),
    })
}

fn footnote_reference_node(node: &SyntaxNode) -> OrgAstNode {
    let elements: Vec<SyntaxElement> = node.children_with_tokens().collect();
    let colon_positions: Vec<usize> = elements
        .iter()
        .enumerate()
        .filter_map(|(index, element)| (element.kind() == SyntaxKind::COLON).then_some(index))
        .collect();
    let label = colon_positions.first().and_then(|first_colon| {
        let second_colon = colon_positions.get(1).copied().unwrap_or(usize::MAX);
        elements
            .iter()
            .enumerate()
            .skip(first_colon + 1)
            .take_while(|(index, _)| *index < second_colon)
            .find_map(|(_, element)| {
                element
                    .as_token()
                    .filter(|token| token.kind() == SyntaxKind::TEXT)
                    .map(|token| token.text().to_string())
            })
    });
    let definition = colon_positions
        .get(1)
        .map(|second_colon| {
            elements
                .iter()
                .skip(second_colon + 1)
                .take_while(|element| element.kind() != SyntaxKind::R_BRACKET)
                .cloned()
                .map(element_to_ast)
                .collect()
        })
        .unwrap_or_default();
    OrgAstNode::FootnoteReference {
        source: source_of_node(node),
        label,
        definition,
    }
}

fn footnote_definition_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let texts = token_texts(node, SyntaxKind::TEXT);
    let label = texts.get(1)?.clone();
    Some(OrgAstNode::FootnoteDefinition {
        source: source_of_node(node),
        affiliated: affiliated(node),
        label,
        children: node
            .children()
            .filter(|child| child.kind() == SyntaxKind::PARAGRAPH)
            .map(|child| node_to_ast(&child))
            .collect(),
    })
}

fn macro_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let texts = token_texts(node, SyntaxKind::TEXT);
    Some(OrgAstNode::Macro {
        source: source_of_node(node),
        name: texts.first()?.clone(),
        arguments: texts.get(1).cloned(),
    })
}

fn inline_src_node(node: &SyntaxNode) -> Option<OrgAstNode> {
    let inline = InlineSrc::cast(node.clone())?;
    Some(OrgAstNode::InlineSrc {
        source: source_of_node(node),
        language: inline.language().to_string(),
        parameters: inline.parameters().map(|token| token.to_string()),
        value: inline.value().to_string(),
    })
}

fn cookie_value(node: &SyntaxNode) -> String {
    node.children_with_tokens()
        .filter_map(|element| match element {
            NodeOrToken::Token(token)
                if !matches!(token.kind(), SyntaxKind::L_BRACKET | SyntaxKind::R_BRACKET) =>
            {
                Some(token.text().to_string())
            }
            _ => None,
        })
        .collect()
}

fn direct_text(node: &SyntaxNode) -> String {
    token_texts(node, SyntaxKind::TEXT).concat()
}

fn child_text(node: &SyntaxNode, child_kind: SyntaxKind) -> Option<String> {
    node.children()
        .find(|child| child.kind() == child_kind)
        .and_then(|child| first_direct_token(&child, SyntaxKind::TEXT))
}

fn first_direct_token(node: &SyntaxNode, kind: SyntaxKind) -> Option<String> {
    node.children_with_tokens()
        .find_map(|element| match element {
            NodeOrToken::Token(token) if token.kind() == kind => Some(token.text().to_string()),
            _ => None,
        })
}

fn token_texts(node: &SyntaxNode, kind: SyntaxKind) -> Vec<String> {
    node.children_with_tokens()
        .filter_map(|element| match element {
            NodeOrToken::Token(token) if token.kind() == kind => Some(token.text().to_string()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use indoc::indoc;
    use serde_json::Value;

    use super::parse_ast;

    fn json(input: &str) -> Value {
        serde_json::to_value(parse_ast(input)).expect("AST must serialize")
    }

    fn collect_types(value: &Value, types: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(node_type)) = map.get("type") {
                    types.insert(node_type.clone());
                }
                for child in map.values() {
                    collect_types(child, types);
                }
            }
            Value::Array(values) => {
                for child in values {
                    collect_types(child, types);
                }
            }
            _ => {}
        }
    }

    fn nodes_of_type<'a>(value: &'a Value, node_type: &str, out: &mut Vec<&'a Value>) {
        match value {
            Value::Object(map) => {
                if map.get("type").and_then(Value::as_str) == Some(node_type) {
                    out.push(value);
                }
                for child in map.values() {
                    nodes_of_type(child, node_type, out);
                }
            }
            Value::Array(values) => {
                for child in values {
                    nodes_of_type(child, node_type, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn headlines_properties_planning_and_keywords_are_semantic() {
        let input = indoc! {"
            :PROPERTIES:
            :ID: doc-id
            :DATA_FLOW_PRODUCER: test
            :END:
            #+TITLE: Demo

            * TODO [#A] Fix *important* thing :work:urgent:
            SCHEDULED: <2026-07-29 Wed 10:00 -2d> DEADLINE: <2026-07-30 Thu +1w>
            :PROPERTIES:
            :EFFORT: 2:00
            :OWNER: Valentin
            :END:
            Body.
            ** DONE COMMENT Archived :ARCHIVE:
            CLOSED: [2026-07-28 Tue]
        "};
        let ast = json(input);

        assert_eq!(ast["type"], "document");
        assert_eq!(
            ast["source"],
            serde_json::json!({"start": 0, "end": input.len()})
        );

        let mut drawers = Vec::new();
        nodes_of_type(&ast, "property-drawer", &mut drawers);
        assert_eq!(drawers.len(), 2);
        assert_eq!(drawers[0]["properties"][0]["key"], "ID");
        assert_eq!(drawers[0]["properties"][1]["key"], "DATA_FLOW_PRODUCER");
        assert_eq!(drawers[1]["properties"][0]["key"], "EFFORT");
        assert_eq!(drawers[1]["properties"][1]["value"], "Valentin");

        let mut headlines = Vec::new();
        nodes_of_type(&ast, "headline", &mut headlines);
        assert_eq!(headlines.len(), 2);
        assert_eq!(headlines[0]["level"], 1);
        assert_eq!(headlines[0]["title_raw"], "Fix *important* thing ");
        assert_eq!(headlines[0]["todo_keyword"], "TODO");
        assert_eq!(headlines[0]["todo_type"], "todo");
        assert_eq!(headlines[0]["priority"], "A");
        assert_eq!(headlines[0]["tags"], serde_json::json!(["work", "urgent"]));
        assert_eq!(headlines[0]["title"][1]["type"], "bold");
        assert_eq!(headlines[1]["todo_type"], "done");
        assert_eq!(headlines[1]["commented"], true);
        assert_eq!(headlines[1]["archived"], true);

        let mut planning = Vec::new();
        nodes_of_type(&ast, "planning", &mut planning);
        assert_eq!(planning.len(), 2);
        assert_eq!(planning[0]["entries"][0]["kind"], "scheduled");
        assert_eq!(planning[0]["entries"][1]["kind"], "deadline");
        assert_eq!(planning[0]["entries"][0]["timestamp"]["type"], "timestamp");

        let mut keywords = Vec::new();
        nodes_of_type(&ast, "keyword", &mut keywords);
        assert_eq!(keywords[0]["key"], "TITLE");
        assert_eq!(keywords[0]["value"], " Demo");
    }

    #[test]
    fn native_clocks_expose_offsets_seconds_ranges_and_running_state() {
        let ast = json(indoc! {"
            * Clocks
            :LOGBOOK:
            CLOCK: [2026-07-28 Tue 09:30 +0200]--[2026-07-28 Tue 11:45 +0200] =>  2:15
            CLOCK: [2026-01-07 Wed 09:57:20 +0100]--[2026-01-07 Wed 12:19:25 +0100] =>  2:22:05
            CLOCK: [2026-07-28 Tue 14:00 +0200]
            :END:
        "});

        let mut clocks = Vec::new();
        nodes_of_type(&ast, "clock", &mut clocks);
        assert_eq!(clocks.len(), 3);
        assert_eq!(clocks[0]["start"]["tz_offset"], "+0200");
        assert_eq!(clocks[0]["end"]["hour"], "11");
        assert_eq!(clocks[0]["duration"], "2:15");
        assert_eq!(clocks[0]["running"], false);
        assert_eq!(clocks[1]["start"]["second"], "20");
        assert_eq!(clocks[1]["end"]["second"], "25");
        assert_eq!(clocks[1]["duration"], "2:22:05");
        assert_eq!(clocks[2]["end"], Value::Null);
        assert_eq!(clocks[2]["duration"], Value::Null);
        assert_eq!(clocks[2]["running"], true);
    }

    #[test]
    fn blocks_lists_tables_links_and_inline_objects_are_semantic() {
        let ast = json(indoc! {r#"
            - [ ] parent
              - [X] child
            1. [@3] ordered
            - term :: description

            | Name | Value |
            |------+-------|
            | one  | *two* |

            [[https://example.com]] [[file:/tmp/a.org][*file* link]] [[id:empty][]]
            *bold* /italic/ _underline_ +strike+ ~code~ =verbatim= \\alpha{}
            See [fn:note] $x+y$ x^2 y_{n} <<target>> <<<radio>>>
            {{{macro(a,b)}}} src_rust[:results raw]{let x = 1;} [50%]
            hard break\\
            next

            #+begin_src rust -n :results output
            fn main() {}
            #+end_src
            #+begin_src
            no language
            #+end_src
            #+begin_quote
            quoted *text*
            #+end_quote
            #+begin_example
            example
            #+end_example
            #+begin_verse
            verse
            #+end_verse
            #+begin_center
            centered
            #+end_center
            #+begin_export html
            <b>x</b>
            #+end_export
            #+begin_note
            special
            #+end_note
            #+begin_comment
            hidden
            #+end_comment
            # comment
            : fixed
            -----
        "#});

        let mut types = BTreeSet::new();
        collect_types(&ast, &mut types);
        for expected in [
            "plain-list",
            "list-item",
            "table",
            "table-row",
            "table-cell",
            "link",
            "bold",
            "italic",
            "underline",
            "strike-through",
            "code",
            "verbatim",
            "entity",
            "footnote-reference",
            "latex-fragment",
            "superscript",
            "subscript",
            "target",
            "radio-target",
            "macro",
            "inline-src",
            "statistics-cookie",
            "line-break",
            "src-block",
            "quote-block",
            "example-block",
            "verse-block",
            "center-block",
            "export-block",
            "special-block",
            "comment",
            "comment-block",
            "fixed-width",
            "horizontal-rule",
        ] {
            assert!(
                types.contains(expected),
                "missing semantic node {expected}: {types:?}"
            );
        }

        let mut source_blocks = Vec::new();
        nodes_of_type(&ast, "src-block", &mut source_blocks);
        assert_eq!(source_blocks[0]["language"], "rust");
        // orgize splits the `#+begin_src` header line on the LAST
        // whitespace run: everything between the language and that run
        // becomes SRC_BLOCK_SWITCHES, the remainder SRC_BLOCK_PARAMETERS.
        // So `rust -n :results output` yields switches `-n :results` and
        // parameters `output` — NOT the `-n` / `:results output` split a
        // reader of the org manual would expect.  We surface orgize's
        // tokenization verbatim rather than re-splitting it, so this
        // asserts the parser's real behavior.
        assert_eq!(source_blocks[0]["switches"], "-n :results");
        assert_eq!(source_blocks[0]["parameters"], "output");
        assert_eq!(source_blocks[0]["value"], "fn main() {}\n");
        assert_eq!(source_blocks[1]["language"], Value::Null);

        let mut links = Vec::new();
        nodes_of_type(&ast, "link", &mut links);
        assert_eq!(links[0]["path"], "https://example.com");
        assert_eq!(links[0]["link_type"], "https");
        assert_eq!(links[0]["description"], Value::Null);
        assert_eq!(links[1]["link_type"], "file");
        assert_eq!(links[1]["description"][0]["type"], "bold");
        assert_eq!(links[2]["link_type"], "id");
        assert_eq!(links[2]["description"], serde_json::json!([]));

        let mut rows = Vec::new();
        nodes_of_type(&ast, "table-row", &mut rows);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1]["rule"], true);
    }

    #[test]
    fn affiliated_keywords_attach_in_order_without_becoming_children() {
        let ast = json(indoc! {"
            #+TITLE: Kept standalone

            #+CAPTION: Table caption
            #+NAME: named-table
            #+ATTR_HTML: :class compact
            | Name | Value |
            |------+-------|
            | A    | 1     |

            #+NAME: python-sample
            #+begin_src python
            print(14)
            #+end_src

            #+RESULTS: python-sample
            : 14
        "});
        let mut tables = Vec::new();
        nodes_of_type(&ast, "table", &mut tables);
        assert_eq!(tables.len(), 1);
        assert_eq!(
            tables[0]["affiliated"],
            serde_json::json!([
                {"key":"CAPTION", "value":"Table caption"},
                {"key":"NAME", "value":"named-table"},
                {"key":"ATTR_HTML", "value":":class compact"}
            ])
        );
        let mut blocks = Vec::new();
        nodes_of_type(&ast, "src-block", &mut blocks);
        assert_eq!(
            blocks[0]["affiliated"],
            serde_json::json!([{"key":"NAME", "value":"python-sample"}])
        );
        let mut fixed = Vec::new();
        nodes_of_type(&ast, "fixed-width", &mut fixed);
        assert_eq!(
            fixed[0]["affiliated"],
            serde_json::json!([{"key":"RESULTS", "value":"python-sample"}])
        );
        let mut keywords = Vec::new();
        nodes_of_type(&ast, "keyword", &mut keywords);
        assert_eq!(keywords.len(), 1);
        assert_eq!(keywords[0]["key"], "TITLE");
    }

    #[test]
    fn every_affiliable_element_serializes_an_affiliated_array() {
        let ast = json(indoc! {"
            Paragraph.

            - list

            #+begin_example
            example
            #+end_example
            #+begin_export html
            export
            #+end_export
            #+begin_quote
            quote
            #+end_quote
            #+begin_center
            center
            #+end_center
            #+begin_note
            special
            #+end_note
            #+begin_comment
            comment
            #+end_comment
            : fixed

            [fn:note] Definition.
        "});
        for kind in [
            "paragraph",
            "plain-list",
            "example-block",
            "export-block",
            "quote-block",
            "center-block",
            "special-block",
            "comment-block",
            "fixed-width",
            "footnote-definition",
        ] {
            let mut nodes = Vec::new();
            nodes_of_type(&ast, kind, &mut nodes);
            assert!(!nodes.is_empty(), "missing {kind}");
            for node in nodes {
                assert_eq!(node["affiliated"], serde_json::json!([]), "{kind}");
            }
        }
    }

    #[test]
    fn plain_links_are_links_with_null_description_and_no_trailing_punctuation() {
        let ast = json(
            "Visit https://orgmode.org, (mailto:person@example.org) and file:/tmp/note.org. =https://literal.org= [[https://bracket.org][label]]\n",
        );
        let mut links = Vec::new();
        nodes_of_type(&ast, "link", &mut links);
        let paths: Vec<_> = links
            .iter()
            .map(|link| link["path"].as_str().unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                "https://orgmode.org",
                "mailto:person@example.org",
                "file:/tmp/note.org",
                "https://bracket.org"
            ]
        );
        assert_eq!(links[0]["description"], Value::Null);
        assert_eq!(links[1]["link_type"], "mailto");
        assert_eq!(links[2]["link_type"], "file");
        assert_eq!(links[3]["description"][0]["type"], "text");
    }

    #[test]
    fn scripts_report_bracing_without_changing_content() {
        let ast = json("H_{2} H_2 x^{n+1} x^2\n");
        let mut subs = Vec::new();
        nodes_of_type(&ast, "subscript", &mut subs);
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0]["braced"], true);
        assert_eq!(subs[1]["braced"], false);
        let mut sups = Vec::new();
        nodes_of_type(&ast, "superscript", &mut sups);
        assert_eq!(sups.len(), 2);
        assert_eq!(sups[0]["braced"], true);
        assert_eq!(sups[1]["braced"], false);
    }

    #[test]
    fn footnote_definitions_keep_label_and_parsed_children() {
        let ast = json("[fn:note] A *bold* definition.\n");
        let mut definitions = Vec::new();
        nodes_of_type(&ast, "footnote-definition", &mut definitions);
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0]["label"], "note");
        assert_eq!(definitions[0]["affiliated"], serde_json::json!([]));
        assert_eq!(definitions[0]["children"][0]["type"], "paragraph");
        assert_eq!(definitions[0]["children"][0]["children"][1]["type"], "bold");
    }

    #[test]
    fn verse_exposes_inline_objects_and_line_breaks_alongside_raw_value() {
        let ast = json("#+begin_verse\nA /soft/ line\nand more\n#+end_verse\n");
        let mut verses = Vec::new();
        nodes_of_type(&ast, "verse-block", &mut verses);
        assert_eq!(verses[0]["value"], "A /soft/ line\nand more\n");
        assert_eq!(verses[0]["affiliated"], serde_json::json!([]));
        assert!(
            verses[0]["children"]
                .as_array()
                .unwrap()
                .iter()
                .any(|child| child["type"] == "italic")
        );
        assert!(
            verses[0]["children"]
                .as_array()
                .unwrap()
                .iter()
                .any(|child| child["text"].as_str().is_some_and(|s| s.contains('\n')))
        );
    }

    #[test]
    fn timestamp_cookies_and_same_day_time_range_are_structured() {
        let ast = json(
            "<2026-10-05 Mon +1w -3d> <2026-10-06 Tue .+2d --2d> <2026-10-07 Wed ++1m> <2026-10-03 Sat 14:00-15:30>\n",
        );
        let mut timestamps = Vec::new();
        nodes_of_type(&ast, "timestamp", &mut timestamps);
        assert_eq!(timestamps.len(), 4);
        assert_eq!(timestamps[0]["repeater"], "+1w");
        assert_eq!(timestamps[0]["warning"], "-3d");
        assert_eq!(timestamps[1]["repeater"], ".+2d");
        assert_eq!(timestamps[1]["warning"], "--2d");
        assert_eq!(timestamps[2]["repeater"], "++1m");
        assert_eq!(timestamps[2]["warning"], Value::Null);
        assert_eq!(timestamps[3]["repeater"], Value::Null);
        assert_eq!(timestamps[3]["warning"], Value::Null);
        assert_eq!(timestamps[3]["start"]["hour"], "14");
        assert_eq!(timestamps[3]["end"]["hour"], "15");
        assert_eq!(timestamps[3]["end"]["minute"], "30");
    }

    #[test]
    fn literal_block_values_are_unescaped_per_org_spec() {
        // Org makes stripping the escaping comma the PARSER's job:
        // "parser removes all the commas in `,*` and `,#+` lines"
        // (org-syntax.html § Blocks).  A client reading `value`
        // must therefore see what the author wrote, not the
        // concrete syntax that protects it from the block fence.
        let ast = json(indoc! {"
            #+begin_example
            ,#+end_example
            ,* not a heading
            plain line
            #+end_example
            #+begin_src rust
            ,* also not a heading
            fn main() {}
            #+end_src
        "});

        let mut examples = Vec::new();
        nodes_of_type(&ast, "example-block", &mut examples);
        assert_eq!(examples.len(), 1);
        assert_eq!(
            examples[0]["value"],
            "#+end_example\n* not a heading\nplain line\n"
        );

        // `SrcBlock` already routes through orgize's own
        // `SourceBlock::value()`, so it was never affected; assert
        // it anyway to pin both paths to one contract.
        let mut sources = Vec::new();
        nodes_of_type(&ast, "src-block", &mut sources);
        assert_eq!(sources[0]["value"], "* also not a heading\nfn main() {}\n");
    }

    #[test]
    fn unescaping_a_block_value_never_eats_ordinary_leading_commas() {
        // The escape is only ever a comma run FOLLOWED by `*` or
        // `#+`.  A line that merely begins with a comma is ordinary
        // text and must survive byte-exact, or a diagnostic like
        // `,serde: expected value` would silently lose a character.
        let ast = json(indoc! {"
            #+begin_example
            ,serde: expected value
            ,,* double comma
            #+end_example
        "});

        let mut examples = Vec::new();
        nodes_of_type(&ast, "example-block", &mut examples);
        // orgize strips a comma only from a `,*` / `,#+` run, and
        // (unlike Emacs) leaves the doubled `,,*` form untouched.
        assert_eq!(
            examples[0]["value"],
            ",serde: expected value\n,,* double comma\n"
        );
    }

    #[test]
    fn unrepresented_orgize_constructs_fall_back_to_exact_raw_nodes() {
        let input = indoc! {"
            #+CALL: named()
            #+BEGIN: clocktable

            contents
            #+END:
            Paragraph with call_square(4) and @@html:<b>@@.
        "};
        let ast = json(input);
        let mut raw = Vec::new();
        nodes_of_type(&ast, "raw", &mut raw);
        assert!(!raw.is_empty());
        let recovered = raw
            .iter()
            .filter_map(|node| node["text"].as_str())
            .collect::<String>();
        // `#+CALL:` is a BABEL_CALL, `call_square(4)` an INLINE_CALL and
        // `@@html:<b>@@` a SNIPPET — none of which we model semantically,
        // so each must survive verbatim as a `raw` node.
        assert!(recovered.contains("#+CALL: named()"));
        assert!(recovered.contains("call_square(4)"));
        assert!(recovered.contains("@@html:<b>@@"));

        // NOTE: `#+BEGIN: clocktable` is deliberately NOT asserted here.
        // orgize parses a dynamic-block opener as an ordinary KEYWORD
        // (name `BEGIN`, value ` clocktable`) rather than a DYN_BLOCK, and
        // KEYWORD *is* modelled semantically — so it correctly appears as
        // a `keyword` node, never as `raw`.  Nothing is lost either way.
        let mut keywords = Vec::new();
        nodes_of_type(&ast, "keyword", &mut keywords);
        assert!(
            keywords.iter().any(|node| {
                node["key"].as_str() == Some("BEGIN")
                    && node["value"].as_str() == Some(" clocktable")
            }),
            "dynamic-block opener should surface as a keyword node: {keywords:?}"
        );
    }

    #[test]
    fn top_level_source_ranges_recover_every_input_byte() {
        let inputs = [
            "",
            "  \n\t\n",
            indoc! {"
                :PROPERTIES:
                :ID: αβγ
                :END:
                #+TITLE: Démo
                * TODO Héllo *世界*
                Paragraph [[https://example.com][link]].
            "},
            "#+begin_src rust\nfn main() { println!(\"é\"); }",
        ];

        for input in inputs {
            let ast = json(input);
            let recovered = ast["children"]
                .as_array()
                .expect("document children")
                .iter()
                .map(|node| {
                    let start = node["source"]["start"].as_u64().expect("source start") as usize;
                    let end = node["source"]["end"].as_u64().expect("source end") as usize;
                    input.get(start..end).expect("UTF-8-aligned source range")
                })
                .collect::<String>();
            assert_eq!(recovered, input);
        }
    }

    #[test]
    fn parse_ast_is_total_for_malformed_and_unusual_utf8_inputs() {
        let deep = (1..=64)
            .map(|level| format!("{} heading {level}\n", "*".repeat(level)))
            .collect::<String>();
        for input in [
            "".to_string(),
            " \t\r\n".to_string(),
            ":PROPERTIES:\n:ID: x\n".to_string(),
            "#+begin_src rust\nfn unfinished() {}".to_string(),
            "*".to_string(),
            deep,
            "🦀 weird\0UTF-8\u{2028}中文 é".to_string(),
        ] {
            let result = std::panic::catch_unwind(|| parse_ast(&input));
            assert!(result.is_ok(), "parse_ast panicked for {input:?}");
        }
    }

    /// A document in the shape real producers emit — a `:PROPERTIES:`
    /// drawer, `#+` keywords, and headlines — parses into all three.
    ///
    /// This replaces a `vigil-watch` test that read two absolute paths
    /// on the author's machine and `continue`d when they were absent.
    /// That test could not move into a library: it asserted nothing on
    /// any machine that did not happen to have those exact files, so it
    /// passed vacuously in CI and in every fresh checkout. The shape it
    /// meant to check is pinned here against checked-in input instead.
    #[test]
    fn a_producer_shaped_document_yields_drawers_keywords_and_headlines() {
        let value = json(indoc! {"
            :PROPERTIES:
            :ID: 01KYKTS6HADBMAG4VWQ6HVT4X1
            :END:
            #+TITLE: Memo

            * Rendered
            Body.
        "});

        assert_eq!(value["type"], "document");
        let mut types = BTreeSet::new();
        collect_types(&value, &mut types);
        for expected in ["property-drawer", "keyword", "headline"] {
            assert!(types.contains(expected), "missing {expected} in {types:?}");
        }
    }
}
