//! Subcommand implementations.
//!
//! Each subcommand owns its orchestration in a dedicated file. The
//! domain logic it composes lives in `crate::org`.
//!
//! Cross-subcommand utilities (filesystem traversal, …) live in
//! sibling helper modules like [`walk`].

pub mod backlinks;
pub mod check;
pub mod clock;
#[cfg(feature = "server")]
pub mod daemon;
pub mod heading;
pub mod headings;
pub mod orphans;
pub mod todos;
pub mod walk;
