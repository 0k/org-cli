//! Filesystem walker shared by every subcommand whose input is a
//! mix of org files and directories.
//!
//! Centralizing this here prevents `link check`, `link orphans`, and any
//! future subcommand from each growing their own slightly-different
//! traversal. The traversal rules are:
//!
//! - A path passed explicitly is kept as-is, even if its extension
//!   isn't `.org` (lets users force-check arbitrary files).
//! - A path that is a directory is walked recursively; only entries
//!   whose extension is (case-insensitively) `org` are kept.
//! - Symlinks are not followed.
//! - Output is sorted and de-duplicated.

use std::fs;
use std::path::{Path, PathBuf};

use glob::Pattern;
use log::{debug, warn};
use walkdir::WalkDir;

/// Collect every `.org` file under the given input paths. Files
/// passed directly are kept as-is even if they don't end in `.org`.
///
/// Returns an error when an input path does not exist on disk
/// (caller-visible mistake) or when the walker fails to read a
/// directory entry (I/O error worth surfacing).
pub fn collect_org_files(inputs: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for input in inputs {
        if !input.exists() {
            return Err(format!("path not found: {}", input.display()));
        }
        if input.is_file() {
            out.push(input.clone());
            continue;
        }
        for entry in WalkDir::new(input).follow_links(false) {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("org"))
                .unwrap_or(false)
            {
                out.push(path.to_path_buf());
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Default location of the agenda-set file (the org-collect output).
pub fn default_agenda_set_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or("cannot determine home directory")?;
    Ok(home.join(".cache/emacs/org-collect/org-agenda-files.tmp"))
}

/// Read an agenda-set file: one absolute path per line, blank lines
/// and lines beginning with `#` are ignored.
pub fn read_agenda_set(p: &Path) -> Result<Vec<PathBuf>, String> {
    let content = fs::read_to_string(p)
        .map_err(|e| format!("cannot read agenda set {}: {}", p.display(), e))?;
    let mut out = Vec::new();
    for line in content.lines() {
        let s = line.trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        out.push(PathBuf::from(s));
    }
    Ok(out)
}

fn compile_globs(raw: &[String]) -> Result<Vec<Pattern>, String> {
    raw.iter()
        .map(|g| Pattern::new(g).map_err(|e| format!("invalid --exclude glob {:?}: {}", g, e)))
        .collect()
}

fn path_excluded(p: &Path, patterns: &[Pattern]) -> bool {
    let s = p.to_string_lossy();
    patterns.iter().any(|pat| pat.matches(&s))
}

/// Resolve the effective input file set for a subcommand.
///
/// When `paths` is empty, the agenda set is read from the default
/// org-collect output file; entries that no longer exist are skipped
/// with a warning, and an all-missing set is a hard error. Otherwise
/// the given `paths` are walked directly.
///
/// The resolved `.org` files are then filtered through the
/// `exclude_globs` patterns. Shared by `clock`, `headings`, `todos`,
/// and the future daemon so the agenda-fallback behaviour stays in
/// one place.
pub fn resolve_input_files(
    paths: &[PathBuf],
    exclude_globs: &[String],
) -> Result<Vec<PathBuf>, String> {
    let raw = if paths.is_empty() {
        let agenda = default_agenda_set_path()?;
        debug!("no PATHS given; using agenda set {}", agenda.display());
        let listed = read_agenda_set(&agenda)?;
        let inputs: Vec<PathBuf> = listed
            .into_iter()
            .filter(|p| {
                if !p.exists() {
                    warn!("agenda set entry not found, skipping: {}", p.display());
                    false
                } else {
                    true
                }
            })
            .collect();
        if inputs.is_empty() {
            return Err(format!(
                "agenda set at {} resolved to zero existing paths",
                agenda.display()
            ));
        }
        collect_org_files(&inputs)?
    } else {
        collect_org_files(paths)?
    };

    let patterns = compile_globs(exclude_globs)?;
    Ok(raw
        .into_iter()
        .filter(|p| !path_excluded(p, &patterns))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// Local tempdir helper; mirrors `check.rs::tests::tempdir_helper`.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            p.push(format!("org-cli-walk-test-{}-{}-{}", tag, pid, nanos));
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

    #[test]
    fn test_collect_org_files_walks_recursively() {
        let td = TempDir::new("collect");
        write(&td.path().join("a.org"), "");
        write(&td.path().join("nested/b.org"), "");
        write(&td.path().join("nested/c.txt"), "");

        let files = collect_org_files(&[td.path().to_path_buf()]).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["a.org", "b.org"]);
    }

    #[test]
    fn test_collect_org_files_keeps_explicit_files() {
        let td = TempDir::new("explicit");
        let f = td.path().join("notes.txt");
        write(&f, "");
        let files = collect_org_files(std::slice::from_ref(&f)).unwrap();
        assert_eq!(files, vec![f]);
    }

    #[test]
    fn test_collect_org_files_errors_on_missing_path() {
        let td = TempDir::new("missing");
        let bogus = td.path().join("nope.org");
        let err = collect_org_files(&[bogus]).unwrap_err();
        assert!(err.contains("path not found"), "got: {}", err);
    }

    #[test]
    fn test_collect_org_files_dedupes_and_sorts() {
        let td = TempDir::new("dedup");
        write(&td.path().join("z.org"), "");
        write(&td.path().join("a.org"), "");
        // Pass the directory twice to provoke duplicates.
        let files = collect_org_files(&[td.path().to_path_buf(), td.path().to_path_buf()]).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["a.org", "z.org"]);
    }

    // ----- agenda set loader --------------------------------------------

    #[test]
    fn read_agenda_set_skips_blank_and_comments() {
        let td = TempDir::new("agenda");
        let f = td.path().join("agenda.tmp");
        write(&f, "/tmp/one.org\n\n# a comment\n/tmp/two.org\n   \n");
        let v = read_agenda_set(&f).unwrap();
        assert_eq!(
            v,
            vec![PathBuf::from("/tmp/one.org"), PathBuf::from("/tmp/two.org")]
        );
    }

    #[test]
    fn read_agenda_set_errors_on_missing_file() {
        let td = TempDir::new("agenda-missing");
        let bogus = td.path().join("nope.tmp");
        let err = read_agenda_set(&bogus).unwrap_err();
        assert!(err.contains("cannot read"), "got: {}", err);
    }

    #[test]
    fn resolve_input_files_applies_exclude_globs() {
        let td = TempDir::new("resolve-exclude");
        write(&td.path().join("keep.org"), "");
        write(&td.path().join("skip.org"), "");
        let files = resolve_input_files(
            &[td.path().to_path_buf()],
            &[format!("{}/skip.org", td.path().display())],
        )
        .unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["keep.org"]);
    }
}
