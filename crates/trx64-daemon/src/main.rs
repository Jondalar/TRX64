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
use trx64_session::Session;

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

// ── RPC method dispatch ───────────────────────────────────────────────────────

fn dispatch(req: Request, state: &SharedState) -> Response {
    let id = req.id.clone();
    match req.method.as_str() {
        "ping" => {
            Response::ok(id, json!({}))
        }

        "session/create" => {
            let mut st = state.lock().unwrap();
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
            // STUB: do not emulate; machine state is unchanged.
            // Real execution is the cpu-6510 loop item.
            Response::ok(id, json!({ "state": null }))
        }

        "session/state" => {
            let st = state.lock().unwrap();
            let machine = &st.session.machine;
            let cpu = &machine.cpu;
            Response::ok(id, json!({
                "c64Cycles": machine.clk,
                "driveCycles": 0,
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
                }
            }))
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
