//! `backlinks` subcommand — targeted reverse link-graph query.
//!
//! Where [`crate::cli::action::check`] asks "do all my links point
//! at real files?" (forward validity) and
//! [`crate::cli::action::orphans`] asks "are *all* my files pointed
//! at by *some* link?" (aggregate set-difference), `backlinks` asks
//! the **targeted** reverse question: "which org files link to *this
//! one* file?".
//!
//! It is the third member of the link-graph family and shares the
//! exact same edge-extraction/resolution core
//! ([`crate::org::link::extract_file_links`] +
//! [`crate::org::link::resolve_target`]) used by the other two — no
//! new link logic is introduced. Instead of validating each edge or
//! computing a set difference, it filters edges by a single resolved
//! target and reports the sources.
//!
//! ## Canonicalization
//!
//! Both the query target and every link target are run through the
//! same best-effort canonicalization (via
//! [`crate::org::link::resolve_target`] and `fs::canonicalize`), so a
//! link written `[[file:../target.org]]` from a nested directory
//! matches a target given as `target.org` from the root. This is the
//! property a naive `grep` cannot provide.
//!
//! ## v1 scope
//!
//! Only `[[file:...]]` links are considered (the same subset
//! [`crate::org::link::extract_file_links`] produces). "Content"
//! references — `#+INCLUDE:`, `id:` links, raw text search — are out
//! of scope for v1.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use colored::*;
use log::{debug, warn};
use serde::Serialize;

use crate::cli::action::walk::collect_org_files;
use crate::org::link::{extract_file_links, resolve_target};

/// CLI options received from `Commands::Backlinks`.
#[derive(Debug, Clone)]
pub struct BacklinksOptions {
    /// The file to find backlinks for. Resolved (canonicalized) the
    /// same way as link targets so relative/`..` paths match.
    pub target: PathBuf,
    /// Link-graph scope: org files to scan for `[[file:...]]` links.
    pub paths: Vec<PathBuf>,
    pub nul: bool,
    pub json: bool,
    pub first: bool,
}

/// One backlink record, format-agnostic.
#[derive(Debug, Clone, Serialize)]
pub struct Backlink {
    /// Absolute path to the org file that contains the link.
    pub source_file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub raw: String,
    /// Resolved (absolute) path of the queried target — identical
    /// for every record in a run, echoed for self-describing output.
    pub target: PathBuf,
}

/// Entry point invoked by the dispatcher. Returns `Ok(0)` when at
/// least one backlink was found, `Ok(1)` when none were found, `Err`
/// on usage / I/O errors.
///
/// The exit-code convention is deliberately the inverse of `check` /
/// `orphans`: here "no backlinks" is the notable empty result worth
/// signalling with a non-zero status (e.g. for scripts asserting a
/// file *is* referenced before deleting it).
pub fn run(opts: BacklinksOptions) -> Result<i32, String> {
    if !opts.target.exists() {
        return Err(format!("target not found: {}", opts.target.display()));
    }
    // Canonicalize the query target once. Fall back to a lexical
    // absolute path so we can still compare meaningfully if
    // canonicalization fails for some exotic reason.
    let target = canonicalize_or_join(&opts.target);
    debug!("resolved backlink target: {}", target.display());

    let org_files = collect_org_files(&opts.paths)?;
    debug!("scanning {} org file(s) for links", org_files.len());

    let mut writer = StdoutWriter::new(&opts);
    let mut backlink_count: usize = 0;

    for source_file in &org_files {
        let content = match fs::read_to_string(source_file) {
            Ok(c) => c,
            Err(e) => {
                warn!("cannot read {}: {}", source_file.display(), e);
                continue;
            }
        };

        let source_dir = source_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        for link in extract_file_links(&content) {
            let resolved = resolve_target(&source_dir, &link).path;
            if resolved != target {
                continue;
            }
            backlink_count += 1;
            let record = Backlink {
                source_file: source_file.clone(),
                line: link.line,
                column: link.column,
                raw: link.raw.clone(),
                target: target.clone(),
            };
            writer.emit(&record).map_err(|e| e.to_string())?;
            if opts.first {
                writer.flush().map_err(|e| e.to_string())?;
                return Ok(0);
            }
        }
    }

    writer.finish(backlink_count).map_err(|e| e.to_string())?;
    Ok(if backlink_count == 0 { 1 } else { 0 })
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

/// Canonicalize when possible; fall back to a lexical absolute path.
/// We need the same shape on both sides of the comparison as
/// [`crate::org::link::resolve_target`] produces for link targets.
fn canonicalize_or_join(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(p))
                .unwrap_or_else(|_| p.to_path_buf())
        }
    })
}

// ---------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------

/// Wraps stdout and selects the right serializer per CLI flags.
/// Mirrors the equivalent in `check.rs` / `orphans.rs`; the eventual
/// `output/` module will absorb all three.
struct StdoutWriter<'a> {
    opts: &'a BacklinksOptions,
    stdout: io::BufWriter<io::Stdout>,
}

impl<'a> StdoutWriter<'a> {
    fn new(opts: &'a BacklinksOptions) -> Self {
        Self {
            opts,
            stdout: io::BufWriter::new(io::stdout()),
        }
    }

    fn emit(&mut self, b: &Backlink) -> io::Result<()> {
        if self.opts.json {
            let line = serde_json::to_string(b).map_err(|e| io::Error::other(e.to_string()))?;
            writeln!(self.stdout, "{}", line)
        } else if self.opts.nul {
            // <source>\0<line>\0<column>\0<raw>\0\0
            write!(
                self.stdout,
                "{}\0{}\0{}\0{}\0\0",
                b.source_file.display(),
                b.line,
                b.column,
                b.raw,
            )
        } else {
            // Human: "<file>:<line>:<col> -> <raw>"
            writeln!(
                self.stdout,
                "{}:{}:{} {} {}",
                b.source_file.display().to_string().bright_blue(),
                b.line,
                b.column,
                "->".dimmed(),
                b.raw.dimmed(),
            )
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdout.flush()
    }

    fn finish(&mut self, backlink_count: usize) -> io::Result<()> {
        if !self.opts.json && !self.opts.nul {
            if backlink_count == 0 {
                writeln!(self.stdout, "{}", "No backlinks found.".bright_red())?;
            } else {
                writeln!(
                    self.stdout,
                    "\n{} {}",
                    backlink_count.to_string().green(),
                    "backlink(s) found".green(),
                )?;
            }
        }
        self.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same tempdir helper pattern used elsewhere in the crate.
    /// Local to keep the module self-contained.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            p.push(format!("org-cli-backlinks-test-{}-{}-{}", tag, pid, nanos));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(p: &Path, s: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, s).unwrap();
    }

    fn opts_for(target: PathBuf, paths: Vec<PathBuf>) -> BacklinksOptions {
        BacklinksOptions {
            target,
            paths,
            nul: false,
            json: false,
            first: false,
        }
    }

    #[test]
    fn test_run_errors_on_missing_target() {
        let td = TempDir::new("missing-target");
        let bogus = td.path().join("nope.org");
        let err = run(opts_for(bogus, vec![td.path().to_path_buf()])).unwrap_err();
        assert!(err.contains("target not found"), "got: {}", err);
    }

    #[test]
    fn test_run_no_backlinks_returns_one() {
        let td = TempDir::new("no-backlinks");
        write(&td.path().join("target.org"), "* H\n");
        write(&td.path().join("other.org"), "[[file:other.org][self]]\n");
        let exit = run(opts_for(
            td.path().join("target.org"),
            vec![td.path().to_path_buf()],
        ))
        .unwrap();
        assert_eq!(exit, 1, "nobody links target.org → exit 1");
    }

    #[test]
    fn test_run_detects_direct_backlink() {
        let td = TempDir::new("direct");
        write(&td.path().join("target.org"), "* H\n");
        write(&td.path().join("source.org"), "[[file:target.org][t]]\n");
        let exit = run(opts_for(
            td.path().join("target.org"),
            vec![td.path().to_path_buf()],
        ))
        .unwrap();
        assert_eq!(exit, 0, "source.org links target.org → exit 0");
    }

    #[test]
    fn test_run_matches_relative_link_via_canonicalization() {
        // A nested source links the target with `../target.org`.
        // Canonicalization on both sides must make them equal.
        let td = TempDir::new("relative");
        write(&td.path().join("target.org"), "* H\n");
        write(
            &td.path().join("nested/source.org"),
            "[[file:../target.org::*H][t]]\n",
        );
        let exit = run(opts_for(
            td.path().join("target.org"),
            vec![td.path().to_path_buf()],
        ))
        .unwrap();
        assert_eq!(exit, 0, "relative ../target.org must match");
    }

    #[test]
    fn test_run_ignores_links_to_other_files() {
        let td = TempDir::new("other");
        write(&td.path().join("target.org"), "* H\n");
        write(&td.path().join("decoy.org"), "* H\n");
        write(&td.path().join("source.org"), "[[file:decoy.org][d]]\n");
        let exit = run(opts_for(
            td.path().join("target.org"),
            vec![td.path().to_path_buf()],
        ))
        .unwrap();
        assert_eq!(exit, 1, "only links to decoy.org → target has no backlinks");
    }

    #[test]
    fn test_run_first_flag_returns_zero_on_match() {
        let td = TempDir::new("first");
        write(&td.path().join("target.org"), "* H\n");
        write(&td.path().join("a.org"), "[[file:target.org][t]]\n");
        write(&td.path().join("b.org"), "[[file:target.org][t]]\n");
        let mut opts = opts_for(td.path().join("target.org"), vec![td.path().to_path_buf()]);
        opts.first = true;
        assert_eq!(run(opts).unwrap(), 0);
    }

    #[test]
    fn test_run_errors_on_missing_scan_path() {
        let td = TempDir::new("bad-scan");
        write(&td.path().join("target.org"), "* H\n");
        let bogus = td.path().join("nope");
        let err = run(opts_for(td.path().join("target.org"), vec![bogus])).unwrap_err();
        assert!(err.contains("path not found"), "got: {}", err);
    }
}
