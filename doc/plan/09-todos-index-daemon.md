# Plan 09 — TODO index (sqlite) + daemon mode (HTTP REST + SSE)

## Goal

1. `org-cli todos` — one-shot action: sync all TODO headings from the agenda
   set (or given paths) into a sqlite index, then list/query them fast.
2. `org-cli daemon` — long-running mode: inotify watcher on the agenda-list
   file and on all indexed `.org` files, incremental sqlite updates, HTTP
   REST snapshot + SSE push to registered clients (via `kal-sse`).

## Layering (strippability requirement)

The HTTP+SSE part must be easy to strip or to integrate into vigil-watch.

```
┌───────────────────────────────────────────────────────────┐
│ serve/  (cargo feature "server", in default features)     │
│   tokio + axum + kal-sse; bridges mpsc → EventLog → SSE   │
├───────────────────────────────────────────────────────────┤
│ watch/  (core; notify only, runtime-agnostic)             │
│   debounced fs events → index sync → Vec<TodoEvent>       │
│   delivered over std::sync::mpsc                          │
├───────────────────────────────────────────────────────────┤
│ index/  (core; rusqlite only, pure sync)                  │
│   schema, fingerprint-based incremental upsert, queries;  │
│   sync ops return Vec<TodoEvent> (the diff)               │
├───────────────────────────────────────────────────────────┤
│ org/    (existing; unchanged parsing primitives)          │
│   iter_headings_with, TodoKeywords::scan_buffer           │
└───────────────────────────────────────────────────────────┘
```

- `TodoEvent` (Clone + Send + Serialize enum) lives in `index/` — the core
  vocabulary. The server layer only serializes it.
- **Strip**: `default-features = false` → no tokio/axum/kal-sse compiled at
  all; `daemon` subcommand disappears (`#[cfg(feature = "server")]`).
- **Integrate in vigil-watch**: depend on
  `org-cli = { git = "https://github.com/0k/org-cli", default-features = false }`, reuse
  `index` + `watch` + `TodoEvent`, append into vigil-watch's own kal-sse
  `EventLog` and axum router. `serve/` is a thin (~150 line) reference
  adapter following the exact vigil-watch kal-sse pattern.

## Architectural consolidation map

| Existing concept | Outcome |
|---|---|
| `org::heading::iter_headings_with` / `Heading` | **Reuse** — only parsing path |
| `org::todo_keywords::TodoKeywords::scan_buffer` | **Reuse** per file |
| `resolve_input_files` + agenda reading (in `cli/action/clock.rs`) | **Redraw (Outcome B)**: move to `cli/action/walk.rs`; now shared by clock, headings, todos, daemon |
| `src/config.rs` | **Extend** with `todos:` section (`db-path`, `serve-addr`) |
| Output formats (human/`-0`/`-j`) | **Follow** existing per-action pattern |
| `index/`, `watch/`, `serve/` | **New concepts** (no prior persistence/watch/HTTP code exists) |

## CLI surface

```
org-cli todos [PATH]... [--search STR] [--keyword KW] [--no-sync] [-0|-j]
org-cli daemon [PATH]... [--addr 127.0.0.1:7480]
```

- `todos`: sync index (unless `--no-sync`), then query+print.
  `--search` = substring in title; `--keyword` = exact TODO-state match.
- `daemon`: initial sync, then watch + serve until killed.

## SQLite (rusqlite, bundled, WAL)

```sql
files(id INTEGER PK, path TEXT UNIQUE, mtime_ns INTEGER, size INTEGER)
headings(id INTEGER PK, file_id → files ON DELETE CASCADE,
         line INTEGER, level INTEGER, keyword TEXT, title TEXT,
         tags_json TEXT, is_done INTEGER)
CREATE INDEX idx_headings_keyword ON headings(keyword);
```

- Fingerprint = (mtime_ns, size); unchanged files are never reparsed.
- Per-file transaction: delete rows + reinsert (cheapest correct unit).
- Only headings **with a TODO keyword** are indexed (active or done state).
- Queries: `keyword = ?`; `title LIKE '%'||?||'%'` (escape `%_`).
  FTS5 upgrade path documented, not built (YAGNI at agenda-set scale).
- Default db path: `~/.cache/org-cli/todos.sqlite`.

## Watcher (notify)

- Watch the agenda-list file (`~/.cache/emacs/org-collect/org-agenda-files.tmp`)
  → membership changes re-resolve the set and re-arm watches.
- Watch **parent directories** of indexed files (non-recursive), filter to
  `*.org` — survives emacs rename-on-save; ignore `.#*` lockfiles, `*~`,
  hidden files.
- Debounce ~200 ms, coalesce per path; reparse only touched files.

## HTTP contract (kal-sse two-endpoint pattern)

- `GET /api/todos?search=&keyword=&sub=` → JSON snapshot,
  headers `x-sub-id`, `x-snapshot-index`.
- `GET /sse/todos?sub=` → `kal_sse::axum::sse_response`; `409 needs_snapshot`
  on unknown/expired sub.
- Events (`TodoEvent` serialized):
  `{"type":"file-changed","path":…,"todos":[…]}`,
  `{"type":"file-removed","path":…}`.
- Compaction task every 60 s. `EventLog::new(15 min TTL, 10_000, 256)`.

## New dependencies

- Core: `rusqlite = { version = "0.32", features = ["bundled"] }`,
  `notify = "6"`.
- Feature `server` (default): `tokio` (rt-multi-thread, macros, sync, time),
  `axum = "0.8"`, `kal-sse = { git = "https://github.com/0k/kal-sse", features = ["axum"] }`.
- Dev: `reqwest` (blocking, for daemon integration test).

## Phases

1. **Core**: walk.rs redraw + `index/` + `todos` action. TDD: unit tests on
   schema/upsert/diff/query; integration `tests/todos_integration.rs`.
2. **Daemon**: `watch/` + `serve/` + `daemon` action.
   Integration test: ephemeral port, snapshot, modify fixture, assert SSE
   event + db update.

Each phase ends with: cargo fmt, clippy --all-targets --all-features,
test, review pass.
