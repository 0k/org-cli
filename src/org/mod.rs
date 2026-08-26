//! Org-mode parsing and manipulation.
//!
//! Domain types live here. Each type owns one well-defined org concept
//! (e.g. `Link`). The CLI layer (`crate::cli::action`) composes them.

pub mod ast;
pub mod clock;
pub mod clock_add;
pub mod filter;
pub mod heading;
pub mod heading_edit;
pub mod link;
pub mod property;
pub mod snapshot;
pub mod syntax;
pub mod todo_keywords;
