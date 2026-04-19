//! Org-mode parsing and manipulation.
//!
//! Domain types live here. Each type owns one well-defined org concept
//! (e.g. `Link`). The CLI layer (`crate::cli::action`) composes them.

pub mod link;
