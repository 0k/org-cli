//! Cross-check the orgize-driven projection against the line scanner
//! on the user's real corpus.
//!
//! Ignored by default: it depends on the local agenda set. Run with
//! `cargo test --test clock_project_corpus -- --ignored --nocapture`.
//!
//! The two must agree on every CLOCK line that is NOT inside a
//! literal block; the projection must find strictly fewer only where
//! the scanner was reading documentation as data.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use org_cli::org::clock::parse_clock_line;
use org_cli::org::clock_project::project;

fn agenda_files() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap();
    let list = fs::read_to_string(format!(
        "{home}/.cache/emacs/org-collect/org-agenda-files.tmp"
    ))
    .expect("agenda set present");
    list.lines()
        .filter(|l| !l.trim().is_empty())
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect()
}

#[test]
#[ignore]
fn projection_agrees_with_scanner_outside_literal_blocks() {
    let mut scanner_only: Vec<(PathBuf, usize)> = Vec::new();
    let mut projection_only: Vec<(PathBuf, usize)> = Vec::new();
    let mut scan_total = 0usize;
    let mut proj_total = 0usize;

    for path in agenda_files() {
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let scan: BTreeSet<usize> = content
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains("CLOCK:"))
            .filter(|(_, l)| matches!(parse_clock_line(l), Ok(Some(p)) if p.violations.is_empty()))
            .map(|(i, _)| i + 1)
            .collect();
        let proj: BTreeSet<usize> = project(&path.to_string_lossy(), &content)
            .blob
            .records
            .iter()
            .map(|r| r.line as usize)
            .collect();
        scan_total += scan.len();
        proj_total += proj.len();
        for l in scan.difference(&proj) {
            scanner_only.push((path.clone(), *l));
        }
        for l in proj.difference(&scan) {
            projection_only.push((path.clone(), *l));
        }
    }

    eprintln!("scanner facts    : {scan_total}");
    eprintln!("projection facts : {proj_total}");
    eprintln!("scanner-only     : {}", scanner_only.len());
    for (p, l) in scanner_only.iter().take(40) {
        eprintln!("   {}:{}", p.display(), l);
    }
    eprintln!("projection-only  : {}", projection_only.len());
    for (p, l) in projection_only.iter().take(20) {
        eprintln!("   {}:{}", p.display(), l);
    }

    // The projection must never invent a fact the scanner cannot see.
    assert!(
        projection_only.is_empty(),
        "projection found CLOCKs the scanner did not: {:?}",
        &projection_only[..projection_only.len().min(10)]
    );
}
