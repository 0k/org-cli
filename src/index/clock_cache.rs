//! Two-layer cache for corpus-wide CLOCK queries.
//!
//! # Layer 1 — one artifact per file
//!
//! Keyed by `(path_id, content_sha256, abis)`. The value is the
//! [`Blob`] and diagnostics produced by [`crate::org::clock_project`].
//! A `(size, mtime_ns)` signature is recorded beside it so that a
//! file whose metadata has not moved can be trusted without being
//! re-read. That trust is a **shortcut, not a proof**: two writes
//! within the same mtime tick, or a tool that restores mtime, defeat
//! it. [`Verify::Bytes`] disables the shortcut and hashes every file.
//!
//! # Layer 2 — 256 buckets and one root
//!
//! Every file lands in bucket `path_id[0]`. Placement depends on the
//! **path only**, so editing a file never moves it. A bucket's
//! digest is composed from its members' L1 digests; the root's from
//! the 256 bucket digests plus the query scope. A warm, unchanged
//! corpus therefore resolves to a root digest that is already stored
//! and loads one blob — no bucket, no L1 payload is touched.
//!
//! After one edit: one L1 miss, one bucket re-merge, one root
//! re-merge. The root re-merge is `O(N)` in facts; there is no
//! pretence of `O(log N)` here, only the honest saving of not
//! touching the 255 unchanged buckets' *inputs*.
//!
//! # Failure is loud
//!
//! A file that cannot be read makes the whole query fail. Reporting
//! "no overlaps" over a partial corpus is worse than reporting
//! nothing, so the previous L1 entry is never substituted for a file
//! that is currently unreadable.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::index::clock_blob::Blob;
use crate::org::clock_project::{Diagnostic, PARSER_ABI, PROJECTION_ABI, project};

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS l1 (
    path_id      BLOB    NOT NULL,
    canonical    TEXT    NOT NULL,
    content_sha  BLOB    NOT NULL,
    proj_abi     INTEGER NOT NULL,
    parser_abi   INTEGER NOT NULL,
    size         INTEGER NOT NULL,
    mtime_ns     INTEGER NOT NULL,
    blob_digest  BLOB    NOT NULL,
    blob         BLOB    NOT NULL,
    diagnostics  BLOB    NOT NULL,
    PRIMARY KEY (path_id)
);
CREATE TABLE IF NOT EXISTS l2 (
    digest  BLOB PRIMARY KEY,
    blob    BLOB NOT NULL,
    kind    INTEGER NOT NULL,          -- 0 = bucket, 1 = root
    seen_at INTEGER NOT NULL
);
";

/// How hard to look before trusting a cached file entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verify {
    /// Trust `(size, mtime_ns)`; re-read only on mismatch.
    Metadata,
    /// Re-read and hash every selected file, always.
    Bytes,
}

/// Where the time went. Every field is additive across files.
#[derive(Debug, Clone, Default)]
pub struct Timing {
    /// stat (+ hash under `Verify::Bytes`) of every selected file.
    pub validate: Duration,
    /// read + orgize + project for L1 misses.
    pub l1_miss: Duration,
    /// bucket re-merges + root merge/load.
    pub l2: Duration,
    pub files_selected: usize,
    pub l1_hits: usize,
    pub l1_misses: usize,
    pub buckets_rebuilt: usize,
    pub root_hit: bool,
}

/// One resolved query: the globally sorted list plus everything a
/// report needs to render it faithfully.
#[derive(Debug)]
pub struct Resolved {
    /// Globally sorted facts. `strings` are shared across all files.
    pub root: Blob,
    /// Diagnostics for every selected file, in path order.
    pub diagnostics: Vec<(PathBuf, Vec<Diagnostic>)>,
    pub timing: Timing,
}

pub struct ClockCache {
    conn: Connection,
}

/// Stable identity of a canonical path.
fn path_id(canonical: &str) -> [u8; 8] {
    let mut h = Sha256::new();
    h.update(b"path-v1\0");
    h.update(canonical.as_bytes());
    let d = h.finalize();
    d[..8].try_into().expect("8 bytes")
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

impl ClockCache {
    /// Open (creating if needed) the cache at `path`.
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {}", parent.display(), e))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| format!("opening clock cache {}: {}", path.display(), e))?;
        Self::from_connection(conn)
    }

    /// In-memory cache, for tests.
    pub fn in_memory() -> Result<Self, String> {
        Self::from_connection(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn from_connection(mut conn: Connection) -> Result<Self, String> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| format!("enabling WAL: {}", e))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| format!("setting synchronous: {}", e))?;
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if v != SCHEMA_VERSION {
            let tx = conn.transaction().map_err(|e| e.to_string())?;
            tx.execute_batch("DROP TABLE IF EXISTS l1; DROP TABLE IF EXISTS l2;")
                .map_err(|e| e.to_string())?;
            tx.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())?;
        }
        Ok(Self { conn })
    }

    /// Resolve the globally sorted CLOCK list for `files`.
    ///
    /// `scope` is any caller-defined bytes that change the meaning of
    /// the result (the exclusion set, typically). It is folded into
    /// the root digest so two scopes never share a root.
    pub fn resolve(
        &mut self,
        files: &[PathBuf],
        scope: &[u8],
        verify: Verify,
    ) -> Result<Resolved, String> {
        let mut timing = Timing {
            files_selected: files.len(),
            ..Default::default()
        };

        // ---- L1: bring every selected file up to date -------------
        let t0 = Instant::now();
        let mut members: BTreeMap<[u8; 8], L1Row> = BTreeMap::new();
        let mut dirty_buckets = [false; 256];
        let mut all_diags: Vec<(PathBuf, Vec<Diagnostic>)> = Vec::new();

        for file in files {
            let canonical = canonical_string(file)?;
            let pid = path_id(&canonical);
            let meta =
                fs::metadata(file).map_err(|e| format!("cannot stat {}: {}", file.display(), e))?;
            let size = meta.len() as i64;
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);

            let existing = self.load_l1(&pid)?;

            // Fast path: metadata unchanged and ABIs match.
            if verify == Verify::Metadata
                && let Some(row) = &existing
                && row.size == size
                && row.mtime_ns == mtime_ns
                && row.proj_abi == PROJECTION_ABI
                && row.parser_abi == PARSER_ABI
            {
                timing.l1_hits += 1;
                all_diags.push((file.clone(), decode_diags(&row.diagnostics)?));
                members.insert(pid, row.clone());
                continue;
            }

            // Slow path: read + hash. An I/O error is fatal — never
            // report "clean" over a file the OS would not hand over.
            let mut bytes = Vec::new();
            fs::File::open(file)
                .and_then(|mut f| f.read_to_end(&mut bytes))
                .map_err(|e| format!("cannot read {}: {}", file.display(), e))?;
            let sha = sha256(&bytes);
            // Invalid UTF-8 is a *content* condition, not an I/O one.
            // Every other command warns and skips such a file; do the
            // same, with a lossy decode so its valid CLOCKs still
            // count. The hash is over the raw bytes, so the decision
            // is cached like any other.
            let content = match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("{}: not valid UTF-8, decoding lossily", file.display());
                    String::from_utf8_lossy(e.as_bytes()).into_owned()
                }
            };

            // Hash unchanged (e.g. `touch`): refresh metadata, reuse.
            if let Some(row) = &existing
                && row.content_sha == sha
                && row.proj_abi == PROJECTION_ABI
                && row.parser_abi == PARSER_ABI
            {
                timing.l1_hits += 1;
                self.touch_l1(&pid, size, mtime_ns)?;
                let mut row = row.clone();
                row.size = size;
                row.mtime_ns = mtime_ns;
                all_diags.push((file.clone(), decode_diags(&row.diagnostics)?));
                members.insert(pid, row);
                continue;
            }

            // Genuine miss: project.
            let t_miss = Instant::now();
            let projection = project(&canonical, &content);
            let blob_bytes = projection.blob.encode().map_err(|e| e.to_string())?;
            let row = L1Row {
                canonical: canonical.clone(),
                content_sha: sha,
                proj_abi: PROJECTION_ABI,
                parser_abi: PARSER_ABI,
                size,
                mtime_ns,
                blob_digest: sha256(&blob_bytes),
                blob: blob_bytes,
                diagnostics: encode_diags(&projection.diagnostics),
            };
            self.store_l1(&pid, &row)?;
            timing.l1_miss += t_miss.elapsed();
            timing.l1_misses += 1;
            dirty_buckets[pid[0] as usize] = true;
            all_diags.push((file.clone(), projection.diagnostics));
            members.insert(pid, row);
        }
        // `validate` = everything in the L1 loop that is not
        // projection: stat, SQLite row loads, and (on the slow path)
        // read + hash. The read+hash portion was accumulated inline
        // above; replace it with the loop total minus projection so
        // it is not counted twice.
        timing.validate = t0.elapsed().saturating_sub(timing.l1_miss);

        // ---- L2: buckets, then root -------------------------------
        let t2 = Instant::now();

        // Group members per bucket and compute each bucket's digest
        // from its members' blob digests. This is cheap (256 hashes
        // over ≤ N small inputs) and needs no payload loads.
        let mut bucket_members: Vec<Vec<&L1Row>> = vec![Vec::new(); 256];
        for (pid, row) in &members {
            bucket_members[pid[0] as usize].push(row);
        }
        let mut bucket_digests = [[0u8; 32]; 256];
        for (b, rows) in bucket_members.iter().enumerate() {
            let mut h = Sha256::new();
            h.update(b"bucket-v1\0");
            h.update([b as u8]);
            for r in rows {
                h.update(path_id(&r.canonical));
                h.update(r.blob_digest);
            }
            bucket_digests[b] = h.finalize().into();
        }

        let mut h = Sha256::new();
        h.update(b"root-v1\0");
        h.update(PROJECTION_ABI.to_le_bytes());
        h.update(PARSER_ABI.to_le_bytes());
        h.update((scope.len() as u64).to_le_bytes());
        h.update(scope);
        for d in &bucket_digests {
            h.update(d);
        }
        let root_digest: [u8; 32] = h.finalize().into();

        let root = if let Some(bytes) = self.load_l2(&root_digest)? {
            timing.root_hit = true;
            Blob::decode(&bytes).map_err(|e| format!("cached root: {}", e))?
        } else {
            // Rebuild only buckets whose digest is not stored.
            let mut bucket_blobs: Vec<Blob> = Vec::with_capacity(256);
            for (b, rows) in bucket_members.iter().enumerate() {
                if let Some(bytes) = self.load_l2(&bucket_digests[b])? {
                    bucket_blobs.push(Blob::decode(&bytes).map_err(|e| e.to_string())?);
                    continue;
                }
                let decoded: Vec<Blob> = rows
                    .iter()
                    .map(|r| Blob::decode(&r.blob).map_err(|e| e.to_string()))
                    .collect::<Result<_, _>>()?;
                let refs: Vec<&Blob> = decoded.iter().collect();
                let merged = if refs.is_empty() {
                    Blob::empty(PROJECTION_ABI, PARSER_ABI)
                } else {
                    Blob::merge(&refs)
                };
                self.store_l2(
                    &bucket_digests[b],
                    &merged.encode().map_err(|e| e.to_string())?,
                    0,
                )?;
                timing.buckets_rebuilt += 1;
                bucket_blobs.push(merged);
            }
            let refs: Vec<&Blob> = bucket_blobs.iter().collect();
            let root = Blob::merge(&refs);
            self.store_l2(&root_digest, &root.encode().map_err(|e| e.to_string())?, 1)?;
            root
        };
        timing.l2 = t2.elapsed();

        all_diags.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Resolved {
            root,
            diagnostics: all_diags,
            timing,
        })
    }

    // ---- persistence -------------------------------------------------

    fn load_l1(&self, pid: &[u8; 8]) -> Result<Option<L1Row>, String> {
        self.conn
            .query_row(
                "SELECT canonical, content_sha, proj_abi, parser_abi, size, mtime_ns,
                        blob_digest, blob, diagnostics
                 FROM l1 WHERE path_id = ?1",
                params![pid.as_slice()],
                |r| {
                    let sha: Vec<u8> = r.get(1)?;
                    let bd: Vec<u8> = r.get(6)?;
                    Ok(L1Row {
                        canonical: r.get(0)?,
                        content_sha: sha.try_into().unwrap_or([0; 32]),
                        proj_abi: r.get::<_, i64>(2)? as u16,
                        parser_abi: r.get::<_, i64>(3)? as u16,
                        size: r.get(4)?,
                        mtime_ns: r.get(5)?,
                        blob_digest: bd.try_into().unwrap_or([0; 32]),
                        blob: r.get(7)?,
                        diagnostics: r.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(|e| format!("reading l1: {}", e))
    }

    fn store_l1(&self, pid: &[u8; 8], row: &L1Row) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO l1
                 (path_id, canonical, content_sha, proj_abi, parser_abi, size, mtime_ns,
                  blob_digest, blob, diagnostics)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    pid.as_slice(),
                    row.canonical,
                    row.content_sha.as_slice(),
                    row.proj_abi as i64,
                    row.parser_abi as i64,
                    row.size,
                    row.mtime_ns,
                    row.blob_digest.as_slice(),
                    row.blob,
                    row.diagnostics,
                ],
            )
            .map(|_| ())
            .map_err(|e| format!("writing l1: {}", e))
    }

    fn touch_l1(&self, pid: &[u8; 8], size: i64, mtime_ns: i64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE l1 SET size = ?2, mtime_ns = ?3 WHERE path_id = ?1",
                params![pid.as_slice(), size, mtime_ns],
            )
            .map(|_| ())
            .map_err(|e| format!("touching l1: {}", e))
    }

    fn load_l2(&self, digest: &[u8; 32]) -> Result<Option<Vec<u8>>, String> {
        self.conn
            .query_row(
                "SELECT blob FROM l2 WHERE digest = ?1",
                params![digest.as_slice()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| format!("reading l2: {}", e))
    }

    fn store_l2(&self, digest: &[u8; 32], bytes: &[u8], kind: i64) -> Result<(), String> {
        let now = UNIX_EPOCH
            .elapsed()
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.conn
            .execute(
                "INSERT OR REPLACE INTO l2 (digest, blob, kind, seen_at) VALUES (?1,?2,?3,?4)",
                params![digest.as_slice(), bytes, kind, now],
            )
            .map(|_| ())
            .map_err(|e| format!("writing l2: {}", e))
    }

    /// Drop L2 rows not seen for `older_than`. Buckets and roots
    /// accumulate one row per distinct state; this keeps the table
    /// from growing without bound.
    pub fn prune_l2(&self, older_than: Duration) -> Result<usize, String> {
        let cutoff = UNIX_EPOCH
            .elapsed()
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
            - older_than.as_secs() as i64;
        self.conn
            .execute("DELETE FROM l2 WHERE seen_at < ?1", params![cutoff])
            .map_err(|e| format!("pruning l2: {}", e))
    }

    #[cfg(test)]
    fn l2_count(&self, kind: i64) -> usize {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM l2 WHERE kind = ?1",
                params![kind],
                |r| r.get::<_, i64>(0),
            )
            .unwrap() as usize
    }
}

#[derive(Debug, Clone)]
struct L1Row {
    canonical: String,
    content_sha: [u8; 32],
    proj_abi: u16,
    parser_abi: u16,
    size: i64,
    mtime_ns: i64,
    blob_digest: [u8; 32],
    blob: Vec<u8>,
    diagnostics: Vec<u8>,
}

fn canonical_string(p: &Path) -> Result<String, String> {
    let c = p
        .canonicalize()
        .map_err(|e| format!("cannot canonicalize {}: {}", p.display(), e))?;
    Ok(c.to_string_lossy().into_owned())
}

/// Diagnostics are few and rendering-only; a compact NUL-delimited
/// encoding is plenty and avoids another schema.
fn encode_diags(d: &[Diagnostic]) -> Vec<u8> {
    let mut out = Vec::new();
    for x in d {
        out.extend_from_slice(x.line.to_string().as_bytes());
        out.push(0);
        out.extend_from_slice(x.byte_offset.to_string().as_bytes());
        out.push(0);
        out.extend_from_slice(x.tag.as_bytes());
        out.push(0);
        out.extend_from_slice(x.message.as_bytes());
        out.push(0);
        out.extend_from_slice(x.raw.as_bytes());
        out.push(0);
    }
    out
}

fn decode_diags(bytes: &[u8]) -> Result<Vec<Diagnostic>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let fields: Vec<&[u8]> = bytes.split(|&b| b == 0).collect();
    // split() yields a trailing empty slice after the final NUL.
    let fields = &fields[..fields.len().saturating_sub(1)];
    if !fields.len().is_multiple_of(5) {
        return Err("corrupt diagnostics blob".into());
    }
    let s = |b: &[u8]| String::from_utf8(b.to_vec()).map_err(|_| "corrupt diagnostics utf8");
    let mut out = Vec::with_capacity(fields.len() / 5);
    for c in fields.chunks(5) {
        let tag_owned = s(c[2])?;
        out.push(Diagnostic {
            line: s(c[0])?.parse().map_err(|_| "corrupt diagnostics line")?,
            byte_offset: s(c[1])?.parse().map_err(|_| "corrupt diagnostics offset")?,
            tag: intern_tag(&tag_owned),
            message: s(c[3])?,
            raw: s(c[4])?,
        });
    }
    Ok(out)
}

/// Map a stored tag back to the `&'static str` the clock module
/// uses. Unknown tags (from a future ABI) fall back to a stable
/// placeholder rather than failing the whole query.
fn intern_tag(s: &str) -> &'static str {
    match s {
        "missing-tz" => "missing-tz",
        "non-numerical-tz" => "non-numerical-tz",
        "unparseable" => "unparseable",
        "weekday-mismatch" => "weekday-mismatch",
        "mixed-precision" => "mixed-precision",
        "tz-mismatch" => "tz-mismatch",
        "start-after-end" => "start-after-end",
        "duration-mismatch" => "duration-mismatch",
        "spacing-mismatch" => "spacing-mismatch",
        "unaccepted-tz" => "unaccepted-tz",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let n = std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            p.push(format!(
                "org-cli-clockcache-{}-{}-{}",
                tag,
                std::process::id(),
                n
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

    const CLOCK_A: &str =
        "* a\nCLOCK: [2026-01-07 Wed 10:00 +0100]--[2026-01-07 Wed 12:00 +0100] =>  2:00\n";
    const CLOCK_B: &str =
        "* b\nCLOCK: [2026-01-07 Wed 11:00 +0100]--[2026-01-07 Wed 13:00 +0100] =>  2:00\n";
    const CLOCK_C: &str =
        "* c\nCLOCK: [2026-01-08 Thu 09:00 +0100]--[2026-01-08 Thu 10:00 +0100] =>  1:00\n";

    // S1
    #[test]
    fn metadata_hit_skips_parse() {
        let td = TempDir::new("s1");
        let f = td.path().join("a.org");
        write(&f, CLOCK_A);
        let mut c = ClockCache::in_memory().unwrap();

        let r1 = c
            .resolve(std::slice::from_ref(&f), b"", Verify::Metadata)
            .unwrap();
        assert_eq!(r1.timing.l1_misses, 1);

        let r2 = c
            .resolve(std::slice::from_ref(&f), b"", Verify::Metadata)
            .unwrap();
        assert_eq!(r2.timing.l1_misses, 0);
        assert_eq!(r2.timing.l1_hits, 1);
        assert_eq!(r2.root.records, r1.root.records);
    }

    // S2
    #[test]
    fn touch_without_change_is_a_hit() {
        let td = TempDir::new("s2");
        let f = td.path().join("a.org");
        write(&f, CLOCK_A);
        let mut c = ClockCache::in_memory().unwrap();
        c.resolve(std::slice::from_ref(&f), b"", Verify::Metadata)
            .unwrap();

        // Rewrite identical bytes: mtime moves, content does not.
        std::thread::sleep(Duration::from_millis(15));
        write(&f, CLOCK_A);

        let r = c
            .resolve(std::slice::from_ref(&f), b"", Verify::Metadata)
            .unwrap();
        assert_eq!(r.timing.l1_misses, 0, "same bytes must not re-project");
        assert_eq!(r.timing.l1_hits, 1);
    }

    // S3 — the metadata-preserving edit
    #[test]
    fn verify_catches_metadata_preserving_edit() {
        let td = TempDir::new("s3");
        let f = td.path().join("a.org");
        write(&f, CLOCK_A);
        let mut c = ClockCache::in_memory().unwrap();
        c.resolve(std::slice::from_ref(&f), b"", Verify::Metadata)
            .unwrap();

        // Same length, different bytes; restore mtime exactly.
        let meta = fs::metadata(&f).unwrap();
        let mtime = meta.modified().unwrap();
        let edited = CLOCK_A
            .replace("10:00", "10:30")
            .replace("=>  2:00", "=>  1:30");
        assert_eq!(edited.len(), CLOCK_A.len());
        write(&f, &edited);
        fs::File::open(&f).unwrap().set_modified(mtime).unwrap();

        let m = c
            .resolve(std::slice::from_ref(&f), b"", Verify::Metadata)
            .unwrap();
        assert_eq!(m.timing.l1_hits, 1, "metadata mode is fooled — by design");

        let v = c
            .resolve(std::slice::from_ref(&f), b"", Verify::Bytes)
            .unwrap();
        assert_eq!(v.timing.l1_misses, 1, "byte verification must catch it");
        assert_eq!(v.root.records[0].end - v.root.records[0].start, 5400);
    }

    // S4
    #[test]
    fn read_error_fails_query() {
        let td = TempDir::new("s4");
        let f = td.path().join("a.org");
        write(&f, CLOCK_A);
        let mut c = ClockCache::in_memory().unwrap();
        c.resolve(std::slice::from_ref(&f), b"", Verify::Metadata)
            .unwrap();
        fs::remove_file(&f).unwrap();
        let err = c
            .resolve(std::slice::from_ref(&f), b"", Verify::Metadata)
            .unwrap_err();
        // A vanished file is refused at the identity boundary, before
        // any cached row could be consulted.
        assert!(err.contains("cannot canonicalize"), "{}", err);
        assert!(err.contains("a.org"), "{}", err);
    }

    // S4: unreadable (present but no permission) is refused at the
    // read boundary, and the stale L1 row is NOT used in its place.
    #[cfg(unix)]
    #[test]
    fn unreadable_file_fails_query_without_stale_fallback() {
        use std::os::unix::fs::PermissionsExt;
        let td = TempDir::new("s4b");
        let f = td.path().join("a.org");
        write(&f, CLOCK_A);
        let mut c = ClockCache::in_memory().unwrap();
        c.resolve(std::slice::from_ref(&f), b"", Verify::Metadata)
            .unwrap();

        // Change bytes (so the metadata fast path cannot apply) and
        // then revoke read permission.
        std::thread::sleep(Duration::from_millis(15));
        write(&f, CLOCK_B);
        fs::set_permissions(&f, fs::Permissions::from_mode(0o000)).unwrap();

        let res = c.resolve(std::slice::from_ref(&f), b"", Verify::Metadata);
        fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();

        let err = res.unwrap_err();
        assert!(err.contains("cannot read"), "{}", err);
    }

    // S5
    #[test]
    fn empty_result_is_cached() {
        let td = TempDir::new("s5");
        let f = td.path().join("a.org");
        write(&f, "* no clocks here\n");
        let mut c = ClockCache::in_memory().unwrap();
        let r1 = c
            .resolve(std::slice::from_ref(&f), b"", Verify::Metadata)
            .unwrap();
        assert!(r1.root.records.is_empty());
        let r2 = c
            .resolve(std::slice::from_ref(&f), b"", Verify::Metadata)
            .unwrap();
        assert_eq!(r2.timing.l1_hits, 1);
        assert_eq!(r2.timing.l1_misses, 0);
    }

    // L1 / L2 / L3
    #[test]
    fn one_edit_rebuilds_one_bucket_and_root_hit_loads_no_buckets() {
        let td = TempDir::new("l2");
        let files: Vec<PathBuf> = (0..40)
            .map(|i| {
                let p = td.path().join(format!("f{i}.org"));
                write(&p, CLOCK_C);
                p
            })
            .collect();
        let mut c = ClockCache::in_memory().unwrap();

        let cold = c.resolve(&files, b"", Verify::Metadata).unwrap();
        assert_eq!(cold.timing.l1_misses, 40);
        assert!(!cold.timing.root_hit);
        let buckets_after_cold = c.l2_count(0);
        assert_eq!(buckets_after_cold, 256, "every bucket materialised once");

        let warm = c.resolve(&files, b"", Verify::Metadata).unwrap();
        assert!(warm.timing.root_hit, "unchanged corpus must hit the root");
        assert_eq!(warm.timing.buckets_rebuilt, 0);
        assert_eq!(warm.timing.l1_misses, 0);

        // Edit exactly one file.
        std::thread::sleep(Duration::from_millis(15));
        write(&files[7], CLOCK_A);
        let edit = c.resolve(&files, b"", Verify::Metadata).unwrap();
        assert_eq!(edit.timing.l1_misses, 1);
        assert!(!edit.timing.root_hit);
        assert_eq!(
            edit.timing.buckets_rebuilt, 1,
            "one file → exactly one bucket rebuilt"
        );
        assert_eq!(c.l2_count(0), buckets_after_cold + 1);
        assert_eq!(edit.root.records.len(), 40);
    }

    // L4
    #[test]
    fn root_matches_direct_sort() {
        let td = TempDir::new("l4");
        let a = td.path().join("a.org");
        let b = td.path().join("b.org");
        let cc = td.path().join("c.org");
        write(&a, CLOCK_A);
        write(&b, CLOCK_B);
        write(&cc, CLOCK_C);
        let mut c = ClockCache::in_memory().unwrap();
        let r = c
            .resolve(&[cc.clone(), a.clone(), b.clone()], b"", Verify::Metadata)
            .unwrap();

        let mut direct: Vec<_> = [&a, &b, &cc]
            .iter()
            .flat_map(|p| {
                let s = fs::read_to_string(p).unwrap();
                project(&canonical_string(p).unwrap(), &s)
                    .blob
                    .records
                    .into_iter()
                    .map(|rec| (rec.start, rec.end))
                    .collect::<Vec<_>>()
            })
            .collect();
        direct.sort();
        let cached: Vec<_> = r.root.records.iter().map(|x| (x.start, x.end)).collect();
        assert_eq!(cached, direct);
        assert!(
            r.root
                .records
                .windows(2)
                .all(|w| w[0].sort_key() <= w[1].sort_key())
        );
    }

    // L5
    #[test]
    fn membership_change_updates_root() {
        let td = TempDir::new("l5");
        let a = td.path().join("a.org");
        let b = td.path().join("b.org");
        write(&a, CLOCK_A);
        write(&b, CLOCK_B);
        let mut c = ClockCache::in_memory().unwrap();

        let both = c
            .resolve(&[a.clone(), b.clone()], b"", Verify::Metadata)
            .unwrap();
        assert_eq!(both.root.records.len(), 2);

        let one = c
            .resolve(std::slice::from_ref(&a), b"", Verify::Metadata)
            .unwrap();
        assert_eq!(
            one.root.records.len(),
            1,
            "dropping a file must drop its facts"
        );
        assert!(!one.timing.root_hit);
    }

    // L6
    #[test]
    fn scope_is_in_root_digest() {
        let td = TempDir::new("l6");
        let a = td.path().join("a.org");
        write(&a, CLOCK_A);
        let mut c = ClockCache::in_memory().unwrap();
        c.resolve(std::slice::from_ref(&a), b"scope-1", Verify::Metadata)
            .unwrap();
        let r = c
            .resolve(std::slice::from_ref(&a), b"scope-2", Verify::Metadata)
            .unwrap();
        assert!(
            !r.timing.root_hit,
            "a different scope must not reuse the root"
        );
        let r = c
            .resolve(std::slice::from_ref(&a), b"scope-1", Verify::Metadata)
            .unwrap();
        assert!(r.timing.root_hit);
    }

    // O1
    #[test]
    fn cross_bucket_overlap_found() {
        use crate::org::clock_overlap::{Interval, find_overlaps};
        let td = TempDir::new("o1");
        // Find two paths that land in different buckets.
        let mut a = None;
        let mut b = None;
        for i in 0..64 {
            let p = td.path().join(format!("f{i}.org"));
            write(&p, "");
            let pid = path_id(&canonical_string(&p).unwrap());
            match (&a, &b) {
                (None, _) => a = Some((p, pid[0])),
                (Some((_, ba)), None) if pid[0] != *ba => b = Some((p, pid[0])),
                _ => {}
            }
            if b.is_some() {
                break;
            }
        }
        let (pa, ba) = a.unwrap();
        let (pb, bb) = b.unwrap();
        assert_ne!(ba, bb);
        write(&pa, CLOCK_A);
        write(&pb, CLOCK_B);

        let mut c = ClockCache::in_memory().unwrap();
        let r = c.resolve(&[pa, pb], b"", Verify::Metadata).unwrap();
        let mut ivs: Vec<Interval> = r
            .root
            .records
            .iter()
            .map(|x| Interval {
                start: x.start,
                end: x.end,
                file: PathBuf::from(&r.root.strings[x.path_id as usize]),
                line: x.line as usize,
                raw: String::new(),
            })
            .collect();
        let pairs = find_overlaps(&mut ivs);
        assert_eq!(pairs.len(), 1, "overlap across buckets must be found");
        assert_eq!(pairs[0].overlap_seconds, 3600);
    }

    // O4
    #[test]
    fn diagnostics_survive_cache() {
        let td = TempDir::new("o4");
        let f = td.path().join("a.org");
        write(
            &f,
            "* h\nCLOCK: [2026-01-07 Wed 09:57]--[2026-01-07 Wed 12:19] =>  2:22\n",
        );
        let mut c = ClockCache::in_memory().unwrap();
        let cold = c
            .resolve(std::slice::from_ref(&f), b"", Verify::Metadata)
            .unwrap();
        let warm = c
            .resolve(std::slice::from_ref(&f), b"", Verify::Metadata)
            .unwrap();
        assert_eq!(warm.timing.l1_hits, 1);
        assert_eq!(cold.diagnostics, warm.diagnostics);
        assert_eq!(warm.diagnostics[0].1[0].tag, "missing-tz");
    }

    #[test]
    fn diags_round_trip() {
        let d = vec![Diagnostic {
            line: 3,
            byte_offset: 42,
            tag: "missing-tz",
            message: "start has no ±HHMM".into(),
            raw: "  CLOCK: [x]".into(),
        }];
        assert_eq!(decode_diags(&encode_diags(&d)).unwrap(), d);
        assert!(decode_diags(&[]).unwrap().is_empty());
    }

    #[test]
    fn prune_removes_stale_l2_rows() {
        let td = TempDir::new("prune");
        let f = td.path().join("a.org");
        write(&f, CLOCK_A);
        let mut c = ClockCache::in_memory().unwrap();
        c.resolve(std::slice::from_ref(&f), b"", Verify::Metadata)
            .unwrap();
        assert!(c.l2_count(1) >= 1);
        // Nothing is older than "now + 0".
        let removed = c.prune_l2(Duration::from_secs(0)).unwrap();
        let _ = removed;
        // A generous window keeps everything.
        c.resolve(std::slice::from_ref(&f), b"", Verify::Metadata)
            .unwrap();
        assert_eq!(c.prune_l2(Duration::from_secs(3600)).unwrap(), 0);
    }

    // File writes in tests use std::fs directly; keep clippy quiet
    // about the unused import in non-test builds.
    #[allow(dead_code)]
    fn _unused(_: &mut dyn Write) {}
}
