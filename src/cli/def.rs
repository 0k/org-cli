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
    /// Query the org *link graph*: validate links, find unreferenced
    /// files, or list what points at a file.
    ///
    /// A link defect is a property of the graph, not of one file:
    /// deleting `a.org` breaks links that live in *other* files. These
    /// actions therefore scan the whole corpus by default — narrowing
    /// the scope hides exactly the inbound breakage you want to catch.
    Link {
        #[command(subcommand)]
        action: LinkCommands,
    },

    /// Check the structural validity of org files themselves
    /// (blocks, drawers, and property metadata).
    Syntax {
        #[command(subcommand)]
        action: SyntaxCommands,
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

    /// Edit org-mode headings: change keyword/title/tags/properties/
    /// content, add a heading, delete a subtree, or move a subtree
    /// within or across files.
    ///
    /// Dry-run by default (prints a unified diff); pass `--write` to
    /// apply the change atomically. `-j` emits one NDJSON `EditRecord`
    /// per applied op, `-0` NUL-separated records.
    Heading {
        #[command(subcommand)]
        action: HeadingCommands,
    },

    /// Run the long-lived TODO index daemon: watch org files, keep the
    /// sqlite index in sync, and serve an HTTP snapshot + SSE push
    /// stream (`GET /api/todos`, `GET /sse/todos`), one on-demand
    /// heading body + AST read (`GET /api/heading`), and one versioned
    /// heading mutation (`PATCH /api/heading`).
    ///
    /// When PATHS is omitted, the agenda set is read from
    /// `~/.cache/emacs/org-collect/org-agenda-files.tmp` and the
    /// agenda file itself is watched for membership changes. Runs
    /// until interrupted (Ctrl-C).
    #[cfg(feature = "server")]
    Daemon {
        /// Files or directories to watch. When omitted, the agenda
        /// set is used.
        paths: Vec<std::path::PathBuf>,

        /// TCP address to bind. Overrides the config
        /// `todos: serve-addr:` and the built-in default
        /// (`127.0.0.1:7480`).
        #[arg(long = "addr", value_name = "ADDR")]
        addr: Option<String>,

        /// Path to the sqlite index. Overrides the config
        /// `todos: db-path:` and the built-in default
        /// (`~/.cache/org-cli/todos.sqlite`).
        #[arg(long = "db", value_name = "PATH")]
        db: Option<std::path::PathBuf>,

        /// Expose heading writes when ADDR is not loopback.
        ///
        /// This is an exposure guard, not authentication: a local reverse
        /// proxy or tunnel bypasses it. Real remote safety requires
        /// authentication in front of org-cli.
        #[arg(long)]
        allow_remote_writes: bool,
    },
}

/// `org-cli link <action>` sub-actions — query or preserve the link graph.
#[derive(Subcommand, Debug)]
pub enum LinkCommands {
    /// Move one file/directory, or apply a strict YAML batch plan, while
    /// rewriting every affected `[[file:...]]` target under ROOT.
    ///
    /// This is a dry-run by default: it prints a deterministic summary and
    /// unified diffs without changing the filesystem. Pass `--write` to move
    /// OLD to NEW (or every `--plan` mapping), atomically rewrite changed org
    /// files, and verify the resulting all-followable link graph. Choose
    /// exactly one mode: positional OLD NEW, or `--plan YAML`.
    ///
    /// `--write` requires a quiescent corpus and path tree. Cooperating
    /// `org-cli link move` writers serialize only when they use the same
    /// canonical ROOT; nested or overlapping different roots do not share a
    /// lock. The command cannot protect against an uncooperative actor changing
    /// paths between system calls.
    Move {
        /// Existing file or directory to move. Must be within ROOT.
        #[arg(value_name = "OLD")]
        old: Option<std::path::PathBuf>,

        /// New path. Must not exist, must be within ROOT, and its parent must
        /// already exist.
        #[arg(value_name = "NEW")]
        new: Option<std::path::PathBuf>,

        /// Strict YAML batch plan (`moves: [{from, to}]`). Paths are relative
        /// to canonical ROOT; absolute paths must remain within ROOT.
        #[arg(long, value_name = "YAML")]
        plan: Option<std::path::PathBuf>,

        /// Explicit corpus root scanned recursively for `*.org` link sources.
        #[arg(long, value_name = "ROOT")]
        root: std::path::PathBuf,

        /// Apply under the quiescent-corpus contract. Cooperating moves using
        /// the same canonical ROOT are serialized; otherwise print a dry-run.
        #[arg(long)]
        write: bool,
    },

    /// Check that every `[[file:...]]` link in the given org files
    /// resolves to an existing path (and heading anchor, if any).
    ///
    /// Scans the whole corpus by default. Narrowing PATHS to the files
    /// you just changed will *miss* breakage: deleting a file breaks
    /// the links pointing at it, and those live elsewhere.
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
    /// (the inverse of `link check` / `link orphans`: here "no
    /// backlinks" is the notable result, useful for asserting a file
    /// *is* referenced before deleting it).
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
}

/// `org-cli syntax <action>` sub-actions — per-file structure.
#[derive(Subcommand, Debug)]
pub enum SyntaxCommands {
    /// Check block and drawer structure: delimiters balance,
    /// `:PROPERTIES:` drawers occupy their privileged heading position
    /// and contain only node properties, and generic drawers contain
    /// neither headlines nor nested drawers.
    ///
    /// A defect here is local to one file, so PATHS can safely be
    /// narrowed to just the files a commit touches — unlike
    /// `link check`, which needs the whole corpus.
    ///
    /// This matters because an unterminated drawer breaks no link: it
    /// silently removes every property inside it from the parse, so a
    /// link-only gate stays green while the metadata becomes
    /// machine-invisible.
    Check {
        /// Files or directories to scan. Directories are walked
        /// recursively; only `*.org` files are inspected.
        #[arg(default_value = ".")]
        paths: Vec<std::path::PathBuf>,

        /// NUL-separated record output (for safe shell piping).
        #[arg(short = '0', conflicts_with = "json")]
        nul: bool,

        /// NDJSON output (one JSON object per defect).
        #[arg(short = 'j', long)]
        json: bool,

        /// Stop at the first defect.
        #[arg(short = '1', long)]
        first: bool,
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

        /// Re-read and hash every file instead of trusting size+mtime.
        #[arg(long)]
        verify: bool,

        /// Bypass the cache: read and parse every file from scratch.
        #[arg(long = "no-cache")]
        no_cache: bool,

        /// Print a phase-by-phase timing breakdown on stderr.
        #[arg(long)]
        timing: bool,
    },

    /// Report pairs of CLOCK lines that cover the same instant.
    ///
    /// Unlike ``clock check``, whose rules each apply to a single
    /// line, this compares every clean CLOCK entry against every
    /// other one across the whole corpus — you cannot be doing two
    /// things at the same time, so any overlap is a bookkeeping
    /// error somewhere.
    ///
    /// Intervals are half-open: ``10:00-11:00`` and ``11:00-12:00``
    /// are adjacent, NOT overlapping, since that is exactly what
    /// clocking out of one task and into the next produces.
    ///
    /// Lines that fail ``clock check`` are skipped with a warning
    /// (a line whose own timestamps are broken cannot be placed on
    /// a timeline). Run ``clock check`` first for a clean corpus.
    ///
    /// Files matching ``clock.overlap.exclude-globs`` in the config
    /// contribute no intervals — that is where CLOCK lines which
    /// record evidence rather than work (activity segments,
    /// presence windows) are declared. ``--exclude`` adds to that
    /// list rather than replacing it.
    Overlap {
        /// Files or directories to scan. When omitted, the agenda
        /// set is read from
        /// `~/.cache/emacs/org-collect/org-agenda-files.tmp`.
        paths: Vec<std::path::PathBuf>,

        /// Glob patterns matched against candidate file paths;
        /// matching files are skipped. Repeatable. Added to the
        /// configured `clock.overlap.exclude-globs`.
        #[arg(long = "exclude", value_name = "GLOB")]
        exclude_globs: Vec<String>,

        /// NUL-separated record output (for safe shell piping).
        #[arg(short = '0', conflicts_with = "json")]
        nul: bool,

        /// NDJSON output (one JSON object per overlapping pair).
        #[arg(short = 'j', long)]
        json: bool,

        /// Stop at the first overlapping pair.
        #[arg(short = '1', long)]
        first: bool,

        /// Re-read and hash every file instead of trusting size+mtime.
        /// Slower, but immune to edits that preserve metadata.
        #[arg(long)]
        verify: bool,

        /// Bypass the cache: read and parse every file from scratch.
        #[arg(long = "no-cache")]
        no_cache: bool,

        /// Print a phase-by-phase timing breakdown on stderr.
        #[arg(long)]
        timing: bool,
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

        /// Re-read and hash every file instead of trusting size+mtime.
        #[arg(long)]
        verify: bool,

        /// Bypass the cache: read and parse every file from scratch.
        #[arg(long = "no-cache")]
        no_cache: bool,

        /// Print a phase-by-phase timing breakdown on stderr.
        #[arg(long)]
        timing: bool,
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

/// `org-cli heading <action>` sub-actions.
#[derive(Subcommand, Debug)]
pub enum HeadingCommands {
    /// Edit fields of one existing heading, addressed by PATH or
    /// `--line N`. Field edits are applied in a fixed order (keyword,
    /// priority, title, tags, properties, content). At least one edit
    /// flag is required.
    Set {
        /// Target org file.
        file: std::path::PathBuf,

        /// Heading path (`A/B/C`; `\/` escapes a literal slash in a
        /// title). Mutually exclusive with `--line`.
        #[arg(value_name = "PATH", conflicts_with = "line")]
        path: Option<String>,

        /// Target the heading on this 1-based line number instead of
        /// a path.
        #[arg(long, value_name = "N")]
        line: Option<usize>,

        /// Set the TODO keyword (validated against the file's keyword
        /// set). Mutually exclusive with `--no-keyword`.
        #[arg(long, value_name = "KW", conflicts_with = "no_keyword")]
        keyword: Option<String>,

        /// Remove the TODO keyword.
        #[arg(long)]
        no_keyword: bool,

        /// Set priority A, B, or C. `none` removes the cookie and means
        /// "not yet evaluated", never "low priority".
        #[arg(long, value_name = "P", value_parser = ["A", "B", "C", "none"])]
        priority: Option<String>,

        /// Replace the title text.
        #[arg(long, value_name = "T")]
        title: Option<String>,

        /// Add a tag (merged with existing tags). Repeatable.
        /// Mutually exclusive with `--set-tags`.
        #[arg(long = "add-tag", value_name = "T", conflicts_with = "set_tags")]
        add_tag: Vec<String>,

        /// Remove a tag (from existing tags). Repeatable. Mutually
        /// exclusive with `--set-tags`.
        #[arg(long = "remove-tag", value_name = "T", conflicts_with = "set_tags")]
        remove_tag: Vec<String>,

        /// Replace ALL tags with this comma-separated list. Pass an
        /// empty string to remove all tags. Mutually exclusive with
        /// `--add-tag`/`--remove-tag`.
        #[arg(long = "set-tags", value_name = "T,T")]
        set_tags: Option<String>,

        /// Set a property `KEY=VALUE`. Repeatable.
        #[arg(long = "property", value_name = "K=V")]
        property: Vec<String>,

        /// Remove a property by key. Repeatable.
        #[arg(long = "unset-property", value_name = "K")]
        unset_property: Vec<String>,

        /// Replace the section body with STR (`-` reads stdin).
        /// Mutually exclusive with `--content-file`.
        #[arg(long, value_name = "STR", conflicts_with = "content_file")]
        content: Option<String>,

        /// Replace the section body with the contents of a file.
        #[arg(long = "content-file", value_name = "F")]
        content_file: Option<std::path::PathBuf>,

        /// Apply the edit in place (atomic write). Without this flag
        /// the command is a dry-run and prints a unified diff.
        #[arg(long)]
        write: bool,

        /// NDJSON output (one `EditRecord` per applied op).
        #[arg(short = 'j', long, conflicts_with = "nul")]
        json: bool,

        /// NUL-separated record output.
        #[arg(short = '0', long = "nul", conflicts_with = "json")]
        nul: bool,
    },

    /// Add a new heading under an optional parent PATH at a chosen
    /// position among its direct children.
    Add {
        /// Target org file.
        file: std::path::PathBuf,

        /// Parent heading path. When omitted, the heading is added at
        /// the top level.
        #[arg(long = "under", value_name = "PATH")]
        under: Option<String>,

        /// Position among the parent's direct children:
        /// `first`, `last`, `before=N`, or `after=N` (1-based).
        /// Defaults to `last`.
        #[arg(long, value_name = "POS", default_value = "last")]
        position: String,

        /// TODO keyword for the new heading.
        #[arg(long, value_name = "KW")]
        keyword: Option<String>,

        /// Title of the new heading (required).
        #[arg(long, value_name = "T")]
        title: String,

        /// Tag for the new heading. Repeatable.
        #[arg(long = "tag", value_name = "T")]
        tag: Vec<String>,

        /// Property `KEY=VALUE` for the new heading. Repeatable.
        #[arg(long = "property", value_name = "K=V")]
        property: Vec<String>,

        /// Section body for the new heading (`-` reads stdin).
        /// Mutually exclusive with `--content-file`.
        #[arg(long, value_name = "STR", conflicts_with = "content_file")]
        content: Option<String>,

        /// Section body from a file.
        #[arg(long = "content-file", value_name = "F")]
        content_file: Option<std::path::PathBuf>,

        /// Apply the edit in place (atomic write). Without this flag
        /// the command is a dry-run and prints a unified diff.
        #[arg(long)]
        write: bool,

        /// NDJSON output.
        #[arg(short = 'j', long, conflicts_with = "nul")]
        json: bool,

        /// NUL-separated record output.
        #[arg(short = '0', long = "nul", conflicts_with = "json")]
        nul: bool,
    },

    /// Delete a heading and its full subtree, addressed by PATH or
    /// `--line N`.
    Delete {
        /// Target org file.
        file: std::path::PathBuf,

        /// Heading path. Mutually exclusive with `--line`.
        #[arg(value_name = "PATH", conflicts_with = "line")]
        path: Option<String>,

        /// Target the heading on this 1-based line number.
        #[arg(long, value_name = "N")]
        line: Option<usize>,

        /// Apply the edit in place (atomic write). Without this flag
        /// the command is a dry-run and prints a unified diff.
        #[arg(long)]
        write: bool,

        /// NDJSON output.
        #[arg(short = 'j', long, conflicts_with = "nul")]
        json: bool,

        /// NUL-separated record output.
        #[arg(short = '0', long = "nul", conflicts_with = "json")]
        nul: bool,
    },

    /// Move a heading (and its subtree) within the file or to another
    /// file, re-leveling to the destination depth.
    Move {
        /// Source org file.
        file: std::path::PathBuf,

        /// Source heading path. Mutually exclusive with `--line`.
        #[arg(value_name = "PATH", conflicts_with = "line")]
        path: Option<String>,

        /// Source heading 1-based line number.
        #[arg(long, value_name = "N")]
        line: Option<usize>,

        /// Destination file. Defaults to the source file.
        #[arg(long = "to-file", value_name = "FILE2")]
        to_file: Option<std::path::PathBuf>,

        /// Destination parent path (resolved in the destination file).
        /// Mutually exclusive with `--top-level`.
        #[arg(long = "under", value_name = "PATH2", conflicts_with = "top_level")]
        under: Option<String>,

        /// Move to the destination file's top level.
        #[arg(long = "top-level")]
        top_level: bool,

        /// Position among the destination parent's direct children:
        /// `first`, `last`, `before=N`, or `after=N`. Defaults to
        /// `last`.
        #[arg(long, value_name = "POS", default_value = "last")]
        position: String,

        /// Apply the edit in place (atomic write). Without this flag
        /// the command is a dry-run and prints a unified diff.
        #[arg(long)]
        write: bool,

        /// NDJSON output.
        #[arg(short = 'j', long, conflicts_with = "nul")]
        json: bool,

        /// NUL-separated record output.
        #[arg(short = '0', long = "nul", conflicts_with = "json")]
        nul: bool,
    },
}
