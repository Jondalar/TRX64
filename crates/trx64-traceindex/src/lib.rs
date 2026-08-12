//! # trx64-traceindex — native `.c64retrace` read (Spec 802)
//!
//! **The rule this crate exists to enforce:** TRX64 is the runtime *and* the
//! monitor. Every verb — `dump`, `ringdump`, capture **and trace read** — runs
//! with no Node, no TypeScript, no C64RE source tree and no spawned helper.
//! C64RE consumes TRX64 and keeps no second reader.
//!
//! Before this crate, TRX64 could **write** its trace format in Rust and could
//! not **read** it: every read op shelled out to a `tsx` sidecar that
//! dynamically imported C64RE's TypeScript — a Rust daemon acting as a
//! pass-through back into the very repo that was asking it the question. It
//! worked on exactly one machine, never on Windows, and never in the container.
//!
//! ## No format change
//!
//! The `.c64retrace` binary format and the DuckDB index schema are untouched.
//! Existing stores keep working, existing traces stay readable, and a store
//! built here is indistinguishable from a TS-built one apart from the two
//! wall-clock timestamp columns (`trace_run.created_at`, `meta.captured_at`).
//!
//! ## Layout
//!
//! | module | role | status |
//! |---|---|---|
//! | [`error`] | shared `TraceReadError` / `Result`; error TEXT is contract | done |
//! | [`decode`] | `.c64retrace` streaming frame parser (header + windowed decode) | done |
//! | [`rows`] | frame → `trace_event` row projection (`data_json`, `seq`, channels) | done |
//! | [`schema`] | Shape A + Shape B DDL, compat-view install/heal, shape detection | done |
//! | [`build`] | `index_binary_log` — whole-file build, temp + atomic rename | done |
//! | [`ensure`] | job registry, `ensure_index[_bounded]`, the `index` op | done |
//! | [`conn`] | **shared** store access for every reader: `with_conn`, `query_json` | done |
//! | [`queries`] | `store_fn` family (`getInfo`/`topPcs`/…/`safeQuery`) | done |
//! | [`map`] | `map` — memory-map text renderer | done |
//! | [`taint`] | `taint` / `taint_text` | done |
//! | [`swimlane`] | `swimlane` / `swimlane_text` (`chis`) rendering | done |
//! | [`query_events`] | `query_events` — typed event query | **scaffold** |
//! | [`follow_path`] | `follow_path` — backwards causal chain | **scaffold** |
//! | [`profile_loader`] | `profile_loader` — loader/protection profile | **scaffold** |
//!
//! ## Op surface (what the daemon's `trace/read` dispatches to)
//!
//! `index` · `store_fn` · `map` · `swimlane` · `swimlane_text` · `taint` ·
//! `taint_text` · `query_events` · `follow_path` · `profile_loader`.
//!
//! The last three are **scaffolded, not implemented** (Spec 802 F2): the daemon
//! routes them here, arguments are parsed and the store is opened, and the core
//! function returns a "not implemented yet" error. There is deliberately no
//! fallback — re-adding the spawn would reinstate the dependency this spec
//! removes. Their argument CASE differs per op and is fixed in the parsers:
//! `query_events` / `follow_path` take camelCase, `profile_loader` snake_case.
//!
//! **Dropped:** the raw `sql` passthrough (Spec 802 OQ1) — `safeQuery`, with its
//! SELECT/WITH prefix gate and row cap, is the supported query door.
//!
//! ## Threading
//!
//! `duckdb` is a blocking C++ library. Index builds run on a dedicated
//! `std::thread` ([`ensure::start_background_index`]) and must never be placed
//! on the tokio executor that serves the WS.
//!
//! ## Typical use from the daemon
//!
//! ```ignore
//! use trx64_traceindex as ti;
//!
//! // at trace finalize — synchronous, so an index failure fails the finalize
//! let res = ti::index_binary_log(&retrace, &duckdb, Some(&ti::IndexOverrides {
//!     overhead_ms: Some(overhead),
//!     stop_checkpoint_id: stop_ckpt,
//!     marks: Some(state.marks.clone()),   // Spec 802 R2 J-3
//!     ..Default::default()
//! }))?;
//!
//! // the `index` op
//! let json = ti::op_index(&duckdb, None, wait)?;
//! ```

pub mod build;
pub mod conn;
pub mod decode;
pub mod ensure;
pub mod error;
pub mod rows;
pub mod schema;

// ── the reader ops ───────────────────────────────────────────────────────────
pub mod follow_path;
pub mod map;
pub mod profile_loader;
pub mod queries;
pub mod query_events;
pub mod swimlane;
pub mod taint;

// ── the public surface, flattened ────────────────────────────────────────────
pub use error::{Result, TraceReadError};

pub use decode::{
    decode_event, decode_event_stream, decode_file_header, read_retrace_meta, stream_retrace,
    DecodedEvent, ParsedHeader, RetraceReader, ScanSummary, Step, TraceDefinition, TraceFileMeta,
    FORMAT_VERSION, MAGIC,
};

pub use rows::{event_to_row, TraceEventRow};

pub use schema::{
    create_trace_run_store, ensure_spec726_compat_layer, store_shape, StoreShape, ANCHORS_726,
    BUS_EVENTS_726, INSTRUCTIONS_726,
};

pub use build::{index_binary_log, retrace_path_for, IndexOverrides, IndexResult};

pub use ensure::{
    op_index_with,
    adaptive_index_timeout_ms, ensure_index, ensure_index_bounded, index_error, index_is_stale,
    is_indexing, op_index, start_background_index, IndexJob,
};

pub use conn::{json_to_duck, open_read_only, query_json, query_named, with_conn};

// The ops themselves, flattened — this is what `trace_read_native` in the daemon
// dispatches to. Each takes the store path + the WS `args` object verbatim.
pub use follow_path::{op_follow_path, PathQuery};
pub use map::op_map;
pub use profile_loader::{op_profile_loader, ProfileArgs};
pub use queries::{op_store_fn, store_fn};
pub use query_events::{op_query_events, EventFamily, EventQuery, EventRow};
pub use swimlane::{op_swimlane, op_swimlane_text};
pub use taint::{op_taint, op_taint_text};
