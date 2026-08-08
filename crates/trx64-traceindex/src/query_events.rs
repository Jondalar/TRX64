//! `query_events` — the typed event query op (Spec 802 F2 / C64RE Spec 232).
//!
//! Reference: `src/runtime/headless/v2/query-events.ts` in the C64RE tree
//! (`queryEvents(backend, q)`), reached from the retired sidecar as
//! `case "query_events"`. Its `backend` is `DuckDbQueryBackend`
//! (`v2/duckdb-backend.ts`), whose `exec` **inlines** the `?` parameters as SQL
//! literals instead of binding them — see [`fill_placeholders`] for why that
//! detail is reproduced here rather than replaced with real bind parameters.
//!
//! # This module is a DEPENDENCY of the other two F2 ops
//!
//! `follow_path` and `profile_loader` are written entirely in terms of
//! [`query_events`] — they issue nothing but `queryEvents(...)` calls, exactly
//! as their TypeScript originals do. So the items marked **FROZEN CONTRACT**
//! below ([`EventFamily`], [`EventQuery`], [`EventRow`] and the signature of
//! [`query_events`]) are shared surface: changing them breaks a sibling
//! module. Fill the body; leave the shape.
//!
//! # Argument case — camelCase
//!
//! `query_events` takes **camelCase** args (`runId`, `cycleRange`, …) because
//! the sidecar forwarded the WS `args` object straight into the TS query
//! object. `taint` and `follow_path` do the same; every OTHER op takes
//! snake_case. Getting this wrong is a silent parity break (R3 §5).

use crate::conn::{query_named, with_conn};
use crate::error::{Result, TraceReadError};
use crate::queries::js;
use crate::schema::{StoreShape, BUS_EVENTS_726, INSTRUCTIONS_726};
use duckdb::Connection;
use serde_json::{Map, Value};
use std::path::Path;

// ─────────────────────────────────────────────────────────────────────────────
// FROZEN CONTRACT — shared with follow_path.rs and profile_loader.rs
// ─────────────────────────────────────────────────────────────────────────────

/// One result row, in the shape the reference emits it: a JSON object.
///
/// The reference's `EventRow` is a discriminated union whose members differ per
/// family (`cpu_step` carries `pc`/`opcode`/`a`/`x`/`y`/`sp`/`flags`,
/// `mem_read` carries `pc`/`addr`/`value`/`region`, …). A JSON object models
/// that exactly and keeps the ops that CONSUME rows (`follow_path`,
/// `profile_loader`) able to read a field without knowing the family — which is
/// what their TS originals do (`(row as any)[k]`).
///
/// Use [`row_num`] / [`row_cycle`] / [`row_family`] to read one.
pub type EventRow = Map<String, Value>;

/// The event families the reference's `MAP` knows.
///
/// A family outside this list is NOT an error: `MAP[q.family]` misses and
/// `queryEvents` returns `[]`. That is why every entry point takes
/// `Option<EventFamily>` — `None` means "unknown or absent" and must yield an
/// empty result, never a failure.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum EventFamily {
    CpuStep,
    MemRead,
    MemWrite,
    IrqAssert,
    IrqAck,
    NmiAssert,
    CiaTimerUnderflow,
    DriveAtnChange,
    DriveClkChange,
    DriveDataChange,
    GcrByte,
    TrapFire,
    // ── families the reference maps to `null` (no producer) → always `[]` ────
    CpuJam,
    MemIndirectResolve,
    ResetAssert,
    VicBadline,
    VicRasterIrq,
    VicSpriteCollision,
    VicDmaSteal,
    CiaRegisterRead,
    CiaRegisterWrite,
    ViaTimerUnderflow,
    ViaRegisterRead,
    ViaRegisterWrite,
    SidRegisterWrite,
    KeyboardPress,
    KeyboardRelease,
    HookAudit,
    BreakpointHit,
}

impl EventFamily {
    /// The wire name — also the value of the row's `family` field.
    pub fn as_str(self) -> &'static str {
        use EventFamily::*;
        match self {
            CpuStep => "cpu_step",
            MemRead => "mem_read",
            MemWrite => "mem_write",
            IrqAssert => "irq_assert",
            IrqAck => "irq_ack",
            NmiAssert => "nmi_assert",
            CiaTimerUnderflow => "cia_timer_underflow",
            DriveAtnChange => "drive_atn_change",
            DriveClkChange => "drive_clk_change",
            DriveDataChange => "drive_data_change",
            GcrByte => "gcr_byte",
            TrapFire => "trap_fire",
            CpuJam => "cpu_jam",
            MemIndirectResolve => "mem_indirect_resolve",
            ResetAssert => "reset_assert",
            VicBadline => "vic_badline",
            VicRasterIrq => "vic_raster_irq",
            VicSpriteCollision => "vic_sprite_collision",
            VicDmaSteal => "vic_dma_steal",
            CiaRegisterRead => "cia_register_read",
            CiaRegisterWrite => "cia_register_write",
            ViaTimerUnderflow => "via_timer_underflow",
            ViaRegisterRead => "via_register_read",
            ViaRegisterWrite => "via_register_write",
            SidRegisterWrite => "sid_register_write",
            KeyboardPress => "keyboard_press",
            KeyboardRelease => "keyboard_release",
            HookAudit => "hook_audit",
            BreakpointHit => "breakpoint_hit",
        }
    }

    /// Wire name → family. `None` for anything the reference's `MAP` has no key
    /// for (including an absent argument) → the caller must return `[]`.
    pub fn from_name(s: &str) -> Option<Self> {
        use EventFamily::*;
        Some(match s {
            "cpu_step" => CpuStep,
            "mem_read" => MemRead,
            "mem_write" => MemWrite,
            "irq_assert" => IrqAssert,
            "irq_ack" => IrqAck,
            "nmi_assert" => NmiAssert,
            "cia_timer_underflow" => CiaTimerUnderflow,
            "drive_atn_change" => DriveAtnChange,
            "drive_clk_change" => DriveClkChange,
            "drive_data_change" => DriveDataChange,
            "gcr_byte" => GcrByte,
            "trap_fire" => TrapFire,
            "cpu_jam" => CpuJam,
            "mem_indirect_resolve" => MemIndirectResolve,
            "reset_assert" => ResetAssert,
            "vic_badline" => VicBadline,
            "vic_raster_irq" => VicRasterIrq,
            "vic_sprite_collision" => VicSpriteCollision,
            "vic_dma_steal" => VicDmaSteal,
            "cia_register_read" => CiaRegisterRead,
            "cia_register_write" => CiaRegisterWrite,
            "via_timer_underflow" => ViaTimerUnderflow,
            "via_register_read" => ViaRegisterRead,
            "via_register_write" => ViaRegisterWrite,
            "sid_register_write" => SidRegisterWrite,
            "keyboard_press" => KeyboardPress,
            "keyboard_release" => KeyboardRelease,
            "hook_audit" => HookAudit,
            "breakpoint_hit" => BreakpointHit,
            _ => return None,
        })
    }

    /// The reference's `MAP[family]`, minus its `rowFromDb` projection.
    ///
    /// `None` = the family is mapped to `null` (declared, no producer) →
    /// `queryEvents` returns `[]` without touching the store.
    pub fn mapping(self) -> Option<FamilyMapping> {
        use EventFamily::*;
        let m = |table, kind_filter| FamilyMapping { table, kind_filter, chip_filter: None };
        Some(match self {
            CpuStep => m(EventTable::Instructions, None),
            MemRead => m(EventTable::BusEvents, Some("read")),
            MemWrite => m(EventTable::BusEvents, Some("write")),
            IrqAssert => m(EventTable::ChipEvents, Some("irq_assert")),
            IrqAck => m(EventTable::ChipEvents, Some("irq_ack")),
            NmiAssert => m(EventTable::ChipEvents, Some("nmi_assert")),
            CiaTimerUnderflow => m(EventTable::ChipEvents, Some("timer_underflow")),
            // All three IEC line families read the SAME `line_change` rows and
            // differ only in which column the projection turns into `level`.
            DriveAtnChange | DriveClkChange | DriveDataChange => {
                m(EventTable::BusEvents, Some("line_change"))
            }
            GcrByte => m(EventTable::ChipEvents, Some("byte_ready")),
            TrapFire => m(EventTable::ChipEvents, Some("trap_fire")),
            _ => return None,
        })
    }
}

/// The logical source table of a family.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventTable {
    Instructions,
    BusEvents,
    ChipEvents,
}

/// `MAP`'s value, minus `rowFromDb`.
#[derive(Clone, Copy, Debug)]
pub struct FamilyMapping {
    pub table: EventTable,
    /// `chip_events.kind` / `bus_events.kind` value to filter on.
    pub kind_filter: Option<&'static str>,
    /// `chip_events.chip` filter. **No entry of the reference's `MAP` sets
    /// it** — the field is declared and every mapping leaves it undefined, so
    /// the `chip = ?` branch is dead. Kept so the ported SQL builder can mirror
    /// the reference branch-for-branch.
    pub chip_filter: Option<&'static str>,
}

/// The `EventQuery` of query-events.ts — **camelCase on the wire**.
#[derive(Clone, Debug, Default)]
pub struct EventQuery {
    /// Bound as the first `?` (`run_id = ?`). `None` binds SQL `NULL`, which is
    /// what the reference's `undefined` param does.
    pub run_id: Option<String>,
    /// `None` = absent or unknown → the result is `[]` (see [`EventFamily`]).
    pub family: Option<EventFamily>,
    pub cycle_range: Option<(f64, f64)>,
    pub pc_range: Option<(f64, f64)>,
    pub addr_range: Option<(f64, f64)>,
    /// A raw SQL `WHERE` fragment. **Already normalised for JS truthiness**:
    /// an empty string is `None` here, because `if (q.predicate)` is false for
    /// `""`. The forbidden-token gate is NOT applied here — it belongs in
    /// [`query_events`], where its error must surface as
    /// `predicate contains forbidden tokens`.
    pub predicate: Option<String>,
    /// Raw, uncoerced. Apply [`EventQuery::effective_limit`], never this.
    pub limit: Option<f64>,
}

impl EventQuery {
    /// Parse the WS `args` object. **camelCase** (R3 §5).
    pub fn from_camel(v: &Value) -> Self {
        EventQuery {
            run_id: js::opt_str(v, "runId"),
            family: match v.get("family") {
                Some(Value::String(s)) => EventFamily::from_name(s),
                _ => None,
            },
            cycle_range: js::opt_pair(v, "cycleRange"),
            pc_range: js::opt_pair(v, "pcRange"),
            addr_range: js::opt_pair(v, "addrRange"),
            predicate: match v.get("predicate") {
                // JS truthiness: `""` is falsy, so an empty predicate is absent.
                Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                _ => None,
            },
            limit: js::opt_num(v, "limit"),
        }
    }

    /// `const limit = q.limit && q.limit > 0 && q.limit <= 100000 ? q.limit : 10000`
    ///
    /// Note the JS truthiness guard in front: `0`, `NaN`, absent and `null` all
    /// fall through to the default 10000 rather than producing `LIMIT 0`.
    pub fn effective_limit(&self) -> f64 {
        match self.limit {
            Some(l) if l != 0.0 && !l.is_nan() && l > 0.0 && l <= 100_000.0 => l,
            _ => 10_000.0,
        }
    }
}

// ── row accessors (the `(row as any)[k]` of the TS consumers) ────────────────

/// The row's `family` field, or `""` when absent.
pub fn row_family(row: &EventRow) -> &str {
    row.get("family").and_then(Value::as_str).unwrap_or("")
}

/// A numeric field of a row. `None` when absent or not a number.
pub fn row_num(row: &EventRow, key: &str) -> Option<f64> {
    row.get(key).and_then(Value::as_f64)
}

/// The row's `cycle`, or `NaN` when absent (all rows carry one).
pub fn row_cycle(row: &EventRow) -> f64 {
    row_num(row, "cycle").unwrap_or(f64::NAN)
}

/// The `FROM` source for a logical table on this store shape.
///
/// On a Spec-726 live-sink store the compat views are bypassed and the reader
/// projects straight out of `trace_event` (Spec 726 §6a: readers never name the
/// legacy tables), so the source is the projection SQL wrapped in parentheses.
/// `chip_events` has NO 726 projection — the reference returns `[]` for it
/// rather than querying, which is what `None` means here.
pub fn from_source(table: EventTable, shape: StoreShape) -> Option<String> {
    if shape != StoreShape::Spec726 {
        return Some(
            match table {
                EventTable::Instructions => "instructions",
                EventTable::BusEvents => "bus_events",
                EventTable::ChipEvents => "chip_events",
            }
            .to_string(),
        );
    }
    match table {
        EventTable::Instructions => Some(format!("({INSTRUCTIONS_726})")),
        EventTable::BusEvents => Some(format!("({BUS_EVENTS_726})")),
        EventTable::ChipEvents => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The op — `queryEvents()`, ported
// ─────────────────────────────────────────────────────────────────────────────

// ── SQL construction (the reference's `where` / `params` builder) ────────────

/// The `?`-placeholder SQL of one query plus its parameters, in the reference's
/// exact clause ORDER. Split out of [`query_events`] so a test can pin the
/// generated text without touching a store.
///
/// `None` = the query never runs (`MAP[family] === null`, or `chip_events` on a
/// 726 store) and the caller must return `[]`.
fn build_sql(shape: StoreShape, q: &EventQuery) -> Result<Option<(String, Vec<Value>)>> {
    let Some(family) = q.family else {
        return Ok(None);
    };
    let Some(mapping) = family.mapping() else {
        return Ok(None);
    };
    let Some(from) = from_source(mapping.table, shape) else {
        return Ok(None);
    };

    // NOTE the ORDER of these pushes: it fixes both the WHERE text and the
    // parameter positions, and the reference is the only authority for it.
    let mut wheres: Vec<String> = vec!["run_id = ?".into()];
    let mut params: Vec<Value> = vec![match &q.run_id {
        // `params.push(q.runId)` with an absent `runId` pushes `undefined`,
        // which the backend inlines as SQL `NULL` — `run_id = NULL` then
        // matches nothing. Faithfully reproduced (R3 §3's live taint defect
        // depends on it).
        Some(s) => Value::String(s.clone()),
        None => Value::Null,
    }];

    if let Some(kind) = mapping.kind_filter {
        wheres.push("kind = ?".into());
        params.push(Value::String(kind.to_string()));
    }
    if let Some(chip) = mapping.chip_filter {
        // Dead in practice: no `MAP` entry sets `chipFilter`. Kept branch-for-branch.
        wheres.push("chip = ?".into());
        params.push(Value::String(chip.to_string()));
    }
    if let Some((lo, hi)) = q.cycle_range {
        wheres.push("clock BETWEEN ? AND ?".into());
        params.push(num_param(lo));
        params.push(num_param(hi));
    }
    if let Some((lo, hi)) = q.pc_range {
        if matches!(mapping.table, EventTable::BusEvents | EventTable::Instructions) {
            wheres.push("pc BETWEEN ? AND ?".into());
            params.push(num_param(lo));
            params.push(num_param(hi));
        }
    }
    if let Some((lo, hi)) = q.addr_range {
        if mapping.table == EventTable::BusEvents {
            wheres.push("addr BETWEEN ? AND ?".into());
            params.push(num_param(lo));
            params.push(num_param(hi));
        }
    }
    if let Some(pred) = &q.predicate {
        // The reference's sandbox gate, verbatim: `/[;]|UNION|DROP|DELETE|INSERT|UPDATE/i`
        // — a plain case-insensitive SUBSTRING test, so `dropped = 1` is
        // rejected too. Not hardened, not loosened (R3 §6 error catalogue).
        if predicate_has_forbidden_tokens(pred) {
            return Err(TraceReadError::other("predicate contains forbidden tokens"));
        }
        wheres.push(format!("({pred})"));
    }

    // `LIMIT ${limit}` is INTERPOLATED, not a parameter — via JS `String(n)`,
    // so an integral limit never renders as `10000.0`.
    let limit = js::string(&js::num(q.effective_limit()));
    let sql = format!(
        "SELECT * FROM {from} WHERE {} ORDER BY clock LIMIT {limit}",
        wheres.join(" AND ")
    );
    Ok(Some((sql, params)))
}

/// A numeric parameter as the backend would receive it.
///
/// `NaN` becomes JSON `null` → inlined as `NULL`, which is what the reference
/// does for a MISSING range element (`Number(undefined)` never happens there —
/// `q.cycleRange[1]` is `undefined` and `inlineParam(undefined)` is `"NULL"`).
/// A literally non-finite number cannot reach here from JSON.
fn num_param(f: f64) -> Value {
    js::num(f)
}

/// `/[;]|UNION|DROP|DELETE|INSERT|UPDATE/i.test(predicate)`.
fn predicate_has_forbidden_tokens(predicate: &str) -> bool {
    if predicate.contains(';') {
        return true;
    }
    let up = predicate.to_ascii_uppercase();
    ["UNION", "DROP", "DELETE", "INSERT", "UPDATE"]
        .iter()
        .any(|t| up.contains(t))
}

/// The backend's `exec()` parameter INLINING (`duckdb-backend.ts`), reproduced.
///
/// # Why inline instead of binding
///
/// `DuckDbQueryBackend.exec` does `sql.replace(/\?/g, inlineParam)` and then
/// runs a parameterless statement. Binding the same values through a prepared
/// statement is *not* equivalent: DuckDB resolves an untyped `?` to the type of
/// the column it is compared against, so `pc BETWEEN ? AND ?` (pc is
/// `USMALLINT`) would reject a `pcRange` of `[0, 0x10000]` with a conversion
/// error, and `clock BETWEEN ? AND ?` (UBIGINT) would reject a negative
/// `cycleRange`. The reference's inlined integer LITERALS widen instead and
/// return rows. Inlining keeps that behaviour — and makes the executed SQL text
/// identical to the reference's, which is what the parity run compares against.
///
/// The count check reproduces `param count mismatch: sql has <n> placeholders,
/// <m> params` (R3 §6): the only way to trip it is a `?` inside `predicate`,
/// which the token gate does not forbid.
fn fill_placeholders(sql: &str, params: &[Value]) -> Result<String> {
    let mut out = String::with_capacity(sql.len() + 32);
    let mut i = 0usize;
    for ch in sql.chars() {
        if ch == '?' {
            // `params[i++]` past the end is `undefined` → `"NULL"`, and `i`
            // keeps counting — that is what the mismatch message reports.
            out.push_str(&inline_param(params.get(i)));
            i += 1;
        } else {
            out.push(ch);
        }
    }
    if i != params.len() {
        return Err(TraceReadError::other(format!(
            "param count mismatch: sql has {i} placeholders, {} params",
            params.len()
        )));
    }
    Ok(out)
}

/// `inlineParam(p)` of `duckdb-backend.ts`.
fn inline_param(p: Option<&Value>) -> String {
    match p {
        // `p === null || p === undefined`
        None | Some(Value::Null) => "NULL".into(),
        Some(Value::Bool(b)) => if *b { "TRUE" } else { "FALSE" }.into(),
        // `String(p)` — integral values render without a decimal point.
        Some(v @ Value::Number(_)) => js::string(v),
        Some(Value::String(s)) => format!("'{}'", s.replace('\'', "''")),
        // The reference throws `unsupported param type: object` here. No
        // parameter this module builds can be an array or an object, so the
        // arm is unreachable; inline the JSON text rather than add a dead
        // error path.
        Some(other) => format!("'{}'", other.to_string().replace('\'', "''")),
    }
}

// ── row projection (the reference's per-family `rowFromDb`) ──────────────────

/// `Number(row[key])` as it survives `JSON.stringify`: a missing column is
/// `NaN` → `null`, a SQL `NULL` is `0` (`Number(null) === 0`).
fn n(row: &Map<String, Value>, key: &str) -> Value {
    match row.get(key) {
        None => Value::Null,
        Some(v) => js::number(v),
    }
}

/// `Number(row[key] ?? 0)` — a missing column and a SQL `NULL` both read as 0.
fn n0(row: &Map<String, Value>, key: &str) -> Value {
    match row.get(key) {
        None | Some(Value::Null) => js::num(0.0),
        Some(v) => js::number(v),
    }
}

/// `row[key] ?? fallback` — `??` fires on `null` and on a missing column only.
fn coalesce(row: &Map<String, Value>, key: &str, fallback: &str) -> Value {
    match row.get(key) {
        None | Some(Value::Null) => Value::String(fallback.to_string()),
        Some(v) => v.clone(),
    }
}

/// JS truthiness of `row[key]` — the `r.line_atn ? 1 : 0` test.
fn truthy(row: &Map<String, Value>, key: &str) -> bool {
    match row.get(key) {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::Number(x)) => x.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// One DB row → one `EventRow`, per `MAP[family].rowFromDb`.
///
/// `runId` is the QUERY's `runId`, echoed back — not the row's column. An
/// absent one is an absent KEY (`JSON.stringify` drops `undefined`), which is
/// unobservable in practice because `run_id = NULL` returns no rows.
fn row_from_db(family: EventFamily, r: &Map<String, Value>, run_id: Option<&str>) -> EventRow {
    use EventFamily::*;
    let mut out = Map::new();
    if let Some(id) = run_id {
        out.insert("runId".into(), Value::String(id.to_string()));
    }
    out.insert("family".into(), Value::String(family.as_str().into()));
    out.insert("cycle".into(), n(r, "clock"));

    match family {
        CpuStep => {
            out.insert("pc".into(), n(r, "pc"));
            out.insert("opcode".into(), n(r, "opcode"));
            out.insert("a".into(), n(r, "a"));
            out.insert("x".into(), n(r, "x"));
            out.insert("y".into(), n(r, "y"));
            out.insert("sp".into(), n(r, "sp"));
            out.insert("flags".into(), n(r, "p"));
        }
        MemRead | MemWrite => {
            out.insert("pc".into(), n0(r, "pc"));
            out.insert("addr".into(), n0(r, "addr"));
            out.insert("value".into(), n0(r, "value"));
            out.insert("region".into(), Value::String("ram".into()));
        }
        IrqAssert | IrqAck | NmiAssert => {
            out.insert("source".into(), coalesce(r, "chip", "manual"));
        }
        CiaTimerUnderflow => {
            // `chip: r.chip` — the RAW column, `null` included; only a missing
            // column drops the key.
            if let Some(v) = r.get("chip") {
                out.insert("chip".into(), v.clone());
            }
            // `Number(r.unit) === 0 ? "ta" : "tb"` — a missing column is NaN → "tb".
            let ta = matches!(n(r, "unit"), Value::Number(ref x) if x.as_f64() == Some(0.0));
            out.insert("timer".into(), Value::String(if ta { "ta" } else { "tb" }.into()));
        }
        DriveAtnChange | DriveClkChange | DriveDataChange => {
            let line = match family {
                DriveAtnChange => "line_atn",
                DriveClkChange => "line_clk",
                _ => "line_data",
            };
            out.insert("level".into(), Value::from(if truthy(r, line) { 1 } else { 0 }));
        }
        GcrByte => {
            out.insert("byte".into(), n(r, "value"));
            out.insert("trackHalf".into(), n0(r, "unit"));
        }
        TrapFire => {
            // `String(r.chip ?? "unknown")`
            out.insert("hookName".into(), Value::String(js::string(&coalesce(r, "chip", "unknown"))));
        }
        // Unreachable: these families have no `mapping()`, so no row exists.
        _ => {}
    }
    out
}

// ── the op ──────────────────────────────────────────────────────────────────

/// Run one event query against an OPEN store.
///
/// Takes a `&Connection` rather than a path because `follow_path` and
/// `profile_loader` issue dozens of these per request and must reuse the single
/// connection their op opened — the reference does the same (one `withDuckDb`,
/// many `exec`).
///
/// Ties on `clock` are compared order-insensitively by the parity gate
/// (R3 §7), so no extra `ORDER BY` tiebreaker may be added.
///
/// **Guard ORDER is load-bearing:** an unmapped family and `chip_events` on a
/// 726 store return `[]` *before* the predicate is inspected, so a forbidden
/// predicate on `cpu_jam` is an empty result, not an error.
pub fn query_events(conn: &Connection, shape: StoreShape, q: &EventQuery) -> Result<Vec<EventRow>> {
    let Some((sql, params)) = build_sql(shape, q)? else {
        return Ok(Vec::new());
    };
    let family = q.family.expect("build_sql returned Some ⇒ family is mapped");
    let filled = fill_placeholders(&sql, &params)?;
    let rows = query_named(conn, &filled, &[])?;
    Ok(rows
        .iter()
        .map(|r| row_from_db(family, r, q.run_id.as_deref()))
        .collect())
}

/// Op wrapper: `query_events` (camelCase args) → a JSON array of rows.
pub fn op_query_events(duckdb_path: &Path, args: &Value) -> Result<Value> {
    let q = EventQuery::from_camel(args);
    with_conn(duckdb_path, |conn, shape| {
        let rows = query_events(conn, shape, &q)?;
        Ok(Value::Array(rows.into_iter().map(Value::Object).collect()))
    })
}

// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn family_names_round_trip() {
        for name in [
            "cpu_step",
            "mem_read",
            "mem_write",
            "irq_assert",
            "irq_ack",
            "nmi_assert",
            "cia_timer_underflow",
            "drive_atn_change",
            "drive_clk_change",
            "drive_data_change",
            "gcr_byte",
            "trap_fire",
            "cpu_jam",
            "breakpoint_hit",
        ] {
            let f = EventFamily::from_name(name).expect(name);
            assert_eq!(f.as_str(), name);
        }
        assert_eq!(EventFamily::from_name("not_a_family"), None);
    }

    #[test]
    fn producerless_families_have_no_mapping() {
        assert!(EventFamily::CpuJam.mapping().is_none());
        assert!(EventFamily::VicBadline.mapping().is_none());
        assert!(EventFamily::CpuStep.mapping().is_some());
        assert_eq!(
            EventFamily::MemWrite.mapping().unwrap().kind_filter,
            Some("write")
        );
    }

    #[test]
    fn args_are_camel_case() {
        let q = EventQuery::from_camel(&json!({
            "runId": "run-1",
            "family": "mem_write",
            "cycleRange": [10, 20],
            "addrRange": [0xd000, 0xd0ff],
            "limit": 5
        }));
        assert_eq!(q.run_id.as_deref(), Some("run-1"));
        assert_eq!(q.family, Some(EventFamily::MemWrite));
        assert_eq!(q.cycle_range, Some((10.0, 20.0)));
        assert_eq!(q.addr_range, Some((53248.0, 53503.0)));
        assert_eq!(q.pc_range, None);
        assert_eq!(q.effective_limit(), 5.0);

        // snake_case must NOT be read — it is a different op's convention.
        let snake = EventQuery::from_camel(&json!({ "run_id": "run-1", "cycle_range": [1, 2] }));
        assert_eq!(snake.run_id, None);
        assert_eq!(snake.cycle_range, None);
    }

    #[test]
    fn limit_follows_the_js_guard() {
        let l = |v: Value| EventQuery::from_camel(&json!({ "limit": v })).effective_limit();
        assert_eq!(l(json!(0)), 10_000.0); // falsy → default
        assert_eq!(l(json!(-5)), 10_000.0);
        assert_eq!(l(json!(100_001)), 10_000.0);
        assert_eq!(l(json!(100_000)), 100_000.0);
        assert_eq!(l(Value::Null), 10_000.0);
        assert_eq!(EventQuery::default().effective_limit(), 10_000.0);
    }

    #[test]
    fn empty_predicate_is_absent() {
        assert_eq!(
            EventQuery::from_camel(&json!({ "predicate": "" })).predicate,
            None
        );
        assert_eq!(
            EventQuery::from_camel(&json!({ "predicate": "value = 7" })).predicate,
            Some("value = 7".into())
        );
    }

    #[test]
    fn chip_events_has_no_726_projection() {
        assert!(from_source(EventTable::ChipEvents, StoreShape::Spec726).is_none());
        assert_eq!(
            from_source(EventTable::ChipEvents, StoreShape::Legacy217).as_deref(),
            Some("chip_events")
        );
        assert!(from_source(EventTable::Instructions, StoreShape::Spec726)
            .unwrap()
            .starts_with('('));
    }

    // ── the SQL builder ──────────────────────────────────────────────────────
    //
    // Measured against the sidecar on a real store (Spec 802 F2-A): 84/84 cases
    // byte-equal, incl. every case pinned below.

    /// `build_sql`, with the parameters inlined the way the backend does — i.e.
    /// the exact statement the store executes.
    fn executed_sql(shape: StoreShape, args: Value) -> Result<Option<String>> {
        let q = EventQuery::from_camel(&args);
        match build_sql(shape, &q)? {
            None => Ok(None),
            Some((sql, params)) => Ok(Some(fill_placeholders(&sql, &params)?)),
        }
    }

    #[test]
    fn sql_matches_the_reference_clause_order() {
        // Legacy store, no optional clause.
        assert_eq!(
            executed_sql(StoreShape::Legacy217, json!({ "runId": "r1", "family": "cpu_step" }))
                .unwrap()
                .unwrap(),
            "SELECT * FROM instructions WHERE run_id = 'r1' ORDER BY clock LIMIT 10000"
        );
        // Every optional clause at once — the ORDER is the contract.
        let sql = executed_sql(
            StoreShape::Legacy217,
            json!({
                "runId": "r1", "family": "mem_write",
                "cycleRange": [10, 20], "pcRange": [0xc000, 0xc0ff],
                "addrRange": [0xd000, 0xd0ff], "predicate": "value = 7", "limit": 25
            }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM bus_events WHERE run_id = 'r1' AND kind = 'write' \
             AND clock BETWEEN 10 AND 20 AND pc BETWEEN 49152 AND 49407 \
             AND addr BETWEEN 53248 AND 53503 AND (value = 7) ORDER BY clock LIMIT 25"
        );
        // A 726 store projects out of trace_event instead of naming a view.
        let sql726 = executed_sql(
            StoreShape::Spec726,
            json!({ "runId": "r1", "family": "cpu_step", "limit": 5 }),
        )
        .unwrap()
        .unwrap();
        assert!(sql726.starts_with("SELECT * FROM (\n  SELECT"), "{sql726}");
        assert!(sql726.contains("FROM trace_event"), "{sql726}");
        assert!(sql726.ends_with(") WHERE run_id = 'r1' ORDER BY clock LIMIT 5"), "{sql726}");
    }

    #[test]
    fn range_clauses_are_scoped_to_the_table() {
        // instructions: pc yes, addr no.
        let cpu = executed_sql(
            StoreShape::Legacy217,
            json!({ "runId": "r", "family": "cpu_step", "pcRange": [1, 2], "addrRange": [3, 4] }),
        )
        .unwrap()
        .unwrap();
        assert!(cpu.contains("pc BETWEEN 1 AND 2"), "{cpu}");
        assert!(!cpu.contains("addr BETWEEN"), "{cpu}");
        // chip_events: neither.
        let gcr = executed_sql(
            StoreShape::Legacy217,
            json!({ "runId": "r", "family": "gcr_byte", "pcRange": [1, 2], "addrRange": [3, 4] }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            gcr,
            "SELECT * FROM chip_events WHERE run_id = 'r' AND kind = 'byte_ready' \
             ORDER BY clock LIMIT 10000"
        );
    }

    #[test]
    fn limit_is_interpolated_not_bound() {
        let limit_of = |v: Value| {
            let sql = executed_sql(
                StoreShape::Legacy217,
                json!({ "runId": "r", "family": "cpu_step", "limit": v }),
            )
            .unwrap()
            .unwrap();
            sql.rsplit("LIMIT ").next().unwrap().to_string()
        };
        assert_eq!(limit_of(json!(1)), "1");
        assert_eq!(limit_of(json!(100_000)), "100000");
        assert_eq!(limit_of(json!(100_001)), "10000");
        assert_eq!(limit_of(json!(0)), "10000");
        assert_eq!(limit_of(Value::Null), "10000");
        // An integral limit never renders as `5.0` (JS `String(5)`).
        assert_eq!(limit_of(json!(5.0)), "5");
    }

    #[test]
    fn parameters_are_inlined_like_the_backend() {
        // String escaping — a quote in runId doubles, it does not break out.
        let sql = executed_sql(
            StoreShape::Legacy217,
            json!({ "runId": "r'1", "family": "cpu_step" }),
        )
        .unwrap()
        .unwrap();
        assert!(sql.contains("run_id = 'r''1'"), "{sql}");
        // A missing range element is `undefined` → `NULL` (matches nothing).
        let sql = executed_sql(
            StoreShape::Legacy217,
            json!({ "runId": "r", "family": "cpu_step", "cycleRange": [100] }),
        )
        .unwrap()
        .unwrap();
        assert!(sql.contains("clock BETWEEN 100 AND NULL"), "{sql}");
        // An absent runId binds SQL NULL — the reference's `undefined` param.
        let sql = executed_sql(StoreShape::Legacy217, json!({ "family": "cpu_step" }))
            .unwrap()
            .unwrap();
        assert!(sql.contains("run_id = NULL"), "{sql}");
        // A `?` inside the predicate reproduces the backend's count check.
        let e = executed_sql(
            StoreShape::Legacy217,
            json!({ "runId": "r", "family": "cpu_step", "predicate": "pc = ?" }),
        )
        .unwrap_err();
        assert_eq!(e.to_string(), "param count mismatch: sql has 2 placeholders, 1 params");
    }

    // ── the predicate sandbox ────────────────────────────────────────────────

    #[test]
    fn forbidden_predicate_tokens_are_rejected_verbatim() {
        for bad in [
            "1=1; DROP TABLE meta",
            "1=1 union select 1",
            "dropped = 1",   // SUBSTRING match — not hardened, not loosened
            "inserted_at > 0",
            "delete_me = 1",
            "UpDaTe = 1",
        ] {
            let e = executed_sql(
                StoreShape::Legacy217,
                json!({ "runId": "r", "family": "cpu_step", "predicate": bad }),
            )
            .expect_err(bad);
            assert_eq!(e.to_string(), "predicate contains forbidden tokens", "{bad}");
        }
        // A benign predicate is wrapped in parens, unchanged.
        let sql = executed_sql(
            StoreShape::Legacy217,
            json!({ "runId": "r", "family": "cpu_step", "predicate": "pc > 100 AND a = 1" }),
        )
        .unwrap()
        .unwrap();
        assert!(sql.contains("AND (pc > 100 AND a = 1) ORDER BY"), "{sql}");
    }

    #[test]
    fn empty_result_guards_run_before_the_predicate_gate() {
        // Unmapped family → `[]`, NOT an error, even with a forbidden predicate.
        let evil = json!({ "runId": "r", "family": "cpu_jam", "predicate": "1; DROP TABLE meta" });
        assert!(executed_sql(StoreShape::Legacy217, evil).unwrap().is_none());
        // Same for chip_events on a 726 store.
        let evil726 = json!({ "runId": "r", "family": "irq_assert", "predicate": "1; DROP TABLE meta" });
        assert!(executed_sql(StoreShape::Spec726, evil726).unwrap().is_none());
        // …and for an unknown family name.
        assert!(executed_sql(StoreShape::Legacy217, json!({ "family": "nope" }))
            .unwrap()
            .is_none());
    }

    // ── row projection, end to end against a real store ──────────────────────

    const LEGACY_FIXTURE: &str = r#"
CREATE TABLE instructions (
  run_id TEXT, seq UBIGINT, cpu TEXT, clock UBIGINT, master_clock UBIGINT,
  pc USMALLINT, opcode UTINYINT, b1 UTINYINT, b2 UTINYINT,
  a UTINYINT, x UTINYINT, y UTINYINT, sp UTINYINT, p UTINYINT, source TEXT);
CREATE TABLE bus_events (
  run_id TEXT, seq UBIGINT, cpu TEXT, clock UBIGINT, master_clock UBIGINT,
  pc USMALLINT, kind TEXT, addr USMALLINT, value UTINYINT, old_value UTINYINT,
  line_atn BOOLEAN, line_clk BOOLEAN, line_data BOOLEAN, source TEXT);
CREATE TABLE chip_events (
  run_id TEXT, seq UBIGINT, cpu TEXT, clock UBIGINT, master_clock UBIGINT,
  pc USMALLINT, chip TEXT, kind TEXT, unit UTINYINT, value UTINYINT,
  old_value UTINYINT, source TEXT);

INSERT INTO instructions VALUES
 ('r1',1,'c64',100,100,49152,169,1,0,1,2,3,253,32,'fx'),
 ('r1',2,'c64',101,101,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,'fx'),
 ('r2',3,'c64',100,100,4096,96,0,0,0,0,0,0,0,'fx');
INSERT INTO bus_events VALUES
 ('r1',10,'c64',100,100,49152,'read',53266,157,NULL,NULL,NULL,NULL,'fx'),
 ('r1',11,'c64',101,101,NULL,'read',NULL,NULL,NULL,NULL,NULL,NULL,'fx'),
 ('r1',12,'c64',102,102,49168,'write',53280,1,0,NULL,NULL,NULL,'fx'),
 ('r1',13,'c64',103,103,1024,'line_change',NULL,NULL,NULL,TRUE,FALSE,NULL,'fx');
INSERT INTO chip_events VALUES
 ('r1',20,'c64',110,110,49152,'cia1','irq_assert',NULL,NULL,NULL,'fx'),
 ('r1',21,'c64',111,111,49152,NULL,'irq_assert',NULL,NULL,NULL,'fx'),
 ('r1',22,'c64',114,114,49152,'cia1','timer_underflow',0,NULL,NULL,'fx'),
 ('r1',23,'c64',115,115,49152,'cia2','timer_underflow',1,NULL,NULL,'fx'),
 ('r1',24,'c64',116,116,49152,NULL,'timer_underflow',NULL,NULL,NULL,'fx'),
 ('r1',25,'drive8',117,117,NULL,'via2','byte_ready',3,85,NULL,'fx'),
 ('r1',26,'drive8',118,118,NULL,NULL,'byte_ready',NULL,NULL,NULL,'fx'),
 ('r1',27,'c64',119,119,49152,'kernal_load','trap_fire',NULL,NULL,NULL,'fx'),
 ('r1',28,'c64',120,120,49152,NULL,'trap_fire',NULL,NULL,NULL,'fx');
"#;

    fn legacy_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        conn.execute_batch(LEGACY_FIXTURE).expect("fixture");
        conn
    }

    fn rows(conn: &Connection, shape: StoreShape, args: Value) -> Vec<Value> {
        let q = EventQuery::from_camel(&args);
        query_events(conn, shape, &q)
            .expect("query")
            .into_iter()
            .map(Value::Object)
            .collect()
    }

    #[test]
    fn projects_rows_exactly_like_row_from_db() {
        let conn = legacy_conn();
        let l = StoreShape::Legacy217;

        // cpu_step: every field is `Number(x)`, so an all-NULL row reads as 0s.
        assert_eq!(
            rows(&conn, l, json!({ "runId": "r1", "family": "cpu_step" })),
            vec![
                json!({"runId":"r1","family":"cpu_step","cycle":100,"pc":49152,"opcode":169,
                       "a":1,"x":2,"y":3,"sp":253,"flags":32}),
                json!({"runId":"r1","family":"cpu_step","cycle":101,"pc":0,"opcode":0,
                       "a":0,"x":0,"y":0,"sp":0,"flags":0}),
            ]
        );
        // mem_read: `?? 0` on pc/addr/value, constant region.
        assert_eq!(
            rows(&conn, l, json!({ "runId": "r1", "family": "mem_read" })),
            vec![
                json!({"runId":"r1","family":"mem_read","cycle":100,"pc":49152,"addr":53266,"value":157,"region":"ram"}),
                json!({"runId":"r1","family":"mem_read","cycle":101,"pc":0,"addr":0,"value":0,"region":"ram"}),
            ]
        );
        assert_eq!(
            rows(&conn, l, json!({ "runId": "r1", "family": "mem_write" })),
            vec![json!({"runId":"r1","family":"mem_write","cycle":102,"pc":49168,"addr":53280,"value":1,"region":"ram"})]
        );
        // The three IEC families read the SAME row and differ only in the column.
        assert_eq!(
            rows(&conn, l, json!({ "runId": "r1", "family": "drive_atn_change" })),
            vec![json!({"runId":"r1","family":"drive_atn_change","cycle":103,"level":1})]
        );
        assert_eq!(
            rows(&conn, l, json!({ "runId": "r1", "family": "drive_clk_change" })),
            vec![json!({"runId":"r1","family":"drive_clk_change","cycle":103,"level":0})]
        );
        // NULL line_data is falsy → 0, not null.
        assert_eq!(
            rows(&conn, l, json!({ "runId": "r1", "family": "drive_data_change" })),
            vec![json!({"runId":"r1","family":"drive_data_change","cycle":103,"level":0})]
        );
        // `r.chip ?? "manual"`.
        assert_eq!(
            rows(&conn, l, json!({ "runId": "r1", "family": "irq_assert" })),
            vec![
                json!({"runId":"r1","family":"irq_assert","cycle":110,"source":"cia1"}),
                json!({"runId":"r1","family":"irq_assert","cycle":111,"source":"manual"}),
            ]
        );
        // `chip` is the RAW column (null survives); `Number(unit) === 0 ? ta : tb`.
        assert_eq!(
            rows(&conn, l, json!({ "runId": "r1", "family": "cia_timer_underflow" })),
            vec![
                json!({"runId":"r1","family":"cia_timer_underflow","cycle":114,"chip":"cia1","timer":"ta"}),
                json!({"runId":"r1","family":"cia_timer_underflow","cycle":115,"chip":"cia2","timer":"tb"}),
                json!({"runId":"r1","family":"cia_timer_underflow","cycle":116,"chip":null,"timer":"ta"}),
            ]
        );
        // `Number(r.value)` has NO `?? 0`, but `Number(null)` is 0 anyway;
        // `trackHalf` does have one.
        assert_eq!(
            rows(&conn, l, json!({ "runId": "r1", "family": "gcr_byte" })),
            vec![
                json!({"runId":"r1","family":"gcr_byte","cycle":117,"byte":85,"trackHalf":3}),
                json!({"runId":"r1","family":"gcr_byte","cycle":118,"byte":0,"trackHalf":0}),
            ]
        );
        // `String(r.chip ?? "unknown")`.
        assert_eq!(
            rows(&conn, l, json!({ "runId": "r1", "family": "trap_fire" })),
            vec![
                json!({"runId":"r1","family":"trap_fire","cycle":119,"hookName":"kernal_load"}),
                json!({"runId":"r1","family":"trap_fire","cycle":120,"hookName":"unknown"}),
            ]
        );
    }

    #[test]
    fn run_id_filter_and_empty_results() {
        let conn = legacy_conn();
        let l = StoreShape::Legacy217;
        assert_eq!(
            rows(&conn, l, json!({ "runId": "r2", "family": "cpu_step" })).len(),
            1
        );
        // No runId → `run_id = NULL` → no rows (the reference's live taint defect).
        assert!(rows(&conn, l, json!({ "family": "cpu_step" })).is_empty());
        assert!(rows(&conn, l, json!({ "runId": "nope", "family": "cpu_step" })).is_empty());
        // A producerless family never touches the store.
        assert!(rows(&conn, l, json!({ "runId": "r1", "family": "vic_badline" })).is_empty());
    }

    #[test]
    fn ranges_filter_and_a_sql_error_surfaces_bare() {
        let conn = legacy_conn();
        let l = StoreShape::Legacy217;
        assert_eq!(
            rows(&conn, l, json!({ "runId": "r1", "family": "cpu_step", "cycleRange": [101, 200] })).len(),
            1
        );
        assert_eq!(
            rows(&conn, l, json!({ "runId": "r1", "family": "mem_read", "addrRange": [53266, 53266] })).len(),
            1
        );
        assert_eq!(
            rows(&conn, l, json!({ "runId": "r1", "family": "cpu_step", "limit": 1 })).len(),
            1
        );
        // A pc range wider than USMALLINT is a plain integer comparison — it must
        // NOT become a bind-parameter conversion error (this is why the backend's
        // literal inlining is reproduced). The NULL-pc row drops out, as with any
        // BETWEEN.
        assert_eq!(
            rows(&conn, l, json!({ "runId": "r1", "family": "cpu_step", "pcRange": [0, 70000] })).len(),
            1
        );
        // Spec 802 F1: a broken predicate surfaces the DuckDB message unprefixed.
        let q = EventQuery::from_camel(
            &json!({ "runId": "r1", "family": "cpu_step", "predicate": "nope_col = 1" }),
        );
        let e = query_events(&conn, l, &q).expect_err("binder error");
        assert!(
            e.to_string().starts_with("Binder Error: Referenced column \"nope_col\" not found"),
            "got: {e}"
        );
    }

    #[test]
    fn spec726_store_projects_out_of_trace_event() {
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        conn.execute_batch(
            r#"
CREATE TABLE trace_event (
  run_id TEXT, seq UBIGINT, cycle UBIGINT, channel TEXT,
  trigger_kind TEXT, capture_kind TEXT, data_json TEXT);
INSERT INTO trace_event VALUES
 ('r726',0,1000,'cpu','manual','full','{"pc":49152,"opcode":169,"a":1,"x":2,"y":3,"sp":253,"p":32,"clk":1000}'),
 ('r726',1,1001,'drive_pc','manual','full','{"pc":57344,"opcode":234,"side":"drive","clk":77}'),
 ('r726',2,1010,'bus_access','manual','full','{"op":"read","addr":53266,"value":157,"pc":49152,"cycle_c64":1010}'),
 ('r726',3,1011,'io','manual','full','{"op":"write","addr":53280,"value":14,"pc":49168}'),
 ('r726',4,1020,'iec','manual','full','{"atn":true,"clk":false,"data":false,"cycle_c64":1020}');
"#,
        )
        .expect("fixture");
        let s = StoreShape::Spec726;

        // `clk` wins over `cycle` (COALESCE) → the drive row sorts FIRST.
        assert_eq!(
            rows(&conn, s, json!({ "runId": "r726", "family": "cpu_step" })),
            vec![
                json!({"runId":"r726","family":"cpu_step","cycle":77,"pc":57344,"opcode":234,
                       "a":0,"x":0,"y":0,"sp":0,"flags":0}),
                json!({"runId":"r726","family":"cpu_step","cycle":1000,"pc":49152,"opcode":169,
                       "a":1,"x":2,"y":3,"sp":253,"flags":32}),
            ]
        );
        assert_eq!(
            rows(&conn, s, json!({ "runId": "r726", "family": "mem_write" })),
            vec![json!({"runId":"r726","family":"mem_write","cycle":1011,"pc":49168,"addr":53280,"value":14,"region":"ram"})]
        );
        assert_eq!(
            rows(&conn, s, json!({ "runId": "r726", "family": "drive_atn_change" })),
            vec![json!({"runId":"r726","family":"drive_atn_change","cycle":1020,"level":1})]
        );
        // chip_events has no 726 producer → empty, never an error.
        assert!(rows(&conn, s, json!({ "runId": "r726", "family": "gcr_byte" })).is_empty());
    }
}
