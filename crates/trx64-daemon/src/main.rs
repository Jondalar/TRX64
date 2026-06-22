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
use trx64_core::NullSink;
use trx64_session::{Session, TraceState};
use trx64_trace::{FrameSink, TraceChannels, TracingObserver};

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
    /// Queued PETSCII chars for session/type (stub, count tracked only).
    #[allow(dead_code)]
    type_buffer: Vec<u8>,
    /// Monotonic controller-state counter; increments on each debug/run|pause|continue.
    ctrl_frame: u64,
    /// Last stop reason (set on pause, cleared on continue/run).
    ctrl_stop: Option<CtrlStop>,
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
    let full_machine = session.machine.full_assembled && !session.injected;

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

    if full_machine {
        let mut steps: Vec<(u16, u8, u8, u8, u8, u8, u64)> = Vec::new();
        session.machine.run_for_full(budget, &mut obs, |pc, a, x, y, sp, p, drv_clk| {
            steps.push((pc, a, x, y, sp, p, drv_clk));
        });
        if drive_cpu_active {
            for (pc, a, x, y, sp, p, drv_clk) in steps {
                obs.emit_drive_step(pc, a, x, y, sp, p, drv_clk);
            }
        }
    } else if drive_cpu_active {
        let mut steps: Vec<(u16, u8, u8, u8, u8, u8, u64)> = Vec::new();
        session.machine.run_for_drive_sampled(budget, &mut obs, |pc, a, x, y, sp, p, drv_clk| {
            steps.push((pc, a, x, y, sp, p, drv_clk));
        });
        for (pc, a, x, y, sp, p, drv_clk) in steps {
            obs.emit_drive_step(pc, a, x, y, sp, p, drv_clk);
        }
    } else if vic_active {
        session.machine.run_for_vic(budget, &mut obs);
    } else if channels.mem {
        session.machine.run_for_cia(budget, &mut obs);
    } else {
        session.machine.run_for_with(budget, &mut obs);
    }
    if let Some(t) = session.trace.as_mut() {
        t.event_count += obs.event_count;
        t.buf.extend_from_slice(&obs.into_buf());
    }
}

/// Step exactly one instruction (for stepInto / stepOver / until loops).
fn step_one_instruction(session: &mut Session) {
    let full_machine = session.machine.full_assembled && !session.injected;
    let mut obs = NullSink;
    if full_machine {
        session.machine.run_for_full_capped(999_999, 1, &mut obs, |_, _, _, _, _, _, _| {});
    } else {
        session.machine.run_for_capped(999_999, 1, &mut obs);
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
            session.machine.poke(addr, &bytes);
            session.injected = true;
            Ok(format!("wrote {} byte(s) @ ${:04X} (cpu)", bytes.len(), addr))
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
            let mut instructions_elapsed: u64 = 0;
            let mut budget_exhausted = false;
            let mut halted = false;

            loop {
                let current_clk = st.session.machine.clk;
                if current_clk.wrapping_sub(start_clk) >= cycle_budget {
                    budget_exhausted = true;
                    break;
                }
                step_one_instruction(&mut st.session);
                instructions_elapsed += 1;
                if st.session.machine.cpu6510.reg_pc == target_addr {
                    halted = true;
                    break;
                }
            }

            let final_pc = st.session.machine.cpu6510.reg_pc;
            let cycles_elapsed = st.session.machine.clk.wrapping_sub(start_clk);
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
            });
            Response::ok(id, json!(bp_id))
        }

        "listBreakpoints" => {
            // TS BreakpointManager.list() returns specs with hitCount and _ignoreRemaining set on add().
            let st = state.lock().unwrap();
            let list: Vec<Value> = st.breakpoints.api_entries.iter().map(|e| {
                let mut obj = json!({
                    "id": e.id,
                    "predicate": { "kind": "pc", "pc": e.pc as u64 },
                    "action": e.action,
                    "enabled": e.enabled,
                    "hitCount": 0u64,
                    "_ignoreRemaining": 0u64
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
            Response::ok(id, json!({
                "sessionId": "integrated-1",
                "mode": "true-drive",
                "diskPath": "",
                "attached": true,
                "c64Cycles": c64_cycles,
                "pc": pc,
                "trace": null
            }))
        }

        "session/list" => {
            let st = state.lock().unwrap();
            let c64_cycles = st.session.machine.clk;
            Response::ok(id, json!([{
                "sessionId": st.session.id,
                "mode": "true-drive",
                "diskPath": "",
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
            let st = state.lock().unwrap();
            let c64_cycles = st.session.machine.clk;
            // Stub: no real keyboard emulation.
            Response::ok(id, json!({
                "c64Cycles": c64_cycles,
                "queued": 0
            }))
        }

        "session/joystick_set" => {
            Response::ok(id, json!({ "ok": true }))
        }

        "session/screenshot" => {
            Response::err(id, -32001,
                "NOT_IMPLEMENTED: session/screenshot requires VIC framebuffer (vic-render future item)")
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
            let bps = st.breakpoints.list_vice_json();
            let c = &st.session.machine.cpu6510;
            let pc = c.reg_pc as u64;
            let cycles = st.session.machine.clk;
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
            let bps = st.breakpoints.list_vice_json();
            let c = &st.session.machine.cpu6510;
            let pc = c.reg_pc as u64;
            let cycles = st.session.machine.clk;
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
            Response::ok(id, json!({
                "ok": false,
                "detail": "NOT_IMPLEMENTED: runtime/swap_disk_and_continue requires disk image support"
            }))
        }

        // ── media/* ──────────────────────────────────────────────────────────

        "media/ingress" => {
            Response::ok(id, json!({
                "ok": false,
                "detail": "NOT_IMPLEMENTED: media/ingress requires disk image support"
            }))
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
            Response::ok(id, json!({
                "run": {
                    "runId": run_id,
                    "definitionId": "live-capture",
                    "definitionVersion": 1,
                    "cycleStart": cycle_start,
                    "marks": [],
                    "eventCount": 0,
                    "bytesWritten": 0
                },
                "outputPath": output.to_string_lossy(),
                "domains": domains
            }))
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

        // ── vic/* — NOT_IMPLEMENTED ───────────────────────────────────────────

        m if m.starts_with("vic/") => {
            Response::err(id, -32001,
                format!("NOT_IMPLEMENTED: {m}: VIC framebuffer not yet available"))
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
        type_buffer: Vec::new(),
        ctrl_frame: 0, // incremented on each debug/run|pause|continue; first pause → 1
        ctrl_stop: None,
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
