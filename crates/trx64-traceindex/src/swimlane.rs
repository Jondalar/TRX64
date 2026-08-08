//! Native `swimlane` / `chis` — Spec 802 §4.1, ops `swimlane` and `swimlane_text`.
//!
//! Port of `C64ReverseEngineeringMCP/src/runtime/headless/v2/swimlane.ts`
//! (`swimlaneSlice`) + `.../v2/swimlane-render.ts` (`renderText`) + the
//! `queryEvents` subset those two need
//! (`.../v2/query-events.ts`, `.../v2/duckdb-backend.ts`) + the flow derivation
//! (`.../v2/flow-focus.ts` `deriveFlow`). The mnemonic table is lifted from
//! `src/exomizer-ts/generated-opcodes.ts` + `.../cpu/undoc-table.ts`.
//!
//! **The text is the contract.** Monitor transcripts must not change, so the
//! renderer is reproduced byte-for-byte, including the non-ASCII glyphs:
//! `–` U+2013 (window separator), `↺` U+21BA + `×` U+00D7 (fold tag) and `…`
//! U+2026 (truncation / cell summary).
//!
//! # Two daemon entry points, one op
//!
//! Both the `swimlane` / `sw` verb and the `chis` verb reach `swimlane_text`;
//! the daemon builds different args and the sidecar's window resolution does the
//! rest ([`op_swimlane_text`]):
//!
//! | verb | args the daemon sends | effect |
//! |---|---|---|
//! | `swimlane` | `{last_cycles: 2000, stem}` | tail of the store: `[max(min, max−2000) .. max]` |
//! | `swimlane <s> [e]` | `+ {cycle_start, cycle_end?}` | explicit window; a missing `cycle_end` still resolves to `MAX(cycle)` |
//! | `swimlane <name> …` | same, against the named store | the daemon picks the path; nothing changes here |
//! | `chis` / `chis <n>` | `{last_cycles: n ?? 4000, stem}` | identical code path, wider default span |
//! | `chis <s> <e>` | `{cycle_start, cycle_end, stem}` | explicit window |
//!
//! `chis` only reaches here as a **fallback**: the daemon serves it from the
//! live `cpu_history` ring first and falls back to the finalized trace when the
//! ring is empty or the window predates it. That decision, the `# cpuhistory
//! (live ring)` header and the `chis: `/`swimlane: ` error prefixes stay in the
//! daemon — this module owns only what the sidecar owned.
//!
//! # Preserved defects (do NOT "fix" — the parity gate would diverge)
//!
//! 1. **The drive lanes are never populated.** `swimlaneSlice` declares
//!    `drvPc` / `drvOp` / `drvIoRw` / `drvIoAddr` / `drvIoValue` and never
//!    assigns them, so the `1541` and `drv_io` columns can never appear.
//! 2. **`drive_pc` rows are rendered as C64 steps.** The `cpu_step` family maps
//!    to `instructions`, whose Spec-726 projection covers
//!    `channel IN ('cpu','drive_pc')`, and `queryEvents` applies no `cpu`
//!    filter — so drive instructions land in the `c64` column *and* in the
//!    stack-delta stream the flow lane is derived from.
//! 3. **A missing `run_id` matches nothing.** `duckdb-backend.ts` inlines
//!    `undefined` as the SQL literal `NULL`, so `run_id = NULL` is never true.
//!    A store with an empty `trace_run` therefore renders exactly
//!    `swimlane <s>–<e>: (no events in window)`.
//!
//! # One deliberate (behaviour-neutral) consolidation
//!
//! `swimlaneSlice` issues three *byte-identical* queries for the
//! `drive_atn_change` / `drive_clk_change` / `drive_data_change` families — same
//! table, same `kind = 'line_change'` filter, same window, same `LIMIT`; only
//! the column each one projects into `level` differs. This port runs that query
//! **once** and reads all three lines off the same rows. Same rows, same order,
//! same result — three round-trips fewer.

use crate::conn::with_conn;
use crate::error::{Result, TraceReadError};
use crate::schema::{
    StoreShape, BUS_EVENTS_726, INSTRUCTIONS_726, LEGACY_BUS_EVENTS, LEGACY_INSTRUCTIONS,
};
use duckdb::types::Value as DuckValue;
use duckdb::Connection;
use serde_json::Value as Json;
use std::collections::HashMap;
use std::path::Path;

/// `renderText`'s default row cap (`opts.maxRows ?? 200`).
pub const MAX_ROWS_DEFAULT: usize = 200;
/// `foldCells`' default maximum loop period.
pub const MAX_FOLD_PERIOD: usize = 64;
/// `queryEvents` limit `swimlaneSlice` passes for every family.
const EVENT_LIMIT: u32 = 100_000;
/// `sidecar.ts`: `Number(a.last_cycles ?? 2000)`.
const DEFAULT_SPAN: f64 = 2000.0;

// ─────────────────────────────────────────────────────────────────────────────
// Public model
// ─────────────────────────────────────────────────────────────────────────────

/// Spec 746.13 execution-context lane (`flow-focus.ts` `FlowKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowKind {
    Main,
    Irq,
    Nmi,
}

impl FlowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FlowKind::Main => "main",
            FlowKind::Irq => "irq",
            FlowKind::Nmi => "nmi",
        }
    }
    pub fn parse(s: &str) -> Option<FlowKind> {
        match s {
            "main" => Some(FlowKind::Main),
            "irq" => Some(FlowKind::Irq),
            "nmi" => Some(FlowKind::Nmi),
            _ => None,
        }
    }
}

/// One row of the shared cycle timeline (`SwimlaneRow`).
///
/// `drv_*` are declared and never assigned — see preserved defect 1.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SwimlaneRow {
    pub cycle: i64,
    pub c64_pc: Option<i64>,
    /// Mnemonic + a **generic** operand placeholder (`LDA abs`, not `LDA $D011`).
    pub c64_op: Option<&'static str>,
    pub c64_flow: Option<FlowKind>,
    /// `'r'` or `'w'`.
    pub c64_io_rw: Option<char>,
    pub c64_io_addr: Option<i64>,
    pub c64_io_value: Option<i64>,
    pub bus_atn: Option<u8>,
    pub bus_clk: Option<u8>,
    pub bus_data: Option<u8>,
    pub drv_pc: Option<i64>,
    pub drv_op: Option<&'static str>,
    pub drv_io_rw: Option<char>,
    pub drv_io_addr: Option<i64>,
    pub drv_io_value: Option<i64>,
}

/// `SwimlaneSlice`. `start_cycle` / `end_cycle` echo the **requested** window,
/// not the observed one.
#[derive(Debug, Clone, PartialEq)]
pub struct SwimlaneSlice {
    pub start_cycle: f64,
    pub end_cycle: f64,
    pub rows: Vec<SwimlaneRow>,
    pub compact: bool,
}

/// `SwimlaneQuery`.
#[derive(Debug, Clone)]
pub struct SwimlaneQuery {
    /// `None` reproduces TS `undefined` → inlined as SQL `NULL` → matches nothing.
    pub run_id: Option<String>,
    pub cycle_range: (f64, f64),
    /// TS default `true`.
    pub compact: bool,
    pub filter_c64_pc_range: Option<(f64, f64)>,
    pub focus: Option<FlowKind>,
    pub nmi_vector: Option<f64>,
}

impl Default for SwimlaneQuery {
    fn default() -> Self {
        SwimlaneQuery {
            run_id: None,
            cycle_range: (0.0, 0.0),
            compact: true,
            filter_c64_pc_range: None,
            focus: None,
            nmi_vector: None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Op entry points
// ─────────────────────────────────────────────────────────────────────────────

/// The **`swimlane`** op — structured slice (C64RE `runtime_swimlane_slice`).
///
/// Args are snake_case, mapped exactly as `sidecar.ts` maps them:
/// `run_id`, `cycle_start`, `cycle_end`, `compact?`, `focus?`, `nmi_vector?`.
/// The TS handler returns this result **without** the `jsonSafe` down-cast (all
/// values are already numbers).
///
/// NOTE ON KEY ORDER: `serde_json::Map` is ordered (the `preserve_order` feature
/// is not enabled workspace-wide), so the object keys of this `Value` come out
/// sorted rather than in the TS insertion order. Field names and values are
/// identical; for a **byte-exact** comparison against the sidecar use
/// [`slice_to_json_string`], which reproduces the TS insertion order verbatim.
pub fn op_swimlane(duckdb_path: &Path, args: &Json) -> Result<Json> {
    let query = swimlane_query_from_args(args)?;
    let slice = with_conn(duckdb_path, |conn, shape| {
        swimlane_slice(conn, shape, &query)
    })?;
    serde_json::from_str(&slice_to_json_string(&slice))
        .map_err(|e| TraceReadError::other(format!("swimlane: result is not valid JSON: {e}")))
}

/// The **`swimlane_text`** op — `{"text": …}`, the monitor's `swimlane` / `sw`
/// verb and the `chis` trace fallback.
///
/// Args: `cycle_start?`, `cycle_end?`, `last_cycles?`, `stem?`.
pub fn op_swimlane_text(duckdb_path: &Path, args: &Json) -> Result<Json> {
    let text = render_swimlane(duckdb_path, args)?;
    Ok(serde_json::json!({ "text": text }))
}

/// `swimlane_text`'s payload: `"# <stem>\n" + renderText(slice, {maxRows: 200})`.
///
/// Window resolution, in the sidecar's order:
/// 1. `run_id` ← `SELECT run_id FROM trace_run LIMIT 1` (absent ⇒ `NULL`, see
///    preserved defect 3);
/// 2. only when `cycle_start` **or** `cycle_end` is not finite:
///    `SELECT MIN(cycle), MAX(cycle) FROM trace_event WHERE cycle IS NOT NULL`,
///    `span = last_cycles ?? 2000`, `end ??= max`, `start ??= max(min, end − span)`;
/// 3. `swimlaneSlice(…, compact: true)` — `compact` is hard-coded on this path,
///    `focus` / `nmiVector` are never passed.
pub fn render_swimlane(duckdb_path: &Path, args: &Json) -> Result<String> {
    let stem = match args.get("stem") {
        None | Some(Json::Null) => "trace".to_string(),
        Some(v) => js_string(v),
    };
    let span = match args.get("last_cycles") {
        None | Some(Json::Null) => DEFAULT_SPAN,
        Some(v) => js_number(Some(v)),
    };
    let mut cs = js_number(args.get("cycle_start"));
    let mut ce = js_number(args.get("cycle_end"));

    with_conn(duckdb_path, |conn, shape| {
        let run_id = scalar_text(conn, "SELECT run_id FROM trace_run LIMIT 1")?;

        if !cs.is_finite() || !ce.is_finite() {
            let (mn, mx) = cycle_bounds(conn)?;
            if !ce.is_finite() {
                ce = mx;
            }
            if !cs.is_finite() {
                cs = js_max(mn, ce - span);
            }
        }

        let query = SwimlaneQuery {
            run_id,
            cycle_range: (cs, ce),
            compact: true,
            ..SwimlaneQuery::default()
        };
        let slice = swimlane_slice(conn, shape, &query)?;
        Ok(format!(
            "# {stem}\n{}",
            render_text(&slice, MAX_ROWS_DEFAULT, true)
        ))
    })
}

/// `sidecar.ts`'s `case "swimlane"` argument mapping.
fn swimlane_query_from_args(args: &Json) -> Result<SwimlaneQuery> {
    Ok(SwimlaneQuery {
        run_id: match args.get("run_id") {
            None | Some(Json::Null) => None,
            Some(v) => Some(js_string(v)),
        },
        cycle_range: (
            js_number(args.get("cycle_start")),
            js_number(args.get("cycle_end")),
        ),
        // `compact: a.compact as boolean` — absent/null keeps the TS default `true`.
        compact: match args.get("compact") {
            None | Some(Json::Null) => true,
            Some(v) => js_truthy_json(v),
        },
        filter_c64_pc_range: None,
        focus: match args.get("focus") {
            Some(Json::String(s)) if !s.is_empty() => FlowKind::parse(s),
            _ => None,
        },
        nmi_vector: args.get("nmi_vector").map(|v| js_number(Some(v))),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// swimlaneSlice
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct CpuStep {
    cycle: i64,
    pc: i64,
    opcode: i64,
    sp: i64,
}

#[derive(Debug, Clone, Copy)]
struct MemEvent {
    cycle: i64,
    addr: i64,
    value: i64,
}

#[derive(Debug, Clone, Copy)]
struct LineEvent {
    cycle: i64,
    atn: u8,
    clk: u8,
    data: u8,
}

/// `$D000–$DFFF` — the C64 IO region (`isC64IoAddr`).
fn is_c64_io_addr(addr: i64) -> bool {
    (0xd000..=0xdfff).contains(&addr)
}

/// The core of the `swimlane` op, against an already-open store.
///
/// Split out from [`op_swimlane`] so it can be exercised against an in-memory
/// connection (see the tests at the bottom of this file).
pub fn swimlane_slice(
    conn: &Connection,
    shape: StoreShape,
    query: &SwimlaneQuery,
) -> Result<SwimlaneSlice> {
    let (start_cycle, end_cycle) = query.cycle_range;

    let cpu_steps = query_cpu_steps(conn, shape, query)?;
    let mem_reads = query_mem(conn, shape, query, "read")?;
    let mem_writes = query_mem(conn, shape, query, "write")?;
    // One query for all three drive_*_change families (see the module header).
    let line_events = query_line_changes(conn, shape, query)?;

    // Union of every event cycle, ascending.
    let mut cycles: Vec<i64> = Vec::with_capacity(
        cpu_steps.len() + mem_reads.len() + mem_writes.len() + line_events.len(),
    );
    cycles.extend(cpu_steps.iter().map(|e| e.cycle));
    cycles.extend(mem_reads.iter().map(|e| e.cycle));
    cycles.extend(mem_writes.iter().map(|e| e.cycle));
    cycles.extend(line_events.iter().map(|e| e.cycle));
    cycles.sort_unstable();
    cycles.dedup();

    // Spec 746.13 — replay the FlowTracker classification over the ordered
    // CPU_STEP stream. TS re-sorts by cycle first; `sort_by_key` is stable, so
    // ties keep query order exactly like `Array.prototype.sort`.
    let mut flow_steps = cpu_steps.clone();
    flow_steps.sort_by_key(|s| s.cycle);
    let flow_by_cycle = derive_flow(&flow_steps, query.nmi_vector);

    // First event per cycle for the CPU + IEC lanes; every event per cycle for
    // the memory lanes (TS scans for the first IO-space hit).
    let cpu_first = first_by_cycle(&cpu_steps, |e| e.cycle);
    let line_first = first_by_cycle(&line_events, |e| e.cycle);
    let read_idx = group_by_cycle(&mem_reads, |e| e.cycle);
    let write_idx = group_by_cycle(&mem_writes, |e| e.cycle);

    let mut last_bus_atn: Option<u8> = None;
    let mut last_bus_clk: Option<u8> = None;
    let mut last_bus_data: Option<u8> = None;

    let mut rows: Vec<SwimlaneRow> = Vec::with_capacity(cycles.len());
    for cycle in cycles {
        let mut row = SwimlaneRow {
            cycle,
            ..SwimlaneRow::default()
        };

        if let Some(&i) = cpu_first.get(&cycle) {
            let ev = &cpu_steps[i];
            row.c64_pc = Some(ev.pc);
            row.c64_op = Some(opcode_to_mnemonic(ev.opcode));
            // Every cpu_step cycle has a flow entry; `if (fl)` is always true
            // because "main"/"irq"/"nmi" are all truthy strings.
            row.c64_flow = flow_by_cycle.get(&cycle).copied();
        }

        // First IO-space read of the cycle …
        if let Some(list) = read_idx.get(&cycle) {
            for &i in list {
                let ev = &mem_reads[i];
                if is_c64_io_addr(ev.addr) {
                    row.c64_io_rw = Some('r');
                    row.c64_io_addr = Some(ev.addr);
                    row.c64_io_value = Some(ev.value);
                    break;
                }
            }
        }
        // … then the first IO-space write, which takes precedence.
        if let Some(list) = write_idx.get(&cycle) {
            for &i in list {
                let ev = &mem_writes[i];
                if is_c64_io_addr(ev.addr) {
                    row.c64_io_rw = Some('w');
                    row.c64_io_addr = Some(ev.addr);
                    row.c64_io_value = Some(ev.value);
                    break;
                }
            }
        }

        // IEC lines carry forward until the next change event.
        if let Some(&i) = line_first.get(&cycle) {
            let ev = &line_events[i];
            last_bus_atn = Some(ev.atn);
            last_bus_clk = Some(ev.clk);
            last_bus_data = Some(ev.data);
        }
        row.bus_atn = last_bus_atn;
        row.bus_clk = last_bus_clk;
        row.bus_data = last_bus_data;

        rows.push(row);
    }

    // Focus filter (the c64Flow column stays populated regardless).
    if let Some(focus) = query.focus {
        rows.retain(|r| r.c64_flow == Some(focus));
    }

    // Compact: drop rows where nothing changed vs. the previous EMITTED row.
    let rows = if query.compact {
        let mut out: Vec<SwimlaneRow> = Vec::with_capacity(rows.len());
        for row in rows {
            if out.last().map(|prev| row_changed(prev, &row)).unwrap_or(true) {
                out.push(row);
            }
        }
        out
    } else {
        rows
    };

    Ok(SwimlaneSlice {
        start_cycle,
        end_cycle,
        rows,
        compact: query.compact,
    })
}

/// `rowChanged` — all 14 non-cycle fields.
fn row_changed(prev: &SwimlaneRow, cur: &SwimlaneRow) -> bool {
    cur.c64_pc != prev.c64_pc
        || cur.c64_op != prev.c64_op
        || cur.c64_flow != prev.c64_flow
        || cur.c64_io_rw != prev.c64_io_rw
        || cur.c64_io_addr != prev.c64_io_addr
        || cur.c64_io_value != prev.c64_io_value
        || cur.bus_atn != prev.bus_atn
        || cur.bus_clk != prev.bus_clk
        || cur.bus_data != prev.bus_data
        || cur.drv_pc != prev.drv_pc
        || cur.drv_op != prev.drv_op
        || cur.drv_io_rw != prev.drv_io_rw
        || cur.drv_io_addr != prev.drv_io_addr
        || cur.drv_io_value != prev.drv_io_value
}

fn first_by_cycle<T, F: Fn(&T) -> i64>(items: &[T], cycle: F) -> HashMap<i64, usize> {
    let mut m: HashMap<i64, usize> = HashMap::with_capacity(items.len());
    for (i, it) in items.iter().enumerate() {
        m.entry(cycle(it)).or_insert(i);
    }
    m
}

fn group_by_cycle<T, F: Fn(&T) -> i64>(items: &[T], cycle: F) -> HashMap<i64, Vec<usize>> {
    let mut m: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        m.entry(cycle(it)).or_default().push(i);
    }
    m
}

// ─────────────────────────────────────────────────────────────────────────────
// deriveFlow (flow-focus.ts)
// ─────────────────────────────────────────────────────────────────────────────

const OP_BRK: i64 = 0x00;
const OP_JSR: i64 = 0x20;
const OP_PLP: i64 = 0x28;
const OP_RTI: i64 = 0x40;
const OP_PHA: i64 = 0x48;
const OP_RTS: i64 = 0x60;
const OP_PLA: i64 = 0x68;
const OP_PHP: i64 = 0x08;
const OP_TXS: i64 = 0x9a;

/// SP delta (post − pre) caused by an instruction's own execution.
fn stack_effect(op: i64) -> i32 {
    match op {
        OP_PHA | OP_PHP => -1,
        OP_PLA | OP_PLP => 1,
        OP_JSR => -2,
        OP_RTS => 2,
        OP_RTI => 3,
        OP_BRK => -3,
        _ => 0,
    }
}

/// Replay the interrupt-frame classification over an ordered CPU_STEP stream.
///
/// Steps at the same cycle overwrite each other — the map keeps the **last**
/// one, exactly like `Map.prototype.set`.
fn derive_flow(steps: &[CpuStep], nmi_vector: Option<f64>) -> HashMap<i64, FlowKind> {
    let mut flow_by_cycle: HashMap<i64, FlowKind> = HashMap::with_capacity(steps.len());
    let mut stack: Vec<FlowKind> = Vec::new();
    let nmi_pc: Option<i32> = nmi_vector
        .filter(|v| v.is_finite())
        .map(|v| (v as i64 as i32) & 0xffff);

    let mut prev_sp: Option<i32> = None;
    for s in steps {
        let op = s.opcode & 0xff;
        let sp = (s.sp & 0xff) as i32;

        // 1. A hardware IRQ/NMI was dispatched before this (first-handler)
        //    instruction. TXS writes SP arbitrarily → its delta is meaningless.
        if let Some(prev) = prev_sp {
            if op != OP_TXS && op != OP_BRK {
                let delta = (prev + stack_effect(op) - sp) & 0xff;
                if delta == 3 {
                    let current = stack.last().copied().unwrap_or(FlowKind::Main);
                    let is_nmi = nmi_pc.map(|v| ((s.pc as i32) & 0xffff) == v).unwrap_or(false)
                        || current != FlowKind::Main;
                    stack.push(if is_nmi { FlowKind::Nmi } else { FlowKind::Irq });
                }
            }
        }

        // 2. BRK = software interrupt entry (folds into irq, 3-lane model).
        if op == OP_BRK {
            stack.push(FlowKind::Irq);
        }

        // 3. This instruction runs in the current (post-entry) flow.
        flow_by_cycle.insert(s.cycle, stack.last().copied().unwrap_or(FlowKind::Main));

        // 4. RTI pops AFTER recording — the RTI itself runs in the handler flow.
        if op == OP_RTI && !stack.is_empty() {
            stack.pop();
        }

        prev_sp = Some(sp);
    }

    flow_by_cycle
}

// ─────────────────────────────────────────────────────────────────────────────
// queryEvents (the subset swimlaneSlice needs)
// ─────────────────────────────────────────────────────────────────────────────

fn instructions_source(shape: StoreShape) -> String {
    match shape {
        StoreShape::Spec726 => format!("({INSTRUCTIONS_726})"),
        StoreShape::Legacy217 => LEGACY_INSTRUCTIONS.to_string(),
    }
}

fn bus_events_source(shape: StoreShape) -> String {
    match shape {
        StoreShape::Spec726 => format!("({BUS_EVENTS_726})"),
        StoreShape::Legacy217 => LEGACY_BUS_EVENTS.to_string(),
    }
}

/// `duckdb-backend.ts` `inlineParam` for a string.
fn inline_text(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `duckdb-backend.ts` `inlineParam` for a number — non-finite is a hard error
/// (`non-finite param: NaN`), exactly as in TS.
fn inline_number(v: f64) -> Result<String> {
    if !v.is_finite() {
        return Err(TraceReadError::other(format!(
            "non-finite param: {}",
            fmt_js_number(v)
        )));
    }
    Ok(fmt_js_number(v))
}

/// `run_id = …` — `None` inlines as the SQL literal `NULL` (never true).
fn run_id_predicate(run_id: Option<&String>) -> String {
    match run_id {
        Some(id) => format!("run_id = {}", inline_text(id)),
        None => "run_id = NULL".to_string(),
    }
}

fn build_event_sql(
    source: &str,
    run_id: Option<&String>,
    kind: Option<&str>,
    cycle_range: (f64, f64),
    pc_range: Option<(f64, f64)>,
) -> Result<String> {
    let mut wheres = vec![run_id_predicate(run_id)];
    if let Some(k) = kind {
        wheres.push(format!("kind = {}", inline_text(k)));
    }
    wheres.push(format!(
        "clock BETWEEN {} AND {}",
        inline_number(cycle_range.0)?,
        inline_number(cycle_range.1)?
    ));
    if let Some((lo, hi)) = pc_range {
        wheres.push(format!(
            "pc BETWEEN {} AND {}",
            inline_number(lo)?,
            inline_number(hi)?
        ));
    }
    Ok(format!(
        "SELECT * FROM {source} WHERE {} ORDER BY clock LIMIT {EVENT_LIMIT}",
        wheres.join(" AND ")
    ))
}

fn query_cpu_steps(
    conn: &Connection,
    shape: StoreShape,
    query: &SwimlaneQuery,
) -> Result<Vec<CpuStep>> {
    let sql = build_event_sql(
        &instructions_source(shape),
        query.run_id.as_ref(),
        None,
        query.cycle_range,
        query.filter_c64_pc_range,
    )?;
    let table = select_rows(conn, &sql)?;
    Ok(table
        .iter()
        .map(|r| CpuStep {
            cycle: r.num("clock"),
            pc: r.num("pc"),
            opcode: r.num("opcode"),
            sp: r.num("sp"),
        })
        .collect())
}

fn query_mem(
    conn: &Connection,
    shape: StoreShape,
    query: &SwimlaneQuery,
    kind: &str,
) -> Result<Vec<MemEvent>> {
    let sql = build_event_sql(
        &bus_events_source(shape),
        query.run_id.as_ref(),
        Some(kind),
        query.cycle_range,
        None,
    )?;
    let table = select_rows(conn, &sql)?;
    Ok(table
        .iter()
        .map(|r| MemEvent {
            cycle: r.num("clock"),
            addr: r.num("addr"),
            value: r.num("value"),
        })
        .collect())
}

fn query_line_changes(
    conn: &Connection,
    shape: StoreShape,
    query: &SwimlaneQuery,
) -> Result<Vec<LineEvent>> {
    let sql = build_event_sql(
        &bus_events_source(shape),
        query.run_id.as_ref(),
        Some("line_change"),
        query.cycle_range,
        None,
    )?;
    let table = select_rows(conn, &sql)?;
    Ok(table
        .iter()
        .map(|r| LineEvent {
            cycle: r.num("clock"),
            atn: r.level("line_atn"),
            clk: r.level("line_clk"),
            data: r.level("line_data"),
        })
        .collect())
}

// ─────────────────────────────────────────────────────────────────────────────
// DuckDB row access
// ─────────────────────────────────────────────────────────────────────────────

/// A result set with its column names, so columns are read by NAME. The two
/// store shapes project the same names at *different ordinals* (`bus_events`
/// has an `old_value` column that `BUS_EVENTS_726` does not), and the TS reader
/// used `SELECT *` + name lookup — this keeps the SQL byte-identical to TS
/// while staying shape-agnostic.
struct RowTable {
    names: Vec<String>,
    rows: Vec<Vec<DuckValue>>,
}

struct RowRef<'a> {
    names: &'a [String],
    values: &'a [DuckValue],
}

impl RowTable {
    fn iter(&self) -> impl Iterator<Item = RowRef<'_>> {
        let names = &self.names;
        self.rows.iter().map(move |values| RowRef { names, values })
    }
}

impl RowRef<'_> {
    fn get(&self, name: &str) -> Option<&DuckValue> {
        self.names
            .iter()
            .position(|n| n == name)
            .and_then(|i| self.values.get(i))
    }
    /// `Number(r.<name> ?? 0)` — SQL NULL and a missing column both give 0.
    fn num(&self, name: &str) -> i64 {
        duck_number(self.get(name))
    }
    /// `r.<name> ? 1 : 0` — JS truthiness over a BOOLEAN / NULL column.
    fn level(&self, name: &str) -> u8 {
        u8::from(duck_truthy(self.get(name)))
    }
}

/// A failed query surfaces the **raw** DuckDB message, with no Rust-side
/// context prefix — the TS handler let the driver error propagate into
/// `{"error": e.message}`, so a prefix here would be a parity difference. It
/// matters in practice for exactly one case: `swimlane_text` against a legacy
/// Shape-A store, where `SELECT run_id FROM trace_run` hits a Catalog Error in
/// both implementations (the verb has never worked on those stores).
///
/// Spec 802 F1 made this the rule for EVERY read path rather than a local
/// workaround: `TraceReadError::duck(what, e)` now renders the bare source too
/// (`what` survives only in `Debug`/`context()`). New code should use that —
/// it keeps the typed `source`. This helper is left as-is because its output is
/// already byte-identical and `swimlane` passes parity.
fn duck_raw(e: duckdb::Error) -> TraceReadError {
    TraceReadError::other(e.to_string())
}

fn select_rows(conn: &Connection, sql: &str) -> Result<RowTable> {
    let mut stmt = conn.prepare(sql).map_err(duck_raw)?;
    let mut rows = stmt.query([]).map_err(duck_raw)?;
    let names: Vec<String> = rows
        .as_ref()
        .map(|s| s.column_names())
        .unwrap_or_default();
    let ncols = names.len();
    let mut out: Vec<Vec<DuckValue>> = Vec::new();
    while let Some(row) = rows.next().map_err(duck_raw)? {
        let mut vals = Vec::with_capacity(ncols);
        for i in 0..ncols {
            vals.push(row.get::<_, DuckValue>(i).map_err(duck_raw)?);
        }
        out.push(vals);
    }
    Ok(RowTable { names, rows: out })
}

fn duck_number(v: Option<&DuckValue>) -> i64 {
    match v {
        None | Some(DuckValue::Null) => 0,
        Some(DuckValue::Boolean(b)) => i64::from(*b),
        Some(DuckValue::TinyInt(n)) => *n as i64,
        Some(DuckValue::SmallInt(n)) => *n as i64,
        Some(DuckValue::Int(n)) => *n as i64,
        Some(DuckValue::BigInt(n)) => *n,
        Some(DuckValue::UTinyInt(n)) => *n as i64,
        Some(DuckValue::USmallInt(n)) => *n as i64,
        Some(DuckValue::UInt(n)) => *n as i64,
        Some(DuckValue::UBigInt(n)) => *n as i64,
        Some(DuckValue::HugeInt(n)) => *n as i64,
        Some(DuckValue::Float(f)) => *f as i64,
        Some(DuckValue::Double(f)) => *f as i64,
        Some(DuckValue::Text(s)) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

fn duck_truthy(v: Option<&DuckValue>) -> bool {
    match v {
        None | Some(DuckValue::Null) => false,
        Some(DuckValue::Boolean(b)) => *b,
        Some(DuckValue::Text(s)) => !s.is_empty(),
        other => duck_number(other) != 0,
    }
}

/// `SELECT MIN(cycle), MAX(cycle) FROM trace_event WHERE cycle IS NOT NULL`,
/// with the TS `Number(rg?.[i] ?? 0)` fallback for an empty table.
fn cycle_bounds(conn: &Connection) -> Result<(f64, f64)> {
    let table = select_rows(
        conn,
        "SELECT MIN(cycle), MAX(cycle) FROM trace_event WHERE cycle IS NOT NULL",
    )?;
    let Some(row) = table.rows.first() else {
        return Ok((0.0, 0.0));
    };
    Ok((
        duck_number(row.first()) as f64,
        duck_number(row.get(1)) as f64,
    ))
}

fn scalar_text(conn: &Connection, sql: &str) -> Result<Option<String>> {
    let table = select_rows(conn, sql)?;
    Ok(table.rows.first().and_then(|r| r.first()).and_then(|v| {
        match v {
            DuckValue::Null => None,
            DuckValue::Text(s) => Some(s.clone()),
            // `String(rid)` for a non-TEXT run_id column.
            other => Some(duck_number(Some(other)).to_string()),
        }
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// renderText (swimlane-render.ts)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Cells {
    cycle: String,
    c64: String,
    flow: String,
    io: String,
    bus: String,
    drv: String,
    dio: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Col {
    Cycle,
    C64,
    Flow,
    Io,
    Bus,
    Drv,
    Dio,
}

impl Col {
    fn head(self) -> &'static str {
        match self {
            Col::Cycle => "cycle",
            Col::C64 => "c64",
            Col::Flow => "flow",
            Col::Io => "io",
            Col::Bus => "iec",
            Col::Drv => "1541",
            Col::Dio => "drv_io",
        }
    }
}

impl Cells {
    fn get(&self, col: Col) -> &str {
        match col {
            Col::Cycle => &self.cycle,
            Col::C64 => &self.c64,
            Col::Flow => &self.flow,
            Col::Io => &self.io,
            Col::Bus => &self.bus,
            Col::Drv => &self.drv,
            Col::Dio => &self.dio,
        }
    }
}

/// `hex(v, 4)` — `$` + uppercase, zero-padded to 4.
fn hex4(v: i64) -> String {
    let s = format!("{:X}", v);
    if s.len() >= 4 {
        format!("${s}")
    } else {
        format!("${:0>4}", s)
    }
}

/// `fmtIo` — `"$ADDR r=VV"` / `"$ADDR w=VV"`, `""` when the lane is idle.
fn fmt_io(rw: Option<char>, addr: Option<i64>, value: Option<i64>) -> String {
    match (rw, addr) {
        (Some(rw), Some(addr)) => {
            let val = match value {
                Some(v) => {
                    let s = format!("{:X}", v);
                    if s.len() >= 2 {
                        s
                    } else {
                        format!("{:0>2}", s)
                    }
                }
                None => "??".to_string(),
            };
            format!("{} {}={}", hex4(addr), rw, val)
        }
        _ => String::new(),
    }
}

/// `fmtBus` — `"A<a>C<c>D<d>"` with `-` per undefined line, `""` when all three
/// are undefined.
fn fmt_bus(atn: Option<u8>, clk: Option<u8>, data: Option<u8>) -> String {
    if atn.is_none() && clk.is_none() && data.is_none() {
        return String::new();
    }
    let f = |v: Option<u8>| match v {
        Some(n) => n.to_string(),
        None => "-".to_string(),
    };
    format!("A{}C{}D{}", f(atn), f(clk), f(data))
}

/// Merge one body-row position across its fold iterations.
///
/// `[...new Set(vals.filter(v => v !== ""))]` keeps first-appearance order.
fn summarize_cell(vals: &[&str]) -> String {
    let mut distinct: Vec<&str> = Vec::new();
    for v in vals {
        if !v.is_empty() && !distinct.contains(v) {
            distinct.push(v);
        }
    }
    if distinct.len() <= 1 {
        return distinct.first().map(|s| (*s).to_string()).unwrap_or_default();
    }
    // `^(\$[0-9A-F]+ [rw]=)([0-9A-F]+)$` on EVERY member, one shared prefix.
    let matched: Option<Vec<(&str, &str)>> = distinct.iter().map(|s| match_io_cell(s)).collect();
    if let Some(parts) = matched {
        let prefix = parts[0].0;
        if parts.iter().all(|(p, _)| *p == prefix) {
            let vs: Vec<u64> = parts
                .iter()
                .map(|(_, v)| u64::from_str_radix(v, 16).unwrap_or(0))
                .collect();
            let lo = pad2_hex(*vs.iter().min().unwrap());
            let hi = pad2_hex(*vs.iter().max().unwrap());
            return format!("{prefix}{lo}..{hi}");
        }
    }
    format!("{} \u{2026}", distinct[0])
}

fn pad2_hex(v: u64) -> String {
    let s = format!("{:X}", v);
    if s.len() >= 2 {
        s
    } else {
        format!("{:0>2}", s)
    }
}

/// Hand-rolled `^(\$[0-9A-F]+ [rw]=)([0-9A-F]+)$`; returns `(prefix, value)`.
fn match_io_cell(s: &str) -> Option<(&str, &str)> {
    let b = s.as_bytes();
    let is_hex = |c: u8| c.is_ascii_digit() || (b'A'..=b'F').contains(&c);
    if b.first() != Some(&b'$') {
        return None;
    }
    let mut i = 1;
    while i < b.len() && is_hex(b[i]) {
        i += 1;
    }
    if i == 1 || i + 2 >= b.len() {
        return None;
    }
    if b[i] != b' ' || (b[i + 1] != b'r' && b[i + 1] != b'w') || b[i + 2] != b'=' {
        return None;
    }
    let vstart = i + 3;
    let mut j = vstart;
    while j < b.len() && is_hex(b[j]) {
        j += 1;
    }
    if j == vstart || j != b.len() {
        return None;
    }
    Some((&s[..vstart], &s[vstart..]))
}

enum Item {
    Row(Cells),
    Group { reps: usize, body: Vec<Cells> },
}

/// `foldCells` — collapse consecutive loop iterations, keyed on the SHAPE
/// (`flow` + `c64` + `drv`) only, so IO/bus variation is summarised rather than
/// swallowed and an IRQ block breaks the fold automatically.
fn fold_cells(all: &[Cells], max_period: usize) -> Vec<Item> {
    let keys: Vec<String> = all
        .iter()
        .map(|c| format!("{}{}{}", c.flow, c.c64, c.drv))
        .collect();
    let mut out: Vec<Item> = Vec::new();
    let mut i = 0usize;
    while i < all.len() {
        let mut found: Option<(usize, usize)> = None;
        let max_l = max_period.min((all.len() - i) / 2);
        for l in 1..=max_l {
            let mut reps = 1usize;
            loop {
                let mut matches = true;
                for k in 0..l {
                    if i + reps * l + k >= all.len() || keys[i + k] != keys[i + reps * l + k] {
                        matches = false;
                        break;
                    }
                }
                if !matches {
                    break;
                }
                reps += 1;
            }
            if reps >= 2 && l * reps >= 3 {
                found = Some((l, reps));
                break; // smallest period wins
            }
        }
        match found {
            Some((l, reps)) => {
                let mut body: Vec<Cells> = Vec::with_capacity(l);
                for k in 0..l {
                    let variants: Vec<&Cells> = (0..reps).map(|r| &all[i + r * l + k]).collect();
                    let ios: Vec<&str> = variants.iter().map(|v| v.io.as_str()).collect();
                    let buses: Vec<&str> = variants.iter().map(|v| v.bus.as_str()).collect();
                    let dios: Vec<&str> = variants.iter().map(|v| v.dio.as_str()).collect();
                    let mut cell = variants[0].clone();
                    cell.io = summarize_cell(&ios);
                    cell.bus = summarize_cell(&buses);
                    cell.dio = summarize_cell(&dios);
                    body.push(cell);
                }
                out.push(Item::Group { reps, body });
                i += l * reps;
            }
            None => {
                out.push(Item::Row(all[i].clone()));
                i += 1;
            }
        }
    }
    out
}

/// One rendered line: cells `padEnd`-ed to their column width and joined with
/// TWO spaces, an optional fold tag after two more, then `trimEnd()`.
fn fmt_row(widths: &[usize], vals: &[&str], tag: Option<&str>) -> String {
    let mut line = String::new();
    for (n, (cell, &w)) in vals.iter().zip(widths.iter()).enumerate() {
        if n > 0 {
            line.push_str("  ");
        }
        line.push_str(cell);
        for _ in cell.chars().count()..w {
            line.push(' ');
        }
    }
    if let Some(t) = tag {
        line.push_str("  ");
        line.push_str(t);
    }
    line.trim_end().to_string()
}

/// `renderText` — the plain-text (TUI) renderer, byte-for-byte.
pub fn render_text(slice: &SwimlaneSlice, max_rows: usize, fold: bool) -> String {
    let all: Vec<Cells> = slice
        .rows
        .iter()
        .map(|row| Cells {
            cycle: row.cycle.to_string(),
            c64: format!(
                "{}{}",
                row.c64_pc.map(hex4).unwrap_or_default(),
                match row.c64_op {
                    Some(op) if !op.is_empty() => format!(" {op}"),
                    _ => String::new(),
                }
            ),
            flow: row.c64_flow.map(|f| f.as_str().to_string()).unwrap_or_default(),
            io: fmt_io(row.c64_io_rw, row.c64_io_addr, row.c64_io_value),
            bus: fmt_bus(row.bus_atn, row.bus_clk, row.bus_data),
            drv: format!(
                "{}{}",
                row.drv_pc.map(hex4).unwrap_or_default(),
                match row.drv_op {
                    Some(op) if !op.is_empty() => format!(" {op}"),
                    _ => String::new(),
                }
            ),
            dio: fmt_io(row.drv_io_rw, row.drv_io_addr, row.drv_io_value),
        })
        // drop empty filler rows
        .filter(|c| {
            !c.c64.is_empty()
                || !c.io.is_empty()
                || !c.bus.is_empty()
                || !c.drv.is_empty()
                || !c.dio.is_empty()
        })
        .collect();

    let items: Vec<Item> = if fold {
        fold_cells(&all, MAX_FOLD_PERIOD)
    } else {
        all.iter().cloned().map(Item::Row).collect()
    };

    // Expand to render-rows; a fold group's FIRST body row carries the `↺×N` tag.
    let mut rr: Vec<(Cells, Option<String>)> = Vec::new();
    for it in items {
        match it {
            Item::Row(c) => rr.push((c, None)),
            Item::Group { reps, body } => {
                for (idx, c) in body.into_iter().enumerate() {
                    let tag = if idx == 0 {
                        Some(format!("\u{21ba}\u{00d7}{reps}"))
                    } else {
                        None
                    };
                    rr.push((c, tag));
                }
            }
        }
    }

    let shown = &rr[..rr.len().min(max_rows)];
    let truncated = rr.len() > max_rows;
    if shown.is_empty() {
        return format!(
            "swimlane {}\u{2013}{}: (no events in window)",
            fmt_js_number(slice.start_cycle),
            fmt_js_number(slice.end_cycle)
        );
    }

    // A lane is shown ONLY if it carries data; `main` does not count as flow data
    // (TS: `r.c[k] !== "" && !(k === "flow" && r.c[k] === "main")`).
    let has = |k: Col| {
        shown.iter().any(|(c, _)| {
            let cell = c.get(k);
            if k == Col::Flow && cell == "main" {
                return false;
            }
            !cell.is_empty()
        })
    };
    let mut cols: Vec<Col> = vec![Col::Cycle, Col::C64];
    for k in [Col::Flow, Col::Io, Col::Bus, Col::Drv, Col::Dio] {
        if has(k) {
            cols.push(k);
        }
    }

    let widths: Vec<usize> = cols
        .iter()
        .map(|&k| {
            shown
                .iter()
                .map(|(c, _)| c.get(k).chars().count())
                .chain([k.head().chars().count(), 1])
                .max()
                .unwrap_or(1)
        })
        .collect();

    let mut lines: Vec<String> = Vec::with_capacity(shown.len() + 3);
    lines.push(format!(
        "swimlane {}\u{2013}{}{}  {} rows ({} raw)",
        fmt_js_number(slice.start_cycle),
        fmt_js_number(slice.end_cycle),
        if slice.compact { " (compact)" } else { "" },
        shown.len(),
        all.len()
    ));
    let heads: Vec<&str> = cols.iter().map(|k| k.head()).collect();
    lines.push(fmt_row(&widths, &heads, None));
    for (c, tag) in shown {
        let vals: Vec<&str> = cols.iter().map(|&k| c.get(k)).collect();
        lines.push(fmt_row(&widths, &vals, tag.as_deref()));
    }
    if truncated {
        lines.push(format!(
            "\u{2026} {} more rows \u{2014} narrow with `swimlane <s> <e>`",
            rr.len() - max_rows
        ));
    }
    lines.join("\n")
}

// ─────────────────────────────────────────────────────────────────────────────
// JSON serialisation (TS insertion order)
// ─────────────────────────────────────────────────────────────────────────────

/// The `swimlane` op result, serialised with the **exact** key order the TS
/// handler's object literals produce — `{startCycle, endCycle, rows, compact}`
/// and, per row, `{cycle, c64Pc, c64Op, c64Flow, c64IoRw, c64IoAddr, c64IoValue,
/// busAtn, busClk, busData}`. Absent fields are OMITTED, never `null`.
///
/// This is the byte-parity artifact; [`op_swimlane`] returns the same content as
/// a `serde_json::Value` for the daemon.
pub fn slice_to_json_string(slice: &SwimlaneSlice) -> String {
    let mut out = String::with_capacity(64 + slice.rows.len() * 96);
    out.push_str("{\"startCycle\":");
    out.push_str(&fmt_js_number(slice.start_cycle));
    out.push_str(",\"endCycle\":");
    out.push_str(&fmt_js_number(slice.end_cycle));
    out.push_str(",\"rows\":[");
    for (i, r) in slice.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"cycle\":");
        out.push_str(&r.cycle.to_string());
        if let Some(pc) = r.c64_pc {
            out.push_str(",\"c64Pc\":");
            out.push_str(&pc.to_string());
        }
        if let Some(op) = r.c64_op {
            out.push_str(",\"c64Op\":");
            out.push_str(&json_quote(op));
        }
        if let Some(fl) = r.c64_flow {
            out.push_str(",\"c64Flow\":\"");
            out.push_str(fl.as_str());
            out.push('"');
        }
        if let Some(rw) = r.c64_io_rw {
            out.push_str(",\"c64IoRw\":\"");
            out.push(rw);
            out.push('"');
        }
        if let Some(a) = r.c64_io_addr {
            out.push_str(",\"c64IoAddr\":");
            out.push_str(&a.to_string());
        }
        if let Some(v) = r.c64_io_value {
            out.push_str(",\"c64IoValue\":");
            out.push_str(&v.to_string());
        }
        if let Some(v) = r.bus_atn {
            out.push_str(",\"busAtn\":");
            out.push_str(&v.to_string());
        }
        if let Some(v) = r.bus_clk {
            out.push_str(",\"busClk\":");
            out.push_str(&v.to_string());
        }
        if let Some(v) = r.bus_data {
            out.push_str(",\"busData\":");
            out.push_str(&v.to_string());
        }
        // drv_* are never populated (preserved defect 1) but are emitted for
        // completeness should that ever change.
        if let Some(v) = r.drv_pc {
            out.push_str(",\"drvPc\":");
            out.push_str(&v.to_string());
        }
        if let Some(op) = r.drv_op {
            out.push_str(",\"drvOp\":");
            out.push_str(&json_quote(op));
        }
        if let Some(rw) = r.drv_io_rw {
            out.push_str(",\"drvIoRw\":\"");
            out.push(rw);
            out.push('"');
        }
        if let Some(a) = r.drv_io_addr {
            out.push_str(",\"drvIoAddr\":");
            out.push_str(&a.to_string());
        }
        if let Some(v) = r.drv_io_value {
            out.push_str(",\"drvIoValue\":");
            out.push_str(&v.to_string());
        }
        out.push('}');
    }
    out.push_str("],\"compact\":");
    out.push_str(if slice.compact { "true" } else { "false" });
    out.push('}');
    out
}

fn json_quote(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

// ─────────────────────────────────────────────────────────────────────────────
// JS coercion helpers
// ─────────────────────────────────────────────────────────────────────────────

/// `String(n)` for a JS number: integral values carry no decimal point.
fn fmt_js_number(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if v == v.trunc() && v.abs() < 9.0e18 {
        return (v as i64).to_string();
    }
    let s = format!("{v}");
    s
}

/// `Number(x)`: absent ⇒ `NaN`, `null` ⇒ `0`, `""` ⇒ `0`, garbage ⇒ `NaN`.
fn js_number(v: Option<&Json>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Json::Null) => 0.0,
        Some(Json::Bool(b)) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Some(Json::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Json::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                0.0
            } else {
                t.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        Some(_) => f64::NAN,
    }
}

/// `Math.max` — NaN in either operand poisons the result (Rust's `f64::max`
/// does the opposite and would silently repair a bad `last_cycles`).
fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a > b {
        a
    } else {
        b
    }
}

/// `String(x)` for the values the ops actually carry.
fn js_string(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        Json::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn js_truthy_json(v: &Json) -> bool {
    match v {
        Json::Null => false,
        Json::Bool(b) => *b,
        Json::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Json::String(s) => !s.is_empty(),
        _ => true,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// opcode → mnemonic
// ─────────────────────────────────────────────────────────────────────────────

/// `opcodeToMnemonic` — **generic operand placeholders, not decoded operands**.
///
/// Precomputed from `generated-opcodes.ts` `OPCODE_TABLE` (documented ISA;
/// `imp`/`acc` render bare, otherwise `MNE` + a mode placeholder) with the
/// `undoc-table.ts` `UNDOC_TABLE` fallback, which contributes the bare
/// UPPERCASED `kind` and **no** operand suffix — which is why `$EB` renders as
/// `SBC_IMM`. An opcode in neither table renders as `$XX`.
pub const MNEMONICS: [&str; 256] = [
    "BRK", "ORA (zp,X)", "$02", "SLO", // $00
    "NOP", "ORA zp", "ASL zp", "SLO", // $04
    "PHP", "ORA #imm", "ASL", "ANC", // $08
    "NOP", "ORA abs", "ASL abs", "SLO", // $0C
    "BPL rel", "ORA (zp),Y", "$12", "SLO", // $10
    "NOP", "ORA zp,X", "ASL zp,X", "SLO", // $14
    "CLC", "ORA abs,Y", "NOP", "SLO", // $18
    "NOP", "ORA abs,X", "ASL abs,X", "SLO", // $1C
    "JSR abs", "AND (zp,X)", "$22", "RLA", // $20
    "BIT zp", "AND zp", "ROL zp", "RLA", // $24
    "PLP", "AND #imm", "ROL", "ANC", // $28
    "BIT abs", "AND abs", "ROL abs", "RLA", // $2C
    "BMI rel", "AND (zp),Y", "$32", "RLA", // $30
    "NOP", "AND zp,X", "ROL zp,X", "RLA", // $34
    "SEC", "AND abs,Y", "NOP", "RLA", // $38
    "NOP", "AND abs,X", "ROL abs,X", "RLA", // $3C
    "RTI", "EOR (zp,X)", "$42", "SRE", // $40
    "NOP", "EOR zp", "LSR zp", "SRE", // $44
    "PHA", "EOR #imm", "LSR", "ALR", // $48
    "JMP abs", "EOR abs", "LSR abs", "SRE", // $4C
    "BVC rel", "EOR (zp),Y", "$52", "SRE", // $50
    "NOP", "EOR zp,X", "LSR zp,X", "SRE", // $54
    "CLI", "EOR abs,Y", "NOP", "SRE", // $58
    "NOP", "EOR abs,X", "LSR abs,X", "SRE", // $5C
    "RTS", "ADC (zp,X)", "$62", "RRA", // $60
    "NOP", "ADC zp", "ROR zp", "RRA", // $64
    "PLA", "ADC #imm", "ROR", "ARR", // $68
    "JMP (abs)", "ADC abs", "ROR abs", "RRA", // $6C
    "BVS rel", "ADC (zp),Y", "$72", "RRA", // $70
    "NOP", "ADC zp,X", "ROR zp,X", "RRA", // $74
    "SEI", "ADC abs,Y", "NOP", "RRA", // $78
    "NOP", "ADC abs,X", "ROR abs,X", "RRA", // $7C
    "NOP", "STA (zp,X)", "NOP", "SAX", // $80
    "STY zp", "STA zp", "STX zp", "SAX", // $84
    "DEY", "NOP", "TXA", "XAA", // $88
    "STY abs", "STA abs", "STX abs", "SAX", // $8C
    "BCC rel", "STA (zp),Y", "$92", "AHX", // $90
    "STY zp,X", "STA zp,X", "STX zp,Y", "SAX", // $94
    "TYA", "STA abs,Y", "TXS", "TAS", // $98
    "SHY", "STA abs,X", "SHX", "AHX", // $9C
    "LDY #imm", "LDA (zp,X)", "LDX #imm", "LAX", // $A0
    "LDY zp", "LDA zp", "LDX zp", "LAX", // $A4
    "TAY", "LDA #imm", "TAX", "LAX", // $A8
    "LDY abs", "LDA abs", "LDX abs", "LAX", // $AC
    "BCS rel", "LDA (zp),Y", "$B2", "LAX", // $B0
    "LDY zp,X", "LDA zp,X", "LDX zp,Y", "LAX", // $B4
    "CLV", "LDA abs,Y", "TSX", "LAS", // $B8
    "LDY abs,X", "LDA abs,X", "LDX abs,Y", "LAX", // $BC
    "CPY #imm", "CMP (zp,X)", "NOP", "DCP", // $C0
    "CPY zp", "CMP zp", "DEC zp", "DCP", // $C4
    "INY", "CMP #imm", "DEX", "AXS", // $C8
    "CPY abs", "CMP abs", "DEC abs", "DCP", // $CC
    "BNE rel", "CMP (zp),Y", "$D2", "DCP", // $D0
    "NOP", "CMP zp,X", "DEC zp,X", "DCP", // $D4
    "CLD", "CMP abs,Y", "NOP", "DCP", // $D8
    "NOP", "CMP abs,X", "DEC abs,X", "DCP", // $DC
    "CPX #imm", "SBC (zp,X)", "NOP", "ISB", // $E0
    "CPX zp", "SBC zp", "INC zp", "ISB", // $E4
    "INX", "SBC #imm", "NOP", "SBC_IMM", // $E8
    "CPX abs", "SBC abs", "INC abs", "ISB", // $EC
    "BEQ rel", "SBC (zp),Y", "$F2", "ISB", // $F0
    "NOP", "SBC zp,X", "INC zp,X", "ISB", // $F4
    "SED", "SBC abs,Y", "NOP", "ISB", // $F8
    "NOP", "SBC abs,X", "INC abs,X", "ISB", // $FC
];

/// `opcodeToMnemonic(opcode)`. Only the low byte matters; the TS table index is
/// `OPCODE_TABLE[opcode]`, and a value outside 0..255 hits neither table and
/// would render `$<hex2>` — unreachable from a `UTINYINT` column.
pub fn opcode_to_mnemonic(opcode: i64) -> &'static str {
    MNEMONICS[(opcode & 0xff) as usize]
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::create_trace_run_store;

    // ── pure formatting ────────────────────────────────────────────────────

    #[test]
    fn mnemonics_match_the_ts_tables() {
        // documented, imp/acc render bare
        assert_eq!(opcode_to_mnemonic(0x00), "BRK");
        assert_eq!(opcode_to_mnemonic(0x0a), "ASL");
        assert_eq!(opcode_to_mnemonic(0xea), "NOP");
        // documented with a generic operand placeholder
        assert_eq!(opcode_to_mnemonic(0xad), "LDA abs");
        assert_eq!(opcode_to_mnemonic(0x9d), "STA abs,X");
        assert_eq!(opcode_to_mnemonic(0xb1), "LDA (zp),Y");
        assert_eq!(opcode_to_mnemonic(0xa1), "LDA (zp,X)");
        assert_eq!(opcode_to_mnemonic(0x6c), "JMP (abs)");
        assert_eq!(opcode_to_mnemonic(0xd0), "BNE rel");
        assert_eq!(opcode_to_mnemonic(0xa9), "LDA #imm");
        assert_eq!(opcode_to_mnemonic(0xb6), "LDX zp,Y");
        // undoc: bare UPPERCASED kind, NO operand suffix
        assert_eq!(opcode_to_mnemonic(0xeb), "SBC_IMM");
        assert_eq!(opcode_to_mnemonic(0x07), "SLO");
        assert_eq!(opcode_to_mnemonic(0xbf), "LAX");
        // in neither table
        assert_eq!(opcode_to_mnemonic(0x02), "$02");
        assert_eq!(opcode_to_mnemonic(0xb2), "$B2");
    }

    #[test]
    fn hex_and_cell_formatting() {
        assert_eq!(hex4(0xc000), "$C000");
        assert_eq!(hex4(0x12), "$0012");
        assert_eq!(fmt_io(Some('r'), Some(0xd012), Some(0x9d)), "$D012 r=9D");
        assert_eq!(fmt_io(Some('w'), Some(0xd020), Some(0x0)), "$D020 w=00");
        assert_eq!(fmt_io(Some('w'), Some(0xd020), None), "$D020 w=??");
        assert_eq!(fmt_io(None, Some(0xd020), Some(1)), "");
        assert_eq!(fmt_bus(Some(1), Some(0), Some(1)), "A1C0D1");
        assert_eq!(fmt_bus(Some(1), None, None), "A1C-D-");
        assert_eq!(fmt_bus(None, None, None), "");
    }

    #[test]
    fn summarize_cell_collapses_a_shared_prefix_to_a_range() {
        assert_eq!(summarize_cell(&["", ""]), "");
        assert_eq!(summarize_cell(&["$D012 r=9D", "", "$D012 r=9D"]), "$D012 r=9D");
        assert_eq!(
            summarize_cell(&["$D012 r=9D", "$D012 r=A2", "$D012 r=01"]),
            "$D012 r=01..A2"
        );
        // different prefixes → first + ellipsis
        assert_eq!(
            summarize_cell(&["$D012 r=9D", "$D011 r=1B"]),
            "$D012 r=9D \u{2026}"
        );
        // non-IO cells never match the pattern
        assert_eq!(summarize_cell(&["A1C0D1", "A1C1D1"]), "A1C0D1 \u{2026}");
    }

    #[test]
    fn io_cell_pattern_is_anchored() {
        assert_eq!(match_io_cell("$D012 r=9D"), Some(("$D012 r=", "9D")));
        assert_eq!(match_io_cell("$D012 w=0"), Some(("$D012 w=", "0")));
        assert!(match_io_cell("$D012 r=9d").is_none()); // lowercase value
        assert!(match_io_cell("$d012 r=9D").is_none()); // lowercase addr
        assert!(match_io_cell("$D012 x=9D").is_none());
        assert!(match_io_cell("$ r=9D").is_none());
        assert!(match_io_cell("$D012 r=").is_none());
        assert!(match_io_cell("A1C0D1").is_none());
    }

    #[test]
    fn js_number_coercion() {
        assert!(js_number(None).is_nan());
        assert_eq!(js_number(Some(&serde_json::json!(null))), 0.0);
        assert_eq!(js_number(Some(&serde_json::json!(1234))), 1234.0);
        assert_eq!(js_number(Some(&serde_json::json!("1234"))), 1234.0);
        assert!(js_number(Some(&serde_json::json!("abc"))).is_nan());
        assert_eq!(fmt_js_number(1234.0), "1234");
        assert_eq!(fmt_js_number(f64::NAN), "NaN");
        // Rust's f64::max would return 0.0 here; JS Math.max returns NaN.
        assert!(js_max(0.0, f64::NAN).is_nan());
        assert_eq!(js_max(0.0, 17.0), 17.0);
    }

    #[test]
    fn non_finite_cycle_bound_is_a_param_error() {
        let e = build_event_sql("t", None, None, (f64::NAN, 10.0), None).unwrap_err();
        assert_eq!(e.to_string(), "non-finite param: NaN");
    }

    #[test]
    fn a_missing_run_id_inlines_as_sql_null() {
        let sql = build_event_sql("instructions", None, None, (1.0, 2.0), None).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM instructions WHERE run_id = NULL AND clock BETWEEN 1 AND 2 \
             ORDER BY clock LIMIT 100000"
        );
        let sql = build_event_sql(
            "bus_events",
            Some(&"o'brien".to_string()),
            Some("read"),
            (1.0, 2.0),
            Some((0.0, 0xffff as f64)),
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM bus_events WHERE run_id = 'o''brien' AND kind = 'read' \
             AND clock BETWEEN 1 AND 2 AND pc BETWEEN 0 AND 65535 ORDER BY clock LIMIT 100000"
        );
    }

    // ── deriveFlow ─────────────────────────────────────────────────────────

    fn step(cycle: i64, pc: i64, opcode: i64, sp: i64) -> CpuStep {
        CpuStep { cycle, pc, opcode, sp }
    }

    #[test]
    fn flow_detects_an_irq_entry_and_the_rti_return() {
        // CPU_STEP records POST-instruction registers, so `sp` is the value
        // AFTER the opcode's own stack effect. main (SP=$FD) … the hardware IRQ
        // sequence pushes 3 → the first handler instruction retires at SP=$FA;
        // the RTI pops them back to $FD, which `stackEffect(RTI)=+3` cancels out.
        let steps = vec![
            step(10, 0x0810, 0xea, 0xfd), // NOP, main
            step(11, 0x0811, 0xea, 0xfd), // NOP, main
            step(12, 0xea31, 0xa9, 0xfa), // LDA #imm — delta 3 ⇒ irq entry
            step(13, 0xea33, 0x40, 0xfd), // RTI (runs in the handler flow, pops after)
            step(14, 0x0812, 0xea, 0xfd), // back in main
        ];
        let f = derive_flow(&steps, None);
        assert_eq!(f[&10], FlowKind::Main);
        assert_eq!(f[&12], FlowKind::Irq);
        assert_eq!(f[&13], FlowKind::Irq);
        assert_eq!(f[&14], FlowKind::Main);
    }

    #[test]
    fn an_nmi_vector_hint_and_preemption_both_yield_nmi() {
        let steps = vec![
            step(10, 0x0810, 0xea, 0xfd),
            step(11, 0xfe43, 0xa9, 0xfa), // matches the NMI vector
        ];
        assert_eq!(derive_flow(&steps, Some(0xfe43_i64 as f64))[&11], FlowKind::Nmi);
        assert_eq!(derive_flow(&steps, None)[&11], FlowKind::Irq);

        // NMI preempting an IRQ frame: second entry with no vector hint.
        let steps = vec![
            step(10, 0x0810, 0xea, 0xfd),
            step(11, 0xea31, 0xea, 0xfa), // irq
            step(12, 0xfe43, 0xea, 0xf7), // preempts irq ⇒ nmi
        ];
        let f = derive_flow(&steps, None);
        assert_eq!(f[&11], FlowKind::Irq);
        assert_eq!(f[&12], FlowKind::Nmi);
    }

    #[test]
    fn txs_and_brk_do_not_trip_the_delta_heuristic() {
        // TXS moves SP by 3 as its own effect — must NOT read as an interrupt.
        let steps = vec![step(1, 0x0800, 0xa2, 0xfd), step(2, 0x0802, 0x9a, 0xfa)];
        assert_eq!(derive_flow(&steps, None)[&2], FlowKind::Main);
        // BRK pushes 3 itself and enters via arm 2, not arm 1 (one push only).
        let steps = vec![step(1, 0x0800, 0xea, 0xfd), step(2, 0x0801, 0x00, 0xfa)];
        assert_eq!(derive_flow(&steps, None)[&2], FlowKind::Irq);
    }

    // ── foldCells / renderText ─────────────────────────────────────────────

    fn cells(cycle: &str, c64: &str, flow: &str, io: &str, bus: &str) -> Cells {
        Cells {
            cycle: cycle.into(),
            c64: c64.into(),
            flow: flow.into(),
            io: io.into(),
            bus: bus.into(),
            drv: String::new(),
            dio: String::new(),
        }
    }

    #[test]
    fn fold_collapses_a_polling_loop_and_summarises_the_varying_read() {
        let all: Vec<Cells> = (0..4)
            .flat_map(|i| {
                [
                    cells(
                        &(100 + i * 2).to_string(),
                        "$C000 LDA abs",
                        "main",
                        &format!("$D012 r={:02X}", 0x9d + i),
                        "",
                    ),
                    cells(&(101 + i * 2).to_string(), "$C003 BNE rel", "main", "", ""),
                ]
            })
            .collect();
        let items = fold_cells(&all, MAX_FOLD_PERIOD);
        assert_eq!(items.len(), 1);
        match &items[0] {
            Item::Group { reps, body } => {
                assert_eq!(*reps, 4);
                assert_eq!(body.len(), 2);
                assert_eq!(body[0].cycle, "100"); // first iteration's cycle
                assert_eq!(body[0].io, "$D012 r=9D..A0");
                assert_eq!(body[1].io, "");
            }
            Item::Row(_) => panic!("expected a fold group"),
        }
    }

    #[test]
    fn a_flow_change_fences_the_fold() {
        let mut all: Vec<Cells> = Vec::new();
        for i in 0..3 {
            all.push(cells(&i.to_string(), "$C000 LDA abs", "main", "", ""));
        }
        all.push(cells("3", "$EA31 LDA #imm", "irq", "", ""));
        for i in 4..7 {
            all.push(cells(&i.to_string(), "$C000 LDA abs", "main", "", ""));
        }
        let items = fold_cells(&all, MAX_FOLD_PERIOD);
        // group(main×3) + the lone irq row + group(main×3)
        assert_eq!(items.len(), 3);
        assert!(matches!(items[0], Item::Group { reps: 3, .. }));
        assert!(matches!(&items[1], Item::Row(c) if c.flow == "irq"));
        assert!(matches!(items[2], Item::Group { reps: 3, .. }));
    }

    fn row(cycle: i64, pc: i64, op: &'static str, flow: FlowKind) -> SwimlaneRow {
        SwimlaneRow {
            cycle,
            c64_pc: Some(pc),
            c64_op: Some(op),
            c64_flow: Some(flow),
            ..SwimlaneRow::default()
        }
    }

    #[test]
    fn render_text_layout_is_byte_exact() {
        let slice = SwimlaneSlice {
            start_cycle: 1000.0,
            end_cycle: 1003.0,
            compact: true,
            rows: vec![
                SwimlaneRow {
                    c64_io_rw: Some('r'),
                    c64_io_addr: Some(0xd012),
                    c64_io_value: Some(0x9d),
                    ..row(1000, 0xc000, "LDA abs", FlowKind::Main)
                },
                row(1001, 0xc003, "BNE rel", FlowKind::Main),
                row(1002, 0xea31, "LDA #imm", FlowKind::Irq),
            ],
        };
        let out = render_text(&slice, MAX_ROWS_DEFAULT, true);
        assert_eq!(
            out,
            concat!(
                // c64 width = len("$EA31 LDA #imm") = 14
                "swimlane 1000\u{2013}1003 (compact)  3 rows (3 raw)\n",
                "cycle  c64             flow  io\n",
                "1000   $C000 LDA abs   main  $D012 r=9D\n",
                "1001   $C003 BNE rel   main\n",
                "1002   $EA31 LDA #imm  irq"
            )
        );
    }

    #[test]
    fn the_flow_column_is_hidden_when_everything_is_main() {
        let slice = SwimlaneSlice {
            start_cycle: 0.0,
            end_cycle: 2.0,
            compact: false,
            rows: vec![
                row(0, 0xc000, "LDA abs", FlowKind::Main),
                row(1, 0xc003, "STA abs", FlowKind::Main),
            ],
        };
        let out = render_text(&slice, MAX_ROWS_DEFAULT, false);
        assert_eq!(
            out,
            concat!(
                "swimlane 0\u{2013}2  2 rows (2 raw)\n",
                "cycle  c64\n",
                "0      $C000 LDA abs\n",
                "1      $C003 STA abs"
            )
        );
    }

    #[test]
    fn an_empty_window_renders_one_line() {
        let slice = SwimlaneSlice {
            start_cycle: 5.0,
            end_cycle: 9.0,
            compact: true,
            rows: vec![],
        };
        assert_eq!(
            render_text(&slice, MAX_ROWS_DEFAULT, true),
            "swimlane 5\u{2013}9: (no events in window)"
        );
        // a row with no lane data at all is dropped as filler, same outcome
        let slice = SwimlaneSlice {
            start_cycle: 5.0,
            end_cycle: 9.0,
            compact: true,
            rows: vec![SwimlaneRow { cycle: 6, ..SwimlaneRow::default() }],
        };
        assert_eq!(
            render_text(&slice, MAX_ROWS_DEFAULT, true),
            "swimlane 5\u{2013}9: (no events in window)"
        );
    }

    #[test]
    fn the_truncation_footer_counts_render_rows() {
        let rows: Vec<SwimlaneRow> = (0..250)
            .map(|i: i64| row(i, 0xc000 + (i % 7), "LDA abs", FlowKind::Main))
            .collect();
        let slice = SwimlaneSlice {
            start_cycle: 0.0,
            end_cycle: 249.0,
            compact: true,
            rows,
        };
        // fold off so the render-row count is exactly the row count
        let out = render_text(&slice, MAX_ROWS_DEFAULT, false);
        let last = out.lines().last().unwrap();
        assert_eq!(
            last,
            "\u{2026} 50 more rows \u{2014} narrow with `swimlane <s> <e>`"
        );
        assert!(out.starts_with("swimlane 0\u{2013}249 (compact)  200 rows (250 raw)\n"));
    }

    #[test]
    fn the_fold_tag_is_appended_after_the_padded_columns() {
        let rows: Vec<SwimlaneRow> = (0..4)
            .map(|i| row(i, 0xc000, "LDA abs", FlowKind::Main))
            .collect();
        let slice = SwimlaneSlice {
            start_cycle: 0.0,
            end_cycle: 3.0,
            compact: false,
            rows,
        };
        let out = render_text(&slice, MAX_ROWS_DEFAULT, true);
        assert_eq!(
            out,
            concat!(
                "swimlane 0\u{2013}3  1 rows (4 raw)\n",
                "cycle  c64\n",
                "0      $C000 LDA abs  \u{21ba}\u{00d7}4"
            )
        );
    }

    #[test]
    fn slice_json_uses_the_ts_insertion_order_and_omits_absent_fields() {
        let slice = SwimlaneSlice {
            start_cycle: 10.0,
            end_cycle: 20.0,
            compact: true,
            rows: vec![
                SwimlaneRow {
                    c64_io_rw: Some('w'),
                    c64_io_addr: Some(0xd020),
                    c64_io_value: Some(0),
                    bus_atn: Some(1),
                    bus_clk: Some(0),
                    bus_data: Some(1),
                    ..row(11, 0xc000, "STA abs", FlowKind::Irq)
                },
                SwimlaneRow { cycle: 12, ..SwimlaneRow::default() },
            ],
        };
        assert_eq!(
            slice_to_json_string(&slice),
            concat!(
                r#"{"startCycle":10,"endCycle":20,"rows":["#,
                r#"{"cycle":11,"c64Pc":49152,"c64Op":"STA abs","c64Flow":"irq","#,
                r#""c64IoRw":"w","c64IoAddr":53280,"c64IoValue":0,"#,
                r#""busAtn":1,"busClk":0,"busData":1},"#,
                r#"{"cycle":12}"#,
                r#"],"compact":true}"#
            )
        );
    }

    // ── end-to-end over a real (in-memory) Spec-726 store ──────────────────

    /// Build a tiny Shape-B store: 4 CPU steps, one IO read, one IO write and
    /// one IEC line change.
    fn tiny_store() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        create_trace_run_store(&conn).unwrap();
        conn.execute_batch(
            r#"
            INSERT INTO trace_run (run_id, def_id, def_version, cycle_start, cycle_end)
              VALUES ('run-1', 'def-1', 1, 1000, 1010);
            INSERT INTO trace_event VALUES
              ('run-1', 0, 1000, 'cpu', 'pc-range', 'cpu-row',
               '{"pc":49152,"opcode":173,"b1":18,"b2":208,"a":0,"x":0,"y":0,"sp":253,"p":32}'),
              ('run-1', 1, 1000, 'io', 'mem-access', 'mem-row',
               '{"addr":53266,"value":157,"op":"read","pc":49152,"side":"c64","cycle_c64":1000}'),
              ('run-1', 2, 1001, 'cpu', 'pc-range', 'cpu-row',
               '{"pc":49155,"opcode":141,"b1":32,"b2":208,"a":0,"x":0,"y":0,"sp":253,"p":32}'),
              ('run-1', 3, 1001, 'io', 'mem-access', 'mem-row',
               '{"addr":53280,"value":1,"op":"write","pc":49155,"side":"c64","cycle_c64":1001}'),
              ('run-1', 4, 1002, 'iec', 'iec-transition', 'iec-row',
               '{"atn":true,"clk":false,"data":true,"c64_atn":true,"c64_clk":false,"c64_data":true,"drv_clk":false,"drv_data":true,"drv_atn_ack":false}'),
              ('run-1', 5, 1003, 'cpu', 'pc-range', 'cpu-row',
               '{"pc":49158,"opcode":234,"b1":0,"b2":0,"a":0,"x":0,"y":0,"sp":253,"p":32}');
            "#,
        )
        .unwrap();
        conn
    }

    #[test]
    fn slice_over_a_real_store_joins_the_lanes_on_one_timeline() {
        let conn = tiny_store();
        let q = SwimlaneQuery {
            run_id: Some("run-1".into()),
            cycle_range: (0.0, 100_000.0),
            compact: true,
            ..SwimlaneQuery::default()
        };
        let slice = swimlane_slice(&conn, StoreShape::Spec726, &q).unwrap();
        assert_eq!(slice.start_cycle, 0.0);
        assert_eq!(slice.end_cycle, 100_000.0);
        assert_eq!(slice.rows.len(), 4);

        let r0 = &slice.rows[0];
        assert_eq!(r0.cycle, 1000);
        assert_eq!(r0.c64_pc, Some(0xc000));
        assert_eq!(r0.c64_op, Some("LDA abs"));
        assert_eq!(r0.c64_flow, Some(FlowKind::Main));
        assert_eq!(r0.c64_io_rw, Some('r'));
        assert_eq!(r0.c64_io_addr, Some(0xd012));
        assert_eq!(r0.c64_io_value, Some(0x9d));
        assert_eq!(r0.bus_atn, None);

        // The write wins over a read in the same cycle (there is no read here).
        let r1 = &slice.rows[1];
        assert_eq!(r1.c64_io_rw, Some('w'));
        assert_eq!(r1.c64_io_addr, Some(0xd020));
        assert_eq!(r1.c64_io_value, Some(1));

        // IEC levels land and then carry forward.
        let r2 = &slice.rows[2];
        assert_eq!(r2.cycle, 1002);
        assert_eq!(r2.c64_pc, None);
        assert_eq!((r2.bus_atn, r2.bus_clk, r2.bus_data), (Some(1), Some(0), Some(1)));
        let r3 = &slice.rows[3];
        assert_eq!(r3.cycle, 1003);
        assert_eq!(r3.c64_op, Some("NOP"));
        assert_eq!((r3.bus_atn, r3.bus_clk, r3.bus_data), (Some(1), Some(0), Some(1)));

        // The rendered text of the same slice.
        assert_eq!(
            render_text(&slice, MAX_ROWS_DEFAULT, true),
            concat!(
                // the IEC lane only starts at 1002 and then carries forward
                "swimlane 0\u{2013}100000 (compact)  4 rows (4 raw)\n",
                "cycle  c64            io          iec\n",
                "1000   $C000 LDA abs  $D012 r=9D\n",
                "1001   $C003 STA abs  $D020 w=01\n",
                "1002                              A1C0D1\n",
                "1003   $C006 NOP                  A1C0D1"
            )
        );
    }

    #[test]
    fn a_wrong_or_missing_run_id_matches_nothing() {
        let conn = tiny_store();
        for run_id in [None, Some("nope".to_string())] {
            let q = SwimlaneQuery {
                run_id,
                cycle_range: (0.0, 100_000.0),
                compact: true,
                ..SwimlaneQuery::default()
            };
            let slice = swimlane_slice(&conn, StoreShape::Spec726, &q).unwrap();
            assert!(slice.rows.is_empty());
            assert_eq!(
                render_text(&slice, MAX_ROWS_DEFAULT, true),
                "swimlane 0\u{2013}100000: (no events in window)"
            );
        }
    }

    #[test]
    fn the_window_clips_and_compact_drops_unchanged_rows() {
        let conn = tiny_store();
        let q = SwimlaneQuery {
            run_id: Some("run-1".into()),
            cycle_range: (1002.0, 1003.0),
            compact: true,
            ..SwimlaneQuery::default()
        };
        let slice = swimlane_slice(&conn, StoreShape::Spec726, &q).unwrap();
        assert_eq!(
            slice.rows.iter().map(|r| r.cycle).collect::<Vec<_>>(),
            vec![1002, 1003]
        );

        // compact:false keeps every cycle; here both differ anyway, so use a
        // window where the IEC state repeats to prove the drop.
        let q = SwimlaneQuery {
            run_id: Some("run-1".into()),
            cycle_range: (1002.0, 1002.0),
            compact: true,
            ..SwimlaneQuery::default()
        };
        let slice = swimlane_slice(&conn, StoreShape::Spec726, &q).unwrap();
        assert_eq!(slice.rows.len(), 1);
    }

    #[test]
    fn focus_narrows_to_one_lane() {
        let conn = tiny_store();
        let q = SwimlaneQuery {
            run_id: Some("run-1".into()),
            cycle_range: (0.0, 100_000.0),
            compact: true,
            focus: Some(FlowKind::Irq),
            ..SwimlaneQuery::default()
        };
        let slice = swimlane_slice(&conn, StoreShape::Spec726, &q).unwrap();
        assert!(slice.rows.is_empty());

        let q = SwimlaneQuery {
            run_id: Some("run-1".into()),
            cycle_range: (0.0, 100_000.0),
            compact: true,
            focus: Some(FlowKind::Main),
            ..SwimlaneQuery::default()
        };
        let slice = swimlane_slice(&conn, StoreShape::Spec726, &q).unwrap();
        // only the three cpu_step cycles carry a flow lane
        assert_eq!(
            slice.rows.iter().map(|r| r.cycle).collect::<Vec<_>>(),
            vec![1000, 1001, 1003]
        );
    }

    #[test]
    fn drive_pc_rows_render_in_the_c64_lane() {
        // Preserved defect 2: INSTRUCTIONS_726 covers channel IN ('cpu','drive_pc')
        // and queryEvents applies no cpu filter.
        let conn = Connection::open_in_memory().unwrap();
        create_trace_run_store(&conn).unwrap();
        conn.execute_batch(
            r#"INSERT INTO trace_event VALUES
              ('run-1', 0, 2000, 'drive_pc', 'pc-range', 'cpu-row',
               '{"pc":63488,"opcode":234,"b1":0,"b2":0,"a":0,"x":0,"y":0,"sp":255,"p":32,"side":"drive","clk":2000}');"#,
        )
        .unwrap();
        let q = SwimlaneQuery {
            run_id: Some("run-1".into()),
            cycle_range: (0.0, 100_000.0),
            compact: true,
            ..SwimlaneQuery::default()
        };
        let slice = swimlane_slice(&conn, StoreShape::Spec726, &q).unwrap();
        assert_eq!(slice.rows.len(), 1);
        assert_eq!(slice.rows[0].c64_pc, Some(0xf800));
        assert_eq!(slice.rows[0].drv_pc, None); // never populated
    }

    #[test]
    fn cycle_bounds_fall_back_to_zero_on_an_empty_store() {
        let conn = Connection::open_in_memory().unwrap();
        create_trace_run_store(&conn).unwrap();
        assert_eq!(cycle_bounds(&conn).unwrap(), (0.0, 0.0));
        assert_eq!(
            scalar_text(&conn, "SELECT run_id FROM trace_run LIMIT 1").unwrap(),
            None
        );
    }
}
