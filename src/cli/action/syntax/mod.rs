//! `syntax` subcommand family — structural validity of one org file.
//!
//! A syntax defect is *local*: an unterminated `:PROPERTIES:` drawer in
//! `foo.org` is a defect of `foo.org` alone, judged without reading any
//! other file. Two consequences follow, and both are why this is a
//! separate family from [`crate::cli::action::link`]:
//!
//! - **It scopes to changed files.** A pre-commit hook can pass just the
//!   staged paths and get a complete answer, where the link graph would
//!   need the whole corpus to see inbound breakage.
//! - **It is cheap.** No parse tree and no cross-file resolution — a
//!   single line-oriented pass, unlike `link check`, which parses every
//!   source *and* every link target.

pub mod check;
