//! CLI argument definitions using clap.

use clap::{Parser, Subcommand};

/// Query and edit org-mode files from the command line.
#[derive(Parser, Debug)]
#[command(name = "org-cli")]
#[command(version, about, long_about = None)]
pub struct Args {
    /// Turn general verbose logging
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Configure component wise logging
    #[arg(long, short = 'l', action = clap::ArgAction::Append)]
    pub log: Option<Vec<String>>,

    /// prepend time to each log line
    #[arg(long)]
    pub log_time: bool,

    /// Subcommand to execute
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Available commands
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Check that every `[[file:...]]` link in the given org files
    /// resolves to an existing path (and heading anchor, if any).
    Check {
        /// Files or directories to scan. Directories are walked
        /// recursively; only `*.org` files are inspected.
        #[arg(default_value = ".")]
        paths: Vec<std::path::PathBuf>,

        /// NUL-separated record output (for safe shell piping).
        #[arg(short = '0', conflicts_with = "json")]
        nul: bool,

        /// NDJSON output (one JSON object per broken link).
        #[arg(short = 'j', long)]
        json: bool,

        /// Stop at the first broken link.
        #[arg(short = '1', long)]
        first: bool,

        /// Treat warnings (e.g. unresolved `::N` line anchors) as errors.
        #[arg(long)]
        strict: bool,

        /// Require exact heading match for `[[file:...::*Heading]]`
        /// anchors. By default, heading anchors match any headline
        /// whose title contains the anchor text as a substring, which
        /// mirrors Emacs' org-mode default link-following behavior.
        #[arg(long)]
        strict_headings: bool,
    },
}
