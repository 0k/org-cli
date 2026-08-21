//! `syntax check` action — structural validity of org files.
//!
//! Walks the requested paths, scans every `.org` file with
//! [`crate::org::syntax::scan`], and reports invalid block, generic
//! drawer, and property-drawer structure.
//!
//! This module is the *orchestration* layer; what counts as a defect
//! lives in [`crate::org::syntax`].
//!
//! ## Why this exists next to `link check`
//!
//! An unterminated `:PROPERTIES:` drawer breaks no link, so it passes
//! [`crate::cli::action::link::check`] forever — while making every
//! property inside it vanish from the parse. A repository that stores
//! provenance in drawers can therefore gate on link integrity, stay
//! green, and still have machine-invisible metadata.

use std::borrow::Cow;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use colored::*;
use log::debug;
use serde::Serialize;

use crate::cli::action::walk::collect_org_files;
use crate::org::syntax::{SyntaxDefect, scan};

/// CLI options received from `SyntaxCommands::Check`.
#[derive(Debug, Clone)]
pub struct CheckOptions {
    pub paths: Vec<PathBuf>,
    pub nul: bool,
    pub json: bool,
    pub first: bool,
}

/// One defect record, format-agnostic: a [`SyntaxDefect`] plus the file
/// it was found in.
#[derive(Debug, Clone, Serialize)]
pub struct FileDefect {
    /// Path to the org file that contains the defect.
    pub source_file: PathBuf,
    pub line: usize,
    pub reason_tag: &'static str,
    pub reason_message: String,
    /// Construct name: `PROPERTIES`, `LOGBOOK`, `src`, `example`, …
    pub construct: String,
    /// The opening line, trimmed.
    pub raw: String,
    /// Line of the headline that bounded the search for a terminator,
    /// when the search did not run to end of file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounded_at: Option<usize>,
}

impl FileDefect {
    fn new(source_file: PathBuf, d: SyntaxDefect) -> Self {
        Self {
            source_file,
            line: d.line,
            reason_tag: d.kind.tag(),
            reason_message: d.message(),
            construct: d.construct.clone(),
            raw: d.raw.clone(),
            bounded_at: d.bounded_at,
        }
    }
}

/// Entry point invoked by the dispatcher. Returns Ok(0) on a clean run,
/// Ok(1) when defects were reported, Err on usage / I/O errors.
pub fn run(opts: CheckOptions) -> Result<i32, String> {
    let org_files = collect_org_files(&opts.paths)?;
    debug!("scanning {} org file(s)", org_files.len());

    let mut defect_count: usize = 0;
    let mut writer = StdoutWriter::new(&opts);

    for source_file in &org_files {
        let content = fs::read_to_string(source_file)
            .map_err(|e| format!("cannot read {}: {}", source_file.display(), e))?;

        for defect in scan(&content) {
            defect_count += 1;
            let record = FileDefect::new(source_file.clone(), defect);
            writer.emit(&record).map_err(|e| e.to_string())?;

            if opts.first {
                writer.flush().map_err(|e| e.to_string())?;
                return Ok(1);
            }
        }
    }

    writer.finish(defect_count).map_err(|e| e.to_string())?;
    Ok(if defect_count == 0 { 0 } else { 1 })
}

// ---------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------

/// Wraps stdout and selects the right serializer per CLI flags.
struct StdoutWriter<'a> {
    opts: &'a CheckOptions,
    stdout: io::BufWriter<io::Stdout>,
}

/// Escape the record delimiter out of textual NUL fields.
///
/// Filesystem paths cannot contain NUL and are emitted as raw OS bytes;
/// source lines can, because NUL is valid UTF-8. Represent it as the two
/// printable bytes `\\0` so one malformed line cannot desynchronize a
/// consumer splitting records on `\0\0`.
fn escape_nul(s: &str) -> Cow<'_, str> {
    if s.contains('\0') {
        Cow::Owned(s.replace('\0', "\\0"))
    } else {
        Cow::Borrowed(s)
    }
}

impl<'a> StdoutWriter<'a> {
    fn new(opts: &'a CheckOptions) -> Self {
        Self {
            opts,
            stdout: io::BufWriter::new(io::stdout()),
        }
    }

    fn emit(&mut self, d: &FileDefect) -> io::Result<()> {
        if self.opts.json {
            let line = serde_json::to_string(d).map_err(|e| io::Error::other(e.to_string()))?;
            writeln!(self.stdout, "{}", line)
        } else if self.opts.nul {
            // <source>\0<line>\0<reason_tag>\0<construct>\0<raw>
            //   \0<bounded_at>\0\0
            //
            // `bounded_at` is never empty (`-` when absent): a trailing
            // empty field would emit a second consecutive NUL and make
            // the `\0\0` record terminator ambiguous for a reader
            // splitting on it.
            self.stdout
                .write_all(d.source_file.as_os_str().as_encoded_bytes())?;
            write!(
                self.stdout,
                "\0{}\0{}\0{}\0{}\0{}\0\0",
                d.line,
                escape_nul(d.reason_tag),
                escape_nul(&d.construct),
                escape_nul(&d.raw),
                d.bounded_at
                    .map(|l| l.to_string())
                    .unwrap_or_else(|| "-".to_string()),
            )
        } else {
            // "<file>:<line>: <reason>"
            // "    <raw>"
            writeln!(
                self.stdout,
                "{}:{}: {} {}\n    {}",
                d.source_file.display().to_string().bright_blue(),
                d.line,
                d.reason_tag.bright_red(),
                d.reason_message.bright_yellow(),
                d.raw.dimmed(),
            )
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdout.flush()
    }

    fn finish(&mut self, defect_count: usize) -> io::Result<()> {
        if !self.opts.json && !self.opts.nul {
            if defect_count == 0 {
                writeln!(self.stdout, "{}", "All org syntax OK.".green())?;
            } else {
                writeln!(
                    self.stdout,
                    "\n{} {}",
                    defect_count.to_string().bright_red(),
                    "syntax defect(s) found".bright_red(),
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
    use std::path::Path;

    /// Tiny ad-hoc tempdir helper, mirroring `link::check`'s.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            p.push(format!(
                "org-cli-syntax-test-{}-{}-{}",
                tag,
                std::process::id(),
                nanos
            ));
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
        fs::write(p, s).unwrap();
    }

    fn opts_for(paths: Vec<PathBuf>) -> CheckOptions {
        CheckOptions {
            paths,
            nul: false,
            json: false,
            first: false,
        }
    }

    #[test]
    fn test_run_clean_tree_returns_zero() {
        let td = TempDir::new("clean");
        write(
            &td.path().join("ok.org"),
            "* H\n:PROPERTIES:\n:A: 1\n:END:\nbody\n",
        );
        assert_eq!(run(opts_for(vec![td.path().to_path_buf()])).unwrap(), 0);
    }

    #[test]
    fn test_run_detects_unterminated_drawer() {
        let td = TempDir::new("bad-drawer");
        write(
            &td.path().join("bad.org"),
            "* H\n:PROPERTIES:\n:A: 1\n* N\n",
        );
        assert_eq!(run(opts_for(vec![td.path().to_path_buf()])).unwrap(), 1);
    }

    #[test]
    fn test_run_detects_unterminated_block() {
        let td = TempDir::new("bad-block");
        write(&td.path().join("bad.org"), "* H\n#+begin_src sh\necho hi\n");
        assert_eq!(run(opts_for(vec![td.path().to_path_buf()])).unwrap(), 1);
    }

    #[test]
    fn test_run_rejects_unreadable_file_instead_of_claiming_clean() {
        // A checker cannot truthfully print "All org syntax OK" when
        // it could not inspect a selected file.
        let td = TempDir::new("unreadable");
        fs::write(td.path().join("binary.org"), [0xff, 0xfe, 0x00]).unwrap();
        let err = run(opts_for(vec![td.path().to_path_buf()])).unwrap_err();
        assert!(err.contains("binary.org"), "got: {err}");
        assert!(err.contains("UTF-8"), "got: {err}");
    }

    #[test]
    fn test_file_defect_carries_location_and_reason() {
        let d = SyntaxDefect {
            line: 7,
            kind: crate::org::syntax::DefectKind::UnterminatedDrawer,
            construct: "PROPERTIES".to_string(),
            raw: ":PROPERTIES:".to_string(),
            bounded_at: Some(12),
        };
        let f = FileDefect::new(PathBuf::from("/a/b.org"), d);
        assert_eq!(f.line, 7);
        assert_eq!(f.reason_tag, "unterminated-drawer");
        assert_eq!(f.bounded_at, Some(12));
        assert!(f.reason_message.contains("line 12"));
    }
}
