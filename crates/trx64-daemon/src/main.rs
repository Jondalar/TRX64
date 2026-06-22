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

    fn err(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError { code, message: message.into() }),
        }
    }
}

// ── Shared state ──────────────────────────────────────────────────────────────

/// Singleton session, kept in memory for the daemon's lifetime.
struct State {
    session: Session,
}

type SharedState = Arc<Mutex<State>>;

// ── ROM directory resolution ──────────────────────────────────────────────────

fn rom_dir() -> PathBuf {
    let root = env::var("C64RE_ROOT").unwrap_or_else(|_| {
        "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP".to_string()
    });
    PathBuf::from(root).join("resources").join("roms")
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
    // Domain → channel mapping (= TS domainsToChannels). When the vic domain is
    // active the VIC must be ticked per CPU cycle (VIC-isolated run path); the
    // record filter then yields an EMPTY trace for a vic-only domain set (the
    // oracle's vic channel has no producer), and the normal cpu/memory frames
    // when those domains are requested.
    // FULL-MACHINE path (ADR-021): when the session has NOT been CPU-isolated
    // by a monitor inject, it is the assembled full-C64 boot (session/create →
    // session/run from the KERNAL reset vector). Route to the FullBus run loop
    // (PLA banking + per-cycle VIC + both CIAs + cross-chip IRQ/NMI + drive
    // catch-up). The isolated gates always inject first, so they keep their
    // FlatRam/CiaBus/VicBus paths untouched.
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
        // FULL-MACHINE traced run (boot-trace-short: c64-cpu + memory domains).
        // Drive PC samples are emitted only if the drive8-cpu channel is active.
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
        // drive8-cpu domain: run C64 + drive together, sampling drive PC per
        // C64 instruction boundary (ADR-015/ADR-012 isolation gate).
        // Collect steps first (avoids double-borrow of obs), then emit.
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
        // memory domain (no vic): route through the CIA-isolated bus so $DC00-$DDFF
        // accesses hit the cycle-exact 6526 chips. Behaves identically to flat RAM
        // for every non-CIA address, so plain CPU exercisers are unaffected.
        session.machine.run_for_cia(budget, &mut obs);
    } else {
        session.machine.run_for_with(budget, &mut obs);
    }
    if let Some(t) = session.trace.as_mut() {
        t.event_count += obs.event_count;
        t.buf.extend_from_slice(&obs.into_buf());
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
            // wr [lens] <addr> <byte..>  (lens optional; we accept & ignore it)
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
            // A monitor inject marks this session CPU-ISOLATED (not the full boot).
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
                            // set_flags is private; emulate via PLP-style decompose.
                            c.reg_p = (v as u8) & !0xa2; // clear N,Z from reg_p
                            c.flag_n = (v as u8) & 0x80;
                            c.flag_z = if (v as u8) & 0x02 != 0 { 0 } else { 1 };
                            done.push(format!("fl=${:02X}", v as u8));
                        }
                        _ => done.push(format!("unknown reg '{reg}'")),
                    }
                }
                // Keep the legacy snapshot in sync for session/state readers.
                session.machine.sync_after_monitor();
                // A register-set (notably `r pc=`) marks this session isolated.
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
            // If no run happened (empty buf), still write a header-only file.
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

        "session/run" => {
            let mut st = state.lock().unwrap();
            let cycles = req
                .params
                .get("cycles")
                .and_then(|v| v.as_u64())
                .unwrap_or(19705);
            run_cycle_budget(&mut st.session, cycles);
            // TS normal-completion result = { c64Cycles } only.
            Response::ok(id, json!({ "c64Cycles": st.session.machine.clk }))
        }

        "session/state" => {
            let st = state.lock().unwrap();
            let machine = &st.session.machine;
            let cpu = &machine.cpu;
            // VIC state snapshot (= TS session/state vic block). Computed from the
            // VIC register file + CIA2 PA (VIC bank). screen/chargen/bitmap ptrs
            // derive from $D018; mode from $D011/$D016; border/bg from $D020/$D021.
            let v = |off: u8| machine.vic.read_reg(off);
            let d011 = v(0x11);
            let d016 = v(0x16);
            let d018 = v(0x18);
            let mode = ((d011 >> 5) & 3) | (((d016 >> 4) & 1) << 2);
            let screen_ptr = (((d018 >> 4) & 0xf) as u64) << 10;
            let chargen_ptr = (((d018 >> 1) & 7) as u64) << 11;
            let bitmap_ptr = if d018 & 8 != 0 { 0x2000u64 } else { 0 };
            // VIC bank = (cia2 PRA & DDRA & 3) ^ 3.
            let cia2_pra = machine.cia2.peek(0xdd00);
            let cia2_ddra = machine.cia2.peek(0xdd02);
            let bank = ((cia2_pra & cia2_ddra & 3) ^ 3) as u64;
            // Vectors: banked reads of $FFFE/$FFFA (KERNAL) + RAM $0314/$0318.
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

        // Start a CPU/memory trace; returns outputPath (.duckdb). The product
        // authority is the sibling .c64retrace, written at trace/run/stop.
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
            // Meta JSON embedded in the .c64retrace header. The oracle does not
            // diff header meta (volatile), so only the shape must be sane.
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

        other => {
            Response::err(id, -32601, format!("Method not found: {other}"))
        }
    }
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

    let state: SharedState = Arc::new(Mutex::new(State { session }));

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
