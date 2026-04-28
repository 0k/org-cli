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

    /// Find files that are *not* the target of any `[[file:...]]`
    /// link anywhere in the scanned org graph (orphans /
    /// "forgotten by the graph").
    ///
    /// PATH... gives the link-graph scope (which org files to scan
    /// for links). The candidate set (which files to check for
    /// orphan-ness) defaults to the same paths but can be narrowed
    /// with --filter-path or widened to non-org files via
    /// --include-ext.
    Orphans {
        /// Files or directories to scan for `[[file:...]]` links
        /// AND, by default, to look for candidate files in. Walked
        /// recursively for `*.org` files (link sources) and for
        /// candidate files matching --include-ext.
        #[arg(default_value = ".")]
        paths: Vec<std::path::PathBuf>,

        /// NUL-separated record output (for safe shell piping).
        #[arg(short = '0', conflicts_with = "json")]
        nul: bool,

        /// NDJSON output (one JSON object per orphan).
        #[arg(short = 'j', long)]
        json: bool,

        /// Stop at the first orphan found.
        #[arg(short = '1', long)]
        first: bool,

        /// Only report orphans whose path is under one of these
        /// prefixes (after canonicalization). Repeatable. When
        /// omitted, every orphan found is reported.
        #[arg(long = "filter-path", value_name = "PATH")]
        filter_paths: Vec<std::path::PathBuf>,

        /// File extensions to consider as orphan candidates.
        /// Repeatable. Default: `org`. Pass e.g. `--include-ext pdf
        /// --include-ext jpg` to widen the candidate set.
        #[arg(long = "include-ext", value_name = "EXT")]
        include_exts: Vec<String>,

        /// Glob patterns to skip when scanning candidate files
        /// (e.g. `--exclude '**/.git/**'`). Repeatable.
        #[arg(long = "exclude", value_name = "GLOB")]
        exclude_globs: Vec<String>,

        /// When a link points to `foo/`, treat every file inside
        /// `foo/` (recursively) as referenced. Default off: only
        /// `foo/` itself is referenced.
        #[arg(long = "directory-references-all-children")]
        directory_references_all_children: bool,
    },
}
