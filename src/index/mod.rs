//! SQLite-backed index of org-mode TODO headings.
//!
//! This is the pure-sync core of the `todos` feature: it owns the
//! persistence schema, the fingerprint-based incremental sync, and the
//! query surface. It knows nothing about async runtimes, HTTP, or
//! filesystem watching — those live in higher layers (Phase 2). The
//! only IO it performs is reading org files and talking to SQLite.
//!
//! The vocabulary type shared across layers is [`TodoEvent`]: every
//! [`TodoStore::sync`] returns the diff it applied as a `Vec<TodoEvent>`,
//! which the future daemon serialises onto an SSE stream verbatim.
//!
//! Parsing is delegated wholesale to [`crate::org`] primitives
//! ([`crate::org::todo_keywords::scan_buffer`] +
//! [`crate::org::heading::iter_headings_with`]); this module never
//! reimplements heading or keyword parsing.

mod store;

pub use store::{TodoEvent, TodoQuery, TodoRecord, TodoStore};
