//! Subcommand implementations.
//!
//! Each subcommand owns its orchestration in a dedicated file. The
//! domain logic it composes lives in `crate::org`.
//!
//! Cross-subcommand utilities (filesystem traversal, …) live in
//! sibling helper modules like [`walk`].

pub mod check;
pub mod walk;
