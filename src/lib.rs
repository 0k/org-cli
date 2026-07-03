//! org-cli: Query and edit org-mode files from the command line.

pub mod cli;
pub mod config;
pub mod index;
pub mod org;
pub mod watch;

/// The HTTP+SSE serving layer and the `daemon` subcommand. Compiled
/// only under the default `server` feature; `--no-default-features`
/// strips tokio/axum/kal-sse entirely (see `serve` module docs).
#[cfg(feature = "server")]
pub mod serve;
