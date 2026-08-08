//! `store_fn` — the trace-store reader family (Spec 802 §4.1, op `store_fn`).
//!
//! Six read-only functions dispatched by the `fn` argument, answering against
//! BOTH store shapes. The FROM-source is picked from the
//! [`StoreShape`](crate::schema::StoreShape) that [`crate::conn::with_conn`]
//! hands in:
//!
//! | `fn` | Shape B source | Shape A source | reference |
//! |---|---|---|---|
//! | `getInfo` | `trace_run` + counts | `meta` + 5 table counts | `queries.ts:67` / `queries-legacy217.ts:12` |
//! | `topPcs` | `({INSTRUCTIONS_726})` | `instructions` | `queries.ts:176` |
//! | `findBusEvents` | `({BUS_EVENTS_726})` | `bus_events` | `queries.ts:195` |
//! | `listAnchors` | `({ANCHORS_726})` | `anchors` | `queries.ts:122` |
//! | `findAnchor` | `({ANCHORS_726})` | `anchors` | `queries.ts:149` |
//! | `safeQuery` | — (the caller's SQL) | — | `queries.ts:220` |
//!
//! The raw `sql` op is **DROPPED** with the port (owner decision — it had no
//! caller in either repo).
//!
//! # The output contract
//!
//! Every result is the reference's value after its `jsonSafe` down-cast
//! (`BigInt` → `Number`), which is what [`crate::conn::query_json`] already
//! produces. Field names, nesting, null-vs-omitted and the deliberate artefacts
//! below are all part of the contract:
//!
//! * `getInfo.masterClockRange` is **omitted** (not `null`) when the store has
//!   no cycles — the reference returns `undefined` and `JSON.stringify` drops
//!   the key.
//! * `getInfo.meta.def_version` is **stringified** even though the column is an
//!   `INTEGER`.
//! * `listAnchors` on a Shape-B store reports `cpu: "null"` (the *string*, from
//!   JS `String(null)`) and `pc: 0` (from `Number(null)`), because `ANCHORS_726`
//!   projects both as typed NULLs. `findAnchor` likewise reports `pc: 0`. These
//!   are visible in `trace_store_anchor_list` output; they are reproduced, not
//!   fixed.
//! * `findBusEvents.pc` / `.value` stay `null` (not omitted) when NULL.
//! * `safeQuery` returns **positional row arrays** — no column names — capped
//!   client-side, exactly like `all.slice(0, rowLimit)`.
//!
//! ## Key ORDER is not observable
//!
//! The reference builds `meta` in insertion order, but the result crosses the
//! wire through `serde_json::Value` (a `BTreeMap` — `preserve_order` is off
//! workspace-wide by deliberate choice, R2 §J-2). The old Rust daemon already
//! reparsed the sidecar's stdout into a `Value` before answering, so consumers
//! have *always* seen alphabetically-ordered keys. Emitting a `Value` here is
//! therefore parity, not a divergence.
//!
//! # Argument decoding
//!
//! `store_fn`'s per-`fn` parameters live in the **nested** `args.args` object,
//! and the reference reaches them through JS coercions that are part of the
//! observable behaviour — `Number(undefined) & 0xffff === 0`,
//! `String(undefined) === "undefined"`, default parameters that fire only for
//! `undefined` (never for `null`). [`js`] reproduces those; see its tests.

use crate::conn::{self, with_conn};
use crate::error::{Result, TraceReadError};
use crate::schema::{
    StoreShape, ANCHORS_726, BUS_EVENTS_726, INSTRUCTIONS_726, LEGACY_ANCHORS, LEGACY_BUS_EVENTS,
    LEGACY_INSTRUCTIONS,
};
use duckdb::types::Value as DuckValue;
use duckdb::Connection;
use serde_json::{json, Map, Value};
use std::path::Path;

// ─────────────────────────────────────────────────────────────────────────────
// Entry points
// ─────────────────────────────────────────────────────────────────────────────

/// The `store_fn` op: `args = {"fn": "<name>", "args": {…}}`.
///
/// Decodes the op envelope exactly as the reference dispatcher does
/// (`args.args ?? {}`, `String(args.fn)`) and delegates to [`store_fn`].
pub fn op_store_fn(duckdb_path: &Path, args: &Value) -> Result<Value> {
    let fa = match args.get("args") {
        Some(v) if !v.is_null() => v.clone(),
        _ => json!({}),
    };
    let name = js::string_of(args.get("fn"));
    store_fn(duckdb_path, &name, &fa)
}

/// Dispatch one `store_fn` call. `fa` is the **nested** per-`fn` argument object.
///
/// Two things happen BEFORE any argument gate, in the reference's order:
///
/// 1. **Preflight** — neither a store nor a `.c64retrace` authority at this stem
///    is a `no trace store and no .c64retrace authority at …` error. The
///    reference runs this in the op dispatcher, ahead of every op; repeating it
///    here makes this entry point correct on its own (and it is idempotent if
///    the WS layer checks too, exactly like [`crate::op_index`]).
/// 2. **Bounded index-ensure** — the sidecar calls `ensureIndexBounded(duckdb)`
///    before its `switch`, so an unbuildable index is reported ahead of
///    `invalid anchor name` / `only SELECT/WITH…` / `unknown fn`.
///
/// Calling a typed reader ([`find_anchor`], [`safe_query`], …) directly skips
/// the preflight and keeps its own argument gate first — which is what the
/// reference's `queries.ts` functions do when called outside the sidecar.
pub fn store_fn(duckdb_path: &Path, name: &str, fa: &Value) -> Result<Value> {
    let retrace = crate::build::retrace_path_for(duckdb_path);
    if !duckdb_path.exists() && !retrace.exists() {
        return Err(TraceReadError::NoAuthority {
            duckdb: duckdb_path.display().to_string(),
            retrace: retrace.display().to_string(),
        });
    }
    crate::ensure::ensure_index_bounded(duckdb_path, None)?;
    match name {
        "getInfo" => get_info(duckdb_path),
        "topPcs" => top_pcs(
            duckdb_path,
            &js::string_of(fa.get("cpu")),
            js::clamped_limit(fa.get("limit"), 20, 200),
        ),
        "findBusEvents" => find_bus_events(
            duckdb_path,
            js::to_u16(fa.get("addr")),
            js::clamped_limit(fa.get("limit"), 100, 10_000),
        ),
        "listAnchors" => list_anchors(duckdb_path),
        "findAnchor" => find_anchor(
            duckdb_path,
            &js::string_of(fa.get("name")),
            js::clamped_limit(fa.get("limit"), 200, 10_000),
        ),
        "safeQuery" => safe_query(
            duckdb_path,
            &js::string_of(fa.get("sql")),
            js::row_limit(fa.get("limit")),
        ),
        other => Err(TraceReadError::other(format!(
            "trace/read store_fn: unknown fn \"{other}\""
        ))),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// getInfo
// ─────────────────────────────────────────────────────────────────────────────

/// `{ meta, tableCounts, masterClockRange? }` — store identity + row counts.
pub fn get_info(duckdb_path: &Path) -> Result<Value> {
    with_conn(duckdb_path, get_info_on_conn)
}

pub fn get_info_on_conn(conn: &Connection, shape: StoreShape) -> Result<Value> {
    match shape {
        StoreShape::Spec726 => get_info_726(conn),
        StoreShape::Legacy217 => get_info_legacy217(conn),
    }
}

/// Shape B: identity from `trace_run`, counts from `trace_event` / `trace_mark`.
/// **Does not read `meta`** — the product reader path never names a legacy table
/// (Spec 726 §6a).
fn get_info_726(conn: &Connection) -> Result<Value> {
    let mut meta = Map::new();
    meta.insert(
        "schema".into(),
        json!("trace_run/trace_event/trace_mark"),
    );
    meta.insert("source".into(), json!("live-sink-726"));

    let run = conn::query_json(
        conn,
        "SELECT run_id, def_id, def_version, retention, created_at FROM trace_run LIMIT 1",
    )?;
    if let Some(rr) = run.first() {
        // Order mirrors the reference's insertion order; `def_version` is
        // stringified although the column is an INTEGER.
        for (i, key) in ["run_id", "def_id", "def_version", "retention", "created_at"]
            .iter()
            .enumerate()
        {
            match rr.get(i) {
                Some(v) if !v.is_null() => {
                    meta.insert((*key).into(), json!(js::string(v)));
                }
                _ => {}
            }
        }
    }

    let counts = conn::query_json(
        conn,
        "SELECT 'events:' || channel AS k, count(*) AS n FROM trace_event GROUP BY channel \
         UNION ALL SELECT 'events:total', count(*) FROM trace_event \
         UNION ALL SELECT 'marks', count(*) FROM trace_mark",
    )?;
    let table_counts = counts_to_map(&counts);

    let range = conn::query_json(
        conn,
        "SELECT MIN(cycle), MAX(cycle) FROM trace_event WHERE cycle IS NOT NULL",
    )?;
    Ok(info_object(meta, table_counts, &range))
}

/// Shape A: the whole `meta` table + the five base-table counts.
fn get_info_legacy217(conn: &Connection) -> Result<Value> {
    let mut meta = Map::new();
    for row in conn::query_json(conn, "SELECT key, value FROM meta ORDER BY key")? {
        let k = js::string(row.first().unwrap_or(&Value::Null));
        let v = js::string(row.get(1).unwrap_or(&Value::Null));
        meta.insert(k, json!(v));
    }

    let counts = conn::query_json(
        conn,
        "SELECT 'instructions', count(*) FROM instructions \
         UNION ALL SELECT 'bus_events', count(*) FROM bus_events \
         UNION ALL SELECT 'chip_events', count(*) FROM chip_events \
         UNION ALL SELECT 'anchors', count(*) FROM anchors \
         UNION ALL SELECT 'rollups', count(*) FROM rollups",
    )?;
    let table_counts = counts_to_map(&counts);

    let range = conn::query_json(
        conn,
        "SELECT MIN(master_clock), MAX(master_clock) FROM instructions WHERE master_clock IS NOT NULL",
    )?;
    Ok(info_object(meta, table_counts, &range))
}

fn counts_to_map(rows: &[Vec<Value>]) -> Map<String, Value> {
    let mut out = Map::new();
    for row in rows {
        let k = js::string(row.first().unwrap_or(&Value::Null));
        let n = row.get(1).cloned().unwrap_or(Value::Null);
        out.insert(k, js::number(&n));
    }
    out
}

/// Assemble the `TraceStoreInfo` object. `masterClockRange` is OMITTED — never
/// `null` — when `MIN(...)` came back NULL.
fn info_object(
    meta: Map<String, Value>,
    table_counts: Map<String, Value>,
    range: &[Vec<Value>],
) -> Value {
    let mut out = Map::new();
    out.insert("meta".into(), Value::Object(meta));
    out.insert("tableCounts".into(), Value::Object(table_counts));
    if let Some(r) = range.first() {
        let min = r.first().cloned().unwrap_or(Value::Null);
        if !min.is_null() {
            let max = r.get(1).cloned().unwrap_or(Value::Null);
            out.insert(
                "masterClockRange".into(),
                json!({ "min": js::number(&min), "max": js::number(&max) }),
            );
        }
    }
    Value::Object(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// topPcs
// ─────────────────────────────────────────────────────────────────────────────

/// `[{ pc, count }]`, hottest first. `limit` is already clamped to `1..=200` by
/// the dispatcher; passing an unclamped value here clamps it again.
pub fn top_pcs(duckdb_path: &Path, cpu: &str, limit: u32) -> Result<Value> {
    with_conn(duckdb_path, |conn, shape| {
        top_pcs_on_conn(conn, shape, cpu, limit)
    })
}

pub fn top_pcs_on_conn(
    conn: &Connection,
    shape: StoreShape,
    cpu: &str,
    limit: u32,
) -> Result<Value> {
    let from = instructions_from(shape);
    let sql = format!(
        "SELECT pc, count(*) AS n \
         FROM {from} \
         WHERE cpu = '{cpu}' \
         GROUP BY pc \
         ORDER BY n DESC \
         LIMIT {lim}",
        cpu = sq(cpu),
        lim = limit.clamp(1, 200),
    );
    let rows = conn::query_json(conn, &sql)?;
    Ok(Value::Array(
        rows.iter()
            .map(|r| {
                json!({
                    "pc": js::number(r.first().unwrap_or(&Value::Null)),
                    "count": js::number(r.get(1).unwrap_or(&Value::Null)),
                })
            })
            .collect(),
    ))
}

// ─────────────────────────────────────────────────────────────────────────────
// findBusEvents
// ─────────────────────────────────────────────────────────────────────────────

/// `[{ seq, cpu, kind, clock, pc, value }]` for one address, in `seq` order.
/// `addr` is masked to 16 bits (the reference's `addr & 0xffff`).
pub fn find_bus_events(duckdb_path: &Path, addr: u32, limit: u32) -> Result<Value> {
    with_conn(duckdb_path, |conn, shape| {
        find_bus_events_on_conn(conn, shape, addr, limit)
    })
}

pub fn find_bus_events_on_conn(
    conn: &Connection,
    shape: StoreShape,
    addr: u32,
    limit: u32,
) -> Result<Value> {
    let from = bus_events_from(shape);
    let sql = format!(
        "SELECT seq, cpu, kind, clock, pc, value \
         FROM {from} \
         WHERE addr = {addr} \
         ORDER BY seq \
         LIMIT {lim}",
        addr = addr & 0xffff,
        lim = limit.clamp(1, 10_000),
    );
    let rows = conn::query_json(conn, &sql)?;
    Ok(Value::Array(
        rows.iter()
            .map(|r| {
                json!({
                    "seq":   js::number(r.first().unwrap_or(&Value::Null)),
                    "cpu":   js::string(r.get(1).unwrap_or(&Value::Null)),
                    "kind":  js::string(r.get(2).unwrap_or(&Value::Null)),
                    "clock": js::number(r.get(3).unwrap_or(&Value::Null)),
                    // pc / value keep NULL as JSON null (never omitted).
                    "pc":    js::number_or_null(r.get(4).unwrap_or(&Value::Null)),
                    "value": js::number_or_null(r.get(5).unwrap_or(&Value::Null)),
                })
            })
            .collect(),
    ))
}

// ─────────────────────────────────────────────────────────────────────────────
// listAnchors / findAnchor
// ─────────────────────────────────────────────────────────────────────────────

/// `[{ name, cpu, pc, occurrences, firstClock, lastClock }]`, most frequent
/// first. No `LIMIT`.
pub fn list_anchors(duckdb_path: &Path) -> Result<Value> {
    with_conn(duckdb_path, list_anchors_on_conn)
}

pub fn list_anchors_on_conn(conn: &Connection, shape: StoreShape) -> Result<Value> {
    let from = anchors_from(shape);
    let sql = format!(
        "SELECT name, cpu, pc, count(*) AS n, MIN(clock), MAX(clock) \
         FROM {from} \
         GROUP BY name, cpu, pc \
         ORDER BY n DESC"
    );
    let rows = conn::query_json(conn, &sql)?;
    Ok(Value::Array(
        rows.iter()
            .map(|r| {
                json!({
                    "name": js::string(r.first().unwrap_or(&Value::Null)),
                    // Shape B projects cpu/pc as typed NULLs; String(null) ===
                    // "null" and Number(null) === 0. Reproduced deliberately.
                    "cpu":  js::string(r.get(1).unwrap_or(&Value::Null)),
                    "pc":   js::number(r.get(2).unwrap_or(&Value::Null)),
                    "occurrences": js::number(r.get(3).unwrap_or(&Value::Null)),
                    "firstClock":  js::number_or_null(r.get(4).unwrap_or(&Value::Null)),
                    "lastClock":   js::number_or_null(r.get(5).unwrap_or(&Value::Null)),
                })
            })
            .collect(),
    ))
}

/// `[{ occurrence, pc, clock, seq }]` for one anchor name, in occurrence order.
///
/// The name is sanitized BEFORE the store is opened — `^[a-zA-Z0-9_\-]+$`, else
/// `invalid anchor name: <name>`.
pub fn find_anchor(duckdb_path: &Path, name: &str, limit: u32) -> Result<Value> {
    assert_anchor_name(name)?;
    with_conn(duckdb_path, |conn, shape| {
        find_anchor_on_conn(conn, shape, name, limit)
    })
}

pub fn find_anchor_on_conn(
    conn: &Connection,
    shape: StoreShape,
    name: &str,
    limit: u32,
) -> Result<Value> {
    assert_anchor_name(name)?;
    let from = anchors_from(shape);
    let sql = format!(
        "SELECT occurrence, pc, clock, seq \
         FROM {from} \
         WHERE name = '{name}' \
         ORDER BY occurrence \
         LIMIT {lim}",
        name = sq(name),
        lim = limit.clamp(1, 10_000),
    );
    let rows = conn::query_json(conn, &sql)?;
    Ok(Value::Array(
        rows.iter()
            .map(|r| {
                json!({
                    "occurrence": js::number(r.first().unwrap_or(&Value::Null)),
                    "pc":         js::number(r.get(1).unwrap_or(&Value::Null)),
                    "clock":      js::number(r.get(2).unwrap_or(&Value::Null)),
                    "seq":        js::number(r.get(3).unwrap_or(&Value::Null)),
                })
            })
            .collect(),
    ))
}

/// `^[a-zA-Z0-9_\-]+$`, ASCII only — the reference regex, hand-rolled (no
/// `regex` dependency in this crate).
fn assert_anchor_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(TraceReadError::other(format!("invalid anchor name: {name}")))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// safeQuery
// ─────────────────────────────────────────────────────────────────────────────

/// Run a caller-supplied read-only query and return **positional** row arrays.
///
/// Ported AS-IS by owner decision: the gate is a lowercase-prefix check for
/// `select` / `with` and nothing else, and the cap is a post-hoc truncation —
/// the SQL is never rewritten, so a query carrying its own `ORDER BY` / `LIMIT`
/// keeps its meaning. The gate runs BEFORE the store is opened, exactly like the
/// reference.
///
/// Known and deliberate holes (they are the reference's): `"selectx …"` passes
/// the prefix check and then fails in SQL; a leading SQL comment is rejected;
/// `"SELECT 1; DROP TABLE t"` passes the gate. The connection is opened
/// `READ_ONLY` first, which is what actually stops a write.
pub fn safe_query(duckdb_path: &Path, sql: &str, row_limit: usize) -> Result<Value> {
    conn::assert_select_only(sql)?;
    with_conn(duckdb_path, |conn, _shape| {
        safe_query_on_conn(conn, sql, row_limit)
    })
}

pub fn safe_query_on_conn(conn: &Connection, sql: &str, row_limit: usize) -> Result<Value> {
    conn::assert_select_only(sql)?;
    let rows = read_rows_capped(conn, sql, row_limit)?;
    Ok(Value::Array(rows.into_iter().map(Value::Array).collect()))
}

/// Like [`crate::conn::query_json`] but stops fetching once `cap` rows are in.
/// The statement is always executed (so a bad query errors even at `cap == 0`),
/// matching `runAndReadAll(...)` followed by `all.slice(0, rowLimit)`.
fn read_rows_capped(conn: &Connection, sql: &str, cap: usize) -> Result<Vec<Vec<Value>>> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| TraceReadError::duck("prepare query", e))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| TraceReadError::duck("run query", e))?;
    let ncols = rows.as_ref().map(|s| s.column_count()).unwrap_or(0);
    let mut out: Vec<Vec<Value>> = Vec::new();
    if cap == 0 {
        return Ok(out);
    }
    while let Some(row) = rows
        .next()
        .map_err(|e| TraceReadError::duck("fetch row", e))?
    {
        let mut vals = Vec::with_capacity(ncols);
        for i in 0..ncols {
            let v: DuckValue = row
                .get(i)
                .map_err(|e| TraceReadError::duck("read column", e))?;
            vals.push(conn::value_to_json(&v));
        }
        out.push(vals);
        if out.len() >= cap {
            break;
        }
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// FROM-source selection + SQL literal escaping
// ─────────────────────────────────────────────────────────────────────────────

fn instructions_from(shape: StoreShape) -> String {
    match shape {
        StoreShape::Spec726 => format!("({INSTRUCTIONS_726})"),
        StoreShape::Legacy217 => LEGACY_INSTRUCTIONS.to_string(),
    }
}

fn bus_events_from(shape: StoreShape) -> String {
    match shape {
        StoreShape::Spec726 => format!("({BUS_EVENTS_726})"),
        StoreShape::Legacy217 => LEGACY_BUS_EVENTS.to_string(),
    }
}

fn anchors_from(shape: StoreShape) -> String {
    match shape {
        StoreShape::Spec726 => format!("({ANCHORS_726})"),
        StoreShape::Legacy217 => LEGACY_ANCHORS.to_string(),
    }
}

/// SQL single-quote escaping for an interpolated string literal.
///
/// The reference interpolates `cpu` and `name` raw. `name` is already
/// regex-gated (no quote can survive), so this only ever changes behaviour for a
/// `cpu` value containing a quote — where the reference produced a SQL syntax
/// error and this produces a literal that matches nothing. Every legitimate
/// input (`c64` / `drive8`) yields byte-identical SQL, so the parity gate is
/// unaffected; the daemon binds `0.0.0.0` in the container, which makes the
/// unescaped variant indefensible.
fn sq(s: &str) -> String {
    s.replace('\'', "''")
}

// ─────────────────────────────────────────────────────────────────────────────
// JS coercions — observable behaviour, not cosmetics
// ─────────────────────────────────────────────────────────────────────────────

/// The handful of JavaScript coercions the reference's result mapping depends
/// on. They are reproduced because they are *visible*: `String(null)` is why
/// `listAnchors` reports the string `"null"`, `Number(null)` is why `pc` is `0`,
/// and `Number(undefined) & 0xffff` is why a missing `addr` queries address 0
/// instead of erroring.
pub mod js {
    use serde_json::{Number, Value};

    /// JS `String(x)` over a DuckDB column value. `undefined` (an absent key)
    /// stringifies to `"undefined"`.
    pub fn string_of(v: Option<&Value>) -> String {
        match v {
            None => "undefined".into(),
            Some(v) => string(v),
        }
    }

    /// JS `String(x)` over a present value.
    pub fn string(v: &Value) -> String {
        match v {
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => number_to_string(n),
            Value::String(s) => s.clone(),
            // Not reachable for any column of either store shape.
            other => other.to_string(),
        }
    }

    /// JS number→string: integral values never carry a `.0` suffix.
    fn number_to_string(n: &Number) -> String {
        if let Some(i) = n.as_i64() {
            return i.to_string();
        }
        if let Some(u) = n.as_u64() {
            return u.to_string();
        }
        match n.as_f64() {
            Some(f) if f.is_nan() => "NaN".into(),
            Some(f) if f.is_infinite() => {
                if f > 0.0 {
                    "Infinity".into()
                } else {
                    "-Infinity".into()
                }
            }
            Some(f) if f == f.trunc() && f.abs() < 1e21 => format!("{}", f as i128),
            Some(f) => format!("{f}"),
            None => "NaN".into(),
        }
    }

    /// JS `Number(x)` as it survives `JSON.stringify` — `NaN` becomes `null`.
    pub fn number(v: &Value) -> Value {
        match to_f64(v) {
            Some(f) => from_f64(f),
            None => Value::Null,
        }
    }

    /// Same, but a SQL NULL stays JSON `null` instead of becoming `0` — the
    /// reference guards these columns with `x === null ? null : Number(x)`.
    pub fn number_or_null(v: &Value) -> Value {
        if v.is_null() {
            Value::Null
        } else {
            number(v)
        }
    }

    /// JS `Number(x)`; `None` == `NaN`.
    pub fn to_f64(v: &Value) -> Option<f64> {
        match v {
            Value::Null => Some(0.0),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Number(n) => n.as_f64(),
            Value::String(s) => {
                let t = s.trim();
                if t.is_empty() {
                    Some(0.0)
                } else {
                    t.parse::<f64>().ok()
                }
            }
            _ => None,
        }
    }

    /// Integral f64 → a JSON integer (so `1` never serializes as `1.0`).
    fn from_f64(f: f64) -> Value {
        if f.is_nan() || f.is_infinite() {
            return Value::Null;
        }
        if f == f.trunc() && f.abs() <= 9_007_199_254_740_992.0 {
            return Value::Number(Number::from(f as i64));
        }
        Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
    }

    /// `Number(x) & 0xffff` with JS `ToInt32` semantics: `NaN` / `±Infinity`
    /// become `0`, fractions truncate toward zero, and the result wraps modulo
    /// 2^32 before the mask. A **missing** `addr` therefore queries address 0 —
    /// it does not error.
    pub fn to_u16(v: Option<&Value>) -> u32 {
        let f = match v {
            None => return 0, // Number(undefined) === NaN → NaN & 0xffff === 0
            Some(v) => match to_f64(v) {
                Some(f) => f,
                None => return 0,
            },
        };
        if !f.is_finite() {
            return 0;
        }
        let wrapped = f.trunc().rem_euclid(4_294_967_296.0);
        (wrapped as u64 as u32) & 0xffff
    }

    /// `Math.max(1, Math.min(hi, limit))` with the reference's default-parameter
    /// rule: the default fires ONLY for an absent key (`undefined`), never for
    /// an explicit `null` (which coerces to 0 and clamps up to 1).
    ///
    /// One deliberate simplification: a non-numeric limit (`"abc"`, an object)
    /// makes the reference emit `LIMIT NaN` and fail in the SQL binder; here it
    /// falls back to the default. No caller can produce it — every MCP tool
    /// schema types `limit` as an integer.
    pub fn clamped_limit(v: Option<&Value>, default: u32, hi: u32) -> u32 {
        let Some(v) = v else { return default };
        let Some(f) = to_f64(v) else { return default };
        if f.is_nan() {
            return default;
        }
        let clamped = f.min(hi as f64).max(1.0);
        clamped as u32
    }

    /// `all.slice(0, rowLimit)` — default 200 for an absent key; an explicit
    /// `null` yields 0 rows, as in JS.
    pub fn row_limit(v: Option<&Value>) -> usize {
        let Some(v) = v else { return 200 };
        match to_f64(v) {
            Some(f) if f.is_finite() && f > 0.0 => f as usize,
            Some(f) if f.is_infinite() && f > 0.0 => usize::MAX,
            _ => 0,
        }
    }

    // ── argument coercion for the v2 ops (Spec 802 F2) ───────────────────────

    /// JS `Number(x)` over an ARGUMENT slot: an absent key is `undefined`, and
    /// `Number(undefined)` is `NaN` — NOT `0`. (A present `null` IS `0`.)
    ///
    /// This is the coercion `Number(a.cycle_start)` / `Number(q.limit)` applies
    /// at every v2 op boundary; the distinction between a missing key and an
    /// explicit `null` is load-bearing for the ops' default rules.
    pub fn number_arg(v: Option<&Value>) -> f64 {
        match v {
            None => f64::NAN,
            Some(v) => to_f64(v).unwrap_or(f64::NAN),
        }
    }

    /// The `jsonSafe` number emitter: an integral value serializes WITHOUT a
    /// decimal point (`5000`, not `5000.0`), and `NaN` / `±Infinity` become
    /// `null` — which is what `JSON.stringify` does to them.
    pub fn num(f: f64) -> Value {
        if !f.is_finite() {
            return Value::Null;
        }
        if f.fract() == 0.0 && f.abs() <= 9.007_199_254_740_992e15 {
            return Value::from(f as i64);
        }
        Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
    }

    /// An optional string field of an argument object. A present non-string
    /// coerces via its JSON text; `null` and an absent key are both `None`.
    pub fn opt_str(v: &Value, key: &str) -> Option<String> {
        match v.get(key) {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Null) | None => None,
            Some(other) => Some(other.to_string()),
        }
    }

    /// An optional number field: `None` for an absent key or an explicit
    /// `null`, so a `??`-style default can tell them apart from `0`.
    pub fn opt_num(v: &Value, key: &str) -> Option<f64> {
        match v.get(key) {
            None | Some(Value::Null) => None,
            some => Some(number_arg(some)),
        }
    }

    /// A `[lo, hi]` argument pair (`cycleRange` / `pcRange` / `addrRange`).
    ///
    /// The reference tests the field for TRUTHINESS (`if (q.cycleRange)`), so
    /// `null` / absent → `None`. A present array yields `Number()`-coerced
    /// elements; a missing element is `NaN`, matching `Number(undefined)`.
    pub fn opt_pair(v: &Value, key: &str) -> Option<(f64, f64)> {
        match v.get(key) {
            Some(Value::Array(a)) => Some((number_arg(a.first()), number_arg(a.get(1)))),
            _ => None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── scratch dir ──────────────────────────────────────────────────────────

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let n = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir().join(format!("trx64-queries-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            Scratch(p)
        }
        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Write a store with `sql`, checkpoint, close. The reader reopens it
    /// READ_ONLY through `with_conn`, exactly like the daemon does.
    fn build_store(path: &Path, ddl_batches: &[&str]) {
        let c = Connection::open(path).unwrap();
        for b in ddl_batches {
            c.execute_batch(b).unwrap();
        }
        let _ = c.execute_batch("CHECKPOINT");
        c.close().unwrap();
    }

    // ── Shape B fixture ──────────────────────────────────────────────────────

    const CPU_ROW: &str = r#"{"pc":49152,"opcode":169,"b1":1,"b2":2,"a":3,"x":253,"y":36,"sp":65,"p":0}"#;

    fn shape_b_store(path: &Path) {
        let c = Connection::open(path).unwrap();
        crate::schema::create_trace_run_store(&c).unwrap();
        c.execute_batch(&format!(
            r#"
INSERT INTO trace_run VALUES
  ('run-1','def-a',3,'{{"id":"def-a"}}','probe',NULL,NULL,NULL,NULL,NULL,
   1000,1006,7,4242,NULL,'keep','2026-08-08T10:00:00.000Z');

INSERT INTO trace_event VALUES
  ('run-1',0,1000,'cpu','pc-range','cpu-row','{cpu}'),
  ('run-1',1,1001,'cpu','pc-range','cpu-row','{cpu}'),
  ('run-1',2,1002,'cpu','pc-range','cpu-row','{{"pc":49155,"opcode":234,"b1":0,"b2":0,"a":0,"x":0,"y":0,"sp":255,"p":32}}'),
  ('run-1',3,1003,'bus_access','mem-access','mem-row','{{"addr":53248,"value":7,"op":"write","pc":49152,"side":"c64","oldValue":3,"cycle_c64":1003}}'),
  ('run-1',4,1004,'io','mem-access','mem-row','{{"addr":53248,"value":9,"op":"read","pc":49155,"side":"c64","cycle_c64":1004}}'),
  ('run-1',5,1005,'drive_pc','pc-range','cpu-row','{{"pc":1024,"opcode":76,"b1":0,"b2":16,"a":1,"x":2,"y":3,"sp":250,"p":48,"side":"drive","clk":500}}'),
  ('run-1',6,1006,'iec','iec-transition','iec-row','{{"atn":true,"clk":false,"data":true,"c64_atn":true,"c64_clk":false,"c64_data":false,"drv_clk":false,"drv_data":true,"drv_atn_ack":false}}');

INSERT INTO trace_mark VALUES
  ('run-1',1500,'boot'),
  ('run-1',1600,'boot'),
  ('run-1',1700,'loaded');
"#,
            cpu = CPU_ROW
        ))
        .unwrap();
        let _ = c.execute_batch("CHECKPOINT");
        c.close().unwrap();
    }

    // ── Shape B: every store_fn ──────────────────────────────────────────────

    #[test]
    fn shape_b_get_info() {
        let sc = Scratch::new("info");
        let db = sc.join("t.duckdb");
        shape_b_store(&db);

        let v = get_info(&db).unwrap();
        assert_eq!(
            v["meta"],
            json!({
                "schema": "trace_run/trace_event/trace_mark",
                "source": "live-sink-726",
                "run_id": "run-1",
                "def_id": "def-a",
                "def_version": "3",          // stringified INTEGER — contract
                "retention": "keep",
                "created_at": "2026-08-08T10:00:00.000Z",
            })
        );
        assert_eq!(
            v["tableCounts"],
            json!({
                "events:cpu": 3,
                "events:bus_access": 1,
                "events:io": 1,
                "events:drive_pc": 1,
                "events:iec": 1,
                "events:total": 7,
                "marks": 3,
            })
        );
        assert_eq!(v["masterClockRange"], json!({ "min": 1000, "max": 1006 }));
    }

    #[test]
    fn empty_store_omits_master_clock_range() {
        let sc = Scratch::new("empty");
        let db = sc.join("t.duckdb");
        let c = Connection::open(&db).unwrap();
        crate::schema::create_trace_run_store(&c).unwrap();
        let _ = c.execute_batch("CHECKPOINT");
        c.close().unwrap();

        let v = get_info(&db).unwrap();
        assert!(
            v.as_object().unwrap().get("masterClockRange").is_none(),
            "the key must be OMITTED, not null: {v}"
        );
        assert_eq!(v["tableCounts"]["events:total"], json!(0));
        assert_eq!(v["tableCounts"]["marks"], json!(0));
        // No trace_run row → only the two literal meta entries.
        assert_eq!(
            v["meta"],
            json!({ "schema": "trace_run/trace_event/trace_mark", "source": "live-sink-726" })
        );
    }

    #[test]
    fn shape_b_top_pcs() {
        let sc = Scratch::new("toppcs");
        let db = sc.join("t.duckdb");
        shape_b_store(&db);

        assert_eq!(
            top_pcs(&db, "c64", 20).unwrap(),
            json!([{ "pc": 49152, "count": 2 }, { "pc": 49155, "count": 1 }])
        );
        // The drive row is projected as cpu='drive8' (channel drive_pc).
        assert_eq!(
            top_pcs(&db, "drive8", 20).unwrap(),
            json!([{ "pc": 1024, "count": 1 }])
        );
        // The limit is applied.
        assert_eq!(
            top_pcs(&db, "c64", 1).unwrap(),
            json!([{ "pc": 49152, "count": 2 }])
        );
        // An unknown cpu is not an error — it is an empty result.
        assert_eq!(top_pcs(&db, "undefined", 20).unwrap(), json!([]));
    }

    #[test]
    fn shape_b_find_bus_events() {
        let sc = Scratch::new("bus");
        let db = sc.join("t.duckdb");
        shape_b_store(&db);

        assert_eq!(
            find_bus_events(&db, 0xd000, 100).unwrap(),
            json!([
                { "seq": 3, "cpu": "c64", "kind": "write", "clock": 1003, "pc": 49152, "value": 7 },
                { "seq": 4, "cpu": "c64", "kind": "read",  "clock": 1004, "pc": 49155, "value": 9 },
            ])
        );
        // Masked to 16 bits, like `addr & 0xffff`.
        assert_eq!(
            find_bus_events(&db, 0x12_d000, 100).unwrap(),
            find_bus_events(&db, 0xd000, 100).unwrap()
        );
        assert_eq!(find_bus_events(&db, 0x1234, 100).unwrap(), json!([]));
    }

    #[test]
    fn shape_b_anchors() {
        let sc = Scratch::new("anchors");
        let db = sc.join("t.duckdb");
        shape_b_store(&db);

        // cpu is the STRING "null" and pc is 0 — the reference's String(null) /
        // Number(null) artefacts, visible in trace_store_anchor_list.
        assert_eq!(
            list_anchors(&db).unwrap(),
            json!([
                { "name": "boot",   "cpu": "null", "pc": 0, "occurrences": 2,
                  "firstClock": 1500, "lastClock": 1600 },
                { "name": "loaded", "cpu": "null", "pc": 0, "occurrences": 1,
                  "firstClock": 1700, "lastClock": 1700 },
            ])
        );
        assert_eq!(
            find_anchor(&db, "boot", 200).unwrap(),
            json!([
                { "occurrence": 1, "pc": 0, "clock": 1500, "seq": 1500 },
                { "occurrence": 2, "pc": 0, "clock": 1600, "seq": 1600 },
            ])
        );
        assert_eq!(find_anchor(&db, "boot", 1).unwrap().as_array().unwrap().len(), 1);
        assert_eq!(find_anchor(&db, "nope", 200).unwrap(), json!([]));
    }

    #[test]
    fn find_anchor_sanitizes_the_name_before_touching_the_store() {
        // The gate fires even for a path that has no store at all.
        let e = find_anchor(Path::new("/nope/absent.duckdb"), "boot'; DROP TABLE x --", 200)
            .unwrap_err();
        assert_eq!(e.to_string(), "invalid anchor name: boot'; DROP TABLE x --");
        assert_eq!(
            find_anchor(Path::new("/nope/absent.duckdb"), "", 200)
                .unwrap_err()
                .to_string(),
            "invalid anchor name: "
        );
        // Legal names: alnum + underscore + dash.
        assert!(assert_anchor_name("boot_stage-2").is_ok());
        assert!(assert_anchor_name("bööt").is_err());
    }

    #[test]
    fn shape_b_safe_query() {
        let sc = Scratch::new("safeq");
        let db = sc.join("t.duckdb");
        shape_b_store(&db);

        // Positional row arrays, no column names.
        assert_eq!(
            safe_query(&db, "SELECT channel, seq FROM trace_event ORDER BY seq LIMIT 3", 200).unwrap(),
            json!([["cpu", 0], ["cpu", 1], ["cpu", 2]])
        );
        // The cap is a post-hoc truncation — the SQL is NOT rewritten.
        assert_eq!(
            safe_query(&db, "SELECT seq FROM trace_event ORDER BY seq", 2).unwrap(),
            json!([[0], [1]])
        );
        assert_eq!(safe_query(&db, "SELECT seq FROM trace_event", 0).unwrap(), json!([]));
        // The compat VIEWs are queryable through the same door (map/swimlane rely on it).
        assert_eq!(
            safe_query(&db, "SELECT count(*) FROM bus_events WHERE kind='write'", 200).unwrap(),
            json!([[1]])
        );
    }

    #[test]
    fn safe_query_gate_runs_before_the_store_is_opened() {
        let e = safe_query(Path::new("/nope/absent.duckdb"), "DROP TABLE trace_event", 200)
            .unwrap_err();
        assert_eq!(e.to_string(), "only SELECT/WITH queries are allowed");
        // Ported AS-IS: prefix check only, so a leading comment is rejected …
        assert!(safe_query(Path::new("/nope/absent.duckdb"), "-- x\nSELECT 1", 200)
            .unwrap_err()
            .to_string()
            .starts_with("only SELECT/WITH"));
        // … and WITH passes.
        let sc = Scratch::new("gate");
        let db = sc.join("t.duckdb");
        shape_b_store(&db);
        assert_eq!(
            safe_query(&db, "WITH x AS (SELECT 7 AS n) SELECT n FROM x", 200).unwrap(),
            json!([[7]])
        );
    }

    // ── the op envelope ──────────────────────────────────────────────────────

    #[test]
    fn op_envelope_dispatch_and_arg_nesting() {
        let sc = Scratch::new("op");
        let db = sc.join("t.duckdb");
        shape_b_store(&db);

        // Nested args, and a missing `args` object (the monitor's `getInfo`).
        assert_eq!(
            op_store_fn(&db, &json!({ "fn": "getInfo" })).unwrap()["tableCounts"]["events:total"],
            json!(7)
        );
        assert_eq!(
            op_store_fn(&db, &json!({ "fn": "topPcs", "args": { "cpu": "c64", "limit": 1 } })).unwrap(),
            json!([{ "pc": 49152, "count": 2 }])
        );
        assert_eq!(
            op_store_fn(&db, &json!({ "fn": "findBusEvents", "args": { "addr": 53248 } }))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            op_store_fn(&db, &json!({ "fn": "findAnchor", "args": { "name": "loaded" } })).unwrap(),
            json!([{ "occurrence": 1, "pc": 0, "clock": 1700, "seq": 1700 }])
        );
        assert_eq!(
            op_store_fn(&db, &json!({ "fn": "safeQuery", "args": { "sql": "SELECT 1", "limit": 5 } }))
                .unwrap(),
            json!([[1]])
        );
    }

    #[test]
    fn unknown_fn_message_matches_the_reference() {
        let sc = Scratch::new("unknownfn");
        let db = sc.join("t.duckdb");
        shape_b_store(&db);

        assert_eq!(
            op_store_fn(&db, &json!({ "fn": "sql", "args": { "sql": "SELECT 1" } }))
                .unwrap_err()
                .to_string(),
            "trace/read store_fn: unknown fn \"sql\"",
            "the raw `sql` op is DROPPED — it must report as unknown"
        );
        assert_eq!(
            op_store_fn(&db, &json!({})).unwrap_err().to_string(),
            "trace/read store_fn: unknown fn \"undefined\"",
            "String(undefined) === \"undefined\""
        );
        assert_eq!(
            op_store_fn(&db, &json!({ "fn": null })).unwrap_err().to_string(),
            "trace/read store_fn: unknown fn \"null\""
        );
    }

    #[test]
    fn store_fn_preflights_the_missing_pair() {
        // Neither a store nor an authority → the dispatcher's preflight message,
        // ahead of every per-fn argument gate.
        let e = op_store_fn(
            Path::new("/nope/absent.duckdb"),
            &json!({ "fn": "safeQuery", "args": { "sql": "DROP TABLE t" } }),
        )
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "no trace store and no .c64retrace authority at /nope/absent.duckdb \
             (looked for /nope/absent.c64retrace)"
        );
    }

    #[test]
    fn missing_addr_queries_address_zero_instead_of_erroring() {
        let sc = Scratch::new("addr0");
        let db = sc.join("t.duckdb");
        shape_b_store(&db);
        // Number(undefined) & 0xffff === 0 — the reference does NOT error here.
        assert_eq!(
            op_store_fn(&db, &json!({ "fn": "findBusEvents", "args": {} })).unwrap(),
            json!([])
        );
    }

    // ── Shape A (legacy Spec-217) ────────────────────────────────────────────

    fn shape_a_store(path: &Path) {
        build_store(
            path,
            &[
                crate::schema::SHAPE_A_DDL,
                r#"
INSERT INTO meta VALUES ('schema_version','2'), ('source','vice');

INSERT INTO instructions VALUES
  ('r',0,'c64',10,10,49152,169,1,2,3,4,5,253,32,'vice'),
  ('r',1,'c64',11,11,49152,169,1,2,3,4,5,253,32,'vice'),
  ('r',2,'c64',12,12,49155,234,0,0,0,0,0,253,32,'vice'),
  ('r',3,'drive8',13,13,1024,76,0,16,1,2,3,250,48,'vice');

INSERT INTO bus_events VALUES
  ('r',10,'c64',20,20,49152,'write',53248,7,3,NULL,NULL,NULL,'vice'),
  ('r',11,'c64',21,21,NULL,'read',53248,9,NULL,NULL,NULL,NULL,'vice');

INSERT INTO anchors VALUES
  ('r','vice','c64','entry',49152,1,10,10,0),
  ('r','vice','c64','entry',49152,2,11,11,1);
"#,
            ],
        );
    }

    #[test]
    fn legacy_shape_a_takes_the_legacy_branch() {
        let sc = Scratch::new("legacy");
        let db = sc.join("t.duckdb");
        shape_a_store(&db);

        let v = get_info(&db).unwrap();
        // Legacy getInfo reports the WHOLE meta table and the 5 base-table counts.
        assert_eq!(v["meta"], json!({ "schema_version": "2", "source": "vice" }));
        assert_eq!(
            v["tableCounts"],
            json!({
                "instructions": 4, "bus_events": 2, "chip_events": 0,
                "anchors": 2, "rollups": 0,
            })
        );
        assert_eq!(v["masterClockRange"], json!({ "min": 10, "max": 13 }));

        assert_eq!(
            top_pcs(&db, "c64", 20).unwrap(),
            json!([{ "pc": 49152, "count": 2 }, { "pc": 49155, "count": 1 }])
        );
        assert_eq!(
            find_bus_events(&db, 0xd000, 100).unwrap(),
            json!([
                { "seq": 10, "cpu": "c64", "kind": "write", "clock": 20, "pc": 49152, "value": 7 },
                // a real NULL pc stays null — never omitted, never 0
                { "seq": 11, "cpu": "c64", "kind": "read",  "clock": 21, "pc": null, "value": 9 },
            ])
        );
        // A legacy store carries REAL cpu/pc anchor columns.
        assert_eq!(
            list_anchors(&db).unwrap(),
            json!([{ "name": "entry", "cpu": "c64", "pc": 49152, "occurrences": 2,
                     "firstClock": 10, "lastClock": 11 }])
        );
        assert_eq!(
            find_anchor(&db, "entry", 200).unwrap(),
            json!([
                { "occurrence": 1, "pc": 49152, "clock": 10, "seq": 0 },
                { "occurrence": 2, "pc": 49152, "clock": 11, "seq": 1 },
            ])
        );
    }

    // ── JS coercion helpers ──────────────────────────────────────────────────

    #[test]
    fn js_coercions() {
        use super::js;

        assert_eq!(js::string(&Value::Null), "null");
        assert_eq!(js::string_of(None), "undefined");
        assert_eq!(js::string(&json!(3)), "3");
        assert_eq!(js::string(&json!(3.5)), "3.5");
        assert_eq!(js::string(&json!(true)), "true");

        assert_eq!(js::number(&Value::Null), json!(0));
        assert_eq!(js::number_or_null(&Value::Null), Value::Null);
        assert_eq!(js::number(&json!(12345678901234u64)), json!(12345678901234u64));

        // addr masking, JS ToInt32 style
        assert_eq!(js::to_u16(None), 0);
        assert_eq!(js::to_u16(Some(&Value::Null)), 0);
        assert_eq!(js::to_u16(Some(&json!(0xdd00))), 0xdd00);
        assert_eq!(js::to_u16(Some(&json!(0x1_dd00u32))), 0xdd00);
        assert_eq!(js::to_u16(Some(&json!(-1))), 0xffff);
        assert_eq!(js::to_u16(Some(&json!("$dd00"))), 0); // NaN → 0

        // default fires only for an absent key
        assert_eq!(js::clamped_limit(None, 20, 200), 20);
        assert_eq!(js::clamped_limit(Some(&json!(500)), 20, 200), 200);
        assert_eq!(js::clamped_limit(Some(&json!(0)), 20, 200), 1);
        assert_eq!(js::clamped_limit(Some(&Value::Null), 20, 200), 1);
        assert_eq!(js::clamped_limit(Some(&json!(7)), 20, 200), 7);

        assert_eq!(js::row_limit(None), 200);
        assert_eq!(js::row_limit(Some(&json!(5))), 5);
        assert_eq!(js::row_limit(Some(&Value::Null)), 0);
    }

    // ── a REAL indexed trace (skips cleanly when none is on disk) ────────────

    /// `.c64retrace` files are gitignored, so this takes `TRX64_TEST_RETRACE` or
    /// auto-finds the SMALLEST trace under the repo's own trace dirs. With none
    /// present it prints a SKIP line and passes — it must never fail a clean
    /// clone. Multi-GB captures are skipped unless named explicitly.
    #[test]
    fn real_indexed_trace_answers_every_store_fn() {
        const MAX_AUTO_BYTES: u64 = 64 * 1024 * 1024;

        let retrace = match std::env::var("TRX64_TEST_RETRACE") {
            Ok(p) if Path::new(&p).exists() => PathBuf::from(p),
            _ => match find_smallest_retrace(MAX_AUTO_BYTES) {
                Some(p) => p,
                None => {
                    eprintln!(
                        "SKIP real_indexed_trace_answers_every_store_fn: no .c64retrace found \
                         (set TRX64_TEST_RETRACE=<path> to run it)"
                    );
                    return;
                }
            },
        };

        let sc = Scratch::new("real");
        let db = sc.join("real.duckdb");
        let res = crate::build::index_binary_log(&retrace, &db, None)
            .unwrap_or_else(|e| panic!("indexing {} failed: {e}", retrace.display()));
        eprintln!(
            "real trace {}: {} events, {} marks, {} channels",
            retrace.display(),
            res.event_count,
            res.mark_count,
            res.channels
        );

        // getInfo agrees with the builder's own accounting.
        let info = get_info(&db).unwrap();
        assert_eq!(info["meta"]["source"], json!("live-sink-726"));
        assert_eq!(info["meta"]["run_id"], json!(res.run_id));
        assert_eq!(
            info["tableCounts"]["events:total"],
            json!(res.event_count),
            "getInfo total must equal the indexer's event_count"
        );
        assert_eq!(info["tableCounts"]["marks"], json!(res.mark_count));
        let channel_keys = info["tableCounts"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|k| k.starts_with("events:") && *k != "events:total")
            .count();
        assert_eq!(channel_keys, res.channels);
        if res.event_count > 0 {
            let r = &info["masterClockRange"];
            assert!(r.is_object(), "a non-empty trace must report a cycle range");
            assert!(r["min"].as_u64().unwrap() <= r["max"].as_u64().unwrap());
        }

        // topPcs: ranked, capped, and consistent with a hand-written count.
        let top = top_pcs(&db, "c64", 5).unwrap();
        let top = top.as_array().unwrap();
        assert!(top.len() <= 5);
        let mut prev = u64::MAX;
        for row in top {
            let n = row["count"].as_u64().unwrap();
            assert!(n <= prev, "topPcs must be ordered by count DESC");
            prev = n;
        }
        if let Some(first) = top.first() {
            let pc = first["pc"].as_u64().unwrap();
            let check = safe_query(
                &db,
                &format!(
                    "SELECT count(*) FROM ({INSTRUCTIONS_726}) WHERE cpu='c64' AND pc={pc}"
                ),
                200,
            )
            .unwrap();
            assert_eq!(check[0][0], first["count"]);
        }

        // findBusEvents: every row is the requested address, seq is ascending.
        let addr_row = safe_query(
            &db,
            &format!("SELECT addr FROM ({BUS_EVENTS_726}) WHERE addr IS NOT NULL LIMIT 1"),
            200,
        )
        .unwrap();
        if let Some(a) = addr_row.get(0).and_then(|r| r[0].as_u64()) {
            let ev = find_bus_events(&db, a as u32, 25).unwrap();
            let ev = ev.as_array().unwrap();
            assert!(!ev.is_empty());
            assert!(ev.len() <= 25);
            let mut last = 0u64;
            for e in ev {
                assert!(e["seq"].as_u64().unwrap() >= last);
                last = e["seq"].as_u64().unwrap();
                assert!(matches!(e["cpu"].as_str(), Some("c64") | Some("drive8")));
                assert!(e.get("pc").is_some(), "pc key must exist even when null");
                assert!(e.get("value").is_some());
            }
        }

        // listAnchors / findAnchor round-trip through the marks the trace carries.
        let anchors = list_anchors(&db).unwrap();
        let anchors = anchors.as_array().unwrap();
        let total: u64 = anchors.iter().map(|a| a["occurrences"].as_u64().unwrap()).sum();
        assert_eq!(total, res.mark_count, "anchors must cover every mark");
        for a in anchors {
            assert_eq!(a["cpu"], json!("null"));
            assert_eq!(a["pc"], json!(0));
            let name = a["name"].as_str().unwrap();
            if assert_anchor_name(name).is_ok() {
                let occ = find_anchor(&db, name, 10_000).unwrap();
                assert_eq!(occ.as_array().unwrap().len() as u64, a["occurrences"].as_u64().unwrap());
                for (i, o) in occ.as_array().unwrap().iter().enumerate() {
                    assert_eq!(o["occurrence"], json!(i as u64 + 1));
                }
            }
        }

        // safeQuery still refuses a non-SELECT on a real store.
        assert!(safe_query(&db, "DELETE FROM trace_event", 200).is_err());
    }

    /// Smallest `.c64retrace` under the repo's trace dirs, below `max_bytes`.
    fn find_smallest_retrace(max_bytes: u64) -> Option<PathBuf> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()?
            .parent()?
            .to_path_buf();
        let mut best: Option<(u64, PathBuf)> = None;
        let mut stack = vec![root.join("tools").join("oracle").join("traces"), root.join("traces")];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let Ok(md) = e.metadata() else { continue };
                if md.is_dir() {
                    stack.push(p);
                } else if p.extension().and_then(|s| s.to_str()) == Some("c64retrace")
                    && md.len() <= max_bytes
                    && best.as_ref().map(|(n, _)| md.len() < *n).unwrap_or(true)
                {
                    best = Some((md.len(), p));
                }
            }
        }
        best.map(|(_, p)| p)
    }
}
