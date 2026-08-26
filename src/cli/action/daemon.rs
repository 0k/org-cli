//! `daemon` subcommand — long-running TODO index with an HTTP + SSE
//! serving surface (cargo feature `server`).
//!
//! Orchestration only. It resolves the input file set (agenda fallback
//! via [`crate::cli::action::walk::resolve_input_files`]), opens the
//! sqlite index, performs the initial full sync, then:
//!
//! 1. Starts the [`crate::watch::TodoWatcher`] on a dedicated
//!    `std::thread` (owning the writer store; diffs flow out over an
//!    `mpsc` channel).
//! 2. Opens a **second, read-only** [`crate::index::TodoStore`] at the
//!    same db path for snapshot queries (sqlite WAL: one writer +
//!    concurrent readers).
//! 3. Builds a tokio runtime and runs the axum server
//!    ([`crate::serve::serve`]), bridging the watcher's diff channel
//!    into the SSE event log. Ctrl-C triggers graceful shutdown.
//!
//! Status is logged exclusively through the `log` crate (fern is wired
//! by [`crate::cli::run`]); nothing is printed to stdout/stderr
//! directly.

use std::net::SocketAddr;
use std::sync::mpsc;

use log::{info, warn};

use crate::cli::action::walk::{default_agenda_set_path, resolve_input_files};
use crate::config::Config;
use crate::index::{TodoEvent, TodoStore};
use crate::watch::TodoWatcher;

/// CLI options for `daemon`.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
    /// Files or directories to watch. Empty → agenda set (and the
    /// agenda file itself is watched for membership changes).
    pub paths: Vec<std::path::PathBuf>,
    /// Bind address override. When `None`, falls back to config
    /// `todos: serve-addr:` then the built-in default.
    pub addr: Option<String>,
    /// Explicit index path, overriding config + default.
    pub db: Option<std::path::PathBuf>,
    /// Permit mutation exposure on a non-loopback bind.
    ///
    /// This is an exposure guard, not authorisation. A local reverse proxy or
    /// tunnel bypasses the bind-address check; real remote safety requires
    /// authentication.
    pub allow_remote_writes: bool,
}

/// Run the daemon. Blocks until Ctrl-C (or a fatal error). Returns 0
/// on clean shutdown.
pub fn run(opts: DaemonOptions) -> Result<i32, String> {
    let cfg = Config::load_default()?;

    let db_path = match &opts.db {
        Some(p) => p.clone(),
        None => cfg.todos.resolve_db_path()?,
    };
    let addr_str = opts
        .addr
        .clone()
        .unwrap_or_else(|| cfg.todos.resolve_serve_addr());
    let addr: SocketAddr = addr_str
        .parse()
        .map_err(|e| format!("invalid serve address {:?}: {}", addr_str, e))?;

    // Resolve the effective file set. When no explicit paths were
    // given, we ALSO track the agenda-list file so membership changes
    // re-resolve the set at runtime.
    let files = resolve_input_files(&opts.paths, &[])?;
    let agenda_path = if opts.paths.is_empty() {
        match default_agenda_set_path() {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(
                    "cannot resolve agenda file path, membership watch disabled: {}",
                    e
                );
                None
            }
        }
    } else {
        None
    };

    // Open the writer store and perform the initial full sync.
    let mut store = TodoStore::open(&db_path)?;
    let initial = store.sync(&files)?;
    info!(
        "daemon: indexed {} file(s) at startup ({} initial diff event(s)); db {}",
        files.len(),
        initial.len(),
        db_path.display()
    );

    // The channel carrying applied diffs from the watch loop to the
    // SSE bridge.
    let (events_tx, events_rx) = mpsc::channel::<Vec<TodoEvent>>();

    // Start the watcher on its own thread; it owns the writer store.
    let watcher = TodoWatcher::new(
        store,
        opts.paths.clone(),
        agenda_path,
        files.clone(),
        events_tx,
    );
    let shutdown = watcher.shutdown_handle();
    let watch_handle = std::thread::Builder::new()
        .name("todo-watch".into())
        .spawn(move || {
            if let Err(e) = watcher.run() {
                warn!("watch loop terminated with error: {}", e);
            }
        })
        .map_err(|e| format!("spawning watch thread: {}", e))?;

    // A second, read-only store at the same path for snapshot queries.
    let read_store = TodoStore::open(&db_path)?;

    // Build a multi-thread tokio runtime and run the server. Ctrl-C
    // resolves the graceful-shutdown future.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("building tokio runtime: {}", e))?;

    info!("daemon: starting HTTP+SSE server on {}", addr);
    let serve_result = runtime.block_on(async move {
        crate::serve::serve(
            addr,
            read_store,
            events_rx,
            opts.allow_remote_writes,
            async {
                match tokio::signal::ctrl_c().await {
                    Ok(()) => info!("daemon: Ctrl-C received, shutting down"),
                    Err(e) => warn!("daemon: failed to listen for Ctrl-C: {}", e),
                }
            },
        )
        .await
    });

    // Stop the watch loop and join it before returning.
    shutdown.stop();
    let _ = watch_handle.join();

    serve_result.map(|()| 0)
}
