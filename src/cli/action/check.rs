//! `check` subcommand — link integrity check for org files.
//!
//! Walks the requested paths (files and directories), parses every
//! `.org` file with [`crate::org::link::extract_file_links`], resolves
//! each `[[file:...]]` target relative to the containing org file's
//! directory, and reports broken links.
//!
//! This module is the *orchestration* layer. The link domain logic
//! (parsing, anchor classification, anchor index) lives in
//! [`crate::org::link`].

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use colored::*;
use log::{debug, warn};
use serde::Serialize;

use crate::cli::action::walk::collect_org_files;
use crate::org::link::{
    Anchor, AnchorIndex, BreakReason, Link, extract_file_links, resolve_target,
};

/// CLI options received from `Commands::Check`.
#[derive(Debug, Clone)]
pub struct CheckOptions {
    pub paths: Vec<PathBuf>,
    pub nul: bool,
    pub json: bool,
    pub first: bool,
    pub strict: bool,
    /// When true, `[[*Heading]]` anchors require an exact match
    /// against a headline's plain title. When false (default), the
    /// anchor is matched as a substring of the headline title,
    /// mirroring Emacs' org-mode default link-following behavior.
    pub strict_headings: bool,
}

/// One broken-link record, format-agnostic.
#[derive(Debug, Clone, Serialize)]
pub struct BrokenLink {
    /// Absolute path to the org file that contains the link.
    pub source_file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub raw: String,
    /// Resolved (absolute) path the link points at.
    pub resolved_target: PathBuf,
    pub reason_tag: &'static str,
    pub reason_message: String,
}

/// Entry point invoked by the dispatcher. Returns Ok(0) on clean run,
/// Ok(1) when broken links were reported, Err on usage / I/O errors.
pub fn run(opts: CheckOptions) -> Result<i32, String> {
    let org_files = collect_org_files(&opts.paths)?;
    debug!("scanning {} org file(s)", org_files.len());

    // Cache of AnchorIndex per resolved target file, so a target
    // pointed to from many links is parsed at most once.
    let mut anchor_cache: std::collections::HashMap<PathBuf, Option<AnchorIndex>> =
        std::collections::HashMap::new();

    let mut broken_count: usize = 0;
    let mut writer = StdoutWriter::new(&opts);

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

            let break_reason = match validate_link(
                &link,
                &resolved,
                &mut anchor_cache,
                opts.strict,
                opts.strict_headings,
            ) {
                Ok(()) => continue,
                Err(r) => r,
            };

            broken_count += 1;
            let record = BrokenLink {
                source_file: source_file.clone(),
                line: link.line,
                column: link.column,
                raw: link.raw.clone(),
                resolved_target: resolved,
                reason_tag: break_reason.tag(),
                reason_message: break_reason.message(),
            };
            writer.emit(&record).map_err(|e| e.to_string())?;

            if opts.first {
                writer.flush().map_err(|e| e.to_string())?;
                return Ok(1);
            }
        }
    }

    writer.finish(broken_count).map_err(|e| e.to_string())?;
    Ok(if broken_count == 0 { 0 } else { 1 })
}

/// Validate one link. Returns `Ok(())` if the link resolves cleanly.
fn validate_link(
    link: &Link,
    resolved: &Path,
    anchor_cache: &mut std::collections::HashMap<PathBuf, Option<AnchorIndex>>,
    strict: bool,
    strict_headings: bool,
) -> Result<(), BreakReason> {
    // Step 1: target path must exist on disk. A trailing-slash link
    // (directory) is fine as long as the directory exists.
    if !resolved.exists() {
        return Err(BreakReason::PathMissing);
    }

    // Step 2: anchor validation, only meaningful on files (not dirs).
    let Some(anchor) = &link.anchor else {
        return Ok(());
    };
    if !resolved.is_file() {
        // Anchors on directories don't make sense; treat as broken
        // only in strict mode (matches `--strict` documented behavior).
        return if strict {
            Err(BreakReason::HeadingNotFound {
                heading: format!("{:?}", anchor),
            })
        } else {
            Ok(())
        };
    }

    let is_org = resolved
        .extension()
        .map(|e| e.eq_ignore_ascii_case("org"))
        .unwrap_or(false);

    match anchor {
        Anchor::Heading(h) => {
            if !is_org {
                // Heading anchor on non-org file: report only in strict.
                return if strict {
                    Err(BreakReason::HeadingNotFound { heading: h.clone() })
                } else {
                    Ok(())
                };
            }
            let idx = ensure_index(resolved, anchor_cache)?;
            let found = if strict_headings {
                idx.has_heading_exact(h)
            } else {
                idx.has_heading_substring(h)
            };
            if found {
                Ok(())
            } else {
                Err(BreakReason::HeadingNotFound { heading: h.clone() })
            }
        }
        Anchor::CustomId(id) => {
            if !is_org {
                return if strict {
                    Err(BreakReason::CustomIdNotFound { id: id.clone() })
                } else {
                    Ok(())
                };
            }
            let idx = ensure_index(resolved, anchor_cache)?;
            if idx.has_custom_id(id) {
                Ok(())
            } else {
                Err(BreakReason::CustomIdNotFound { id: id.clone() })
            }
        }
        Anchor::LineNumber(n) => {
            // For org files we know line_count from the index. For
            // non-org files we count newlines on demand. In strict
            // mode, line anchors on non-org files are flagged.
            if is_org {
                let idx = ensure_index(resolved, anchor_cache)?;
                if *n >= 1 && *n <= idx.line_count {
                    Ok(())
                } else {
                    Err(BreakReason::LineOutOfRange {
                        asked: *n,
                        max: idx.line_count,
                    })
                }
            } else if strict {
                Err(BreakReason::LineAnchorOnNonOrg { asked: *n })
            } else {
                Ok(())
            }
        }
        Anchor::Other(_) => Ok(()), // unsupported anchor kind, not an error
    }
}

/// Get (and cache) the AnchorIndex for a target file. Read errors
/// surface as `Err(BreakReason::PathMissing)` since from the link's
/// point of view the target can't be inspected.
fn ensure_index<'a>(
    target: &Path,
    cache: &'a mut std::collections::HashMap<PathBuf, Option<AnchorIndex>>,
) -> Result<&'a AnchorIndex, BreakReason> {
    let key = target.to_path_buf();
    let entry = cache
        .entry(key)
        .or_insert_with(|| match fs::read_to_string(target) {
            Ok(c) => Some(AnchorIndex::from_org_content(&c)),
            Err(e) => {
                warn!("cannot read target {}: {}", target.display(), e);
                None
            }
        });
    entry.as_ref().ok_or(BreakReason::PathMissing)
}

// ---------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------

/// Wraps stdout and selects the right serializer per CLI flags.
/// Inline here on purpose — Plan 03 will lift the `output/` module
/// for `headings`, and `check` will be retrofitted at that point.
struct StdoutWriter<'a> {
    opts: &'a CheckOptions,
    stdout: io::BufWriter<io::Stdout>,
}

impl<'a> StdoutWriter<'a> {
    fn new(opts: &'a CheckOptions) -> Self {
        Self {
            opts,
            stdout: io::BufWriter::new(io::stdout()),
        }
    }

    fn emit(&mut self, b: &BrokenLink) -> io::Result<()> {
        if self.opts.json {
            let line = serde_json::to_string(b).map_err(|e| io::Error::other(e.to_string()))?;
            writeln!(self.stdout, "{}", line)
        } else if self.opts.nul {
            // <source>\0<line>\0<column>\0<reason_tag>\0<resolved>\0<raw>\0\0
            write!(
                self.stdout,
                "{}\0{}\0{}\0{}\0{}\0{}\0\0",
                b.source_file.display(),
                b.line,
                b.column,
                b.reason_tag,
                b.resolved_target.display(),
                b.raw,
            )
        } else {
            // Human format: "<file>:<line>:<col>: <reason> -> <target>\n    <raw>"
            writeln!(
                self.stdout,
                "{}:{}:{}: {} {} {}\n    {}",
                b.source_file.display().to_string().bright_blue(),
                b.line,
                b.column,
                "broken".bright_red(),
                "->".dimmed(),
                format!("{} ({})", b.resolved_target.display(), b.reason_message).bright_yellow(),
                b.raw.dimmed(),
            )
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdout.flush()
    }

    fn finish(&mut self, broken_count: usize) -> io::Result<()> {
        if !self.opts.json && !self.opts.nul {
            if broken_count == 0 {
                writeln!(self.stdout, "{}", "All links OK.".green())?;
            } else {
                writeln!(
                    self.stdout,
                    "\n{} {}",
                    broken_count.to_string().bright_red(),
                    "broken link(s) found".bright_red(),
                )?;
            }
        }
        self.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempdir_helper::TempDir;

    /// Tiny ad-hoc tempdir helper to avoid pulling in `tempfile`.
    /// Cleans up on drop.
    mod tempdir_helper {
        use std::fs;
        use std::path::PathBuf;
        pub struct TempDir(pub PathBuf);
        impl TempDir {
            pub fn new(tag: &str) -> Self {
                let mut p = std::env::temp_dir();
                let pid = std::process::id();
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0);
                p.push(format!("org-cli-test-{}-{}-{}", tag, pid, nanos));
                fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }

    fn write(p: &Path, s: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(p).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    // Note: `collect_org_files` and `resolve_path` are tested in
    // their new homes (`cli::action::walk` and `org::link`
    // respectively). This module no longer owns those tests.

    fn opts_for(paths: Vec<PathBuf>) -> CheckOptions {
        CheckOptions {
            paths,
            nul: false,
            json: false,
            first: false,
            strict: false,
            strict_headings: false,
        }
    }

    #[test]
    fn test_run_clean_archive_returns_zero() {
        let td = TempDir::new("clean");
        write(&td.path().join("target.org"), "* Heading\n");
        write(
            &td.path().join("source.org"),
            "See [[file:target.org::*Heading][T]]\n",
        );
        assert_eq!(run(opts_for(vec![td.path().to_path_buf()])).unwrap(), 0);
    }

    #[test]
    fn test_run_detects_missing_path() {
        let td = TempDir::new("missing");
        write(
            &td.path().join("source.org"),
            "[[file:does-not-exist.org][x]]\n",
        );
        assert_eq!(run(opts_for(vec![td.path().to_path_buf()])).unwrap(), 1);
    }

    #[test]
    fn test_run_detects_missing_heading() {
        let td = TempDir::new("missing-heading");
        write(&td.path().join("target.org"), "* Other Heading\n");
        write(
            &td.path().join("source.org"),
            "[[file:target.org::*Missing][m]]\n",
        );
        assert_eq!(run(opts_for(vec![td.path().to_path_buf()])).unwrap(), 1);
    }

    #[test]
    fn test_run_relative_link_resolves_against_source_dir() {
        let td = TempDir::new("relative");
        write(&td.path().join("topic/foo.org"), "* H\n");
        write(
            &td.path().join("visit/v.org"),
            "[[file:../topic/foo.org::*H][h]]\n",
        );
        assert_eq!(run(opts_for(vec![td.path().to_path_buf()])).unwrap(), 0);
    }

    #[test]
    fn test_run_directory_link_ok() {
        let td = TempDir::new("dir-link");
        fs::create_dir_all(td.path().join("subdir")).unwrap();
        write(
            &td.path().join("source.org"),
            "[[file:subdir/][a directory]]\n",
        );
        assert_eq!(run(opts_for(vec![td.path().to_path_buf()])).unwrap(), 0);
    }

    #[test]
    fn test_run_substring_heading_is_default_ok() {
        // Mirrors the real archive case: a timestamped heading like
        // "* [2026-04-06 Mon] Témoignage — X" is linked to with the
        // anchor "*Témoignage — X" (a substring). Default org-mode-
        // compatible behavior should accept it.
        let td = TempDir::new("substring-ok");
        write(
            &td.path().join("target.org"),
            "* [2026-04-06 Mon] Témoignage — antécédents\n",
        );
        write(
            &td.path().join("source.org"),
            "[[file:target.org::*Témoignage — antécédents][t]]\n",
        );
        assert_eq!(run(opts_for(vec![td.path().to_path_buf()])).unwrap(), 0);
    }

    #[test]
    fn test_run_strict_headings_flag_rejects_substring_match() {
        let td = TempDir::new("strict-reject");
        write(
            &td.path().join("target.org"),
            "* [2026-04-06 Mon] Témoignage — antécédents\n",
        );
        write(
            &td.path().join("source.org"),
            "[[file:target.org::*Témoignage — antécédents][t]]\n",
        );
        let mut opts = opts_for(vec![td.path().to_path_buf()]);
        opts.strict_headings = true;
        assert_eq!(run(opts).unwrap(), 1);
    }
}
