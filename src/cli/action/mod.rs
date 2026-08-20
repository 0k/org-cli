//! Subcommand implementations.
//!
//! Each subcommand owns its orchestration in a dedicated file. The
//! domain logic it composes lives in `crate::org`.
//!
//! Commands that share one input and one domain module are grouped
//! into a family directory mirroring the CLI surface: [`link`] holds
//! the link-graph queries (`link check`, `link orphans`,
//! `link backlinks`), [`syntax`] the per-file structural checks.
//!
//! Cross-subcommand utilities (filesystem traversal, …) live in
//! sibling helper modules like [`walk`].

pub mod clock;
#[cfg(feature = "server")]
pub mod daemon;
pub mod heading;
pub mod headings;
pub mod link;
pub mod syntax;
pub mod todos;
pub mod walk;
