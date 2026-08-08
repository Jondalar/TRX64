//! DuckDB index schema — the DDL, the compat layer, and store-shape detection.
//!
//! **NO SCHEMA CHANGE.** Every statement below is verbatim from the reference
//! implementation (`trace-run-store.ts` for Shape B, `duckdb-store.ts` for
//! Shape A). Existing stores keep working; a store built by this crate is
//! indistinguishable from a TS-built one apart from the two wall-clock
//! timestamp columns.
//!
//! # Two store shapes
//!
//! | | Shape A — "Spec-217 native store" | Shape B — "Spec-726 index store" |
//! |---|---|---|
//! | who writes it | the dev / VICE-capture producer (TS only, no TRX64 counterpart) | **this crate's indexer** |
//! | base tables | 9, incl. real `anchors` / `rollups` | 4: `trace_run`, `trace_event`, `trace_mark`, `meta` |
//! | `anchors` / `rollups` | base tables filled post-hoc | **VIEWs** (`anchors` over `trace_mark`, `rollups` permanently empty) |
//! | detected by | absence of `trace_event` | presence of `trace_event` |
//!
//! We **write** Shape B only. Shape A's DDL is kept here because the native
//! reader (stage 2) must still answer `getInfo` / `topPcs` / `findBusEvents` /
//! `listAnchors` / `findAnchor` / `safeQuery` against legacy Shape-A files that
//! exist on disk.

use crate::error::{Result, TraceReadError};
use duckdb::Connection;

/// Value written into `meta.schema_version` by the Shape-A writer.
pub const SHAPE_A_SCHEMA_VERSION: &str = "2";
/// Value written into `meta.schema_version` by the indexer (Shape B).
pub const SCHEMA_VERSION_726: &str = "spec-708-streaming";
/// Value written into `meta.writer_version` by the indexer (Shape B).
pub const WRITER_VERSION_726: &str = "spec-726.2c";

// ─────────────────────────────────────────────────────────────────────────────
// §A — Shape A DDL (9 tables), verbatim. REFERENCE ONLY: no native writer.
// ─────────────────────────────────────────────────────────────────────────────

/// The 9 `CREATE TABLE IF NOT EXISTS` statements of the Spec-217 native store,
/// in `applySchema()` order. No secondary indexes, no foreign keys, two primary
/// keys (`meta.key`, `trace_bookmarks.id`).
///
/// Kept for completeness + a future migration/repair tool. **The indexer never
/// runs this** — building Shape A from a `.c64retrace` is not a thing either
/// implementation does.
pub const SHAPE_A_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS instructions (
  run_id TEXT, seq UBIGINT, cpu TEXT, clock UBIGINT, master_clock UBIGINT,
  pc USMALLINT, opcode UTINYINT, b1 UTINYINT, b2 UTINYINT,
  a UTINYINT, x UTINYINT, y UTINYINT, sp UTINYINT, p UTINYINT, source TEXT
);
CREATE TABLE IF NOT EXISTS bus_events (
  run_id TEXT, seq UBIGINT, cpu TEXT, clock UBIGINT, master_clock UBIGINT,
  pc USMALLINT, kind TEXT, addr USMALLINT, value UTINYINT, old_value UTINYINT,
  line_atn BOOLEAN, line_clk BOOLEAN, line_data BOOLEAN, source TEXT
);
CREATE TABLE IF NOT EXISTS bus_event_extras (
  run_id TEXT, parent_seq UBIGINT, key TEXT, value TEXT
);
CREATE TABLE IF NOT EXISTS chip_events (
  run_id TEXT, seq UBIGINT, cpu TEXT, clock UBIGINT, master_clock UBIGINT,
  pc USMALLINT, chip TEXT, kind TEXT, unit UTINYINT, value UTINYINT,
  old_value UTINYINT, source TEXT
);
CREATE TABLE IF NOT EXISTS chip_event_extras (
  run_id TEXT, parent_seq UBIGINT, key TEXT, value TEXT
);
CREATE TABLE IF NOT EXISTS anchors (
  run_id TEXT, source TEXT, cpu TEXT, name TEXT, pc USMALLINT,
  occurrence UBIGINT, clock UBIGINT, master_clock UBIGINT, seq UBIGINT
);
CREATE TABLE IF NOT EXISTS rollups (
  run_id TEXT, source TEXT, level UTINYINT, window_index UBIGINT,
  clock_start UBIGINT, clock_end UBIGINT, cpu TEXT,
  top_pcs_json JSON, bus_counts_json JSON, irq_counts_json JSON, phase TEXT
);
CREATE TABLE IF NOT EXISTS trace_bookmarks (
  run_id TEXT NOT NULL, id TEXT NOT NULL PRIMARY KEY, cycle UBIGINT NOT NULL,
  family TEXT, event_key TEXT, label TEXT NOT NULL, note TEXT, author_tag TEXT,
  tags TEXT[], bind_mode TEXT NOT NULL DEFAULT 'both'
);
"#;

// ─────────────────────────────────────────────────────────────────────────────
// §B — Shape B DDL: what the native indexer PRODUCES.
// ─────────────────────────────────────────────────────────────────────────────

/// The three base tables `openTraceRunStore` creates, in order.
pub const SHAPE_B_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS trace_run (
  run_id TEXT PRIMARY KEY, def_id TEXT, def_version INTEGER, def_json TEXT, name TEXT,
  start_checkpoint_id TEXT, stop_checkpoint_id TEXT, media_sha TEXT, media_name TEXT, branch_id TEXT,
  cycle_start UBIGINT, cycle_end UBIGINT, event_count UBIGINT, bytes_written UBIGINT,
  overhead_ms DOUBLE, retention TEXT, created_at TEXT);
CREATE TABLE IF NOT EXISTS trace_event (
  run_id TEXT, seq UBIGINT, cycle UBIGINT, channel TEXT,
  trigger_kind TEXT, capture_kind TEXT, data_json TEXT);
CREATE TABLE IF NOT EXISTS trace_mark (run_id TEXT, cycle UBIGINT, label TEXT);
"#;

/// The compat layer: `meta` + 5 views. `CREATE OR REPLACE VIEW` throughout, so
/// re-running upgrades a view in place; safe to call repeatedly.
///
/// `anchors.pc` is typed `USMALLINT` and always NULL — `listAnchors` GROUPs BY
/// it, so it must stay a real typed NULL column, not be omitted.
/// `rollups` deliberately has only 7 columns here (Shape A's table has 11).
const COMPAT_VIEWS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE OR REPLACE VIEW instructions AS
  SELECT
    run_id,
    seq,
    CASE WHEN channel = 'drive_pc' OR json_extract_string(data_json, '$.side') = 'drive'
         THEN 'drive8' ELSE 'c64' END AS cpu,
    CAST(COALESCE(json_extract(data_json, '$.clk'), cycle) AS UBIGINT) AS clock,
    cycle AS master_clock,
    CAST(json_extract(data_json, '$.pc') AS USMALLINT) AS pc,
    CAST(json_extract(data_json, '$.opcode') AS UTINYINT) AS opcode,
    CAST(json_extract(data_json, '$.b1') AS UTINYINT) AS b1,
    CAST(json_extract(data_json, '$.b2') AS UTINYINT) AS b2,
    CAST(json_extract(data_json, '$.a') AS UTINYINT) AS a,
    CAST(json_extract(data_json, '$.x') AS UTINYINT) AS x,
    CAST(json_extract(data_json, '$.y') AS UTINYINT) AS y,
    CAST(json_extract(data_json, '$.sp') AS UTINYINT) AS sp,
    CAST(json_extract(data_json, '$.p') AS UTINYINT) AS p,
    'trace_event' AS source
  FROM trace_event
  WHERE channel IN ('cpu', 'drive_pc');

CREATE OR REPLACE VIEW bus_events AS
  SELECT
    run_id,
    seq,
    CASE WHEN json_extract_string(data_json, '$.side') = 'drive' THEN 'drive8' ELSE 'c64' END AS cpu,
    CAST(COALESCE(json_extract(data_json, '$.cycle_drive'),
                  json_extract(data_json, '$.cycle_c64'),
                  cycle) AS UBIGINT) AS clock,
    cycle AS master_clock,
    CAST(json_extract(data_json, '$.pc') AS USMALLINT) AS pc,
    CASE
      WHEN channel = 'iec' THEN 'line_change'
      ELSE json_extract_string(data_json, '$.op')
    END AS kind,
    CAST(json_extract(data_json, '$.addr') AS USMALLINT) AS addr,
    CAST(json_extract(data_json, '$.value') AS UTINYINT) AS value,
    CAST(json_extract(data_json, '$.oldValue') AS UTINYINT) AS old_value,
    CASE WHEN channel = 'iec'
         THEN CAST(json_extract(data_json, '$.atn') AS BOOLEAN) END AS line_atn,
    CASE WHEN channel = 'iec'
         THEN CAST(json_extract(data_json, '$.clk') AS BOOLEAN) END AS line_clk,
    CASE WHEN channel = 'iec'
         THEN CAST(json_extract(data_json, '$.data') AS BOOLEAN) END AS line_data,
    'trace_event' AS source
  FROM trace_event
  WHERE channel IN ('bus_access', 'io', 'iec');

CREATE OR REPLACE VIEW chip_events AS
  SELECT run_id,
         CAST(NULL AS UBIGINT)   AS seq,
         CAST(NULL AS TEXT)      AS cpu,
         CAST(NULL AS UBIGINT)   AS clock,
         CAST(NULL AS UBIGINT)   AS master_clock,
         CAST(NULL AS USMALLINT) AS pc,
         CAST(NULL AS TEXT)      AS chip,
         CAST(NULL AS TEXT)      AS kind,
         CAST(NULL AS UTINYINT)  AS unit,
         CAST(NULL AS UTINYINT)  AS value,
         CAST(NULL AS UTINYINT)  AS old_value,
         CAST(NULL AS TEXT)      AS source
  FROM trace_event WHERE 1=0;

CREATE OR REPLACE VIEW anchors AS
  SELECT
    run_id,
    'trace_mark'            AS source,
    CAST(NULL AS TEXT)      AS cpu,
    label                   AS name,
    CAST(NULL AS USMALLINT) AS pc,
    CAST(row_number() OVER (PARTITION BY label ORDER BY cycle) AS UBIGINT) AS occurrence,
    cycle                   AS clock,
    cycle                   AS master_clock,
    cycle                   AS seq
  FROM trace_mark;

CREATE OR REPLACE VIEW rollups AS
  SELECT run_id,
         CAST(NULL AS TEXT)     AS source,
         CAST(NULL AS UTINYINT) AS level,
         CAST(NULL AS UBIGINT)  AS window_index,
         CAST(NULL AS UBIGINT)  AS clock_start,
         CAST(NULL AS UBIGINT)  AS clock_end,
         CAST(NULL AS TEXT)     AS cpu
  FROM trace_event WHERE 1=0;
"#;

/// The three idempotent `meta` backfills the compat layer runs last. At build
/// time the first two are immediately overwritten with identical values by the
/// run-header write, and the third is a no-op (`trace_run` is still empty).
/// They matter only when HEALING an old store.
const COMPAT_BACKFILL_DDL: &str = r#"
INSERT OR REPLACE INTO meta(key, value)
  SELECT 'schema_version', 'spec-708-streaming'
  WHERE NOT EXISTS (SELECT 1 FROM meta WHERE key='schema_version');
INSERT OR REPLACE INTO meta(key, value)
  SELECT 'source', 'trace_event'
  WHERE NOT EXISTS (SELECT 1 FROM meta WHERE key='source');
INSERT OR REPLACE INTO meta(key, value)
  SELECT 'run_id', run_id FROM trace_run
  WHERE NOT EXISTS (SELECT 1 FROM meta WHERE key='run_id') LIMIT 1;
"#;

// ─────────────────────────────────────────────────────────────────────────────
// Reader-side SQL projections (used by stage 2's `queries.rs`).
// ─────────────────────────────────────────────────────────────────────────────

/// Inline CTE: one executed instruction per row, over a Shape-B store.
/// Readers SELECT `FROM (INSTRUCTIONS_726)` so the product read path never
/// names a legacy table (Spec 726 §6a).
pub const INSTRUCTIONS_726: &str = r#"
  SELECT
    run_id,
    seq,
    CASE WHEN channel = 'drive_pc' OR json_extract_string(data_json, '$.side') = 'drive'
         THEN 'drive8' ELSE 'c64' END AS cpu,
    CAST(COALESCE(json_extract(data_json, '$.clk'), cycle) AS UBIGINT) AS clock,
    cycle AS master_clock,
    CAST(json_extract(data_json, '$.pc') AS USMALLINT) AS pc,
    CAST(json_extract(data_json, '$.opcode') AS UTINYINT) AS opcode,
    CAST(json_extract(data_json, '$.b1') AS UTINYINT) AS b1,
    CAST(json_extract(data_json, '$.b2') AS UTINYINT) AS b2,
    CAST(json_extract(data_json, '$.a') AS UTINYINT) AS a,
    CAST(json_extract(data_json, '$.x') AS UTINYINT) AS x,
    CAST(json_extract(data_json, '$.y') AS UTINYINT) AS y,
    CAST(json_extract(data_json, '$.sp') AS UTINYINT) AS sp,
    CAST(json_extract(data_json, '$.p') AS UTINYINT) AS p
  FROM trace_event
  WHERE channel IN ('cpu', 'drive_pc')"#;

/// Inline CTE: one bus/IEC event per row, over a Shape-B store.
pub const BUS_EVENTS_726: &str = r#"
  SELECT
    run_id,
    seq,
    CASE WHEN json_extract_string(data_json, '$.side') = 'drive' THEN 'drive8' ELSE 'c64' END AS cpu,
    CAST(COALESCE(json_extract(data_json, '$.cycle_drive'),
                  json_extract(data_json, '$.cycle_c64'),
                  cycle) AS UBIGINT) AS clock,
    cycle AS master_clock,
    CAST(json_extract(data_json, '$.pc') AS USMALLINT) AS pc,
    CASE WHEN channel = 'iec' THEN 'line_change'
         ELSE json_extract_string(data_json, '$.op') END AS kind,
    CAST(json_extract(data_json, '$.addr') AS USMALLINT) AS addr,
    CAST(json_extract(data_json, '$.value') AS UTINYINT) AS value,
    CASE WHEN channel = 'iec' THEN CAST(json_extract(data_json, '$.atn') AS BOOLEAN) END AS line_atn,
    CASE WHEN channel = 'iec' THEN CAST(json_extract(data_json, '$.clk') AS BOOLEAN) END AS line_clk,
    CASE WHEN channel = 'iec' THEN CAST(json_extract(data_json, '$.data') AS BOOLEAN) END AS line_data
  FROM trace_event
  WHERE channel IN ('bus_access', 'io', 'iec')"#;

/// Inline CTE: named anchors from `trace_mark` (per-label occurrence ordinal).
pub const ANCHORS_726: &str = r#"
  SELECT
    run_id,
    label AS name,
    CAST(NULL AS TEXT) AS cpu,
    CAST(NULL AS USMALLINT) AS pc,
    CAST(row_number() OVER (PARTITION BY label ORDER BY cycle) AS UBIGINT) AS occurrence,
    cycle AS clock,
    cycle AS seq
  FROM trace_mark"#;

/// FROM-sources for a legacy Shape-A store (real base tables).
pub const LEGACY_INSTRUCTIONS: &str = "instructions";
pub const LEGACY_BUS_EVENTS: &str = "bus_events";
pub const LEGACY_ANCHORS: &str = "anchors";

// ─────────────────────────────────────────────────────────────────────────────
// Shape detection + compat install
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreShape {
    /// Spec-726 index store — has `trace_event`. Written by this crate.
    Spec726,
    /// Spec-217 native store — no `trace_event`; real `instructions` /
    /// `bus_events` / `meta` base tables. Read-only legacy.
    Legacy217,
}

/// Detect the store shape by the presence of the `trace_event` table — the
/// same signature `isSpec726Store` / `isLiveSinkStore` use.
pub fn store_shape(conn: &Connection) -> StoreShape {
    let found: std::result::Result<i32, _> = conn.query_row(
        "SELECT 1 FROM information_schema.tables \
         WHERE table_schema='main' AND table_name='trace_event' LIMIT 1",
        [],
        |r| r.get(0),
    );
    match found {
        Ok(_) => StoreShape::Spec726,
        Err(_) => StoreShape::Legacy217,
    }
}

/// Create the three Shape-B base tables and install the compat layer.
/// Used by the index builder on a fresh temp store.
pub fn create_trace_run_store(conn: &Connection) -> Result<()> {
    // Build-only site (the indexer's fresh temp store) → `duck_build`: no
    // sidecar counterpart, so the step name is kept in the message.
    conn.execute_batch(SHAPE_B_DDL)
        .map_err(|e| TraceReadError::duck_build("create trace_run store", e))?;
    ensure_spec726_compat_layer(conn)?;
    Ok(())
}

/// Idempotent compat-layer install / **heal**.
///
/// Early-outs on a non-726 store (a genuine Shape-A store has its own base
/// tables and must not be shadowed by views). Exposed as its own function —
/// **not inlined into store creation** — because the reader opens `READ_ONLY`
/// first and only falls back to read-write to heal an old store that predates
/// the views.
///
/// Both failure sites stay on the READ-path [`TraceReadError::Duck`] variant
/// (bare message, Spec 802 F1) because `conn::with_conn` reaches this function
/// on every read of an old store — the sidecar's `withDuckDb` healed the same
/// way and let the driver error through unprefixed. It costs the *build* caller
/// a little context; the read path's parity outranks that.
pub fn ensure_spec726_compat_layer(conn: &Connection) -> Result<()> {
    if store_shape(conn) != StoreShape::Spec726 {
        return Ok(());
    }
    conn.execute_batch(COMPAT_VIEWS_DDL)
        .map_err(|e| TraceReadError::duck("install spec-726 compat views", e))?;
    conn.execute_batch(COMPAT_BACKFILL_DDL)
        .map_err(|e| TraceReadError::duck("backfill meta", e))?;
    Ok(())
}

/// Does this store already carry the compat views? Used by the read path to
/// decide whether a read-write heal pass is needed at all.
pub fn has_compat_views(conn: &Connection) -> bool {
    let n: std::result::Result<i64, _> = conn.query_row(
        "SELECT count(*) FROM information_schema.tables \
         WHERE table_schema='main' \
           AND table_name IN ('instructions','bus_events','chip_events','anchors','rollups')",
        [],
        |r| r.get(0),
    );
    matches!(n, Ok(5))
}
