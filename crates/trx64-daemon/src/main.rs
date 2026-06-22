//! trx64-daemon — WS JSON-RPC 2.0 server on 127.0.0.1:4312.
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

fn main() {
    eprintln!("trx64-daemon 0.0.1 — skeleton. WS surface + emulation ported per loop/backlog.md");
    // TODO(loop): bind ws://127.0.0.1:4312, JSON-RPC 2.0 + binary frames [type:u8][seq:u32 LE].
}
