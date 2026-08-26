//! The SQLite-backed TODO store: schema, incremental sync, and queries.
//!
//! See [`crate::index`] for the module-level rationale.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::org::snapshot::FileSnapshot;
use crate::org::todo_keywords::TodoKeywords;

/// One indexed TODO heading, as returned by [`TodoStore::query`].
///
/// Field order mirrors the on-disk columns and the human/NUL output
/// layout so serialisation and formatting stay obvious.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TodoRecord {
    /// Absolute (or as-indexed) path of the file the heading lives in.
    pub path: String,
    /// 1-based line number of the heading within its file.
    pub line: usize,
    /// Heading depth (number of leading stars).
    pub level: u32,
    /// The org-mode TODO keyword (always `Some` — non-keyword
    /// headings are never indexed).
    pub keyword: String,
    /// The heading title, keyword and tags stripped.
    pub title: String,
    /// The heading's tags, in document order.
    pub tags: Vec<String>,
    /// Whether `keyword` is a *done* (terminal) state for the file's
    /// effective keyword set.
    pub is_done: bool,
    /// The heading's priority as a bare value (e.g. `A`), if present.
    pub priority: Option<String>,
    /// Lowercase hex SHA-256 of the exact heading line without its terminator.
    pub fingerprint: String,
    /// Lowercase hex SHA-256 of the exact file bytes that produced this row.
    pub etag: String,
    /// The effective TODO keyword set used to parse this row's snapshot.
    pub todo_keywords: TodoKeywords,
}

/// A query against the index. Both fields `None` selects every row.
#[derive(Debug, Clone, Default)]
pub struct TodoQuery {
    /// Case-insensitive substring match against the title. The `%`
    /// and `_` LIKE wildcards in the search text are escaped so they
    /// match literally.
    pub search: Option<String>,
    /// Exact-match TODO keyword filter.
    pub keyword: Option<String>,
}

/// The diff produced by a [`TodoStore::sync`] call.
///
/// This is the cross-layer vocabulary type: the future daemon
/// serialises these verbatim onto its SSE stream. The wire form uses a
/// `type` discriminator with kebab-case values
/// (`file-changed` / `file-removed`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum TodoEvent {
    /// A new or modified file: `todos` is the full current TODO set
    /// for that file (replace-in-place semantics for consumers).
    FileChanged {
        /// The file's path.
        path: String,
        /// Lowercase hex SHA-256 of the exact bytes that produced this event.
        etag: String,
        /// The effective TODO keyword set used to parse this event.
        todo_keywords: TodoKeywords,
        /// Every TODO heading currently in the file.
        todos: Vec<TodoRecord>,
    },
    /// A file that was in the index but is gone from the input set (or
    /// became unreadable): consumers should drop all its TODOs.
    FileRemoved {
        /// The removed file's path.
        path: String,
    },
}

/// A SQLite-backed index of org-mode TODO headings.
pub struct TodoStore {
    conn: Connection,
}

impl TodoStore {
    /// Open (creating if necessary) the store at `path`.
    ///
    /// Parent directories are created, the schema is applied
    /// idempotently, WAL journalling and foreign-key enforcement are
    /// enabled.
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {}", parent.display(), e))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| format!("opening sqlite db {}: {}", path.display(), e))?;
        Self::from_connection(conn)
    }

    /// Build a store over an already-open connection (used by tests
    /// with `:memory:`). Applies the pragmas and schema.
    fn from_connection(mut conn: Connection) -> Result<Self, String> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| format!("enabling WAL: {}", e))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| format!("enabling foreign_keys: {}", e))?;
        let stored_version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|e| format!("reading schema version: {}", e))?;
        if stored_version != SCHEMA_VERSION {
            let tx = conn
                .transaction()
                .map_err(|e| format!("starting schema migration: {}", e))?;
            tx.execute_batch(
                "DROP TABLE IF EXISTS headings;
                 DROP TABLE IF EXISTS files;",
            )
            .map_err(|e| format!("dropping old schema: {}", e))?;
            tx.execute_batch(SCHEMA)
                .map_err(|e| format!("applying schema: {}", e))?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|e| format!("setting schema version: {}", e))?;
            tx.commit()
                .map_err(|e| format!("committing schema migration: {}", e))?;
        }
        Ok(Self { conn })
    }

    /// Incrementally sync the index against `files`.
    ///
    /// The fingerprint `(mtime_ns, size)` of each file is compared
    /// against what the index recorded; only new or changed files are
    /// re-read and re-parsed. Files present in the index but absent
    /// from `files` — or that cannot be read/stat'd — are removed.
    ///
    /// Each file's update runs in its own transaction (delete the old
    /// rows, insert the new). Returns the applied diff as a
    /// `Vec<TodoEvent>`, ordered: changes (in `files` order) first,
    /// then removals.
    pub fn sync(&mut self, files: &[PathBuf]) -> Result<Vec<TodoEvent>, String> {
        self.sync_forcing(files, &BTreeSet::new())
    }

    /// Incrementally sync the index, always re-reading paths in `force`.
    ///
    /// This is the watcher-facing variant of [`Self::sync`]. A filesystem
    /// notification is authoritative even when an atomic replacement happens
    /// to preserve both the recorded modification timestamp and byte length.
    pub fn sync_forcing(
        &mut self,
        files: &[PathBuf],
        force: &BTreeSet<PathBuf>,
    ) -> Result<Vec<TodoEvent>, String> {
        let mut events = Vec::new();
        let mut seen: Vec<String> = Vec::with_capacity(files.len());
        let fallback = TodoKeywords::default();
        let canonical_force: BTreeSet<PathBuf> = force
            .iter()
            .filter_map(|path| fs::canonicalize(path).ok())
            .collect();

        for file in files {
            let path_str = file.to_string_lossy().to_string();
            seen.push(path_str.clone());

            let canonical = match fs::canonicalize(file) {
                Ok(canonical) => canonical,
                Err(_) => {
                    seen.pop();
                    continue;
                }
            };
            let canonical_str = canonical.to_string_lossy().to_string();

            // A file we cannot stat is treated as absent: it will be
            // pruned in the removal pass below (we simply don't record
            // it as seen-and-present).
            let Ok(meta) = fs::metadata(file) else {
                // Drop from `seen` so the prune pass removes it.
                seen.pop();
                continue;
            };
            let size = meta.len() as i64;
            let mtime_ns = mtime_ns_of(&meta);

            if !force.contains(file)
                && !canonical_force.contains(&canonical)
                && let Some((old_mtime, old_size, old_canonical)) = self.fingerprint(&path_str)?
                && old_mtime == mtime_ns
                && old_size == size
                && old_canonical == canonical_str
            {
                // Unchanged: skip re-parse, emit nothing.
                continue;
            }

            let snapshot = match FileSnapshot::read(file, &fallback) {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    // Unreadable content: treat as absent (prune).
                    seen.pop();
                    continue;
                }
            };
            let todos = records_from_snapshot(&path_str, &snapshot);
            self.replace_file(&path_str, &canonical_str, mtime_ns, size, &snapshot, &todos)?;
            events.push(TodoEvent::FileChanged {
                path: path_str,
                etag: snapshot.etag,
                todo_keywords: snapshot.keywords,
                todos,
            });
        }

        // Removal pass: any indexed file not in `seen` is gone.
        for path_str in self.indexed_paths()? {
            if !seen.contains(&path_str) {
                self.remove_file(&path_str)?;
                events.push(TodoEvent::FileRemoved { path: path_str });
            }
        }

        Ok(events)
    }

    /// Return the indexed display path for an exact canonical path.
    ///
    /// The returned string is the existing API/display identity stored in
    /// `files.path`; canonical identity is used only for the membership check.
    pub fn indexed_path_for_canonical(&self, canonical: &Path) -> Result<Option<String>, String> {
        let canonical = canonical.to_string_lossy();
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM files WHERE canonical_path = ? LIMIT 1")
            .map_err(|e| format!("preparing canonical path query: {}", e))?;
        let mut rows = stmt
            .query(params![canonical.as_ref()])
            .map_err(|e| format!("running canonical path query: {}", e))?;
        match rows.next().map_err(|e| format!("reading row: {}", e))? {
            Some(row) => row
                .get(0)
                .map(Some)
                .map_err(|e| format!("reading indexed path: {}", e)),
            None => Ok(None),
        }
    }

    /// Query the index. Rows are ordered by path, then line.
    pub fn query(&self, q: &TodoQuery) -> Result<Vec<TodoRecord>, String> {
        let mut sql = String::from(
            "SELECT f.path, h.line, h.level, h.keyword, h.title, h.tags_json, h.is_done, \
                    h.priority, h.fingerprint, f.etag, f.todo_active_json, f.todo_done_json \
             FROM headings h JOIN files f ON f.id = h.file_id",
        );
        let mut clauses: Vec<String> = Vec::new();
        let mut binds: Vec<String> = Vec::new();

        if let Some(kw) = &q.keyword {
            clauses.push("h.keyword = ?".to_string());
            binds.push(kw.clone());
        }
        if let Some(search) = &q.search {
            // Case-insensitive substring; escape LIKE metacharacters so
            // `%` and `_` in the needle match literally. SQLite's LIKE
            // is already case-insensitive for ASCII.
            clauses.push("h.title LIKE ? ESCAPE '\\'".to_string());
            binds.push(format!("%{}%", escape_like(search)));
        }
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY f.path, h.line");

        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| format!("preparing query: {}", e))?;
        let param_refs: Vec<&dyn rusqlite::ToSql> =
            binds.iter().map(|b| b as &dyn rusqlite::ToSql).collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                let path: String = row.get(0)?;
                let line: i64 = row.get(1)?;
                let level: i64 = row.get(2)?;
                let keyword: String = row.get(3)?;
                let title: String = row.get(4)?;
                let tags_json: String = row.get(5)?;
                let is_done: i64 = row.get(6)?;
                let priority: Option<String> = row.get(7)?;
                let fingerprint: String = row.get(8)?;
                let etag: String = row.get(9)?;
                let todo_active_json: String = row.get(10)?;
                let todo_done_json: String = row.get(11)?;
                Ok((
                    path,
                    line,
                    level,
                    keyword,
                    title,
                    tags_json,
                    is_done,
                    priority,
                    fingerprint,
                    etag,
                    todo_active_json,
                    todo_done_json,
                ))
            })
            .map_err(|e| format!("running query: {}", e))?;

        let mut out = Vec::new();
        for row in rows {
            let (
                path,
                line,
                level,
                keyword,
                title,
                tags_json,
                is_done,
                priority,
                fingerprint,
                etag,
                todo_active_json,
                todo_done_json,
            ) = row.map_err(|e| format!("reading row: {}", e))?;
            let tags: Vec<String> = serde_json::from_str(&tags_json)
                .map_err(|e| format!("decoding tags_json {:?}: {}", tags_json, e))?;
            let active: Vec<String> = serde_json::from_str(&todo_active_json)
                .map_err(|e| format!("decoding todo_active_json {:?}: {}", todo_active_json, e))?;
            let done: Vec<String> = serde_json::from_str(&todo_done_json)
                .map_err(|e| format!("decoding todo_done_json {:?}: {}", todo_done_json, e))?;
            out.push(TodoRecord {
                path,
                line: line as usize,
                level: level as u32,
                keyword,
                title,
                tags,
                is_done: is_done != 0,
                priority,
                fingerprint,
                etag,
                todo_keywords: TodoKeywords { active, done },
            });
        }
        Ok(out)
    }

    /// Return the stored `(mtime_ns, size, canonical_path)` state of `path`,
    /// if the file is already indexed.
    fn fingerprint(&self, path: &str) -> Result<Option<(i64, i64, String)>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT mtime_ns, size, canonical_path FROM files WHERE path = ?")
            .map_err(|e| format!("preparing fingerprint query: {}", e))?;
        let mut rows = stmt
            .query(params![path])
            .map_err(|e| format!("running fingerprint query: {}", e))?;
        match rows.next().map_err(|e| format!("reading row: {}", e))? {
            Some(row) => {
                let mtime: i64 = row.get(0).map_err(|e| e.to_string())?;
                let size: i64 = row.get(1).map_err(|e| e.to_string())?;
                let canonical: String = row.get(2).map_err(|e| e.to_string())?;
                Ok(Some((mtime, size, canonical)))
            }
            None => Ok(None),
        }
    }

    /// Return every path currently recorded in the `files` table.
    fn indexed_paths(&self) -> Result<Vec<String>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM files")
            .map_err(|e| format!("preparing paths query: {}", e))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| format!("running paths query: {}", e))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| format!("reading row: {}", e))?);
        }
        Ok(out)
    }

    /// Replace a file's rows atomically: upsert the `files` row with
    /// its new fingerprint, delete its old headings, insert the new
    /// set.
    fn replace_file(
        &mut self,
        path: &str,
        canonical_path: &str,
        mtime_ns: i64,
        size: i64,
        snapshot: &FileSnapshot,
        todos: &[TodoRecord],
    ) -> Result<(), String> {
        let todo_active_json = serde_json::to_string(&snapshot.keywords.active)
            .map_err(|e| format!("encoding active TODO keywords: {}", e))?;
        let todo_done_json = serde_json::to_string(&snapshot.keywords.done)
            .map_err(|e| format!("encoding done TODO keywords: {}", e))?;
        let tx = self
            .conn
            .transaction()
            .map_err(|e| format!("starting transaction: {}", e))?;
        tx.execute(
            "INSERT INTO files( \
                 path, canonical_path, mtime_ns, size, etag, todo_active_json, todo_done_json \
             ) VALUES(?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(path) DO UPDATE SET \
                 canonical_path = excluded.canonical_path, \
                 mtime_ns = excluded.mtime_ns, \
                 size = excluded.size, \
                 etag = excluded.etag, \
                 todo_active_json = excluded.todo_active_json, \
                 todo_done_json = excluded.todo_done_json",
            params![
                path,
                canonical_path,
                mtime_ns,
                size,
                snapshot.etag,
                todo_active_json,
                todo_done_json
            ],
        )
        .map_err(|e| format!("upserting file row: {}", e))?;
        let file_id: i64 = tx
            .query_row("SELECT id FROM files WHERE path = ?", params![path], |r| {
                r.get(0)
            })
            .map_err(|e| format!("fetching file id: {}", e))?;
        tx.execute("DELETE FROM headings WHERE file_id = ?", params![file_id])
            .map_err(|e| format!("deleting old headings: {}", e))?;
        {
            let mut ins = tx
                .prepare(
                    "INSERT INTO headings( \
                         file_id, line, level, keyword, title, tags_json, is_done, priority, fingerprint \
                     ) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .map_err(|e| format!("preparing insert: {}", e))?;
            for t in todos {
                let tags_json =
                    serde_json::to_string(&t.tags).map_err(|e| format!("encoding tags: {}", e))?;
                ins.execute(params![
                    file_id,
                    t.line as i64,
                    t.level as i64,
                    t.keyword,
                    t.title,
                    tags_json,
                    t.is_done as i64,
                    t.priority,
                    t.fingerprint,
                ])
                .map_err(|e| format!("inserting heading: {}", e))?;
            }
        }
        tx.commit()
            .map_err(|e| format!("committing transaction: {}", e))?;
        Ok(())
    }

    /// Delete a file and (via `ON DELETE CASCADE`) all its headings.
    fn remove_file(&mut self, path: &str) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM files WHERE path = ?", params![path])
            .map_err(|e| format!("deleting file row: {}", e))?;
        Ok(())
    }
}

/// Current version of the derived-cache schema.
const SCHEMA_VERSION: i64 = 2;

/// The schema created after dropping any mismatched cache version.
const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS files (
    id               INTEGER PRIMARY KEY,
    path             TEXT UNIQUE NOT NULL,
    canonical_path   TEXT NOT NULL,
    mtime_ns         INTEGER NOT NULL,
    size             INTEGER NOT NULL,
    etag             TEXT NOT NULL,
    todo_active_json TEXT NOT NULL,
    todo_done_json   TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS headings (
    id          INTEGER PRIMARY KEY,
    file_id     INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    line        INTEGER NOT NULL,
    level       INTEGER NOT NULL,
    keyword     TEXT,
    title       TEXT NOT NULL,
    tags_json   TEXT NOT NULL,
    is_done     INTEGER NOT NULL,
    priority    TEXT,
    fingerprint TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_headings_keyword ON headings(keyword);
";

/// Project one coherent file snapshot into the TODO records to index.
///
/// Reuses the shared org primitives: the effective keyword set is the
/// in-buffer `#+TODO:` declaration when present, else the default set
/// (matching `headings.rs`). Only headings that carry a keyword are
/// kept; `is_done` reflects the file's done set.
pub fn records_from_snapshot(path: &str, snapshot: &FileSnapshot) -> Vec<TodoRecord> {
    snapshot
        .headings
        .iter()
        .filter_map(|heading| {
            let keyword = heading.keyword.clone()?;
            let is_done = snapshot.keywords.done.contains(&keyword);
            Some(TodoRecord {
                path: path.to_string(),
                line: heading.line,
                level: heading.level,
                keyword,
                title: heading.title.clone(),
                tags: heading.tags.clone(),
                is_done,
                priority: heading.priority.clone(),
                fingerprint: heading.fingerprint.clone(),
                etag: snapshot.etag.clone(),
                todo_keywords: snapshot.keywords.clone(),
            })
        })
        .collect()
}

/// Escape `%`, `_` and `\` for a LIKE pattern using `\` as the escape
/// character. The bind then wraps the result in `%…%`.
fn escape_like(needle: &str) -> String {
    let mut out = String::with_capacity(needle.len());
    for ch in needle.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Extract a nanosecond-resolution modification fingerprint from file
/// metadata. Falls back to `0` when the platform can't report mtime.
fn mtime_ns_of(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use std::collections::BTreeSet;
    use std::path::Path as StdPath;

    /// Minimal tempdir helper, mirrors the sibling modules'.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            p.push(format!("org-cli-index-test-{}-{}-{}", tag, pid, nanos));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &StdPath {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(p: &StdPath, s: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, s).unwrap();
    }

    /// Open an in-memory store (schema applied) for query/parse tests.
    fn mem_store() -> TodoStore {
        TodoStore::from_connection(Connection::open_in_memory().unwrap()).unwrap()
    }

    #[test]
    fn open_creates_schema_idempotently() {
        let td = TempDir::new("schema");
        let db = td.path().join("nested/dir/todos.sqlite");
        // Parent dirs don't exist yet — open must create them.
        let store = TodoStore::open(&db).unwrap();
        // Re-applying schema on a second open must not error.
        drop(store);
        let store2 = TodoStore::open(&db).unwrap();
        // A query on the empty db returns nothing.
        assert!(store2.query(&TodoQuery::default()).unwrap().is_empty());
    }

    #[test]
    fn open_migrates_old_schema_and_sync_repopulates_new_columns() {
        let td = TempDir::new("migrate");
        let db = td.path().join("todos.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE files (
                 id INTEGER PRIMARY KEY,
                 path TEXT UNIQUE NOT NULL,
                 mtime_ns INTEGER NOT NULL,
                 size INTEGER NOT NULL
             );
             CREATE TABLE headings (
                 id INTEGER PRIMARY KEY,
                 file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 line INTEGER NOT NULL,
                 level INTEGER NOT NULL,
                 keyword TEXT,
                 title TEXT NOT NULL,
                 tags_json TEXT NOT NULL,
                 is_done INTEGER NOT NULL
             );
             PRAGMA user_version = 0;",
        )
        .unwrap();
        drop(conn);

        let org = td.path().join("migrated.org");
        write(
            &org,
            "#+TODO: TODO WIP | DONE CANCELED\n* WIP [#A] Migrated\n",
        );
        let mut store = TodoStore::open(&db).unwrap();
        store.sync(std::slice::from_ref(&org)).unwrap();

        let rows = store.query(&TodoQuery::default()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].priority.as_deref(), Some("A"));
        assert!(!rows[0].fingerprint.is_empty());
        assert!(!rows[0].etag.is_empty());
        assert_eq!(
            rows[0].todo_keywords,
            TodoKeywords {
                active: vec!["TODO".to_string(), "WIP".to_string()],
                done: vec!["DONE".to_string(), "CANCELED".to_string()],
            }
        );
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn sync_indexes_only_keyworded_headings() {
        let td = TempDir::new("only-kw");
        let f = td.path().join("a.org");
        write(
            &f,
            indoc! {"
                * TODO First task :home:
                * Plain heading
                ** DONE Sub done
                * Another plain
            "},
        );
        let mut store = mem_store();
        let events = store.sync(std::slice::from_ref(&f)).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            TodoEvent::FileChanged { todos, .. } => {
                let titles: Vec<&str> = todos.iter().map(|t| t.title.as_str()).collect();
                assert_eq!(titles, vec!["First task", "Sub done"]);
            }
            other => panic!("expected FileChanged, got {:?}", other),
        }
        // The query view agrees: two rows, non-keyword headings excluded.
        let rows = store.query(&TodoQuery::default()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "First task");
        assert_eq!(rows[0].tags, vec!["home".to_string()]);
        assert_eq!(rows[1].title, "Sub done");
    }

    #[test]
    fn sync_skips_unchanged_file_second_run() {
        let td = TempDir::new("skip");
        let f = td.path().join("a.org");
        write(&f, "* TODO A task\n");
        let mut store = mem_store();
        let first = store.sync(std::slice::from_ref(&f)).unwrap();
        assert_eq!(first.len(), 1, "first sync must index the file");
        // Second sync with no changes: empty diff (file not reparsed).
        let second = store.sync(std::slice::from_ref(&f)).unwrap();
        assert!(
            second.is_empty(),
            "unchanged file must not be reparsed; diff={:?}",
            second
        );
    }

    #[test]
    fn canonical_lookup_preserves_indexed_display_path() {
        let td = TempDir::new("canonical-identity");
        let target = td.path().join("target.org");
        let indexed_link = td.path().join("indexed-link.org");
        write(&target, "* TODO Through link\n");
        std::os::unix::fs::symlink("target.org", &indexed_link).unwrap();
        let canonical = fs::canonicalize(&target).unwrap();
        let mut store = mem_store();

        store.sync(std::slice::from_ref(&indexed_link)).unwrap();

        assert_eq!(
            store.indexed_path_for_canonical(&canonical).unwrap(),
            Some(indexed_link.to_string_lossy().to_string())
        );
    }

    #[test]
    fn sync_forcing_reparses_forced_same_fingerprint_file() {
        let td = TempDir::new("force");
        let file = td.path().join("forced.org");
        write(&file, "* TODO AAAA\n");
        let modified = fs::metadata(&file).unwrap().modified().unwrap();
        let mut store = mem_store();
        store.sync(std::slice::from_ref(&file)).unwrap();

        write(&file, "* TODO BBBB\n");
        fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let force = BTreeSet::from([file.clone()]);

        let events = store
            .sync_forcing(std::slice::from_ref(&file), &force)
            .unwrap();

        assert_eq!(events.len(), 1);
        match &events[0] {
            TodoEvent::FileChanged { todos, .. } => {
                assert_eq!(todos.len(), 1);
                assert_eq!(todos[0].title, "BBBB");
            }
            other => panic!("expected FileChanged, got {other:?}"),
        }
        assert_eq!(store.query(&TodoQuery::default()).unwrap()[0].title, "BBBB");
    }

    #[test]
    fn sync_reports_file_changed_on_modification() {
        let td = TempDir::new("modify");
        let f = td.path().join("a.org");
        write(&f, "* TODO Original\n");
        let mut store = mem_store();
        store.sync(std::slice::from_ref(&f)).unwrap();

        // Modify: new content + a size change guarantees a different
        // fingerprint regardless of mtime granularity.
        write(&f, "* TODO Rewritten task with more bytes\n* TODO Second\n");
        let events = store.sync(std::slice::from_ref(&f)).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            TodoEvent::FileChanged { todos, .. } => {
                let titles: Vec<&str> = todos.iter().map(|t| t.title.as_str()).collect();
                assert_eq!(titles, vec!["Rewritten task with more bytes", "Second"]);
            }
            other => panic!("expected FileChanged, got {:?}", other),
        }
        let rows = store.query(&TodoQuery::default()).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn sync_query_and_events_never_pair_records_with_another_snapshot_version() {
        const OLD_ETAG: &str = "8e82111e863d2622d6e143c4f05dd92f091f9a006cd3a88d7a342e88c8256cf0";
        const NEW_ETAG: &str = "4366882759ec106d0b6ce2b5b6a5e2f8339c95f700c4ce78232afe63048803ef";
        const EMPTY_ETAG: &str = "e16db6d89f98537a8ff8a280751cc2a0b37e4f5f1cd0dca983f8c618b5f7a2ec";

        let td = TempDir::new("coherent");
        let file = td.path().join("coherent.org");
        let mut store = mem_store();

        write(&file, "* TODO [#A] Old\n");
        let old_events = store.sync(std::slice::from_ref(&file)).unwrap();
        let old_rows = store.query(&TodoQuery::default()).unwrap();
        let (old_event_etag, old_event_todos, old_event_keywords) = match &old_events[0] {
            TodoEvent::FileChanged {
                etag,
                todo_keywords,
                todos,
                ..
            } => (etag, todos, todo_keywords),
            other => panic!("expected FileChanged, got {other:?}"),
        };
        assert_eq!(old_event_etag, OLD_ETAG);
        assert_eq!(old_event_keywords, &TodoKeywords::default());
        assert_eq!(old_event_todos, &old_rows);
        assert_eq!(old_rows[0].title, "Old");
        assert_eq!(old_rows[0].priority.as_deref(), Some("A"));
        assert_eq!(old_rows[0].etag, OLD_ETAG);
        assert_eq!(old_rows[0].todo_keywords, TodoKeywords::default());

        write(
            &file,
            "#+TODO: TODO WIP | DONE CANCELED\n* WIP [#破] New record with more bytes\n* DONE End\n",
        );
        let new_events = store.sync(std::slice::from_ref(&file)).unwrap();
        let new_rows = store.query(&TodoQuery::default()).unwrap();
        let (new_event_etag, new_event_todos, new_event_keywords) = match &new_events[0] {
            TodoEvent::FileChanged {
                etag,
                todo_keywords,
                todos,
                ..
            } => (etag, todos, todo_keywords),
            other => panic!("expected FileChanged, got {other:?}"),
        };
        let declared = TodoKeywords {
            active: vec!["TODO".to_string(), "WIP".to_string()],
            done: vec!["DONE".to_string(), "CANCELED".to_string()],
        };
        assert_eq!(new_event_etag, NEW_ETAG);
        assert_eq!(new_event_keywords, &declared);
        assert_eq!(new_event_todos, &new_rows);
        assert_eq!(
            new_rows
                .iter()
                .map(|record| record.title.as_str())
                .collect::<Vec<_>>(),
            vec!["New record with more bytes", "End"]
        );
        assert_eq!(new_rows[0].priority.as_deref(), Some("破"));
        assert!(new_rows.iter().all(|record| record.etag == NEW_ETAG));
        assert!(
            new_rows
                .iter()
                .all(|record| record.todo_keywords == declared)
        );
        assert!(old_rows.iter().all(|record| record.etag == OLD_ETAG));
        assert!(old_rows.iter().all(|record| record.title == "Old"));
        assert_ne!(old_event_etag, new_event_etag);

        write(&file, "#+TODO: NEXT | FINISHED\n* Plain heading only\n");
        let empty_events = store.sync(std::slice::from_ref(&file)).unwrap();
        assert!(store.query(&TodoQuery::default()).unwrap().is_empty());
        match &empty_events[0] {
            TodoEvent::FileChanged {
                etag,
                todo_keywords,
                todos,
                ..
            } => {
                assert_eq!(etag, EMPTY_ETAG);
                assert_eq!(
                    todo_keywords,
                    &TodoKeywords {
                        active: vec!["NEXT".to_string()],
                        done: vec!["FINISHED".to_string()],
                    }
                );
                assert!(todos.is_empty());
            }
            other => panic!("expected FileChanged, got {other:?}"),
        }
    }

    #[test]
    fn sync_reports_file_removed_when_absent_from_input() {
        let td = TempDir::new("remove");
        let a = td.path().join("a.org");
        let b = td.path().join("b.org");
        write(&a, "* TODO In A\n");
        write(&b, "* TODO In B\n");
        let mut store = mem_store();
        store.sync(&[a.clone(), b.clone()]).unwrap();

        // Second sync without b: b must be removed.
        let events = store.sync(std::slice::from_ref(&a)).unwrap();
        let removed: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                TodoEvent::FileRemoved { path } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(removed, vec![b.to_string_lossy().to_string().as_str()]);
        let rows = store.query(&TodoQuery::default()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "In A");
    }

    #[test]
    fn sync_removes_unreadable_file() {
        let td = TempDir::new("unreadable");
        let a = td.path().join("a.org");
        write(&a, "* TODO Present\n");
        let mut store = mem_store();
        store.sync(std::slice::from_ref(&a)).unwrap();

        // Delete the file, then sync with it still in the input slice:
        // it becomes unstat-able → pruned as removed.
        fs::remove_file(&a).unwrap();
        let events = store.sync(std::slice::from_ref(&a)).unwrap();
        let removed: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                TodoEvent::FileRemoved { path } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(removed, vec![a.to_string_lossy().to_string().as_str()]);
        assert!(store.query(&TodoQuery::default()).unwrap().is_empty());
    }

    #[test]
    fn query_search_escapes_like_metacharacters() {
        let td = TempDir::new("like");
        let f = td.path().join("a.org");
        write(
            &f,
            indoc! {"
                * TODO Pay 50% deposit
                * TODO Pay half deposit
                * TODO snake_case fix
                * TODO snakeXcase other
            "},
        );
        let mut store = mem_store();
        store.sync(std::slice::from_ref(&f)).unwrap();

        // `%` must match literally, not as a wildcard.
        let pct = store
            .query(&TodoQuery {
                search: Some("50%".to_string()),
                keyword: None,
            })
            .unwrap();
        assert_eq!(pct.len(), 1, "got {:?}", pct);
        assert_eq!(pct[0].title, "Pay 50% deposit");

        // `_` must match literally, not as single-char wildcard: it
        // must NOT match "snakeXcase".
        let under = store
            .query(&TodoQuery {
                search: Some("snake_case".to_string()),
                keyword: None,
            })
            .unwrap();
        assert_eq!(under.len(), 1, "got {:?}", under);
        assert_eq!(under[0].title, "snake_case fix");
    }

    #[test]
    fn query_search_is_case_insensitive() {
        let td = TempDir::new("ci");
        let f = td.path().join("a.org");
        write(&f, "* TODO Buy Milk\n");
        let mut store = mem_store();
        store.sync(std::slice::from_ref(&f)).unwrap();
        let rows = store
            .query(&TodoQuery {
                search: Some("milk".to_string()),
                keyword: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn query_keyword_exact_match() {
        let td = TempDir::new("kw");
        let f = td.path().join("a.org");
        write(
            &f,
            indoc! {"
                * TODO A
                * DONE B
                * TODO C
            "},
        );
        let mut store = mem_store();
        store.sync(std::slice::from_ref(&f)).unwrap();
        let todos = store
            .query(&TodoQuery {
                search: None,
                keyword: Some("TODO".to_string()),
            })
            .unwrap();
        let titles: Vec<&str> = todos.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["A", "C"]);
    }

    #[test]
    fn done_keyword_sets_is_done() {
        let td = TempDir::new("done");
        let f = td.path().join("a.org");
        write(
            &f,
            indoc! {"
                #+TODO: TODO WIP | DONE CANCELED
                * TODO Active
                * WIP Working
                * DONE Finished
                * CANCELED Dropped
            "},
        );
        let mut store = mem_store();
        store.sync(std::slice::from_ref(&f)).unwrap();
        let rows = store.query(&TodoQuery::default()).unwrap();
        let by = |kw: &str| rows.iter().find(|r| r.keyword == kw).unwrap();
        assert!(!by("TODO").is_done);
        assert!(!by("WIP").is_done);
        assert!(by("DONE").is_done);
        assert!(by("CANCELED").is_done);
    }

    #[test]
    fn query_ordered_by_path_then_line() {
        let td = TempDir::new("order");
        let a = td.path().join("a.org");
        let b = td.path().join("b.org");
        write(&b, "* TODO b-one\n* TODO b-two\n");
        write(&a, "* TODO a-one\n* TODO a-two\n");
        let mut store = mem_store();
        // Sync in b, a order — output must still be path-sorted.
        store.sync(&[b.clone(), a.clone()]).unwrap();
        let rows = store.query(&TodoQuery::default()).unwrap();
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["a-one", "a-two", "b-one", "b-two"]);
    }
}
