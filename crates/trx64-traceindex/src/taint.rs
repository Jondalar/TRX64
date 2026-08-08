//! Taint analysis — the `taint` (structured) and `taint_text` (monitor) ops.
//! Spec 802 §4.1.
//!
//! Backwards data-flow from `(startAddr, startCycle)`: find the most recent
//! write to the address before the cycle, classify it by the opcode that
//! performed it, then recurse on what that write consumed (the source
//! register's last load, or — for a read-modify-write — the same address's
//! prior value). Pure forensics over an existing trace; nothing re-executes.
//!
//! # Ported 1:1, defects included
//!
//! The reference is `C64ReverseEngineeringMCP/src/runtime/headless/v2/taint.ts`
//! (`traceTaint`) plus the `taint` / `taint_text` arms of
//! `TRX64/tools/trace-read-sidecar/sidecar.ts`. Spec 802's parity gate compares
//! native output against the sidecar, so **every** observable behaviour is
//! reproduced rather than improved. The four that look like bugs and are not
//! mistakes in this port:
//!
//! 1. **`run_id` is inlined as SQL `NULL` when the caller omits it** (the TS
//!    backend's `inlineParam(undefined) === "NULL"`), and `WHERE run_id = NULL`
//!    matches nothing. The monitor's `taint` verb never sends a run id, so on
//!    that path every subquery returns zero rows and the synthetic fallback
//!    root below is what gets printed. Spec 802 R3 §3.2 records this as a live
//!    defect with two options; option (a) — port faithfully, fix separately —
//!    is what is implemented. [`store_run_id`] is the one-line ingredient for
//!    option (b) and is deliberately **not** wired in.
//! 2. **The graph is never empty.** When nothing is found, `traceTaint`
//!    synthesises a root `{cycle: startCycle, pc: 0, addr: startAddr, value: 0,
//!    contribution: "direct_write"}` and inserts it. Consequently the
//!    `taint: no contributing write found …` line in [`render_graph_text`] is
//!    unreachable — it is kept because it is part of the pinned string set.
//! 3. **`findRegisterLoad` only ever matches a transfer instruction.** The
//!    opcode-effects table assigns `destReg` to `TAX/TAY/TXA/TYA/TSX/TXS` only;
//!    `LDA`/`LDX`/`LDY` are not in the table at all (they classify as `none`).
//!    A store fed by an immediate or absolute load therefore terminates the
//!    walk instead of following the load.
//! 4. **The IRQ-boundary probe and the IEC bridge are inert on a Spec-726
//!    store.** Both `irq_assert` and `drive_data_change` resolve through
//!    families whose 726 projection is empty (`chip_events`) or which need a
//!    `driveRunId` no caller passes. Ported anyway, so a legacy Shape-A store —
//!    where `chip_events` is a real table — behaves as it always did.
//!
//! # Numbers
//!
//! Everything the TS reads out of DuckDB goes through `Number(...)`, so this
//! port carries cycles / PCs / addresses / values as `f64` and reproduces JS
//! arithmetic and formatting exactly (`Math.max(0, NaN) === NaN`, `NaN < x`
//! is false, `JSON.stringify(NaN) === "null"`, integral numbers serialise
//! without a decimal point). See `js_max`, `js_num_str` and `jn` below.

use crate::conn::{value_to_json, with_conn};
use crate::error::{Result, TraceReadError};
use crate::schema::{
    StoreShape, BUS_EVENTS_726, INSTRUCTIONS_726, LEGACY_BUS_EVENTS, LEGACY_INSTRUCTIONS,
};
use duckdb::Connection;
use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};
use std::path::Path;

// ─────────────────────────────────────────────────────────────────────────────
// JS number semantics
// ─────────────────────────────────────────────────────────────────────────────

/// `Math.max` — NaN-poisoning, unlike Rust's `f64::max` (which *ignores* NaN).
fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a > b {
        a
    } else {
        b
    }
}

/// `String(n)` for a JS `Number`: integral values carry no decimal point,
/// `NaN` / `Infinity` stringify as words.
fn js_num_str(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if n == 0.0 {
        return "0".to_string();
    }
    if n.fract() == 0.0 && n.abs() < 1e21 {
        return format!("{}", n as i128);
    }
    format!("{n}")
}

/// `JSON.stringify` of a JS `Number`: integral → integer, non-finite → `null`.
fn jn(n: f64) -> Value {
    if !n.is_finite() {
        return Value::Null;
    }
    if n.fract() == 0.0 && n.abs() <= 9.007_199_254_740_992e15 {
        return Value::from(n as i64);
    }
    serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
}

/// ECMAScript `ToInt32`, the coercion behind JS bitwise operators.
fn to_int32(n: f64) -> i32 {
    if !n.is_finite() {
        return 0;
    }
    let m = n.trunc().rem_euclid(4_294_967_296.0);
    if m >= 2_147_483_648.0 {
        (m - 4_294_967_296.0) as i32
    } else {
        m as i32
    }
}

/// The sidecar's `hx`: `(n & 0xffff).toString(16).padStart(4, "0")`.
fn hx(n: f64) -> String {
    format!("{:04x}", (to_int32(n) as u32) & 0xffff)
}

/// The sidecar's value formatting: `n.toString(16).padStart(2, "0")` — note it
/// is **not** masked, because the column is a `UTINYINT`.
fn hx2(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && (0.0..1e18).contains(&n) {
        return format!("{:02x}", n as u64);
    }
    // Non-integral / negative never occurs on this column; keep it lossless.
    js_num_str(n)
}

/// `Number(x)` over a JSON value: `undefined` → NaN, `null` → 0, bool → 0/1,
/// numeric strings (incl. `0x…`) parse, anything else → NaN.
fn js_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                return 0.0;
            }
            if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                return u64::from_str_radix(hex, 16).map(|v| v as f64).unwrap_or(f64::NAN);
            }
            t.parse::<f64>().unwrap_or(f64::NAN)
        }
        Some(_) => f64::NAN,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Opcode effects table (taint.ts `opcodeEffects`)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reg {
    A,
    X,
    Y,
    Sp,
    Flags,
}

impl Reg {
    fn as_str(self) -> &'static str {
        match self {
            Reg::A => "A",
            Reg::X => "X",
            Reg::Y => "Y",
            Reg::Sp => "SP",
            Reg::Flags => "flags",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EffKind {
    DirectWrite,
    RmwModify,
    StackPush,
    Transfer,
    None,
}

#[derive(Clone, Copy, Debug)]
struct Effects {
    kind: EffKind,
    source_reg: Option<Reg>,
    dest_reg: Option<Reg>,
    is_rmw: bool,
}

impl Effects {
    const fn new(kind: EffKind) -> Self {
        Effects { kind, source_reg: None, dest_reg: None, is_rmw: false }
    }
    const fn src(kind: EffKind, r: Reg) -> Self {
        Effects { kind, source_reg: Some(r), dest_reg: None, is_rmw: false }
    }
    const fn rmw() -> Self {
        Effects { kind: EffKind::RmwModify, source_reg: None, dest_reg: None, is_rmw: true }
    }
    const fn xfer(src: Reg, dst: Reg) -> Self {
        Effects {
            kind: EffKind::Transfer,
            source_reg: Some(src),
            dest_reg: Some(dst),
            is_rmw: false,
        }
    }
}

/// `opcodeEffects(opcode)`. The argument is an `f64` because it arrives as
/// `Number(row.opcode)`; a value that is not an integral byte matches no `case`
/// in JS either and falls through to `none`.
fn opcode_effects(opcode: f64) -> Effects {
    if !(opcode.is_finite() && opcode.fract() == 0.0 && (0.0..=255.0).contains(&opcode)) {
        return Effects::new(EffKind::None);
    }
    match opcode as u8 {
        // STA
        0x81 | 0x85 | 0x8d | 0x91 | 0x95 | 0x99 | 0x9d => Effects::src(EffKind::DirectWrite, Reg::A),
        // STX
        0x86 | 0x8e | 0x96 => Effects::src(EffKind::DirectWrite, Reg::X),
        // STY
        0x84 | 0x8c | 0x94 => Effects::src(EffKind::DirectWrite, Reg::Y),
        // SAX (undocumented) — store A&X
        0x83 | 0x87 | 0x8f | 0x97 => Effects::src(EffKind::DirectWrite, Reg::A),
        // SHX / SHY / AHX / TAS / SHA (undocumented high-byte stores)
        0x93 | 0x9b | 0x9c | 0x9e | 0x9f => Effects::src(EffKind::DirectWrite, Reg::A),
        // PHA
        0x48 => Effects::src(EffKind::StackPush, Reg::A),
        // PHP
        0x08 => Effects::src(EffKind::StackPush, Reg::Flags),
        // JSR — pushes the return address; no source register
        0x20 => Effects::new(EffKind::StackPush),
        // INC / DEC / ASL / LSR / ROL / ROR
        0xe6 | 0xee | 0xf6 | 0xfe => Effects::rmw(),
        0xc6 | 0xce | 0xd6 | 0xde => Effects::rmw(),
        0x06 | 0x0e | 0x16 | 0x1e => Effects::rmw(),
        0x46 | 0x4e | 0x56 | 0x5e => Effects::rmw(),
        0x26 | 0x2e | 0x36 | 0x3e => Effects::rmw(),
        0x66 | 0x6e | 0x76 | 0x7e => Effects::rmw(),
        // Undocumented compound RMW: SLO, RLA, SRE, RRA, DCP, ISC
        0x03 | 0x07 | 0x0f | 0x13 | 0x17 | 0x1b | 0x1f => Effects::rmw(),
        0x23 | 0x27 | 0x2f | 0x33 | 0x37 | 0x3b | 0x3f => Effects::rmw(),
        0x43 | 0x47 | 0x4f | 0x53 | 0x57 | 0x5b | 0x5f => Effects::rmw(),
        0x63 | 0x67 | 0x6f | 0x73 | 0x77 | 0x7b | 0x7f => Effects::rmw(),
        0xc3 | 0xc7 | 0xcf | 0xd3 | 0xd7 | 0xdb | 0xdf => Effects::rmw(),
        0xe3 | 0xe7 | 0xef | 0xf3 | 0xf7 | 0xfb | 0xff => Effects::rmw(),
        // Transfers
        0xaa => Effects::xfer(Reg::A, Reg::X),  // TAX
        0xa8 => Effects::xfer(Reg::A, Reg::Y),  // TAY
        0x8a => Effects::xfer(Reg::X, Reg::A),  // TXA
        0x98 => Effects::xfer(Reg::Y, Reg::A),  // TYA
        0xba => Effects::xfer(Reg::Sp, Reg::X), // TSX
        0x9a => Effects::xfer(Reg::X, Reg::Sp), // TXS
        _ => Effects::new(EffKind::None),
    }
}

/// `$D000-$DFFF` — VIC, SID, CIA1, CIA2.
fn is_io_register_addr(addr: f64) -> bool {
    (0xd000 as f64..=0xdfff as f64).contains(&addr)
}

/// `$DD0D` / `$DC0D` — the CIA ICRs, the IEC-linked interrupt registers.
fn is_iec_bridge_addr(addr: f64) -> bool {
    addr == 0xdd0d as f64 || addr == 0xdc0d as f64
}

// ─────────────────────────────────────────────────────────────────────────────
// query-events (the subset taint needs), ported from v2/query-events.ts
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    CpuStep,
    MemRead,
    MemWrite,
    IrqAssert,
    DriveDataChange,
}

impl Family {
    /// `MAP[family].table` + `kindFilter`.
    fn table_and_kind(self) -> (&'static str, Option<&'static str>) {
        match self {
            Family::CpuStep => ("instructions", None),
            Family::MemRead => ("bus_events", Some("read")),
            Family::MemWrite => ("bus_events", Some("write")),
            Family::IrqAssert => ("chip_events", Some("irq_assert")),
            Family::DriveDataChange => ("bus_events", Some("line_change")),
        }
    }
}

/// A decoded `EventRow`. All numeric fields go through `Number(...)`, so a SQL
/// `NULL` lands as `0` exactly like `Number(null)` / `Number(x ?? 0)`.
#[derive(Clone, Debug)]
struct EventRow {
    cycle: f64,
    pc: f64,
    addr: f64,
    value: f64,
    opcode: f64,
    sp: f64,
}

/// `inlineParam` for a string — `'` doubled, exactly like `sq()`.
fn inline_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `inlineParam` for a number — rejects non-finite values with the TS message.
fn inline_num(n: f64) -> Result<String> {
    if !n.is_finite() {
        return Err(TraceReadError::other(format!("non-finite param: {}", js_num_str(n))));
    }
    Ok(js_num_str(n))
}

/// `mapping.table` → the real FROM source for this store shape. `None` means
/// "no producer on this shape" and the query yields an empty result (additive,
/// per Spec 726 §6a) — that is how `chip_events` behaves on a Shape-B store.
fn from_source(shape: StoreShape, table: &str) -> Option<String> {
    match (shape, table) {
        (StoreShape::Spec726, "instructions") => Some(format!("({INSTRUCTIONS_726})")),
        (StoreShape::Spec726, "bus_events") => Some(format!("({BUS_EVENTS_726})")),
        (StoreShape::Spec726, _) => None,
        (StoreShape::Legacy217, "instructions") => Some(LEGACY_INSTRUCTIONS.to_string()),
        (StoreShape::Legacy217, "bus_events") => Some(LEGACY_BUS_EVENTS.to_string()),
        (StoreShape::Legacy217, other) => Some(other.to_string()),
    }
}

/// One `queryEvents` call.
///
/// `run_id == None` reproduces `inlineParam(undefined) === "NULL"` — see the
/// module header, defect (1).
#[allow(clippy::too_many_arguments)]
fn query_events(
    conn: &Connection,
    shape: StoreShape,
    run_id: Option<&str>,
    family: Family,
    cycle_range: Option<(f64, f64)>,
    pc_range: Option<(f64, f64)>,
    addr_range: Option<(f64, f64)>,
    limit: u32,
) -> Result<Vec<EventRow>> {
    let (table, kind) = family.table_and_kind();
    let Some(src) = from_source(shape, table) else {
        return Ok(Vec::new());
    };

    let mut where_parts: Vec<String> = Vec::with_capacity(5);
    where_parts.push(format!(
        "run_id = {}",
        run_id.map(inline_str).unwrap_or_else(|| "NULL".to_string())
    ));
    if let Some(k) = kind {
        where_parts.push(format!("kind = {}", inline_str(k)));
    }
    if let Some((a, b)) = cycle_range {
        where_parts.push(format!("clock BETWEEN {} AND {}", inline_num(a)?, inline_num(b)?));
    }
    // pcRange applies to bus_events + instructions only.
    if let Some((a, b)) = pc_range {
        if table == "bus_events" || table == "instructions" {
            where_parts.push(format!("pc BETWEEN {} AND {}", inline_num(a)?, inline_num(b)?));
        }
    }
    // addrRange applies to bus_events only.
    if let Some((a, b)) = addr_range {
        if table == "bus_events" {
            where_parts.push(format!("addr BETWEEN {} AND {}", inline_num(a)?, inline_num(b)?));
        }
    }

    let limit = if limit > 0 && limit <= 100_000 { limit } else { 10_000 };
    let sql = format!(
        "SELECT * FROM {src} WHERE {} ORDER BY clock LIMIT {limit}",
        where_parts.join(" AND ")
    );
    exec_rows(conn, &sql)
}

/// Run a `SELECT *` and pull the columns `rowFromDb` reads, by NAME.
fn exec_rows(conn: &Connection, sql: &str) -> Result<Vec<EventRow>> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| TraceReadError::duck("prepare taint query", e))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| TraceReadError::duck("run taint query", e))?;
    let names: Vec<String> = rows.as_ref().map(|s| s.column_names()).unwrap_or_default();
    let idx = |want: &str| names.iter().position(|n| n == want);
    let (i_clock, i_pc, i_addr, i_value, i_opcode, i_sp) = (
        idx("clock"),
        idx("pc"),
        idx("addr"),
        idx("value"),
        idx("opcode"),
        idx("sp"),
    );

    let mut out = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| TraceReadError::duck("fetch taint row", e))?
    {
        let num = |i: Option<usize>| -> Result<f64> {
            match i {
                // Absent column ⇒ `undefined` in JS. `Number(undefined)` is NaN,
                // but every consumer of an absent column in query-events.ts uses
                // `?? 0`, and the columns that do not (instructions.pc/opcode)
                // always exist on both shapes. Treat absent as 0 — identical.
                None => Ok(0.0),
                Some(i) => {
                    let v: duckdb::types::Value = row
                        .get(i)
                        .map_err(|e| TraceReadError::duck("read taint column", e))?;
                    Ok(json_to_number(&value_to_json(&v)))
                }
            }
        };
        out.push(EventRow {
            cycle: num(i_clock)?,
            pc: num(i_pc)?,
            addr: num(i_addr)?,
            value: num(i_value)?,
            opcode: num(i_opcode)?,
            sp: num(i_sp)?,
        });
    }
    Ok(out)
}

/// `Number(cell)` — `null` → 0, bool → 0/1, number → itself.
fn json_to_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => js_number(Some(&Value::String(s.clone()))),
        _ => f64::NAN,
    }
}

/// Stable descending sort by cycle, then take the head — i.e. the newest row,
/// and among equal cycles the one the SQL (`ORDER BY clock`) returned first.
/// `Array.prototype.sort` has been required to be stable since ES2019, so this
/// is the reference behaviour, not an interpretation of it.
fn newest_first(rows: &[EventRow]) -> Option<&EventRow> {
    let mut best: Option<&EventRow> = None;
    for r in rows {
        match best {
            None => best = Some(r),
            Some(b) if r.cycle > b.cycle => best = Some(r),
            _ => {}
        }
    }
    best
}

// ─────────────────────────────────────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────────────────────────────────────

/// `TaintQuery` — the resolved query `traceTaint` runs.
#[derive(Clone, Debug)]
pub struct TaintQuery {
    /// `undefined` here inlines as SQL `NULL` and matches nothing. See the
    /// module header, defect (1).
    pub run_id: Option<String>,
    pub start_cycle: f64,
    pub start_addr: f64,
    /// default 100
    pub max_depth: f64,
    /// default 1_000_000
    pub cycle_window: f64,
    /// default true
    pub follow_irq: bool,
    /// default true
    pub cross_domain: bool,
    pub drive_run_id: Option<String>,
}

impl TaintQuery {
    pub fn new(run_id: Option<String>, start_cycle: f64, start_addr: f64) -> Self {
        TaintQuery {
            run_id,
            start_cycle,
            start_addr,
            max_depth: 100.0,
            cycle_window: 1_000_000.0,
            follow_irq: true,
            cross_domain: true,
            drive_run_id: None,
        }
    }
}

/// One entry of `TaintNode.inputs`: either `{ "addr": n }` or `{ "reg": "A" }`.
#[derive(Clone, Debug, PartialEq)]
pub enum TaintInput {
    Addr(f64),
    Reg(&'static str),
}

impl TaintInput {
    fn to_json(&self) -> Value {
        let mut m = Map::new();
        match self {
            TaintInput::Addr(a) => {
                m.insert("addr".into(), jn(*a));
            }
            TaintInput::Reg(r) => {
                m.insert("reg".into(), Value::from(*r));
            }
        }
        Value::Object(m)
    }
}

#[derive(Clone, Debug)]
pub struct TaintNode {
    pub id: String,
    pub cycle: f64,
    pub pc: f64,
    pub addr: f64,
    pub value: f64,
    /// `direct_write` | `rmw_modify` | `io_register_read` | `stack_push` |
    /// `transfer` | `irq_boundary` | `iec_bridge`
    pub contribution: &'static str,
    pub inputs: Vec<TaintInput>,
    /// `c64` | `drive`
    pub domain: &'static str,
}

impl TaintNode {
    fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), Value::from(self.id.clone()));
        m.insert("cycle".into(), jn(self.cycle));
        m.insert("pc".into(), jn(self.pc));
        m.insert("addr".into(), jn(self.addr));
        m.insert("value".into(), jn(self.value));
        m.insert("contribution".into(), Value::from(self.contribution));
        m.insert(
            "inputs".into(),
            Value::Array(self.inputs.iter().map(TaintInput::to_json).collect()),
        );
        m.insert("domain".into(), Value::from(self.domain));
        Value::Object(m)
    }
}

/// The result of [`taint_graph`]. `nodes` is kept in **insertion order** — the
/// text renderer prints `Object.values(nodes).slice(0, 40)`, which is insertion
/// order in JS. The JSON `nodes` object is keyed by id; its key order is not
/// part of the contract (Spec 802 R3 §7 compares it order-insensitively).
#[derive(Clone, Debug)]
pub struct TaintGraph {
    pub root: TaintNode,
    pub nodes: Vec<TaintNode>,
    pub edges: Vec<(String, String)>,
    pub truncated: bool,
}

impl TaintGraph {
    /// `jsonSafe(graph)` — the `taint` op's result value.
    pub fn to_json(&self) -> Value {
        let mut nodes = Map::new();
        for n in &self.nodes {
            nodes.insert(n.id.clone(), n.to_json());
        }
        let edges: Vec<Value> = self
            .edges
            .iter()
            .map(|(f, t)| {
                let mut m = Map::new();
                m.insert("from".into(), Value::from(f.clone()));
                m.insert("to".into(), Value::from(t.clone()));
                Value::Object(m)
            })
            .collect();
        let mut m = Map::new();
        m.insert("root".into(), self.root.to_json());
        m.insert("nodes".into(), Value::Object(nodes));
        m.insert("edges".into(), Value::Array(edges));
        m.insert("truncated".into(), Value::Bool(self.truncated));
        Value::Object(m)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Core algorithm — taint.ts `traceTaint`
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct WorkItem {
    run_id: Option<String>,
    domain: &'static str,
    addr: f64,
    before_cycle: f64,
    depth: f64,
    parent_id: Option<String>,
}

/// Backwards data-flow trace. Ported line-for-line from `traceTaint`.
pub fn taint_graph(conn: &Connection, shape: StoreShape, q: &TaintQuery) -> Result<TaintGraph> {
    let max_depth = q.max_depth;
    let cycle_window = q.cycle_window;
    let follow_irq = q.follow_irq;
    let cross_domain = q.cross_domain;

    let min_cycle = q.start_cycle - cycle_window;

    let mut order: Vec<TaintNode> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut truncated = false;
    let mut root: Option<TaintNode> = None;

    let mut queue: VecDeque<WorkItem> = VecDeque::new();
    queue.push_back(WorkItem {
        run_id: q.run_id.clone(),
        domain: "c64",
        addr: q.start_addr,
        before_cycle: q.start_cycle,
        depth: 0.0,
        parent_id: None,
    });

    while let Some(item) = queue.pop_front() {
        if item.depth > max_depth {
            truncated = true;
            continue;
        }
        if item.before_cycle < min_cycle {
            continue;
        }

        // Most recent mem_write at item.addr before item.beforeCycle.
        let write_rows = query_events(
            conn,
            shape,
            item.run_id.as_deref(),
            Family::MemWrite,
            Some((js_max(0.0, min_cycle), item.before_cycle - 1.0)),
            None,
            Some((item.addr, item.addr)),
            10_000,
        )?;
        if write_rows.is_empty() {
            continue;
        }
        let Some(write_row) = newest_first(&write_rows).cloned() else {
            continue;
        };

        let node_id = format!("{}@{}", js_num_str(write_row.cycle), js_hex(write_row.addr));
        if index.contains_key(&node_id) {
            // Already visited; add edge only.
            if let Some(p) = &item.parent_id {
                edges.push((p.clone(), node_id.clone()));
            }
            continue;
        }

        // The cpu_step that performed the write, identified by its PC.
        let cpu_rows = query_events(
            conn,
            shape,
            item.run_id.as_deref(),
            Family::CpuStep,
            Some((write_row.cycle - 10.0, write_row.cycle)),
            Some((write_row.pc, write_row.pc)),
            None,
            5,
        )?;
        let cpu_row = cpu_rows.first().cloned();
        let opcode = cpu_row.as_ref().map(|r| r.opcode).unwrap_or(0.0);
        let effects = opcode_effects(opcode);

        // Values sourced from CIA/VIC/SID hardware.
        let is_io = is_io_register_addr(write_row.addr)
            || (effects.kind == EffKind::None && is_io_register_addr(write_row.pc));

        let contribution: &'static str = if is_io {
            "io_register_read"
        } else {
            match effects.kind {
                EffKind::StackPush => "stack_push",
                EffKind::Transfer => "transfer",
                EffKind::RmwModify => "rmw_modify",
                EffKind::DirectWrite => "direct_write",
                // fallback — identical to the TS `else` arm
                EffKind::None => "direct_write",
            }
        };

        let mut inputs: Vec<TaintInput> = Vec::new();
        if let Some(r) = effects.source_reg {
            inputs.push(TaintInput::Reg(r.as_str()));
        }
        if effects.is_rmw {
            inputs.push(TaintInput::Addr(write_row.addr));
        }

        let node = TaintNode {
            id: node_id.clone(),
            cycle: write_row.cycle,
            pc: write_row.pc,
            addr: write_row.addr,
            value: write_row.value,
            contribution,
            inputs,
            domain: item.domain,
        };
        index.insert(node_id.clone(), order.len());
        order.push(node.clone());

        if root.is_none() {
            root = Some(node.clone());
        }
        if let Some(p) = &item.parent_id {
            edges.push((p.clone(), node_id.clone()));
        }

        // ---- IRQ-boundary check (only when followIrq is disabled) ----
        if !follow_irq {
            let irq_rows = query_events(
                conn,
                shape,
                item.run_id.as_deref(),
                Family::IrqAssert,
                Some((write_row.cycle - 500.0, write_row.cycle)),
                None,
                None,
                5,
            )?;
            if !irq_rows.is_empty() {
                let irq_id = format!("irq@{}", js_num_str(write_row.cycle));
                if !index.contains_key(&irq_id) {
                    let n = TaintNode {
                        id: irq_id.clone(),
                        cycle: write_row.cycle,
                        pc: write_row.pc,
                        addr: write_row.addr,
                        value: write_row.value,
                        contribution: "irq_boundary",
                        inputs: Vec::new(),
                        domain: item.domain,
                    };
                    index.insert(irq_id.clone(), order.len());
                    order.push(n);
                }
                edges.push((node_id.clone(), irq_id));
                continue;
            }
        }

        // ---- IEC cross-domain bridge (D2) ----
        if cross_domain && is_iec_bridge_addr(write_row.addr) && q.drive_run_id.is_some() {
            let drive_run_id = q.drive_run_id.clone().expect("checked above");
            let ddc_rows = query_events(
                conn,
                shape,
                item.run_id.as_deref(),
                Family::DriveDataChange,
                Some((write_row.cycle - 500.0, write_row.cycle + 100.0)),
                None,
                None,
                10,
            )?;
            if !ddc_rows.is_empty() {
                let bridge_id = format!("iec@{}", js_num_str(write_row.cycle));
                if !index.contains_key(&bridge_id) {
                    let n = TaintNode {
                        id: bridge_id.clone(),
                        cycle: write_row.cycle,
                        pc: write_row.pc,
                        addr: write_row.addr,
                        value: write_row.value,
                        contribution: "iec_bridge",
                        inputs: Vec::new(),
                        domain: "c64",
                    };
                    index.insert(bridge_id.clone(), order.len());
                    order.push(n);
                    edges.push((node_id.clone(), bridge_id.clone()));
                }
                // Drive-side: VIA1 port B ($1800) carries IEC data.
                queue.push_back(WorkItem {
                    run_id: Some(drive_run_id),
                    domain: "drive",
                    addr: 0x1800 as f64,
                    before_cycle: write_row.cycle + 100.0,
                    depth: item.depth + 1.0,
                    parent_id: Some(bridge_id),
                });
            }
            // Terminate IEC-sourced values — the `continue` is OUTSIDE the
            // `ddcRows.length > 0` guard in the reference, so an IEC bridge
            // address always ends the walk here.
            continue;
        }

        // ---- Recurse on the source register's last load ----
        if let (Some(src_reg), Some(cpu)) = (effects.source_reg, cpu_row.as_ref()) {
            match register_addr(src_reg, cpu) {
                Some(reg_addr) => {
                    queue.push_back(WorkItem {
                        run_id: item.run_id.clone(),
                        domain: item.domain,
                        addr: reg_addr,
                        before_cycle: write_row.cycle,
                        depth: item.depth + 1.0,
                        parent_id: Some(node_id.clone()),
                    });
                }
                None => {
                    let load_site = find_register_load(
                        conn,
                        shape,
                        item.run_id.as_deref(),
                        src_reg,
                        write_row.cycle,
                        js_max(0.0, min_cycle),
                        item.depth,
                        max_depth,
                    )?;
                    if let Some(addr) = load_site {
                        queue.push_back(WorkItem {
                            run_id: item.run_id.clone(),
                            domain: item.domain,
                            addr,
                            before_cycle: write_row.cycle,
                            depth: item.depth + 1.0,
                            parent_id: Some(node_id.clone()),
                        });
                    }
                }
            }
        }

        // ---- RMW: recurse on the prior value of the same address ----
        if effects.is_rmw {
            queue.push_back(WorkItem {
                run_id: item.run_id.clone(),
                domain: item.domain,
                addr: write_row.addr,
                before_cycle: write_row.cycle - 1.0,
                depth: item.depth + 1.0,
                parent_id: Some(node_id.clone()),
            });
        }

        if item.depth >= max_depth {
            truncated = true;
        }
    }

    // Fallback root when nothing was found — so `nodes` is never empty.
    let root = match root {
        Some(r) => r,
        None => {
            let fallback_id =
                format!("{}@{}", js_num_str(q.start_cycle), js_hex(q.start_addr));
            let n = TaintNode {
                id: fallback_id.clone(),
                cycle: q.start_cycle,
                pc: 0.0,
                addr: q.start_addr,
                value: 0.0,
                contribution: "direct_write",
                inputs: Vec::new(),
                domain: "c64",
            };
            index.insert(fallback_id, order.len());
            order.push(n.clone());
            n
        }
    };

    Ok(TaintGraph { root, nodes: order, edges, truncated })
}

/// `Number.prototype.toString(16)` — lowercase, unpadded, `-` for negatives.
fn js_hex(n: f64) -> String {
    if !n.is_finite() {
        return js_num_str(n);
    }
    if n.fract() != 0.0 {
        // Never occurs on an `addr` column; keep it readable rather than exact.
        return js_num_str(n);
    }
    let i = n as i128;
    if i < 0 {
        format!("-{:x}", -i)
    } else {
        format!("{i:x}")
    }
}

/// `findRegisterLoad` — walk back through cpu_steps for the instruction that
/// last loaded `reg`, then find the memory read at that instruction's PC.
#[allow(clippy::too_many_arguments)]
fn find_register_load(
    conn: &Connection,
    shape: StoreShape,
    run_id: Option<&str>,
    reg: Reg,
    before_cycle: f64,
    min_cycle: f64,
    depth: f64,
    max_depth: f64,
) -> Result<Option<f64>> {
    if depth >= max_depth {
        return Ok(None);
    }

    let steps = query_events(
        conn,
        shape,
        run_id,
        Family::CpuStep,
        Some((min_cycle, before_cycle - 1.0)),
        None,
        None,
        10_000,
    )?;

    // Newest → oldest (stable descending sort by cycle).
    let mut sorted: Vec<&EventRow> = steps.iter().collect();
    sorted.sort_by(|a, b| {
        b.cycle
            .partial_cmp(&a.cycle)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for step in sorted {
        let eff = opcode_effects(step.opcode);
        if eff.dest_reg != Some(reg) {
            continue;
        }
        let reads = query_events(
            conn,
            shape,
            run_id,
            Family::MemRead,
            Some((step.cycle - 10.0, step.cycle + 10.0)),
            Some((step.pc, step.pc)),
            None,
            5,
        )?;
        // The FIRST matching step decides — a load with no memory read
        // (immediate mode) terminates the walk instead of continuing it.
        return Ok(reads.first().map(|r| r.addr));
    }

    Ok(None)
}

/// `registerAddr` — only `SP` maps to memory (`$0100 + SP`).
fn register_addr(reg: Reg, cpu_step: &EventRow) -> Option<f64> {
    if reg == Reg::Sp {
        return Some(0x0100 as f64 + cpu_step.sp);
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// Text rendering — sidecar.ts `case "taint_text"`
// ─────────────────────────────────────────────────────────────────────────────

/// The monitor's `taint` output. `—` is U+2014; the hex is lowercase.
///
/// `start_addr` / `start_cycle` are the values from the REQUEST (not from the
/// graph), matching the reference.
pub fn render_graph_text(graph: &TaintGraph, start_addr: f64, start_cycle: f64) -> String {
    if graph.nodes.is_empty() {
        // Unreachable in practice — the fallback root guarantees one node.
        return format!(
            "taint: no contributing write found for ${} @cyc {} (try an explicit cycle from `swimlane`/`map`)",
            hx(start_addr),
            js_num_str(start_cycle)
        );
    }
    let mut lines = Vec::with_capacity(graph.nodes.len().min(40) + 1);
    lines.push(format!(
        "taint ${} @cyc {} \u{2014} {} node(s){}:",
        hx(start_addr),
        js_num_str(start_cycle),
        graph.nodes.len(),
        if graph.truncated { " (truncated)" } else { "" }
    ));
    for n in graph.nodes.iter().take(40) {
        lines.push(format!(
            "  cyc {} pc=${} {} ${}=${}",
            js_num_str(n.cycle),
            hx(n.pc),
            n.contribution,
            hx(n.addr),
            hx2(n.value)
        ));
    }
    lines.join("\n")
}

// ─────────────────────────────────────────────────────────────────────────────
// Op entry points
// ─────────────────────────────────────────────────────────────────────────────

/// Arguments of the `taint` / `taint_text` ops.
///
/// The two ops disagree on naming — `taint` forwards **camelCase** straight
/// into the TS query object, `taint_text` reads **snake_case** (Spec 802 R3
/// §5). [`TaintArgs::from_camel`] / [`TaintArgs::from_snake`] cover both;
/// the struct itself is naming-neutral.
#[derive(Clone, Debug, Default)]
pub struct TaintArgs {
    pub run_id: Option<String>,
    /// `None` means "not supplied": [`trace_taint`] treats it as `NaN`
    /// (`Number(undefined)`), [`render_taint`] resolves it from the store's
    /// own `MAX(cycle)`.
    pub start_cycle: Option<f64>,
    pub start_addr: f64,
    pub max_depth: Option<f64>,
    pub cycle_window: Option<f64>,
    pub follow_irq: Option<bool>,
    pub cross_domain: Option<bool>,
    pub drive_run_id: Option<String>,
}

fn opt_str(v: &Value, key: &str) -> Option<String> {
    match v.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    }
}

fn opt_num(v: &Value, key: &str) -> Option<f64> {
    match v.get(key) {
        None | Some(Value::Null) => None,
        some => Some(js_number(some)),
    }
}

impl TaintArgs {
    /// The `taint` op: camelCase, forwarded verbatim into the query object.
    pub fn from_camel(v: &Value) -> Self {
        TaintArgs {
            run_id: opt_str(v, "runId"),
            start_cycle: opt_num(v, "startCycle"),
            start_addr: js_number(v.get("startAddr")),
            max_depth: opt_num(v, "maxDepth"),
            cycle_window: opt_num(v, "cycleWindow"),
            follow_irq: v.get("followIrq").and_then(Value::as_bool),
            cross_domain: v.get("crossDomain").and_then(Value::as_bool),
            drive_run_id: opt_str(v, "driveRunId"),
        }
    }

    /// The `taint_text` op: snake_case, and only three fields are read
    /// (`traceTaint(backend, { runId, startCycle, startAddr })`) — every other
    /// knob stays at its default.
    pub fn from_snake(v: &Value) -> Self {
        let sc = js_number(v.get("start_cycle"));
        TaintArgs {
            run_id: opt_str(v, "run_id"),
            start_cycle: if sc.is_finite() { Some(sc) } else { None },
            start_addr: js_number(v.get("start_addr")),
            ..TaintArgs::default()
        }
    }

    fn to_query(&self, start_cycle: f64) -> TaintQuery {
        TaintQuery {
            run_id: self.run_id.clone(),
            start_cycle,
            start_addr: self.start_addr,
            max_depth: self.max_depth.unwrap_or(100.0),
            cycle_window: self.cycle_window.unwrap_or(1_000_000.0),
            follow_irq: self.follow_irq.unwrap_or(true),
            cross_domain: self.cross_domain.unwrap_or(true),
            drive_run_id: self.drive_run_id.clone(),
        }
    }
}

/// The `taint` op — the structured graph, `jsonSafe`d.
pub fn trace_taint(duckdb_path: &Path, args: &TaintArgs) -> Result<Value> {
    with_conn(duckdb_path, |conn, shape| {
        let q = args.to_query(args.start_cycle.unwrap_or(f64::NAN));
        Ok(taint_graph(conn, shape, &q)?.to_json())
    })
}

/// The monitor's `taint` verb — the text rendering of the same graph.
///
/// When `args.start_cycle` is absent the window anchors to the STORE's own
/// `MAX(cycle)`, not to a live clock.
pub fn render_taint(duckdb_path: &Path, args: &TaintArgs) -> Result<String> {
    with_conn(duckdb_path, |conn, shape| {
        let start_cycle = match args.start_cycle {
            Some(c) => c,
            None => max_cycle(conn)?,
        };
        let q = args.to_query(start_cycle);
        let graph = taint_graph(conn, shape, &q)?;
        Ok(render_graph_text(&graph, args.start_addr, start_cycle))
    })
}

/// `SELECT MAX(cycle) FROM trace_event WHERE cycle IS NOT NULL`, `?? 0`.
///
/// Shape-B only, exactly like the reference: on a legacy Shape-A store the
/// table does not exist and the query fails — the sidecar behaved identically.
fn max_cycle(conn: &Connection) -> Result<f64> {
    let v = crate::conn::scalar_u64(
        conn,
        "SELECT MAX(cycle) FROM trace_event WHERE cycle IS NOT NULL",
    )?;
    Ok(v.unwrap_or(0) as f64)
}

/// `SELECT run_id FROM trace_run LIMIT 1`.
///
/// **Not used by any op here.** It is the missing ingredient of Spec 802 R3
/// §3.2 option (b) — resolving the run id the way `swimlane_text` already does,
/// which would make the monitor's `taint` verb actually work. Wiring it in is a
/// deliberate, recorded divergence from the sidecar and must not happen
/// silently before the parity gate.
pub fn store_run_id(conn: &Connection) -> Result<Option<String>> {
    crate::conn::scalar_string(conn, "SELECT run_id FROM trace_run LIMIT 1")
}

/// Op wrapper: `taint` (camelCase args) → the graph JSON.
pub fn op_taint(duckdb_path: &Path, args: &Value) -> Result<Value> {
    trace_taint(duckdb_path, &TaintArgs::from_camel(args))
}

/// Op wrapper: `taint_text` (snake_case args) → `{"text": …}`.
pub fn op_taint_text(duckdb_path: &Path, args: &Value) -> Result<Value> {
    let text = render_taint(duckdb_path, &TaintArgs::from_snake(args))?;
    let mut m = Map::new();
    m.insert("text".into(), Value::from(text));
    Ok(Value::Object(m))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::create_trace_run_store;

    /// A Shape-B store in memory, plus a tiny event-writing helper.
    struct Fixture {
        conn: Connection,
        seq: u64,
    }

    impl Fixture {
        fn new() -> Self {
            let conn = Connection::open_in_memory().expect("in-memory duckdb");
            create_trace_run_store(&conn).expect("shape B ddl");
            Fixture { conn, seq: 0 }
        }

        fn ev(&mut self, cycle: u64, channel: &str, data_json: &str) {
            let sql = format!(
                "INSERT INTO trace_event VALUES ('r1', {}, {}, '{}', 'mem-access', 'mem-row', '{}')",
                self.seq,
                cycle,
                channel,
                data_json.replace('\'', "''")
            );
            self.conn.execute_batch(&sql).expect("insert trace_event");
            self.seq += 1;
        }

        /// A `cpu` channel row (the `instructions` projection).
        fn cpu(&mut self, cycle: u64, pc: u16, opcode: u8, sp: u8) {
            let d = format!(
                r#"{{"pc":{pc},"opcode":{opcode},"b1":0,"b2":0,"a":0,"x":0,"y":0,"sp":{sp},"p":0}}"#
            );
            self.ev(cycle, "cpu", &d);
        }

        /// A `bus_access` write row.
        fn write(&mut self, cycle: u64, addr: u16, value: u8, pc: u16) {
            let d = format!(
                r#"{{"addr":{addr},"value":{value},"op":"write","pc":{pc},"side":"c64","cycle_c64":{cycle}}}"#
            );
            self.ev(cycle, "bus_access", &d);
        }

        /// A `bus_access` read row.
        fn read(&mut self, cycle: u64, addr: u16, value: u8, pc: u16) {
            let d = format!(
                r#"{{"addr":{addr},"value":{value},"op":"read","pc":{pc},"side":"c64","cycle_c64":{cycle}}}"#
            );
            self.ev(cycle, "bus_access", &d);
        }

        /// An `io` write row (channel `io`, still `bus_events`).
        fn io_write(&mut self, cycle: u64, addr: u16, value: u8, pc: u16) {
            let d = format!(
                r#"{{"addr":{addr},"value":{value},"op":"write","pc":{pc},"side":"c64","cycle_c64":{cycle}}}"#
            );
            self.ev(cycle, "io", &d);
        }

        fn run(&self, q: &TaintQuery) -> TaintGraph {
            taint_graph(&self.conn, StoreShape::Spec726, q).expect("taint")
        }
    }

    // ── number / formatting helpers ─────────────────────────────────────────

    #[test]
    fn js_number_formatting_has_no_decimal_point() {
        assert_eq!(js_num_str(12345.0), "12345");
        assert_eq!(js_num_str(0.0), "0");
        assert_eq!(js_num_str(-1.0), "-1");
        assert_eq!(js_num_str(f64::NAN), "NaN");
        assert_eq!(js_num_str(f64::INFINITY), "Infinity");
        assert_eq!(jn(1000.0), serde_json::json!(1000));
        assert_eq!(serde_json::to_string(&jn(5000.0)).unwrap(), "5000");
        assert_eq!(jn(f64::NAN), Value::Null);
    }

    #[test]
    fn hex_helpers_match_the_sidecar() {
        assert_eq!(hx(0xc000 as f64), "c000");
        assert_eq!(hx(0.0), "0000");
        assert_eq!(hx(0x1c000 as f64), "c000"); // & 0xffff
        assert_eq!(hx(f64::NAN), "0000"); // ToInt32(NaN) === 0
        assert_eq!(hx2(0x42 as f64), "42");
        assert_eq!(hx2(5.0), "05");
        assert_eq!(js_hex(0xc800 as f64), "c800");
        assert_eq!(js_hex(0.0), "0");
    }

    #[test]
    fn js_max_poisons_on_nan_unlike_rust_f64_max() {
        assert_eq!(js_max(0.0, 5.0), 5.0);
        assert!(js_max(0.0, f64::NAN).is_nan());
        // The trap this helper exists for:
        assert_eq!(f64::max(0.0, f64::NAN), 0.0);
    }

    #[test]
    fn non_finite_params_fail_like_inline_param() {
        let e = inline_num(f64::NAN).unwrap_err();
        assert_eq!(e.to_string(), "non-finite param: NaN");
        assert_eq!(inline_num(7.0).unwrap(), "7");
    }

    // ── opcode table ────────────────────────────────────────────────────────

    #[test]
    fn opcode_effects_table() {
        assert_eq!(opcode_effects(0x8d as f64).kind, EffKind::DirectWrite);
        assert_eq!(opcode_effects(0x8d as f64).source_reg, Some(Reg::A));
        assert_eq!(opcode_effects(0x86 as f64).source_reg, Some(Reg::X));
        assert_eq!(opcode_effects(0x84 as f64).source_reg, Some(Reg::Y));
        assert!(opcode_effects(0xee as f64).is_rmw);
        assert_eq!(opcode_effects(0xee as f64).kind, EffKind::RmwModify);
        assert_eq!(opcode_effects(0x48 as f64).kind, EffKind::StackPush);
        assert_eq!(opcode_effects(0x08 as f64).source_reg, Some(Reg::Flags));
        assert_eq!(opcode_effects(0x20 as f64).source_reg, None);
        assert_eq!(opcode_effects(0xba as f64).source_reg, Some(Reg::Sp));
        assert_eq!(opcode_effects(0xba as f64).dest_reg, Some(Reg::X));
        // LDA is deliberately NOT in the table — defect (3) in the header.
        assert_eq!(opcode_effects(0xa9 as f64).kind, EffKind::None);
        assert_eq!(opcode_effects(0xa5 as f64).dest_reg, None);
        // Missing cpu_step ⇒ opcode 0 ⇒ BRK ⇒ none.
        assert_eq!(opcode_effects(0.0).kind, EffKind::None);
    }

    // ── the algorithm ───────────────────────────────────────────────────────

    #[test]
    fn single_direct_write_is_one_node_with_a_register_input() {
        let mut f = Fixture::new();
        f.cpu(1000, 0xc000, 0x8d, 0xff); // STA $c800
        f.write(1000, 0xc800, 0x42, 0xc000);

        let g = f.run(&TaintQuery::new(Some("r1".into()), 2000.0, 0xc800 as f64));
        assert_eq!(g.nodes.len(), 1);
        let n = &g.nodes[0];
        assert_eq!(n.id, "1000@c800");
        assert_eq!(n.contribution, "direct_write");
        assert_eq!(n.value, 0x42 as f64);
        assert_eq!(n.pc, 0xc000 as f64);
        assert_eq!(n.inputs, vec![TaintInput::Reg("A")]);
        assert_eq!(n.domain, "c64");
        assert!(g.edges.is_empty());
        assert!(!g.truncated);
        assert_eq!(g.root.id, n.id);

        assert_eq!(
            render_graph_text(&g, 0xc800 as f64, 2000.0),
            "taint $c800 @cyc 2000 \u{2014} 1 node(s):\n  cyc 1000 pc=$c000 direct_write $c800=$42"
        );
    }

    #[test]
    fn the_newest_write_wins_and_older_ones_are_ignored() {
        let mut f = Fixture::new();
        f.cpu(1000, 0xc000, 0x8d, 0xff);
        f.write(1000, 0xc800, 0x11, 0xc000);
        f.cpu(1500, 0xc010, 0x8d, 0xff);
        f.write(1500, 0xc800, 0x22, 0xc010);
        // After the start cycle — must not be seen.
        f.cpu(2500, 0xc020, 0x8d, 0xff);
        f.write(2500, 0xc800, 0x33, 0xc020);

        let g = f.run(&TaintQuery::new(Some("r1".into()), 2000.0, 0xc800 as f64));
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].value, 0x22 as f64);
        assert_eq!(g.nodes[0].id, "1500@c800");
    }

    #[test]
    fn rmw_recurses_onto_the_prior_value_of_the_same_address() {
        let mut f = Fixture::new();
        f.cpu(1000, 0xc000, 0x8d, 0xff); // STA $c800
        f.write(1000, 0xc800, 0x04, 0xc000);
        f.cpu(2000, 0xc010, 0xee, 0xff); // INC $c800
        f.write(2000, 0xc800, 0x05, 0xc010);

        let g = f.run(&TaintQuery::new(Some("r1".into()), 3000.0, 0xc800 as f64));
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.nodes[0].id, "2000@c800");
        assert_eq!(g.nodes[0].contribution, "rmw_modify");
        assert_eq!(g.nodes[0].inputs, vec![TaintInput::Addr(0xc800 as f64)]);
        assert_eq!(g.nodes[1].id, "1000@c800");
        assert_eq!(g.nodes[1].contribution, "direct_write");
        assert_eq!(g.edges, vec![("2000@c800".to_string(), "1000@c800".to_string())]);
        assert_eq!(g.root.id, "2000@c800");
    }

    #[test]
    fn an_io_target_is_classified_as_an_io_register_read() {
        let mut f = Fixture::new();
        f.cpu(1000, 0xc000, 0x8d, 0xff);
        f.io_write(1000, 0xd020, 0x0e, 0xc000);

        let g = f.run(&TaintQuery::new(Some("r1".into()), 2000.0, 0xd020 as f64));
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].contribution, "io_register_read");
        // The source register input survives the IO reclassification.
        assert_eq!(g.nodes[0].inputs, vec![TaintInput::Reg("A")]);
    }

    #[test]
    fn an_unclassified_opcode_at_an_io_pc_is_also_an_io_register_read() {
        let mut f = Fixture::new();
        // opcode 0xa9 (LDA #) classifies as `none`; the PC is in IO space.
        f.cpu(1000, 0xd400, 0xa9, 0xff);
        f.write(1000, 0x0400, 0x01, 0xd400);

        let g = f.run(&TaintQuery::new(Some("r1".into()), 2000.0, 0x0400 as f64));
        assert_eq!(g.nodes[0].contribution, "io_register_read");
        assert!(g.nodes[0].inputs.is_empty());
    }

    #[test]
    fn transfer_source_follows_a_memory_read_at_the_transfer_site() {
        let mut f = Fixture::new();
        // TXA at $c005 reads $2000 (contrived, but it is what the reference
        // looks for: a mem_read at the transfer instruction's PC).
        f.cpu(900, 0xc005, 0x8a, 0xff); // TXA — destReg A
        f.read(900, 0x2000, 0x77, 0xc005);
        // The write we taint: STA $c800, sourceReg A.
        f.cpu(1000, 0xc000, 0x8d, 0xff);
        f.write(1000, 0xc800, 0x42, 0xc000);
        // And a prior write to $2000, so the recursion produces a 2nd node.
        f.cpu(500, 0xc100, 0x8d, 0xff);
        f.write(500, 0x2000, 0x77, 0xc100);

        let g = f.run(&TaintQuery::new(Some("r1".into()), 2000.0, 0xc800 as f64));
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.nodes[0].id, "1000@c800");
        assert_eq!(g.nodes[1].id, "500@2000");
        assert_eq!(g.edges, vec![("1000@c800".to_string(), "500@2000".to_string())]);
    }

    #[test]
    fn tsx_maps_the_stack_pointer_to_0100_plus_sp() {
        let mut f = Fixture::new();
        // TSX (0xba) has sourceReg SP → registerAddr → $0100 + sp.
        f.cpu(1000, 0xc000, 0xba, 0x30);
        f.write(1000, 0xc800, 0x42, 0xc000);
        // A prior write into $0130 so the SP recursion lands somewhere.
        f.cpu(600, 0xc100, 0x8d, 0xff);
        f.write(600, 0x0130, 0x99, 0xc100);

        let g = f.run(&TaintQuery::new(Some("r1".into()), 2000.0, 0xc800 as f64));
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.nodes[0].contribution, "transfer");
        assert_eq!(g.nodes[1].id, "600@130");
        assert_eq!(g.edges, vec![("1000@c800".to_string(), "600@130".to_string())]);
    }

    #[test]
    fn a_revisited_node_only_adds_an_edge() {
        let mut f = Fixture::new();
        // $c800 is INC'd twice; both RMW steps walk back to the same origin,
        // and the second visit must add an edge without duplicating the node.
        f.cpu(1000, 0xc000, 0x8d, 0xff);
        f.write(1000, 0xc800, 0x01, 0xc000);
        f.cpu(2000, 0xc010, 0xee, 0xff);
        f.write(2000, 0xc800, 0x02, 0xc010);
        f.cpu(3000, 0xc020, 0xee, 0xff);
        f.write(3000, 0xc800, 0x03, 0xc020);

        let g = f.run(&TaintQuery::new(Some("r1".into()), 4000.0, 0xc800 as f64));
        let ids: Vec<&str> = g.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["3000@c800", "2000@c800", "1000@c800"]);
        assert_eq!(
            g.edges,
            vec![
                ("3000@c800".to_string(), "2000@c800".to_string()),
                ("2000@c800".to_string(), "1000@c800".to_string()),
            ]
        );
    }

    #[test]
    fn the_cycle_window_bounds_the_search() {
        let mut f = Fixture::new();
        f.cpu(10, 0xc000, 0x8d, 0xff);
        f.write(10, 0xc800, 0x42, 0xc000);

        let mut q = TaintQuery::new(Some("r1".into()), 100_000.0, 0xc800 as f64);
        q.cycle_window = 1000.0; // minCycle = 99_000 — the write at 10 is outside
        let g = f.run(&q);
        // Fallback root only.
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].id, "100000@c800");
        assert_eq!(g.nodes[0].pc, 0.0);
        assert_eq!(g.nodes[0].value, 0.0);
        assert_eq!(g.nodes[0].contribution, "direct_write");
    }

    #[test]
    fn max_depth_truncates_and_flags_the_graph() {
        let mut f = Fixture::new();
        for i in 0..6u64 {
            let c = 1000 + i * 100;
            f.cpu(c, 0xc000 + i as u16, 0xee, 0xff); // INC $c800 chain
            f.write(c, 0xc800, i as u8, 0xc000 + i as u16);
        }
        let mut q = TaintQuery::new(Some("r1".into()), 5000.0, 0xc800 as f64);
        q.max_depth = 2.0;
        let g = f.run(&q);
        assert!(g.truncated, "depth guard must mark the graph truncated");
        assert_eq!(g.nodes.len(), 3, "depths 0,1,2 are walked; depth 3 is cut");
    }

    // ── the fallback root + the run_id defect ───────────────────────────────

    #[test]
    fn nothing_found_still_yields_a_synthetic_root() {
        let f = Fixture::new();
        let g = f.run(&TaintQuery::new(Some("r1".into()), 777.0, 0xdead as f64));
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.root.id, "777@dead");
        assert_eq!(g.root.cycle, 777.0);
        assert_eq!(g.root.addr, 0xdead as f64);
        assert_eq!(g.root.domain, "c64");
        assert!(g.root.inputs.is_empty());
        assert_eq!(
            render_graph_text(&g, 0xdead as f64, 777.0),
            "taint $dead @cyc 777 \u{2014} 1 node(s):\n  cyc 777 pc=$0000 direct_write $dead=$00"
        );
    }

    /// Spec 802 R3 §3.2 — pinned defect: the monitor sends no `run_id`, so
    /// every subquery runs `WHERE run_id = NULL`, matches nothing, and the
    /// synthetic root is all that is printed. Option (a): reproduce it.
    #[test]
    fn a_missing_run_id_matches_nothing_pinned_defect() {
        let mut f = Fixture::new();
        f.cpu(1000, 0xc000, 0x8d, 0xff);
        f.write(1000, 0xc800, 0x42, 0xc000);

        // With the run id: the real write is found.
        let with_id = f.run(&TaintQuery::new(Some("r1".into()), 2000.0, 0xc800 as f64));
        assert_eq!(with_id.nodes[0].value, 0x42 as f64);

        // Without it: `run_id = NULL` → nothing → the bogus single node.
        let without = f.run(&TaintQuery::new(None, 2000.0, 0xc800 as f64));
        assert_eq!(without.nodes.len(), 1);
        assert_eq!(without.nodes[0].pc, 0.0);
        assert_eq!(without.nodes[0].value, 0.0);
        assert_eq!(
            render_graph_text(&without, 0xc800 as f64, 2000.0),
            "taint $c800 @cyc 2000 \u{2014} 1 node(s):\n  cyc 2000 pc=$0000 direct_write $c800=$00"
        );

        // …and the ingredient for the eventual fix is available but unwired.
        f.conn
            .execute_batch(
                "INSERT INTO trace_run (run_id) VALUES ('r1')",
            )
            .expect("seed trace_run");
        assert_eq!(store_run_id(&f.conn).unwrap().as_deref(), Some("r1"));
    }

    // ── JSON shape ──────────────────────────────────────────────────────────

    #[test]
    fn json_shape_matches_the_structured_op() {
        let mut f = Fixture::new();
        f.cpu(1000, 0xc000, 0x8d, 0xff);
        f.write(1000, 0xc800, 0x04, 0xc000);
        f.cpu(2000, 0xc010, 0xee, 0xff);
        f.write(2000, 0xc800, 0x05, 0xc010);

        let g = f.run(&TaintQuery::new(Some("r1".into()), 3000.0, 0xc800 as f64));
        let j = g.to_json();

        assert_eq!(j["truncated"], Value::Bool(false));
        assert_eq!(j["root"]["id"], "2000@c800");
        assert_eq!(j["nodes"]["2000@c800"]["contribution"], "rmw_modify");
        assert_eq!(j["nodes"]["1000@c800"]["value"], serde_json::json!(4));
        assert_eq!(j["edges"][0]["from"], "2000@c800");
        assert_eq!(j["edges"][0]["to"], "1000@c800");
        assert_eq!(j["nodes"]["2000@c800"]["inputs"][0]["addr"], serde_json::json!(51200));
        assert_eq!(j["nodes"]["1000@c800"]["inputs"][0]["reg"], "A");

        // Integers must serialise WITHOUT a decimal point (R2 §J-1).
        let s = serde_json::to_string(&j).unwrap();
        assert!(s.contains("\"cycle\":2000"), "got {s}");
        assert!(!s.contains(".0"), "no float formatting anywhere: {s}");

        // A node carries no `reg` key (traceTaint never sets it).
        assert!(j["root"].get("reg").is_none());
    }

    // ── argument parsing ────────────────────────────────────────────────────

    #[test]
    fn arg_parsing_camel_and_snake() {
        let camel = serde_json::json!({
            "runId": "r1", "startCycle": 12345, "startAddr": 53248,
            "maxDepth": 10, "cycleWindow": 500, "followIrq": false,
            "crossDomain": false, "driveRunId": "d8"
        });
        let a = TaintArgs::from_camel(&camel);
        assert_eq!(a.run_id.as_deref(), Some("r1"));
        assert_eq!(a.start_cycle, Some(12345.0));
        assert_eq!(a.start_addr, 53248.0);
        assert_eq!(a.max_depth, Some(10.0));
        assert_eq!(a.cycle_window, Some(500.0));
        assert_eq!(a.follow_irq, Some(false));
        assert_eq!(a.cross_domain, Some(false));
        assert_eq!(a.drive_run_id.as_deref(), Some("d8"));

        let snake = serde_json::json!({ "start_addr": 49152, "start_cycle": 999 });
        let b = TaintArgs::from_snake(&snake);
        assert_eq!(b.start_addr, 49152.0);
        assert_eq!(b.start_cycle, Some(999.0));
        assert_eq!(b.run_id, None);
        // taint_text reads only three fields; everything else defaults.
        assert_eq!(b.max_depth, None);
        assert!(b.to_query(999.0).follow_irq);
        assert_eq!(b.to_query(999.0).cycle_window, 1_000_000.0);

        // Absent start_cycle ⇒ resolve from the store.
        let c = TaintArgs::from_snake(&serde_json::json!({ "start_addr": 1 }));
        assert_eq!(c.start_cycle, None);
        // Absent startAddr ⇒ NaN ⇒ the query fails like `inlineParam` did.
        let d = TaintArgs::from_camel(&serde_json::json!({ "runId": "r1" }));
        assert!(d.start_addr.is_nan());
    }

    #[test]
    fn a_nan_start_addr_fails_with_the_inline_param_message() {
        let f = Fixture::new();
        let q = TaintQuery::new(Some("r1".into()), 1000.0, f64::NAN);
        let e = taint_graph(&f.conn, StoreShape::Spec726, &q).unwrap_err();
        assert_eq!(e.to_string(), "non-finite param: NaN");
    }

    // ── store-shape routing ─────────────────────────────────────────────────

    #[test]
    fn from_source_routing_per_shape() {
        assert!(from_source(StoreShape::Spec726, "instructions")
            .unwrap()
            .contains("FROM trace_event"));
        assert!(from_source(StoreShape::Spec726, "bus_events")
            .unwrap()
            .contains("FROM trace_event"));
        // chip_events has no 726 producer → empty result, not an error.
        assert!(from_source(StoreShape::Spec726, "chip_events").is_none());
        assert_eq!(
            from_source(StoreShape::Legacy217, "instructions").unwrap(),
            "instructions"
        );
        assert_eq!(
            from_source(StoreShape::Legacy217, "chip_events").unwrap(),
            "chip_events"
        );
    }

    #[test]
    fn irq_and_iec_probes_are_inert_on_a_726_store() {
        let mut f = Fixture::new();
        f.cpu(1000, 0xc000, 0x8d, 0xff);
        f.write(1000, 0xc800, 0x42, 0xc000);

        // followIrq=false would consult chip_events — empty on Shape B, so the
        // walk continues exactly as with followIrq=true.
        let mut q = TaintQuery::new(Some("r1".into()), 2000.0, 0xc800 as f64);
        q.follow_irq = false;
        let g = f.run(&q);
        assert_eq!(g.nodes.len(), 1);
        assert!(g.nodes.iter().all(|n| n.contribution != "irq_boundary"));
    }

    #[test]
    fn an_iec_bridge_address_terminates_the_walk() {
        let mut f = Fixture::new();
        f.cpu(1000, 0xc000, 0x8d, 0xff);
        f.io_write(1000, 0xdd0d, 0x81, 0xc000);

        let mut q = TaintQuery::new(Some("r1".into()), 2000.0, 0xdd0d as f64);
        q.drive_run_id = Some("d8".into());
        let g = f.run(&q);
        // No `drive_data_change` rows exist, so no bridge node is created —
        // but the `continue` still fires, so no register recursion happens.
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].contribution, "io_register_read");
        assert!(g.edges.is_empty());
    }

    // ── parity harness (Spec 802 §5.2) ──────────────────────────────────────

    /// Write the taint parity corpus — a Shape-B store exercising every branch
    /// of `traceTaint` — so the SAME file can be handed to the TS sidecar and
    /// to the native ops.
    ///
    /// Opt-in; SKIPs when `TRX64_TAINT_FIXTURE_OUT` is unset.
    ///
    /// ```text
    /// TRX64_TAINT_FIXTURE_OUT=/tmp/taint_corpus.duckdb \
    ///   cargo test -p trx64-traceindex --lib taint::tests::write_the_parity_corpus
    /// ```
    #[test]
    fn write_the_parity_corpus() {
        let Ok(out) = std::env::var("TRX64_TAINT_FIXTURE_OUT") else {
            eprintln!("SKIP: set TRX64_TAINT_FIXTURE_OUT=<path>.duckdb");
            return;
        };
        let _ = std::fs::remove_file(&out);
        let conn = Connection::open(&out).expect("open corpus store");
        create_trace_run_store(&conn).expect("shape B ddl");
        let mut f = Fixture { conn, seq: 0 };

        // (a) direct_write whose source register resolves through a transfer
        //     (TXA at $c005 reads $2000) into a second node.
        f.cpu(500, 0xc100, 0x8d, 0xff);
        f.write(500, 0x2000, 0x77, 0xc100);
        f.cpu(600, 0xc110, 0x8d, 0xff);
        f.write(600, 0x0130, 0x99, 0xc110);
        f.cpu(900, 0xc005, 0x8a, 0xff); // TXA — destReg A
        f.read(900, 0x2000, 0x77, 0xc005);
        f.cpu(1000, 0xc000, 0x8d, 0xff); // STA $c800
        f.write(1000, 0xc800, 0x42, 0xc000);

        // (b) an RMW chain: STA then two INCs on the same address.
        f.cpu(1100, 0xc020, 0x8d, 0xff);
        f.write(1100, 0xc900, 0x01, 0xc020);
        f.cpu(1200, 0xc030, 0xee, 0xff); // INC $c900
        f.write(1200, 0xc900, 0x02, 0xc030);
        f.cpu(1300, 0xc040, 0xee, 0xff);
        f.write(1300, 0xc900, 0x03, 0xc040);

        // (c) an IO target → io_register_read.
        f.cpu(1500, 0xc200, 0x8d, 0xff);
        f.io_write(1500, 0xd020, 0x0e, 0xc200);

        // (d) an IEC bridge address ($dd0d) → terminates the walk.
        f.cpu(1600, 0xc300, 0x8d, 0xff);
        f.io_write(1600, 0xdd0d, 0x81, 0xc300);

        // (e) TSX → registerAddr maps SP to $0100 + sp ($0130 above).
        f.cpu(1700, 0xc400, 0xba, 0x30);
        f.write(1700, 0xca00, 0x42, 0xc400);

        // (f) an unclassified opcode executing in IO space → io_register_read.
        f.cpu(1800, 0xd400, 0xa9, 0xff);
        f.write(1800, 0x0400, 0x01, 0xd400);

        // (g) PHA / JSR / PHP — the stack_push arm.
        f.cpu(1900, 0xc500, 0x48, 0xfe); // PHA
        f.write(1900, 0x01fe, 0x33, 0xc500);
        f.cpu(1950, 0xc510, 0x08, 0xfd); // PHP
        f.write(1950, 0x01fd, 0x24, 0xc510);

        // (h) a long RMW chain — the 40-line render cap and maxDepth.
        for i in 0..45u64 {
            let c = 2000 + i * 10;
            f.cpu(c, 0xc600, 0xee, 0xff);
            f.write(c, 0xcb00, i as u8, 0xc600);
        }

        // (i) a mark + a run header, so the store looks like a real one.
        f.conn
            .execute_batch(
                "INSERT INTO trace_mark VALUES ('r1', 1000, 'start');\n\
                 INSERT INTO trace_run (run_id, def_id, def_version, name, cycle_start, \
                   cycle_end, event_count, retention, created_at) \
                 VALUES ('r1', 'taint-corpus', 1, 'taint corpus', 500, 2440, 0, 'keep', \
                   '2026-01-01T00:00:00.000Z');",
            )
            .expect("seed run + mark");

        // Fold the WAL in so a SEPARATE PROCESS (the sidecar) sees the rows.
        let _ = f.conn.execute_batch("CHECKPOINT");
        drop(f.conn);
        eprintln!("taint parity corpus written to {out}");
    }

    /// Replay recorded sidecar answers against the native ops.
    ///
    /// Opt-in, because `.duckdb` stores are gitignored and a fresh clone has
    /// none — it SKIPs cleanly when the env is unset, exactly like the crate's
    /// `.c64retrace` integration test.
    ///
    /// ```text
    /// TRX64_TAINT_PARITY_DUCKDB=/path/to/trace.duckdb \
    /// TRX64_TAINT_PARITY_CASES=/path/to/cases.jsonl \
    /// cargo test -p trx64-traceindex --lib taint::tests::parity
    /// ```
    ///
    /// Each line of the cases file is one recorded sidecar call:
    /// `{"op": "taint"|"taint_text", "args": {…}, "expect": <the sidecar's result>}`
    /// — `args` in the op's own naming convention (camelCase for `taint`,
    /// snake_case for `taint_text`, per Spec 802 R3 §5).
    #[test]
    fn parity_against_recorded_sidecar_answers() {
        let (Ok(db), Ok(cases)) = (
            std::env::var("TRX64_TAINT_PARITY_DUCKDB"),
            std::env::var("TRX64_TAINT_PARITY_CASES"),
        ) else {
            eprintln!("SKIP: set TRX64_TAINT_PARITY_DUCKDB + TRX64_TAINT_PARITY_CASES");
            return;
        };
        let db = std::path::PathBuf::from(db);
        let body = std::fs::read_to_string(&cases).expect("read cases file");

        let mut n = 0usize;
        for (lineno, line) in body.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let case: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("case {} is not JSON: {e}", lineno + 1));
            let op = case["op"].as_str().unwrap_or("taint");
            let args = &case["args"];
            let expect = &case["expect"];

            let got = match op {
                "taint" => op_taint(&db, args).expect("native taint"),
                "taint_text" => op_taint_text(&db, args).expect("native taint_text"),
                other => panic!("case {}: unknown op {other}", lineno + 1),
            };
            assert_eq!(
                &got,
                expect,
                "\ncase {} ({op}) args={args}\n  native:  {got}\n  sidecar: {expect}\n",
                lineno + 1
            );
            n += 1;
        }
        assert!(n > 0, "the cases file contained no cases");
        eprintln!("parity: {n} recorded sidecar answers reproduced");
    }

    #[test]
    fn render_caps_the_body_at_40_lines_but_counts_them_all() {
        let mut f = Fixture::new();
        for i in 0..45u64 {
            let c = 1000 + i * 10;
            f.cpu(c, 0xc000, 0xee, 0xff);
            f.write(c, 0xc800, i as u8, 0xc000);
        }
        let g = f.run(&TaintQuery::new(Some("r1".into()), 2000.0, 0xc800 as f64));
        let txt = render_graph_text(&g, 0xc800 as f64, 2000.0);
        let lines: Vec<&str> = txt.split('\n').collect();
        assert_eq!(lines.len(), 41, "1 header + 40 body lines");
        assert!(lines[0].starts_with(&format!(
            "taint $c800 @cyc 2000 \u{2014} {} node(s)",
            g.nodes.len()
        )));
        assert!(g.nodes.len() > 40);
    }
}
