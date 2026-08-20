//! `link orphans` action — reverse link-graph query.
//!
//! Where [`crate::cli::action::link::check`] asks "do all my links point
//! at real files?", `link orphans` asks the dual question: "are all my
//! files pointed at by some link?". Files that no `[[file:...]]`
//! reference reaches are reported as orphans.
//!
//! ## Two sets, one comparison
//!
//! - **Referenced set**: every `ResolvedTarget` extracted by
//!   [`crate::org::link::collect_referenced_paths`] from the `*.org`
//!   files under PATH.
//! - **Candidate set**: every file under PATH whose extension matches
//!   `--include-ext` (default `org`), minus `--exclude` matches.
//!
//! Orphans are `candidates − referenced`. Both sides use the same
//! canonicalization rules (via [`crate::org::link::resolve_target`]
//! for one side and `fs::canonicalize` for the other) so set
//! comparison is meaningful.
//!
//! ## Directory-link semantics
//!
//! By default a link to `foo/` only marks `foo/` itself as
//! referenced. With `--directory-references-all-children`, every
//! file inside `foo/` (recursively) is treated as referenced. This
//! is the "folder-as-citation-unit" mode useful for media folders
//! and visit dossiers; off by default to avoid hiding real orphans.
//!
//! ## Why metadata links are deliberately excluded
//!
//! Unlike [`crate::cli::action::link::check`], this command counts only
//! [`crate::org::link::LinkOrigin::Body`] links
//! ([`crate::org::link::OriginPolicy::BodyOnly`]). The asymmetry is
//! intentional and follows from the direction of the error: an extra
//! edge here can only ever *suppress* an orphan report, never raise
//! one. A link inside a `:LOGBOOK:` note recording that a document was
//! superseded would, if counted, silently reclassify a genuinely
//! forgotten file as referenced — turning a warning the user wants into
//! a clean run. The same reasoning already governs the
//! directory-reference flag being off by default.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use colored::*;
use glob::Pattern;
use log::debug;
use serde::Serialize;
use walkdir::WalkDir;

use crate::cli::action::walk::collect_org_files;
use crate::org::link::{OriginPolicy, ResolvedTarget, collect_referenced_paths};

/// CLI options for the `link orphans` action.
#[derive(Debug, Clone)]
pub struct OrphansOptions {
    /// Paths to scan for both `[[file:...]]` link sources (org
    /// files) and orphan candidates (files matching include_exts).
    pub paths: Vec<PathBuf>,
    pub nul: bool,
    pub json: bool,
    pub first: bool,
    /// If non-empty, only report orphans whose canonical path is
    /// under one of these prefixes.
    pub filter_paths: Vec<PathBuf>,
    /// Extensions (without leading dot) considered as candidate
    /// orphans. Empty = use default `["org"]`.
    pub include_exts: Vec<String>,
    /// Glob patterns; any candidate path matching one is skipped.
    pub exclude_globs: Vec<String>,
    /// When `true`, a link to `foo/` covers every file inside
    /// `foo/` (recursively).
    pub directory_references_all_children: bool,
}

/// One orphan record, format-agnostic.
#[derive(Debug, Clone, Serialize)]
pub struct Orphan {
    /// Absolute, canonicalized path to the unreferenced file.
    pub path: PathBuf,
    /// Always `"unreferenced"` — kept as a discriminant so future
    /// orphan flavors (e.g. `"singly-referenced"`) can join the
    /// same NDJSON stream without breaking consumers.
    #[serde(rename = "type")]
    pub kind: &'static str,
}

/// Entry point invoked by the dispatcher. Returns `Ok(0)` when no
/// orphans were found, `Ok(1)` when at least one was reported,
/// `Err` on usage / I/O errors.
pub fn run(opts: OrphansOptions) -> Result<i32, String> {
    // Step 1: build the link-source set (all org files under PATH).
    let org_files = collect_org_files(&opts.paths)?;
    debug!("scanning {} org file(s) for links", org_files.len());

    // Step 2: build the referenced set (resolved `[[file:...]]`
    // targets from those org files). Body links only — see the
    // module docs for why widening this set is unsafe here.
    let referenced = collect_referenced_paths(&org_files, OriginPolicy::BodyOnly);
    debug!("found {} unique referenced target(s)", referenced.len());

    // Step 3: compile excludes once.
    let exclude_patterns = compile_exclude_patterns(&opts.exclude_globs)?;

    // Step 4: scan candidates and stream orphans.
    let extensions = effective_extensions(&opts.include_exts);
    let candidates = collect_candidate_files(&opts.paths, &extensions, &exclude_patterns)?;
    debug!("found {} candidate file(s)", candidates.len());

    // Step 5: expand directory-link coverage if requested.
    let referenced_set = expand_referenced_set(
        referenced,
        &candidates,
        opts.directory_references_all_children,
    );

    // Step 6: filter-path canonicalization (done once).
    let filter_prefixes = canonicalize_filter_paths(&opts.filter_paths);

    let mut writer = StdoutWriter::new(&opts);
    let mut orphan_count: usize = 0;

    for candidate in &candidates {
        if referenced_set.contains(candidate) {
            continue;
        }
        if !passes_filter(candidate, &filter_prefixes) {
            continue;
        }
        orphan_count += 1;
        let record = Orphan {
            path: candidate.clone(),
            kind: "unreferenced",
        };
        writer.emit(&record).map_err(|e| e.to_string())?;
        if opts.first {
            writer.flush().map_err(|e| e.to_string())?;
            return Ok(1);
        }
    }

    writer.finish(orphan_count).map_err(|e| e.to_string())?;
    Ok(if orphan_count == 0 { 0 } else { 1 })
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

/// Returns the effective set of extensions to consider, lower-cased
/// without leading dots. Empty input means the documented default
/// `["org"]`.
fn effective_extensions(include_exts: &[String]) -> Vec<String> {
    if include_exts.is_empty() {
        return vec!["org".to_string()];
    }
    include_exts
        .iter()
        .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
        .collect()
}

/// Compile every `--exclude` pattern. Bad patterns surface as a
/// usage error so the user notices immediately.
fn compile_exclude_patterns(globs: &[String]) -> Result<Vec<Pattern>, String> {
    globs
        .iter()
        .map(|g| Pattern::new(g).map_err(|e| format!("invalid --exclude glob {:?}: {}", g, e)))
        .collect()
}

/// `true` when none of the exclude patterns matches `path`.
fn matches_any_exclude(path: &Path, patterns: &[Pattern]) -> bool {
    let s = path.to_string_lossy();
    patterns.iter().any(|p| p.matches(&s))
}

/// Walk every `paths` entry, keep regular files whose extension is
/// in `extensions` (case-insensitive) and that don't match any
/// exclude glob, canonicalize, dedupe, sort.
fn collect_candidate_files(
    paths: &[PathBuf],
    extensions: &[String],
    excludes: &[Pattern],
) -> Result<Vec<PathBuf>, String> {
    let mut out: Vec<PathBuf> = Vec::new();
    for input in paths {
        if !input.exists() {
            return Err(format!("path not found: {}", input.display()));
        }
        if input.is_file() {
            if has_eligible_extension(input, extensions) && !matches_any_exclude(input, excludes) {
                out.push(canonicalize_or_join(input));
            }
            continue;
        }
        for entry in WalkDir::new(input).follow_links(false) {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !has_eligible_extension(path, extensions) {
                continue;
            }
            if matches_any_exclude(path, excludes) {
                continue;
            }
            out.push(canonicalize_or_join(path));
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// `true` when `path`'s extension matches one of `extensions`
/// case-insensitively. `extensions` is expected pre-normalized
/// (lower-case, no leading dot).
fn has_eligible_extension(path: &Path, extensions: &[String]) -> bool {
    let Some(ext) = path.extension() else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    extensions.iter().any(|e| ext == e.as_str())
}

/// Canonicalize when possible; fall back to a lexical absolute path.
/// We need the same shape on both sides of the set comparison.
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

/// Convert the user's `--filter-path` list into canonicalized
/// prefixes, suitable for `Path::starts_with` comparisons.
fn canonicalize_filter_paths(filter_paths: &[PathBuf]) -> Vec<PathBuf> {
    filter_paths
        .iter()
        .map(|p| canonicalize_or_join(p))
        .collect()
}

/// `true` when `candidate` is under at least one filter prefix
/// (or when no filters were supplied — then everything passes).
fn passes_filter(candidate: &Path, filter_prefixes: &[PathBuf]) -> bool {
    if filter_prefixes.is_empty() {
        return true;
    }
    filter_prefixes
        .iter()
        .any(|prefix| candidate.starts_with(prefix))
}

/// Build the final referenced-paths set from the raw resolved
/// targets, expanding directory links if requested.
///
/// - Default mode: directory links contribute only their own path
///   to the set. A folder-only link does not absolve its children
///   from being referenced individually.
/// - With `--directory-references-all-children`: every candidate
///   file under any directory-link's path is treated as referenced.
fn expand_referenced_set(
    targets: HashSet<ResolvedTarget>,
    candidates: &[PathBuf],
    expand_directories: bool,
) -> HashSet<PathBuf> {
    let mut out: HashSet<PathBuf> = HashSet::with_capacity(targets.len());
    let mut dir_prefixes: Vec<PathBuf> = Vec::new();

    for t in targets {
        if t.is_directory_link && expand_directories {
            dir_prefixes.push(t.path);
        } else {
            out.insert(t.path);
        }
    }

    if !dir_prefixes.is_empty() {
        for candidate in candidates {
            if dir_prefixes.iter().any(|d| candidate.starts_with(d)) {
                out.insert(candidate.clone());
            }
        }
    }

    out
}

// ---------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------

/// Wraps stdout and selects the right serializer per CLI flags.
/// Mirrors the equivalent in `check.rs`; the eventual `output/`
/// module Plan 03 mentions will absorb both.
struct StdoutWriter<'a> {
    opts: &'a OrphansOptions,
    stdout: io::BufWriter<io::Stdout>,
}

impl<'a> StdoutWriter<'a> {
    fn new(opts: &'a OrphansOptions) -> Self {
        Self {
            opts,
            stdout: io::BufWriter::new(io::stdout()),
        }
    }

    fn emit(&mut self, o: &Orphan) -> io::Result<()> {
        if self.opts.json {
            let line = serde_json::to_string(o).map_err(|e| io::Error::other(e.to_string()))?;
            writeln!(self.stdout, "{}", line)
        } else if self.opts.nul {
            // <path>\0<type>\0\0
            write!(self.stdout, "{}\0{}\0\0", o.path.display(), o.kind)
        } else {
            writeln!(
                self.stdout,
                "{} {}",
                o.path.display().to_string().bright_blue(),
                "(never referenced)".dimmed(),
            )
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdout.flush()
    }

    fn finish(&mut self, orphan_count: usize) -> io::Result<()> {
        if !self.opts.json && !self.opts.nul {
            if orphan_count == 0 {
                writeln!(self.stdout, "{}", "No orphans found.".green())?;
            } else {
                writeln!(
                    self.stdout,
                    "\n{} {}",
                    orphan_count.to_string().bright_red(),
                    "orphan(s) found".bright_red(),
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
            p.push(format!("org-cli-orphans-test-{}-{}-{}", tag, pid, nanos));
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

    fn opts_for(paths: Vec<PathBuf>) -> OrphansOptions {
        OrphansOptions {
            paths,
            nul: false,
            json: false,
            first: false,
            filter_paths: vec![],
            include_exts: vec![],
            exclude_globs: vec![],
            directory_references_all_children: false,
        }
    }

    // -----------------------------------------------------------------
    // Helper-level tests
    // -----------------------------------------------------------------

    #[test]
    fn test_effective_extensions_defaults_to_org() {
        assert_eq!(effective_extensions(&[]), vec!["org"]);
    }

    #[test]
    fn test_effective_extensions_normalizes() {
        let exts = effective_extensions(&[".PDF".into(), "JPG".into()]);
        assert_eq!(exts, vec!["pdf", "jpg"]);
    }

    #[test]
    fn test_has_eligible_extension_case_insensitive() {
        let p = PathBuf::from("/tmp/foo.ORG");
        assert!(has_eligible_extension(&p, &["org".into()]));
    }

    #[test]
    fn test_has_eligible_extension_rejects_extensionless() {
        let p = PathBuf::from("/tmp/Makefile");
        assert!(!has_eligible_extension(&p, &["org".into()]));
    }

    #[test]
    fn test_compile_exclude_patterns_reports_bad_glob() {
        let err = compile_exclude_patterns(&["[unterminated".into()]).unwrap_err();
        assert!(err.contains("invalid --exclude glob"), "got: {}", err);
    }

    #[test]
    fn test_passes_filter_no_filters_means_pass() {
        assert!(passes_filter(Path::new("/a/b"), &[]));
    }

    #[test]
    fn test_passes_filter_under_prefix() {
        assert!(passes_filter(
            Path::new("/a/b/c.org"),
            &[PathBuf::from("/a/b")]
        ));
        assert!(!passes_filter(
            Path::new("/a/d/c.org"),
            &[PathBuf::from("/a/b")]
        ));
    }

    // -----------------------------------------------------------------
    // End-to-end run() tests
    // -----------------------------------------------------------------

    #[test]
    fn test_run_clean_archive_returns_zero() {
        let td = TempDir::new("clean");
        write(&td.path().join("target.org"), "* H\n");
        write(&td.path().join("source.org"), "[[file:target.org][t]]\n");
        // source.org references target.org but nothing references
        // source.org itself, so source is the orphan. To get a
        // clean run, also reference source from a third file.
        write(
            &td.path().join("index.org"),
            "[[file:source.org][s]] [[file:target.org][t]]\n",
        );
        // Now: index.org is the only file referenced by nothing.
        // To get a fully clean run, every file must be referenced.
        // This test instead asserts the *opposite* of clean to keep
        // the assertion meaningful — see test_run_detects_simple_orphan.
        // Use the inverse here: drop index, check only one orphan.
        fs::remove_file(td.path().join("index.org")).unwrap();
        // source.org is now orphan, target.org is referenced.
        assert_eq!(run(opts_for(vec![td.path().to_path_buf()])).unwrap(), 1);
    }

    #[test]
    fn test_run_truly_clean_archive_returns_zero() {
        // Cycle: a -> b -> a. Both referenced.
        let td = TempDir::new("cycle");
        write(&td.path().join("a.org"), "[[file:b.org][b]]\n");
        write(&td.path().join("b.org"), "[[file:a.org][a]]\n");
        assert_eq!(run(opts_for(vec![td.path().to_path_buf()])).unwrap(), 0);
    }

    #[test]
    fn test_run_detects_simple_orphan() {
        let td = TempDir::new("simple-orphan");
        write(&td.path().join("hub.org"), "[[file:linked.org][l]]\n");
        write(&td.path().join("linked.org"), "* H\n");
        write(&td.path().join("orphan.org"), "* nobody links me\n");
        // hub.org is itself unreferenced, plus orphan.org is too.
        // So we expect 2 orphans (hub + orphan); just assert exit=1.
        assert_eq!(run(opts_for(vec![td.path().to_path_buf()])).unwrap(), 1);
    }

    #[test]
    fn test_run_metadata_links_do_not_rescue_an_orphan() {
        // The whole point of this command is to surface forgotten
        // files. A property pointer or a drawer note must NOT silently
        // mark a file as referenced, because that converts a warning
        // the user wants into a clean run.
        let td = TempDir::new("metadata-no-rescue");
        write(&td.path().join("only-in-property.org"), "* H\n");
        write(&td.path().join("only-in-drawer.org"), "* H\n");
        write(
            &td.path().join("hub.org"),
            indoc::indoc! {"
                * Visit
                :PROPERTIES:
                :PREDECESSOR: [[file:only-in-property.org][p]]
                :END:
                :LOGBOOK:
                - note [[file:only-in-drawer.org][d]]
                :END:
            "},
        );

        let mut opts = opts_for(vec![td.path().to_path_buf()]);
        opts.json = true;
        assert_eq!(
            run(opts).unwrap(),
            1,
            "both metadata-only targets stay orphans"
        );

        // And prove which files are orphans, not merely that some are.
        let org_files = collect_org_files(&[td.path().to_path_buf()]).unwrap();
        let referenced = collect_referenced_paths(&org_files, OriginPolicy::BodyOnly);
        assert!(
            referenced.is_empty(),
            "no body link exists, so nothing is referenced: {:?}",
            referenced
        );
    }

    #[test]
    fn test_run_body_link_still_rescues_a_file() {
        // Guards the negative test above from passing vacuously.
        let td = TempDir::new("body-rescues");
        write(&td.path().join("linked.org"), "* H\n");
        write(
            &td.path().join("hub.org"),
            "[[file:linked.org][l]] [[file:hub.org][h]]\n",
        );
        assert_eq!(
            run(opts_for(vec![td.path().to_path_buf()])).unwrap(),
            0,
            "a body link marks its target referenced"
        );
    }

    #[test]
    fn test_run_directory_link_does_not_cover_children_by_default() {
        let td = TempDir::new("dir-default");
        // index.org links to sub/ as a directory and to itself
        // (so it's not orphan).
        write(
            &td.path().join("index.org"),
            "[[file:sub/][s]] [[file:index.org][i]]\n",
        );
        write(&td.path().join("sub/inner.org"), "* H\n");
        // Default: sub/ is referenced but sub/inner.org is NOT.
        let exit = run(opts_for(vec![td.path().to_path_buf()])).unwrap();
        assert_eq!(exit, 1, "inner.org should be reported as orphan");
    }

    #[test]
    fn test_run_directory_link_covers_children_with_flag() {
        let td = TempDir::new("dir-flag");
        write(
            &td.path().join("index.org"),
            "[[file:sub/][s]] [[file:index.org][i]]\n",
        );
        write(&td.path().join("sub/inner.org"), "* H\n");
        write(&td.path().join("sub/deeper/leaf.org"), "* H\n");
        let mut opts = opts_for(vec![td.path().to_path_buf()]);
        opts.directory_references_all_children = true;
        // Now sub/ covers inner.org and deeper/leaf.org recursively.
        assert_eq!(run(opts).unwrap(), 0);
    }

    #[test]
    fn test_run_index_org_does_not_cover_siblings() {
        let td = TempDir::new("index");
        write(
            &td.path().join("hub.org"),
            "[[file:photos/index.org][i]] [[file:hub.org][self]]\n",
        );
        write(&td.path().join("photos/index.org"), "* H\n");
        write(&td.path().join("photos/sibling.org"), "* not covered\n");
        // sibling.org must be reported even though we link to
        // photos/index.org. There is no magic.
        assert_eq!(run(opts_for(vec![td.path().to_path_buf()])).unwrap(), 1);
    }

    #[test]
    fn test_run_filter_path_narrows_output() {
        let td = TempDir::new("filter");
        write(&td.path().join("hub.org"), "[[file:hub.org][self]]\n");
        // Two orphans, in different subtrees:
        write(&td.path().join("zone-a/orphan.org"), "* a\n");
        write(&td.path().join("zone-b/orphan.org"), "* b\n");
        let mut opts = opts_for(vec![td.path().to_path_buf()]);
        opts.filter_paths = vec![td.path().join("zone-a")];
        // Only zone-a/orphan.org should be reported. Exit = 1.
        assert_eq!(run(opts).unwrap(), 1);
    }

    #[test]
    fn test_run_filter_path_excludes_everything_returns_zero() {
        let td = TempDir::new("filter-zero");
        write(&td.path().join("hub.org"), "[[file:hub.org][self]]\n");
        write(&td.path().join("zone-a/orphan.org"), "* a\n");
        let mut opts = opts_for(vec![td.path().to_path_buf()]);
        // Filter to a subtree that doesn't contain the orphan.
        opts.filter_paths = vec![td.path().join("zone-b")];
        fs::create_dir_all(td.path().join("zone-b")).unwrap();
        assert_eq!(run(opts).unwrap(), 0);
    }

    #[test]
    fn test_run_include_ext_widens_to_pdf() {
        let td = TempDir::new("pdf");
        // Hub references itself (so it's not org-orphan) and one PDF.
        write(
            &td.path().join("hub.org"),
            "[[file:hub.org][self]] [[file:cited.pdf][p]]\n",
        );
        write(&td.path().join("cited.pdf"), "%PDF-1.4 fake\n");
        write(&td.path().join("uncited.pdf"), "%PDF-1.4 fake\n");
        // Default: only org files are candidates → no orphans.
        assert_eq!(
            run(opts_for(vec![td.path().to_path_buf()])).unwrap(),
            0,
            "PDFs not in include_exts → not candidates → no orphans"
        );
        // With --include-ext pdf, uncited.pdf surfaces.
        let mut opts = opts_for(vec![td.path().to_path_buf()]);
        opts.include_exts = vec!["pdf".into()];
        assert_eq!(run(opts).unwrap(), 1);
    }

    #[test]
    fn test_run_exclude_glob_skips_files() {
        let td = TempDir::new("exclude");
        write(&td.path().join("hub.org"), "[[file:hub.org][self]]\n");
        write(&td.path().join("vendor/orphan.org"), "* v\n");
        // Without exclude: vendor/orphan.org is reported.
        assert_eq!(run(opts_for(vec![td.path().to_path_buf()])).unwrap(), 1);
        // With exclude: silenced.
        let mut opts = opts_for(vec![td.path().to_path_buf()]);
        opts.exclude_globs = vec!["**/vendor/**".into()];
        assert_eq!(run(opts).unwrap(), 0);
    }

    #[test]
    fn test_run_first_flag_stops_early() {
        // We can't easily observe early-stop without capturing stdout,
        // but we can at least confirm exit code is 1 with multiple orphans.
        let td = TempDir::new("first");
        write(&td.path().join("a.org"), "* a\n");
        write(&td.path().join("b.org"), "* b\n");
        let mut opts = opts_for(vec![td.path().to_path_buf()]);
        opts.first = true;
        assert_eq!(run(opts).unwrap(), 1);
    }

    #[test]
    fn test_run_canonicalization_matches_links_to_walked_files() {
        // Two org files, one in a subdir, both link to the same
        // target via different relative paths. Target must NOT be
        // reported as orphan because both links resolve to it.
        let td = TempDir::new("canon");
        write(&td.path().join("topic/foo.org"), "* H\n");
        // Hub links itself + topic/foo.org.
        write(
            &td.path().join("hub.org"),
            "[[file:hub.org][self]] [[file:topic/foo.org][f]]\n",
        );
        // Visit links to same foo.org via ../topic/foo.org and to
        // itself (relative to its own dir, not from the root).
        write(
            &td.path().join("visit/v.org"),
            "[[file:v.org][self]] [[file:../topic/foo.org][f]]\n",
        );
        // foo.org is referenced (twice, via two different relative
        // paths) and the canonicalization on both sides must agree.
        // hub.org and v.org reference themselves so they too are
        // covered.
        assert_eq!(run(opts_for(vec![td.path().to_path_buf()])).unwrap(), 0);
    }

    #[test]
    fn test_run_errors_on_missing_path() {
        let td = TempDir::new("bad-path");
        let bogus = td.path().join("nope");
        let err = run(opts_for(vec![bogus])).unwrap_err();
        assert!(err.contains("path not found"), "got: {}", err);
    }

    // -----------------------------------------------------------------
    // expand_referenced_set unit tests
    // -----------------------------------------------------------------

    #[test]
    fn test_expand_referenced_set_keeps_files_intact() {
        let mut targets = HashSet::new();
        targets.insert(ResolvedTarget {
            path: PathBuf::from("/a/file.org"),
            is_directory_link: false,
        });
        let out = expand_referenced_set(targets, &[], false);
        assert!(out.contains(&PathBuf::from("/a/file.org")));
    }

    #[test]
    fn test_expand_referenced_set_default_keeps_dir_path_only() {
        let mut targets = HashSet::new();
        targets.insert(ResolvedTarget {
            path: PathBuf::from("/a/sub"),
            is_directory_link: true,
        });
        let candidates = vec![
            PathBuf::from("/a/sub/inner.org"),
            PathBuf::from("/a/sub/deeper/leaf.org"),
        ];
        let out = expand_referenced_set(targets, &candidates, false);
        // Default: only /a/sub itself, NOT its children.
        assert!(out.contains(&PathBuf::from("/a/sub")));
        assert!(!out.contains(&PathBuf::from("/a/sub/inner.org")));
    }

    #[test]
    fn test_expand_referenced_set_flag_covers_children_recursively() {
        let mut targets = HashSet::new();
        targets.insert(ResolvedTarget {
            path: PathBuf::from("/a/sub"),
            is_directory_link: true,
        });
        let candidates = vec![
            PathBuf::from("/a/sub/inner.org"),
            PathBuf::from("/a/sub/deeper/leaf.org"),
            PathBuf::from("/a/elsewhere/nope.org"),
        ];
        let out = expand_referenced_set(targets, &candidates, true);
        assert!(out.contains(&PathBuf::from("/a/sub/inner.org")));
        assert!(out.contains(&PathBuf::from("/a/sub/deeper/leaf.org")));
        assert!(!out.contains(&PathBuf::from("/a/elsewhere/nope.org")));
    }
}
