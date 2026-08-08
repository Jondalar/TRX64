//! `profile_loader` — loader / protection profiling (Spec 802 F2 / C64RE Spec 245).
//!
//! Reference: `src/runtime/headless/v2/loader-profile.ts`
//! (`profileLoader(backend, runId, range, opts)`), reached from the retired
//! sidecar as
//! `profileLoader(backend, a.scenario_id, [Number(a.cycle_start), Number(a.cycle_end)])`
//! — i.e. with `opts` OMITTED, so every option keeps its default.
//!
//! # Built entirely on `query_events`
//!
//! Like `follow_path`, the reference issues no SQL: it fetches seven families
//! (`cpu_step`, `mem_read`, `mem_write`, `drive_atn_change`,
//! `drive_clk_change`, `drive_data_change`, `gcr_byte`) through `queryEvents`
//! and does everything else in memory. This port does the same — one
//! connection, seven [`crate::query_events::query_events`] calls, then
//! [`profile_from_events`], which is pure and therefore unit-testable without a
//! store.
//!
//! # Argument case — snake_case
//!
//! `{ scenario_id, cycle_start, cycle_end }`. This op is snake_case; its two
//! F2 siblings (`query_events`, `follow_path`) are camelCase (R3 §5).
//!
//! # JS semantics that are parity surface here
//!
//! * **`NaN` propagates, it does not become 0.** An absent `cycle_start`
//!   yields `Number(undefined) === NaN`. That value never reaches the
//!   aggregator — the backend's `inlineParam` rejects a non-finite bind param
//!   first (see [`assert_finite_param`]) — but every number still leaves
//!   through [`js::num`], which renders a non-finite one as `null` exactly as
//!   `JSON.stringify` does.
//! * **Integral numbers carry no `.0`.** Again [`js::num`].
//! * **Stable sorts.** `Array.prototype.sort` is stable in V8 and the three
//!   comparators here (`a.addr - b.addr`, `a - b`, `a.cycle - b.cycle`) return
//!   `NaN` — i.e. "equal" per the spec — for a non-comparable pair. Rust's
//!   `sort_by` is stable too, so `partial_cmp(...).unwrap_or(Equal)` is a
//!   faithful transcription, including the tie order.
//! * **`bitTimingHistogram` is a JS object keyed by a number**, so its keys are
//!   the numbers' `String()` forms.
//! * **`undefined` fields vanish from the JSON.** An absent `scenario_id` makes
//!   `scenarioId` *undefined*, and `JSON.stringify` drops the key entirely —
//!   whereas an explicit `null` is kept as `null`. Both are reproduced
//!   ([`op_profile_loader`] owns the null case, since only the wire form can
//!   tell the two apart).

use crate::conn::with_conn;
use crate::error::{Result, TraceReadError};
use crate::queries::js;
use crate::query_events::{query_events, row_num, EventFamily, EventQuery, EventRow};
use crate::schema::StoreShape;
use duckdb::Connection;
use serde_json::{Map, Value};
use std::cell::OnceCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;

// ─────────────────────────────────────────────────────────────────────────────
// Constants — 1:1 with loader-profile.ts
// ─────────────────────────────────────────────────────────────────────────────

/// CIA 1/2 timer registers ($DC04-$DC07, $DD04-$DD07). Consulted only by the
/// `timing_check` heuristic, which is inert — see [`get_abs`].
const CIA_TIMER_ADDRS: [f64; 8] = [
    0xDC04 as f64,
    0xDC05 as f64,
    0xDC06 as f64,
    0xDC07 as f64,
    0xDD04 as f64,
    0xDD05 as f64,
    0xDD06 as f64,
    0xDD07 as f64,
];

const IO_ADDR_LO: f64 = 0xD000 as f64;
const IO_ADDR_HI: f64 = 0xDFFF as f64;

/// `pc >= $C000` counts as a drive step (the reference's `DRIVE_PC_LO`).
const DRIVE_PC_LO: f64 = 0xC000 as f64;
/// One IEC bit ≈ 56 cycles at 1 MHz (the reference's `IEC_CYCLES_PER_BIT`).
const IEC_CYCLES_PER_BIT: f64 = 56.0;

const OP_BNE: f64 = 0xD0 as f64;
const OP_BEQ: f64 = 0xF0 as f64;
const OP_JMP_IND: f64 = 0x6C as f64;
const OP_JSR: f64 = 0x20 as f64;
const LDA_ABS: f64 = 0xAD as f64;

/// EOR (imm, zp, abs, abs.x, abs.y, (zp,x), (zp),y).
const EOR_OPCODES: [f64; 7] = [
    0x49 as f64,
    0x45 as f64,
    0x4D as f64,
    0x5D as f64,
    0x59 as f64,
    0x41 as f64,
    0x51 as f64,
];
/// ADC, same addressing modes.
const ADC_OPCODES: [f64; 7] = [
    0x69 as f64,
    0x65 as f64,
    0x6D as f64,
    0x7D as f64,
    0x79 as f64,
    0x61 as f64,
    0x71 as f64,
];
/// CMP, same addressing modes.
const CMP_OPCODES: [f64; 7] = [
    0xC9 as f64,
    0xC5 as f64,
    0xCD as f64,
    0xDD as f64,
    0xD9 as f64,
    0xC1 as f64,
    0xD1 as f64,
];
/// STA (all stores to memory).
const STA_OPCODES: [f64; 7] = [
    0x85 as f64,
    0x8D as f64,
    0x95 as f64,
    0x99 as f64,
    0x9D as f64,
    0x81 as f64,
    0x91 as f64,
];

/// Lookback / lookahead window, in instructions (the reference's `WINDOW`).
const WINDOW: usize = 8;

/// `Set.prototype.has` over a number set: SameValueZero, so `NaN` never hits
/// (no set below contains it) and `-0` matches `0`, which `==` gives us.
fn has_op(set: &[f64], op: f64) -> bool {
    set.iter().any(|&x| x == op)
}

// ─────────────────────────────────────────────────────────────────────────────
// Arguments
// ─────────────────────────────────────────────────────────────────────────────

/// The op's arguments — **snake_case on the wire** — plus the `opts` defaults.
///
/// The sidecar never forwarded `ProfileLoaderOptions`, so `min_confidence`,
/// `pattern_thresholds` and `limit` are always their defaults on this path.
/// They are named here because the reference reads them, and because leaving
/// them implicit is how a default silently drifts.
#[derive(Clone, Debug)]
pub struct ProfileArgs {
    /// `scenario_id` on the wire; it is the `runId` every `queryEvents` call
    /// binds, and it is echoed back as the result's `scenarioId`.
    pub scenario_id: Option<String>,
    /// `Number(a.cycle_start)`; `NaN` when absent.
    pub cycle_start: f64,
    /// `Number(a.cycle_end)`; `NaN` when absent.
    pub cycle_end: f64,
    /// `opts.minConfidence ?? 0` — candidates below this are dropped.
    pub min_confidence: f64,
    /// `opts.limit ?? 50_000` — max events fetched PER FAMILY.
    pub limit: f64,
}

impl Default for ProfileArgs {
    fn default() -> Self {
        ProfileArgs {
            scenario_id: None,
            cycle_start: f64::NAN,
            cycle_end: f64::NAN,
            min_confidence: 0.0,
            limit: 50_000.0,
        }
    }
}

impl ProfileArgs {
    /// Parse the WS `args` object. **snake_case** (R3 §5).
    pub fn from_snake(v: &Value) -> Self {
        ProfileArgs {
            scenario_id: js::opt_str(v, "scenario_id"),
            cycle_start: js::number_arg(v.get("cycle_start")),
            cycle_end: js::number_arg(v.get("cycle_end")),
            ..ProfileArgs::default()
        }
    }

    /// `cyclesTotal = endCycle - startCycle` (not clamped by the reference).
    pub fn cycles_total(&self) -> f64 {
        self.cycle_end - self.cycle_start
    }

    /// `opts.patternThresholds?.[pattern] ?? minConfidence`.
    ///
    /// `patternThresholds` is unreachable on the wire path (the sidecar passes
    /// no `opts`), so this is always `min_confidence` today; it is a function so
    /// that adding the option later has one place to change.
    fn threshold_for(&self, _pattern: &str) -> f64 {
        self.min_confidence
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JS number rendering (template literals + object keys)
// ─────────────────────────────────────────────────────────────────────────────

/// JS `String(n)` for a number — the form a template literal and an object key
/// take. Integral values never carry a `.0`.
fn js_num_str(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if f == f.trunc() && f.abs() < 1e21 {
        return format!("{}", f as i128);
    }
    format!("{f}")
}

/// The reference's `hex()`: `n.toString(16).toUpperCase().padStart(4, "0")`.
///
/// Faithful for the whole reachable domain (`pc` / `addr` are integer columns),
/// including the odd corners JS produces: a negative value keeps its sign
/// *inside* the padding (`-5` → `"00-5"`), and a value wider than four digits
/// is not truncated.
fn hex(n: f64) -> String {
    let body = if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else if n == n.trunc() && n.abs() < 9.007_199_254_740_992e15 {
        let i = n as i128;
        if i < 0 {
            format!("-{:x}", -i)
        } else {
            format!("{i:x}")
        }
    } else {
        // A fractional value would render as a hex fraction in JS. No column of
        // either store shape can produce one; keep something readable.
        format!("{n}")
    };
    let up = body.to_uppercase();
    let len = up.chars().count();
    if len >= 4 {
        up
    } else {
        format!("{}{up}", "0".repeat(4 - len))
    }
}

/// A hash key with JS `Map`/`Set` (SameValueZero) semantics: every `NaN` is one
/// key, and `-0` is the same key as `0`.
fn js_key(x: f64) -> u64 {
    if x.is_nan() {
        f64::NAN.to_bits()
    } else if x == 0.0 {
        0.0f64.to_bits()
    } else {
        x.to_bits()
    }
}

/// `(a, b) => a - b` as `Array.prototype.sort` consumes it: a `NaN` result is
/// treated as `+0`, i.e. "equal", and the sort is stable.
fn js_cmp(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// A numeric field of an event row, as the reference sees it **in memory**.
///
/// `query_events` has already applied `rowFromDb`'s `Number(...)`, and its
/// `NaN` (which cannot arise from a numeric column) arrives as JSON `null`
/// — so mapping "absent or null" back to `NaN` restores the reference's value
/// exactly, including the comparisons that a `NaN` silently loses.
fn n(row: &EventRow, key: &str) -> f64 {
    row_num(row, key).unwrap_or(f64::NAN)
}

// ─────────────────────────────────────────────────────────────────────────────
// The op
// ─────────────────────────────────────────────────────────────────────────────

/// Aggregate a loader profile from an OPEN store.
///
/// Returns the `LoaderProfile` as JSON: `{ scenarioId, startCycle, endCycle,
/// cyclesTotal, c64Cycles, driveCycles, iecCycles, ioTouches, iecActivity,
/// diskActivity, protectionCandidates }`.
pub fn profile_loader(conn: &Connection, shape: StoreShape, args: &ProfileArgs) -> Result<Value> {
    // Each `queryEvents` below hands `cycleRange` to the backend as two params,
    // and `duckdb-backend.ts`'s `inlineParam` REJECTS a non-finite number. The
    // sidecar reaches this op as `[Number(a.cycle_start), Number(a.cycle_end)]`,
    // so an ABSENT bound is `NaN` and the whole op fails before a row is read —
    // `swimlane` and `taint` reproduce the same rejection. Checked once here, in
    // param order, which is observationally identical to the reference: all
    // seven parallel queries would reject with this message, so whichever wins
    // the `Promise.all` race carries the same text.
    assert_finite_param(args.cycle_start)?;
    assert_finite_param(args.cycle_end)?;

    // The reference's `Promise.all` of seven `queryEvents` calls. Order of
    // issue is irrelevant; the seven result sets are not.
    let cpu_steps = fetch(conn, shape, args, EventFamily::CpuStep)?;
    let mem_reads = fetch(conn, shape, args, EventFamily::MemRead)?;
    let mem_writes = fetch(conn, shape, args, EventFamily::MemWrite)?;
    let atn_changes = fetch(conn, shape, args, EventFamily::DriveAtnChange)?;
    let clk_changes = fetch(conn, shape, args, EventFamily::DriveClkChange)?;
    let data_changes = fetch(conn, shape, args, EventFamily::DriveDataChange)?;
    let gcr_bytes = fetch(conn, shape, args, EventFamily::GcrByte)?;

    Ok(profile_from_events(
        args,
        &cpu_steps,
        &mem_reads,
        &mem_writes,
        &atn_changes,
        &clk_changes,
        &data_changes,
        &gcr_bytes,
    ))
}

/// `duckdb-backend.ts` `inlineParam` for a number: a non-finite value is a hard
/// error whose message is parity surface (`swimlane.rs` / `taint.rs` carry the
/// same guard as `inline_number` / `inline_num`).
fn assert_finite_param(v: f64) -> Result<()> {
    if v.is_finite() {
        Ok(())
    } else {
        Err(TraceReadError::other(format!(
            "non-finite param: {}",
            js_num_str(v)
        )))
    }
}

/// One `queryEvents(backend, { runId, family, cycleRange, limit })`.
fn fetch(
    conn: &Connection,
    shape: StoreShape,
    args: &ProfileArgs,
    family: EventFamily,
) -> Result<Vec<EventRow>> {
    query_events(
        conn,
        shape,
        &EventQuery {
            run_id: args.scenario_id.clone(),
            family: Some(family),
            // The reference always passes the pair, even when both ends are
            // `NaN` (`if (q.cycleRange)` tests the ARRAY, which is truthy).
            cycle_range: Some((args.cycle_start, args.cycle_end)),
            limit: Some(args.limit),
            ..EventQuery::default()
        },
    )
}

/// Steps 2-6 of `profileLoader` — everything after the fetches, and pure.
#[allow(clippy::too_many_arguments)]
pub(crate) fn profile_from_events(
    args: &ProfileArgs,
    cpu_steps: &[EventRow],
    mem_reads: &[EventRow],
    mem_writes: &[EventRow],
    atn_changes: &[EventRow],
    clk_changes: &[EventRow],
    data_changes: &[EventRow],
    gcr_bytes: &[EventRow],
) -> Value {
    // ---- 2. Cycle split ----------------------------------------------------
    let mut c64_cycles = 0.0f64;
    let mut drive_cycles = 0.0f64;
    for ev in cpu_steps {
        // A `NaN` pc fails `>=` and lands on the c64 side, as in JS.
        if n(ev, "pc") >= DRIVE_PC_LO {
            drive_cycles += 1.0;
        } else {
            c64_cycles += 1.0;
        }
    }
    let iec_cycles = clk_changes.len() as f64 * IEC_CYCLES_PER_BIT;

    // ---- 3. IO touches -----------------------------------------------------
    struct IoRec {
        addr: f64,
        reads: f64,
        writes: f64,
        vals: Vec<f64>,
        seen: HashSet<u64>,
    }
    // A JS `Map` in insertion order: the Vec IS the order, the HashMap is the
    // lookup. (The array is re-sorted by address below, stably.)
    let mut io: Vec<IoRec> = Vec::new();
    let mut io_at: HashMap<u64, usize> = HashMap::new();
    let touch = |io: &mut Vec<IoRec>,
                 io_at: &mut HashMap<u64, usize>,
                 addr: f64,
                 value: f64,
                 write: bool| {
        // `addr < LO || addr > HI` — a `NaN` fails BOTH, so it is NOT skipped.
        if addr < IO_ADDR_LO || addr > IO_ADDR_HI {
            return;
        }
        let k = js_key(addr);
        let idx = *io_at.entry(k).or_insert_with(|| {
            io.push(IoRec { addr, reads: 0.0, writes: 0.0, vals: Vec::new(), seen: HashSet::new() });
            io.len() - 1
        });
        let rec = &mut io[idx];
        if write {
            rec.writes += 1.0;
        } else {
            rec.reads += 1.0;
        }
        if rec.seen.insert(js_key(value)) {
            rec.vals.push(value);
        }
    };
    for ev in mem_reads {
        touch(&mut io, &mut io_at, n(ev, "addr"), n(ev, "value"), false);
    }
    for ev in mem_writes {
        touch(&mut io, &mut io_at, n(ev, "addr"), n(ev, "value"), true);
    }
    io.sort_by(|a, b| js_cmp(a.addr, b.addr));

    let io_touches: Vec<Value> = io
        .into_iter()
        .map(|mut rec| {
            rec.vals.sort_by(|a, b| js_cmp(*a, *b));
            let mut o = Map::new();
            o.insert("addr".into(), js::num(rec.addr));
            o.insert("reads".into(), js::num(rec.reads));
            o.insert("writes".into(), js::num(rec.writes));
            o.insert(
                "distinctValues".into(),
                Value::Array(rec.vals.into_iter().map(js::num).collect()),
            );
            Value::Object(o)
        })
        .collect();

    // ---- 4. IEC activity ---------------------------------------------------
    let atn_edges = atn_changes.len() as f64;
    let clk_edges = clk_changes.len() as f64;
    let data_edges = data_changes.len() as f64;
    // 8 bits per byte, 2 CLK edges per bit.
    let bytes_transferred = (clk_edges / 16.0).floor();

    let mut histogram: Map<String, Value> = Map::new();
    for i in 1..clk_changes.len() {
        let gap = n(&clk_changes[i], "cycle") - n(&clk_changes[i - 1], "cycle");
        if gap > 0.0 && gap < 10_000.0 {
            // Bucket to the nearest 10 cycles. `Math.round` is half-UP, but the
            // gap is positive here, so `f64::round` (half-away-from-zero)
            // agrees on the whole reachable domain.
            let bucket = (gap / 10.0).round() * 10.0;
            let key = js_num_str(bucket);
            let prev = histogram.get(&key).and_then(Value::as_f64).unwrap_or(0.0);
            histogram.insert(key, js::num(prev + 1.0));
        }
    }

    let mut iec_activity = Map::new();
    iec_activity.insert("atnEdges".into(), js::num(atn_edges));
    iec_activity.insert("clkEdges".into(), js::num(clk_edges));
    iec_activity.insert("dataEdges".into(), js::num(data_edges));
    iec_activity.insert("bytesTransferred".into(), js::num(bytes_transferred));
    iec_activity.insert("bitTimingHistogram".into(), Value::Object(histogram));

    // ---- 5. Disk activity --------------------------------------------------
    let mut tracks: Vec<f64> = Vec::new();
    let mut tracks_seen: HashSet<u64> = HashSet::new();
    let mut seek_count = 0.0f64;
    // `prevTrack = -1` is the "no previous event" sentinel AND a legal track
    // value (`trackHalf` of -1 or -2 floors to -1); the reference lets the two
    // collide, so the port must too.
    let mut prev_track = -1.0f64;
    for ev in gcr_bytes {
        let track = (n(ev, "trackHalf") / 2.0).floor();
        if tracks_seen.insert(js_key(track)) {
            tracks.push(track);
        }
        if track != prev_track && prev_track != -1.0 {
            seek_count += 1.0;
        }
        prev_track = track;
    }
    tracks.sort_by(|a, b| js_cmp(*a, *b));

    let mut disk_activity = Map::new();
    disk_activity.insert(
        "tracksVisited".into(),
        Value::Array(tracks.into_iter().map(js::num).collect()),
    );
    disk_activity.insert("bytesReadFromGcr".into(), js::num(gcr_bytes.len() as f64));
    disk_activity.insert("seekCount".into(), js::num(seek_count));

    // ---- 6. Protection pattern detection -----------------------------------
    let raw = detect_patterns(cpu_steps, mem_reads, mem_writes);
    let protection_candidates: Vec<Value> = raw
        .into_iter()
        .filter(|c| c.confidence >= args.threshold_for(c.pattern))
        .map(|c| {
            let mut o = Map::new();
            o.insert("pc".into(), js::num(c.pc));
            o.insert("pattern".into(), Value::String(c.pattern.into()));
            o.insert("cycle".into(), js::num(c.cycle));
            o.insert("description".into(), Value::String(c.description));
            o.insert("confidence".into(), js::num(c.confidence));
            Value::Object(o)
        })
        .collect();

    // ---- result ------------------------------------------------------------
    let mut out = Map::new();
    // `scenarioId: undefined` is DROPPED by JSON.stringify. An explicit wire
    // `null` is re-inserted by `op_profile_loader`.
    if let Some(sid) = &args.scenario_id {
        out.insert("scenarioId".into(), Value::String(sid.clone()));
    }
    out.insert("startCycle".into(), js::num(args.cycle_start));
    out.insert("endCycle".into(), js::num(args.cycle_end));
    out.insert("cyclesTotal".into(), js::num(args.cycles_total()));
    out.insert("c64Cycles".into(), js::num(c64_cycles));
    out.insert("driveCycles".into(), js::num(drive_cycles));
    out.insert("iecCycles".into(), js::num(iec_cycles));
    out.insert("ioTouches".into(), Value::Array(io_touches));
    out.insert("iecActivity".into(), Value::Object(iec_activity));
    out.insert("diskActivity".into(), Value::Object(disk_activity));
    out.insert(
        "protectionCandidates".into(),
        Value::Array(protection_candidates),
    );
    Value::Object(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// Pattern detection engine
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Candidate {
    pc: f64,
    pattern: &'static str,
    cycle: f64,
    description: String,
    confidence: f64,
}

/// The reference's `getAbs(cpuSteps, i)`.
///
/// It **deliberately returns 0**: a `cpu_step` row carries no operand bytes, so
/// the absolute address of an abs-mode instruction is not recoverable, and the
/// reference documents 0 as "unknown — callers tolerate it". Two consequences,
/// both reproduced verbatim below rather than "fixed":
///
/// * `timing_check` never fires — `CIA_TIMER_ADDRS` does not contain 0.
/// * `vector_indirect` never fires — its branch is guarded by `indAddr !== 0`.
fn get_abs(_cpu_steps: &[EventRow], _i: usize) -> f64 {
    0.0
}

fn detect_patterns(
    cpu_steps: &[EventRow],
    mem_reads: &[EventRow],
    mem_writes: &[EventRow],
) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = Vec::new();

    // Write-address index for vector_indirect. The reference builds it eagerly;
    // since the only consumer is behind an inert guard, build it on first use —
    // which never comes. Same values, no cost.
    let writes_by_addr: OnceCell<HashMap<u64, Vec<f64>>> = OnceCell::new();
    let build_writes_by_addr = || {
        let mut m: HashMap<u64, Vec<f64>> = HashMap::new();
        for w in mem_writes {
            m.entry(js_key(n(w, "addr")))
                .or_default()
                .push(n(w, "cycle"));
        }
        m
    };

    // The reference also builds a `ciaReadPcs` set here and never reads it.
    // Omitted: it has no observable effect.

    // `memReads.find(r => r.pc === prev.pc && r.addr >= 0 && r.addr < 0xD000)`
    // is only tested for truthiness, so the linear scan collapses into a
    // membership test on the set of PCs that have such a read.
    let mut ram_read_pcs: HashSet<u64> = HashSet::new();
    for r in mem_reads {
        let addr = n(r, "addr");
        if addr >= 0.0 && addr < 0xD000 as f64 {
            ram_read_pcs.insert(js_key(n(r, "pc")));
        }
    }

    // `getStaTarget(memWrites, pc)` returns the FIRST write with that pc — so
    // the scan collapses into a first-wins map.
    let mut sta_target: HashMap<u64, f64> = HashMap::new();
    for w in mem_writes {
        sta_target.entry(js_key(n(w, "pc"))).or_insert(n(w, "addr"));
    }

    let pcs: Vec<f64> = cpu_steps.iter().map(|s| n(s, "pc")).collect();
    let ops: Vec<f64> = cpu_steps.iter().map(|s| n(s, "opcode")).collect();
    let cycles: Vec<f64> = cpu_steps.iter().map(|s| n(s, "cycle")).collect();

    for i in 0..cpu_steps.len() {
        let op = ops[i];

        // ---- key_compare ---------------------------------------------------
        if op == OP_BNE || op == OP_BEQ {
            let lo = i.saturating_sub(WINDOW);
            let mut has_cmp = false;
            let mut has_ram_read = false;
            for j in lo..i {
                if has_op(&CMP_OPCODES, ops[j]) {
                    has_cmp = true;
                    if ram_read_pcs.contains(&js_key(pcs[j])) {
                        has_ram_read = true;
                    }
                }
            }
            if has_cmp {
                candidates.push(Candidate {
                    pc: pcs[i],
                    pattern: "key_compare",
                    cycle: cycles[i],
                    description: format!(
                        "BNE/BEQ at ${} after CMP; RAM-backed={has_ram_read}",
                        hex(pcs[i])
                    ),
                    confidence: if has_ram_read { 0.80 } else { 0.50 },
                });
            }
        }

        // ---- timing_check --------------------------------------------------
        // INERT by construction (`get_abs` → 0 ∉ CIA_TIMER_ADDRS). Kept
        // branch-for-branch so the port reads against the reference.
        if op == LDA_ABS && has_op(&CIA_TIMER_ADDRS, get_abs(cpu_steps, i)) {
            let hi = cpu_steps.len().min(i + WINDOW);
            let mut has_cmp = false;
            for j in (i + 1)..hi {
                if has_op(&CMP_OPCODES, ops[j]) {
                    has_cmp = true;
                    break;
                }
            }
            candidates.push(Candidate {
                pc: pcs[i],
                pattern: "timing_check",
                cycle: cycles[i],
                description: format!(
                    "LDA CIA-timer at ${}, compare follows={has_cmp}",
                    hex(pcs[i])
                ),
                confidence: if has_cmp { 0.90 } else { 0.65 },
            });
        }

        // ---- self_modify ---------------------------------------------------
        if has_op(&STA_OPCODES, op) {
            // `!== null` only — a target of 0 still counts.
            if let Some(&write_target) = sta_target.get(&js_key(pcs[i])) {
                let hi = cpu_steps.len().min(i + WINDOW);
                for j in (i + 1)..hi {
                    let fut_pc = pcs[j];
                    if write_target >= fut_pc + 1.0 && write_target <= fut_pc + 3.0 {
                        candidates.push(Candidate {
                            pc: pcs[i],
                            pattern: "self_modify",
                            cycle: cycles[i],
                            description: format!(
                                "STA ${} patches operand of instruction at ${}",
                                hex(write_target),
                                hex(fut_pc)
                            ),
                            confidence: 0.92,
                        });
                        break;
                    }
                }
            }
        }

        // ---- vector_indirect -----------------------------------------------
        // INERT by construction (`get_abs` → 0, and the guard is `!== 0`).
        if op == OP_JMP_IND || op == OP_JSR {
            let ind_addr = get_abs(cpu_steps, i);
            if ind_addr != 0.0 {
                let by_addr = writes_by_addr.get_or_init(build_writes_by_addr);
                let here = by_addr.get(&js_key(ind_addr));
                let next = by_addr.get(&js_key(ind_addr + 1.0));
                if here.is_some() || next.is_some() {
                    let prior_write = here
                        .into_iter()
                        .chain(next)
                        .flatten()
                        .any(|&c| c < cycles[i]);
                    if prior_write {
                        candidates.push(Candidate {
                            pc: pcs[i],
                            pattern: "vector_indirect",
                            cycle: cycles[i],
                            description: if op == OP_JMP_IND {
                                format!(
                                    "JMP (${}) via pointer modified within scenario",
                                    hex(ind_addr)
                                )
                            } else {
                                format!(
                                    "JSR ${} where operand was modified within scenario",
                                    hex(ind_addr)
                                )
                            },
                            confidence: 0.85,
                        });
                    }
                }
            }
        }

        // ---- checksum_loop -------------------------------------------------
        if has_op(&EOR_OPCODES, op) || has_op(&ADC_OPCODES, op) {
            let hi = cpu_steps.len().min(i + WINDOW * 2);
            let mut has_loop = false;
            let mut has_cmp = false;
            for j in (i + 1)..hi {
                let o = ops[j];
                if o == OP_BNE || o == OP_BEQ {
                    has_loop = true;
                }
                if has_op(&CMP_OPCODES, o) {
                    has_cmp = true;
                }
            }
            let lo = i.saturating_sub(WINDOW);
            for j in lo..i {
                if has_op(&CMP_OPCODES, ops[j]) {
                    has_cmp = true;
                }
            }
            if has_loop {
                candidates.push(Candidate {
                    pc: pcs[i],
                    pattern: "checksum_loop",
                    cycle: cycles[i],
                    description: format!(
                        "EOR/ADC loop at ${}; compare present={has_cmp}",
                        hex(pcs[i])
                    ),
                    confidence: if has_cmp { 0.75 } else { 0.45 },
                });
            }
        }
    }

    // Deduplicate on `${pc}:${pattern}`, keeping the highest confidence. A JS
    // `Map.set` on an existing key keeps the ORIGINAL insertion position, so
    // the replacement must happen in place.
    let mut order: Vec<Candidate> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for c in candidates {
        let key = format!("{}:{}", js_num_str(c.pc), c.pattern);
        match at.get(&key) {
            Some(&k) => {
                if c.confidence > order[k].confidence {
                    order[k] = c;
                }
            }
            None => {
                at.insert(key, order.len());
                order.push(c);
            }
        }
    }
    order.sort_by(|a, b| js_cmp(a.cycle, b.cycle));
    order
}

// ─────────────────────────────────────────────────────────────────────────────
// Op wrapper
// ─────────────────────────────────────────────────────────────────────────────

/// Op wrapper: `profile_loader` (snake_case args) → the profile object.
pub fn op_profile_loader(duckdb_path: &Path, args: &Value) -> Result<Value> {
    let a = ProfileArgs::from_snake(args);
    // `scenario_id: null` and an absent `scenario_id` both parse to `None`, but
    // JSON.stringify treats them differently: `null` is emitted, `undefined` is
    // dropped. Only the wire form can tell them apart, so the distinction is
    // restored here rather than widening `ProfileArgs`.
    let explicit_null = matches!(args.get("scenario_id"), Some(Value::Null));
    with_conn(duckdb_path, |conn, shape| {
        let mut out = profile_loader(conn, shape, &a)?;
        if explicit_null {
            if let Some(o) = out.as_object_mut() {
                o.insert("scenarioId".into(), Value::Null);
            }
        }
        Ok(out)
    })
}

// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn args_are_snake_case() {
        let a = ProfileArgs::from_snake(&json!({
            "scenario_id": "run-1",
            "cycle_start": 1000,
            "cycle_end": 9000
        }));
        assert_eq!(a.scenario_id.as_deref(), Some("run-1"));
        assert_eq!(a.cycle_start, 1000.0);
        assert_eq!(a.cycle_end, 9000.0);
        assert_eq!(a.cycles_total(), 8000.0);

        // camelCase is a different op's convention and must not be read.
        let camel = ProfileArgs::from_snake(&json!({ "scenarioId": "r", "cycleStart": 1 }));
        assert_eq!(camel.scenario_id, None);
        assert!(camel.cycle_start.is_nan());
    }

    #[test]
    fn opts_keep_their_defaults_on_the_wire_path() {
        let a = ProfileArgs::from_snake(&json!({}));
        assert_eq!(a.min_confidence, 0.0);
        assert_eq!(a.limit, 50_000.0);
        assert!(a.cycle_start.is_nan());
        assert!(a.cycle_end.is_nan());
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    fn row(pairs: &[(&str, Value)]) -> EventRow {
        let mut m = Map::new();
        for (k, v) in pairs {
            m.insert((*k).into(), v.clone());
        }
        m
    }

    fn step(cycle: i64, pc: i64, opcode: i64) -> EventRow {
        row(&[
            ("family", json!("cpu_step")),
            ("cycle", json!(cycle)),
            ("pc", json!(pc)),
            ("opcode", json!(opcode)),
        ])
    }

    fn bus(family: &str, cycle: i64, pc: i64, addr: i64, value: i64) -> EventRow {
        row(&[
            ("family", json!(family)),
            ("cycle", json!(cycle)),
            ("pc", json!(pc)),
            ("addr", json!(addr)),
            ("value", json!(value)),
        ])
    }

    fn line(cycle: i64, level: i64) -> EventRow {
        row(&[("cycle", json!(cycle)), ("level", json!(level))])
    }

    fn gcr(cycle: i64, track_half: i64) -> EventRow {
        row(&[
            ("cycle", json!(cycle)),
            ("byte", json!(0x55)),
            ("trackHalf", json!(track_half)),
        ])
    }

    fn profile(args: &ProfileArgs, sets: [&[EventRow]; 7]) -> Value {
        profile_from_events(
            args, sets[0], sets[1], sets[2], sets[3], sets[4], sets[5], sets[6],
        )
    }

    fn args(start: f64, end: f64) -> ProfileArgs {
        ProfileArgs {
            scenario_id: Some("run-1".into()),
            cycle_start: start,
            cycle_end: end,
            ..ProfileArgs::default()
        }
    }

    // ── number / string rendering ───────────────────────────────────────────

    #[test]
    fn hex_matches_the_js_helper() {
        // n.toString(16).toUpperCase().padStart(4, "0")
        assert_eq!(hex(0.0), "0000");
        assert_eq!(hex(0xd020 as f64), "D020");
        assert_eq!(hex(0x1a as f64), "001A");
        assert_eq!(hex(0x12345 as f64), "12345"); // wider than 4 → no truncation
        assert_eq!(hex(-5.0), "00-5"); // JS pads the START, sign included
    }

    #[test]
    fn js_number_strings_have_no_decimal_point() {
        assert_eq!(js_num_str(60.0), "60");
        assert_eq!(js_num_str(0.0), "0");
        assert_eq!(js_num_str(f64::NAN), "NaN");
        assert_eq!(js_num_str(1234.5), "1234.5");
    }

    // ── aggregation ─────────────────────────────────────────────────────────

    #[test]
    fn cycle_split_uses_c000_and_nan_falls_to_the_c64_side() {
        let steps = [
            step(10, 0x0810, 0xEA),
            step(11, 0xBFFF, 0xEA),
            step(12, 0xC000, 0xEA), // drive
            step(13, 0xF00F, 0xEA), // drive
            row(&[("cycle", json!(14)), ("opcode", json!(0xEA))]), // pc absent → NaN
        ];
        let clk = [line(1, 0), line(2, 1), line(3, 0)];
        let p = profile(&args(0.0, 100.0), [&steps, &[], &[], &[], &clk, &[], &[]]);
        assert_eq!(p["c64Cycles"], json!(3));
        assert_eq!(p["driveCycles"], json!(2));
        assert_eq!(p["iecCycles"], json!(168)); // 3 * 56
        assert_eq!(p["cyclesTotal"], json!(100));
    }

    #[test]
    fn io_touches_are_address_sorted_with_ascending_distinct_values() {
        let reads = [
            bus("mem_read", 1, 0x0810, 0xD012, 0x40),
            bus("mem_read", 2, 0x0813, 0xD012, 0x10),
            bus("mem_read", 3, 0x0816, 0xD012, 0x40), // dup value
            bus("mem_read", 4, 0x0819, 0xCFFF, 0x01), // below IO → ignored
        ];
        let writes = [
            bus("mem_write", 5, 0x081c, 0xD020, 0x00),
            bus("mem_write", 6, 0x081f, 0xD012, 0xFF),
            bus("mem_write", 7, 0x0822, 0xE000, 0x00), // above IO → ignored
        ];
        let p = profile(&args(0.0, 10.0), [&[], &reads, &writes, &[], &[], &[], &[]]);
        assert_eq!(
            p["ioTouches"],
            json!([
                { "addr": 0xD012, "reads": 3, "writes": 1, "distinctValues": [0x10, 0x40, 0xFF] },
                { "addr": 0xD020, "reads": 0, "writes": 1, "distinctValues": [0] },
            ])
        );
    }

    #[test]
    fn clk_gap_histogram_buckets_to_the_nearest_ten() {
        // gaps: 56 → 60, 54 → 50, 0 → dropped, 20000 → dropped (>= 10000)
        let clk = [
            line(1000, 0),
            line(1056, 1),
            line(1110, 0),
            line(1110, 1),
            line(21110, 0),
            line(21166, 1), // gap 56 → 60 again
        ];
        let p = profile(&args(0.0, 30000.0), [&[], &[], &[], &[], &clk, &[], &[]]);
        assert_eq!(p["iecActivity"]["clkEdges"], json!(6));
        assert_eq!(p["iecActivity"]["bytesTransferred"], json!(0)); // floor(6/16)
        assert_eq!(
            p["iecActivity"]["bitTimingHistogram"],
            json!({ "60": 2, "50": 1 })
        );
    }

    #[test]
    fn disk_activity_counts_seeks_but_never_on_the_first_event() {
        let gcr_rows = [
            gcr(10, 36), // track 18
            gcr(11, 36),
            gcr(12, 34), // track 17 → seek
            gcr(13, 36), // track 18 → seek
        ];
        let p = profile(&args(0.0, 100.0), [&[], &[], &[], &[], &[], &[], &gcr_rows]);
        assert_eq!(
            p["diskActivity"],
            json!({ "tracksVisited": [17, 18], "bytesReadFromGcr": 4, "seekCount": 2 })
        );
    }

    // ── pattern detection ───────────────────────────────────────────────────

    #[test]
    fn key_compare_confidence_depends_on_a_ram_backed_read() {
        // CMP at $0810 with a RAM read at the same pc, then BNE at $0812.
        let steps = [step(100, 0x0810, 0xC9), step(101, 0x0812, 0xD0)];
        let reads = [bus("mem_read", 100, 0x0810, 0x00FE, 0x2A)];
        let p = profile(&args(0.0, 200.0), [&steps, &reads, &[], &[], &[], &[], &[]]);
        assert_eq!(
            p["protectionCandidates"],
            json!([{
                "pc": 0x0812,
                "pattern": "key_compare",
                "cycle": 101,
                "description": "BNE/BEQ at $0812 after CMP; RAM-backed=true",
                "confidence": 0.8,
            }])
        );

        // Same shape, but the read is in IO space → not RAM-backed.
        let io_reads = [bus("mem_read", 100, 0x0810, 0xDC04, 0x2A)];
        let p2 = profile(&args(0.0, 200.0), [&steps, &io_reads, &[], &[], &[], &[], &[]]);
        assert_eq!(p2["protectionCandidates"][0]["confidence"], json!(0.5));
        assert_eq!(
            p2["protectionCandidates"][0]["description"],
            json!("BNE/BEQ at $0812 after CMP; RAM-backed=false")
        );
    }

    #[test]
    fn self_modify_needs_the_write_to_land_on_a_near_future_operand() {
        // STA $0815 at $0810; the instruction at $0814 has its operand patched.
        let steps = [step(10, 0x0810, 0x8D), step(11, 0x0814, 0xA9)];
        let writes = [bus("mem_write", 10, 0x0810, 0x0815, 0x42)];
        let p = profile(&args(0.0, 100.0), [&steps, &[], &writes, &[], &[], &[], &[]]);
        assert_eq!(
            p["protectionCandidates"],
            json!([{
                "pc": 0x0810,
                "pattern": "self_modify",
                "cycle": 10,
                "description": "STA $0815 patches operand of instruction at $0814",
                "confidence": 0.92,
            }])
        );

        // Out of the [pc+1, pc+3] band → no candidate.
        let far = [bus("mem_write", 10, 0x0810, 0x0900, 0x42)];
        let p2 = profile(&args(0.0, 100.0), [&steps, &[], &far, &[], &[], &[], &[]]);
        assert_eq!(p2["protectionCandidates"], json!([]));
    }

    #[test]
    fn checksum_loop_reports_whether_a_compare_is_present() {
        let with_cmp = [
            step(10, 0x0810, 0x49), // EOR #imm
            step(11, 0x0812, 0xC9), // CMP #imm
            step(12, 0x0814, 0xD0), // BNE
        ];
        let p = profile(&args(0.0, 100.0), [&with_cmp, &[], &[], &[], &[], &[], &[]]);
        let cands = p["protectionCandidates"].as_array().unwrap();
        assert_eq!(cands[0]["pattern"], json!("checksum_loop"));
        assert_eq!(cands[0]["confidence"], json!(0.75));
        assert_eq!(
            cands[0]["description"],
            json!("EOR/ADC loop at $0810; compare present=true")
        );
        // The BNE also produces a key_compare, ordered after it by cycle.
        assert_eq!(cands[1]["pattern"], json!("key_compare"));
        assert_eq!(cands[1]["cycle"], json!(12));

        let no_cmp = [step(10, 0x0810, 0x69), step(11, 0x0812, 0xD0)];
        let p2 = profile(&args(0.0, 100.0), [&no_cmp, &[], &[], &[], &[], &[], &[]]);
        assert_eq!(p2["protectionCandidates"][0]["confidence"], json!(0.45));
    }

    #[test]
    fn timing_check_and_vector_indirect_are_inert_by_construction() {
        // LDA abs + a following CMP, and a JMP ($xxxx) whose pointer was
        // written earlier: both WOULD fire if `getAbs` recovered an operand.
        let steps = [
            step(10, 0x0810, 0xAD), // LDA $DC04 — operand unknowable
            step(11, 0x0813, 0xC9), // CMP
            step(12, 0x0815, 0x6C), // JMP ($0314)
            step(13, 0x0818, 0x20), // JSR
        ];
        let writes = [
            bus("mem_write", 5, 0x0800, 0x0314, 0x00),
            bus("mem_write", 6, 0x0803, 0x0315, 0x08),
        ];
        let p = profile(&args(0.0, 100.0), [&steps, &[], &writes, &[], &[], &[], &[]]);
        let pats: Vec<&str> = p["protectionCandidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["pattern"].as_str().unwrap())
            .collect();
        assert!(!pats.contains(&"timing_check"), "got {pats:?}");
        assert!(!pats.contains(&"vector_indirect"), "got {pats:?}");
        assert_eq!(get_abs(&steps, 0), 0.0);
    }

    #[test]
    fn duplicates_collapse_to_the_highest_confidence_and_sort_by_cycle() {
        // The same pc reached twice: once RAM-backed (0.80), once not (0.50).
        // The kept record keeps the FIRST occurrence's position/cycle.
        let steps = [
            step(10, 0x0810, 0xC9),
            step(11, 0x0812, 0xD0), // key_compare, no RAM read → 0.50
            step(12, 0x0810, 0xC9),
            step(13, 0x0812, 0xD0), // same pc → dedupe
        ];
        let reads = [bus("mem_read", 12, 0x0810, 0x00FE, 0x2A)];
        let p = profile(&args(0.0, 100.0), [&steps, &reads, &[], &[], &[], &[], &[]]);
        let cands = p["protectionCandidates"].as_array().unwrap();
        // Both branches see the RAM read (the reference indexes by pc, not by
        // cycle), so both are 0.80 and the second is dropped.
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0]["cycle"], json!(11));
        assert_eq!(cands[0]["confidence"], json!(0.8));
    }

    // ── result shape ────────────────────────────────────────────────────────

    #[test]
    fn a_non_finite_cycle_bound_is_rejected_before_any_query() {
        // `Number(undefined)` is NaN, and the backend's `inlineParam` refuses
        // it — the sidecar fails this exact way on `{}` and on camelCase args.
        let e = assert_finite_param(f64::NAN).unwrap_err();
        assert_eq!(e.to_string(), "non-finite param: NaN");
        // A STRING bound can reach `Infinity` (`Number("Infinity")`).
        let a = ProfileArgs::from_snake(&json!({ "cycle_start": "Infinity" }));
        assert_eq!(
            assert_finite_param(a.cycle_start).unwrap_err().to_string(),
            "non-finite param: Infinity"
        );
        assert!(assert_finite_param(0.0).is_ok());
        assert!(assert_finite_param(-1.0).is_ok());
    }

    #[test]
    fn the_empty_profile_keeps_its_shape_and_drops_an_undefined_scenario_id() {
        // Reached over the wire only with FINITE bounds (see the guard above);
        // this pins the aggregator's own `jsonSafe` shape, NaN rendering
        // included.
        let a = ProfileArgs::from_snake(&json!({}));
        let p = profile(&a, [&[], &[], &[], &[], &[], &[], &[]]);
        let o = p.as_object().unwrap();
        assert!(!o.contains_key("scenarioId"), "undefined key must be dropped");
        assert_eq!(o["startCycle"], Value::Null);
        assert_eq!(o["endCycle"], Value::Null);
        assert_eq!(o["cyclesTotal"], Value::Null);
        assert_eq!(o["c64Cycles"], json!(0));
        assert_eq!(o["ioTouches"], json!([]));
        assert_eq!(o["iecActivity"]["bitTimingHistogram"], json!({}));
        assert_eq!(o["diskActivity"]["tracksVisited"], json!([]));
        assert_eq!(o["protectionCandidates"], json!([]));
    }

    #[test]
    fn integral_numbers_never_gain_a_decimal_point() {
        let steps = [step(10, 0x0810, 0xEA)];
        let p = profile(&args(1000.0, 9000.0), [&steps, &[], &[], &[], &[], &[], &[]]);
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"startCycle\":1000"), "{s}");
        assert!(s.contains("\"cyclesTotal\":8000"), "{s}");
        assert!(!s.contains(".0"), "an integral number gained a `.0`: {s}");
    }
}
