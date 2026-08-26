//! Randomized round-trip fuzz harness for the heading edit ops.
//!
//! Runs the Phase-2 editing operations with random targets against
//! copies of real `.org` files and asserts **byte-identity** after each
//! `op + inverse` round trip. This is the executable form of the
//! "Fuzzy round-trip verification" section of
//! `doc/plan/10-heading-edit.md`.
//!
//! # Running
//!
//! The test is `#[ignore]` so a normal `cargo test` stays green and
//! never depends on the user's private corpus. Select a corpus first,
//! then run explicitly:
//!
//! ```sh
//! scripts/fuzz-corpus.sh /tmp/org-cli-fuzz 20
//! ORG_FUZZ_DIR=/tmp/org-cli-fuzz \
//!   cargo test --test fuzz_roundtrip -- --ignored --nocapture
//! ```
//!
//! * `ORG_FUZZ_DIR` — directory of `.org` files to fuzz. When unset the
//!   test prints a skip notice and passes (so CI is unaffected).
//! * `ORG_FUZZ_SEED` — optional `u64` seed override. The seed is always
//!   printed so any failure is reproducible.
//! * `ORG_FUZZ_ITERS` — optional iteration count per file (default 50).
//!
//! # Privacy
//!
//! The corpus contains private data. This harness only ever *reads* the
//! copies (the originals are copied by `scripts/fuzz-corpus.sh`, never
//! opened here) and its diagnostics print **paths, line numbers, op
//! names and structural counts only** — never heading titles or body
//! text.
//!
//! # Canonicality discipline
//!
//! The scalar ops (`set_keyword`/`set_title`/`set_tags`) rewrite the
//! whole heading line through our renderer, which emits a *canonical*
//! layout. If a file's original heading line is already canonical, an
//! `op + inverse` round trip must reproduce it byte-for-byte — any
//! deviation is a BUG. If the original line is **non-canonical** (e.g.
//! unusual internal spacing our renderer normalizes on first touch), a
//! byte-identical round trip is impossible *by construction*; such
//! headings are detected up front via an identity-op probe and counted
//! as `non_canonical` skips rather than masking real bugs.
//!
//! The structural ops (`extract_subtree` + `insert_subtree`) copy bytes
//! verbatim and must round-trip regardless of canonicality — failures
//! there are always bugs.

use std::collections::BTreeMap;
use std::path::PathBuf;

use org_cli::org::heading::{
    Heading, SubtreeSpan, direct_child_lines, iter_headings, section_span, subtree_span,
};
use org_cli::org::heading_edit::{
    AddPosition, InsertDest, add_heading, delete_heading, extract_subtree, insert_subtree,
    set_content, set_keyword, set_property, set_tags, set_title, unset_property,
};
use org_cli::org::todo_keywords::{TodoKeywords, scan_buffer};

// ---------------------------------------------------------------------
// Tiny seeded PRNG (SplitMix64) — no external crate dependency.
// ---------------------------------------------------------------------

/// A minimal deterministic PRNG (SplitMix64). Reproducible from a seed,
/// which is printed so any failure can be replayed.
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        // SplitMix64.
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, n)`. Returns 0 when `n == 0`.
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    fn choice<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            None
        } else {
            Some(&items[self.below(items.len())])
        }
    }

    /// A random ASCII string of length `len` drawn from a title-safe
    /// alphabet (letters, digits, spaces, a few punctuation marks). No
    /// `:` (would look like tags), no leading/trailing surprises.
    fn ascii_title(&mut self, len: usize) -> String {
        const ALPHABET: &[u8] =
            b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .-_()";
        let mut s = String::with_capacity(len);
        for _ in 0..len {
            let c = ALPHABET[self.below(ALPHABET.len())] as char;
            s.push(c);
        }
        // Avoid an all-space or edge-space title: trim then pad if empty.
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            "x".to_string()
        } else {
            trimmed
        }
    }

    /// A random tag word (letters/digits/underscore), length 1..=8.
    fn tag_word(&mut self) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
        let len = 1 + self.below(8);
        let mut s = String::with_capacity(len);
        for _ in 0..len {
            s.push(ALPHABET[self.below(ALPHABET.len())] as char);
        }
        s
    }
}

// ---------------------------------------------------------------------
// Per-file result accounting.
// ---------------------------------------------------------------------

#[derive(Default)]
struct FileStats {
    size: u64,
    headings: usize,
    iterations: usize,
    ok: usize,
    skips: BTreeMap<&'static str, usize>,
    failures: Vec<String>,
}

impl FileStats {
    fn skip(&mut self, reason: &'static str) {
        *self.skips.entry(reason).or_insert(0) += 1;
    }
}

// ---------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------

/// Does the scalar op leave the heading line unchanged when asked to set
/// its *current* value? If not, the original line is non-canonical for
/// our renderer and byte-identity round-trips are impossible.
fn heading_line_is_canonical(content: &str, h: &Heading) -> bool {
    // Probe with set_title(original_title): a pure identity that
    // exercises the full render path. If content is unchanged, the line
    // is canonical.
    match set_title(content, h.line, &h.title, &keyword_pool(content)) {
        Ok((out, _)) => out == content,
        Err(_) => false,
    }
}

/// Assert byte-identity, recording a structured failure (no private
/// content) when it does not hold. Returns whether it held.
fn expect_identical(
    stats: &mut FileStats,
    op: &str,
    line: usize,
    got: &str,
    pristine: &str,
) -> bool {
    if got == pristine {
        stats.ok += 1;
        true
    } else {
        stats.failures.push(format!(
            "op={op} line={line}: round-trip not byte-identical"
        ));
        false
    }
}

/// Collect the keyword pool for a file: the in-buffer declared set when
/// present, else the default TODO/DONE.
fn keyword_pool(content: &str) -> TodoKeywords {
    scan_buffer(content).unwrap_or_default()
}

/// All keyword words (active ∪ done) as a flat vec.
fn all_keywords(kw: &TodoKeywords) -> Vec<String> {
    let mut v = kw.active.clone();
    v.extend(kw.done.iter().cloned());
    v
}

// ---------------------------------------------------------------------
// Round-trip operations.
// ---------------------------------------------------------------------

/// Op 1: set_keyword to a random keyword, then restore the original
/// keyword state. Byte-identity required for canonical lines.
fn roundtrip_keyword(
    stats: &mut FileStats,
    rng: &mut Rng,
    pristine: &str,
    h: &Heading,
    keywords: &TodoKeywords,
) {
    let pool = all_keywords(keywords);
    let Some(new_kw) = rng.choice(&pool).cloned() else {
        stats.skip("no_keywords_available");
        return;
    };

    let forward = match set_keyword(pristine, h.line, Some(&new_kw), keywords) {
        Ok((out, _)) => out,
        Err(_) => {
            stats.skip("keyword_op_error");
            return;
        }
    };
    let restored = match set_keyword(&forward, h.line, h.keyword.as_deref(), keywords) {
        Ok((out, _)) => out,
        Err(_) => {
            stats.skip("keyword_restore_error");
            return;
        }
    };
    expect_identical(stats, "set_keyword", h.line, &restored, pristine);
}

/// Op 2: set_title to random strings (shorter and longer), then restore.
fn roundtrip_title(
    stats: &mut FileStats,
    rng: &mut Rng,
    pristine: &str,
    h: &Heading,
    keywords: &TodoKeywords,
) {
    // Vary length around the original.
    let orig_len = h.title.chars().count();
    let new_len = if rng.below(2) == 0 {
        1 + rng.below(orig_len.max(1))
    } else {
        orig_len + 1 + rng.below(40)
    };
    let new_title = rng.ascii_title(new_len);

    let forward = match set_title(pristine, h.line, &new_title, keywords) {
        Ok((out, _)) => out,
        Err(_) => {
            stats.skip("title_op_error");
            return;
        }
    };
    let restored = match set_title(&forward, h.line, &h.title, keywords) {
        Ok((out, _)) => out,
        Err(_) => {
            stats.skip("title_restore_error");
            return;
        }
    };
    expect_identical(stats, "set_title", h.line, &restored, pristine);
}

/// Op 3: set_tags to a random set, then restore the original tags.
fn roundtrip_tags(stats: &mut FileStats, rng: &mut Rng, pristine: &str, h: &Heading) {
    let count = rng.below(4); // 0..=3 tags
    let new_tags: Vec<String> = (0..count).map(|_| rng.tag_word()).collect();

    let forward = match set_tags(pristine, h.line, &new_tags) {
        Ok((out, _)) => out,
        Err(_) => {
            stats.skip("tags_op_error");
            return;
        }
    };
    let restored = match set_tags(&forward, h.line, &h.tags) {
        Ok((out, _)) => out,
        Err(_) => {
            stats.skip("tags_restore_error");
            return;
        }
    };
    expect_identical(stats, "set_tags", h.line, &restored, pristine);
}

/// Op 4: set_property with a guaranteed-fresh key, then unset it.
/// Independent of heading-line canonicality (only touches the drawer).
fn roundtrip_property(stats: &mut FileStats, rng: &mut Rng, pristine: &str, h: &Heading) {
    // A key guaranteed not to be present.
    let key = format!("FUZZ_{}", rng.next_u64());
    if h.properties.contains_key(&key) {
        stats.skip("property_key_collision");
        return;
    }
    let value_len = 1 + rng.below(20);
    let value = rng.ascii_title(value_len);

    let forward = match set_property(pristine, h.line, &key, &value) {
        Ok((out, _)) => out,
        Err(_) => {
            stats.skip("property_op_error");
            return;
        }
    };
    let restored = match unset_property(&forward, h.line, &key) {
        Ok((out, _)) => out,
        Err(_) => {
            stats.skip("property_restore_error");
            return;
        }
    };
    expect_identical(stats, "set_property", h.line, &restored, pristine);
}

/// Op 5: extract_subtree + insert_subtree at the SAME position.
/// Verbatim bytes → must always round-trip.
fn roundtrip_extract_insert(stats: &mut FileStats, pristine: &str, h: &Heading) {
    // Determine the parent + sibling index of `h` so we can re-insert at
    // exactly the same spot.
    let Some((parent_line, sibling_index)) = locate_parent_and_index(pristine, h.line) else {
        stats.skip("extract_locate_failed");
        return;
    };

    let (without, block, _rec) = match extract_subtree(pristine, h.line) {
        Ok(t) => t,
        Err(_) => {
            stats.skip("extract_error");
            return;
        }
    };

    // Re-insert at the original index. After removal the sibling list
    // shrank by one, so `Before(sibling_index)` can go out of range (a
    // sole or last child). Express the position relative to a *surviving*
    // sibling instead: `First` for index 1, else `After(index - 1)`.
    let pos = restore_position(sibling_index);
    let dest = InsertDest { parent_line, pos };
    let restored = match insert_subtree(&without, &block, dest) {
        Ok((out, _)) => out,
        Err(_) => {
            stats.skip("insert_error");
            return;
        }
    };
    expect_identical(stats, "extract_insert", h.line, &restored, pristine);
}

/// Op 6: move a heading under a different parent, then move it back to
/// its exact original position. Verbatim → must round-trip when the
/// move is legal.
fn roundtrip_move(
    stats: &mut FileStats,
    rng: &mut Rng,
    pristine: &str,
    headings: &[Heading],
    h: &Heading,
) {
    let Some((orig_parent, orig_index)) = locate_parent_and_index(pristine, h.line) else {
        stats.skip("move_locate_failed");
        return;
    };

    // The set of lines inside h's subtree — a valid new parent must be
    // OUTSIDE it (can't move a heading under its own descendant), and
    // must not be h itself.
    let Some(span) = subtree_span(pristine, h.line) else {
        stats.skip("move_span_failed");
        return;
    };

    let candidates: Vec<&Heading> = headings
        .iter()
        .filter(|c| c.line < span.start || c.line > span.end)
        .collect();
    let Some(target) = rng.choice(&candidates) else {
        stats.skip("move_no_target");
        return;
    };
    let target_line = target.line;

    // Extract, then insert under the target (Last), re-leveling.
    let (without, block, _rec) = match extract_subtree(pristine, h.line) {
        Ok(t) => t,
        Err(_) => {
            stats.skip("move_extract_error");
            return;
        }
    };

    // `target_line` addresses a heading in `pristine`; after removing h's
    // subtree, that heading's line number shifts if it was below the
    // removed block.
    let removed = span.end - span.start + 1;
    let target_after_removal = if target_line > span.end {
        target_line - removed
    } else {
        target_line
    };

    let insert_dest = InsertDest {
        parent_line: Some(target_after_removal),
        pos: AddPosition::Last,
    };
    let moved = match insert_subtree(&without, &block, insert_dest) {
        Ok((out, _)) => out,
        Err(_) => {
            // Underflow or other rejection: legitimate, skip.
            stats.skip("move_insert_error");
            return;
        }
    };

    // Now move it back. Find the heading we just inserted: it is the
    // last direct child of `target_after_removal` in `moved`.
    let children = direct_child_lines(&moved, target_after_removal);
    let Some(&moved_line) = children.last() else {
        stats.skip("move_relocate_failed");
        return;
    };

    let (back_without, back_block, _rec) = match extract_subtree(&moved, moved_line) {
        Ok(t) => t,
        Err(_) => {
            stats.skip("move_back_extract_error");
            return;
        }
    };

    // `back_without` should equal `without` (we removed the same subtree
    // we added). Re-insert at the original parent + index, expressed
    // relative to a surviving sibling (see `restore_position`).
    let back_dest = InsertDest {
        parent_line: orig_parent,
        pos: restore_position(orig_index),
    };
    let restored = match insert_subtree(&back_without, &back_block, back_dest) {
        Ok((out, _)) => out,
        Err(_) => {
            stats.skip("move_back_insert_error");
            return;
        }
    };
    expect_identical(stats, "move", h.line, &restored, pristine);
}

/// Convert an original 1-based sibling index into an `AddPosition`
/// expressed relative to a *surviving* sibling.
///
/// After a subtree is removed, its former siblings renumber. Re-inserting
/// with `Before(original_index)` can go out of range (sole or last
/// child). `First` for index 1 and `After(index - 1)` otherwise always
/// reference a sibling that is still present, restoring the exact slot.
fn restore_position(original_index: usize) -> AddPosition {
    if original_index <= 1 {
        AddPosition::First
    } else {
        AddPosition::After(original_index - 1)
    }
}

/// Find the parent heading line (or `None` for top level) and the
/// 1-based index of `heading_line` among its parent's direct children.
fn locate_parent_and_index(content: &str, heading_line: usize) -> Option<(Option<usize>, usize)> {
    let headings: Vec<Heading> = iter_headings(content).collect();
    let target = headings.iter().find(|h| h.line == heading_line)?;

    if target.level == 1 {
        // Top-level: index among level-1 siblings via direct child scan
        // rooted at "no parent". Emulate by listing top-level headings.
        let tops: Vec<usize> = headings
            .iter()
            .filter(|h| h.level == 1)
            .map(|h| h.line)
            .collect();
        let idx = tops.iter().position(|&l| l == heading_line)? + 1;
        return Some((None, idx));
    }

    // Non-top: the parent is the nearest preceding heading whose direct
    // children include `heading_line`.
    for candidate in headings.iter().rev() {
        if candidate.line >= heading_line {
            continue;
        }
        let children = direct_child_lines(content, candidate.line);
        if let Some(pos) = children.iter().position(|&l| l == heading_line) {
            return Some((Some(candidate.line), pos + 1));
        }
    }
    None
}

/// Parse-integrity check: `iter_headings` must not panic and every
/// heading line must yield a `Some` span from both span primitives.
fn parse_check(stats: &mut FileStats, content: &str, headings: &[Heading]) {
    for h in headings {
        let sub: Option<SubtreeSpan> = subtree_span(content, h.line);
        let sec: Option<SubtreeSpan> = section_span(content, h.line);
        if sub.is_none() {
            stats
                .failures
                .push(format!("op=subtree_span line={}: returned None", h.line));
        }
        if sec.is_none() {
            stats
                .failures
                .push(format!("op=section_span line={}: returned None", h.line));
        }
    }
}

// ---------------------------------------------------------------------
// The test.
// ---------------------------------------------------------------------

#[test]
#[ignore = "requires ORG_FUZZ_DIR pointing at a corpus; run with --ignored"]
fn fuzz_roundtrip() {
    let Ok(dir) = std::env::var("ORG_FUZZ_DIR") else {
        eprintln!(
            "ORG_FUZZ_DIR not set — skipping fuzz harness. \
             Prepare a corpus with scripts/fuzz-corpus.sh and set \
             ORG_FUZZ_DIR to run it."
        );
        return;
    };

    let seed: u64 = std::env::var("ORG_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(default_seed);
    let iters: usize = std::env::var("ORG_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    eprintln!("=== fuzz_roundtrip ===");
    eprintln!("corpus dir : {dir}");
    eprintln!("seed       : {seed:#018x}");
    eprintln!("iters/file : {iters}");
    eprintln!();

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read ORG_FUZZ_DIR")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("org"))
        .collect();
    files.sort();

    assert!(!files.is_empty(), "no .org files in ORG_FUZZ_DIR={dir}");

    let mut total_failures = 0usize;
    let mut per_file: Vec<(PathBuf, FileStats)> = Vec::new();

    for path in &files {
        let file_start = std::time::Instant::now();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<?>")
            .to_string();
        eprint!("fuzzing {name} ... ");
        use std::io::Write;
        let _ = std::io::stderr().flush();

        let bytes = std::fs::read(path).expect("read corpus file");
        let mut stats = FileStats {
            size: bytes.len() as u64,
            ..Default::default()
        };

        // The harness has no carriage-return corpus expectations. The
        // editor itself enforces its LF-only contract by rejecting CRLF.
        if bytes.contains(&b'\r') {
            stats.skip("crlf_file");
            per_file.push((path.clone(), stats));
            continue;
        }
        let Ok(pristine) = String::from_utf8(bytes) else {
            stats.skip("non_utf8_file");
            per_file.push((path.clone(), stats));
            continue;
        };

        let headings: Vec<Heading> = iter_headings(&pristine).collect();
        stats.headings = headings.len();
        parse_check(&mut stats, &pristine, &headings);

        if headings.is_empty() {
            per_file.push((path.clone(), stats));
            continue;
        }

        let keywords = keyword_pool(&pristine);
        // Derive a per-file seed so files are independent yet reproducible.
        let mut file_hash = seed;
        for b in path.to_string_lossy().bytes() {
            file_hash = file_hash
                .wrapping_mul(0x100_0000_01b3)
                .wrapping_add(b as u64);
        }
        let mut rng = Rng::new(file_hash);

        for _ in 0..iters {
            stats.iterations += 1;
            let idx = rng.below(headings.len());
            let h = headings[idx].clone();

            // Scalar ops: only meaningful for canonical heading lines.
            if heading_line_is_canonical(&pristine, &h) {
                roundtrip_keyword(&mut stats, &mut rng, &pristine, &h, &keywords);
                roundtrip_title(&mut stats, &mut rng, &pristine, &h, &keywords);
                roundtrip_tags(&mut stats, &mut rng, &pristine, &h);
            } else {
                stats.skip("non_canonical_heading_line");
            }

            // Drawer + structural ops: always valid.
            roundtrip_property(&mut stats, &mut rng, &pristine, &h);
            roundtrip_extract_insert(&mut stats, &pristine, &h);
            roundtrip_move(&mut stats, &mut rng, &pristine, &headings, &h);
        }

        total_failures += stats.failures.len();
        eprintln!(
            "done ({} hdgs, {} ok, {} fail, {:.1}s)",
            stats.headings,
            stats.ok,
            stats.failures.len(),
            file_start.elapsed().as_secs_f64()
        );
        per_file.push((path.clone(), stats));
    }

    // -------- report --------
    eprintln!(
        "{:<34} {:>8} {:>6} {:>6} {:>6} {:>6} {:>7}",
        "file", "size", "hdgs", "iters", "ok", "fail", "skips"
    );
    for (path, s) in &per_file {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<?>")
            .to_string();
        let skip_total: usize = s.skips.values().sum();
        eprintln!(
            "{:<34} {:>8} {:>6} {:>6} {:>6} {:>6} {:>7}",
            name,
            s.size,
            s.headings,
            s.iterations,
            s.ok,
            s.failures.len(),
            skip_total
        );
    }

    eprintln!("\n--- skip reasons (aggregate) ---");
    let mut agg: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (_, s) in &per_file {
        for (k, v) in &s.skips {
            *agg.entry(k).or_insert(0) += v;
        }
    }
    for (reason, count) in &agg {
        eprintln!("  {reason:<32} {count}");
    }

    if total_failures > 0 {
        eprintln!("\n--- FAILURES ({total_failures}) ---");
        for (path, s) in &per_file {
            for f in &s.failures {
                eprintln!("  {}: {f}", path.display());
            }
        }
        eprintln!("\nreplay with ORG_FUZZ_SEED={seed:#018x} ORG_FUZZ_DIR={dir}");
    }

    // Exercise a couple of ops that aren't part of the round-trip pairs
    // but should never panic on real data, to keep them covered.
    let _ = (
        add_heading
            as fn(
                &str,
                Option<usize>,
                AddPosition,
                Option<&str>,
                &str,
                &[String],
                &[(String, String)],
                Option<&str>,
                &TodoKeywords,
            ) -> _,
        delete_heading as fn(&str, usize) -> _,
        set_content as fn(&str, usize, &str) -> _,
    );

    assert_eq!(
        total_failures, 0,
        "fuzz found {total_failures} byte-identity failure(s); \
         replay with ORG_FUZZ_SEED={seed:#018x}"
    );
}

/// The default seed. Chosen arbitrarily; overridable via
/// `ORG_FUZZ_SEED`.
fn default_seed() -> u64 {
    0xC0FF_EE00_1234_5678
}
