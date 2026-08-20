//! `link` subcommand family — queries over the org *link graph*.
//!
//! The three actions here share one input (every `[[file:...]]` link
//! reachable in the scanned corpus) and one domain module
//! ([`crate::org::link`]), but make deliberately different promises:
//!
//! | action      | question                                    |
//! |-------------|---------------------------------------------|
//! | [`check`]     | do all my links point at real files?      |
//! | [`orphans`]   | which files does nothing point at?        |
//! | [`backlinks`] | what points at *this* file?               |
//!
//! Those promises drive different link-origin policies — see
//! [`crate::org::link::OriginPolicy`].
//!
//! ## Why these are one family, and why `syntax` is not
//!
//! A link defect is a property of the *graph*, not of a file: deleting
//! `a.org` breaks links living in other files, so the damage appears
//! outside the change set and the scan must span the whole corpus. That
//! is the opposite of [`crate::cli::action::syntax`], whose defects are
//! local to one file and therefore scope cleanly to just the files a
//! commit touches.

pub mod backlinks;
pub mod check;
pub mod orphans;
