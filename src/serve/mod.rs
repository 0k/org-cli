//! Thin HTTP + SSE adapter over the [`crate::watch`] diff stream
//! (cargo feature `server`, in default features).
//!
//! # Layering & strippability
//!
//! This is the **only** module that imports `tokio`, `axum`, or
//! `kal-sse`. It is a deliberately thin adapter (~200 lines): all the
//! sync/diff intelligence lives one layer down in [`crate::watch`] and
//! [`crate::index`]. Building with `--no-default-features` drops this
//! module — and the three heavy dependencies — entirely, leaving the
//! pure-sync core usable on its own.
//!
//! # The seam
//!
//! [`crate::watch::TodoWatcher`] delivers applied diffs as
//! `Vec<TodoEvent>` over a [`std::sync::mpsc::Receiver`]. This module
//! bridges that receiver into an `Arc<Mutex<kal_sse::EventLog<TodoEvent>>>`
//! (a dedicated std thread doing `recv()` → `log.lock().append(ev)`)
//! and serves the resilient two-endpoint kal-sse contract over axum.
//!
//! # vigil-watch integration path
//!
//! A host that already owns an async runtime and a `kal-sse` `EventLog`
//! does **not** use this module. It depends on `org-cli` with
//! `default-features = false`, reuses [`crate::index`] +
//! [`crate::watch`] + [`crate::index::TodoEvent`], and wires the
//! watcher's `mpsc::Receiver` straight into its own `EventLog` (via a
//! `spawn_blocking` drain) and its own axum router. This module is the
//! reference for exactly that wiring — copy the bridge thread and the
//! two handlers, drop the runtime bootstrap.
//!
//! # HTTP contract (kal-sse two-endpoint pattern)
//!
//! * `GET /api/todos?search=&keyword=&sub=` — JSON snapshot of the
//!   current TODO set. Registers (or refreshes) the subscription keyed
//!   by `?sub=<uuid>` and returns `x-sub-id` + `x-snapshot-index`
//!   headers. The client reuses the id on the SSE endpoint.
//! * `GET /sse/todos?sub=` — the live diff stream via
//!   [`kal_sse::axum::sse_response`]; a missing / empty `sub` yields
//!   `409 needs_snapshot`, mirroring vigil-watch.

use std::net::SocketAddr;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use kal_sse::EventLog;
use log::{debug, info, warn};
use serde::Deserialize;

use crate::index::{TodoEvent, TodoQuery, TodoRecord, TodoStore};

/// Disconnect-grace TTL for a subscription's resume cursor: 15 min.
const EVENT_TTL: Duration = Duration::from_secs(900);
/// Hard cap on retained events (memory safety valve).
const MAX_EVENTS: usize = 10_000;
/// Live broadcast channel depth for SSE fan-out.
const CHANNEL_CAPACITY: usize = 256;
/// How often the background task reclaims expired subscription handles
/// and trims the retained buffer.
const COMPACT_INTERVAL: Duration = Duration::from_secs(60);
/// Human-readable pointer embedded in the `409 needs_snapshot` body.
const RESYNC_HINT: &str = "resync via GET /api/todos?sub=<uuid>";

/// Shared state handed to every axum handler.
///
/// `log` is the resilient event log the watcher's diffs are appended
/// into. `store` is a **second, read-only** [`TodoStore`] opened at the
/// same sqlite path as the watch loop's writer — sqlite WAL supports
/// one writer plus concurrent readers, so snapshot queries never block
/// the sync path and vice versa. This is cleaner than sharing the
/// writer behind a mutex (which would serialise every query behind
/// every reparse).
#[derive(Clone)]
struct AppState {
    log: Arc<Mutex<EventLog<TodoEvent>>>,
    store: Arc<Mutex<TodoStore>>,
}

/// Query parameters shared by the snapshot and SSE endpoints.
#[derive(Debug, Deserialize)]
struct TodosParams {
    /// Case-insensitive substring filter on the title.
    search: Option<String>,
    /// Exact TODO-keyword filter.
    keyword: Option<String>,
    /// Subscription id (kal-sse `?sub=<uuid>`).
    sub: Option<String>,
}

/// Build the shared [`EventLog`] and spawn the std bridge thread that
/// drains `rx` into it.
///
/// Returns the log handle so the caller can hand it to the axum state.
/// The bridge thread runs until `rx` is closed (i.e. the watcher's
/// `Sender` is dropped), at which point it exits cleanly.
fn spawn_bridge(rx: Receiver<Vec<TodoEvent>>) -> Arc<Mutex<EventLog<TodoEvent>>> {
    let log = Arc::new(Mutex::new(EventLog::new(
        EVENT_TTL,
        MAX_EVENTS,
        CHANNEL_CAPACITY,
    )));
    let bridge_log = Arc::clone(&log);
    std::thread::Builder::new()
        .name("todo-sse-bridge".into())
        .spawn(move || {
            // Each diff batch from the watcher is a Vec<TodoEvent>; we
            // append each event individually so an SSE consumer sees
            // one `data:` line per file change (matching the wire
            // contract's per-event shape).
            for batch in rx.iter() {
                let mut guard = match bridge_log.lock() {
                    Ok(g) => g,
                    Err(_) => {
                        warn!("sse bridge: event-log mutex poisoned, exiting");
                        return;
                    }
                };
                for event in batch {
                    guard.append(event);
                }
            }
            debug!("sse bridge: watcher channel closed, thread exiting");
        })
        .expect("spawning sse bridge thread");
    log
}

/// Build the axum router with its shared state.
fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/api/todos", get(snapshot_handler))
        .route("/sse/todos", get(sse_handler))
        .with_state(state)
}

/// `GET /api/todos` — the REST snapshot half of the resilient channel.
///
/// Registers (or refreshes) the subscription keyed by `?sub=<uuid>`
/// (minting one when absent), pins its cursor at the current event-log
/// index, queries the store, and returns the JSON array plus the
/// `x-sub-id` and `x-snapshot-index` headers. The client then connects
/// `GET /sse/todos?sub=<uuid>` carrying only the id.
async fn snapshot_handler(
    State(state): State<AppState>,
    Query(params): Query<TodosParams>,
) -> Result<(HeaderMap, Json<Vec<TodoRecord>>), StatusCode> {
    // Register on the event log FIRST so the cursor we hand out pins at
    // the current last_index; every diff appended after this point is
    // delivered over the SSE stream, gap-free.
    let (sub_id, idx) = match state.log.lock() {
        Ok(mut log) => log.register(params.sub.clone()),
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    };

    let query = TodoQuery {
        search: params.search.clone(),
        keyword: params.keyword.clone(),
    };
    let records = match state.store.lock() {
        Ok(store) => store.query(&query).map_err(|e| {
            warn!("snapshot query failed: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?,
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    };

    let mut headers = HeaderMap::new();
    if let Ok(v) = idx.to_string().parse() {
        headers.insert("x-snapshot-index", v);
    }
    if let Ok(v) = sub_id.parse() {
        headers.insert("x-sub-id", v);
    }
    Ok((headers, Json(records)))
}

/// `GET /sse/todos` — the live diff stream.
///
/// Serves the backlog-then-live [`TodoEvent`] stream via
/// [`kal_sse::axum::sse_response`], which owns cursor advancement, the
/// disconnect TTL, and `409 needs_snapshot` on an unknown/expired
/// subscription. A missing or empty `sub` is answered `409` up front —
/// the client must snapshot first to register.
///
/// Returns a type-erased [`Response`] so this crate needn't name
/// `sse_response`'s `impl Stream` return (which would pull in
/// `futures_core` as a direct dependency).
async fn sse_handler(State(state): State<AppState>, Query(params): Query<TodosParams>) -> Response {
    let sub_id = match params.sub {
        Some(s) if !s.is_empty() => s,
        _ => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "needs_snapshot",
                    "hint": RESYNC_HINT,
                })),
            )
                .into_response();
        }
    };

    match kal_sse::axum::sse_response(
        Arc::clone(&state.log),
        sub_id,
        |ev: &TodoEvent| serde_json::to_string(ev).unwrap_or_default(),
        RESYNC_HINT,
    ) {
        Ok(sse) => sse.into_response(),
        Err(conflict) => conflict.into_response(),
    }
}

/// Run the daemon's HTTP + SSE server until `shutdown` resolves.
///
/// * `addr` — the socket address to bind.
/// * `store` — a read-only [`TodoStore`] at the daemon's db path (the
///   snapshot query source; NOT the watcher's writer).
/// * `rx` — the watcher's diff receiver; its batches are bridged into
///   the SSE event log.
/// * `shutdown` — a future that, once resolved, triggers graceful
///   shutdown (typically the daemon's Ctrl-C signal).
///
/// This is `async`; the daemon action drives it on a tokio runtime.
pub async fn serve<F>(
    addr: SocketAddr,
    store: TodoStore,
    rx: Receiver<Vec<TodoEvent>>,
    shutdown: F,
) -> Result<(), String>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let log = spawn_bridge(rx);
    let state = AppState {
        log: Arc::clone(&log),
        store: Arc::new(Mutex::new(store)),
    };

    // Periodic compaction: reclaim expired handles + trim the buffer.
    {
        let log = Arc::clone(&log);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(COMPACT_INTERVAL);
            loop {
                ticker.tick().await;
                if let Ok(mut guard) = log.lock() {
                    guard.compact(Instant::now());
                }
            }
        });
    }

    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("binding {}: {}", addr, e))?;
    let bound = listener
        .local_addr()
        .map_err(|e| format!("reading bound address: {}", e))?;
    info!("daemon listening on http://{}", bound);

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| format!("serving http: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// The bridge thread drains diff batches into the event log,
    /// appending one index per event.
    #[test]
    fn bridge_appends_each_event_to_log() {
        let (tx, rx) = mpsc::channel();
        let log = spawn_bridge(rx);

        tx.send(vec![
            TodoEvent::FileChanged {
                path: "/a.org".into(),
                todos: Vec::new(),
            },
            TodoEvent::FileRemoved {
                path: "/b.org".into(),
            },
        ])
        .unwrap();
        // Dropping the sender closes the channel so the bridge finishes.
        drop(tx);

        // Poll (bounded) until the two events land.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let idx = log.lock().unwrap().last_index();
            if idx >= 2 {
                break;
            }
            assert!(Instant::now() < deadline, "bridge never appended events");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(log.lock().unwrap().last_index(), 2);
    }

    /// A `TodoEvent` serialises to the documented tagged wire shape.
    #[test]
    fn todo_event_serialises_to_tagged_shape() {
        let ev = TodoEvent::FileRemoved {
            path: "/x.org".into(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert_eq!(s, r#"{"type":"file-removed","path":"/x.org"}"#);
    }
}
