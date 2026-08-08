//! Shared store-access helpers — **used by every stage-2 module**
//! (`queries.rs`, `map.rs`, `taint.rs`, `swimlane.rs`).
//!
//! Nothing in stage 2 should open a `Connection` itself: [`with_conn`] is the
//! single entry point and it encodes three rules that are easy to get wrong:
//!
//! 1. **Lazy-on-read index build.** Every read op runs
//!    [`crate::ensure::ensure_index_bounded`] first, so a `.c64retrace` with no
//!    `.duckdb` yet materializes on first read (and a build in flight is waited
//!    on, up to the adaptive bound) instead of failing with "store not found".
//! 2. **`READ_ONLY` first.** A read-write handle takes a cross-process file
//!    lock, so a reader process cannot open a store the daemon is touching
//!    ("Could not set lock"). Read-only needs no exclusive lock and many
//!    concurrent handles are fine.
//! 3. **Heal only when needed.** Fall back to read-write *only* to install the
//!    Spec-726 compat views on an old store that predates them.

use crate::error::{Result, TraceReadError};
use crate::schema::{self, StoreShape};
use duckdb::types::Value;
use duckdb::{AccessMode, Config, Connection};
use std::path::Path;

pub use crate::build::retrace_path_for;

/// Open a trace store and hand a connection + its detected shape to `f`.
///
/// `duckdb_path` is the INDEX path; its `.c64retrace` authority is the sibling
/// (see [`retrace_path_for`]).
pub fn with_conn<T, F>(duckdb_path: &Path, f: F) -> Result<T>
where
    F: FnOnce(&Connection, StoreShape) -> Result<T>,
{
    crate::ensure::ensure_index_bounded(duckdb_path, None)?;
    if !duckdb_path.exists() {
        let retrace = retrace_path_for(duckdb_path);
        return Err(TraceReadError::NoAuthority {
            duckdb: duckdb_path.display().to_string(),
            retrace: retrace.display().to_string(),
        });
    }

    // 1. READ_ONLY.
    if let Ok(conn) = open_read_only(duckdb_path) {
        let shape = schema::store_shape(&conn);
        // A 726 store that already carries the views needs no write access.
        // A legacy Shape-A store has real base tables and needs none either.
        if shape == StoreShape::Legacy217 || schema::has_compat_views(&conn) {
            return f(&conn, shape);
        }
        // else: a 726 store missing its views → fall through and heal.
    }

    // 2. Read-write + heal.
    let conn = Connection::open(duckdb_path).map_err(|e| {
        TraceReadError::duck(format!("open trace store {}", duckdb_path.display()), e)
    })?;
    schema::ensure_spec726_compat_layer(&conn)?;
    let shape = schema::store_shape(&conn);
    f(&conn, shape)
}

/// Open a store `READ_ONLY` (no exclusive lock, safe alongside a live daemon).
pub fn open_read_only(duckdb_path: &Path) -> Result<Connection> {
    let cfg = Config::default()
        .access_mode(AccessMode::ReadOnly)
        .map_err(|e| TraceReadError::duck("configure READ_ONLY", e))?;
    Connection::open_with_flags(duckdb_path, cfg).map_err(|e| {
        TraceReadError::duck(
            format!("open trace store READ_ONLY {}", duckdb_path.display()),
            e,
        )
    })
}

/// Run a query and return every row as JSON values.
///
/// This is the `jsonSafe` down-cast the sidecar applied to every result:
/// DuckDB's 64-bit integers become JSON **numbers** (cycles and seq are far
/// below 2^53), so the shape matches what the TS reader emitted.
pub fn query_json(conn: &Connection, sql: &str) -> Result<Vec<Vec<serde_json::Value>>> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| TraceReadError::duck("prepare query", e))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| TraceReadError::duck("run query", e))?;
    let ncols = rows.as_ref().map(|s| s.column_count()).unwrap_or(0);
    let mut out = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| TraceReadError::duck("fetch row", e))?
    {
        let mut vals = Vec::with_capacity(ncols);
        for i in 0..ncols {
            let v: Value = row
                .get(i)
                .map_err(|e| TraceReadError::duck("read column", e))?;
            vals.push(value_to_json(&v));
        }
        out.push(vals);
    }
    Ok(out)
}

/// Run a **parameterised** SELECT and return each row as a JSON OBJECT keyed by
/// column name.
///
/// This is the Rust twin of the TS reader backend's
/// `exec(sql, params): Promise<any[]>` — the v2 ops (`query_events`,
/// `follow_path`, `profile_loader`) are written against row objects
/// (`r.clock`, `r.pc`, `r.addr`, …), so porting their `rowFromDb` mappers is a
/// transcription rather than a redesign. Placeholders are `?`, positional, in
/// `params` order — exactly the shape `queryEvents` builds.
///
/// `params` are serde JSON values and map as: string → `Text`, integral number
/// → `BigInt`, other number → `Double`, bool → `Boolean`, null → `Null`.
/// Anything else is bound as its JSON text (no caller produces it).
///
/// Errors are bare DuckDB messages (Spec 802 F1) — same as [`query_json`].
pub fn query_named(
    conn: &Connection,
    sql: &str,
    params: &[serde_json::Value],
) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
    let bound: Vec<Value> = params.iter().map(json_to_duck).collect();
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| TraceReadError::duck("prepare query", e))?;
    let mut rows = stmt
        .query(duckdb::params_from_iter(bound.iter()))
        .map_err(|e| TraceReadError::duck("run query", e))?;
    let names: Vec<String> = rows
        .as_ref()
        .map(|s| s.column_names())
        .unwrap_or_default();
    let mut out = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| TraceReadError::duck("fetch row", e))?
    {
        let mut obj = serde_json::Map::new();
        for (i, name) in names.iter().enumerate() {
            let v: Value = row
                .get(i)
                .map_err(|e| TraceReadError::duck("read column", e))?;
            obj.insert(name.clone(), value_to_json(&v));
        }
        out.push(obj);
    }
    Ok(out)
}

/// serde JSON → a DuckDB bind value. See [`query_named`].
pub fn json_to_duck(v: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    match v {
        J::Null => Value::Null,
        J::Bool(b) => Value::Boolean(*b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::BigInt(i)
            } else if let Some(f) = n.as_f64() {
                // JS has one number type; an integral f64 must still bind as an
                // integer so `clock BETWEEN ? AND ?` compares against BIGINT.
                if f.fract() == 0.0 && f.abs() <= 9.007_199_254_740_992e15 {
                    Value::BigInt(f as i64)
                } else {
                    Value::Double(f)
                }
            } else {
                Value::Null
            }
        }
        J::String(s) => Value::Text(s.clone()),
        other => Value::Text(other.to_string()),
    }
}

/// Column names of a query, in ordinal order.
pub fn query_columns(conn: &Connection, sql: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| TraceReadError::duck("prepare query", e))?;
    let rows = stmt
        .query([])
        .map_err(|e| TraceReadError::duck("run query", e))?;
    Ok(rows.as_ref().map(|s| s.column_names()).unwrap_or_default())
}

/// DuckDB value → JSON. Integers stay integers; `NULL` becomes `null`.
pub fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Null => J::Null,
        Value::Boolean(b) => J::Bool(*b),
        Value::TinyInt(n) => J::from(*n),
        Value::SmallInt(n) => J::from(*n),
        Value::Int(n) => J::from(*n),
        Value::BigInt(n) => J::from(*n),
        Value::UTinyInt(n) => J::from(*n),
        Value::USmallInt(n) => J::from(*n),
        Value::UInt(n) => J::from(*n),
        Value::UBigInt(n) => J::from(*n),
        Value::HugeInt(n) => J::from(*n as i64),
        Value::Float(f) => serde_json::Number::from_f64(*f as f64).map(J::Number).unwrap_or(J::Null),
        Value::Double(f) => serde_json::Number::from_f64(*f).map(J::Number).unwrap_or(J::Null),
        Value::Text(s) => J::String(s.clone()),
        Value::Blob(b) => J::String(format!("<blob {} bytes>", b.len())),
        other => J::String(format!("{other:?}")),
    }
}

/// Scalar helpers for the common single-value queries.
pub fn scalar_u64(conn: &Connection, sql: &str) -> Result<Option<u64>> {
    let rows = query_json(conn, sql)?;
    Ok(rows
        .first()
        .and_then(|r| r.first())
        .and_then(|v| v.as_u64()))
}

pub fn scalar_string(conn: &Connection, sql: &str) -> Result<Option<String>> {
    let rows = query_json(conn, sql)?;
    Ok(rows.first().and_then(|r| r.first()).and_then(|v| match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }))
}

/// `safeQuery`'s prefix gate, ported as-is: only `SELECT` / `WITH`, and the
/// caller caps rows at 200 by default.
///
/// Ported verbatim by owner decision; the raw `sql` op is DROPPED with the port
/// (it had no caller).
pub fn assert_select_only(sql: &str) -> Result<()> {
    let lc = sql.trim_start().to_ascii_lowercase();
    if lc.starts_with("select") || lc.starts_with("with") {
        Ok(())
    } else {
        Err(TraceReadError::other("only SELECT/WITH queries are allowed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn select_gate() {
        assert!(assert_select_only("SELECT 1").is_ok());
        assert!(assert_select_only("  with x as (select 1) select * from x").is_ok());
        let e = assert_select_only("DROP TABLE trace_event").unwrap_err();
        assert_eq!(e.to_string(), "only SELECT/WITH queries are allowed");
        assert!(assert_select_only("INSERT INTO meta VALUES ('a','b')").is_err());
    }

    #[test]
    fn json_value_mapping() {
        assert_eq!(value_to_json(&Value::UBigInt(12345)), serde_json::json!(12345));
        assert_eq!(value_to_json(&Value::Null), serde_json::Value::Null);
        assert_eq!(value_to_json(&Value::Boolean(true)), serde_json::json!(true));
        assert_eq!(
            value_to_json(&Value::Text("cpu".into())),
            serde_json::json!("cpu")
        );
    }

    // ── Spec 802 F1 — DuckDB errors reach the caller BARE ────────────────────
    //
    // The sidecar surfaced the driver's `e.message` unmodified. These pin the
    // exact caller-visible text for the two measured cases; the earlier native
    // reader prefixed them with `prepare query: ` / `open trace store <p>: `.

    const BINDER_ERR: &str =
        "Binder Error: Referenced column \"nope_col\" not found in FROM clause!";

    #[test]
    fn f1_query_error_is_the_bare_duckdb_message() {
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        conn.execute_batch("CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1);")
            .expect("fixture");

        let e = query_json(&conn, "SELECT nope_col FROM t").expect_err("binder error");
        let msg = e.to_string();
        assert!(
            msg.starts_with(BINDER_ERR),
            "expected the bare driver message, got: {msg}"
        );
        assert!(
            !msg.contains("prepare query"),
            "internal context leaked into the caller-visible message: {msg}"
        );
        // The context is not lost — it is just not part of `Display`.
        assert_eq!(e.context(), Some("prepare query"));
        assert!(format!("{e:?}").contains("prepare query"));

        // Same for the columns probe and the parameterised row reader.
        let e2 = query_columns(&conn, "SELECT nope_col FROM t").expect_err("binder error");
        assert!(e2.to_string().starts_with(BINDER_ERR), "{e2}");
        let e3 = query_named(&conn, "SELECT nope_col FROM t WHERE a = ?", &[json!(1)])
            .expect_err("binder error");
        assert!(e3.to_string().starts_with(BINDER_ERR), "{e3}");
    }

    #[test]
    fn f1_open_error_is_the_bare_duckdb_message() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("trx64-f1-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("not-a-store.duckdb");
        std::fs::write(&p, b"definitely not a duckdb database").unwrap();

        let e = open_read_only(&p).expect_err("not a duckdb file");
        let msg = e.to_string();
        assert!(
            msg.starts_with("IO Error: The file \""),
            "expected the bare driver message, got: {msg}"
        );
        assert!(
            msg.contains("is not a valid DuckDB database file!"),
            "unexpected driver text: {msg}"
        );
        assert!(
            !msg.contains("open trace store"),
            "internal context leaked into the caller-visible message: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn f1_index_build_errors_keep_their_context() {
        // The one deliberate exception: the index-build path is native-only
        // (the sidecar delegated to C64RE's TS indexer), so naming the failing
        // build step is a gain, not a parity break.
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        let raw = conn.prepare("SELECT nope_col FROM (SELECT 1 AS a)").unwrap_err();
        let e = TraceReadError::duck_build("insert trace_run", raw);
        assert!(
            e.to_string().starts_with("insert trace_run: Binder Error:"),
            "got: {e}"
        );
    }
}
