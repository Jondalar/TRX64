//! trx64-daemon — WS JSON-RPC 2.0 server on 127.0.0.1:<port>.
//!
//! The ONLY layer that knows the wire protocol. Drop-in for the Node daemon:
//! same contract, so UI + MCP tools stay byte-for-byte unchanged.
//!
//! Surface to implement (immovable — see loop/backlog.md Stage 2):
//!   session/* · debug/run|pause|continue · api/call (allowlist) · trace/* ·
//!   checkpoint/* · runtime/snapshot_tree|promote_branch · media/* · monitor/exec · ping
//!
//! Lifecycle rules: boot paused · idle-safe · opChain serialization · per-project ·
//! port-bind race arbiter (first to bind wins) · ping liveness · crash-log.

use std::{
    env,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::NullSink;
use trx64_session::{Session, TraceState};
use trx64_trace::{FrameSink, TraceChannels, TracingObserver};

mod observers;

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "trx64-daemon", version, about = "C64 headless runtime daemon")]
struct Cli {
    /// WebSocket port to listen on.
    #[arg(long, default_value = "4312")]
    port: u16,

    /// Project path (stored, not used for routing in Phase 1).
    #[arg(long, default_value = "")]
    project: String,
}

// ── JSON-RPC 2.0 wire types ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Request {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i64,
    message: String,
}

impl Response {
    fn ok(id: Value, result: Value) -> Self {
        Self { jsonrpc: "2.0", id, result: Some(result), error: None }
    }

    /// For void methods (TS returns undefined → JSON-RPC omits result key).
    fn void(id: Value) -> Self {
        Self { jsonrpc: "2.0", id, result: None, error: None }
    }

    fn err(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError { code, message: message.into() }),
        }
    }
}

// ── Breakpoint stores ─────────────────────────────────────────────────────────

/// Simple numbered breakpoint (debug/break_* methods, numeric IDs).
struct BpEntry {
    num: u32,
    pc: u16,
    #[allow(dead_code)]
    enabled: bool,
}

/// String-ID breakpoint (api/call addPcBreakpoint/listBreakpoints/removeBreakpoint).
struct ApiBpEntry {
    id: String,
    pc: u16,
    action: String,
    enabled: bool,
    hit_limit: Option<u32>,
    /// `ignore <id> <n>` — skip the first N hits (VICE semantics, mirrored into
    /// the registry observer's `ignore_left`).
    ignore_count: u32,
    /// Real hit count, copied back from the registry after each run.
    hit_count: u64,
}

struct Breakpoints {
    next_num: u32,
    entries: Vec<BpEntry>,
    api_entries: Vec<ApiBpEntry>,
}

impl Breakpoints {
    fn new() -> Self {
        Self { next_num: 1, entries: Vec::new(), api_entries: Vec::new() }
    }

    fn list_vice_json(&self) -> Value {
        json!(self.entries.iter().map(|e| json!({
            "num": e.num as u64,
            "addr": e.pc as u64
        })).collect::<Vec<_>>())
    }
}

// ── Shared state ──────────────────────────────────────────────────────────────

/// Stop reason for debug/pause.
#[derive(Clone)]
struct CtrlStop {
    reason: &'static str,
    pc: u16,
    cycles: u64,
}

/// Singleton session, kept in memory for the daemon's lifetime.
struct State {
    session: Session,
    breakpoints: Breakpoints,
    /// The breakpoint/watchpoint POLICY (cond-AST, hit/ignore, watch tables).
    /// Re-synced from `breakpoints` before each run; drives the core's debug gates.
    observers: observers::ObserverRegistry,
    /// Queued PETSCII chars for session/type (stub, count tracked only).
    #[allow(dead_code)]
    type_buffer: Vec<u8>,
    /// Monotonic controller-state counter; increments on each debug/run|pause|continue.
    ctrl_frame: u64,
    /// Last stop reason (set on pause, cleared on continue/run).
    ctrl_stop: Option<CtrlStop>,
    /// Monotonic checkpoint counter for media/ingress checkpoint IDs.
    checkpoint_counter: u64,
    /// Declarative trace definitions (Spec 708), keyed by definition id. These are
    /// opaque JSON objects validated by [`validate_trace_definition`]; the daemon
    /// stores them per-session exactly like the TS controller's `traceDefinitions`
    /// map. No core primitive — a definition is pure data until a run taps it.
    trace_definitions: std::collections::HashMap<String, Value>,
}

type SharedState = Arc<Mutex<State>>;

// ── ROM directory resolution ──────────────────────────────────────────────────

fn rom_dir() -> PathBuf {
    let root = env::var("C64RE_ROOT").unwrap_or_else(|_| {
        "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP".to_string()
    });
    PathBuf::from(root).join("resources").join("roms")
}

// ── Project root for crash log ────────────────────────────────────────────────

fn project_dir() -> PathBuf {
    env::var("C64RE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("trx64"))
}

// ── CPU-isolated run + monitor + trace helpers ────────────────────────────────

/// Default sibling `.duckdb` output path under a temp runtime dir.
fn default_trace_output(session_id: &str) -> PathBuf {
    std::env::temp_dir()
        .join("trx64-runtime")
        .join(session_id)
        .join("live.duckdb")
}

/// Run a cycle budget (= TS session/run). Instruction-stepped: execute whole
/// instructions until `clk - start >= budget`. Streams trace frames if active.
fn run_cycle_budget(session: &mut Session, budget: u64) {
    // Full VIC-ticked machine when ROMs are assembled AND we are not on the
    // chip-ISOLATED CPU-inject path. The per-cycle VIC renderer (vic_draw.rs) builds
    // the displayed frame by SWEEPING the raster, so a render scenario that injected
    // VIC registers via `wr io` (io_injected) MUST run the full machine to sweep —
    // even though that is an injection. But the cycle-exact CPU/CIA-ISOLATED gates
    // inject a program via plain `wr` (injected, NOT io_injected) and must stay on
    // the CPU-only path so VIC badline steals don't perturb their cycle counts.
    let full_machine =
        session.machine.full_assembled && (!session.injected || session.io_injected);

    let Some((channels, need_header, meta_json)) = session.trace.as_ref().map(|t| {
        (TraceChannels::from_domains(&t.domains), t.buf.is_empty(), t.meta_json.clone())
    }) else {
        // No active trace: run untraced.
        let mut obs = NullSink;
        if full_machine {
            session.machine.run_for_full(budget, &mut obs, |_, _, _, _, _, _, _| {});
        } else {
            session.machine.run_for(budget, &mut obs);
        }
        return;
    };
    // First run after start: write the file header into the buffer.
    if need_header {
        if let Some(t) = session.trace.as_mut() {
            t.buf = FrameSink::with_header(&meta_json).buf;
        }
    }
    let vic_active = channels.vic;
    let drive_cpu_active = channels.drive_cpu;

    // Accumulate events from this run, then append to the persistent buffer.
    let mut obs = TracingObserver::with_channels(FrameSink::events_only(), channels);

    // TRACE_DRAIN chunking (= TS ws-server.ts session/run): when a trace is
    // active AND the budget exceeds 100k cycles, the golden runs the budget in
    // 100k-cycle SEGMENTS (producer-side backpressure for the trace worker). Each
    // segment is a separate `runFor` whose `clk - start >= seg` break resets per
    // segment, so each segment overshoots by up to one instruction and the
    // overshoot ACCUMULATES across segments. A single-pass run would overshoot
    // only once, ending a few drive cycles short — diverging from the golden at
    // the run tail (drive-boot-deep: ~8 trailing sampled records). Match the
    // golden by replaying the same 100k segmentation here.
    const TRACE_DRAIN_CYCLES: u64 = 100_000;
    let mut remaining = budget;
    while remaining != 0 {
        let seg = remaining.min(TRACE_DRAIN_CYCLES);
        remaining -= seg;

        if full_machine {
            let mut steps: Vec<(u16, u8, u8, u8, u8, u8, u64)> = Vec::new();
            session.machine.run_for_full(seg, &mut obs, |pc, a, x, y, sp, p, drv_clk| {
                steps.push((pc, a, x, y, sp, p, drv_clk));
            });
            if drive_cpu_active {
                for (pc, a, x, y, sp, p, drv_clk) in steps {
                    obs.emit_drive_step(pc, a, x, y, sp, p, drv_clk);
                }
            }
        } else if drive_cpu_active {
            let mut steps: Vec<(u16, u8, u8, u8, u8, u8, u64)> = Vec::new();
            session.machine.run_for_drive_sampled(seg, &mut obs, |pc, a, x, y, sp, p, drv_clk| {
                steps.push((pc, a, x, y, sp, p, drv_clk));
            });
            for (pc, a, x, y, sp, p, drv_clk) in steps {
                obs.emit_drive_step(pc, a, x, y, sp, p, drv_clk);
            }
        } else if vic_active {
            session.machine.run_for_vic(seg, &mut obs);
        } else if channels.sid {
            // SID isolation gate: routes $D400-$D7FF to the SID 6581 model.
            // The `sid` domain has NO live trace producer (reserved, like vic —
            // ADR-015 pattern); SID writes appear as op-0x11 RAM_WRITE from the
            // CPU bus tap. The cpu/memory channels are co-enabled by `sid` domain.
            session.machine.run_for_sid(seg, &mut obs);
        } else if channels.mem {
            session.machine.run_for_cia(seg, &mut obs);
        } else {
            session.machine.run_for_with(seg, &mut obs);
        }
    }
    if let Some(t) = session.trace.as_mut() {
        t.event_count += obs.event_count;
        t.buf.extend_from_slice(&obs.into_buf());
    }
}

/// Step exactly one instruction (for stepInto / stepOver / until loops).
fn step_one_instruction(session: &mut Session) {
    // Full VIC-ticked machine when ROMs are assembled AND we are not on the
    // chip-ISOLATED CPU-inject path. The per-cycle VIC renderer (vic_draw.rs) builds
    // the displayed frame by SWEEPING the raster, so a render scenario that injected
    // VIC registers via `wr io` (io_injected) MUST run the full machine to sweep —
    // even though that is an injection. But the cycle-exact CPU/CIA-ISOLATED gates
    // inject a program via plain `wr` (injected, NOT io_injected) and must stay on
    // the CPU-only path so VIC badline steals don't perturb their cycle counts.
    let full_machine =
        session.machine.full_assembled && (!session.injected || session.io_injected);
    let mut obs = NullSink;
    if full_machine {
        session.machine.run_for_full_capped(999_999, 1, &mut obs, |_, _, _, _, _, _, _| {});
    } else {
        session.machine.run_for_capped(999_999, 1, &mut obs);
    }
}

/// The result of a breakpoint/watchpoint-gated run ([`run_until_break`]).
struct BreakRun {
    /// True if a break/watchpoint actually halted the run (vs budget exhaustion).
    halted: bool,
    /// Stop reason matching `RuntimeStopInfo.reason` (types.ts): "breakpoint"
    /// for an exec hit, "observer" for a load/store watchpoint hit.
    reason: &'static str,
    /// The observer name that fired (for breakpointId resolution).
    which: Option<String>,
    pc: u16,
    cycles_elapsed: u64,
}

/// Whether the current bp surface needs the breakpoint/observer driver at all.
fn observers_armed(reg: &observers::ObserverRegistry) -> bool {
    reg.exec_active || reg.access_armed()
}

/// Re-sync the [`ObserverRegistry`] from the daemon's breakpoint surfaces
/// (`api_entries` string-ids + numbered `entries`), preserving each observer's
/// accumulated `hits` / remaining `ignore_left`. The registry is the run-time
/// SOURCE OF TRUTH the core's debug gates consult; the bp lists are the wire-shape
/// CRUD store. After a run, [`writeback_hits`] copies the real hit counts back.
fn sync_observers(bp: &Breakpoints, reg: &mut observers::ObserverRegistry) {
    // Snapshot current live counts so a rebuild doesn't reset them.
    let prior: std::collections::HashMap<String, (u64, u64)> = reg
        .list()
        .iter()
        .map(|o| (o.name.clone(), (o.hits, o.ignore_left)))
        .collect();
    reg.clear();
    // String-id breakpoints (addPcBreakpoint / mem watchpoints).
    for e in &bp.api_entries {
        if !e.enabled {
            continue;
        }
        let (trigger, lo, hi, cond_src) = parse_api_bp(e);
        let action = if e.action == "log" {
            observers::ObsAction::Log
        } else {
            observers::ObsAction::Break
        };
        let _ = reg.add(observers::ObsSpec {
            name: e.id.clone(),
            trigger,
            lo,
            hi,
            cond_src,
            action,
            log_exprs: None,
            cmd_src: None,
            mark_label: None,
            trace_scope: None,
        });
        // Restore live counts (default: fresh hits=0, ignore_left=ignore_count).
        let (hits, ignore_left) = prior
            .get(&e.id)
            .copied()
            .unwrap_or((e.hit_count, e.ignore_count as u64));
        reg.set_counts(&e.id, hits, ignore_left);
    }
    // Numbered exec breakpoints (debug/break_add).
    for e in &bp.entries {
        if !e.enabled {
            continue;
        }
        let name = format!("bp#{}", e.num);
        let _ = reg.add(observers::ObsSpec {
            name: name.clone(),
            trigger: observers::ObsTrigger::Exec,
            lo: e.pc,
            hi: e.pc,
            cond_src: None,
            action: observers::ObsAction::Break,
            log_exprs: None,
            cmd_src: None,
            mark_label: None,
            trace_scope: None,
        });
        let (hits, ignore_left) = prior.get(&name).copied().unwrap_or((0, 0));
        reg.set_counts(&name, hits, ignore_left);
    }
}

/// Decode an [`ApiBpEntry`] into an observer trigger/range/cond. The `action`
/// field overloads as the watchpoint kind: "watch_read"/"watch_write"/"watch"
/// arm load/store observers; an `action` of the form "cond:<expr>" carries a
/// raw condition (the daemon's compact way to express a conditional bp over the
/// existing wire). Default = an exec breakpoint at the single PC.
fn parse_api_bp(e: &ApiBpEntry) -> (observers::ObsTrigger, u16, u16, Option<String>) {
    if let Some(expr) = e.action.strip_prefix("cond:") {
        return (
            observers::ObsTrigger::Exec,
            e.pc,
            e.pc,
            Some(expr.to_string()),
        );
    }
    match e.action.as_str() {
        "watch_read" | "load" => (observers::ObsTrigger::Load, e.pc, e.pc, None),
        "watch_write" | "store" => (observers::ObsTrigger::Store, e.pc, e.pc, None),
        "watch" => {
            // A read+write watch can't be one observer (single trigger); model it as
            // a store watch (the common debugging case). A separate load observer can
            // be added with action "watch_read" if needed.
            (observers::ObsTrigger::Store, e.pc, e.pc, None)
        }
        _ => (observers::ObsTrigger::Exec, e.pc, e.pc, None),
    }
}

/// Copy the real hit counts back from the registry into the daemon's bp surface
/// after a run, so `listBreakpoints` / `debug/break_list` report the true counts.
fn writeback_hits(bp: &mut Breakpoints, reg: &observers::ObserverRegistry) {
    for e in bp.api_entries.iter_mut() {
        if let Some(o) = reg.get(&e.id) {
            e.hit_count = o.hits;
        }
    }
}

/// Default cycle budget for a synchronous breakpoint-gated run (the daemon is
/// request/response; a real autonomous loop would be unbounded, so we cap at a
/// generous ~10 frames of PAL cycles — enough to reach any boot-time bp).
const DEBUG_RUN_BUDGET: u64 = 10_000_000;

/// Drive `debug/run` / `debug/continue`. When breakpoints/watchpoints are armed,
/// SEGMENT-RUN the machine until one trips (or the budget exhausts) and return the
/// real stop info. When none are armed, preserve the historical immediate
/// `running` return (no advance) so the zero-cost / no-debug contract is unchanged.
fn run_debug_control(id: Value, st: &mut State, frame: u64, is_continue: bool) -> Response {
    {
        let State { breakpoints, observers: reg, .. } = &mut *st;
        sync_observers(breakpoints, reg);
    }

    if !observers_armed(&st.observers) {
        // No debug gate: historical behavior — report running, machine unchanged.
        let bps = st.breakpoints.list_vice_json();
        let pc = st.session.machine.c64_core.reg_pc as u64;
        let cycles = st.session.machine.clk;
        return Response::ok(id, json!({
            "runState": "running",
            "pacing": { "mode": "pal", "ratio": 1 },
            "pc": pc,
            "cycles": cycles,
            "frame": frame,
            "breakpoints": bps,
            "stop": null,
            "controlOwner": "llm"
        }));
    }

    // Continuing FROM a breakpoint: advance one instruction past the current PC
    // first, so the boundary check doesn't immediately re-trip the same bp.
    if is_continue {
        step_one_instruction(&mut st.session);
    }

    // Split the borrow of `st` so the registry can be passed as the core observer
    // while the session runs; scope it so the fields free up afterward.
    let run = {
        let State { session, observers: reg, .. } = &mut *st;
        run_until_break(session, reg, DEBUG_RUN_BUDGET)
    };
    {
        let State { breakpoints, observers: reg, .. } = &mut *st;
        writeback_hits(breakpoints, reg);
    }

    let bps = st.breakpoints.list_vice_json();
    let cycles = st.session.machine.clk;
    if run.halted {
        st.session.running = false;
        // Resolve a numeric breakpointId from the numbered bp store by PC, if any.
        let bp_num = st
            .breakpoints
            .entries
            .iter()
            .find(|e| e.pc == run.pc)
            .map(|e| e.num);
        st.ctrl_stop = Some(CtrlStop { reason: "breakpoint", pc: run.pc, cycles });
        let mut stop = json!({
            "reason": run.reason,
            "pc": run.pc as u64,
            "cycles": cycles,
        });
        if let Some(n) = bp_num {
            stop["breakpointId"] = json!(n as u64);
        }
        if let Some(name) = run.which {
            stop["breakpoint"] = json!(name);
        }
        Response::ok(id, json!({
            "runState": "paused",
            "pacing": { "mode": "pal", "ratio": 1 },
            "pc": run.pc as u64,
            "cycles": cycles,
            "frame": frame,
            "breakpoints": bps,
            "stop": stop,
            "controlOwner": "llm"
        }))
    } else {
        // Budget exhausted without a hit: the machine advanced; report running.
        let pc = st.session.machine.c64_core.reg_pc as u64;
        Response::ok(id, json!({
            "runState": "running",
            "pacing": { "mode": "pal", "ratio": 1 },
            "pc": pc,
            "cycles": cycles,
            "frame": frame,
            "breakpoints": bps,
            "stop": null,
            "controlOwner": "llm"
        }))
    }
}

/// SEGMENT-RUN the machine with the registry driving the core's debug gates,
/// self-halting at the first REAL breakpoint/watchpoint (cond true + not ignored).
///
/// 1:1 with the c64re run model: the exec breakpoint SET is armed in the core
/// (halts AT the PC before execute, VICE break-on-exec); the registry's `on_exec`
/// then applies the cond + ignore-count + hit-count gate, and on a non-match the
/// driver steps ONE instruction past the PC and resumes (so a conditional bp that
/// evaluates false does not wedge). Load/store watchpoints arm the core's
/// `access_watch` table; the registry's `on_access` sets `halt_requested`, honored
/// at the next boundary (RunStop::Observer).
fn run_until_break(
    session: &mut Session,
    reg: &mut observers::ObserverRegistry,
    cycle_budget: u64,
) -> BreakRun {
    let full_machine =
        session.machine.full_assembled && (!session.injected || session.io_injected);
    let start_clk = session.machine.clk;
    reg.clear_halt();

    let bp_set = reg.exec_breakpoint_set();
    // An access observer with a condition needs an exact per-instruction env
    // (the cond may read a/x/y/pc). Single-step those segments so the env the
    // registry sees at on_access time is the at-access CPU state; unconditional
    // watchpoints (the common case) run in full segments.
    let access_needs_step = reg
        .list()
        .iter()
        .any(|o| o.enabled && o.trigger != observers::ObsTrigger::Exec && o.cond.is_some());
    let seg_cap: u64 = if access_needs_step { 1 } else { u64::MAX };

    loop {
        let elapsed = session.machine.clk.wrapping_sub(start_clk);
        if elapsed >= cycle_budget {
            return BreakRun {
                halted: false,
                reason: "budget",
                which: None,
                pc: session.machine.c64_core.reg_pc,
                cycles_elapsed: elapsed,
            };
        }
        let seg_budget = (cycle_budget - elapsed).min(if seg_cap == u64::MAX {
            cycle_budget
        } else {
            seg_cap.max(1)
        });
        let max_instr = if seg_cap == 1 { 1 } else { seg_budget.div_ceil(2) + 1000 };

        // Refresh the env from the current (segment-start) CPU + raster state so
        // exec/access conditions eval against it.
        reg.set_env(observers::CpuSnapshot::from_machine(&session.machine));

        let access_watch = reg.access_watch_owned();
        let aw_ref = access_watch.as_deref();
        let bp_ref = bp_set.as_ref();

        let stop = if full_machine {
            session.machine.run_for_full_capped_dbg(
                seg_budget,
                max_instr,
                bp_ref,
                None,
                aw_ref,
                reg,
                |_, _, _, _, _, _, _| {},
            )
        } else {
            // CPU-isolated path (no full machine). The dbg entry point lives on the
            // full SC path only; for the isolated path we step + check the bp set
            // manually so isolated gates still get exec breakpoints.
            run_isolated_segment(&mut session.machine, bp_ref, max_instr)
        };

        match stop {
            trx64_core::RunStop::Breakpoint(pc) => {
                // Core halted AT pc, before executing it. Apply the cond/ignore gate.
                reg.set_env(observers::CpuSnapshot::from_machine(&session.machine));
                let real = reg.on_exec(pc);
                if real {
                    let which = reg.last_halt.as_ref().map(|h| h.name.clone());
                    return BreakRun {
                        halted: true,
                        reason: "breakpoint",
                        which,
                        pc,
                        cycles_elapsed: session.machine.clk.wrapping_sub(start_clk),
                    };
                }
                // Cond false or ignored: step one instruction PAST the bp PC so the
                // boundary check doesn't re-trip on the same PC, then resume.
                step_one_instruction(session);
            }
            trx64_core::RunStop::Observer => {
                // A watchpoint requested the halt during the last instruction.
                let which = reg.last_halt.as_ref().map(|h| h.name.clone());
                let pc = session.machine.c64_core.reg_pc;
                return BreakRun {
                    halted: true,
                    reason: "observer",
                    which,
                    pc,
                    cycles_elapsed: session.machine.clk.wrapping_sub(start_clk),
                };
            }
            trx64_core::RunStop::CycleBudget | trx64_core::RunStop::Completed => {
                // Segment finished without a hit; loop re-checks the total budget.
                if seg_cap != u64::MAX && session.machine.clk == start_clk {
                    // Defensive: a 0-cycle segment (shouldn't happen) — bail.
                    return BreakRun {
                        halted: false,
                        reason: "budget",
                        which: None,
                        pc: session.machine.c64_core.reg_pc,
                        cycles_elapsed: 0,
                    };
                }
            }
        }
    }
}

/// CPU-isolated exec-breakpoint segment (the full SC dbg entry point is full-machine
/// only). Steps single instructions, checking the bp set BEFORE each — matching the
/// full path's break-AT-pc-before-execute semantics. Watchpoints are not supported
/// on the isolated path (no bus gate there); only the exec bp set is honored.
fn run_isolated_segment(
    machine: &mut trx64_core::Machine,
    bp_set: Option<&std::collections::HashSet<u16>>,
    max_instr: u64,
) -> trx64_core::RunStop {
    let mut obs = NullSink;
    let mut executed = 0u64;
    loop {
        if executed >= max_instr {
            return trx64_core::RunStop::CycleBudget;
        }
        let pc = machine.cpu6510.reg_pc;
        if let Some(bps) = bp_set {
            if bps.contains(&pc) {
                return trx64_core::RunStop::Breakpoint(pc);
            }
        }
        machine.run_for_capped(999_999, 1, &mut obs);
        executed += 1;
    }
}

/// Minimal VICE-style monitor: supports `wr [lens] <addr> <bytes..>`, `r`,
/// `r reg=val ...`. Enough to inject a program + set PC, CPU-isolated.
fn run_monitor(session: &mut Session, command: &str) -> Result<String, String> {
    let toks: Vec<&str> = command.split_whitespace().collect();
    if toks.is_empty() {
        return Ok(String::new());
    }
    let op = toks[0].to_ascii_lowercase();
    match op.as_str() {
        "wr" => {
            let mut i = 1;
            // Optional lens: `io` routes the write through the I/O space (VIC /
            // SID / colour-RAM / CIA) instead of flat RAM. `cpu`/`ram` = RAM.
            let io_lens = matches!(toks.get(i), Some(&"io"));
            if matches!(toks.get(i), Some(&("cpu" | "ram" | "io"))) {
                i += 1;
            }
            let addr = parse_hex(toks.get(i).copied().ok_or("wr: missing addr")?)
                .ok_or("wr: bad addr")? as u16;
            i += 1;
            let bytes: Result<Vec<u8>, String> = toks[i..]
                .iter()
                .map(|t| parse_hex(t).map(|v| v as u8).ok_or_else(|| format!("wr: bad byte {t}")))
                .collect();
            let bytes = bytes?;
            if bytes.is_empty() {
                return Err("wr: need >=1 byte value ($00-$FF)".into());
            }
            if io_lens {
                session.machine.poke_io(addr, &bytes);
                // I/O-lens inject = a render scenario programming the VIC/colour-RAM;
                // it still needs the full VIC-ticked machine to sweep the per-cycle
                // frame, so flag io_injected (which keeps the full-machine run path)
                // rather than `injected` (which would route to the chip-isolated bus).
                session.io_injected = true;
            } else {
                session.machine.poke(addr, &bytes);
                session.injected = true;
            }
            let lens = if io_lens { "io" } else { "cpu" };
            Ok(format!("wrote {} byte(s) @ ${:04X} ({lens})", bytes.len(), addr))
        }
        "r" | "registers" => {
            let sets: Vec<&str> = toks[1..].iter().copied().filter(|t| t.contains('=')).collect();
            if !sets.is_empty() {
                let mut done = Vec::new();
                for pair in sets {
                    let mut it = pair.splitn(2, '=');
                    let reg = it.next().unwrap_or("").to_ascii_lowercase();
                    let val_s = it.next().unwrap_or("");
                    let v = match parse_hex(val_s) {
                        Some(v) => v,
                        None => {
                            done.push(format!("bad {pair}"));
                            continue;
                        }
                    };
                    let c = &mut session.machine.cpu6510;
                    match reg.as_str() {
                        "a" | "ac" => { c.reg_a = v as u8; done.push(format!("a=${:02X}", v as u8)); }
                        "x" | "xr" => { c.reg_x = v as u8; done.push(format!("x=${:02X}", v as u8)); }
                        "y" | "yr" => { c.reg_y = v as u8; done.push(format!("y=${:02X}", v as u8)); }
                        "sp" => { c.reg_sp = v as u8; done.push(format!("sp=${:02X}", v as u8)); }
                        "pc" => { c.reg_pc = v as u16; done.push(format!("pc=${:04X}", v as u16)); }
                        "p" | "fl" | "flags" => {
                            c.reg_p = (v as u8) & !0xa2;
                            c.flag_n = (v as u8) & 0x80;
                            c.flag_z = if (v as u8) & 0x02 != 0 { 0 } else { 1 };
                            done.push(format!("fl=${:02X}", v as u8));
                        }
                        _ => done.push(format!("unknown reg '{reg}'")),
                    }
                }
                session.machine.sync_after_monitor();
                session.injected = true;
                Ok(format!("set {}", done.join(" ")))
            } else {
                let c = &session.machine.cpu6510;
                Ok(format!(
                    "  ADDR AC XR YR SP NV-BDIZC\n.;{:04X} {:02X} {:02X} {:02X} {:02X} {:02X}",
                    c.reg_pc, c.reg_a, c.reg_x, c.reg_y, c.reg_sp, c.flags()
                ))
            }
        }
        "m" | "mb" | "mem" => {
            // `m [lens] <addr_lo> [<addr_hi>]` — memory dump.
            //
            // TS monitor-shell.ts format: rows of 32 bytes starting at
            // (addr & ~0x1f), hex section padEnd(96), then "  " + ascii.
            //   ">C:XXXX  HH HH HH...   ...."
            //   row starts at (start & ~0x1f); a row shows up to 32 bytes.
            let mut i = 1;
            // Skip optional lens token (cpu/ram/rom/io).
            if matches!(toks.get(i), Some(&("cpu" | "ram" | "rom" | "io" | "cart"))) {
                i += 1;
            }
            let addr_lo = parse_hex(toks.get(i).copied().ok_or("m: missing addr")?)
                .ok_or("m: bad addr")? as u16;
            i += 1;
            let addr_hi = toks
                .get(i)
                .and_then(|t| parse_hex(t))
                .map(|v| v as u16)
                .unwrap_or(addr_lo);
            let row_start = addr_lo & !0x1f_u16; // 32-byte aligned row
            // Build one row (may be partial if addr_hi < row_start+31).
            let mut hex_bytes: Vec<String> = Vec::new();
            let mut ascii = String::new();
            let mut col = 0u32;
            let mut a = row_start;
            loop {
                if a >= addr_lo && a <= addr_hi {
                    let b = session.machine.ram[a as usize];
                    hex_bytes.push(format!("{:02X}", b));
                    ascii.push(if b >= 0x20 && b < 0x7f { b as char } else { '.' });
                } else {
                    // Before start or after end in the row window: skip (don't show).
                    // TS does: for j in 0..32 { if a+j <= end { push byte } }
                    // so we only push bytes that are within [start, end].
                    // But for bytes in [row_start, addr_lo) they are NOT pushed.
                    // The padEnd(96) pads the WHOLE row slot, so missing leading bytes
                    // also consume their 3-char slot (they appear as spaces).
                    // However, when addr_lo is row-aligned (e.g. 0x0200 = 0x0200 & ~0x1f)
                    // there are no pre-bytes. Handle gracefully by inserting empty.
                }
                col += 1;
                if col >= 32 || a == addr_hi { break; }
                if a == 0xffff { break; }
                a = a.wrapping_add(1);
            }
            // For partial rows, we only push the bytes in [addr_lo..=addr_hi].
            // The TS format pads `bytes.join(" ")` to 96 chars regardless.
            let hex_str = hex_bytes.join(" ");
            // Pad to exactly 96 chars (32×3 = 96).
            let hex_padded = format!("{:<96}", hex_str);
            Ok(format!(">C:{:04X}  {}  {}", row_start, hex_padded, ascii))
        }
        _ => Err(format!("monitor: unsupported command '{op}'")),
    }
}

/// Parse a hex token (optional leading `$`).
fn parse_hex(tok: &str) -> Option<u32> {
    let t = tok.strip_prefix('$').unwrap_or(tok);
    u32::from_str_radix(t, 16).ok()
}

/// Flush the active trace to its `.c64retrace` path; returns (run, status) JSON.
fn finalize_trace(session: &mut Session) -> (Value, Value) {
    match session.trace.take() {
        None => (Value::Null, json!({ "active": false })),
        Some(t) => {
            let bytes = if t.buf.is_empty() {
                FrameSink::with_header(&t.meta_json).buf
            } else {
                t.buf
            };
            if let Some(parent) = t.retrace_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let bytes_written = bytes.len();
            let _ = std::fs::write(&t.retrace_path, &bytes);
            (
                json!({
                    "runId": t.run_id,
                    "definitionId": "live-capture",
                    "eventCount": t.event_count,
                    "bytesWritten": bytes_written,
                }),
                json!({ "active": false, "binary": true }),
            )
        }
    }
}

// ── Spec 708 trace-definition validation (1:1 port of trace-definition.ts) ─────

/// Domains the validator accepts (= TS `DOMAINS`).
const TRACE_DOMAINS: &[&str] =
    &["c64-cpu", "drive8-cpu", "iec", "vic", "sid", "memory"];

/// A 0..=0xFFFF integer check (= TS `u16`).
fn is_u16(v: &Value) -> bool {
    matches!(v.as_i64(), Some(n) if (0..=0xffff).contains(&n)) && v.is_i64() == v.as_i64().is_some()
}

/// 1:1 port of `validateTraceDefinition` (trace-definition.ts:73). Pure; returns
/// the full error list (no throw). Result shape `{ ok, errors }` matches the TS.
fn validate_trace_definition(def: &Value) -> (bool, Vec<String>) {
    let mut e: Vec<String> = Vec::new();
    if !def.is_object() {
        return (false, vec!["definition is not an object".into()]);
    }
    let get = |k: &str| def.get(k);

    match get("id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => {}
        _ => e.push("id: required non-empty string".into()),
    }
    match get("version") {
        Some(v) if v.is_i64() && v.as_i64().map(|n| n >= 1).unwrap_or(false) => {}
        _ => e.push("version: integer >= 1".into()),
    }
    match get("name").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => {}
        _ => e.push("name: required non-empty string".into()),
    }

    let domains = get("domains").and_then(|v| v.as_array());
    match domains {
        Some(arr) if !arr.is_empty() => {
            for d in arr {
                if let Some(s) = d.as_str() {
                    if !TRACE_DOMAINS.contains(&s) {
                        e.push(format!("domains: unknown \"{s}\""));
                    }
                } else {
                    e.push(format!("domains: unknown \"{d}\""));
                }
            }
        }
        _ => e.push("domains: at least one".into()),
    }

    let triggers = get("triggers").and_then(|v| v.as_array());
    match triggers {
        Some(arr) if !arr.is_empty() => {
            for (i, t) in arr.iter().enumerate() {
                e.extend(validate_trace_trigger(t, i));
            }
        }
        _ => e.push("triggers: at least one".into()),
    }

    let captures = get("captures").and_then(|v| v.as_array());
    match captures {
        Some(arr) if !arr.is_empty() => {
            for (i, c) in arr.iter().enumerate() {
                e.extend(validate_trace_capture(c, i));
            }
        }
        _ => e.push("captures: at least one".into()),
    }

    match get("retention").and_then(|v| v.as_str()) {
        Some("transient") | Some("evidence") => {}
        _ => e.push("retention: \"transient\" | \"evidence\"".into()),
    }

    if let Some(cp) = get("checkpointPolicy") {
        if !cp.is_null() {
            match cp.as_str() {
                Some("on-trigger") => e.push(
                    "checkpointPolicy: \"on-trigger\" not yet supported — use \"at-start\" or \"at-stop\""
                        .into(),
                ),
                Some("none") | Some("at-start") | Some("at-stop") => {}
                _ => e.push("checkpointPolicy: none | at-start | at-stop".into()),
            }
        }
    }

    // §708.7 coverage: every capture/trigger that needs a domain must declare it.
    if let (Some(doms), Some(caps)) = (domains, captures) {
        let dset: std::collections::HashSet<&str> =
            doms.iter().filter_map(|v| v.as_str()).collect();
        for (i, c) in caps.iter().enumerate() {
            if let Some(need) = capture_requires_domain(c) {
                if !dset.contains(need) {
                    let kind = c.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                    e.push(format!(
                        "captures[{i}]: \"{kind}\" requires domain \"{need}\" in domains"
                    ));
                }
            }
        }
    }
    if let (Some(doms), Some(trigs)) = (domains, triggers) {
        let dset: std::collections::HashSet<&str> =
            doms.iter().filter_map(|v| v.as_str()).collect();
        for (i, t) in trigs.iter().enumerate() {
            if let Some(need) = trigger_requires_domain(t) {
                if !dset.contains(need) {
                    let kind = t.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                    e.push(format!(
                        "triggers[{i}]: \"{kind}\" requires domain \"{need}\" in domains"
                    ));
                }
            }
        }
    }

    if let Some(stop) = get("stop") {
        if !stop.is_null() {
            let kind = stop.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if !["cycle-budget", "event-count", "manual"].contains(&kind) {
                e.push("stop.kind invalid".into());
            }
            if (kind == "cycle-budget" || kind == "event-count")
                && !matches!(stop.get("value").and_then(|v| v.as_f64()), Some(n) if n > 0.0)
            {
                e.push(format!("stop.value: positive number for {kind}"));
            }
        }
    }

    (e.is_empty(), e)
}

/// 1:1 port of `validateTrigger` (trace-definition.ts:126).
fn validate_trace_trigger(t: &Value, i: usize) -> Vec<String> {
    let p = format!("triggers[{i}]");
    let kind = t.get("kind").and_then(|v| v.as_str());
    match kind {
        Some("pc-range") => {
            let mut out = Vec::new();
            let dom = t.get("domain").and_then(|v| v.as_str());
            if dom != Some("c64-cpu") && dom != Some("drive8-cpu") {
                out.push(format!("{p}.domain: c64-cpu | drive8-cpu"));
            }
            let from = t.get("from");
            let to = t.get("to");
            let ok = from.map(is_u16).unwrap_or(false)
                && to.map(is_u16).unwrap_or(false)
                && from.and_then(|f| f.as_i64()) <= to.and_then(|tv| tv.as_i64());
            if !ok {
                out.push(format!("{p}: from/to must be 0..$FFFF with from<=to"));
            }
            out
        }
        Some("mem-access") => {
            let mut out = Vec::new();
            let access = t.get("access").and_then(|v| v.as_str()).unwrap_or("");
            if !["read", "write", "any"].contains(&access) {
                out.push(format!("{p}.access: read | write | any"));
            }
            let from = t.get("from");
            let to = t.get("to");
            let ok = from.map(is_u16).unwrap_or(false)
                && to.map(is_u16).unwrap_or(false)
                && from.and_then(|f| f.as_i64()) <= to.and_then(|tv| tv.as_i64());
            if !ok {
                out.push(format!("{p}: from/to must be 0..$FFFF with from<=to"));
            }
            out
        }
        Some("iec-transition") => {
            let line = t.get("line");
            match line.and_then(|v| v.as_str()) {
                None => vec![],
                Some(l) if ["atn", "clk", "data"].contains(&l) => vec![],
                _ if line.map(|v| v.is_null()).unwrap_or(true) => vec![],
                _ => vec![format!("{p}.line: atn | clk | data")],
            }
        }
        Some("raster-window") => {
            let from = t.get("fromLine").and_then(|v| v.as_i64());
            let to = t.get("toLine").and_then(|v| v.as_i64());
            if matches!((from, to), (Some(f), Some(tv)) if f <= tv) {
                vec![]
            } else {
                vec![format!("{p}: fromLine<=toLine integers")]
            }
        }
        Some("monitor-stop") => vec![format!(
            "{p}: \"monitor-stop\" trigger not supported — no runtime event semantics; use pc-range / mem-access / raster-window"
        )],
        Some("manual-mark") => vec![format!(
            "{p}: \"manual-mark\" trigger not supported — record marks via trace/run/mark, not as a capture trigger"
        )],
        other => vec![format!(
            "{p}: unknown trigger kind \"{}\"",
            other.unwrap_or("")
        )],
    }
}

/// 1:1 port of `validateCapture` (trace-definition.ts:155).
fn validate_trace_capture(c: &Value, i: usize) -> Vec<String> {
    let p = format!("captures[{i}]");
    match c.get("kind").and_then(|v| v.as_str()) {
        Some("cpu-row") => {
            let dom = c.get("domain").and_then(|v| v.as_str());
            if dom == Some("c64-cpu") || dom == Some("drive8-cpu") {
                vec![]
            } else {
                vec![format!("{p}.domain: c64-cpu | drive8-cpu")]
            }
        }
        Some("mem-row") | Some("iec-row") | Some("vic-row") | Some("checkpoint-ref") => vec![],
        other => vec![format!("{p}: unknown capture kind \"{}\"", other.unwrap_or(""))],
    }
}

/// 1:1 port of `captureRequiresDomain` (trace-definition.ts:169).
fn capture_requires_domain(c: &Value) -> Option<&'static str> {
    match c.get("kind").and_then(|v| v.as_str()) {
        Some("cpu-row") => Some(
            if c.get("domain").and_then(|v| v.as_str()) == Some("drive8-cpu") {
                "drive8-cpu"
            } else {
                "c64-cpu"
            },
        ),
        Some("mem-row") => Some("memory"),
        Some("iec-row") => Some("iec"),
        Some("vic-row") => Some("vic"),
        _ => None,
    }
}

/// 1:1 port of `triggerRequiresDomain` (trace-definition.ts:181).
fn trigger_requires_domain(t: &Value) -> Option<&'static str> {
    match t.get("kind").and_then(|v| v.as_str()) {
        Some("pc-range") => Some(
            if t.get("domain").and_then(|v| v.as_str()) == Some("drive8-cpu") {
                "drive8-cpu"
            } else {
                "c64-cpu"
            },
        ),
        Some("mem-access") => Some("memory"),
        Some("iec-transition") => Some("iec"),
        Some("raster-window") => Some("vic"),
        _ => None,
    }
}

/// 1:1 port of `slugTraceId` (trace-definition.ts:192): kebab-case from a name.
fn slug_trace_id(name: &str) -> String {
    let lower = name.to_lowercase();
    // Collapse any run of non-[a-z0-9] into a single '-'.
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in lower.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            slug.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(48).collect();
    if slug.is_empty() {
        // TS: `trace-${Date.now().toString(36)}`.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("trace-{}", radix36(now))
    } else {
        slug
    }
}

/// base-36 of a u128 (= JS `Number.toString(36)`), lowercase.
fn radix36(mut n: u128) -> String {
    if n == 0 {
        return "0".into();
    }
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

// ── 6502 disassembler ─────────────────────────────────────────────────────────

fn instr_len(opcode: u8) -> usize {
    use trx64_core::tables::{MICROCODE_TABLE, UNDOC_TABLE};
    let mode = MICROCODE_TABLE[opcode as usize]
        .map(|e| e.mode)
        .or_else(|| UNDOC_TABLE[opcode as usize].map(|e| e.mode));
    match mode {
        Some("imp") | Some("acc") => 1,
        Some("imm") | Some("zp") | Some("zpx") | Some("zpy")
        | Some("indx") | Some("indy") | Some("rel") => 2,
        Some("abs") | Some("absx") | Some("absy") | Some("ind") => 3,
        _ => 1, // Unknown/JAM: treat as 1-byte
    }
}

fn disasm_one(addr: u16, read: impl Fn(u16) -> u8) -> Value {
    use trx64_core::tables::{MICROCODE_TABLE, UNDOC_TABLE};

    let opcode = read(addr);
    let len = instr_len(opcode);
    let bytes: Vec<u8> = (0..len as u16).map(|i| read(addr.wrapping_add(i))).collect();
    let b1 = bytes.get(1).copied().unwrap_or(0);
    let b2 = bytes.get(2).copied().unwrap_or(0);

    let (mne, mode) = MICROCODE_TABLE[opcode as usize]
        .map(|e| (e.op.to_uppercase(), e.mode))
        .or_else(|| UNDOC_TABLE[opcode as usize].map(|e| (e.kind.to_uppercase(), e.mode)))
        .unwrap_or_else(|| (format!(".byte ${:02X}", opcode), "imp"));

    let operand = match mode {
        "imp" | "acc" => String::new(),
        "imm" => format!("#${:02X}", b1),
        "zp" => format!("${:02X}", b1),
        "zpx" => format!("${:02X},X", b1),
        "zpy" => format!("${:02X},Y", b1),
        "rel" => {
            let off = b1 as i8 as i32;
            let target = (addr as i32 + 2 + off) as u16;
            format!("${:04X}", target)
        }
        "abs" => format!("${:04X}", (b1 as u16) | ((b2 as u16) << 8)),
        "absx" => format!("${:04X},X", (b1 as u16) | ((b2 as u16) << 8)),
        "absy" => format!("${:04X},Y", (b1 as u16) | ((b2 as u16) << 8)),
        "ind" => format!("(${:04X})", (b1 as u16) | ((b2 as u16) << 8)),
        "indx" => format!("(${:02X},X)", b1),
        "indy" => format!("(${:02X}),Y", b1),
        _ => String::new(),
    };

    let byte_str = bytes.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ");
    let text = if operand.is_empty() {
        format!("${:04X}  {:<8}  {}", addr, byte_str, mne)
    } else {
        format!("${:04X}  {:<8}  {} {}", addr, byte_str, mne, operand)
    };

    json!({
        "addr": addr as u64,
        "bytes": bytes.iter().map(|b| *b as u64).collect::<Vec<_>>(),
        "mnemonic": mne,
        "operand": operand,
        "text": text
    })
}

// ── api/call dispatch ─────────────────────────────────────────────────────────

fn dispatch_api_call(id: Value, params: &Value, state: &SharedState) -> Response {
    let method = match params.get("method").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return Response::err(id, -32602, "api/call: missing method"),
    };
    let args = params.get("args").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    match method.as_str() {
        "monitorRegisters" => {
            let st = state.lock().unwrap();
            let c = &st.session.machine.cpu6510;
            Response::ok(id, json!({
                "pc": c.reg_pc as u64,
                "a": c.reg_a as u64,
                "x": c.reg_x as u64,
                "y": c.reg_y as u64,
                "sp": c.reg_sp as u64,
                "flags": c.flags() as u64,
                "cycles": st.session.machine.clk
            }))
        }

        "monitorMemory" => {
            let start_addr = args.first().and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let end_addr = args.get(1).and_then(|v| v.as_u64()).unwrap_or(start_addr as u64 + 255) as u16;
            let st = state.lock().unwrap();
            let count = if end_addr >= start_addr { (end_addr - start_addr + 1) as usize } else { 0 };
            let bytes: Vec<u64> = (0..count)
                .map(|i| st.session.machine.read_full(start_addr.wrapping_add(i as u16)) as u64)
                .collect();
            Response::ok(id, json!(bytes))
        }

        "monitorDisasm" => {
            let addr = args.first().and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let count = args.get(1).and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            let st = state.lock().unwrap();
            let mut cursor = addr;
            let mut result = Vec::new();
            for _ in 0..count {
                let entry = disasm_one(cursor, |a| st.session.machine.read_full(a));
                let len = instr_len(st.session.machine.read_full(cursor)) as u16;
                result.push(entry);
                cursor = cursor.wrapping_add(len.max(1));
            }
            Response::ok(id, json!(result))
        }

        "stepInto" => {
            // TS AgentQueryApi.stepInto() returns void — WS omits result key entirely.
            let mut st = state.lock().unwrap();
            step_one_instruction(&mut st.session);
            drop(st);
            Response::void(id)
        }

        "stepOver" => {
            // opts is optional first arg (or second depending on spec); args[0] is opts
            let _opts = args.first();
            let cycle_budget: u64 = 100_000;
            let mut st = state.lock().unwrap();

            let start_pc = st.session.machine.cpu6510.reg_pc;
            let start_clk = st.session.machine.clk;
            // Length of current instruction to find the "next" PC
            let opcode = st.session.machine.read_full(start_pc);
            let instr_bytes = instr_len(opcode) as u16;
            let next_pc = start_pc.wrapping_add(instr_bytes);

            // Track initial SP for stack watch
            let initial_sp = st.session.machine.cpu6510.reg_sp;

            let mut instructions_elapsed: u64 = 0;
            #[allow(unused_assignments)]
            let mut halt_reason = "next_pc";
            #[allow(unused_assignments)]
            let mut halted = true;

            loop {
                let current_clk = st.session.machine.clk;
                if current_clk.wrapping_sub(start_clk) >= cycle_budget {
                    halt_reason = "budget_exhausted";
                    halted = false;
                    break;
                }
                step_one_instruction(&mut st.session);
                instructions_elapsed += 1;
                let pc = st.session.machine.cpu6510.reg_pc;
                let sp = st.session.machine.cpu6510.reg_sp;
                if pc == next_pc {
                    halt_reason = "next_pc";
                    halted = true;
                    break;
                }
                // Stack watch: if SP returns to initial level (RTS/RTI returned)
                if sp == initial_sp && instructions_elapsed > 1 {
                    halt_reason = "stack_watch";
                    halted = true;
                    break;
                }
            }

            let final_pc = st.session.machine.cpu6510.reg_pc;
            let cycles_elapsed = st.session.machine.clk.wrapping_sub(start_clk);
            // TS _instrCount() == cpu.cycles (not a real instruction counter), so
            // instructionsElapsed == cyclesElapsed in all TS-generated goldens.
            Response::ok(id, json!({
                "halted": halted,
                "haltReason": halt_reason,
                "cyclesElapsed": cycles_elapsed,
                "instructionsElapsed": cycles_elapsed,
                "finalPc": final_pc as u64
            }))
        }

        "until" => {
            let target_addr = args.first().and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let cycle_budget: u64 = 10_000_000;
            let mut st = state.lock().unwrap();
            let start_clk = st.session.machine.clk;

            // `until <addr>` runs until the target PC OR any armed breakpoint trips
            // (consults the bp SET, not just the single target — Spec 754 + VICE
            // `until`). Mirror the standing bp surface into the registry, then add
            // the ephemeral target as a temporary exec observer, drive the segment
            // run, and remove the ephemeral after.
            {
                let State { breakpoints, observers: reg, .. } = &mut *st;
                sync_observers(breakpoints, reg);
            }
            let _ = st.observers.add(observers::ObsSpec {
                name: "__until__".to_string(),
                trigger: observers::ObsTrigger::Exec,
                lo: target_addr,
                hi: target_addr,
                cond_src: None,
                action: observers::ObsAction::Break,
                log_exprs: None,
                cmd_src: None,
                mark_label: None,
                trace_scope: None,
            });
            let run = {
                let State { session, observers: reg, .. } = &mut *st;
                run_until_break(session, reg, cycle_budget)
            };
            {
                let State { breakpoints, observers: reg, .. } = &mut *st;
                writeback_hits(breakpoints, reg);
            }
            st.observers.remove("__until__");

            let halted = run.halted;
            let budget_exhausted = !run.halted;
            let final_pc = st.session.machine.c64_core.reg_pc;
            let cycles_elapsed = run.cycles_elapsed;
            let _ = start_clk;
            // TS _instrCount() == cpu.cycles, so instructionsElapsed == cyclesElapsed.
            Response::ok(id, json!({
                "halted": halted,
                "budgetExhausted": budget_exhausted,
                "cyclesElapsed": cycles_elapsed,
                "instructionsElapsed": cycles_elapsed,
                "finalPc": final_pc as u64
            }))
        }

        "addPcBreakpoint" => {
            let bp_id = args.first().and_then(|v| v.as_str()).unwrap_or("bp0").to_string();
            let pc = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let action = args.get(2).and_then(|v| v.as_str()).unwrap_or("halt").to_string();
            let mut st = state.lock().unwrap();
            // Remove existing with same id before re-adding
            st.breakpoints.api_entries.retain(|e| e.id != bp_id);
            st.breakpoints.api_entries.push(ApiBpEntry {
                id: bp_id.clone(),
                pc,
                action,
                enabled: true,
                hit_limit: None,
                ignore_count: 0,
                hit_count: 0,
            });
            Response::ok(id, json!(bp_id))
        }

        "listBreakpoints" => {
            // TS BreakpointManager.list() returns specs with hitCount and _ignoreRemaining set on add().
            let st = state.lock().unwrap();
            let list: Vec<Value> = st.breakpoints.api_entries.iter().map(|e| {
                // Report the REAL hit count + remaining ignore from the registry
                // observer (falls back to the bp-surface mirror when no run yet).
                let (hits, ignore_rem) = st.observers.get(&e.id)
                    .map(|o| (o.hits, o.ignore_left))
                    .unwrap_or((e.hit_count, e.ignore_count as u64));
                let mut obj = json!({
                    "id": e.id,
                    "predicate": { "kind": "pc", "pc": e.pc as u64 },
                    "action": e.action,
                    "enabled": e.enabled,
                    "hitCount": hits,
                    "_ignoreRemaining": ignore_rem
                });
                if let Some(hl) = e.hit_limit {
                    obj["hitLimit"] = json!(hl);
                }
                obj
            }).collect();
            Response::ok(id, json!(list))
        }

        "removeBreakpoint" => {
            let bp_id = args.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
            let mut st = state.lock().unwrap();
            let before = st.breakpoints.api_entries.len();
            st.breakpoints.api_entries.retain(|e| e.id != bp_id);
            let removed = st.breakpoints.api_entries.len() < before;
            Response::ok(id, json!(removed))
        }

        "status" => {
            // TS AgentQueryApi.status(): hasTraceBackend = false (no live trace unless attached)
            let st = state.lock().unwrap();
            let m = &st.session.machine;
            Response::ok(id, json!({
                "c64Cycles": m.clk,
                "driveCycles": m.drive8.drive_clk,
                "mode": "true-drive",
                "hasTraceBackend": false,
                "hasBookmarkBackend": false,
                "hasScenarioRegistry": false
            }))
        }

        other => {
            Response::err(id, -32601, format!("api/call: unknown method '{other}'"))
        }
    }
}

// ── RPC method dispatch ───────────────────────────────────────────────────────

fn dispatch(req: Request, state: &SharedState) -> Response {
    let id = req.id.clone();
    match req.method.as_str() {
        "ping" => {
            Response::ok(id, json!({}))
        }

        "session/create" => {
            let st = state.lock().unwrap();
            let cpu = &st.session.machine.cpu;
            let pc = cpu.pc as u64;
            let c64_cycles = st.session.machine.clk;
            let disk_path = st.session.disk_path.clone();
            Response::ok(id, json!({
                "sessionId": "integrated-1",
                "mode": "true-drive",
                "diskPath": disk_path,
                "attached": true,
                "c64Cycles": c64_cycles,
                "pc": pc,
                "trace": null
            }))
        }

        "session/list" => {
            let st = state.lock().unwrap();
            let c64_cycles = st.session.machine.clk;
            let disk_path = st.session.disk_path.clone();
            Response::ok(id, json!([{
                "sessionId": st.session.id,
                "mode": "true-drive",
                "diskPath": disk_path,
                "c64Cycles": c64_cycles
            }]))
        }

        "session/close" => {
            // Singleton session — mark running=false but keep alive.
            // TS runtimeSessions.close() releases "controller" and "session".
            let mut st = state.lock().unwrap();
            st.session.running = false;
            Response::ok(id, json!({
                "existed": true,
                "released": ["controller", "session"]
            }))
        }

        "session/run" => {
            let mut st = state.lock().unwrap();
            let cycles = req
                .params
                .get("cycles")
                .and_then(|v| v.as_u64())
                .unwrap_or(19705);
            run_cycle_budget(&mut st.session, cycles);
            Response::ok(id, json!({ "c64Cycles": st.session.machine.clk }))
        }

        "session/state" => {
            let st = state.lock().unwrap();
            let machine = &st.session.machine;
            let cpu = &machine.cpu;
            let v = |off: u8| machine.vic.read_reg(off);
            let d011 = v(0x11);
            let d016 = v(0x16);
            let d018 = v(0x18);
            let mode = ((d011 >> 5) & 3) | (((d016 >> 4) & 1) << 2);
            let screen_ptr = (((d018 >> 4) & 0xf) as u64) << 10;
            let chargen_ptr = (((d018 >> 1) & 7) as u64) << 11;
            let bitmap_ptr = if d018 & 8 != 0 { 0x2000u64 } else { 0 };
            let cia2_pra = machine.cia2.peek(0xdd00);
            let cia2_ddra = machine.cia2.peek(0xdd02);
            let bank = ((cia2_pra & cia2_ddra & 3) ^ 3) as u64;
            let rd16 = |a: u16| -> u64 {
                machine.read_full(a) as u64 | ((machine.read_full(a.wrapping_add(1)) as u64) << 8)
            };
            let sid_regs: Vec<u64> = machine.sid_regs[0..25].iter().map(|b| *b as u64).collect();
            Response::ok(id, json!({
                "c64Cycles": machine.clk,
                "driveCycles": machine.drive8.drive_clk,
                "mode": "true-drive",
                "runState": "paused",
                "cpu": {
                    "pc": cpu.pc as u64,
                    "a": cpu.a as u64,
                    "x": cpu.x as u64,
                    "y": cpu.y as u64,
                    "sp": cpu.sp as u64,
                    "flags": cpu.p as u64,
                    "cycles": cpu.cycles
                },
                "vic": {
                    "rasterLine": machine.vic.raster_line as u64,
                    "rasterCycle": machine.vic.raster_cycle as u64,
                    "mode": mode as u64,
                    "bank": bank,
                    "screenPtr": screen_ptr,
                    "chargenPtr": chargen_ptr,
                    "bitmapPtr": bitmap_ptr,
                    "border": (v(0x20) & 0xf) as u64,
                    "background": (v(0x21) & 0xf) as u64
                },
                "flow": { "focus": "auto", "current": "main", "stack": [] },
                "vectors": {
                    "irq": rd16(0xfffe),
                    "nmi": rd16(0xfffa),
                    "cinv": rd16(0x0314),
                    "cbinv": rd16(0x0318)
                },
                "sid": { "regs": sid_regs, "streaming": false }
            }))
        }

        "session/type" => {
            // PETSCII keyboard input. Mirrors the TS ws-server "session/type":
            // s.typeText(text, hold_cycles ?? 80_000, gap_cycles ?? 80_000) then
            // returns { c64Cycles: cpu.cycles, queued: text.length }. Key events
            // are queued into the matrix relative to the CURRENT cpu clock; the
            // FullBus reads them on each $DC01 access as the KERNAL scans.
            let mut st = state.lock().unwrap();
            let text = req
                .params
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let hold = req
                .params
                .get("hold_cycles")
                .and_then(|v| v.as_u64())
                .unwrap_or(80_000);
            let gap = req
                .params
                .get("gap_cycles")
                .and_then(|v| v.as_u64())
                .unwrap_or(80_000);
            let now = st.session.machine.cpu6510.clk;
            st.session.machine.keyboard.type_text(now, &text, hold, gap);
            let c64_cycles = st.session.machine.clk;
            // `queued` = source character count (TS `text?.length`), counting
            // UTF-16 code units; our ASCII command strings make chars().count()
            // equal to the JS .length.
            let queued = text.chars().count() as u64;
            Response::ok(id, json!({
                "c64Cycles": c64_cycles,
                "queued": queued
            }))
        }

        "session/joystick_set" => {
            // TRX64 has no joystick model (full.rs:366 — "none here"); accept the
            // bits and no-op, matching the TS `{ ok: true }` shape.
            Response::ok(id, json!({ "ok": true }))
        }

        "session/joystick_clear" => {
            // No joystick model → clearing is a no-op. Shape matches ws-server.ts
            // session/joystick_clear `{ ok: true }`.
            Response::ok(id, json!({ "ok": true }))
        }

        // session/input_status — UI inspector read of pressed keys + joystick bits
        // (ws-server.ts:1486). TRX64's keyboard is a timed-event queue (no held-key
        // set / pressed query) and has no joystick model, so pressed is empty and
        // both joysticks read released. Shape matches the TS `{ pressed, joystick1,
        // joystick2 }`.
        "session/input_status" => {
            let released = json!({
                "up": false, "down": false, "left": false, "right": false, "fire": false
            });
            Response::ok(id, json!({
                "pressed": Value::Array(Vec::new()),
                "joystick1": released,
                "joystick2": released
            }))
        }

        // session/load_prg — inject a PRG into RAM (ws-server.ts:761 →
        // loadPrgIntoRam). Reads the local file, writes the body at the load address
        // (PRG header = 2-byte LE load addr), and returns
        // { loadAddress, endAddress, bytesLoaded, path }. Load-only: does NOT set PC
        // or autostart (that is runtime/run_prg).
        "session/load_prg" => {
            let prg_path = match req.params.get("prg_path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return Response::err(id, -32602, "session/load_prg: prg_path required"),
            };
            let bytes = match std::fs::read(&prg_path) {
                Ok(b) => b,
                Err(e) => return Response::err(id, -32602, format!("session/load_prg: read {prg_path}: {e}")),
            };
            if bytes.len() < 2 {
                return Response::err(id, -32602, "session/load_prg: PRG too short (need 2-byte header)");
            }
            // Honor an explicit load_address override; else the PRG's own header.
            let load_address = req
                .params
                .get("load_address")
                .and_then(|v| v.as_u64())
                .map(|v| v as u16)
                .unwrap_or_else(|| (bytes[0] as u16) | ((bytes[1] as u16) << 8));
            let body = &bytes[2..];
            let mut st = state.lock().unwrap();
            st.session.machine.poke(load_address, body);
            st.session.machine.sync_after_monitor();
            let end_address = load_address.wrapping_add(body.len() as u16);
            Response::ok(id, json!({
                "loadAddress": load_address as u64,
                "endAddress": end_address as u64,
                "bytesLoaded": body.len() as u64,
                "path": prg_path
            }))
        }

        // session/reset — RuntimeController re-init (ws-server.ts:1392). mode:"soft"
        // = warm (RAM preserved), else cold. TRX64 has only cold_reset() (RAM is
        // preserved — it does NOT power-on-fill), so "soft" maps to cold_reset() and
        // a full cold power-cycle additionally fills power-on RAM. Both run the
        // KERNAL to READY (5M cycles, matching the TS runFor). Returns
        // { c64Cycles, pc, mode }.
        "session/reset" => {
            let mode = req.params.get("mode").and_then(|v| v.as_str()).unwrap_or("cold");
            let mut st = state.lock().unwrap();
            if mode == "soft" {
                // Warm = HW RESET line, RAM preserved.
                st.session.machine.cold_reset();
            } else {
                // Cold power-cycle = fresh DRAM fill, then reset.
                st.session.machine.fill_power_on_ram();
                st.session.machine.cold_reset();
                st.session.machine.drive8.cold_reset();
            }
            st.session.machine.keyboard.clear();
            run_cycle_budget(&mut st.session, 5_000_000);
            let out_mode = if mode == "soft" { "soft" } else { "cold" };
            let pc = st.session.machine.cpu6510.reg_pc as u64;
            let cycles = st.session.machine.clk;
            // A reset is a control discontinuity — clear stop + advance the frame.
            st.ctrl_stop = None;
            st.ctrl_frame += 1;
            Response::ok(id, json!({
                "c64Cycles": cycles,
                "pc": pc,
                "mode": out_mode
            }))
        }

        // session/drive_status — drive LED/motor/track/PC + IEC bus snapshot
        // (ws-server.ts:1499). c64re's vice probe lacks a motor flag and approximates
        // motorOn from the LED; TRX64 is the mirror — the motor bit
        // (rotation.byte_ready_active & BRA_MOTOR_ON) is public but the LED (VIA2 PB3)
        // is not, so ledOn is derived from motorOn (DOS lights the LED while the motor
        // spins — c64re's own stated rationale, inverted). rwMode defaults read.
        // Shape matches the TS exactly.
        "session/drive_status" => {
            use trx64_core::rotation::BRA_MOTOR_ON;
            let st = state.lock().unwrap();
            let m = &st.session.machine;
            let drv = &m.drive8;
            let half_track = (drv.rotation.current_half_track & 0xff) as u64;
            let track = half_track / 2;
            let motor_on = (drv.rotation.byte_ready_active & BRA_MOTOR_ON) != 0;
            let led_on = motor_on;
            let led_pwm: u64 = if led_on { 1000 } else { 0 };
            let drive_pc = drv.core.reg_pc as u64;
            let c64_pc = m.cpu6510.reg_pc;
            let dd00pra = m.cia2.peek(0xdd00) as u64;
            let dd00ddr = m.cia2.peek(0xdd02) as u64;
            // Transfer-mode heuristic (ws-server.ts:1551): KERNAL serial bands vs
            // the drive idle wait-loop vs custom.
            let transfer_mode = if (0xE000..=0xFFFF).contains(&c64_pc) {
                "kernal"
            } else if (0xF400..=0xF800).contains(&c64_pc) {
                "kernal"
            } else if (0xEBFD..=0xECC0).contains(&drv.core.reg_pc) {
                "idle"
            } else {
                "custom"
            };
            Response::ok(id, json!({
                "device": 8,
                "ledOn": led_on,
                "ledFlashing": false,
                "ledPwm": led_pwm,
                "motorOn": motor_on,
                "rwMode": "read",
                "halfTrack": half_track,
                "track": track,
                "sector": 0,
                "drivePc": drive_pc,
                "dd00": { "pra": dd00pra, "ddr": dd00ddr },
                "transferMode": transfer_mode
            }))
        }

        // session/cart_status — live cartridge status (ws-server.ts:1581). Returns
        // null when no cart attached; else { type, bank, activity, booted, sourceName }.
        // TRX64 has no write-LED generation counter, so activity is "read" when the
        // cart is mapped (exrom==0 || game==0) else "idle"; booted is false (no
        // cartBootedFrom tracking). Shape matches the TS.
        "session/cart_status" => {
            let st = state.lock().unwrap();
            let m = &st.session.machine;
            match m.cartridge.as_ref() {
                None => Response::ok(id, Value::Null),
                Some(cart) => {
                    let type_str = mapper_type_str(cart.mapper_type());
                    let bank = cart.get_state().current_bank as u64;
                    let lines = cart.get_lines();
                    let mapped = lines.exrom == 0 || lines.game == 0;
                    let activity = if mapped { "read" } else { "idle" };
                    let source_name = m
                        .cartridge_image
                        .as_ref()
                        .map(|img| img.name.clone());
                    Response::ok(id, json!({
                        "type": type_str,
                        "bank": bank,
                        "activity": activity,
                        "booted": false,
                        "sourceName": source_name
                    }))
                }
            }
        }

        // session/drive_power — drive 8 cold re-init (ws-server.ts:1620). Single
        // press = cold reset of the drive 6502 (DOS re-runs power-on init). Returns
        // { device, reinitialized, mode }.
        "session/drive_power" => {
            let mut st = state.lock().unwrap();
            st.session.machine.drive8.cold_reset();
            Response::ok(id, json!({
                "device": 8,
                "reinitialized": true,
                "mode": "trx64"
            }))
        }

        "session/screenshot" => {
            let st = state.lock().unwrap();
            let (url, w, h) = render_screenshot(&st.session.machine, 1);
            Response::ok(id, json!({ "dataUrl": url, "width": w, "height": h }))
        }

        "runtime/render_screen" => {
            // Pixel-art upscale: scale 1/2/4 nearest-neighbour. Returns the same
            // {dataUrl,width,height} envelope as session/screenshot.
            let scale = req
                .params
                .get("scale")
                .and_then(|v| v.as_u64())
                .map(|s| s as usize)
                .unwrap_or(1);
            if !matches!(scale, 1 | 2 | 4) {
                return Response::err(id, -32602, "runtime/render_screen: scale must be 1, 2, or 4");
            }
            let st = state.lock().unwrap();
            let (url, w, h) = render_screenshot(&st.session.machine, scale);
            Response::ok(id, json!({ "dataUrl": url, "width": w, "height": h, "scale": scale }))
        }

        // CPU-isolated inject + register-set monitor (subset: wr, r, r reg=val).
        "monitor/exec" => {
            let mut st = state.lock().unwrap();
            let cmd = req
                .params
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            match run_monitor(&mut st.session, &cmd) {
                Ok(out) => Response::ok(id, json!({ "output": out })),
                Err(e) => Response::ok(id, json!({ "error": e })),
            }
        }

        // ── debug/* ──────────────────────────────────────────────────────────

        "debug/run" => {
            let mut st = state.lock().unwrap();
            st.session.running = true;
            st.ctrl_stop = None;
            st.ctrl_frame += 1;
            let frame = st.ctrl_frame;
            run_debug_control(id, &mut st, frame, false)
        }

        "debug/pause" => {
            let mut st = state.lock().unwrap();
            st.session.running = false;
            st.ctrl_frame += 1;
            let frame = st.ctrl_frame;
            let bps = st.breakpoints.list_vice_json();
            let c = &st.session.machine.cpu6510;
            let pc = c.reg_pc as u64;
            let cycles = st.session.machine.clk;
            let stop_obj = json!({ "reason": "pause", "pc": pc, "cycles": cycles });
            st.ctrl_stop = Some(CtrlStop { reason: "pause", pc: c.reg_pc, cycles });
            Response::ok(id, json!({
                "runState": "paused",
                "pacing": { "mode": "pal", "ratio": 1 },
                "pc": pc,
                "cycles": cycles,
                "frame": frame,
                "breakpoints": bps,
                "stop": stop_obj,
                "controlOwner": "llm"
            }))
        }

        "debug/continue" => {
            let mut st = state.lock().unwrap();
            st.session.running = true;
            st.ctrl_stop = None;
            // TS: continue does not increment frame (stays at pause frame).
            let frame = st.ctrl_frame;
            // A continue from a breakpoint must STEP PAST the current PC first
            // (else the boundary check re-trips the same bp immediately).
            run_debug_control(id, &mut st, frame, true)
        }

        "debug/step" => {
            let mut st = state.lock().unwrap();
            step_one_instruction(&mut st.session);
            st.session.running = false;
            let c = &st.session.machine.cpu6510;
            Response::ok(id, json!({
                "runState": "paused",
                "pc": c.reg_pc as u64,
                "a": c.reg_a as u64,
                "x": c.reg_x as u64,
                "y": c.reg_y as u64,
                "sp": c.reg_sp as u64,
                "flags": c.flags() as u64,
                "cycles": st.session.machine.clk
            }))
        }

        // debug/state — the RuntimeController.state() snapshot (runtime-controller.ts
        // :344). Read-only: reports the CURRENT run/pause state, pacing, pc/cycles,
        // controller frame, breakpoints, and last stop. TRX64 has no pacing loop, so
        // pacing is the constant PAL pacing the TS reports for an unpaced session.
        "debug/state" => {
            let st = state.lock().unwrap();
            let bps = st.breakpoints.list_vice_json();
            let c = &st.session.machine.cpu6510;
            let pc = c.reg_pc as u64;
            let cycles = st.session.machine.clk;
            let run_state = if st.session.running { "running" } else { "paused" };
            let stop = match &st.ctrl_stop {
                Some(s) => json!({ "reason": s.reason, "pc": s.pc as u64, "cycles": s.cycles }),
                None => Value::Null,
            };
            Response::ok(id, json!({
                "runState": run_state,
                "pacing": { "mode": "pal", "ratio": 1 },
                "pc": pc,
                "cycles": cycles,
                "frame": st.ctrl_frame,
                "breakpoints": bps,
                "stop": stop,
                "controlOwner": "llm"
            }))
        }

        "debug/break_add" => {
            let pc_val = req.params.get("pc").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let mut st = state.lock().unwrap();
            let num = st.breakpoints.next_num;
            st.breakpoints.next_num += 1;
            st.breakpoints.entries.push(BpEntry { num, pc: pc_val, enabled: true });
            let list: Vec<Value> = st.breakpoints.entries.iter()
                .map(|e| json!({ "num": e.num, "pc": e.pc as u64 }))
                .collect();
            Response::ok(id, json!({
                "num": num,
                "breakpoints": list
            }))
        }

        "debug/break_del" => {
            let del_id = req.params.get("id").and_then(|v| v.as_u64());
            let mut st = state.lock().unwrap();
            if let Some(n) = del_id {
                st.breakpoints.entries.retain(|e| e.num != n as u32);
            } else {
                // No id = delete all
                st.breakpoints.entries.clear();
            }
            let list: Vec<Value> = st.breakpoints.entries.iter()
                .map(|e| json!({ "num": e.num, "pc": e.pc as u64 }))
                .collect();
            Response::ok(id, json!({
                "deleted": true,
                "breakpoints": list
            }))
        }

        "debug/break_list" => {
            let st = state.lock().unwrap();
            let list: Vec<Value> = st.breakpoints.entries.iter()
                .map(|e| json!({ "num": e.num, "pc": e.pc as u64 }))
                .collect();
            Response::ok(id, json!({ "breakpoints": list }))
        }

        // ── api/call ─────────────────────────────────────────────────────────

        "api/call" => {
            dispatch_api_call(id, &req.params, state)
        }

        // ── runtime/* ────────────────────────────────────────────────────────

        "runtime/run_prg" => {
            let prg_path = req.params.get("prg_path").and_then(|v| v.as_str()).map(str::to_string);
            let bytes_b64 = req.params.get("bytes_b64").and_then(|v| v.as_str()).map(str::to_string);
            let run_addr = req.params.get("run").and_then(|v| v.as_u64());

            // Load the PRG bytes
            let prg_bytes: Vec<u8> = if let Some(b64) = bytes_b64 {
                // Base64 decode
                match base64_decode(&b64) {
                    Ok(b) => b,
                    Err(e) => return Response::err(id, -32602, format!("runtime/run_prg: base64 decode error: {e}")),
                }
            } else if let Some(path) = prg_path {
                match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) => return Response::err(id, -32602, format!("runtime/run_prg: file read error: {e}")),
                }
            } else {
                return Response::err(id, -32602, "runtime/run_prg: need prg_path or bytes_b64");
            };

            if prg_bytes.len() < 2 {
                return Response::err(id, -32602, "runtime/run_prg: PRG too short (< 2 bytes)");
            }

            let load_addr = (prg_bytes[0] as u16) | ((prg_bytes[1] as u16) << 8);
            let body = &prg_bytes[2..];

            let mut st = state.lock().unwrap();
            st.session.machine.poke(load_addr, body);
            // Set PC to run address (or load address if not specified)
            let pc = run_addr.unwrap_or(load_addr as u64) as u16;
            st.session.machine.cpu6510.reg_pc = pc;
            st.session.machine.sync_after_monitor();
            st.session.injected = true;

            Response::ok(id, json!({
                "loadAddress": load_addr as u64,
                "action": "loaded"
            }))
        }

        "runtime/mark" => {
            let label = req.params.get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let st = state.lock().unwrap();
            let (run_id, event_count, marks) = match &st.session.trace {
                Some(t) => (t.run_id.clone(), t.event_count, 1u64),
                None => ("".to_string(), 0u64, 0u64),
            };
            Response::ok(id, json!({
                "runId": run_id,
                "eventCount": event_count,
                "marks": marks,
                "label": label
            }))
        }

        "runtime/swap_disk_and_continue" => {
            let path_str = match req.params.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return Response::err(id, -32602, "runtime/swap_disk_and_continue: missing path"),
            };
            let settle_cycles = req.params.get("settle_cycles").and_then(|v| v.as_u64()).unwrap_or(1_500_000);
            let post_cycles = req.params.get("post_cycles").and_then(|v| v.as_u64()).unwrap_or(4_000_000);

            let bytes = match std::fs::read(&path_str) {
                Ok(b) => b,
                Err(e) => return Response::err(id, -32602, format!("runtime/swap_disk_and_continue: file read {path_str}: {e}")),
            };

            let disk_name = path_str.split('/').last().unwrap_or("disk").to_string();
            let format_str = if disk_name.to_lowercase().ends_with(".g64")
                || (bytes.len() >= 8 && &bytes[..8] == b"GCR-1541")
            {
                "g64"
            } else {
                "d64"
            };
            let sha256 = sha256_hex(&bytes);
            let disk_kind = if format_str == "g64" { DiskKind::G64 } else { DiskKind::D64 };
            let image = DiskImage {
                kind: disk_kind,
                bytes,
                backing_path: Some(path_str.clone()),
                read_only: false,
            };

            let mut st = state.lock().unwrap();
            st.session.machine.drive8.attach_disk(image);
            st.session.disk_path = path_str.clone();
            let cycle = st.session.machine.clk;

            Response::ok(id, json!({
                "ok": true,
                "mounted": disk_name,
                "screenBefore": "",
                "screenAfter": "",
                "promptCleared": false,
                "advanced": false,
                "detail": {
                    "insert": {
                        "cycle": cycle,
                        "operation": "disk",
                        "role": "drive8",
                        "format": format_str,
                        "sha256": sha256,
                        "resetPolicy": null,
                        "checkpointBeforeId": null,
                        "checkpointAfterId": null
                    },
                    "settleCycles": settle_cycles,
                    "postCycles": post_cycles,
                    "hadPrompt": false,
                    "stillPrompt": false
                }
            }))
        }

        // ── media/* ──────────────────────────────────────────────────────────

        "media/list_paths" => {
            let c64re_root = std::env::var("C64RE_ROOT")
                .unwrap_or_else(|_| "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP".to_string());
            let samples_path = format!("{c64re_root}/samples");
            let downloads_path = format!("{}/Downloads", std::env::var("HOME").unwrap_or_else(|_| "/Users/alex".to_string()));
            let project_path = std::env::args()
                .skip_while(|a| a != "--project")
                .nth(1)
                .unwrap_or_default();
            let roots = json!([
                { "label": "samples", "path": samples_path, "exists": std::path::Path::new(&samples_path).exists() },
                { "label": "project", "path": project_path, "exists": !project_path.is_empty() && std::path::Path::new(&project_path).exists() },
                { "label": "Downloads", "path": downloads_path, "exists": std::path::Path::new(&downloads_path).exists() }
            ]);
            Response::ok(id, roots)
        }

        "media/browse" => {
            let browse_path = match req.params.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return Response::err(id, -32602, "media/browse: missing path"),
            };

            let canonical = match std::fs::canonicalize(&browse_path) {
                Ok(p) => p.to_string_lossy().to_string(),
                Err(_) => browse_path.clone(),
            };

            let read_dir = match std::fs::read_dir(&browse_path) {
                Ok(rd) => rd,
                Err(e) => return Response::err(id, -32602, format!("media/browse: read_dir error: {e}")),
            };

            let mut entries: Vec<Value> = Vec::new();
            for entry in read_dir.flatten() {
                let entry_path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                let abs_path = entry_path.to_string_lossy().to_string();

                if name.starts_with('.') {
                    continue;
                }

                let meta = entry.metadata().ok();
                let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                let size_bytes = meta.as_ref().map(|m| m.len());

                let lower = name.to_lowercase();
                let file_type = if is_dir {
                    "dir"
                } else if lower.ends_with(".d64") {
                    "d64"
                } else if lower.ends_with(".g64") {
                    "g64"
                } else if lower.ends_with(".prg") {
                    "prg"
                } else if lower.ends_with(".crt") {
                    "crt"
                } else if lower.ends_with(".t64") {
                    "t64"
                } else if lower.ends_with(".tap") {
                    "tap"
                } else if lower.ends_with(".vsf") {
                    "vsf"
                } else {
                    "file"
                };

                // Skip unknown file types (TS browseDir only shows known media + dirs)
                if file_type == "file" {
                    continue;
                }

                let mut entry_obj = json!({
                    "name": name,
                    "path": abs_path,
                    "type": file_type,
                    "deferred": false
                });
                if let Some(sz) = size_bytes {
                    if !is_dir {
                        entry_obj["sizeBytes"] = json!(sz);
                    }
                }
                entries.push(entry_obj);
            }

            // Sort using Node.js localeCompare to match TS browseDir's sort((a,b)=>a.localeCompare(b)).
            // ICU collation (used by Node) differs from Rust's Unicode ordering for filenames with
            // punctuation, brackets, underscores — we can't replicate it without ICU.
            let names: Vec<String> = entries.iter()
                .filter_map(|e| e["name"].as_str().map(str::to_string))
                .collect();
            let names_json = serde_json::to_string(&names).unwrap_or_else(|_| "[]".into());
            let sorted_names: Vec<String> = std::process::Command::new("node")
                .arg("-e")
                .arg(format!(
                    "const n={names_json}; console.log(JSON.stringify(n.sort((a,b)=>a.localeCompare(b))));"
                ))
                .output()
                .ok()
                .and_then(|out| serde_json::from_slice::<Vec<String>>(&out.stdout).ok())
                .unwrap_or_else(|| {
                    // Fallback: case-insensitive ASCII sort
                    let mut ns = names.clone();
                    ns.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
                    ns
                });
            // Rebuild entries in sorted order
            let mut name_to_entry: std::collections::HashMap<String, Value> = entries
                .into_iter()
                .map(|e| (e["name"].as_str().unwrap_or("").to_string(), e))
                .collect();
            entries = sorted_names.into_iter()
                .filter_map(|n| name_to_entry.remove(&n))
                .collect();

            Response::ok(id, json!({
                "path": canonical,
                "entries": entries
            }))
        }

        "media/ingress" => {
            let kind = req.params.get("kind").and_then(|v| v.as_str()).unwrap_or("disk").to_string();
            let path = req.params.get("path").and_then(|v| v.as_str()).map(str::to_string);
            let bytes_b64 = req.params.get("bytes_b64").and_then(|v| v.as_str()).map(str::to_string);
            let name = req.params.get("name").and_then(|v| v.as_str()).map(str::to_string);
            let role = req.params.get("role").and_then(|v| v.as_str()).unwrap_or("drive8").to_string();

            match kind.as_str() {
                "eject" => {
                    let mut st = state.lock().unwrap();
                    st.session.machine.drive8.detach_disk();
                    st.session.disk_path = String::new();
                    let cycle = st.session.machine.clk;
                    let cp_before = format!("cp_{}_{}", cycle, st.checkpoint_counter);
                    st.checkpoint_counter += 1;
                    let cp_after = format!("cp_{}_{}", cycle, st.checkpoint_counter);
                    st.checkpoint_counter += 1;
                    Response::ok(id, json!({
                        "ok": true,
                        "event": {
                            "cycle": cycle,
                            "operation": "eject",
                            "role": role,
                            "checkpointBeforeId": cp_before,
                            "checkpointAfterId": cp_after
                        },
                        "paused": true,
                        "wasRunning": false,
                        "detail": { "role": role }
                    }))
                }
                "disk" => {
                    let bytes = if let Some(b64) = bytes_b64 {
                        match base64_decode(&b64) {
                            Ok(b) => b,
                            Err(e) => return Response::err(id, -32602, format!("media/ingress: base64 decode: {e}")),
                        }
                    } else if let Some(ref p) = path {
                        match std::fs::read(p) {
                            Ok(b) => b,
                            Err(e) => return Response::err(id, -32602, format!("media/ingress: file read {p}: {e}")),
                        }
                    } else {
                        return Response::err(id, -32602, "media/ingress: disk requires path or bytes_b64");
                    };

                    let disk_name = name.unwrap_or_else(|| {
                        path.as_deref()
                            .and_then(|p| p.split('/').last())
                            .unwrap_or("disk")
                            .to_string()
                    });
                    let format_str = if disk_name.to_lowercase().ends_with(".g64")
                        || (bytes.len() >= 8 && &bytes[..8] == b"GCR-1541")
                    {
                        "g64"
                    } else {
                        "d64"
                    };
                    let sha256 = sha256_hex(&bytes);
                    let backing_path = path.clone();
                    let disk_path_str = path.clone().unwrap_or_default();

                    let disk_kind = if format_str == "g64" { DiskKind::G64 } else { DiskKind::D64 };
                    let image = DiskImage {
                        kind: disk_kind,
                        bytes,
                        backing_path: backing_path.clone(),
                        read_only: false,
                    };

                    let mut st = state.lock().unwrap();
                    st.session.machine.drive8.attach_disk(image);
                    st.session.disk_path = disk_path_str;
                    let cycle = st.session.machine.clk;
                    let cp_after = format!("cp_{}_{}", cycle, st.checkpoint_counter);
                    st.checkpoint_counter += 1;

                    let detail = if let Some(ref bp) = backing_path {
                        json!({ "name": disk_name, "backingPath": bp })
                    } else {
                        json!({ "name": disk_name })
                    };

                    Response::ok(id, json!({
                        "ok": true,
                        "event": {
                            "cycle": cycle,
                            "operation": "disk",
                            "role": "drive8",
                            "format": format_str,
                            "sha256": sha256,
                            "checkpointAfterId": cp_after
                        },
                        "paused": true,
                        "wasRunning": false,
                        "detail": detail
                    }))
                }
                "prg" => {
                    let prg_bytes = if let Some(b64) = bytes_b64 {
                        match base64_decode(&b64) {
                            Ok(b) => b,
                            Err(e) => return Response::err(id, -32602, format!("media/ingress: base64 decode: {e}")),
                        }
                    } else if let Some(ref p) = path {
                        match std::fs::read(p) {
                            Ok(b) => b,
                            Err(e) => return Response::err(id, -32602, format!("media/ingress: file read {p}: {e}")),
                        }
                    } else {
                        return Response::err(id, -32602, "media/ingress: prg requires path or bytes_b64");
                    };

                    if prg_bytes.len() < 2 {
                        return Response::err(id, -32602, "media/ingress: PRG too short (< 2 bytes)");
                    }
                    let load_addr = (prg_bytes[0] as u16) | ((prg_bytes[1] as u16) << 8);
                    let body = &prg_bytes[2..];
                    let sha256 = sha256_hex(&prg_bytes);
                    let prg_name = name.unwrap_or_else(|| {
                        path.as_deref()
                            .and_then(|p| p.split('/').last())
                            .unwrap_or("program.prg")
                            .to_string()
                    });

                    let mut st = state.lock().unwrap();
                    st.session.machine.poke(load_addr, body);
                    st.session.machine.cpu6510.reg_pc = load_addr;
                    st.session.machine.sync_after_monitor();
                    st.session.injected = true;
                    let cycle = st.session.machine.clk;

                    Response::ok(id, json!({
                        "ok": true,
                        "event": {
                            "cycle": cycle,
                            "operation": "prg",
                            "role": null,
                            "format": "prg",
                            "sha256": sha256,
                            "resetPolicy": null,
                            "checkpointBeforeId": null,
                            "checkpointAfterId": null
                        },
                        "paused": true,
                        "wasRunning": false,
                        "detail": { "name": prg_name, "loadAddress": load_addr as u64 }
                    }))
                }
                "crt" => {
                    Response::err(id, -32601, "media/ingress: crt kind not yet implemented")
                }
                other => {
                    Response::err(id, -32602, format!("media/ingress: unsupported kind '{other}'"))
                }
            }
        }

        "media/unmount" => {
            let role = req.params.get("role").and_then(|v| v.as_str()).unwrap_or("drive8").to_string();
            let mut st = state.lock().unwrap();
            st.session.machine.drive8.detach_disk();
            st.session.disk_path = String::new();
            let cycle = st.session.machine.clk;
            Response::ok(id, json!({
                "ok": true,
                "event": {
                    "cycle": cycle,
                    "operation": "eject",
                    "role": role,
                    "format": null,
                    "sha256": null,
                    "resetPolicy": null,
                    "checkpointBeforeId": null,
                    "checkpointAfterId": null
                },
                "paused": true,
                "wasRunning": false,
                "detail": { "role": role }
            }))
        }

        "media/mount" => {
            let path_str = match req.params.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return Response::err(id, -32602, "media/mount: missing path"),
            };

            let bytes = match std::fs::read(&path_str) {
                Ok(b) => b,
                Err(e) => return Response::err(id, -32602, format!("media/mount: file read {path_str}: {e}")),
            };

            let disk_name = path_str.split('/').last().unwrap_or("disk").to_string();
            let format_str = if disk_name.to_lowercase().ends_with(".g64")
                || (bytes.len() >= 8 && &bytes[..8] == b"GCR-1541")
            {
                "g64"
            } else {
                "d64"
            };
            let sha256 = sha256_hex(&bytes);
            let disk_kind = if format_str == "g64" { DiskKind::G64 } else { DiskKind::D64 };
            let image = DiskImage {
                kind: disk_kind,
                bytes,
                backing_path: Some(path_str.clone()),
                read_only: false,
            };

            let mut st = state.lock().unwrap();
            st.session.machine.drive8.attach_disk(image);
            st.session.disk_path = path_str.clone();
            let cycle = st.session.machine.clk;

            Response::ok(id, json!({
                "mountedPath": path_str,
                "type": format_str,
                "slot": 8u64,
                "sha256": sha256,
                "event": {
                    "cycle": cycle,
                    "operation": "disk",
                    "role": "drive8",
                    "format": format_str,
                    "sha256": sha256,
                    "resetPolicy": null,
                    "checkpointBeforeId": null,
                    "checkpointAfterId": null
                },
                "detail": { "name": disk_name, "backingPath": path_str },
                "paused": true
            }))
        }

        "media/swap" => {
            let path_str = match req.params.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return Response::err(id, -32602, "media/swap: missing path"),
            };

            let bytes = match std::fs::read(&path_str) {
                Ok(b) => b,
                Err(e) => return Response::err(id, -32602, format!("media/swap: file read {path_str}: {e}")),
            };

            let disk_name = path_str.split('/').last().unwrap_or("disk").to_string();
            let format_str = if disk_name.to_lowercase().ends_with(".g64")
                || (bytes.len() >= 8 && &bytes[..8] == b"GCR-1541")
            {
                "g64"
            } else {
                "d64"
            };
            let sha256 = sha256_hex(&bytes);
            let disk_kind = if format_str == "g64" { DiskKind::G64 } else { DiskKind::D64 };
            let image = DiskImage {
                kind: disk_kind,
                bytes,
                backing_path: Some(path_str.clone()),
                read_only: false,
            };

            let mut st = state.lock().unwrap();
            st.session.machine.drive8.attach_disk(image);
            st.session.disk_path = path_str.clone();
            let cycle = st.session.machine.clk;

            Response::ok(id, json!({
                "mountedPath": path_str,
                "type": format_str,
                "slot": 8u64,
                "sha256": sha256,
                "event": {
                    "cycle": cycle,
                    "operation": "disk",
                    "role": "drive8",
                    "format": format_str,
                    "sha256": sha256,
                    "resetPolicy": null,
                    "checkpointBeforeId": null,
                    "checkpointAfterId": null
                },
                "detail": { "name": disk_name, "backingPath": path_str },
                "paused": true
            }))
        }

        "media/persist" => {
            let st = state.lock().unwrap();
            let result = match st.session.machine.drive8.get_attached_disk() {
                None => {
                    Ok(json!({ "written": false, "reason": "no backing path or not mounted" }))
                }
                Some(disk) => {
                    match &disk.backing_path {
                        None => {
                            Ok(json!({ "written": false, "reason": "no backing path or not mounted" }))
                        }
                        Some(bp) => {
                            if disk.read_only {
                                Ok(json!({ "written": false, "reason": "read-only or not dirty" }))
                            } else {
                                let bytes_to_write = disk.bytes.clone();
                                let path_clone = bp.clone();
                                drop(st);
                                match std::fs::write(&path_clone, &bytes_to_write) {
                                    Ok(()) => Ok(json!({
                                        "written": true,
                                        "path": path_clone,
                                        "bytes": bytes_to_write.len()
                                    })),
                                    Err(e) => Err(format!("media/persist: write error: {e}")),
                                }
                            }
                        }
                    }
                }
            };
            match result {
                Ok(v) => Response::ok(id, v),
                Err(e) => Response::err(id, -32001, e),
            }
        }

        // ── trace/* ──────────────────────────────────────────────────────────

        "trace/start_domains" => {
            let mut st = state.lock().unwrap();
            let output = req
                .params
                .get("output")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| default_trace_output(&st.session.id));
            let retrace = output.with_extension("c64retrace");
            let cycle_start = st.session.machine.clk;
            let run_id = format!("run_live-capture_{}", cycle_start);
            let domains: Vec<String> = req
                .params
                .get("domains")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|d| d.as_str().map(String::from)).collect())
                .unwrap_or_else(|| vec!["c64-cpu".into(), "memory".into()]);
            let meta_json = serde_json::to_string(&json!({
                "runId": run_id,
                "defId": "live-capture",
                "defVersion": 1,
                "defName": "live-capture",
                "defJson": "",
                "domains": domains,
                "cycleStart": cycle_start,
                "createdAt": "",
            }))
            .unwrap_or_default();
            st.session.trace = Some(TraceState {
                retrace_path: retrace,
                meta_json,
                cycle_start,
                buf: Vec::new(),
                run_id: run_id.clone(),
                event_count: 0,
                domains: domains.clone(),
            });
            // Echo the mounted media's SHA in the run descriptor (TS oracle parity:
            // a trace started with a disk attached carries `run.media.sha256`).
            let mut run = json!({
                "runId": run_id,
                "definitionId": "live-capture",
                "definitionVersion": 1,
                "cycleStart": cycle_start,
                "marks": [],
                "eventCount": 0,
                "bytesWritten": 0
            });
            if let Some(disk) = st.session.machine.drive8.get_attached_disk() {
                let sha = sha256_hex(&disk.bytes);
                run["media"] = json!({ "sha256": sha });
            }
            Response::ok(id, json!({
                "run": run,
                "outputPath": output.to_string_lossy(),
                "domains": domains
            }))
        }

        // ── Spec 708 — declarative trace definitions (validate / put / list) ──
        // Pure data + a per-session map; no core primitive. Shapes match the TS
        // ws-server.ts handlers (trace/definition/{validate,put,list}) 1:1.

        "trace/definition/validate" => {
            let def = req.params.get("definition").cloned().unwrap_or(Value::Null);
            let (ok, errors) = validate_trace_definition(&def);
            Response::ok(id, json!({ "ok": ok, "errors": errors }))
        }

        "trace/definition/put" => {
            let def = req.params.get("definition").cloned().unwrap_or(Value::Null);
            let (ok, errors) = validate_trace_definition(&def);
            if !ok {
                // TS: `return { ok: false, errors }` (NOT an RPC error).
                return Response::ok(id, json!({ "ok": false, "errors": errors }));
            }
            // TS: `id = definition.id || slugTraceId(definition.name)`.
            let explicit_id = def.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
            let def_id = match explicit_id {
                Some(s) => s.to_string(),
                None => {
                    let name = def.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    slug_trace_id(name)
                }
            };
            // Store the definition with its resolved id (`{ ...definition, id }`).
            let mut stored = def.clone();
            if let Some(obj) = stored.as_object_mut() {
                obj.insert("id".to_string(), json!(def_id));
            }
            let mut st = state.lock().unwrap();
            st.trace_definitions.insert(def_id.clone(), stored);
            Response::ok(id, json!({ "ok": true, "id": def_id }))
        }

        "trace/definition/list" => {
            let st = state.lock().unwrap();
            let definitions: Vec<Value> = st.trace_definitions.values().cloned().collect();
            Response::ok(id, json!({ "definitions": definitions }))
        }

        "trace/run/stop" => {
            let mut st = state.lock().unwrap();
            let status = finalize_trace(&mut st.session);
            Response::ok(id, json!({ "run": status.0, "status": status.1 }))
        }

        "trace/run/status" => {
            let st = state.lock().unwrap();
            let status = match &st.session.trace {
                Some(t) => json!({
                    "active": true,
                    "runId": t.run_id,
                    "eventCount": t.event_count,
                    "binary": true,
                    "retracePath": t.retrace_path.to_string_lossy(),
                }),
                None => json!({ "active": false }),
            };
            Response::ok(id, status)
        }

        // ── vic/inspect — frozen render descriptor + pixel resolve ────────────

        "vic/inspect" => {
            let st = state.lock().unwrap();
            let m = &st.session.machine;
            let v = |off: u8| m.vic.read_reg(off);
            let d011 = v(0x11);
            let d016 = v(0x16);
            let d018 = v(0x18);
            let bank_base = m.vic_bank_base() as u64;
            let mode_bits = ((d011 >> 5) & 3) | (((d016 >> 4) & 1) << 2);
            let mode_name = match (d011 & 0x40 != 0, d011 & 0x20 != 0, d016 & 0x10 != 0) {
                (false, false, false) => "text",
                (false, false, true) => "multicolor-text",
                (true, false, false) => "ecm",
                (false, true, false) => "bitmap",
                (false, true, true) => "multicolor-bitmap",
                _ => "invalid",
            };
            let screen = bank_base + (((d018 >> 4) & 0xf) as u64) << 10;
            let charset = bank_base + (((d018 >> 1) & 7) as u64) << 11;
            let bitmap = bank_base + if d018 & 8 != 0 { 0x2000u64 } else { 0 };
            // Optional pixel resolve (display coords 0..319 × 0..199).
            let pixel = match (
                req.params.get("x").and_then(|v| v.as_u64()),
                req.params.get("y").and_then(|v| v.as_u64()),
            ) {
                (Some(x), Some(y)) if x < 320 && y < 200 => {
                    let (_w, _h, rgba) = m.render_canvas_rgba();
                    // Display origin in the 384×272 canvas is (32, 35).
                    let cx = 32 + x as usize;
                    let cy = 35 + y as usize;
                    let off = (cy * trx64_core::render::CANVAS_W + cx) * 4;
                    json!({ "x": x, "y": y, "rgba": [rgba[off], rgba[off+1], rgba[off+2], rgba[off+3]] })
                }
                _ => serde_json::Value::Null,
            };
            Response::ok(id, json!({
                "mode": mode_bits,
                "modeName": mode_name,
                "bank": bank_base,
                "screen": screen,
                "charset": charset,
                "bitmap": bitmap,
                "border": (v(0x20) & 0xf) as u64,
                "background": (v(0x21) & 0xf) as u64,
                "width": trx64_core::render::CANVAS_W,
                "height": trx64_core::render::CANVAS_H,
                "pixel": pixel
            }))
        }

        m if m.starts_with("vic/") => {
            Response::err(id, -32001,
                format!("NOT_IMPLEMENTED: {m}: not in vic-render scope"))
        }

        // ── checkpoint/*, recorder/*, vsf/*, trace/read, debug/memory_access_map ─

        "debug/memory_access_map" => {
            Response::err(id, -32001, "NOT_IMPLEMENTED: debug/memory_access_map: deferred")
        }

        "trace/read" => {
            Response::err(id, -32001, "NOT_IMPLEMENTED: trace/read: deferred")
        }

        m if m.starts_with("checkpoint/") => {
            Response::err(id, -32001, format!("NOT_IMPLEMENTED: {m}: deferred"))
        }

        m if m.starts_with("recorder/") => {
            Response::err(id, -32001, format!("NOT_IMPLEMENTED: {m}: deferred"))
        }

        "vsf/save" => {
            let output_path = req.params
                .get("output_path")
                .and_then(|v| v.as_str())
                .unwrap_or("/tmp/trx64.vsf")
                .to_string();
            let st = state.lock().unwrap();
            let bytes = trx64_core::vsf::save_vsf(&st.session.machine);
            let bytes_written = bytes.len();
            drop(st);
            // Response shape MATCHES the TS daemon (ws-server.ts vsf/save handler):
            //   { savedPath, bytes }  — savedPath is volatile (oracle whitelists `path`-
            //   like keys; `output_path`/`outputPath` are in VOLATILE_KEYS but `savedPath`
            //   is NOT, so we still return it for shape parity — it is a path string the
            //   oracle compares; both daemons get the SAME output_path param, so it is
            //   byte-equal anyway). `bytes` = on-disk file size.
            match std::fs::write(&output_path, &bytes) {
                Ok(()) => Response::ok(id, json!({
                    "savedPath": output_path,
                    "bytes": bytes_written
                })),
                Err(e) => Response::err(id, -32001, format!("vsf/save: write error: {e}")),
            }
        }

        "vsf/load" => {
            let input_path = req.params
                .get("input_path")
                .and_then(|v| v.as_str())
                .unwrap_or("/tmp/trx64.vsf")
                .to_string();
            let file_bytes = match std::fs::read(&input_path) {
                Ok(b) => b,
                Err(e) => return Response::err(id, -32001, format!("vsf/load: read error: {e}")),
            };
            let file_bytes_len = file_bytes.len();
            let mut st = state.lock().unwrap();
            match trx64_core::vsf::load_vsf(&mut st.session.machine, &file_bytes) {
                Ok(result) => {
                    // Response shape MATCHES the TS daemon (ws-server.ts vsf/load handler):
                    //   { loadedPath, bytes, source, loadedModules }
                    // `bytes` = on-disk file size; `source` = "c64re"/"vice-x64sc";
                    // `loadedModules` = modules restored, in file (= save) order.
                    Response::ok(id, json!({
                        "loadedPath": input_path,
                        "bytes": file_bytes_len,
                        "source": result.source,
                        "loadedModules": result.loaded_modules
                    }))
                }
                Err(e) => Response::err(id, -32001, format!("vsf/load: {e}")),
            }
        }

        m if m.starts_with("vsf/") => {
            Response::err(id, -32001, format!("NOT_IMPLEMENTED: {m}: deferred"))
        }

        other => {
            Response::err(id, -32601, format!("Method not found: {other}"))
        }
    }
}

// ── Cartridge mapper-type → c64re string ──────────────────────────────────────

/// Map a TRX64 [`trx64_core::cart::MapperType`] to the c64re
/// HeadlessCartridgeMapperType string (cartridge.ts) the cart_status `type` field
/// carries, so the wire value matches the TS daemon.
fn mapper_type_str(t: trx64_core::cart::MapperType) -> &'static str {
    use trx64_core::cart::MapperType::*;
    match t {
        Normal8k => "normal_8k",
        Normal16k => "normal_16k",
        Ultimax => "ultimax",
        Ocean => "ocean",
        MagicDesk => "magicdesk",
        MagicDesk16 => "magicdesk16",
        Unsupported => "cartridge",
    }
}

// ── SHA-256 helper ────────────────────────────────────────────────────────────

/// Compute SHA-256 of `data` and return the lowercase hex string.
fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(data);
    hex::encode(hash)
}

// ── Minimal base64 decoder (no external dep) ──────────────────────────────────

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    const TABLE: &[u8; 128] = b"\
\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\
\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\
\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\x3e\xff\xff\xff\x3f\
\x34\x35\x36\x37\x38\x39\x3a\x3b\x3c\x3d\xff\xff\xff\xff\xff\xff\
\xff\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\
\x0f\x10\x11\x12\x13\x14\x15\x16\x17\x18\x19\xff\xff\xff\xff\xff\
\xff\x1a\x1b\x1c\x1d\x1e\x1f\x20\x21\x22\x23\x24\x25\x26\x27\x28\
\x29\x2a\x2b\x2c\x2d\x2e\x2f\x30\x31\x32\x33\xff\xff\xff\xff\xff";

    let input = input.trim().replace('\n', "").replace('\r', "");
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let bytes = input.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        let a = bytes[i];
        let b = bytes[i + 1];
        let c = bytes[i + 2];
        let d = bytes[i + 3];
        if a == b'=' { break; }
        let va = if a < 128 { TABLE[a as usize] } else { 0xff };
        let vb = if b < 128 { TABLE[b as usize] } else { 0xff };
        let vc = if c == b'=' { 0 } else if c < 128 { TABLE[c as usize] } else { 0xff };
        let vd = if d == b'=' { 0 } else if d < 128 { TABLE[d as usize] } else { 0xff };
        if va == 0xff || vb == 0xff || vc == 0xff || vd == 0xff {
            return Err(format!("invalid base64 char at offset {i}"));
        }
        out.push((va << 2) | (vb >> 4));
        if c != b'=' { out.push((vb << 4) | (vc >> 2)); }
        if d != b'=' { out.push((vc << 6) | vd); }
        i += 4;
    }
    Ok(out)
}

/// Standard base64 encode (no line wrapping), for the screenshot data URL.
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut chunks = data.chunks_exact(3);
    for c in &mut chunks {
        let n = (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
        out.push(T[(n >> 18) as usize & 0x3f] as char);
        out.push(T[(n >> 12) as usize & 0x3f] as char);
        out.push(T[(n >> 6) as usize & 0x3f] as char);
        out.push(T[n as usize & 0x3f] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(T[(n >> 18) as usize & 0x3f] as char);
            out.push(T[(n >> 12) as usize & 0x3f] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = (rem[0] as u32) << 16 | (rem[1] as u32) << 8;
            out.push(T[(n >> 18) as usize & 0x3f] as char);
            out.push(T[(n >> 12) as usize & 0x3f] as char);
            out.push(T[(n >> 6) as usize & 0x3f] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

/// Encode an RGBA buffer to PNG bytes (8-bit RGBA, no interlace). The exact zlib
/// bytes differ from Node's encoder, so the render gate compares decoded PIXELS,
/// never PNG-container bytes.
fn rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(rgba).expect("png data");
    }
    buf
}

/// Render the session's frozen display, scaled by `scale` (1/2/4), to a PNG data
/// URL. Returns (dataUrl, width, height).
fn render_screenshot(machine: &trx64_core::Machine, scale: usize) -> (String, u32, u32) {
    let scale = scale.max(1);
    let (w, h, rgba) = machine.render_canvas_rgba();
    let (ow, oh, out) = if scale == 1 {
        (w, h, rgba)
    } else {
        let ow = w * scale;
        let oh = h * scale;
        let mut out = vec![0u8; ow * oh * 4];
        for y in 0..oh {
            let sy = y / scale;
            for x in 0..ow {
                let sx = x / scale;
                let si = (sy * w + sx) * 4;
                let di = (y * ow + x) * 4;
                out[di..di + 4].copy_from_slice(&rgba[si..si + 4]);
            }
        }
        (ow, oh, out)
    };
    let png = rgba_to_png(ow as u32, oh as u32, &out);
    let url = format!("data:image/png;base64,{}", base64_encode(&png));
    (url, ow as u32, oh as u32)
}

// ── Connection handler ────────────────────────────────────────────────────────

async fn handle_connection(stream: TcpStream, addr: SocketAddr, state: SharedState) {
    let ws = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("[trx64] WS handshake failed from {addr}: {e}");
            return;
        }
    };

    eprintln!("[trx64] client connected: {addr}");
    let (mut tx, mut rx) = ws.split();

    while let Some(msg) = rx.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[trx64] recv error from {addr}: {e}");
                break;
            }
        };

        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Ping(data) => {
                let _ = tx.send(Message::Pong(data)).await;
                continue;
            }
            Message::Close(_) => break,
            _ => continue,
        };

        let response = match serde_json::from_str::<Request>(&text) {
            Ok(req) => dispatch(req, &state),
            Err(e) => Response::err(
                Value::Null,
                -32700,
                format!("Parse error: {e}"),
            ),
        };

        let out = serde_json::to_string(&response).unwrap_or_else(|e| {
            format!(r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"Internal serialization error: {e}"}}}}"#)
        });

        if let Err(e) = tx.send(Message::Text(out.into())).await {
            eprintln!("[trx64] send error to {addr}: {e}");
            break;
        }
    }

    eprintln!("[trx64] client disconnected: {addr}");
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    // Install a crash log hook before anything else.
    let crash_log_path = project_dir().join("runtime").join("daemon-crash.log");
    {
        let p = crash_log_path.clone();
        std::panic::set_hook(Box::new(move |info| {
            let msg = format!("[trx64] PANIC: {info}\n");
            eprintln!("{}", msg);
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&p, &msg);
        }));
    }

    let cli = Cli::parse();

    eprintln!("[trx64] project = {:?}", cli.project);

    // Boot the singleton session.
    let roms = rom_dir();
    eprintln!("[trx64] loading ROMs from {}", roms.display());

    let mut session = Session::new("integrated-1");
    match session.boot(&roms) {
        Ok(()) => {
            eprintln!(
                "[trx64] boot ok — reset pc = 0x{:04X} ({})",
                session.machine.cpu.pc,
                session.machine.cpu.pc
            );
        }
        Err(e) => {
            eprintln!("[trx64] WARN: ROM boot failed ({e}), running with blank machine");
        }
    }

    let state: SharedState = Arc::new(Mutex::new(State {
        session,
        breakpoints: Breakpoints::new(),
        observers: observers::ObserverRegistry::new(),
        type_buffer: Vec::new(),
        ctrl_frame: 0, // incremented on each debug/run|pause|continue; first pause → 1
        ctrl_stop: None,
        checkpoint_counter: 0,
        trace_definitions: std::collections::HashMap::new(),
    }));

    let addr: SocketAddr = format!("127.0.0.1:{}", cli.port).parse().unwrap();
    let listener = TcpListener::bind(addr).await.expect("failed to bind");
    eprintln!("[trx64] listening on ws://{addr}");

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    handle_connection(stream, peer, state).await;
                });
            }
            Err(e) => {
                eprintln!("[trx64] accept error: {e}");
            }
        }
    }
}
