//! Runtime-agnostic filesystem watcher that keeps a [`TodoStore`]
//! incrementally in sync and delivers the resulting diffs as
//! `Vec<TodoEvent>` over a [`std::sync::mpsc`] channel.
//!
//! # Layering
//!
//! This module sits **between** [`crate::index`] (pure sqlite sync)
//! and [`crate::serve`] (the HTTP+SSE adapter). It depends only on
//! `notify` and `std` — **never** on tokio, axum, or `kal-sse`. That
//! constraint is load-bearing: it is what lets the whole module be
//! lifted verbatim into another daemon (notably `vigil-watch`) that
//! already owns its own async runtime and `kal-sse` `EventLog`. Such a
//! host wires the [`std::sync::mpsc::Receiver`] end into its own event
//! plumbing (a `spawn_blocking` recv loop appending into its
//! `EventLog`) without this module knowing anything about it.
//!
//! # What it does
//!
//! * Watches the **parent directories** of every indexed `.org` file
//!   (non-recursive, de-duplicated). Directory watching survives
//!   emacs' rename-on-save dance, where the real file is briefly
//!   replaced rather than modified in place.
//! * Optionally watches the **agenda-list file**: when it changes, the
//!   effective file set is re-resolved, the store re-synced (producing
//!   `FileChanged` / `FileRemoved` diffs for membership changes), and
//!   the directory watches re-armed.
//! * **Filters** raw notify events to paths ending `.org`
//!   (case-insensitive) and ignores emacs artifacts: lockfiles
//!   (`.#foo.org`), backups (`foo.org~`) and any hidden dotfile.
//! * **Debounces** events per path over a short window so a burst of
//!   fs notifications for one save collapses into a single reparse.
//! * On each debounced batch, calls [`TodoStore::sync`] with the full
//!   current resolved set (the fingerprint check makes untouched files
//!   free) and forwards the returned diff — when non-empty — through
//!   the [`std::sync::mpsc::Sender`].
//!
//! # Lifecycle
//!
//! [`TodoWatcher::run`] blocks the calling thread. A caller runs it on
//! a dedicated `std::thread`. Shutdown is cooperative via a shared
//! [`std::sync::atomic::AtomicBool`]: flip it and the loop returns at
//! the next debounce tick. [`TodoWatcher::shutdown_handle`] hands out
//! the flag so tests (and the daemon's Ctrl-C path) can stop it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use log::{debug, warn};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::cli::action::walk::{read_agenda_set, resolve_input_files};
use crate::index::{TodoEvent, TodoStore};

/// Debounce window: filesystem events for a given path are coalesced
/// over this span before a reparse is triggered. Short enough to feel
/// instant, long enough to absorb the multi-event burst a single
/// editor save produces.
pub const DEBOUNCE: Duration = Duration::from_millis(200);

/// A cooperative shutdown flag shared between the watch loop and its
/// controller. Cloneable `Arc` wrapper around an [`AtomicBool`].
#[derive(Clone, Default)]
pub struct Shutdown(Arc<AtomicBool>);

impl Shutdown {
    /// Create a fresh, un-tripped shutdown flag.
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Request the watch loop to stop. It returns at the next
    /// debounce tick.
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether a stop has been requested.
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// A filesystem watcher that keeps a [`TodoStore`] in sync and streams
/// the applied diffs as `Vec<TodoEvent>`.
///
/// The watcher **owns** the store: [`TodoWatcher::run`] is the sole
/// mutator, so no locking is required on the sync path. Construct it
/// with [`TodoWatcher::new`], then move it onto a thread and call
/// [`TodoWatcher::run`].
pub struct TodoWatcher {
    /// The store this watcher keeps in sync.
    store: TodoStore,
    /// The explicit input paths the user passed (files or dirs), used
    /// to re-resolve the set. Empty when running off the agenda file.
    inputs: Vec<PathBuf>,
    /// The agenda-list file to watch for membership changes, when the
    /// set was resolved from the agenda (i.e. `inputs` was empty).
    agenda_path: Option<PathBuf>,
    /// The current resolved `.org` file set (absolute-ish paths as
    /// produced by `resolve_input_files`).
    files: Vec<PathBuf>,
    /// Sink for applied diffs.
    events_tx: Sender<Vec<TodoEvent>>,
    /// Cooperative shutdown flag.
    shutdown: Shutdown,
}

impl TodoWatcher {
    /// Build a watcher over an already-open, already-synced `store`.
    ///
    /// * `store` — the index to keep in sync (the caller has performed
    ///   the initial full sync).
    /// * `inputs` — the explicit paths the user gave. Empty means "use
    ///   the agenda set"; pass `agenda_path` in that case.
    /// * `agenda_path` — when `inputs` is empty, the agenda-list file
    ///   to watch for membership changes. Ignored (and need not exist)
    ///   when `inputs` is non-empty.
    /// * `files` — the already-resolved current `.org` file set (from
    ///   [`resolve_input_files`]).
    /// * `events_tx` — the sink diffs are forwarded to.
    pub fn new(
        store: TodoStore,
        inputs: Vec<PathBuf>,
        agenda_path: Option<PathBuf>,
        files: Vec<PathBuf>,
        events_tx: Sender<Vec<TodoEvent>>,
    ) -> Self {
        Self {
            store,
            inputs,
            agenda_path,
            files,
            events_tx,
            shutdown: Shutdown::new(),
        }
    }

    /// Hand out the cooperative shutdown flag so a controller (test or
    /// daemon Ctrl-C handler) can stop the [`run`](Self::run) loop.
    pub fn shutdown_handle(&self) -> Shutdown {
        self.shutdown.clone()
    }

    /// Run the blocking watch loop until [`shutdown`](Shutdown::stop)
    /// is requested.
    ///
    /// Arms directory watches (plus the agenda file, if any), then
    /// loops: drain debounced fs events, reparse only touched files
    /// via [`TodoStore::sync`], forward any non-empty diff. Returns
    /// `Ok(())` on clean shutdown, or an error string if the initial
    /// watch arming failed.
    pub fn run(mut self) -> Result<(), String> {
        // The notify watcher forwards raw events into this std channel;
        // the loop below debounces them. The watcher must outlive the
        // loop, so it is bound here and moved through re-arming.
        let (raw_tx, raw_rx) = mpsc::channel::<Event>();
        let mut watcher = build_watcher(raw_tx.clone())?;
        self.arm(&mut watcher)?;

        // Pending debounce set: paths touched since the last flush,
        // and the deadline at which the batch fires.
        let mut pending: BTreeSet<PathBuf> = BTreeSet::new();
        let mut deadline: Option<Instant> = None;
        // Set when the agenda file itself changed and the whole set
        // must be re-resolved on the next flush.
        let mut agenda_dirty = false;

        loop {
            if self.shutdown.is_stopped() {
                debug!("watch loop: shutdown requested, exiting");
                return Ok(());
            }

            // Block until the next event or the debounce deadline. When
            // no batch is pending, use a bounded poll so the shutdown
            // flag is still checked promptly.
            let wait = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                None => DEBOUNCE,
            };

            match raw_rx.recv_timeout(wait) {
                Ok(event) => {
                    self.classify(&event, &mut pending, &mut agenda_dirty);
                    // (Re)start the debounce clock on the first event
                    // of a batch; subsequent events extend nothing —
                    // the fixed window keeps the batch bounded.
                    if deadline.is_none() && (!pending.is_empty() || agenda_dirty) {
                        deadline = Some(Instant::now() + DEBOUNCE);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Either the debounce window elapsed (flush) or an
                    // idle poll tick (nothing pending → loop).
                    if deadline.is_some_and(|d| Instant::now() >= d) {
                        deadline = None;
                        let re_resolve = std::mem::take(&mut agenda_dirty);
                        let touched = std::mem::take(&mut pending);
                        self.flush(&mut watcher, re_resolve, touched)?;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    warn!("watch loop: notify channel disconnected, exiting");
                    return Ok(());
                }
            }
        }
    }

    /// Classify one raw notify event into the pending debounce set.
    ///
    /// The agenda file (if watched) is recognised first; any other
    /// event whose paths pass the `.org` + artifact filter marks those
    /// paths for reparse.
    fn classify(&self, event: &Event, pending: &mut BTreeSet<PathBuf>, agenda_dirty: &mut bool) {
        for path in &event.paths {
            if self.agenda_path.as_deref() == Some(path.as_path()) {
                debug!("watch: agenda file changed: {}", path.display());
                *agenda_dirty = true;
                continue;
            }
            if is_relevant_org_path(path) {
                debug!("watch: org path touched: {}", path.display());
                pending.insert(path.clone());
            }
        }
    }

    /// Apply one debounced batch: optionally re-resolve the set (and
    /// re-arm watches) when the agenda changed, then sync and forward
    /// the diff.
    fn flush(
        &mut self,
        watcher: &mut RecommendedWatcher,
        re_resolve: bool,
        _touched: BTreeSet<PathBuf>,
    ) -> Result<(), String> {
        if re_resolve {
            // Membership may have changed: re-resolve and re-arm.
            match resolve_input_files(&self.inputs, &[]) {
                Ok(files) => {
                    self.files = files;
                    if let Err(e) = self.arm(watcher) {
                        warn!("watch: re-arming after agenda change failed: {}", e);
                    }
                }
                Err(e) => warn!("watch: re-resolving agenda set failed: {}", e),
            }
        }

        // Sync the FULL current set: the fingerprint check keeps
        // untouched files free, and passing the full set is what lets
        // `sync` emit `FileRemoved` for anything that dropped out of
        // the membership.
        let diff = self.store.sync(&self.files)?;
        if !diff.is_empty() {
            debug!("watch: sync produced {} event(s)", diff.len());
            // A closed receiver means the consumer is gone; treat it as
            // a shutdown signal rather than an error.
            if self.events_tx.send(diff).is_err() {
                debug!("watch: events receiver dropped, requesting shutdown");
                self.shutdown.stop();
            }
        }
        Ok(())
    }

    /// Arm (or re-arm) directory watches for the current file set plus
    /// the agenda file. Best-effort per path: a single unwatchable
    /// directory is logged and skipped rather than aborting.
    fn arm(&self, watcher: &mut RecommendedWatcher) -> Result<(), String> {
        for dir in parent_dirs(&self.files) {
            if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                warn!("watch: cannot watch dir {}: {}", dir.display(), e);
            }
        }
        if let Some(agenda) = &self.agenda_path {
            // Watch the agenda file's parent directory (non-recursive):
            // watching the file directly misses the rename-on-save that
            // regenerating the agenda list performs.
            if let Some(parent) = agenda.parent()
                && !parent.as_os_str().is_empty()
                && let Err(e) = watcher.watch(parent, RecursiveMode::NonRecursive)
            {
                warn!("watch: cannot watch agenda dir {}: {}", parent.display(), e);
            }
        }
        Ok(())
    }
}

/// Build a `notify` watcher whose event handler forwards successful
/// events into `tx`, dropping errors after a log line.
fn build_watcher(tx: Sender<Event>) -> Result<RecommendedWatcher, String> {
    notify::recommended_watcher(move |res: notify::Result<Event>| match res {
        Ok(event) => {
            let _ = tx.send(event);
        }
        Err(e) => warn!("notify error: {}", e),
    })
    .map_err(|e| format!("creating filesystem watcher: {}", e))
}

/// The de-duplicated, sorted set of parent directories of `files`.
fn parent_dirs(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut set: BTreeSet<PathBuf> = BTreeSet::new();
    for f in files {
        if let Some(parent) = f.parent()
            && !parent.as_os_str().is_empty()
        {
            set.insert(parent.to_path_buf());
        }
    }
    set.into_iter().collect()
}

/// Whether a filesystem path should trigger a reparse: a `.org` file
/// (case-insensitive extension) that is NOT an emacs artifact.
///
/// Rejected artifacts, by basename:
/// * lockfiles — start with `.#` (e.g. `.#notes.org`),
/// * backups — end with `~` (e.g. `notes.org~`),
/// * any hidden dotfile — starts with `.` (covers autosave `.#`
///   lockfiles too, and editor scratch files).
fn is_relevant_org_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.starts_with('.') || name.ends_with('~') {
        return false;
    }
    path.extension()
        .map(|e| e.eq_ignore_ascii_case("org"))
        .unwrap_or(false)
}

/// Read the agenda file and return its listed paths (existing or not).
///
/// Thin re-export of [`read_agenda_set`] kept here so the daemon action
/// can decide whether the agenda file is worth watching without
/// reaching into `walk`.
pub fn agenda_entries(path: &Path) -> Result<Vec<PathBuf>, String> {
    read_agenda_set(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc::{Receiver, RecvTimeoutError};

    /// Bounded recv used everywhere in tests so a missed event fails
    /// fast instead of hanging the suite.
    const TEST_RECV: Duration = Duration::from_secs(5);

    /// Minimal tempdir helper mirroring the sibling modules'.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            p.push(format!("org-cli-watch-test-{}-{}-{}", tag, pid, nanos));
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

    // ---- pure-function unit tests (no fs threads) ----------------

    #[test]
    fn ignore_filter_accepts_plain_org() {
        assert!(is_relevant_org_path(Path::new("/x/notes.org")));
        assert!(is_relevant_org_path(Path::new("/x/UPPER.ORG")));
    }

    #[test]
    fn ignore_filter_rejects_lockfiles_backups_hidden_and_nonorg() {
        // Emacs lockfile.
        assert!(!is_relevant_org_path(Path::new("/x/.#notes.org")));
        // Backup.
        assert!(!is_relevant_org_path(Path::new("/x/notes.org~")));
        // Hidden dotfile.
        assert!(!is_relevant_org_path(Path::new("/x/.notes.org")));
        // Non-org extension.
        assert!(!is_relevant_org_path(Path::new("/x/notes.txt")));
        // No extension.
        assert!(!is_relevant_org_path(Path::new("/x/notes")));
    }

    #[test]
    fn parent_dirs_dedups_and_sorts() {
        let files = vec![
            PathBuf::from("/a/b/one.org"),
            PathBuf::from("/a/b/two.org"),
            PathBuf::from("/a/c/three.org"),
        ];
        let dirs = parent_dirs(&files);
        assert_eq!(
            dirs,
            vec![PathBuf::from("/a/b"), PathBuf::from("/a/c")],
            "parents must be de-duplicated and sorted"
        );
    }

    // ---- classify / debounce (no real fs, synthetic events) ------

    fn event_for(path: &Path) -> Event {
        Event {
            kind: notify::EventKind::Modify(notify::event::ModifyKind::Any),
            paths: vec![path.to_path_buf()],
            attrs: Default::default(),
        }
    }

    fn watcher_for(
        files: Vec<PathBuf>,
        agenda: Option<PathBuf>,
    ) -> (TodoWatcher, Receiver<Vec<TodoEvent>>) {
        let store = TodoStore::open(&std::env::temp_dir().join(format!(
                "org-cli-watch-classify-{}-{}.sqlite",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            )))
        .unwrap();
        let (tx, rx) = mpsc::channel();
        (TodoWatcher::new(store, Vec::new(), agenda, files, tx), rx)
    }

    #[test]
    fn classify_marks_org_paths_and_ignores_artifacts() {
        let (w, _rx) = watcher_for(vec![PathBuf::from("/a/notes.org")], None);
        let mut pending = BTreeSet::new();
        let mut agenda_dirty = false;

        w.classify(
            &event_for(Path::new("/a/notes.org")),
            &mut pending,
            &mut agenda_dirty,
        );
        w.classify(
            &event_for(Path::new("/a/.#notes.org")),
            &mut pending,
            &mut agenda_dirty,
        );
        w.classify(
            &event_for(Path::new("/a/notes.org~")),
            &mut pending,
            &mut agenda_dirty,
        );

        assert_eq!(
            pending.into_iter().collect::<Vec<_>>(),
            vec![PathBuf::from("/a/notes.org")],
            "only the real .org path is marked; artifacts filtered"
        );
        assert!(!agenda_dirty);
    }

    #[test]
    fn classify_recognises_agenda_file() {
        let agenda = PathBuf::from("/cfg/agenda.tmp");
        let (w, _rx) = watcher_for(Vec::new(), Some(agenda.clone()));
        let mut pending = BTreeSet::new();
        let mut agenda_dirty = false;

        w.classify(&event_for(&agenda), &mut pending, &mut agenda_dirty);

        assert!(agenda_dirty, "agenda file change must set the dirty flag");
        assert!(pending.is_empty());
    }

    #[test]
    fn classify_coalesces_repeated_events_for_same_path() {
        let (w, _rx) = watcher_for(vec![PathBuf::from("/a/notes.org")], None);
        let mut pending = BTreeSet::new();
        let mut agenda_dirty = false;
        // Three raw events for one file collapse to a single pending
        // entry — the debounce set is a `BTreeSet`.
        for _ in 0..3 {
            w.classify(
                &event_for(Path::new("/a/notes.org")),
                &mut pending,
                &mut agenda_dirty,
            );
        }
        assert_eq!(pending.len(), 1, "repeated events coalesce to one");
    }

    // ---- end-to-end: real fs, real notify, real sync -------------

    fn write(p: &Path, s: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, s).unwrap();
    }

    #[test]
    fn live_edit_produces_file_changed_diff() {
        let td = TempDir::new("live-edit");
        let f = td.path().join("a.org");
        write(&f, "* TODO Original\n");

        let files = vec![f.clone()];
        let mut store = TodoStore::open(&td.path().join("idx.sqlite")).unwrap();
        store.sync(&files).unwrap(); // initial sync

        let (tx, rx) = mpsc::channel();
        let watcher = TodoWatcher::new(store, files, None, vec![f.clone()], tx);
        let shutdown = watcher.shutdown_handle();
        let handle = std::thread::spawn(move || watcher.run());

        // Give the watcher a moment to arm its directory watch, then
        // modify the file (size change guarantees a new fingerprint).
        std::thread::sleep(Duration::from_millis(300));
        write(&f, "* TODO Rewritten with more bytes\n* TODO Second\n");

        let diff = rx.recv_timeout(TEST_RECV).expect("expected a diff event");
        shutdown.stop();
        let _ = handle.join();

        // The batch reports the file changed with its full new TODO set.
        let changed: Vec<&str> = diff
            .iter()
            .flat_map(|e| match e {
                TodoEvent::FileChanged { todos, .. } => {
                    todos.iter().map(|t| t.title.as_str()).collect::<Vec<_>>()
                }
                TodoEvent::FileRemoved { .. } => Vec::new(),
            })
            .collect();
        assert_eq!(
            changed,
            vec!["Rewritten with more bytes", "Second"],
            "diff must carry the reparsed TODO set; got {:?}",
            diff
        );
    }

    #[test]
    fn new_org_file_in_watched_dir_is_indexed() {
        let td = TempDir::new("new-file");
        let a = td.path().join("a.org");
        write(&a, "* TODO In A\n");

        // The set includes a not-yet-existing sibling so its parent dir
        // is watched and its creation is caught by the full-set sync.
        let b = td.path().join("b.org");
        let files = vec![a.clone(), b.clone()];
        let mut store = TodoStore::open(&td.path().join("idx.sqlite")).unwrap();
        store.sync(std::slice::from_ref(&a)).unwrap();

        let (tx, rx) = mpsc::channel();
        let watcher = TodoWatcher::new(store, files.clone(), None, files, tx);
        let shutdown = watcher.shutdown_handle();
        let handle = std::thread::spawn(move || watcher.run());

        std::thread::sleep(Duration::from_millis(300));
        write(&b, "* TODO In B\n");

        // Poll until b shows up (bounded): the first diff might only
        // carry unrelated churn on slow CI.
        let deadline = Instant::now() + TEST_RECV;
        let mut saw_b = false;
        while Instant::now() < deadline && !saw_b {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(diff) => {
                    saw_b = diff.iter().any(|e| match e {
                        TodoEvent::FileChanged { todos, .. } => {
                            todos.iter().any(|t| t.title == "In B")
                        }
                        _ => false,
                    });
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        shutdown.stop();
        let _ = handle.join();
        assert!(saw_b, "creating b.org must yield a FileChanged for 'In B'");
    }

    #[test]
    fn shutdown_stops_the_run_loop() {
        let td = TempDir::new("shutdown");
        let f = td.path().join("a.org");
        write(&f, "* TODO A\n");
        let files = vec![f.clone()];
        let mut store = TodoStore::open(&td.path().join("idx.sqlite")).unwrap();
        store.sync(&files).unwrap();

        let (tx, _rx) = mpsc::channel();
        let watcher = TodoWatcher::new(store, files.clone(), None, files, tx);
        let shutdown = watcher.shutdown_handle();
        let handle = std::thread::spawn(move || watcher.run());

        shutdown.stop();
        // The loop must return within a couple of debounce windows.
        let joined = handle.join();
        assert!(joined.is_ok(), "run loop must exit cleanly on shutdown");
        assert!(joined.unwrap().is_ok());
    }
}
