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
//! Parsing is delegated wholesale to [`crate::org::snapshot::FileSnapshot`],
//! the single boundary that derives the file ETag, effective TODO keywords,
//! and headings from one content value. This module never reimplements that
//! derivation.

mod store;

pub use store::{TodoEvent, TodoQuery, TodoRecord, TodoStore};
