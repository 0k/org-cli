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
mod locks;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, patch};
use kal_sse::EventLog;
use log::{debug, info, warn};
use serde::{Deserialize, Deserializer, Serialize};

use crate::index::{TodoEvent, TodoQuery, TodoRecord, TodoStore, records_from_snapshot};
use crate::org::heading_edit::{set_keyword, set_priority, write_atomic};
use crate::org::snapshot::FileSnapshot;
use crate::org::todo_keywords::TodoKeywords;
use locks::FileLocks;

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
/// Explanation returned when the exposure guard disables mutations.
const REMOTE_WRITES_DISABLED: &str = "writes are disabled for non-loopback binds; restart with --allow-remote-writes to expose mutation (this is not authentication)";

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
    locks: FileLocks,
    writes_enabled: bool,
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

/// A tri-state mutation field: omitted, explicit removal, or a new value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Patch<T> {
    /// The request omitted this field.
    #[default]
    Unchanged,
    /// The request supplied JSON `null`.
    Remove,
    /// The request supplied a concrete value.
    Set(T),
}

impl<'de, T> Deserialize<'de> for Patch<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).map(|value| match value {
            Some(value) => Self::Set(value),
            None => Self::Remove,
        })
    }
}

fn deserialize_priority_patch<'de, D>(deserializer: D) -> Result<Patch<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Patch::<String>::deserialize(deserializer).map(|patch| match patch {
        Patch::Set(priority) if priority == "none" => Patch::Remove,
        other => other,
    })
}

/// Snapshot-scoped heading mutation request.
#[derive(Debug, Deserialize)]
struct HeadingPatchRequest {
    path: PathBuf,
    line: usize,
    fingerprint: String,
    #[serde(default)]
    keyword: Patch<String>,
    #[serde(default, deserialize_with = "deserialize_priority_patch")]
    priority: Patch<String>,
}

/// File-wide state returned after a successful or stale mutation.
#[derive(Debug, Serialize)]
struct HeadingPatchResponse {
    path: String,
    etag: String,
    todo_keywords: TodoKeywords,
    todos: Vec<TodoRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IfMatchError {
    Missing,
    Malformed,
}

fn parse_if_match(headers: &HeaderMap) -> Result<String, IfMatchError> {
    let values = headers.get_all("if-match");
    let mut values = values.iter();
    let value = values.next().ok_or(IfMatchError::Missing)?;
    if values.next().is_some() {
        return Err(IfMatchError::Malformed);
    }
    let raw = value.to_str().map_err(|_| IfMatchError::Malformed)?.trim();
    if raw.is_empty() || raw == "*" || raw.starts_with("W/") || raw.contains(',') {
        return Err(IfMatchError::Malformed);
    }

    let tag = if raw.starts_with('"') || raw.ends_with('"') {
        raw.strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .filter(|value| !value.is_empty() && !value.contains('"'))
            .ok_or(IfMatchError::Malformed)?
    } else {
        if raw.chars().any(char::is_whitespace) || raw.contains('"') {
            return Err(IfMatchError::Malformed);
        }
        raw
    };
    Ok(tag.to_string())
}

fn writes_allowed(addr: SocketAddr, allow_remote_writes: bool) -> bool {
    addr.ip().is_loopback() || allow_remote_writes
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
        .route("/api/heading", patch(heading_patch_handler))
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

/// `PATCH /api/heading` — mutate one snapshot-scoped heading locator.
async fn heading_patch_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !state.writes_enabled {
        return (StatusCode::FORBIDDEN, REMOTE_WRITES_DISABLED).into_response();
    }

    let if_match = match parse_if_match(&headers) {
        Ok(if_match) => if_match,
        Err(IfMatchError::Missing) => {
            return (
                StatusCode::PRECONDITION_REQUIRED,
                "missing required If-Match header",
            )
                .into_response();
        }
        Err(IfMatchError::Malformed) => {
            return (StatusCode::BAD_REQUEST, "malformed If-Match header").into_response();
        }
    };

    let request: HeadingPatchRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid JSON body: {error}"),
            )
                .into_response();
        }
    };
    let canonical = match std::fs::canonicalize(&request.path) {
        Ok(canonical) => canonical,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let file_lock = match state.locks.lock_for(&canonical) {
        Ok(lock) => lock,
        Err(error) => {
            warn!("heading mutation lock lookup failed: {}", error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let _file_guard = file_lock.lock().await;

    let indexed_path = match state.store.lock() {
        Ok(store) => match store.indexed_path_for_canonical(&canonical) {
            Ok(Some(path)) => path,
            Ok(None) => return StatusCode::NOT_FOUND.into_response(),
            Err(error) => {
                warn!("heading mutation membership query failed: {}", error);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        },
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let content = match std::fs::read_to_string(&canonical) {
        Ok(content) => content,
        Err(error) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("reading \"{}\": {error}", canonical.display()),
            )
                .into_response();
        }
    };
    let snapshot = FileSnapshot::from_content(&content, &TodoKeywords::default());

    if snapshot.etag != if_match {
        return heading_snapshot_response(StatusCode::PRECONDITION_FAILED, indexed_path, snapshot);
    }

    let locator_matches = snapshot
        .headings
        .iter()
        .any(|heading| heading.line == request.line && heading.fingerprint == request.fingerprint);
    if !locator_matches {
        return (
            StatusCode::CONFLICT,
            "heading locator does not match current file",
        )
            .into_response();
    }
    if request.keyword == Patch::Unchanged && request.priority == Patch::Unchanged {
        return (StatusCode::UNPROCESSABLE_ENTITY, "empty heading patch").into_response();
    }

    let mut new_content = content.clone();
    let apply_result = (|| {
        match &request.priority {
            Patch::Unchanged => {}
            Patch::Remove => {
                new_content = set_priority(&new_content, request.line, None, &snapshot.keywords)?.0;
            }
            Patch::Set(priority) => {
                new_content = set_priority(
                    &new_content,
                    request.line,
                    Some(priority),
                    &snapshot.keywords,
                )?
                .0;
            }
        }
        match &request.keyword {
            Patch::Unchanged => {}
            Patch::Remove => {
                new_content = set_keyword(&new_content, request.line, None, &snapshot.keywords)?.0;
            }
            Patch::Set(keyword) => {
                new_content = set_keyword(
                    &new_content,
                    request.line,
                    Some(keyword),
                    &snapshot.keywords,
                )?
                .0;
            }
        }
        Ok::<(), crate::org::heading_edit::EditError>(())
    })();
    if let Err(error) = apply_result {
        return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response();
    }

    if new_content == content {
        return heading_snapshot_response(StatusCode::OK, indexed_path, snapshot);
    }

    if let Err(error) = write_atomic(&canonical, &new_content) {
        return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response();
    }
    let new_snapshot = FileSnapshot::from_content(&new_content, &TodoKeywords::default());
    heading_snapshot_response(StatusCode::OK, indexed_path, new_snapshot)
}

fn heading_snapshot_response(
    status: StatusCode,
    indexed_path: String,
    snapshot: FileSnapshot,
) -> Response {
    let etag_header = format!("\"{}\"", snapshot.etag);
    let todos = records_from_snapshot(&indexed_path, &snapshot);
    let payload = HeadingPatchResponse {
        path: indexed_path,
        etag: snapshot.etag,
        todo_keywords: snapshot.keywords,
        todos,
    };
    let mut response = (status, Json(payload)).into_response();
    let value = match HeaderValue::from_str(&etag_header) {
        Ok(value) => value,
        Err(error) => {
            warn!("cannot encode snapshot ETag response header: {}", error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    response.headers_mut().insert("etag", value);
    response
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
/// * `allow_remote_writes` — exposure override for non-loopback binds.
///   This is not authorisation: local proxies and tunnels can bypass a bind
///   check, so authenticated deployments need authentication in front.
/// * `shutdown` — a future that, once resolved, triggers graceful
///   shutdown (typically the daemon's Ctrl-C signal).
///
/// This is `async`; the daemon action drives it on a tokio runtime.
pub async fn serve<F>(
    addr: SocketAddr,
    store: TodoStore,
    rx: Receiver<Vec<TodoEvent>>,
    allow_remote_writes: bool,
    shutdown: F,
) -> Result<(), String>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let log = spawn_bridge(rx);
    let state = AppState {
        log: Arc::clone(&log),
        store: Arc::new(Mutex::new(store)),
        locks: FileLocks::default(),
        writes_enabled: writes_allowed(addr, allow_remote_writes),
    };

    if !addr.ip().is_loopback() && allow_remote_writes {
        warn!(
            "remote heading writes enabled on {}; this exposure guard is not authorization, and authenticated deployments still require authentication",
            addr
        );
    }

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
    use axum::http::HeaderValue;
    use serde_json::json;
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
                etag: "etag".into(),
                todo_keywords: crate::org::todo_keywords::TodoKeywords::default(),
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

    #[test]
    fn patch_fields_deserialize_absent_null_and_value() {
        let absent: HeadingPatchRequest = serde_json::from_value(json!({
            "path": "/tmp/indexed.org",
            "line": 12,
            "fingerprint": "abc"
        }))
        .unwrap();
        assert_eq!(absent.keyword, Patch::Unchanged);
        assert_eq!(absent.priority, Patch::Unchanged);

        let removed: HeadingPatchRequest = serde_json::from_value(json!({
            "path": "/tmp/indexed.org",
            "line": 12,
            "fingerprint": "abc",
            "keyword": null,
            "priority": null
        }))
        .unwrap();
        assert_eq!(removed.keyword, Patch::Remove);
        assert_eq!(removed.priority, Patch::Remove);

        let set: HeadingPatchRequest = serde_json::from_value(json!({
            "path": "/tmp/indexed.org",
            "line": 12,
            "fingerprint": "abc",
            "keyword": "none",
            "priority": "B"
        }))
        .unwrap();
        assert_eq!(set.keyword, Patch::Set("none".to_string()));
        assert_eq!(set.priority, Patch::Set("B".to_string()));

        let priority_none: HeadingPatchRequest = serde_json::from_value(json!({
            "path": "/tmp/indexed.org",
            "line": 12,
            "fingerprint": "abc",
            "priority": "none"
        }))
        .unwrap();
        assert_eq!(priority_none.keyword, Patch::Unchanged);
        assert_eq!(priority_none.priority, Patch::Remove);
    }

    #[test]
    fn if_match_parser_accepts_single_strong_quoted_or_bare_tag() {
        for (raw, expected) in [("\"abc\"", "abc"), ("abc", "abc")] {
            let mut headers = HeaderMap::new();
            headers.insert("if-match", HeaderValue::from_str(raw).unwrap());
            assert_eq!(parse_if_match(&headers), Ok(expected.to_string()));
        }

        assert_eq!(
            parse_if_match(&HeaderMap::new()),
            Err(IfMatchError::Missing)
        );
        for raw in ["", "*", "W/\"abc\"", "a, b", "\"\""] {
            let mut headers = HeaderMap::new();
            headers.insert("if-match", HeaderValue::from_str(raw).unwrap());
            assert_eq!(
                parse_if_match(&headers),
                Err(IfMatchError::Malformed),
                "If-Match {raw:?}"
            );
        }
    }

    #[test]
    fn write_policy_requires_opt_in_for_non_loopback_bind() {
        let loopback: SocketAddr = "127.0.0.1:7480".parse().unwrap();
        let non_loopback: SocketAddr = "0.0.0.0:7480".parse().unwrap();

        assert!(writes_allowed(loopback, false));
        assert!(writes_allowed(loopback, true));
        assert!(!writes_allowed(non_loopback, false));
        assert!(writes_allowed(non_loopback, true));
    }
}
