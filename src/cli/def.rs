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

    /// Find every org file that links to TARGET via a
    /// `[[file:...]]` reference (reverse link-graph query /
    /// "backlinks").
    ///
    /// TARGET and every scanned link are canonicalized the same
    /// way, so a link written `[[file:../target.org]]` from a
    /// nested directory matches a TARGET given as `target.org` from
    /// the root — something a naive `grep` cannot do.
    ///
    /// Exits 0 when at least one backlink is found, 1 when none are
    /// (the inverse of `check`/`orphans`: here "no backlinks" is the
    /// notable result, useful for asserting a file *is* referenced
    /// before deleting it).
    ///
    /// v1 considers only `[[file:...]]` links; content references
    /// (`#+INCLUDE:`, `id:` links, raw text) are out of scope.
    Backlinks {
        /// The file to find backlinks for.
        target: std::path::PathBuf,

        /// Files or directories giving the link-graph scope (which
        /// org files to scan for links). Directories are walked
        /// recursively; only `*.org` files are inspected.
        #[arg(default_value = ".")]
        paths: Vec<std::path::PathBuf>,

        /// NUL-separated record output (for safe shell piping).
        #[arg(short = '0', conflicts_with = "json")]
        nul: bool,

        /// NDJSON output (one JSON object per backlink).
        #[arg(short = 'j', long)]
        json: bool,

        /// Stop at the first backlink found.
        #[arg(short = '1', long)]
        first: bool,
    },

    /// List headings across org files with optional title/property filters.
    Headings {
        /// Files or directories to scan. When omitted, the agenda
        /// set is read from
        /// `~/.cache/emacs/org-collect/org-agenda-files.tmp`.
        paths: Vec<std::path::PathBuf>,

        /// Glob patterns matched against candidate file paths;
        /// matching files are skipped. Repeatable. No defaults.
        #[arg(long = "exclude", value_name = "GLOB")]
        exclude: Vec<String>,

        /// Match heading title against regex.
        #[arg(short = 'f', long = "filter", value_name = "REGEX")]
        filter: Option<String>,

        /// Case-insensitive title regex.
        #[arg(short = 'i', long)]
        ignore_case: bool,

        /// Keep only headings that have the given property name.
        #[arg(long = "has-property", value_name = "NAME")]
        has_property: Vec<String>,

        /// Keep only headings whose TODO keyword matches exactly. Repeatable.
        #[arg(short = 'k', long = "keyword", value_name = "KEYWORD")]
        keyword: Vec<String>,

        /// Keep only headings that carry any TODO keyword.
        #[arg(long)]
        todo: bool,

        /// Space-separated TODO keyword spec applied to files without an
        /// in-buffer ``#+TODO:`` declaration, e.g.
        /// ``"TODO WIP | DONE CANCELED"``. Words before ``|`` are active
        /// states, after are done states; without ``|`` the last word is
        /// the done state.
        #[arg(long = "todo-keywords", value_name = "SPEC")]
        todo_keywords: Option<String>,

        /// NUL-separated record output (for safe shell piping).
        #[arg(short = '0', conflicts_with = "json")]
        nul: bool,

        /// NDJSON output (one JSON object per heading).
        #[arg(short = 'j', long, conflicts_with = "nul")]
        json: bool,
    },

    /// Sync org-mode TODO headings into a fast sqlite index, then
    /// list/query them.
    ///
    /// When PATHS is omitted, the agenda set is read from
    /// `~/.cache/emacs/org-collect/org-agenda-files.tmp`. The index
    /// is refreshed incrementally (only changed files are reparsed)
    /// before the query runs, unless `--no-sync` is given.
    Todos {
        /// Files or directories to scan. When omitted, the agenda
        /// set is used.
        paths: Vec<std::path::PathBuf>,

        /// Case-insensitive substring match against the heading title.
        #[arg(long = "search", value_name = "STR")]
        search: Option<String>,

        /// Keep only TODOs whose keyword matches exactly.
        #[arg(short = 'k', long = "keyword", value_name = "KW")]
        keyword: Option<String>,

        /// Skip the incremental sync; query the index as-is.
        #[arg(long = "no-sync")]
        no_sync: bool,

        /// Path to the sqlite index. Overrides the config
        /// `todos: db-path:` and the built-in default
        /// (`~/.cache/org-cli/todos.sqlite`).
        #[arg(long = "db", value_name = "PATH")]
        db: Option<std::path::PathBuf>,

        /// NUL-separated record output (for safe shell piping).
        #[arg(short = '0', long = "nul", conflicts_with = "json")]
        nul: bool,

        /// NDJSON output (one JSON object per TODO).
        #[arg(short = 'j', long, conflicts_with = "nul")]
        json: bool,
    },

    /// Lint and format org-mode `CLOCK:` lines.
    Clock {
        #[command(subcommand)]
        action: ClockCommands,
    },
}

/// `org-cli clock <action>` sub-actions.
#[derive(Subcommand, Debug)]
pub enum ClockCommands {
    /// Check that every closed-range CLOCK line follows the
    /// canonical format (numerical timezone, consistent precision,
    /// matching weekday, start ≤ end).
    Check {
        /// Files or directories to scan. When omitted, the agenda
        /// set is read from
        /// `~/.cache/emacs/org-collect/org-agenda-files.tmp`.
        paths: Vec<std::path::PathBuf>,

        /// Glob patterns matched against candidate file paths;
        /// matching files are skipped. Repeatable. No defaults.
        #[arg(long = "exclude", value_name = "GLOB")]
        exclude_globs: Vec<String>,

        /// NUL-separated record output (for safe shell piping).
        #[arg(short = '0', conflicts_with = "json")]
        nul: bool,

        /// NDJSON output (one JSON object per violation).
        #[arg(short = 'j', long)]
        json: bool,

        /// Stop at the first violation.
        #[arg(short = '1', long)]
        first: bool,
    },

    /// Apply mechanical fixes to CLOCK lines: rewrite the weekday
    /// token from the date (normalising French abbreviations like
    /// ``mer.`` to English ``Wed``), and recompute the
    /// ``=> H:MM`` duration suffix from the start/end timestamps
    /// using the canonical Emacs spacing rule.
    ///
    /// Only axes that can be derived from canonical inputs are
    /// touched. Timezones and the timestamps themselves are
    /// considered authored data and are NEVER modified.
    ///
    /// Defaults to dry-run; pass ``--write`` to apply changes.
    /// When PATHS is omitted, the agenda set is used (same as
    /// ``clock check``).
    Fix {
        paths: Vec<std::path::PathBuf>,

        /// Glob patterns matched against candidate file paths;
        /// matching files are skipped. Repeatable.
        #[arg(long = "exclude", value_name = "GLOB")]
        exclude_globs: Vec<String>,

        /// Apply the rewrites in place. Without this flag the
        /// command is a dry-run.
        #[arg(long)]
        write: bool,

        /// NDJSON output (one JSON object per rewrite).
        #[arg(short = 'j', long)]
        json: bool,
    },

    /// Emit one NDJSON record per closed-range CLOCK line that
    /// passes strict canonical validation. Lines that fail
    /// validation are reported on stderr (one diagnostic per
    /// violation), do NOT appear on stdout, and the command exits
    /// with code 1.
    ///
    /// Use this as the input to one-off scripts that need a
    /// trustworthy stream of (start, end, tz) triples — e.g.
    /// timezone-vs-location sanity checks against your own
    /// whereabout records.
    List {
        paths: Vec<std::path::PathBuf>,

        /// Glob patterns matched against candidate file paths;
        /// matching files are skipped. Repeatable.
        #[arg(long = "exclude", value_name = "GLOB")]
        exclude_globs: Vec<String>,

        /// Match enclosing heading title against regex.
        #[arg(short = 'f', long = "filter", value_name = "REGEX")]
        filter: Option<String>,

        /// Case-insensitive enclosing heading title regex.
        #[arg(short = 'i', long)]
        ignore_case: bool,

        /// Keep only CLOCKs whose enclosing heading has the given property name.
        #[arg(long = "has-property", value_name = "NAME")]
        has_property: Vec<String>,

        /// NDJSON output (currently the only supported shape).
        #[arg(short = 'j', long, default_value_t = true)]
        json: bool,
    },

    /// Find a heading in FILE and insert one canonical CLOCK
    /// line into its `:LOGBOOK:` drawer (creating the drawer
    /// if it doesn't exist yet).
    ///
    /// HEADING is digits-only → looked up as a `:TASK_REF:`
    /// property; otherwise as a case-insensitive substring of
    /// the heading title. Multiple substring matches surface as
    /// an ambiguity error.
    ///
    /// The CLOCK line is generated through the same canonical
    /// formatter as ``clock fmt``, so the inserted line is
    /// guaranteed to pass ``clock check``.
    Add {
        /// Target org file.
        file: std::path::PathBuf,

        /// Heading identifier (digits → TASK_REF; otherwise
        /// substring).
        heading: String,

        /// Start time. Same accepted forms as ``clock fmt``.
        start: String,

        /// End time. Same accepted forms as ``clock fmt``.
        end: String,

        /// Numerical zone like ``+0200`` or ``-0500``. When
        /// omitted, the system's current local zone is used.
        #[arg(long, value_name = "±HHMM")]
        tz: Option<String>,

        /// Emit `HH:MM:SS` precision instead of `HH:MM`.
        #[arg(long)]
        seconds: bool,

        /// Round each side to the nearest N-minute mark. 0
        /// (default) disables rounding. Mutually exclusive with
        /// --seconds.
        #[arg(long, default_value_t = 0)]
        grid: u32,

        /// Apply the rewrite in place. Without this flag the
        /// proposed full file content is printed on stdout.
        #[arg(long)]
        write: bool,

        /// NDJSON record describing the rewrite (file, line,
        /// edit_kind, clock_line, heading_line, applied).
        #[arg(short = 'j', long)]
        json: bool,
    },

    /// Print one canonical `CLOCK:` line on stdout.
    Fmt {
        /// Start time. Accepts `HH:MM`, `HH:MM:SS`,
        /// `YYYY-MM-DD HH:MM`, or `YYYY-MM-DD HH:MM:SS`.
        start: String,

        /// End time, same accepted forms as START.
        end: String,

        /// Numerical zone like `+0200` or `-0500`. When omitted,
        /// the system's current local zone is used.
        #[arg(long, value_name = "±HHMM")]
        tz: Option<String>,

        /// Emit `HH:MM:SS` precision instead of `HH:MM`.
        #[arg(long)]
        seconds: bool,

        /// Round each side to the nearest N-minute mark. 0 (default)
        /// disables rounding. Mutually exclusive with --seconds.
        #[arg(long, default_value_t = 0)]
        grid: u32,
    },
}
