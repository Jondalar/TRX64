//! trx64-ffi — typed uniffi bindings for embedding TRX64 in a native Swift app.
//!
//! WHY THIS CRATE EXISTS (the repo cut, decided 2026-06-25, TRX64-App README §7):
//! the typed-binding DEFINITION lives HERE, in TRX64, so the typed Swift API is
//! versioned WITH the runtime and cannot drift. The TRX64-App repo only pins a
//! TRX64 commit, builds the XCFramework, and holds the Swift UI.
//!
//! WHAT IT IS: a TYPED FAÇADE over the daemon's `dispatch()` + `NotifyHub`. Every
//! typed method internally builds a JSON-RPC `Request`, calls the SAME synchronous,
//! socket-free `trx64_daemon::dispatch()` the WebSocket transport uses, and
//! deserializes the JSON `Response` into a typed Rust struct that uniffi exposes to
//! Swift. The typed methods do NOT reimplement any runtime logic — they are thin
//! typed wrappers over one shared handler set, which is exactly what keeps them
//! non-drifting against the daemon contract.
//!
//! - [`Runtime`] (uniffi object) holds the `SharedState` (one machine per process,
//!   per the Single-Path / One-Machine-Per-Process contract).
//! - Typed RESULT records mirror the `json!({...})` each handler returns, field for
//!   field (see `API.md` for the full surface).
//! - [`EventListener`] (uniffi callback interface) + [`Runtime::set_listener`] map
//!   each `NotifyHub` broadcast to a typed [`RuntimeEvent`], on a forwarder thread.
//! - [`Runtime::call`] is the raw-JSON escape hatch → 100 % method coverage for the
//!   long-tail methods not in the typed surface.
//! - [`Trx64Error`] is a typed uniffi error (not a JSON error blob).
//!
//! THREADING / LIFETIME NOTES (uniffi gotchas handled here):
//! - `dispatch` takes `&SharedState` (an `Arc<Mutex<State>>`), so `Runtime` is
//!   `Send + Sync` and all methods take `&self`. uniffi serializes nothing for us;
//!   the `Mutex` inside `SharedState` is the single lock.
//! - The event forwarder is a dedicated OS thread that BLOCK-drains a tokio mpsc
//!   `UnboundedReceiver` (`blocking_recv`, no async runtime needed) fed by a
//!   `NotifyHub` subscription, and calls `EventListener::on_event` per message. The
//!   subscription guard + a stop flag are held in the `Runtime`; `Drop` joins the
//!   thread so the callback object outlives every `on_event` call (no dangling
//!   Swift callback).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::{json, Value};

use trx64_daemon::streaming::NotifySub;
use trx64_daemon::{
    create_embedded_state, dispatch, notify_hub, pull_audio_drain, pull_frame_buffer, Request,
    Response, SharedState, AUDIO_SAMPLE_RATE,
};

uniffi::setup_scaffolding!();

mod events;
mod records;

pub use events::{EventListener, RuntimeEvent};
pub use records::*;

// ── Errors ─────────────────────────────────────────────────────────────────────

/// A typed dispatch error — surfaced to Swift as a thrown error instead of the
/// JSON-RPC error blob. Each typed method returns `Result<_, Trx64Error>` →
/// uniffi maps it to a Swift `throws` function.
#[derive(Debug, uniffi::Error)]
pub enum Trx64Error {
    /// The runtime could not be constructed (e.g. ROMs not found at `rom_dir`).
    Boot { message: String },
    /// A JSON-RPC handler returned an error (`code`, `message`).
    Dispatch { code: i64, message: String },
    /// The handler's JSON response did not match the expected typed shape — a bug in
    /// the façade or a contract change. The raw JSON is included for diagnosis.
    Decode { message: String },
    /// Invalid argument supplied by the caller (e.g. un-decodable base64).
    InvalidArgument { message: String },
}

impl std::fmt::Display for Trx64Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Trx64Error::Boot { message } => write!(f, "boot error: {message}"),
            Trx64Error::Dispatch { code, message } => {
                write!(f, "dispatch error {code}: {message}")
            }
            Trx64Error::Decode { message } => write!(f, "decode error: {message}"),
            Trx64Error::InvalidArgument { message } => write!(f, "invalid argument: {message}"),
        }
    }
}
impl std::error::Error for Trx64Error {}

fn decode<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, Trx64Error> {
    serde_json::from_value(v.clone()).map_err(|e| Trx64Error::Decode {
        message: format!("{e}: {v}"),
    })
}

// ── The Runtime object ───────────────────────────────────────────────────────

/// The embedded TRX64 runtime — one machine, shared. Constructed with the ROM
/// directory; holds the `SharedState` and the event-forwarder thread.
#[derive(uniffi::Object)]
pub struct Runtime {
    state: SharedState,
    /// Event forwarder bookkeeping (subscription guard + stop flag + join handle).
    /// `Mutex<Option<...>>` so `set_listener` can replace an existing forwarder and
    /// `Drop` can take + join it.
    forwarder: Mutex<Option<Forwarder>>,
}

struct Forwarder {
    /// `Option` so `Drop` can explicitly drop the subscription (closing the hub's
    /// sender → the forwarder's `blocking_recv` wakes with `None`) BEFORE joining.
    /// Struct fields drop only AFTER the `Drop::drop` body, so without this the join
    /// would deadlock against a still-open channel.
    sub: Option<NotifySub>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        // 1. Signal stop (in case a message is in flight). 2. Unsubscribe NOW — this
        // drops the hub's `tx`, closing the channel so `blocking_recv` returns `None`
        // and the loop exits. 3. Join the (now-terminating) thread.
        self.stop.store(true, Ordering::SeqCst);
        drop(self.sub.take());
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Runtime {
    /// Build + call `dispatch`, unwrapping the typed JSON `result` or mapping the
    /// JSON-RPC `error` to a typed [`Trx64Error`].
    fn rpc(&self, method: &str, params: Value) -> Result<Value, Trx64Error> {
        let req = Request {
            jsonrpc: "2.0".to_string(),
            id: json!(1),
            method: method.to_string(),
            params,
        };
        let Response { result, error, .. } = dispatch(req, &self.state);
        if let Some(err) = error {
            return Err(Trx64Error::Dispatch {
                code: err.code,
                message: err.message,
            });
        }
        Ok(result.unwrap_or(Value::Null))
    }

    /// Build + call `dispatch`, returning the raw response as a JSON STRING (the
    /// escape-hatch path) — preserves error blobs verbatim.
    fn rpc_raw(&self, method: &str, params: Value) -> String {
        let req = Request {
            jsonrpc: "2.0".to_string(),
            id: json!(1),
            method: method.to_string(),
            params,
        };
        let resp = dispatch(req, &self.state);
        serde_json::to_string(&resp).unwrap_or_else(|e| {
            format!(r#"{{"jsonrpc":"2.0","id":1,"error":{{"code":-32603,"message":"serialize: {e}"}}}}"#)
        })
    }
}

#[uniffi::export]
impl Runtime {
    /// Construct the runtime: boot a fresh singleton machine from ROMs in `rom_dir`,
    /// cold-reset to READY, paused. One machine per process (the runtime core is
    /// single-machine-per-process by design).
    #[uniffi::constructor]
    pub fn new(rom_dir: String) -> Result<Arc<Self>, Trx64Error> {
        let state = create_embedded_state(std::path::Path::new(&rom_dir))
            .map_err(|e| Trx64Error::Boot { message: format!("{e:?}") })?;
        Ok(Arc::new(Runtime {
            state,
            forwarder: Mutex::new(None),
        }))
    }

    // ── session ──────────────────────────────────────────────────────────────

    /// Create (attach to) the singleton session. `pal` is accepted for the wire
    /// contract; the singleton machine is PAL and is not reconstructed on attach.
    pub fn create_session(&self, pal: bool) -> Result<SessionInfo, Trx64Error> {
        let v = self.rpc("session/create", json!({ "pal": pal }))?;
        decode(v)
    }

    /// Full machine state: CPU registers, cycles, run-state, VIC, flow, vectors, SID.
    pub fn state(&self) -> Result<MachineState, Trx64Error> {
        let v = self.rpc("session/state", json!({}))?;
        decode(v)
    }

    /// Reset the machine. `cold` = full power-cycle (fresh DRAM); else warm (RAM kept).
    pub fn reset(&self, cold: bool) -> Result<ResetResult, Trx64Error> {
        let mode = if cold { "cold" } else { "soft" };
        let v = self.rpc("session/reset", json!({ "mode": mode }))?;
        decode(v)
    }

    /// PNG bytes of the current displayed frame (decoded from the handler's data URL).
    pub fn screenshot(&self) -> Result<Vec<u8>, Trx64Error> {
        let v = self.rpc("session/screenshot", json!({}))?;
        let data_url = v
            .get("dataUrl")
            .and_then(|d| d.as_str())
            .ok_or_else(|| Trx64Error::Decode {
                message: format!("session/screenshot: no dataUrl in {v}"),
            })?;
        let b64 = data_url
            .split_once(',')
            .map(|(_, d)| d)
            .unwrap_or(data_url);
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| Trx64Error::Decode {
                message: format!("screenshot base64: {e}"),
            })
    }

    // ── live A/V (pull) ──────────────────────────────────────────────────────────
    //
    // Two ADDITIVE typed PULL methods for the native app's render/audio loops. A/V is
    // BINARY and bypasses the JSON-RPC `dispatch`/event channel (JSON can't carry a
    // frame/PCM efficiently), so these reach the core DIRECTLY via the daemon's
    // `pull_*` helpers — the SAME `&SharedState` lock every handler uses. They do NOT
    // call `dispatch` or any existing method. The app pulls at its own cadence (per
    // video frame, per audio callback). `Vec<u8>`/`Vec<i16>` map to Swift `Data`/
    // `[Int16]`, so no base64 and no JSON on the hot path.

    /// The CURRENT displayed frame at FULL resolution as a palette + index image (the
    /// 384×272 VICE PAL canvas — the SAME `displayed` buffer `screenshot()` and the
    /// scrub thumbnails come from, here full-res + un-palettized). Pull this once per
    /// video frame (~50 Hz) and blit it. See [`FrameBuffer`] for the draw recipe.
    pub fn frame_buffer(&self) -> FrameBuffer {
        let fb = pull_frame_buffer(&self.state);
        FrameBuffer {
            width: fb.width,
            height: fb.height,
            palette: fb.palette,
            indices: fb.indices,
        }
    }

    /// Drain + return the SID PCM accumulated since the last `audioDrain()` — mono
    /// `Int16` at [`audioSampleRate`] (44100 Hz), ready to fill an `AVAudioPCMBuffer`
    /// (`int16ChannelData`). Draining EMPTIES the buffer, so repeated calls don't
    /// re-deliver. Pull this in the AVAudioEngine source callback. The first call
    /// installs the SID capture hook + spawns the audio render thread (which constructs
    /// reSID ONCE and primes it) and returns empty (no cycles have elapsed yet);
    /// thereafter each call returns the samples for exactly the cycles run since the
    /// previous drain. Rendering is a CONTINUOUS persistent-engine render drained from a
    /// PCM ring (mirrors the `--stream` loop / C64RE Spec 768): the render thread holds
    /// one reSID engine for the whole session, fed by a SID write-ring; `audioDrain()`
    /// just pops the PCM ring — no per-pull reconstruct, so the stream is continuous
    /// across drain boundaries (no clicks, no hum).
    pub fn audio_drain(&self) -> Vec<i16> {
        pull_audio_drain(&self.state).samples
    }

    /// The runtime's fixed SID sample rate (Hz) — 44100. Fetch once when configuring
    /// the AVAudioEngine format; every [`audioDrain`] sample is mono at this rate.
    pub fn audio_sample_rate(&self) -> u32 {
        AUDIO_SAMPLE_RATE
    }

    // ── run / step ─────────────────────────────────────────────────────────────

    /// Resume free-running (an embedded host drives the per-frame loop itself; this
    /// flips run-state so the controller behaves as running). Returns debug state.
    pub fn run(&self) -> Result<DebugState, Trx64Error> {
        let v = self.rpc("debug/run", json!({ "source": "ffi" }))?;
        decode(v)
    }

    /// Pause the machine. Returns debug state (`stop.reason = "pause"`).
    pub fn pause(&self) -> Result<DebugState, Trx64Error> {
        let v = self.rpc("debug/pause", json!({ "source": "ffi" }))?;
        decode(v)
    }

    /// Single-step one instruction; returns the new full machine state.
    pub fn step(&self) -> Result<MachineState, Trx64Error> {
        // debug/step returns build_debug_state; session/state returns the richer
        // MachineState. Step then read state, so the typed return is the full state.
        self.rpc("debug/step", json!({ "source": "ffi" }))?;
        let v = self.rpc("session/state", json!({}))?;
        decode(v)
    }

    /// Advance exactly `n` C64 cycles (may stop early on a breakpoint).
    pub fn run_cycles(&self, n: u64) -> Result<RunResult, Trx64Error> {
        let v = self.rpc("session/run", json!({ "cycles": n }))?;
        decode(v)
    }

    /// Set the pacing mode + ratio (`pal` / `warp` / `fixed-ratio`).
    pub fn set_pacing(&self, pacing: Pacing) -> Result<DebugState, Trx64Error> {
        let v = self.rpc(
            "session/set_pacing",
            json!({ "mode": pacing.mode, "ratio": pacing.ratio }),
        )?;
        decode(v)
    }

    // ── input ──────────────────────────────────────────────────────────────────

    /// Press a key (c64re key id, e.g. "A", "RETURN", "RUN_STOP", "L_SHIFT").
    pub fn key_down(&self, key: String) -> Result<(), Trx64Error> {
        self.rpc("session/key_down", json!({ "key": key }))?;
        Ok(())
    }

    /// Release a key.
    pub fn key_up(&self, key: String) -> Result<(), Trx64Error> {
        self.rpc("session/key_up", json!({ "key": key }))?;
        Ok(())
    }

    /// Type a PETSCII string through the keyboard matrix (e.g. "LOAD\"*\",8,1\r").
    pub fn type_text(&self, text: String) -> Result<TypeResult, Trx64Error> {
        let v = self.rpc("session/type", json!({ "text": text }))?;
        decode(v)
    }

    /// Set the joystick state for a port (1 or 2).
    pub fn joystick(&self, port: u8, state: JoystickState) -> Result<(), Trx64Error> {
        if state.is_idle() {
            self.rpc("session/joystick_clear", json!({ "port": port }))?;
        } else {
            self.rpc(
                "session/joystick_set",
                json!({
                    "port": port,
                    "up": state.up, "down": state.down,
                    "left": state.left, "right": state.right,
                    "fire": state.fire,
                }),
            )?;
        }
        Ok(())
    }

    /// Load a PRG image into RAM (honours its 2-byte load-address header). Does not run.
    pub fn load_prg(&self, bytes: Vec<u8>) -> Result<LoadResult, Trx64Error> {
        // session/load_prg reads a file path; write the bytes to a temp file so the
        // typed bytes-API maps onto it without changing the handler.
        let path = write_temp_prg(&bytes)?;
        let v = self.rpc("session/load_prg", json!({ "prg_path": path }));
        let _ = std::fs::remove_file(&path);
        decode(v?)
    }

    /// Load + autostart a PRG (BASIC `RUN` for $0801 loads, else JMP to load address).
    pub fn run_prg(&self, bytes: Vec<u8>) -> Result<RunPrgResult, Trx64Error> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let v = self.rpc("runtime/run_prg", json!({ "bytes_b64": b64 }))?;
        decode(v)
    }

    // ── monitor ──────────────────────────────────────────────────────────────

    /// Run one monitor-REPL command (the full VICE-style monitor: `d`, `m`, `r`,
    /// `g`, `bk`, `obs`, `flow`, …). Returns the monitor output text.
    pub fn monitor_exec(&self, command: String) -> Result<String, Trx64Error> {
        let v = self.rpc("monitor/exec", json!({ "command": command }))?;
        Ok(v.get("output")
            .and_then(|o| o.as_str())
            .unwrap_or("")
            .to_string())
    }

    // ── media ──────────────────────────────────────────────────────────────────

    /// Mount a disk (.d64/.g64) or cartridge (.crt) from a host path. `slot` is
    /// accepted for the wire contract (the drive is slot 8).
    pub fn mount(&self, path: String, slot: u8) -> Result<MediaResult, Trx64Error> {
        let _ = slot;
        let v = self.rpc("media/mount", json!({ "path": path }))?;
        decode(v)
    }

    /// Swap the mounted disk for another (drive stays attached).
    pub fn swap(&self, path: String) -> Result<MediaResult, Trx64Error> {
        let v = self.rpc("media/swap", json!({ "path": path }))?;
        decode(v)
    }

    /// Unmount a slot. `role` = "drive8" (disk) or "cartridge"; default drive8.
    pub fn unmount(&self, slot: u8) -> Result<UnmountResult, Trx64Error> {
        let _ = slot;
        let v = self.rpc("media/unmount", json!({}))?;
        decode(v)
    }

    /// The recent-media list (newest-first, mount timestamps).
    pub fn recent_media(&self) -> Result<Vec<MediaEntry>, Trx64Error> {
        let v = self.rpc("media/recent", json!({}))?;
        decode(v)
    }

    /// The attached cartridge's status, or `None` when no cart is mounted.
    pub fn cart_status(&self) -> Result<Option<CartStatus>, Trx64Error> {
        let v = self.rpc("session/cart_status", json!({}))?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(decode(v)?))
    }

    // ── trace ──────────────────────────────────────────────────────────────────

    /// Start a capture-all trace over the given domains (e.g. ["c64-cpu","memory"]).
    pub fn trace_start(&self, domains: Vec<String>) -> Result<TraceRun, Trx64Error> {
        let v = self.rpc("trace/start_domains", json!({ "domains": domains }))?;
        decode(v.get("run").cloned().unwrap_or(v))
    }

    /// Stop the active trace; returns the finalized run status.
    pub fn trace_stop(&self) -> Result<TraceStatus, Trx64Error> {
        let v = self.rpc("trace/run/stop", json!({}))?;
        decode(v)
    }

    /// Index a finalized `.c64retrace` into its sibling DuckDB. `path` defaults to the
    /// last finalized trace when `None`.
    pub fn trace_index(&self, path: Option<String>) -> Result<IndexResult, Trx64Error> {
        let params = match path {
            Some(p) => json!({ "retrace_path": p }),
            None => json!({}),
        };
        let v = self.rpc("trace/index", params)?;
        decode(v)
    }

    /// Build a `.c64retrace` (+ DuckDB) from the checkpoint ring's cycle window.
    pub fn build_trace_from_ring(&self, start: u64, end: u64) -> Result<TraceFile, Trx64Error> {
        let v = self.rpc(
            "trace/build_from_ring",
            json!({ "cycle_start": start, "cycle_end": end }),
        )?;
        decode(v)
    }

    // ── checkpoint / scrub ─────────────────────────────────────────────────────

    /// Capture a checkpoint into the in-memory ring (rewind / scrub).
    pub fn checkpoint_capture(&self) -> Result<Checkpoint, Trx64Error> {
        let v = self.rpc("checkpoint/capture", json!({}))?;
        decode(v.get("ref").cloned().unwrap_or(v))
    }

    /// Restore a checkpoint by id. `then` = "pause" | "run" | "keep"; `render`
    /// re-presents the rolled-back frame.
    pub fn checkpoint_restore(
        &self,
        id: String,
        then: String,
        render: bool,
    ) -> Result<(), Trx64Error> {
        self.rpc(
            "checkpoint/restore",
            json!({ "id": id, "then": then, "render": render }),
        )?;
        Ok(())
    }

    /// List the checkpoints currently in the ring.
    pub fn checkpoint_list(&self) -> Result<Vec<Checkpoint>, Trx64Error> {
        let v = self.rpc("checkpoint/list", json!({}))?;
        decode(v.get("checkpoints").cloned().unwrap_or(v))
    }

    /// Scrub-filmstrip thumbnails (downscaled, palette + indices) per checkpoint.
    pub fn thumbnails(&self) -> Result<Vec<Thumbnail>, Trx64Error> {
        let v = self.rpc("checkpoint/thumbnails", json!({}))?;
        decode(v.get("thumbnails").cloned().unwrap_or(v))
    }

    // ── reverse-debug ──────────────────────────────────────────────────────────

    /// Inspect-step backward `n` instructions over the reverse-debug history.
    pub fn reverse_step(&self, n: u64) -> Result<ReverseResult, Trx64Error> {
        let v = self.rpc("runtime/reverse_step", json!({ "n": n }))?;
        decode(v)
    }

    /// Who wrote `addr` recently (PC + cycle + old/new value), most recent first.
    pub fn who_wrote(&self, addr: u16, limit: u32) -> Result<Vec<Writer>, Trx64Error> {
        let v = self.rpc("runtime/who_wrote", json!({ "addr": addr, "limit": limit }))?;
        decode(v.get("writers").cloned().unwrap_or(v))
    }

    /// Typed, by-ID diff of two checkpoint anchors (Spec time-travel-tooling Piece 1).
    /// Resolves `id_a` / `id_b` from the in-memory ring, runs the snapshot-diff compute
    /// on their two machine states, and returns a typed [`SnapshotDiff`] (RAM grouped
    /// into contiguous runs + per-chip register-change lists). READ-ONLY — the live
    /// machine is byte-identical after the call. Pair with [`Runtime::checkpoint_list`]
    /// for the ids; works on a ring loaded via [`Runtime::ringbuffer_restore`] too.
    pub fn diff_checkpoints(
        &self,
        id_a: String,
        id_b: String,
    ) -> Result<SnapshotDiff, Trx64Error> {
        let v = self.rpc(
            "runtime/diff_checkpoints",
            json!({ "idA": id_a, "idB": id_b }),
        )?;
        decode(v)
    }

    /// Triage a crash: walk the cause chain from the (optional) PC, else the live PC.
    pub fn crash_triage(&self) -> Result<TriageChain, Trx64Error> {
        let v = self.rpc("runtime/crash_triage", json!({}))?;
        decode(v)
    }

    /// Set the reverse-debug history depth in seconds (resizes the delta buffers).
    pub fn set_reverse_depth(&self, seconds: u64) -> Result<ReverseDepth, Trx64Error> {
        let v = self.rpc("runtime/set_reverse_depth", json!({ "seconds": seconds }))?;
        decode(v)
    }

    // ── snapshot ───────────────────────────────────────────────────────────────

    /// Dump a full machine snapshot (.c64re) to `path`.
    pub fn dump(&self, path: String) -> Result<SnapshotInfo, Trx64Error> {
        let v = self.rpc("snapshot/dump", json!({ "path": path }))?;
        decode(v)
    }

    /// Undump (restore) a machine snapshot (.c64re) from `path`.
    pub fn undump(&self, path: String) -> Result<SnapshotInfo, Trx64Error> {
        let v = self.rpc("snapshot/undump", json!({ "path": path }))?;
        decode(v)
    }

    // ── ringbuffer dump/restore (Spec time-travel-tooling Piece 2) ──────────────

    /// Serialize the WHOLE reverse-debug buffer (checkpoint ring + delta ring +
    /// cpu-history ring + the "current" anchor) into one gzipped `.c64rering` file at
    /// `path` — the tester→dev hand-off. Returns a [`RingDumpInfo`] summary. READ-ONLY
    /// w.r.t. the machine.
    pub fn ringbuffer_dump(&self, path: String) -> Result<RingDumpInfo, Trx64Error> {
        let v = self.rpc("ringbuffer/dump", json!({ "path": path }))?;
        decode(v)
    }

    /// Load a `.c64rering` file from `path`, reconstruct the checkpoint / delta /
    /// cpu-history rings into the runtime, and restore the machine to the dump's
    /// "current" anchor. After this the scrub filmstrip ([`Runtime::checkpoint_list`] /
    /// [`Runtime::thumbnails`]), [`Runtime::reverse_step`], [`Runtime::who_wrote`],
    /// `chis` (via the monitor), and [`Runtime::diff_checkpoints`] all work on the
    /// loaded buffer. Returns a [`RingDumpInfo`] summary.
    pub fn ringbuffer_restore(&self, path: String) -> Result<RingDumpInfo, Trx64Error> {
        let v = self.rpc("ringbuffer/restore", json!({ "path": path }))?;
        decode(v)
    }

    // ── escape hatch ───────────────────────────────────────────────────────────

    /// Raw JSON-RPC escape hatch: send any `method` with `params_json` (a JSON object
    /// string) and get the full JSON-RPC response string back. Covers every method
    /// not in the typed façade — 100 % coverage. Errors are returned IN the JSON
    /// (not raised) so the long-tail surface is fully introspectable.
    pub fn call(&self, method: String, params_json: String) -> String {
        let params: Value = serde_json::from_str(&params_json).unwrap_or(Value::Null);
        self.rpc_raw(&method, params)
    }

    // ── events ─────────────────────────────────────────────────────────────────

    /// Register (or replace) the typed event listener. Subscribes a forwarder to the
    /// `NotifyHub`; each broadcast is mapped to a typed [`RuntimeEvent`] and delivered
    /// via [`EventListener::on_event`] on a dedicated thread. Pass-through `None` is
    /// not offered — call [`Runtime::clear_listener`] to detach.
    pub fn set_listener(&self, listener: Box<dyn EventListener>) {
        // uniffi delivers a callback-interface impl as a `Box<dyn ...>`; the forwarder
        // thread needs shared ownership, so wrap it in an `Arc`.
        let listener: Arc<dyn EventListener> = Arc::from(listener);
        let hub = notify_hub(&self.state);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let sub = hub.subscribe(tx);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("trx64-ffi-events".into())
            .spawn(move || events::forward_loop(rx, listener, stop_thread))
            .expect("spawn ffi event forwarder");
        // Replacing an existing forwarder drops it (joins its thread) first.
        let mut guard = self.forwarder.lock().unwrap();
        *guard = Some(Forwarder {
            sub: Some(sub),
            stop,
            join: Some(join),
        });
    }

    /// Detach the event listener (stops + joins the forwarder thread).
    pub fn clear_listener(&self) {
        let mut guard = self.forwarder.lock().unwrap();
        *guard = None;
    }
}

/// Write PRG bytes to a unique temp file; returns the path. The caller deletes it.
fn write_temp_prg(bytes: &[u8]) -> Result<String, Trx64Error> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("trx64-ffi-{nanos}.prg"));
    std::fs::write(&path, bytes).map_err(|e| Trx64Error::InvalidArgument {
        message: format!("write temp prg: {e}"),
    })?;
    Ok(path.to_string_lossy().to_string())
}
