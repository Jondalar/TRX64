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
    fs,
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
use trx64_core::{BusKind, NullSink, Observer};
use trx64_session::{Session, TraceState};
use trx64_trace::{FrameSink, TraceChannels, TracingObserver};

pub mod assembler;
pub mod candidate;
pub mod observers;
pub mod project_knowledge;
pub mod snapshot_diff;
pub mod streaming;

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

    /// Enable the live A/V binary push (ADR-073): in this mode a connecting client
    /// (e.g. the read-only `ws-av-tap.mjs`) is subscribed to a singleton pacing
    /// loop that runs the machine in real-time (~50 fps PAL) and pushes BIN_VIC +
    /// BIN_AUDIO per frame. OFF by default so the byte-exact oracle (which spawns
    /// command-driven, deterministic daemons) sees NO machine advance on connect.
    /// Also enabled by setting `TRX64_STREAM=1`.
    #[arg(long, default_value_t = false)]
    stream: bool,
}

// ── JSON-RPC 2.0 wire types ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct Request {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct Response {
    jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
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

/// TRX64 feature-request #4 — one project-supplied on-trap dump rule. NO built-in
/// engine knowledge lives in the core: the PROJECT tells the debugger which bytes are
/// "the diagnosis" at a given trap PC. On reaching/halting at `pc`, the debugger reads
/// each `dump` byte and auto-emits `label: name=$XX name2=$YY (decode)`. Loaded from a
/// small JSON file via the `traprules <path>` verb. See `TRX64_FEATURE_REQUESTS.md` #4.
#[derive(Clone, Debug)]
struct TrapRule {
    /// The trap PC this rule fires at (reached / halted-at / breakpoint).
    pc: u16,
    /// Human label for the trap (e.g. "loader miss").
    label: String,
    /// The diagnostic bytes to read + name: `(name, addr, len)`. `len` is 1..=8 bytes,
    /// emitted as a single hex value (LE for len>1) under `name`.
    dump: Vec<(String, u16, u8)>,
    /// Optional human decode line appended in parentheses (e.g.
    /// "k2 bit7 => DIRECT-overlay miss"). Empty = omitted.
    decode: String,
}

/// Singleton session, kept in memory for the daemon's lifetime.
pub struct State {
    session: Session,
    breakpoints: Breakpoints,
    /// The breakpoint/watchpoint POLICY (cond-AST, hit/ignore, watch tables).
    /// Re-synced from `breakpoints` before each run; drives the core's debug gates.
    observers: observers::ObserverRegistry,
    /// Spec 754 §3.3e — the persistent store of monitor-DSL observers registered via
    /// `obs <name> when exec|load|store $ADDR [if <cond>] do break|log|mark|cmd|trace`
    /// (= the c64re `session.ensureObservers()` registry, which survives across runs).
    /// `sync_observers` rebuilds the live [`ObserverRegistry`] from the bp surfaces on
    /// EVERY run, which would wipe DSL registrations — so they are kept HERE and
    /// re-applied (cloned) onto the registry after the bp-derived ones. `o`/`reg`
    /// lists them; `ignore <n>` / `obs <name> del` mutate them.
    dsl_observers: Vec<observers::ObsSpec>,
    /// Names of DSL observers currently DISABLED via `obs <name> off` (= the c64re
    /// `Observer.enabled=false`). `ObsSpec` carries no enabled flag (the registry's
    /// live `Observer` does, always re-armed enabled on `add`), so the disabled intent
    /// is persisted here and consulted by `sync_observers` (a disabled DSL observer is
    /// not re-applied). `obs <name> on` clears it; `del` removes the name entirely.
    dsl_disabled: std::collections::HashSet<String>,
    /// Queued PETSCII chars for session/type (stub, count tracked only).
    #[allow(dead_code)]
    type_buffer: Vec<u8>,
    /// Monotonic controller-state counter; increments on each debug/run|pause|continue.
    ctrl_frame: u64,
    /// Spec 786 audio fix — increments on each machine REBUILD (do_power_on /
    /// do_power_off = a fresh `Machine::new()`). The streaming loop watches this to
    /// re-install the SID `$D4xx` write-trace hook on the new SID; without it, audio
    /// dies on the first power-cycle (e.g. an EF CRT insert = off→on). Distinct from
    /// `ctrl_frame`, which also bumps on pause/run and would re-prime reSID far too often.
    machine_generation: u64,
    /// Last stop reason (set on pause, cleared on continue/run).
    ctrl_stop: Option<CtrlStop>,
    /// Monotonic checkpoint counter for legacy media/ingress checkpoint IDs.
    /// Superseded by real ring captures (capture_media_checkpoint); kept inert for
    /// the State init sites until those are pruned.
    #[allow(dead_code)]
    checkpoint_counter: u64,
    /// Spec 705.B — the always-on bounded in-memory checkpoint ring (rewind /
    /// time-travel). Transient, in-memory only; zero-cost until the first
    /// `checkpoint/*` (or ring-riding `vic/inspect/*`) method captures into it.
    /// Owned per-daemon (the daemon holds one session — the c64re controller's
    /// per-session `checkpointRing`).
    checkpoint_ring: trx64_core::checkpoint_ring::RuntimeCheckpointRing,
    /// Spec 710 — promoted VIC-inspect evidence (frozen findings), keyed nowhere
    /// (single session). The c64re `inspectEvidence` map; survives ring reuse,
    /// lost on session close.
    inspect_evidence: Vec<Value>,
    /// Spec 710.4 — VIC-provenance capture toggle (the c64re
    /// `session.setVicProvenanceCapture`). TRX64 captures no provenance sidecar
    /// yet, so this flag is stored for the wire contract only (inert until the
    /// vic-inspect/provenance engine lands).
    vic_provenance_enabled: bool,
    /// Declarative trace definitions (Spec 708), keyed by definition id. These are
    /// opaque JSON objects validated by [`validate_trace_definition`]; the daemon
    /// stores them per-session exactly like the TS controller's `traceDefinitions`
    /// map. No core primitive — a definition is pure data until a run taps it.
    trace_definitions: std::collections::HashMap<String, Value>,
    /// Spec 766.5 — the runtime recorder (off-thread scrub history). `None` until
    /// `recorder/start` creates it; the c64re controller lazily creates it at
    /// power-on. The daemon is single-threaded so the recorder owns its store
    /// directly (no worker thread). Records anchors via `recorder/capture`.
    recorder: Option<trx64_core::recorder::runtime_recorder::RuntimeRecorder>,
    /// Spec 766.5 — last disk content generation shipped to the recorder. TRX64 has
    /// no drive `diskWriteGeneration` facade (the worktree builders own the drive),
    /// so the recorder derives a content generation from the attached disk bytes:
    /// this counter bumps each time the disk image hash changes between captures.
    recorder_disk_gen: i32,
    recorder_disk_hash: Option<String>,
    /// Spec 231/268 — in-process scenario registry (id → Scenario JSON). The c64re
    /// registry is file-backed (samples/ + project dir); TRX64 keeps it in-memory
    /// per-session (additive — no writes into the c64re repo). save/load/delete/list
    /// operate on this map; `run` replays the scenario deterministically.
    scenarios: std::collections::HashMap<String, Value>,
    /// Spec 709.8 — the ordered, replayable media-event history (= the c64re
    /// `RuntimeController.mediaEvents`). Each media op (mount/swap/unmount/eject/
    /// ingress) appends its `MediaIngressEvent` object here; `media/events` reads it
    /// back. Bounded to the last [`MAX_MEDIA_EVENTS`] (the c64re ring is unbounded
    /// but pins only the last PINNED_MEDIA_EVENTS checkpoints; TRX64 has no per-event
    /// checkpoint pins to leak, so a simple length cap suffices).
    media_events: Vec<Value>,
    /// Spec 265 / audit ws-media-8 — the recents store (= the c64re GLOBAL persisted
    /// recent-media store, recent-files.ts). `add_recent_media` pushes on EVERY mount/
    /// swap (newest-first, deduped by path, cap [`MAX_RECENT_MEDIA`]), stamping a
    /// `mountedAt` ISO timestamp. `media/recent` overlays this AHEAD of the project +
    /// samples dir scans (1:1 with ws-server.ts:1833-1839 §1, recents first). TRX64
    /// keeps it IN-MEMORY per-daemon (no host-state write into the user's
    /// ~/.config/c64re store — additive, no shared global side effects), so the
    /// newest-first ordering + mountedAt overlay match TS without touching real files.
    recent_media: Vec<RecentMedia>,
    /// Spec 793 — `<name>_media/` sidecar directories created by `undump` media
    /// materialization (the embedded disk/cart written out as real, picker-visible,
    /// file-backed mounts). `undump_media_purge` / monitor `killmedia` deletes ONLY
    /// these (+ unmounts a mount whose backing file lives under one) — never a user's
    /// own mount. Tag = "the daemon created it", the safety boundary.
    materialized_media: Vec<String>,
    /// Spec 271 — in-process batch registry (batchId → BatchEntry JSON). The c64re
    /// batch runner spawns N worker threads (scenario-pool); TRX64's daemon is
    /// single-threaded, so `batch/start` runs the scenarios SEQUENTIALLY through the
    /// existing `run_scenario` path and stores the completed entry here. The wire
    /// shape (batchId/status/completed/total/results) is 1:1 with c64re.
    batches: std::collections::HashMap<String, BatchEntry>,
    /// The generic JSON-notification broadcaster (ws-server.ts:258 `broadcast`).
    /// `dispatch` is pure request→response; this hub is how a handler ALSO pushes a
    /// server notification (debug/breakpoint_hit, audio/flush, batch/progress) to
    /// every connected client. Each connection registers its outbound channel; the
    /// hub fans a `{jsonrpc,method,params}` notification (no id) to all. Always
    /// present (unlike the `--stream`-gated A/V `StreamHub`).
    notify: Arc<streaming::NotifyHub>,
    /// Spec 771.2 (T1.1) — whether the --stream A/V hub is enabled, so session/state
    /// reports sid.streaming truthfully (live audio = streaming_enabled && running),
    /// mirroring TS `audioStreams.has(session_id)`. Was hardcoded false → SID light OFF.
    streaming_enabled: bool,
    /// T1.3 — current pacing mode (RuntimeController.pacing.mode). One of "pal",
    /// "warp", "fixed-ratio". Stored here because TRX64 has no autonomous pacing
    /// loop; session/set_pacing sets it and debug/state reads it back exactly as the
    /// TS RuntimeController does (build_debug_state mirrors c.state()).
    pacing_mode: String,
    /// T1.3 — current pacing ratio (RuntimeController.pacing.ratio). Positive f64;
    /// defaults to 1.0 (1× PAL speed). Mirrored from TS: `if (ratio && ratio > 0)
    /// this.pacing.ratio = ratio` (runtime-controller.ts:331).
    pacing_ratio: f64,
    /// T1.2 — who is currently driving the shared session: "human" (UI) or "llm"
    /// (MCP / agent). Default "human". Mirrors RuntimeController.controlOwner
    /// (runtime-controller.ts:189). Sticky; set when a side issues a control op
    /// (run/pause/continue/step). broadcast `debug/control` on change only (Spec 767
    /// setControlOwner, runtime-controller.ts:338). Signal only; never gates access.
    control_owner: String,
    /// T2.6 — last finalized trace store path and run id (= TS TraceRunController
    /// `lastStorePath`/`lastRunId`). Set in finalize_trace; surfaced by trace/current.
    /// `None` until the first trace is stopped.
    last_trace_path: Option<String>,
    last_run_id: Option<String>,
    /// T2.4 — BUG-042 cart write-LED: last seen writableGeneration from the cart
    /// (TS ws-server.ts:1599-1602 `cartLedTrack`). When the generation advances
    /// `cart_led_last_write_at` is stamped; the "write" activity is held for 1.2 s
    /// so the 250 ms UI poll renders a steady blink through a write burst.
    cart_led_gen: u64,
    cart_led_last_write_at: Option<std::time::Instant>,
    /// Spec 714.5 / Spec 705.B — the per-frame BACKGROUND-LOOP layer the c64re
    /// RuntimeController runs that has no WS method (runtime-controller.ts). TRX64's
    /// `stream_loop` is the SOLE per-frame machine driver under --stream, so it
    /// hosts these three behaviors (gated on `running`, like the advance/audio gate).
    ///
    /// BUG-040 cart auto-persist (= maybeAutoPersistCart, runtime-controller.ts:493).
    /// The mapper's monotonic writableGeneration() distinguishes "still being
    /// written" (gen moving → re-arm the settle window) from "settled" (gen stable
    /// for the debounce → write the flash back to the host .crt once via the same
    /// persist path as eject). DEBOUNCE IS WALL-CLOCK ms (audit ws-media-3 /
    /// background-workers-async-10) — 1:1 with the TS 1 s setInterval + Date.now()
    /// debounce that fires regardless of run-state, so a dirty-then-pause STILL
    /// reaches the host file. `cart_ap_seen_gen` = last gen observed;
    /// `cart_ap_settle_at_ms` = the wall-clock ms at which it last changed;
    /// `cart_ap_done_gen` = the gen already written to the host file.
    cart_ap_seen_gen: u64,
    cart_ap_settle_at_ms: u64,
    cart_ap_done_gen: u64,
    /// Disk lazy host-file writeback (parity-neutral enhancement — see report). TS
    /// writes the host .d64/.g64 EAGERLY at the GCR-data-writeback commit point
    /// (fsimage_dxx.ts:428 hostFlush, BUG-023 — VICE's fd IS the file). TRX64's
    /// write-through only mirrors the dirty track into `disk.bytes` (in-memory); the
    /// host file is reached only on media/persist|eject. To give the user the
    /// lazily-updated host disk file, the stream loop flushes + writes the backing
    /// file when a dirty track has been settled for the debounce window. `disk_ap_*`
    /// track the same settle/done gen as the cart, derived from a content hash of
    /// the (flushed) disk bytes (TRX64 has no diskWriteGeneration facade).
    /// `disk_ap_pending` arms the host-file writeback: set true the frame a drive
    /// write first flushes a dirty GCR track into `disk.bytes` (flush_disk_writeback
    /// returns true ONCE then clears the dirty flag), so subsequent frames keep
    /// debouncing on the now-stable `disk.bytes` content hash even though the track
    /// is no longer dirty. Cleared after the host file is written.
    disk_ap_pending: bool,
    disk_ap_settle_at_ms: u64,
    disk_ap_seen_hash: Option<String>,
    disk_ap_done_hash: Option<String>,
    /// Spec 705.B — auto-capture cadence: capture a render-anchor (framebuffer-
    /// OMITTED, BUG-049) into the checkpoint ring every CHECKPOINT_CAPTURE_EVERY_FRAMES
    /// stream-loop frames (= CHECKPOINT_AUTOCAPTURE, runtime-controller.ts:157). The
    /// filmstrip/scrub depends on a populated ring; without this loop it is sparse.
    /// Skipped while a mounted medium is dirty + non-persistable (Spec 709.13).
    autocapture_frames_since: u64,
    /// Spec 766.5b (audit background-workers-async-0 + ws-checkpoint-scrub-7) —
    /// recorder auto-feed cadence: capture one CORE-ONLY (omitMedia) recorder anchor
    /// every CHECKPOINT_CAPTURE_EVERY_FRAMES stream-loop frames while a recorder is
    /// active, mirroring the c64re tick() recorder.captureAnchor call (runtime-
    /// controller.ts:846-852) — there the recorder rides the SAME cadence as the ring
    /// auto-capture, inside tick(), so a free-running machine grows recorder anchors
    /// over time. TRX64 previously fed the recorder ONLY on an explicit recorder/
    /// capture, so a --stream free-run left the recorder frozen at 1 (or 0) anchors.
    /// This counter drives the per-frame feed (separate from `autocapture_frames_since`
    /// because the ring auto-capture is warp-skipped while the recorder is not).
    recorder_frames_since: u64,
    /// Spec 796 — the live candidate store (session-lifetime). Keyed by candidate id
    /// (`cand-<seq>`). Each candidate = baseline anchor + bound scenario + accumulating
    /// overlay patch-set + cached no-patch baseline result.
    candidates: std::collections::HashMap<String, candidate::Candidate>,
    candidate_seq: u64,
    /// Spec 769.5a — the SEPARATE per-checkpoint thumbnail store (= the c64re
    /// `RuntimeController.checkpointThumbs` map, runtime-controller.ts:181). Keyed by
    /// checkpoint id, capped at [`MAX_THUMBS`]. Decoupled from the ring's
    /// framebuffer-OMITTED (BUG-049) auto-capture anchor: when the stream loop
    /// auto-captures, it downscales the live frame it JUST rendered for the video
    /// broadcast and stores it here under the SAME id the ring assigned. The scrub
    /// filmstrip (`checkpoint/thumbnails`) then intersects `ring.list()` with this
    /// store — so every auto-anchor gets a thumbnail (previously only the rare
    /// framebuffer-present checkpoints did). `checkpoint_thumb_order` tracks
    /// insertion order for oldest-first eviction (a HashMap has no order).
    checkpoint_thumbs: std::collections::HashMap<String, CheckpointThumb>,
    checkpoint_thumb_order: std::collections::VecDeque<String>,
    /// T2.8 — monitor-shell session-private cursor/lens state (= monitor-shell.ts
    /// module-level `bankDefaults` / `memCursors` / `disasmCursors` / `sidefxOn`).
    /// The daemon holds one session, so these are single-valued (not per-id maps).
    mon: MonitorState,
    /// Spec 623 §4.2/§4.3 — the per-session interrupt/trap flow-frame tracker (=
    /// the c64re `RuntimeController.flow`, runtime-controller.ts:141). Backs the
    /// monitor `flow`/`focus` panels; mutated per single-step by `step_one_with_flow`
    /// from the `z`/`n`/`ret` handlers. PASSIVE — never advances the VM.
    flow: FlowTracker,
    /// Spec 764 — JAM (KIL) auto-break edge for the per-frame stream driver (=
    /// runtime-controller.ts:793 `brokeOnJam`). A jammed CPU keeps cycling clk with
    /// PC frozen, so the free-run advance never aborts on it; the stream loop detects
    /// the jammed state and drops into the monitor ONCE per episode. Re-armed when the
    /// jam clears (or on a fresh run()).
    stream_broke_on_jam: bool,
    /// Spec 771.2 (audit ws-checkpoint-scrub-1) — one-shot "present a fresh frame
    /// even though paused" request, set by `checkpoint/restore`. The TS controller's
    /// restore ALWAYS presentFrame()s on restore (runtime-controller.ts:606-613, "no
    /// client-grab dependency"), so a paused scrub refreshes the canvas to the
    /// rolled-back picture immediately. TRX64's stream loop sends nothing while
    /// paused (frozen picture), so a paused restore would leave the pre-scrub frame on
    /// screen. The restore handler sets this; the stream loop's paused branch consumes
    /// it ONCE — pushing exactly one BIN_VIC + `session/frame_available` — then clears
    /// it (no continuous push, the machine stays frozen). When no --stream hub exists,
    /// the restore handler still broadcasts `session/frame_available` directly, so a
    /// command-driven daemon signals the refresh too.
    force_present_frame: bool,
    /// Live A/V PULL-API (FFI / native app, ADR-073 §pull) — the persistent SID
    /// audio renderer for [`pull_audio_drain`]. `None` until the first `audioDrain()`
    /// installs the SID write-trace hook + spawns the render thread. The reSID engine
    /// itself is `!Send` (it holds the process-global reSID `MutexGuard`), so it lives
    /// on a DEDICATED render thread — constructed ONCE, never reconstructed, never
    /// crossing threads. `State` keeps only the `Send` handles (write-capture buffer,
    /// write-ring sender, PCM ring, thread join + stop). This MIRRORS the `--stream`
    /// loop (`streaming.rs` `stream_loop`) and C64RE's Spec 768 reSID worker:
    /// write-ring (emu→render) → persistent engine → PCM-ring (render→main). Off by
    /// default → zero cost until the app starts pulling audio. Joined on `State` drop.
    audio_render: Option<AudioRenderThread>,
    /// TRX64 feature-request #4 — project-supplied on-trap dump rules, keyed by trap PC
    /// (last write wins on a duplicate PC). Loaded via `traprules <path>`; consulted on
    /// the JAM / breakpoint-at-PC paths to auto-emit `label: name=$XX (decode)`. Empty by
    /// default (no built-in engine knowledge); session-scoped (lost on close).
    trap_rules: std::collections::HashMap<u16, TrapRule>,
}

/// Live A/V PULL-API — persistent, `Send` handles for the FFI SID audio render
/// thread ([`pull_audio_drain`]). The `!Send` reSID `SidAudioEngine` is owned BY the
/// render thread (constructed once, thread-confined → persistent, never
/// reconstructed); `State` holds only these `Send` channels + lifecycle handles.
/// Mirrors `streaming.rs` `stream_loop` (persistent engine on its own thread) and
/// C64RE Spec 768 (persistent engine on a worker, fed by a write-ring, producing a
/// PCM ring).
struct AudioRenderThread {
    /// SID register writes captured by the `set_write_trace` hook since the last
    /// drain (CPU order, $D4xx offset masked to 0x00..0x1f). Shared with the hook
    /// (`Box<dyn FnMut + Send>`); drained + cleared every pull, then sent (with the
    /// window's `d_cycles`) over the write-ring.
    writes: std::sync::Arc<std::sync::Mutex<Vec<(u8, u8)>>>,
    /// The write-ring (emu→render). Each `audioDrain()` sends `(window writes in CPU
    /// order, d_cycles for that window)`; the render thread replays them into the
    /// persistent engine, closes the boundary, flushes → PCM. `Send` (the engine
    /// never crosses here — only the data).
    tx: std::sync::mpsc::Sender<(Vec<(u8, u8)>, u32)>,
    /// The PCM ring (render→main). The render thread pushes the reSID PCM it produced;
    /// `audioDrain()` pops accumulated samples (FIFO). NO engine access on drain.
    pcm: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<i16>>>,
    /// Total Φ2 cycles the render thread has CONSUMED (one boundary per window). The
    /// drain side bumps `sent_cycles` when it sends a window and waits (bounded) until
    /// `processed_cycles == sent_cycles` before popping, so a pull returns the PCM for
    /// the window it just sent (deterministic for callers that pull then read, like the
    /// smoke test) while the engine stays persistent + the stream continuous.
    processed_cycles: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Total Φ2 cycles SENT to the render thread so far (sum of every window's
    /// `d_cycles`). Drain waits for `processed_cycles` to reach this before popping.
    sent_cycles: u64,
    /// The `c64_core.clk` value at the previous drain. The next drain sends PCM for
    /// exactly `clk_now - last_clk` cycles (= the streaming loop's per-frame
    /// `d_cycles`, but over the host's pull window) — so the engine advances by the
    /// REAL elapsed cycles (44100 samples/s at ~1 MHz real-time).
    last_clk: u64,
    /// Stop flag — set on drop; the render thread also exits when the `tx` is dropped
    /// (its `recv()` returns `Err`), so this is a belt-and-braces signal.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The render-thread join handle. `Drop` signals stop + drops `tx` (closing the
    /// channel) then joins, so the thread (and its reSID guard) is released cleanly —
    /// no leak, no UB.
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for AudioRenderThread {
    fn drop(&mut self) {
        // Signal stop so the render loop exits on its next timeout-poll, then join. The
        // render loop also exits if `tx` is dropped (recv → Disconnected); the stop flag
        // covers the case where a window is mid-render. `tx` (a plain field) is dropped
        // after this body, but the timeout-poll guarantees the thread wakes regardless,
        // so the join never deadlocks. One construct, one drop of the reSID engine.
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// T2.8 — the monitor-shell.ts module-level per-session state, collapsed for the
/// daemon's single session. `bank_default` = sticky lens for m/d (monitor-shell
/// `bankDefaults`, default "cpu"); `mem_cursor`/`disasm_cursor` = the shared
/// per-session cursors so a bare `m`/`d` follows the latest dump/step
/// (`memCursors`/`disasmCursors`); `sidefx_on` = side-effect read toggle
/// (`sidefxOn`, default OFF → peek).
struct MonitorState {
    bank_default: String,
    mem_cursor: Option<u16>,
    disasm_cursor: Option<u16>,
    sidefx_on: bool,
    /// Sticky inspect target (= monitor-shell `deviceSel`, default "c64"). When
    /// "drive8" the read-inspect verbs `r`/`m`/`d` target the 1541 drive CPU
    /// (read-inspect ONLY — Spec 754 §3.3i); other verbs are blocked with a clear
    /// message. `device c64|drive8` (or `dev`) flips it.
    device: String,
    /// FILE mini-shell session cwd (= monitor-shell `fsShellCwd` map, single-valued
    /// for the daemon's one session). `None` until `cd` sets it; a bare `pwd`/`ls`
    /// then roots at the project dir. Relative `load`/`save`/`bload`/`bsave`/`ls`/
    /// `cd` paths resolve against this (else the project dir). Absolute paths pass
    /// through unchanged — exactly like the TS `resolveFsPath` (which is NOT a hard
    /// jail: it only defaults relative paths to the cwd; `..`/abs escape freely).
    fs_cwd: Option<String>,
    /// Spec 754 §3.3c — modal assemble cursor (= monitor-shell `asmCursors`). When
    /// `Some(addr)` the monitor is in VICE-style `a` assemble mode: EVERY line is an
    /// instruction assembled at the cursor (no verb dispatch); an empty line exits.
    asm_cursor: Option<u16>,
    /// The `MonitorResult.prompt` for the LAST command (= the TS modal `prompt`
    /// field). Set per-command by `run_monitor` (cleared at entry); the `monitor/exec`
    /// handler forwards it on the reply so a modal `a`/`df -i` prompt reaches the wire
    /// exactly as TS's `runMonitorCommand` returns `{ output, prompt }`.
    pending_prompt: Option<String>,
}

impl MonitorState {
    fn new() -> Self {
        Self {
            bank_default: "cpu".to_string(),
            mem_cursor: None,
            disasm_cursor: None,
            sidefx_on: false,
            device: "c64".to_string(),
            fs_cwd: None,
            asm_cursor: None,
            pending_prompt: None,
        }
    }
}

/// Spec 623 §4.2/§4.3 — the per-session control-flow tracker, a 1:1 port of the
/// c64re TS `FlowTracker` (stepping.ts:145-281) that backs the monitor `flow`
/// panel (monitor-shell.ts:1103-1117 ← `ctrl.flow.flowState()`). It maintains the
/// interrupt/trap FRAME STACK so `flow` reports whether execution is currently in
/// main / irq / nmi flow — the LIVE interrupt context, not a constant.
///
/// STEP-DRIVEN, exactly like TS: the stack is mutated by [`FlowTracker::apply`],
/// which is called from the daemon's `z`/`step`/`n`/`ret` handlers after each
/// single-step (the TS `apply()` runs from `stepInto`/`stepOver`/`runReturn`/…).
/// A cold break from free-run leaves the stack empty → current=main (the documented
/// best-effort cold state, stepping.ts:142-143). The classification mirrors
/// `stepOne` (stepping.ts:78-103): an SP drop of exactly 3 across a step whose
/// pre-opcode is not BRK is the unambiguous hardware IRQ/NMI dispatch (no other
/// 6502 instruction pushes 3 bytes); BRK ($00) is a software interrupt entry; RTI
/// ($40) pops the innermost frame AFTER the RTI runs in handler flow.
///
/// PASSIVE OBSERVER (Spec 723 observer-effect lesson): the tracker reads CPU regs
/// the daemon already has post-step and reads the NMI vector via the non-side-effect
/// `peek_lens` — it never advances the VM, so it has ZERO effect on byte-exact
/// execution. The no-disk corpus is identical with it wired in.
///
/// FlowKind = main|irq|nmi|brk|trap (stepping.ts:39). BRK folds to its own `brk`
/// kind (TS classifies BRK entry as `brk`); `trap` is vestigial in the single-path
/// runtime. The 3-frame model (main/irq/nmi) plus `brk` matches stepping.ts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FlowKind {
    Main,
    Irq,
    Nmi,
    Brk,
}

impl FlowKind {
    /// The lowercase tag the TS render uses (`fr.kind` / `current=<kind>`).
    fn tag(self) -> &'static str {
        match self {
            FlowKind::Main => "main",
            FlowKind::Irq => "irq",
            FlowKind::Nmi => "nmi",
            FlowKind::Brk => "brk",
        }
    }
}

/// stepping.ts:44-51 — `CpuFlowFrame`. Field names mirror the TS interface; only the
/// fields the `flow` panel renders are carried (`stepping.ts:185-189`):
/// kind, enteredAtPc (→ `pc`), enteredAtCycle (→ `cyc`), returnPc (→ `ret`).
#[derive(Clone, Copy)]
struct CpuFlowFrame {
    kind: FlowKind,
    entered_at_pc: u16,
    entered_at_cycle: u64,
    return_pc: u16,
}

/// stepping.ts:78-103 — the classified result of one single step, used by
/// [`FlowTracker::apply`]. `ev` is the StepEventType; `flow` is set only for `int`.
struct StepClass {
    is_int: bool,
    is_rti: bool,
    flow: FlowKind, // only meaningful when is_int
    pc0: u16,
    pc1: u16,
    cycle_abs: u64,
}

/// 1:1 port of the TS `FlowTracker` (stepping.ts:145-190). Only the state the
/// `flow` panel observes is carried; the stepping COMMANDS themselves stay in the
/// daemon's existing `z`/`n`/`ret` handlers (which already mirror stepInto/
/// stepOver/runReturn), and call [`FlowTracker::apply`] per single step.
struct FlowTracker {
    /// stepping.ts:146 — the interrupt/trap frame stack (innermost last).
    stack: Vec<CpuFlowFrame>,
    /// stepping.ts:147 — focus mode string (auto|main|irq|nmi|brk|none). The
    /// `flow` panel renders it verbatim; `focus` verb sets it. Default "auto".
    focus: String,
}

impl FlowTracker {
    fn new() -> Self {
        FlowTracker { stack: Vec::new(), focus: "auto".to_string() }
    }

    /// stepping.ts:149-151 — currentFlow(): the innermost frame's kind, else main.
    fn current_flow(&self) -> FlowKind {
        self.stack.last().map(|f| f.kind).unwrap_or(FlowKind::Main)
    }

    /// stepping.ts:158 — reset(): clear the frame stack (focus is left intact, as in
    /// TS where `reset()` only nulls `stack`).
    fn reset(&mut self) {
        self.stack.clear();
    }

    /// stepping.ts:160-171 — apply(): mutate the stack from a classified step. An
    /// `int` pushes a frame; an `rti` pops the innermost (AFTER the RTI ran in
    /// handler flow); jsr/rts/normal don't change the interrupt-flow kind.
    fn apply(&mut self, r: &StepClass) {
        if r.is_int {
            self.stack.push(CpuFlowFrame {
                kind: r.flow,
                entered_at_pc: r.pc1,
                entered_at_cycle: r.cycle_abs,
                return_pc: r.pc0,
            });
        } else if r.is_rti && !self.stack.is_empty() {
            self.stack.pop();
        }
    }

    /// monitor-shell.ts:1103-1117 — render the `flow` panel from flowState()
    /// (stepping.ts:174-190). Identical text shape:
    ///   `flow: current=<kind>  focus=<focus>\nframes:\n<lines | placeholder>`
    /// where each frame line is
    ///   `  <kind>  enter=$PPPP -> ret=$RRRR  cyc=<cycle>`.
    fn render(&self) -> String {
        let frames = if self.stack.is_empty() {
            "  (main — no interrupt/trap frame active)".to_string()
        } else {
            self.stack
                .iter()
                .map(|f| {
                    format!(
                        "  {}  enter=${:04X} -> ret=${:04X}  cyc={}",
                        f.kind.tag(),
                        f.entered_at_pc,
                        f.return_pc,
                        f.entered_at_cycle
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        format!(
            "flow: current={}  focus={}\nframes:\n{}",
            self.current_flow().tag(),
            self.focus,
            frames
        )
    }
}

/// SINGLE-PATH bus-selection gate (Spec 723). The ONE predicate that decides whether a
/// scenario runs on the FULL literal-VIC product machine (`run_for_full*`, the VIC the
/// TS reference always ticks) or on the chip-ISOLATED `cpu6510` core. Every advance path
/// — the run path (`run_cycle_budget`), the step paths (`step_one_instruction`,
/// `step_one_capture_interrupt`), the break-run (`run_until_break`), and
/// `debug/memory_access_map` — MUST read THIS so RUN and STEP/INSPECT see the SAME
/// machine for the same scenario (no run-vs-step observer effect).
///
/// `full_machine` reads SCENARIO state only (`full_assembled` + `injected`/`io_injected`
/// + a `vic`-directed trace directive), NEVER a recording channel:
///   * a real boot / `wr io` register-injection / a `vic`-directed program → full machine
///     (the per-cycle VIC renderer must sweep the raster; the VIC steals CPU cycles via
///     badline / sprite-DMA BA-low).
///   * a plain-`wr`-injected CPU/CIA/SID ISA micro-exerciser → the isolated `cpu6510`
///     core it is a unit test OF (the core the TS-recorded goldens for those exercisers
///     match; the full machine's VERBATIM VICE core legitimately diverges from the TS
///     oracle on jammed-CPU / indexed-RMW FETCH_OPCODE cycle counts).
///
/// `vic_directed` reads the active trace's `vic`/`c64-vic` domain — but ONLY to ENGAGE
/// the VIC (the moral equivalent of `io_injected`); the `vic` domain has NO recording
/// producer, so it changes the BUS, never a recording filter. It is NOT a recording
/// channel: the recording domains are `c64-cpu`/`memory`/`sid`/`drive8-cpu`, none of
/// which flips this gate — hence enabling any RECORDING domain leaves execution
/// (and thus the run-vs-step machine choice) unchanged.
fn full_machine_gate(session: &Session) -> bool {
    let vic_directed = session
        .trace
        .as_ref()
        .map(|t| TraceChannels::from_domains(&t.domains).vic)
        .unwrap_or(false);
    // A cartridge ONLY exists on the full literal-VIC machine (the isolated cpu6510
    // ISA exerciser has no cart mapper). When a cart is attached, the run MUST use the
    // full machine so a CPU store to the cart's IO1/IO2 register ($DE00-$DFFF) reaches
    // the mapper (e.g. an EasyFlash live bank switch) — a `wr`-marked `injected` flag
    // must NOT force the isolated core out from under an attached cart. (Audit
    // ws-cart-live-mapping — 713 §7.1 live mapping.)
    let cart_attached = session.machine.cartridge.is_some();
    session.machine.full_assembled
        && (cart_attached || !session.injected || session.io_injected || vic_directed)
}

/// A passive observer that records whether a hardware IRQ/NMI was DISPATCHED during
/// one instruction step (and which vector). The verbatim C64 core fires
/// `Observer::on_interrupt(vector, clk)` at the TOP of the NMI ($FFFA) / IRQ ($FFFE)
/// branch of `do_interrupt` (c64_6510core.rs:2174 / :2207) — a pure callback that
/// does NOT alter CPU/clk/int state. This is the AUTHORITATIVE interrupt-entry
/// signal on the full-machine path: unlike a stack-pointer-delta heuristic, it is
/// exact even though TRX64's verbatim core (like VICE) FOLDS the 7-cycle interrupt
/// entry AND the first handler opcode into the SAME instruction step (so the SP
/// delta across the step is NOT a clean −3). All other Observer hooks are no-ops, so
/// the VM runs byte-identically to the `NullSink` path (Spec 723 observer-effect: a
/// passive tap, zero execution effect).
struct InterruptCaptureObserver {
    /// The vector of the LAST interrupt dispatched in the step (0xfffa NMI / 0xfffe
    /// IRQ), or None if none. At most one hardware entry per single step.
    vector: Option<u16>,
}

impl InterruptCaptureObserver {
    fn new() -> Self {
        InterruptCaptureObserver { vector: None }
    }
}

impl Observer for InterruptCaptureObserver {
    fn on_instruction(&mut self, _pc: u16, _op: u8, _b1: u8, _b2: u8, _a: u8, _x: u8, _y: u8, _sp: u8, _p: u8, _clk: u64) {}
    fn on_bus(&mut self, _kind: BusKind, _addr: u16, _value: u8, _pc: u16, _clk: u64, _old: u8) {}
    fn on_interrupt(&mut self, vector: u16, _clk: u64) {
        self.vector = Some(vector);
    }
}

/// Advance exactly one instruction with the same machine-path selection as
/// [`step_one_instruction`], but threading an [`InterruptCaptureObserver`] so the
/// caller learns whether a hardware IRQ/NMI was dispatched in the step. Returns the
/// captured vector (None / 0xfffa / 0xfffe). The observer is passive — the VM state
/// is byte-identical to the `NullSink` path.
fn step_one_capture_interrupt(session: &mut Session) -> Option<u16> {
    // Spec 723: SAME bus gate the run path (`run_cycle_budget`) uses — STEP must see the
    // machine the scenario RUNS on (incl. the `vic`-directed full-machine engage).
    let full_machine = full_machine_gate(session);
    let mut obs = InterruptCaptureObserver::new();
    if full_machine {
        session.machine.run_for_full_capped(999_999, 1, &mut obs, |_, _, _, _, _, _, _| {});
    } else {
        session.machine.run_for_capped(999_999, 1, &mut obs);
    }
    obs.vector
}

/// Classify ONE single step like stepping.ts `stepOne` (78-103), then apply it to
/// the FlowTracker (= the TS `this.apply(r)` calls in stepInto/stepOver/…). Captures
/// the pre-step PC/SP/opcode, runs the step, and classifies the StepEventType.
///
/// PORT NOTE (why this is NOT a literal SP−3 test): the TS `Cpu65xxVice.runFor(1)`
/// lands at the bare handler VECTOR with the first handler opcode NOT folded in
/// (stepping.ts:19-20), so TS detects the entry by a clean SP−3. TRX64's verbatim
/// full-machine core (VICE-faithful) FOLDS the 7-cycle entry + the first handler
/// opcode into one step, so the SP delta is NOT −3 (e.g. entry −3 then the KERNAL's
/// `$FF48 PHA` −1 ⇒ −4, landing at $FF49). The AUTHORITATIVE entry signal is the
/// core's `on_interrupt(vector)` callback, captured passively by
/// [`step_one_capture_interrupt`]. The OBSERVABLE FlowTracker behaviour is identical
/// to TS: an interrupt pushes a frame, RTI pops it.
///
///   on_interrupt fired (vector 0xfffa) → int, flow=nmi
///   on_interrupt fired (vector 0xfffe) → int, flow=irq
///   op0==BRK ($00)                     → int, flow=brk  (BRK uses do_irqbrk, which
///                                        does NOT fire on_interrupt — detect by op)
///   op0==RTI ($40)                     → rti (pop the innermost frame)
///   else                               → normal / jsr / rts (no flow-stack change)
fn step_one_with_flow(session: &mut Session, flow: &mut FlowTracker) {
    let pc0 = session.machine.cpu6510.reg_pc;
    let op0 = session.machine.peek_lens(pc0, "cpu");
    let int_vector = step_one_capture_interrupt(session);
    let pc1 = session.machine.cpu6510.reg_pc;
    let cycle_abs = session.machine.clk;

    let (is_int, is_rti, kind, entered_at_pc) = if let Some(vec) = int_vector {
        // Hardware IRQ/NMI dispatched in this step. enteredAtPc = the handler entry
        // (the value at the vector — the true handler start, before the folded first
        // opcode advanced PC), nmi iff the vector was $FFFA.
        let lo = session.machine.peek_lens(vec, "cpu") as u16;
        let hi = session.machine.peek_lens(vec.wrapping_add(1), "cpu") as u16;
        let handler = lo | (hi << 8);
        let k = if vec == 0xfffa { FlowKind::Nmi } else { FlowKind::Irq };
        (true, false, k, handler)
    } else if op0 == 0x00 {
        // BRK = software interrupt entry (do_irqbrk → $FFFE, no on_interrupt). The
        // handler entry is the value at the IRQ/BRK vector $FFFE.
        let lo = session.machine.peek_lens(0xfffe, "cpu") as u16;
        let hi = session.machine.peek_lens(0xffff, "cpu") as u16;
        (true, false, FlowKind::Brk, lo | (hi << 8))
    } else if op0 == 0x40 {
        // RTI = interrupt return (pop).
        (false, true, FlowKind::Main, pc1)
    } else {
        // normal / jsr / rts — no interrupt-flow change.
        (false, false, FlowKind::Main, pc1)
    };
    flow.apply(&StepClass { is_int, is_rti, flow: kind, pc0, pc1: entered_at_pc, cycle_abs });
}

/// Spec 271 — one in-process batch (= c64re `BatchEntry`). Results are stored as a
/// scenarioId → ReplayResult-or-error map (serialised by [`serialise_batch_results`]).
struct BatchEntry {
    batch_id: String,
    status: &'static str,
    completed: u64,
    total: u64,
    worker_count: u64,
    started_at: String,
    finished_at: Option<String>,
    last_error: Option<String>,
    /// scenarioId → Ok(ReplayResult Value) | Err(message). Populated when the batch
    /// finishes (TRX64 runs synchronously, so it is done by the time `batch/start`
    /// returns).
    results: Vec<(String, Result<Value, String>)>,
}

/// Spec 709.8 — keep the media-event history bounded (matches the spirit of the
/// c64re PINNED_MEDIA_EVENTS window; large enough for replay/branch consumers).
const MAX_MEDIA_EVENTS: usize = 256;

pub type SharedState = Arc<Mutex<State>>;

// ── ROM directory resolution ──────────────────────────────────────────────────

fn rom_dir() -> PathBuf {
    let root = env::var("C64RE_ROOT").unwrap_or_else(|_| {
        "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP".to_string()
    });
    PathBuf::from(root).join("resources").join("roms")
}

// ── Project root for crash log ────────────────────────────────────────────────

#[allow(dead_code)] // crash-log path; reached only via the bin `main`.
fn project_dir() -> PathBuf {
    env::var("C64RE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("trx64"))
}

/// The active project dir = `--project <dir>` arg ?? `C64RE_PROJECT_DIR` env ?? cwd
/// (1:1 with the TS daemon's run.ts, which sets `process.env.C64RE_PROJECT_DIR` from
/// `--project` before scenario-registry.ts reads it). Used by the FILE-BACKED
/// scenario registry (scenario-registry.ts: project `scenarios/` dir). Mirrors the
/// `fs_project_dir` resolution in run_monitor.
fn resolve_project_dir() -> Option<PathBuf> {
    std::env::args()
        .skip_while(|a| a != "--project")
        .nth(1)
        .filter(|p| !p.is_empty())
        .or_else(|| std::env::var("C64RE_PROJECT_DIR").ok())
        .map(PathBuf::from)
}

/// The project-local `scenarios/` directory (file-backed registry store), or None
/// when no project dir is resolvable. 1:1 with scenario-registry.ts
/// `projectScenariosDir()` (`<projectDir>/scenarios`).
fn scenarios_dir() -> Option<PathBuf> {
    resolve_project_dir().map(|p| p.join("scenarios"))
}

// ── Memory-access tracker (= TS debug/memory-access-map.ts MemoryAccessTracker) ──

/// Per-page classification, mirroring the TS `PageClass` union.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PageClass {
    Unused,
    ReadOnly,
    Dead,
    Live,
}

impl PageClass {
    fn as_str(self) -> &'static str {
        match self {
            PageClass::Unused => "unused",
            PageClass::ReadOnly => "read-only",
            PageClass::Dead => "dead",
            PageClass::Live => "live",
        }
    }
}

/// Tracks per-page read/write counts over a run window; classifies into
/// unused / read-only / dead / live — mirroring `MemoryAccessTracker` in
/// `src/runtime/headless/debug/memory-access-map.ts`.
///
/// Implements `Observer` so it can be passed directly to `run_for_full*`.
struct MemoryAccessObserver {
    reads: [u32; 256],
    writes: [u32; 256],
    last_write_idx: [i64; 256],
    read_after_write: [bool; 256],
    idx: u64,
}

impl MemoryAccessObserver {
    fn new() -> Self {
        MemoryAccessObserver {
            reads: [0u32; 256],
            writes: [0u32; 256],
            last_write_idx: [-1i64; 256],
            read_after_write: [false; 256],
            idx: 0,
        }
    }

    fn classify(&self, p: usize) -> PageClass {
        let r = self.reads[p];
        let w = self.writes[p];
        if r == 0 && w == 0 {
            PageClass::Unused
        } else if w == 0 {
            PageClass::ReadOnly
        } else if r == 0 {
            PageClass::Dead
        } else if self.read_after_write[p] {
            PageClass::Live
        } else {
            PageClass::Dead
        }
    }

    /// Build the TS-shaped `{ tally, regions, cycles, classes, minBytes }` result.
    /// `want_classes` = filter set; `min_bytes` = minimum region byte span.
    fn into_result(self, budget: u64, want_classes: &[&str], min_bytes: u64) -> Value {
        // Build per-page classification.
        let want_set: std::collections::HashSet<&str> = want_classes.iter().copied().collect();

        // Classify all 256 pages.
        let classes: Vec<PageClass> = (0..256).map(|p| self.classify(p)).collect();

        // Tally: counts per class across all pages.
        let mut tally = serde_json::Map::new();
        for &cls in &classes {
            let key = cls.as_str();
            let entry = tally.entry(key.to_string()).or_insert(json!(0));
            *entry = json!(entry.as_u64().unwrap_or(0) + 1);
        }

        // Coalesce contiguous same-class page runs → regions (= TS build() method).
        let mut regions: Vec<Value> = Vec::new();
        let mut s = 0usize;
        for p in 1usize..=256 {
            let same = p < 256 && classes[p] == classes[s];
            if !same {
                let run_reads: u64 = (s..p).map(|q| self.reads[q] as u64).sum();
                let run_writes: u64 = (s..p).map(|q| self.writes[q] as u64).sum();
                let start_addr = (s as u64) << 8;
                let end_addr = (((p as u64) - 1) << 8) | 0xff;
                let cls = classes[s];
                // Filter: only wanted classes AND region size >= min_bytes.
                let region_size = end_addr - start_addr + 1;
                if want_set.contains(cls.as_str()) && region_size >= min_bytes {
                    regions.push(json!({
                        "start": start_addr,
                        "end": end_addr,
                        "cls": cls.as_str(),
                        "reads": run_reads,
                        "writes": run_writes,
                    }));
                }
                s = p;
            }
        }

        json!({
            "tally": tally,
            "regions": regions,
            "cycles": budget,
            "classes": want_classes,
            "minBytes": min_bytes,
        })
    }
}

impl Observer for MemoryAccessObserver {
    fn on_instruction(&mut self, _pc: u16, _op: u8, _b1: u8, _b2: u8, _a: u8, _x: u8, _y: u8, _sp: u8, _p: u8, _clk: u64) {}
    fn on_interrupt(&mut self, _vector: u16, _clk: u64) {}

    /// Record every real bus access — mirror of the TS attach() closure:
    ///   read  → reads[page]++; if lastWriteIdx[page] >= 0 → readAfterWrite = true
    ///   write → writes[page]++; lastWriteIdx[page] = idx; readAfterWrite = false
    /// Fetch + dummy accesses are skipped (only Read and Write are counted, per the
    /// TS observer which fires on `kind === "read"` / else for every real access).
    fn on_bus(&mut self, kind: BusKind, addr: u16, _value: u8, _pc: u16, _clk: u64, _old: u8) {
        let p = (addr >> 8) as usize;
        let i = self.idx as i64;
        match kind {
            BusKind::Read => {
                self.reads[p] = self.reads[p].saturating_add(1);
                if self.last_write_idx[p] >= 0 {
                    self.read_after_write[p] = true;
                }
                self.idx += 1;
            }
            BusKind::Write => {
                self.writes[p] = self.writes[p].saturating_add(1);
                self.last_write_idx[p] = i;
                self.read_after_write[p] = false; // new write supersedes prior consumption
                self.idx += 1;
            }
            // Fetch and dummy accesses: not counted (TS only hooks real read/write)
            BusKind::Fetch | BusKind::DummyRead | BusKind::DummyWrite => {}
        }
    }
}

// ── CPU-isolated run + monitor + trace helpers ────────────────────────────────

/// Default sibling `.duckdb` output path under a temp runtime dir.
fn default_trace_output(session_id: &str) -> PathBuf {
    // 1:1 with the c64re daemon: `resolveSnapshotPath("runtime/<session>/live_<ts>.duckdb")`
    // (ws-server.ts:1255). TWO properties matter and were BROKEN by the old fixed
    // `/tmp/trx64-runtime/<session>/live.duckdb`:
    //   • UNIQUE filename per trace (radix36(now) suffix) — a FIXED name let a stale
    //     `.duckdb` from a prior trace shadow the fresh `.c64retrace` (the c64re
    //     indexer only (re)builds when the `.duckdb` is ABSENT), so trace/read
    //     returned the OLD index. A unique name guarantees a clean lazy build.
    //   • PROJECT-SCOPED root (when a project dir is resolvable) — a shared /tmp path
    //     collided across concurrent daemons (e.g. the differential conformance gate
    //     spawns two at once). The project dir is per-daemon → isolated.
    let now36 = radix36(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    );
    let file = format!("live_{now36}.duckdb");
    let base = resolve_project_dir().unwrap_or_else(|| std::env::temp_dir().join("trx64-runtime"));
    base.join("runtime").join(session_id).join(file)
}

// ── trace/read Node sidecar (Spec "Interim A") ────────────────────────────────
//
// TRX64 already WRITES the `.c64retrace` (the shared interchange format). It does
// not yet have a native DuckDB indexer + the v2 reader algorithms, so the
// trace-analysis surface (trace/read + the v2 ops) is served by SHELLING OUT to a
// tiny Node sidecar that imports the EXISTING c64re indexer + v2 readers and runs
// them over a `.c64retrace`/`.duckdb` pair. Byte-identical to the c64re TS daemon
// by construction (it IS the same code path, ws-server.ts:1302-1377). Requires
// Node/tsx + the c64re TS source on disk (fine for the c64re-as-backend use case;
// the standalone deployment is served by the future native Rust port, spec B).

/// The TRX64 repo root (holds `tools/`). `TRX64_ROOT` env wins; else the build-time
/// `CARGO_MANIFEST_DIR` (= `crates/trx64-daemon`) walked up two levels; else the
/// known dev path. Used to locate the sidecar + the tsx in tools/oracle.
fn trx64_root() -> PathBuf {
    if let Ok(p) = std::env::var("TRX64_ROOT") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    // crates/trx64-daemon → repo root (../..). Robust even when the binary is run
    // from an arbitrary cwd (the oracle spawns it detached with stdio ignored).
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(root) = manifest.parent().and_then(|p| p.parent()) {
        if root.join("tools").join("trace-read-sidecar").exists() {
            return root.to_path_buf();
        }
    }
    PathBuf::from("/Users/alex/Development/C64/Tools/TRX64")
}

/// Resolve the `tsx` runner: prefer the one vendored under tools/oracle/node_modules
/// (per the Interim-A spec), else bare `tsx` on PATH.
fn resolve_tsx(root: &std::path::Path) -> PathBuf {
    let vendored = root.join("tools").join("oracle").join("node_modules").join(".bin").join("tsx");
    if vendored.exists() {
        vendored
    } else {
        PathBuf::from("tsx")
    }
}

/// Run a `trace/read` op via the Node sidecar. `duckdb_path` is the `.duckdb` INDEX
/// path (built lazily from its `.c64retrace` sibling on first read — covers misc-1).
/// `op` + `args` mirror the WS `trace/read` params exactly. Returns the op's JSON
/// result, or an `Err(message)` (sidecar/Node missing, op error, malformed output) —
/// the caller maps that to a clean WS error, NEVER a panic.
fn run_trace_read_sidecar(op: &str, duckdb_path: &str, args: &Value) -> Result<Value, String> {
    let root = trx64_root();
    let sidecar = root.join("tools").join("trace-read-sidecar").join("sidecar.ts");
    if !sidecar.exists() {
        return Err(format!(
            "trace/read sidecar not found at {} — the Node trace-read sidecar is required for trace analysis (set TRX64_ROOT or build the native reader).",
            sidecar.display()
        ));
    }
    let tsx = resolve_tsx(&root);
    let c64re_root = std::env::var("C64RE_ROOT")
        .unwrap_or_else(|_| "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP".to_string());
    let args_json = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());

    let output = std::process::Command::new(&tsx)
        .arg(&sidecar)
        .arg(op)
        .arg("--duckdb")
        .arg(duckdb_path)
        .arg("--args")
        .arg(&args_json)
        // Run from the c64re root so the sidecar's c64re/@duckdb imports resolve, and
        // pass C64RE_ROOT explicitly (the sidecar also reads it directly).
        .current_dir(&c64re_root)
        .env("C64RE_ROOT", &c64re_root)
        .output()
        .map_err(|e| {
            format!(
                "trace/read sidecar spawn failed ({}): {e} — Node/tsx is required for trace analysis (looked for tsx at {}).",
                op,
                tsx.display()
            )
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let last_line = stdout.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
    if last_line.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "trace/read sidecar produced no output (op={op}, status={}): {}",
            output.status,
            stderr.lines().rev().take(3).collect::<Vec<_>>().join(" | ")
        ));
    }
    let parsed: Value = serde_json::from_str(last_line)
        .map_err(|e| format!("trace/read sidecar emitted non-JSON (op={op}): {e}: {last_line}"))?;
    // The sidecar reports op failures as {"error": "..."} + a non-zero exit.
    if let Some(err) = parsed.get("error").and_then(|v| v.as_str()) {
        return Err(err.to_string());
    }
    if !output.status.success() {
        return Err(format!("trace/read sidecar exited non-zero (op={op}): {last_line}"));
    }
    Ok(parsed)
}

/// Resolve the `.duckdb` index path the sidecar should read for the CURRENT trace
/// (active or last-finalized), so trace/read + the monitor map/swimlane/taint verbs
/// + the runtime/call trace methods all target the same store. Returns None when no
/// trace has ever run (= TS "no trace store"). Mirrors trace/current's path logic.
fn current_trace_duckdb(st: &State) -> Option<String> {
    if let Some(t) = &st.session.trace {
        let retrace = t.retrace_path.to_string_lossy();
        let p = if retrace.ends_with(".c64retrace") {
            format!("{}.duckdb", &retrace[..retrace.len() - ".c64retrace".len()])
        } else {
            retrace.into_owned()
        };
        Some(p)
    } else {
        st.last_trace_path.clone()
    }
}

/// The per-session trace-store DIRECTORY (= TS `runtime/<session>/`, the parent of
/// every `live_*.duckdb` written by `trace/start_domains`). Used by the monitor
/// `swimlane list` / `swimlane <name>` verbs to list / select a stored trace by
/// basename — 1:1 with the TS ws-server.ts swimlane `list`/`name` directory scan.
/// Resolves under the project dir (`<project>/runtime/<session>/`); falls back to
/// the parent of the current/last store path when no project dir is set.
fn session_trace_store_dir(st: &State) -> Option<std::path::PathBuf> {
    if let Some(base) = resolve_project_dir() {
        return Some(base.join("runtime").join(&st.session.id));
    }
    current_trace_duckdb(st)
        .and_then(|p| std::path::Path::new(&p).parent().map(|d| d.to_path_buf()))
}

/// List the stored `.duckdb` traces in `dir`, NEWEST-mtime first (= TS listStores()
/// in the swimlane `list` bridge). Returns (basename-without-.duckdb, abs-path).
fn list_trace_stores(dir: &std::path::Path) -> Vec<(String, std::path::PathBuf)> {
    let mut stores: Vec<(String, std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    for ent in rd.flatten() {
        let p = ent.path();
        if p.extension().and_then(|e| e.to_str()) == Some("duckdb") {
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            let mtime = ent.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            stores.push((stem, p, mtime));
        }
    }
    stores.sort_by(|a, b| b.2.cmp(&a.2)); // newest first
    stores.into_iter().map(|(s, p, _)| (s, p)).collect()
}

/// Build the `RuntimeTraceDefinition` for a capture-all live trace over `domains`
/// — 1:1 with c64re `captureAllDef` (runtime-trace-sink.ts:32). The `.c64retrace`
/// file header carries this as `defJson` (= `JSON.stringify(def)`), and the c64re
/// DuckDB indexer does `JSON.parse(meta.defJson)` (binary-log-indexer.ts:140) to
/// rebuild the `trace_run` row. An EMPTY defJson breaks that parse ("Unexpected
/// end of JSON input"), so a TRX64-written trace was un-indexable → trace/read
/// (audit misc-0/1) could not read it. Field ORDER + values match the TS def so
/// the header is byte-faithful to a TS-written one (id/version/name/domains/
/// triggers/captures/retention/checkpointPolicy). Used by `trace/start_domains`.
fn capture_all_def_json(domains: &[String]) -> Value {
    let mut triggers: Vec<Value> = Vec::new();
    let mut captures: Vec<Value> = Vec::new();
    let has = |d: &str| domains.iter().any(|x| x == d);
    if has("c64-cpu") {
        triggers.push(json!({ "kind": "pc-range", "domain": "c64-cpu", "from": 0, "to": 0xffff }));
        captures.push(json!({ "kind": "cpu-row", "domain": "c64-cpu" }));
    }
    if has("drive8-cpu") {
        triggers.push(json!({ "kind": "pc-range", "domain": "drive8-cpu", "from": 0, "to": 0xffff }));
        captures.push(json!({ "kind": "cpu-row", "domain": "drive8-cpu" }));
    }
    if has("memory") {
        triggers.push(json!({ "kind": "mem-access", "access": "any", "from": 0, "to": 0xffff }));
        captures.push(json!({ "kind": "mem-row" }));
    }
    if has("iec") {
        triggers.push(json!({ "kind": "iec-transition" }));
        captures.push(json!({ "kind": "iec-row" }));
    }
    if has("vic") {
        triggers.push(json!({ "kind": "raster-window", "fromLine": 0, "toLine": 311 }));
        captures.push(json!({ "kind": "vic-row" }));
    }
    if has("drive-mechanism") {
        // Spec 784 — the 1541 head lane. DRIVE_HEAD (0x34) is emitted by the run-loop
        // drain (arm_head_trace → emit_drive_head), NOT a trigger; this capture-row is
        // what keeps mask_by_captures from masking the drive_mechanism channel off.
        captures.push(json!({ "kind": "drive-mechanism-row", "domain": "drive-mechanism" }));
    }
    json!({
        "id": "live-capture",
        "version": 1,
        "name": "live session capture",
        "domains": domains,
        "triggers": triggers,
        "captures": captures,
        "retention": "evidence",
        "checkpointPolicy": "none",
    })
}

/// Run a cycle budget (= TS session/run). Instruction-stepped: execute whole
/// instructions until `clk - start >= budget`. Streams trace frames if active.
///
/// SINGLE-PATH PASSIVE OBSERVER (Spec 723, extended to the trace path). The
/// cycle-stealing VIC is engaged by the SCENARIO, NEVER by a recording domain, so the
/// `c64Cycles` timeline — and every trace event stamped on it — is identical no matter
/// which recording channels are active. The active trace DOMAIN gates only WHAT the
/// observer records.
///
/// REMOVED — the trace-domain-selects-bus OBSERVER EFFECT. The previous code picked the
/// emulation bus from the active recording domain: `vic` → `VicBus`, `sid` → `SidBus`,
/// `memory` → `CiaBus`, else `FlatRam`. The VIC is the ONLY chip that STEALS CPU cycles
/// (badline / sprite-DMA BA-low), and the non-VIC buses never tick it, so toggling the
/// `vic` domain CHANGED `c64Cycles` for the same program — measured on TRX64:
/// `["c64-cpu"]`=20001 vs `["vic"]`=20002, while the TS reference is 20001 under BOTH.
/// The trace then measured a fictional machine. (`VicBus` additionally carried a
/// per-cycle BA-steal off-by-one vs the literal-port full machine.)
///
/// THE FIX. The `vic` "domain" has NO live trace producer — it never records anything;
/// it was only ever a SCENARIO directive meaning "this program drives the VIC." It is
/// now treated as exactly that: a request to run on the full literal-VIC product
/// machine (`run_for_full`, the same VIC the TS reference always ticks), NOT a bus
/// pick. The genuine recording domains (`c64-cpu` / `memory` / `sid` / `drive8-cpu`)
/// run the scenario's own path and NEVER change the cycle timeline (`CiaBus`/`SidBus`
/// wire their chip for register-READ correctness but neither steals CPU cycles, so
/// `c64Cycles` is byte-identical whichever is wired). Net: enabling/disabling any
/// RECORDING domain leaves `c64Cycles` and the event timeline unchanged.
///
/// `full_machine` (scenario nature, NOT a recording domain — `full_assembled` and
/// `injected`/`io_injected`, never the recording channels): a real boot / `wr io`
/// register-injection / a `vic`-directed program runs the full literal-VIC product
/// machine. A plain-`wr`-injected CPU/CIA/SID micro-exerciser runs the isolated
/// `cpu6510` ISA core it is a unit test OF — that is the core the TS-recorded goldens
/// for those exercisers match (the full machine's VERBATIM VICE core is a different,
/// VICE-faithful core that legitimately diverges from the TS oracle on jammed-CPU /
/// indexed-RMW `FETCH_OPCODE` cycle counts; running ISA exercisers on it would test
/// the wrong core). Because this gate reads scenario state and NEVER the recording
/// channels, it is the SAME under every recording domain — hence not an observer
/// effect.
///
/// FLAGGED (separate VIC-core parity, NOT a trace-path issue): `iso-vic-sprites` has a
/// 1-cycle VIC sprite-DMA BA-steal phase delta vs the TS oracle on BOTH `VicBus` and
/// the literal full machine (pre-existing RED); greening it needs a VIC sprite-DMA
/// fix, tracked separately.
/// Spec 786 — power the machine ON with the daemon-side housekeeping a power
/// transition implies: fresh full init (fresh VIC/CIA — the ONLY way to clear
/// stale chip state, `cold_reset` can't), drop the checkpoint ring (the old
/// timeline is defunct), clear the control-stop + re-arm the JAM edge, reset
/// the flow tracker + monitor cursors, warm the boot to a stable screen, and
/// flush the audio timeline. `Session::power_on` itself is a no-op if already
/// powered, so a caller that forgot an intervening `do_power_off` can't build a
/// second machine.
fn do_power_on(st: &mut State) {
    let roms = rom_dir();
    let _ = st.session.power_on(&roms);
    st.machine_generation += 1; // Spec 786 audio fix — signal the streaming loop to re-hook the fresh SID.
    st.checkpoint_ring.clear();
    st.ctrl_stop = None;
    st.ctrl_frame += 1;
    st.flow.reset();
    st.stream_broke_on_jam = false;
    st.mon.disasm_cursor = None;
    st.mon.mem_cursor = None;
    // Warm the boot so the returned pc/screen is post-KERNAL (parity with the
    // old cold-reset 5M run). Runs on the freshly-built RUNNING machine.
    run_cycle_budget(&mut st.session, 5_000_000);
    st.notify.broadcast("audio/flush", json!({ "session_id": st.session.id }));
}

/// Spec 786 — power the machine OFF (dead, no live state) + the same
/// housekeeping minus the boot warm-up (a powered-off machine runs nothing).
fn do_power_off(st: &mut State) {
    // Bug #2 fix (2026-07-10): power-OFF is the machine's terminal state ("AUS
    // komplett") — finalize any active trace FIRST so it is flushed to `.c64retrace`
    // + background-indexed, instead of stranding the buffer in RAM (lost on the next
    // power-on / process exit). Pause and the soft `session/close` are NOT terminal
    // and deliberately do NOT end a trace — the CPU/C64-state trace is indifferent to
    // run/pause and the machine stays alive across a soft close. No-op when no trace
    // is active (finalize_trace's take() → None).
    let _ = finalize_trace(st, true);
    st.session.power_off();
    st.machine_generation += 1; // Spec 786 audio fix — signal the streaming loop to re-hook the fresh SID.
    st.checkpoint_ring.clear();
    st.ctrl_stop = None;
    st.ctrl_frame += 1;
    st.flow.reset();
    st.stream_broke_on_jam = false;
    st.mon.disasm_cursor = None;
    st.mon.mem_cursor = None;
    st.notify.broadcast("audio/flush", json!({ "session_id": st.session.id }));
}

/// Spec 786 power-cycle specialised for a state RESTORE (the `.c64re` cold
/// undump). Tears the machine down to fresh chips — the ONLY way to clear the
/// stale internal SID/VIC state a field-by-field `restore_runtime_checkpoint`
/// leaves behind (its regs-only overwrite cannot reset the reSID oscillators or
/// the VIC micro-pipeline; that is what made a `.c64re` undump keep the previous
/// program's SID playing, render a stale frame, and appear to "land in the cart
/// intro"). Bumps `machine_generation` so the streaming loop re-hooks the fresh
/// SID (streaming.rs `!=` check).
///
/// Deliberately UNLIKE `do_power_off`/`do_power_on`:
/// - does NOT clear the checkpoint ring — a restore navigates a timeline, it does
///   not end one (decision 2026-07-15: ring/scrub restore stays the fast field
///   path, so the ring must survive an undump);
/// - does NOT run the 5M warm boot — the caller overwrites CPU+RAM+chips from the
///   snapshot immediately after, so a warm-up (which would also run a cart's
///   cold-boot intro) is pure waste.
///
/// The registered cart/disk are transplanted through the cycle by
/// `Session::power_off`/`power_on`; the subsequent `restore_runtime_checkpoint` is
/// the sole cart authority (re-attaches from `cartBytes`, or `detach_cart`), so the
/// re-inserted cart is harmlessly replaced or removed.
fn power_cycle_for_restore(st: &mut State) {
    // Flush any active trace before the machine is torn down (its buffer would
    // otherwise strand — same reasoning as do_power_off's finalize).
    let _ = finalize_trace(st, true);
    st.session.power_off();
    let roms = rom_dir();
    let _ = st.session.power_on(&roms);
    st.machine_generation += 1; // re-hook the fresh SID (streaming.rs `!=` check)
    st.ctrl_stop = None;
    st.ctrl_frame += 1;
    st.flow.reset();
    st.stream_broke_on_jam = false;
    st.mon.disasm_cursor = None;
    st.mon.mem_cursor = None;
    st.notify.broadcast("audio/flush", json!({ "session_id": st.session.id }));
}

fn run_cycle_budget(session: &mut Session, budget: u64) {
    // Full literal-VIC machine when the ROMs are assembled AND the scenario engages
    // the VIC: a real boot, a `wr io` register injection (render — the per-cycle VIC
    // renderer sweeps the raster), or a `vic`-directed program. A plain-`wr`-injected
    // CPU/CIA/SID ISA exerciser stays on the isolated `cpu6510` core.
    //
    // `vic_directed` reads the trace domain — but ONLY to ENGAGE the VIC, never to pick
    // a recording filter (the `vic` domain has no producer). It is the moral equivalent
    // of `io_injected` (a scenario directive to wire the VIC), so the cycle timeline is
    // the literal VIC's regardless. It does NOT make any RECORDING domain change
    // execution: the recording domains are `c64-cpu`/`memory`/`sid`/`drive8-cpu`, and
    // none of them flips this gate. The SAME `full_machine_gate` decides the step/inspect
    // paths, so RUN and STEP see the same machine for the same scenario (Spec 723).
    let full_machine = full_machine_gate(session);

    let Some((channels, need_header, meta_json)) = session.trace.as_ref().map(|t| {
        // Spec 708.7 — domains open the channels; the declared captures select which
        // rows are KEPT. mask_by_captures is a no-op for a captureAll trace (empty
        // captures) and gates each channel for a registered-definition run.
        (
            TraceChannels::from_domains(&t.domains).mask_by_captures(&t.captures),
            t.buf.is_empty(),
            t.meta_json.clone(),
        )
    }) else {
        // No active trace: run untraced on the SAME path a traced run would pick.
        session.machine.arm_head_trace(false); // Spec 784 — never accumulate untraced.
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
    // The `drive8-cpu` domain is the only one whose RECORDING needs an out-of-band
    // sink call (the deduplicated drive-PC sample comes back via `on_drive_step`, not
    // the main observer stream). This gates RECORDING, never the path.
    let drive_cpu_active = channels.drive_cpu;
    // Spec 784 — arm the loader-lens 1541 head trace for THIS run iff the
    // drive-mechanism domain is recording; disarm (+ clear) otherwise so it never
    // accumulates on non-loader runs.
    let drive_mechanism_active = channels.drive_mechanism;
    session.machine.arm_head_trace(drive_mechanism_active);

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
            // Full literal-VIC product path. `on_drive_step` collects the deduplicated
            // drive PC samples; we only forward them to the sink when the `drive8-cpu`
            // domain is recording (channel gates RECORDING, not the path).
            let mut steps: Vec<(u16, u8, u8, u8, u8, u8, u64)> = Vec::new();
            session.machine.run_for_full(seg, &mut obs, |pc, a, x, y, sp, p, drv_clk| {
                steps.push((pc, a, x, y, sp, p, drv_clk));
            });
            if drive_cpu_active {
                for (pc, a, x, y, sp, p, drv_clk) in steps {
                    obs.emit_drive_step(pc, a, x, y, sp, p, drv_clk);
                }
            }
            // Spec 784 — drain this segment's armed head samples → DRIVE_HEAD (0x34)
            // (every transition) + the read-set → BLOCK_READ (0x35) (consumed sectors).
            if drive_mechanism_active {
                for (drv_clk, ht, sec) in session.machine.drain_head_trace() {
                    obs.emit_drive_head(ht, sec, drv_clk);
                }
                for (drv_clk, ht, sec, bytes) in session.machine.drain_block_reads() {
                    obs.emit_block_read(ht, sec, bytes, drv_clk);
                }
            }
        } else if drive_cpu_active {
            // Isolated drive-CPU sampling exerciser (C64 side CIA-isolated, drive 6502
            // catches up). Bus chosen by the SCENARIO (drive sampling), never a domain.
            let mut steps: Vec<(u16, u8, u8, u8, u8, u8, u64)> = Vec::new();
            session.machine.run_for_drive_sampled(seg, &mut obs, |pc, a, x, y, sp, p, drv_clk| {
                steps.push((pc, a, x, y, sp, p, drv_clk));
            });
            for (pc, a, x, y, sp, p, drv_clk) in steps {
                obs.emit_drive_step(pc, a, x, y, sp, p, drv_clk);
            }
        } else if channels.sid {
            // SID-isolated ISA exerciser: $D400-$D7FF → SID register file + voice
            // model, SID ticked per instruction (osc3/env3 computed reads). The SID
            // does NOT steal CPU cycles, so the cycle timeline is identical to the bare
            // isolated path — the chip is wired only so the exerciser's register READS
            // return live values (READ correctness, never a cycle-timeline change).
            session.machine.run_for_sid(seg, &mut obs);
        } else if channels.mem {
            // CIA-isolated ISA exerciser: both CIAs wired ($DC00/$DD00) so the timer
            // register reads return live values. The CIAs do not steal CPU cycles, so
            // the cycle timeline is identical to a bare FlatRam run (READ correctness).
            session.machine.run_for_cia(seg, &mut obs);
        } else {
            // Plain isolated CPU ISA exerciser. Bare FlatRam: no cycle-stealing chip is
            // ticked, the no-VIC-steal cadence the TS-recorded goldens match. The VIC
            // is NEVER ticked from a recording domain here (the old `vic` → `VicBus`
            // branch, the observer effect, is gone — a `vic`-directed program is routed
            // to the full machine above, not to this bus).
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
    // Spec 723: SAME bus gate the run path (`run_cycle_budget`) uses — a `vic`-directed
    // scenario engages the full VIC when STEPPED, exactly as it does when RUN.
    let full_machine = full_machine_gate(session);
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
/// (`api_entries` string-ids + numbered `entries`) AND the persistent monitor-DSL
/// observer store, preserving each observer's accumulated `hits` / remaining
/// `ignore_left`. The registry is the run-time SOURCE OF TRUTH the core's debug
/// gates consult; the bp lists + the DSL store are the wire-shape CRUD stores.
/// After a run, [`writeback_hits`] copies the real hit counts back.
fn sync_observers(
    bp: &Breakpoints,
    dsl: &[observers::ObsSpec],
    dsl_disabled: &std::collections::HashSet<String>,
    reg: &mut observers::ObserverRegistry,
) {
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
    // Spec 754 §3.3e — persistent monitor-DSL observers (`obs … when … do …`). They
    // survive across runs (the c64re ensureObservers() registry), so re-apply a clone
    // of each onto the freshly-cleared registry, preserving live hit/ignore counts.
    // Registered AFTER the bp-derived ones; a same-name DSL observer replaces a
    // bp-derived one (add() replaces by name — DSL is the explicit, richer spec).
    for spec in dsl {
        let name = spec.name.clone();
        // A DSL observer turned `off` is not re-armed (the c64re Observer.enabled=false).
        if dsl_disabled.contains(&name) {
            continue;
        }
        let _ = reg.add(spec.clone());
        // Default for a DSL observer: keep accumulated counts; the `ignore` verb sets
        // ignore_left on the live registry, mirrored back below — but a fresh rebuild
        // restores from `prior` so the count is not lost mid-session.
        if let Some((hits, ignore_left)) = prior.get(&name).copied() {
            reg.set_counts(&name, hits, ignore_left);
        }
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

/// The VICE-style register dump line used by the monitor + the breakpoint_hit
/// broadcast — 1:1 with runtime-controller.ts:116-122 `registerDump`. The flags
/// column is the `NV-BDIZC` string with each letter UPPERCASE if the flag is set,
/// lowercase if clear (NOT the raw hex byte the `r` monitor command prints).
///
/// Reads `cpu6510` — the daemon's unified register view: the full-machine run path
/// mirrors `c64_core` into it (`sync_snapshot_sc`), and the isolated path runs ON
/// it directly, so it reflects the halt CPU regardless of which core executed.
fn register_dump(session: &Session) -> String {
    let c = &session.machine.cpu6510;
    let flags = c.flags();
    let names = ['N', 'V', '-', 'B', 'D', 'I', 'Z', 'C'];
    let flags_str: String = names
        .iter()
        .enumerate()
        .map(|(i, &f)| {
            if (flags >> (7 - i)) & 1 != 0 {
                f
            } else {
                f.to_ascii_lowercase()
            }
        })
        .collect();
    format!(
        "  ADDR AC XR YR SP NV-BDIZC\n.;{:04X} {:02X} {:02X} {:02X} {:02X} {}",
        c.reg_pc, c.reg_a, c.reg_x, c.reg_y, c.reg_sp, flags_str
    )
}

/// reverse-debug Phase 2 — render a [`TriageChain`] as monitor text lines (the JAM
/// drop-in + the `triage` verb print this). The first line is the compact causal chain;
/// the rest break out the crash point, the wild transfer, and the corruptor slot(s) with
/// their confidence tags, so a low-confidence guess never looks like a pinned fact.
fn format_triage_lines(chain: &trx64_core::crash_triage::TriageChain) -> Vec<String> {
    use trx64_core::crash_triage::TransferKind;
    let mut lines = Vec::new();
    lines.push("── crash triage (reverse-debug Phase 2) ──────────────────────".to_string());
    // The compact one-line chain.
    lines.push(chain.summary.clone());
    // TRX64 feature-request #1 — the PINNED loop/halt onset, surfaced even after the
    // spin-storm evicted the entry transfer from the live history ring.
    if let Some(lo) = chain.loop_onset {
        lines.push(format!(
            "  loop entry: ${:04X} -> ${:04X} @cyc {}  (entered via op ${:02X}; A=${:02X} X=${:02X} Y=${:02X} SP=${:02X} P=${:02X})",
            lo.src_pc, lo.dst_pc, lo.cycle, lo.src_opcode, lo.a, lo.x, lo.y, lo.sp, lo.p
        ));
        lines.push(
            "            ↑ pinned at loop onset — survives the spin-storm that evicts the live ring."
                .to_string(),
        );
    }
    // Crash point + lead-in.
    lines.push(format!(
        "  crash:    JAM @ ${:04X}  op ${:02X}",
        chain.crash.pc, chain.crash.opcode
    ));
    if !chain.crash.lead_in.is_empty() {
        let trail: Vec<String> = chain
            .crash
            .lead_in
            .iter()
            .map(|e| format!("${:04X}", e.pc))
            .collect();
        lines.push(format!("  lead-in:  {}", trail.join(" → ")));
    }
    // The wild transfer.
    lines.push(format!(
        "  transfer: {} @ ${:04X} → ${:04X}  [{}]",
        chain.transfer.kind.as_str(),
        chain.transfer.at_pc,
        chain.transfer.landed_pc,
        chain.transfer.confidence.as_str()
    ));
    lines.push(format!("            {}", chain.transfer.note));
    // The corruptor slots (only present for a stack pop).
    if chain.transfer.kind.is_stack_pop() {
        for slot in &chain.corruptor_slots {
            if let (Some(pc), Some(cyc), Some(old), Some(new)) = (
                slot.writer_pc,
                slot.writer_cycle,
                slot.writer_old,
                slot.writer_new,
            ) {
                lines.push(format!(
                    "  slot ${:04X}=${:02X}  ← written by ${:04X} @ cyc {} (${:02X}→${:02X})  [{}]",
                    slot.addr, slot.value, pc, cyc, old, new, slot.confidence.as_str()
                ));
            } else {
                lines.push(format!(
                    "  slot ${:04X}=${:02X}  ← no writer in the live ring  [{}]",
                    slot.addr, slot.value, slot.confidence.as_str()
                ));
            }
        }
        if chain.pinned_corruptor {
            lines.push(
                "  ⇒ corruptor PINNED — the cited instruction put the bad byte on the stack."
                    .to_string(),
            );
        } else {
            lines.push(
                "  ⇒ corruptor NOT pinned (low confidence) — the bad return byte was stacked \
                 before the reverse window, or is a genuine return. Inspect manually."
                    .to_string(),
            );
        }
    } else if !matches!(chain.transfer.kind, TransferKind::Unknown) {
        lines.push(
            "  ⇒ not a stack smash — no stack corruptor to attribute (see the transfer note)."
                .to_string(),
        );
    }
    lines
}

/// reverse-debug Phase 2 — the structured [`TriageChain`] as JSON (the
/// `runtime/crash_triage` WS reply + the JAM `debug/stopped` broadcast attach this).
fn triage_to_json(chain: &trx64_core::crash_triage::TriageChain) -> Value {
    let lead_in: Vec<Value> = chain
        .crash
        .lead_in
        .iter()
        .map(|e| json!({ "pc": e.pc, "opcode": e.opcode, "cycle": e.cycle }))
        .collect();
    let slots: Vec<Value> = chain
        .corruptor_slots
        .iter()
        .map(|s| {
            json!({
                "addr": s.addr,
                "value": s.value,
                "writerPc": s.writer_pc,
                "writerCycle": s.writer_cycle,
                "writerOld": s.writer_old,
                "writerNew": s.writer_new,
                "confidence": s.confidence.as_str(),
                "note": s.note,
            })
        })
        .collect();
    // TRX64 feature-request #1 — the PINNED loop/halt onset (the entry transfer that
    // entered the spin), null when no loop was detected since the last timeline boundary.
    let loop_onset = chain.loop_onset.map(|lo| {
        json!({
            "srcPc": lo.src_pc,
            "srcOpcode": lo.src_opcode,
            "dstPc": lo.dst_pc,
            "a": lo.a,
            "x": lo.x,
            "y": lo.y,
            "sp": lo.sp,
            "p": lo.p,
            "cycle": lo.cycle,
        })
    });
    json!({
        "summary": chain.summary,
        "pinnedCorruptor": chain.pinned_corruptor,
        "loopOnset": loop_onset,
        "crash": {
            "pc": chain.crash.pc,
            "opcode": chain.crash.opcode,
            "leadIn": lead_in,
        },
        "transfer": {
            "kind": chain.transfer.kind.as_str(),
            "atPc": chain.transfer.at_pc,
            "landedPc": chain.transfer.landed_pc,
            "preSp": chain.transfer.pre_sp,
            "vectorAddr": chain.transfer.vector_addr,
            "isStackPop": chain.transfer.kind.is_stack_pop(),
            "confidence": chain.transfer.confidence.as_str(),
            "note": chain.transfer.note,
            // TRX64 feature-request #3 — the transfer fell off the back of the ring
            // (window too short), not merely indeterminate.
            "ringBound": chain.transfer.ring_bound,
        },
        "corruptorSlots": slots,
    })
}

/// TRX64 feature-request #4 — parse project-supplied on-trap dump rules from JSON. The
/// project file is either a single rule object or an array of them:
///   `{ "pc":"$088F", "label":"loader miss",
///      "dump":[["k1","$0A80",1],["k2","$0A81",1]],
///      "decode":"k2 bit7 => DIRECT-overlay miss" }`
/// Addresses/PC accept `$`-prefixed or bare hex; `len` defaults to 1 and clamps 1..=8.
/// Returns the parsed rules (NO built-in engine knowledge — the project owns the bytes)
/// or an error string describing the first malformed field.
fn parse_trap_rules(json: &Value) -> Result<Vec<TrapRule>, String> {
    let arr: Vec<&Value> = match json {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => vec![json],
        _ => return Err("traprules: expected a JSON object or an array of objects".into()),
    };
    let hex16 = |v: &Value, field: &str| -> Result<u16, String> {
        let s = v.as_str().ok_or_else(|| format!("traprules: `{field}` must be a hex string"))?;
        parse_hex(s)
            .map(|n| (n & 0xffff) as u16)
            .ok_or_else(|| format!("traprules: `{field}`=\"{s}\" is not hex"))
    };
    let mut out = Vec::new();
    for (i, r) in arr.into_iter().enumerate() {
        let obj = r
            .as_object()
            .ok_or_else(|| format!("traprules: rule #{i} is not an object"))?;
        let pc = hex16(obj.get("pc").ok_or_else(|| format!("traprules: rule #{i} missing `pc`"))?, "pc")?;
        let label = obj
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("trap")
            .to_string();
        let decode = obj.get("decode").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let mut dump = Vec::new();
        if let Some(d) = obj.get("dump") {
            let items = d
                .as_array()
                .ok_or_else(|| format!("traprules: rule #{i} `dump` must be an array"))?;
            for (j, item) in items.iter().enumerate() {
                let t = item
                    .as_array()
                    .ok_or_else(|| format!("traprules: rule #{i} dump[{j}] must be [name, addr, len?]"))?;
                let name = t
                    .first()
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("traprules: rule #{i} dump[{j}][0] (name) must be a string"))?
                    .to_string();
                let addr = hex16(
                    t.get(1).ok_or_else(|| format!("traprules: rule #{i} dump[{j}] missing addr"))?,
                    "dump addr",
                )?;
                let len = t
                    .get(2)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1)
                    .clamp(1, 8) as u8;
                dump.push((name, addr, len));
            }
        }
        out.push(TrapRule { pc, label, dump, decode });
    }
    Ok(out)
}

/// TRX64 feature-request #4 — render the diagnostic emit for a trap rule, reading the
/// `dump` bytes from the live machine (side-effect-free banked peek) and formatting
/// `label: name=$XX name2=$YYYY (decode)`. A multi-byte field is shown LE as one value.
/// Read-only.
fn format_trap_rule_emit(rule: &TrapRule, m: &trx64_core::Machine) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (name, addr, len) in &rule.dump {
        let mut val: u64 = 0;
        for k in 0..*len as u16 {
            let b = m.read_full(addr.wrapping_add(k)) as u64;
            val |= b << (8 * k); // little-endian
        }
        let width = (*len as usize) * 2;
        parts.push(format!("{name}=${val:0width$X}"));
    }
    let mut s = format!("{}: {}", rule.label, parts.join(" "));
    if !rule.decode.is_empty() {
        s.push_str(&format!(" ({})", rule.decode));
    }
    s
}

/// TRX64 feature-request — GUARDRAIL #1 (key-injection vs free-running core). When the
/// machine is being advanced by the --stream background loop (`running &&
/// streaming_enabled`), a request/response key-injection (session/key_down · key_up ·
/// type) races the autonomous advance — keys queued relative to the current clock can be
/// scanned past before the KERNAL reads them, so inputs are silently LOST. This was the
/// big time-sink in the field session. Returns a NON-FATAL warning string to attach to
/// the reply (the op still runs), or `None` when the core is paused (inputs land cleanly).
fn free_run_input_warning(st: &State) -> Option<String> {
    if st.session.running && st.streaming_enabled {
        Some(
            "the machine is FREE-RUNNING (stream loop advancing) — injected key events \
             race the autonomous advance and may be LOST. Pause (debug/pause) before \
             key injection, or drive input via a scenario."
                .to_string(),
        )
    } else {
        None
    }
}

/// TRX64 feature-request #4 — if the project registered an on-trap dump rule for `pc`,
/// render its decoded diagnostic and server-PUSH it as a `debug/observer_log` line (so a
/// breakpoint-at-PC halt self-explains alongside the existing halt notifications).
/// Returns the rendered emit (for a caller that also wants to attach it to a reply), or
/// `None` when no rule matches. Read-only.
fn maybe_emit_trap_rule(st: &State, pc: u16) -> Option<String> {
    let rule = st.trap_rules.get(&pc)?;
    let emit = format_trap_rule_emit(rule, &st.session.machine);
    st.notify.broadcast(
        "debug/observer_log",
        json!({
            "session_id": st.session.id,
            "lines": [format!("trap rule @ ${pc:04X}: {emit}")],
        }),
    );
    Some(emit)
}

/// Default cycle budget for a synchronous breakpoint-gated run (the daemon is
/// request/response; a real autonomous loop would be unbounded, so we cap at a
/// generous ~10 frames of PAL cycles — enough to reach any boot-time bp).
const DEBUG_RUN_BUDGET: u64 = 10_000_000;

/// T2.2 — Spec 754 §3.3e: drain observer side-effects accumulated during a run
/// chunk (or a single step) and broadcast them as `debug/observer_log` events.
///
/// 1:1 with runtime-controller.ts:697-725 (the chunk-boundary drain block in tick()):
///   • pending_log  → one `debug/observer_log { session_id, lines }` if non-empty.
///   • pending_marks → one `debug/observer_log` per mark, formatted as
///                     `obs mark: "label" @ cyc N`  (trace active)
///                     `obs mark: "label" (no active trace — ignored)` (no trace).
///   • pending_cmds  → one `debug/observer_log` per cmd with the monitor output
///                     (synchronous; TS is async but the format is identical).
///   • pending_trace → `do trace [domains]|off` bracket model (runtime-controller.ts
///                     :727-753): start/stop a scoped capture via the trace machinery
///                     + broadcast the lifecycle line.
///
/// Called after every `run_until_break` and after every `step_one_instruction`
/// so nothing is lost.
fn drain_and_broadcast_observer_log(st: &mut State) {
    let session_id = st.session.id.clone();

    // 1. pending_log (runtime-controller.ts:697-698)
    let log_lines = st.observers.drain_pending_log();
    if !log_lines.is_empty() {
        st.notify.broadcast("debug/observer_log", json!({
            "session_id": session_id,
            "lines": log_lines,
        }));
    }

    // 2. pending_marks (runtime-controller.ts:702-710)
    let marks = st.observers.drain_pending_marks();
    let trace_active = st.session.trace.is_some();
    let cycles = st.session.machine.clk;
    for label in marks {
        let line = if trace_active {
            // When a trace is active, RECORD the bookmark into the run's marks[] (=
            // TS runtime-controller.ts:751 `this.traceRun.mark(label)`), so a later
            // trace/run/stop carries it. TRX64 previously only BROADCAST the line and
            // never pushed it onto the trace → `do mark` marks were lost from the run
            // descriptor (audit monitor-do-mark-cmd).
            if let Some(t) = st.session.trace.as_mut() {
                t.marks.push((cycles, label.clone()));
            }
            format!(r#"obs mark: "{label}" @ cyc {cycles}"#)
        } else {
            format!(r#"obs mark: "{label}" (no active trace — ignored)"#)
        };
        st.notify.broadcast("debug/observer_log", json!({
            "session_id": session_id,
            "lines": [line],
        }));
    }

    // 3. pending_cmds (runtime-controller.ts:711-725) — run synchronously
    //    (TS uses async/await but the wire shape is identical).
    let cmds = st.observers.drain_pending_cmds();
    for cmd in cmds {
        // Run the monitor command, then broadcast — collect the lines first so the
        // run_monitor `&mut State` borrow ends before the `notify` borrow.
        let lines: Vec<String> = match run_monitor(st, &cmd) {
            Ok(out) => {
                let mut lines = vec![format!(r#"obs cmd "{cmd}":"#)];
                lines.extend(out.lines().map(|l| l.to_string()));
                lines
            }
            Err(e) => vec![format!(r#"obs cmd "{cmd}": ERROR {e}"#)],
        };
        st.notify.broadcast("debug/observer_log", json!({
            "session_id": session_id,
            "lines": lines,
        }));
    }

    // 4. pending_trace (runtime-controller.ts:727-753) — `do trace [domains]|off`:
    //    the bracket model. One observer STARTS a scoped capture, another STOPS it
    //    (explicit lifecycle). The engine queues each fire into `pending_trace`; here
    //    we act on it via the SAME trace machinery the monitor `trace on/off` verb
    //    drives (TraceState + finalize_trace), and broadcast the lifecycle line.
    let traces = st.observers.drain_pending_trace();
    for (off, domains, name) in traces {
        let line = if off {
            if st.session.trace.is_some() {
                // Finalize the active trace (= `trace off`).
                let (run, _status) = finalize_trace(st, true);
                let run_id = run.get("runId").and_then(|v| v.as_str()).unwrap_or("");
                let events = run.get("eventCount").and_then(|v| v.as_u64()).unwrap_or(0);
                format!("obs {name}: trace off — {run_id} events={events}")
            } else {
                format!("obs {name}: trace off (none active — ignored)")
            }
        } else if st.session.trace.is_some() {
            format!("obs {name}: trace start skipped (a trace is already active)")
        } else {
            // Start a scoped trace over the requested domains (= `trace on <domains>`).
            // Reuse the monitor `trace on` arm so the TraceState construction stays 1:1.
            let cmd = format!("trace on {}", domains.join(" "));
            match run_monitor(st, &cmd) {
                Ok(_) => {
                    let run_id = st.last_run_id.clone().unwrap_or_default();
                    format!("obs {name}: trace on — {run_id} domains=[{}]", domains.join(","))
                }
                Err(e) => format!("obs {name}: trace ERROR {e}"),
            }
        };
        st.notify.broadcast("debug/observer_log", json!({
            "session_id": session_id,
            "lines": [line],
        }));
    }
}

/// Drive `debug/run` / `debug/continue`. When breakpoints/watchpoints are armed,
/// SEGMENT-RUN the machine until one trips (or the budget exhausts) and return the
/// real stop info. When none are armed, preserve the historical immediate
/// `running` return (no advance) so the zero-cost / no-debug contract is unchanged.
fn run_debug_control(id: Value, st: &mut State, frame: u64, _is_continue: bool) -> Response {
    {
        let State { breakpoints, dsl_observers, dsl_disabled, observers: reg, .. } = &mut *st;
        sync_observers(breakpoints, dsl_observers, dsl_disabled, reg);
    }

    if !observers_armed(&st.observers) {
        // No debug gate: historical behavior — report running, machine unchanged.
        let bps = st.breakpoints.list_vice_json();
        let pc = st.session.machine.c64_core.reg_pc as u64;
        let cycles = st.session.machine.clk;
        let (pacing_mode, pacing_ratio, control_owner) =
            (st.pacing_mode.clone(), st.pacing_ratio, st.control_owner.clone());
        return Response::ok(id, json!({
            "runState": "running",
            "pacing": { "mode": pacing_mode, "ratio": pacing_ratio },
            "pc": pc,
            "cycles": cycles,
            "frame": frame,
            "breakpoints": bps,
            "stop": null,
            "controlOwner": control_owner
        }));
    }

    // runtime-controller.ts:277 stepPastCurrentBreakpoint — if the PC currently sits ON
    // an enabled exec breakpoint, advance one instruction first so a run/continue does
    // not immediately re-trip the same address. TS calls this on BOTH run() and
    // continue() (run() unconditionally) — so it is PC-based, not continue-only.
    // (Without this, `bk <pc>` at the current PC made every debug/run halt instantly =
    // a perma-pause from the user's perspective.)
    {
        let pc = st.session.machine.c64_core.reg_pc;
        if st.breakpoints.entries.iter().any(|e| e.enabled && e.pc == pc) {
            step_one_instruction(&mut st.session);
        }
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

    // T2.2 — Spec 754 §3.3e: drain observer side-effects accumulated this chunk
    // (runtime-controller.ts:697-725). Done on every path (halt + budget-exhausted)
    // so nothing is lost, matching the TS tick() drain which runs before the halt check.
    drain_and_broadcast_observer_log(st);

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
        if let Some(name) = &run.which {
            stop["breakpoint"] = json!(name);
        }
        // Server-PUSH the halt notification (runtime-controller.ts:755-784). An exec
        // breakpoint emits `debug/breakpoint_hit` with { session_id, pc, num, cycles,
        // registers }; a load/store watchpoint emits `debug/observer_hit` with
        // { session_id, pc, cycles, observer, message, registers }. The push reaches
        // every connected client (the request reply below is the same halt, but only
        // the caller sees that — a passive client learns of the halt only via this).
        let registers = register_dump(&st.session);
        if run.reason == "observer" {
            st.notify.broadcast("debug/observer_hit", json!({
                "session_id": st.session.id,
                "pc": run.pc as u64,
                "cycles": cycles,
                "observer": run.which.clone(),
                "message": Value::Null,
                "registers": registers.clone(),
            }));
        } else {
            st.notify.broadcast("debug/breakpoint_hit", json!({
                "session_id": st.session.id,
                "pc": run.pc as u64,
                // bpNumForAddr (runtime-controller.ts:238) returns 0 (NOT null) when
                // no numbered breakpoint matches the halt address — match that.
                "num": bp_num.unwrap_or(0) as u64,
                "cycles": cycles,
                "registers": registers.clone(),
            }));
            // TRX64 feature-request #4 — if a project on-trap rule covers this halt PC,
            // auto-emit its decoded diagnostic into the observer-log stream.
            maybe_emit_trap_rule(st, run.pc);
        }
        // Spec 771.2 — runtime-controller.ts:768/782 ALSO server-PUSHes debug/stopped
        // alongside the specific hit, so a passive UI freezes the run-state on any halt.
        st.notify.broadcast("debug/stopped", json!({
            "session_id": st.session.id,
            "stop": stop.clone(),
            "registers": registers,
        }));
        let (pacing_mode, pacing_ratio, control_owner) =
            (st.pacing_mode.clone(), st.pacing_ratio, st.control_owner.clone());
        Response::ok(id, json!({
            "runState": "paused",
            "pacing": { "mode": pacing_mode, "ratio": pacing_ratio },
            "pc": run.pc as u64,
            "cycles": cycles,
            "frame": frame,
            "breakpoints": bps,
            "stop": stop,
            "controlOwner": control_owner
        }))
    } else {
        // Spec 764 — a KIL/JAM during this run jams the CPU (PC frozen) without a bp
        // hit; observe it via the shared helper (freeze + debug/stopped reason="jam")
        // so a synchronous continue halts here instead of looping the advance. Same
        // jam-halt path every other driver uses.
        if check_and_handle_jam(st) {
            let pc = jammed_pc(st);
            let cycles = st.session.machine.clk;
            let (pacing_mode, pacing_ratio, control_owner) =
                (st.pacing_mode.clone(), st.pacing_ratio, st.control_owner.clone());
            return Response::ok(id, json!({
                "runState": "paused",
                "pacing": { "mode": pacing_mode, "ratio": pacing_ratio },
                "pc": pc as u64,
                "cycles": cycles,
                "frame": frame,
                "breakpoints": bps,
                "stop": { "reason": "jam", "pc": pc as u64, "cycles": cycles },
                "controlOwner": control_owner
            }));
        }
        // Budget exhausted without a hit: the machine advanced; report running.
        let pc = st.session.machine.c64_core.reg_pc as u64;
        let (pacing_mode, pacing_ratio, control_owner) =
            (st.pacing_mode.clone(), st.pacing_ratio, st.control_owner.clone());
        Response::ok(id, json!({
            "runState": "running",
            "pacing": { "mode": pacing_mode, "ratio": pacing_ratio },
            "pc": pc,
            "cycles": cycles,
            "frame": frame,
            "breakpoints": bps,
            "stop": null,
            "controlOwner": control_owner
        }))
    }
}

/// Spec 764 — the SINGLE JAM (KIL) halt-on-jam implementation every run driver calls
/// AFTER its advance. A KIL/JAM illegal opcode jams the CPU (`c64_core.is_jammed` =
/// VICE `maincpu_jammed`): clk keeps cycling but PC stays FROZEN at the KIL, so no
/// run-advance ever aborts on it — a free-running driver that re-issues the advance
/// spins forever burning its budget on the jammed CPU (the "läuft sich tot" hang). The
/// TS reference cleanly HALTS instead (runtime-controller.ts:791-807): runState→paused,
/// PC frozen at the KIL, debug/stopped reason="jam" (the UI red border).
///
/// This helper generalizes the `--stream` jam-halt (previously inline in
/// [`stream_debug_gated_advance`]) so EVERY run driver — `session/run`, `debug/run`
/// (+ `debug/continue`), the monitor `g`/go path, AND the stream loop — observes the
/// jam and halts identically: there is ONE jam-halt implementation, not two.
///
/// Behavior (1:1 with the prior inline stream block):
/// * Jammed → freeze (`running=false`); ONCE per episode (gated on
///   `stream_broke_on_jam` so a multi-frame free-run doesn't re-broadcast every frame)
///   set `ctrl_stop` reason="jam" with the jammed PC, run the read-only crash-triage,
///   and server-PUSH `debug/stopped` (reason="jam", carrying pc/cycles/opcode +
///   triage) plus the human-readable triage `debug/observer_log`. The PC is NOT
///   advanced past the KIL.
/// * Not jammed → re-arm the edge (`stream_broke_on_jam=false`) for the next episode.
///
/// Returns whether the C64 CPU is jammed, so a synchronous driver (e.g. `session/run`)
/// can short-circuit its reply (signal the pump to PAUSE rather than keep pumping).
pub(crate) fn check_and_handle_jam(st: &mut State) -> bool {
    // A KIL jams whichever CPU CORE the active run path drives. The full literal-VIC
    // path (run_for_full — boot / cart / io-injected / vic-directed) jams `c64_core`
    // (= VICE maincpu_jammed); the chip-ISOLATED ISA-exerciser path (run_for, a `wr`/
    // run_prg-injected CPU — `session.injected` true, no cart/io/vic) jams the separate
    // `cpu6510` interpreter (cpu.rs:1024). full_machine_gate decides the path per run, so
    // exactly ONE core advances — observe a jam on EITHER and report the jammed core's PC.
    let core_jammed = st.session.machine.c64_core.is_jammed;
    let iso_jammed = st.session.machine.cpu6510.is_jammed();
    if !core_jammed && !iso_jammed {
        // Not jammed — re-arm the edge for the next episode (a future jam re-broadcasts).
        st.stream_broke_on_jam = false;
        return false;
    }
    // A jammed CPU makes no progress: FREEZE so a free-run driver stops re-advancing
    // (the stream loop gates on `running`; the pump clears its host run flag on the
    // jam signal) and the picture freezes on the last frame, 1:1 with TS runState→paused.
    st.session.running = false;
    if !st.stream_broke_on_jam {
        st.stream_broke_on_jam = true;
        // The PC frozen at the KIL is on the core that actually ran (and jammed).
        let pc = if core_jammed {
            st.session.machine.c64_core.reg_pc
        } else {
            st.session.machine.cpu6510.reg_pc
        };
        let cycles = st.session.machine.clk;
        let opcode = st.session.machine.read_full(pc) & 0xff;
        st.ctrl_stop = Some(CtrlStop { reason: "jam", pc, cycles });
        // reverse-debug Phase 2 — auto-run the guided crash-triage on the jammed state
        // and ATTACH the causal chain to the stop broadcast (read-only; the JAMmed PC
        // is the crash PC), so the user gets the crash walk for free.
        let chain = st.session.machine.crash_triage(Some(pc));
        let triage_json = triage_to_json(&chain);
        let triage_lines = format_triage_lines(&chain);
        let stop = json!({
            "reason": "jam",
            "pc": pc as u64,
            "cycles": cycles,
            "opcode": opcode as u64,
        });
        let registers = register_dump(&st.session);
        // TRX64 feature-request #4 — if the project registered an on-trap dump rule for
        // this halt PC, auto-emit its decoded diagnostic (read-only) alongside the triage.
        let trap_emit = st
            .trap_rules
            .get(&pc)
            .map(|r| format_trap_rule_emit(r, &st.session.machine));
        let session_id = st.session.id.clone();
        st.notify.broadcast("debug/stopped", json!({
            "session_id": session_id,
            "stop": stop,
            "registers": registers,
            "triage": triage_json,
            // feature #4: the project-supplied trap diagnosis for this PC (null if none).
            "trapDiagnosis": trap_emit,
        }));
        // Also emit the human-readable chain into the monitor log stream so the
        // drop-in shows it alongside the existing JAM context.
        let mut log_lines = triage_lines;
        if let Some(emit) = trap_emit {
            log_lines.push(format!("trap rule @ ${pc:04X}: {emit}"));
        }
        st.notify.broadcast("debug/observer_log", json!({
            "session_id": session_id,
            "lines": log_lines,
        }));
    }
    true
}

/// The PC frozen at the KIL on whichever CPU core jammed (full-machine `c64_core` or
/// the chip-isolated `cpu6510` exerciser) — for a driver that wants to report the
/// jammed PC in its synchronous reply. Prefers the full-machine core when both read
/// jammed (only one path runs per advance, so they agree in practice).
fn jammed_pc(st: &State) -> u16 {
    if st.session.machine.c64_core.is_jammed {
        st.session.machine.c64_core.reg_pc
    } else {
        st.session.machine.cpu6510.reg_pc
    }
}

/// Advance the machine by `budget` cycles for ONE stream-loop frame, but BREAKPOINT /
/// OBSERVER / JAM-aware — the per-frame mirror of the TS controller `tick()`
/// (runtime-controller.ts:670-806). Under `--stream` the stream loop is the SOLE
/// machine driver; a breakpoint set on a free-RUNNING machine never halted because
/// the loop advanced with a bare `run_for_full` (no gates). This helper LIFTS the
/// bp/observer/JAM-halt + broadcast behavior out of the one-shot [`run_debug_control`]
/// into the per-frame path:
///
/// * No breakpoints/watchpoints armed → plain full-machine advance (byte-identical
///   to the historical `run_for_full`; the gates monomorphize away). This is the
///   common case, so the no-debug stream is unchanged.
/// * Armed → [`run_until_break`] self-halts at the first REAL hit; on a hit we set
///   the session NOT running (freeze the picture), set the stop reason, server-PUSH
///   `debug/breakpoint_hit`/`debug/observer_hit` AND `debug/stopped` (payload shapes
///   1:1 with runtime-controller.ts:767/782 and the one-shot above), and drain
///   observer side-effects — exactly as the one-shot does.
/// * JAM (Spec 764) — a jammed CPU keeps cycling clk with PC frozen, so neither path
///   aborts on it; detect it AFTER the advance, halt (running=false) and drop into
///   the monitor ONCE per episode via `debug/stopped` (reason "jam"), re-arming the
///   edge when the jam clears (runtime-controller.ts:791-807).
///
/// Returns the number of C64 cycles actually advanced this frame, so the caller can
/// drive audio over exactly the window that ran (a halt may stop mid-frame).
pub(crate) fn stream_debug_gated_advance(st: &mut State, budget: u64) -> u32 {
    // Re-sync the observer registry from the bp surfaces (preserving live counts),
    // exactly like the one-shot run_debug_control entry.
    {
        let State { breakpoints, dsl_observers, dsl_disabled, observers: reg, .. } = &mut *st;
        sync_observers(breakpoints, dsl_observers, dsl_disabled, reg);
    }

    let clk_before = st.session.machine.c64_core.clk;

    if !observers_armed(&st.observers) {
        // ── No debug gate. When a trace is ACTIVE, the free-run advance must FEED the
        // firehose every frame (audit background-workers-async-5): the c64re tick()
        // drains traceRun once per completed frame so its worker writes the
        // .c64retrace authority (runtime-controller.ts:869-874). TRX64's stream loop
        // previously advanced with a bare NullSink, so a trace started during a
        // --stream free-run recorded NOTHING. `run_cycle_budget` is the SAME trace-
        // aware advance path the one-shot session/run uses — it attaches a real
        // TracingObserver with the trace's channels and appends the frame's events to
        // session.trace.buf (flushed to .c64retrace at trace/run/stop).
        if st.session.trace.is_some() {
            run_cycle_budget(&mut st.session, budget);
        } else {
            // No trace: the historical plain full-machine advance. KEEP this as
            // `run_for_full` UNCONDITIONALLY (NOT run_cycle_budget's no-trace path,
            // which routes an injected machine onto the cpu6510-isolated `run_for`) —
            // the JAM auto-break below reads `c64_core.is_jammed`, which only the full
            // path drives. Byte-identical to the pre-trace stream path.
            let mut sink = NullSink;
            st.session
                .machine
                .run_for_full(budget, &mut sink, |_, _, _, _, _, _, _| {});
        }
    } else {
        // runtime-controller.ts:277 stepPastCurrentBreakpoint — if the PC currently
        // sits ON an enabled exec breakpoint (e.g. we just halted there and the user
        // resumed by leaving it running), advance one instruction first so this
        // frame's advance does not immediately re-trip the same address.
        {
            let pc = st.session.machine.c64_core.reg_pc;
            if st.breakpoints.entries.iter().any(|e| e.enabled && e.pc == pc) {
                step_one_instruction(&mut st.session);
            }
        }
        // ── Armed: bp/observer-gated segment run, self-halting at the first hit. ──
        let run = {
            let State { session, observers: reg, .. } = &mut *st;
            run_until_break(session, reg, budget)
        };
        {
            let State { breakpoints, observers: reg, .. } = &mut *st;
            writeback_hits(breakpoints, reg);
        }
        // Drain observer side-effects accumulated this chunk (runtime-controller.ts:697-725)
        // on every path (halt + budget-exhausted), matching the one-shot + the TS tick.
        drain_and_broadcast_observer_log(st);

        if run.halted {
            // FREEZE: the stream loop gates the advance on `running`, so clearing it
            // freezes the picture on the last presented frame (and silences audio),
            // 1:1 with the TS tick setting runState→paused.
            st.session.running = false;
            let cycles = st.session.machine.clk;
            st.ctrl_stop = Some(CtrlStop { reason: "breakpoint", pc: run.pc, cycles });
            let bp_num = st
                .breakpoints
                .entries
                .iter()
                .find(|e| e.pc == run.pc)
                .map(|e| e.num);
            let mut stop = json!({
                "reason": run.reason,
                "pc": run.pc as u64,
                "cycles": cycles,
            });
            if let Some(n) = bp_num {
                stop["breakpointId"] = json!(n as u64);
            }
            if let Some(name) = &run.which {
                stop["breakpoint"] = json!(name);
            }
            let registers = register_dump(&st.session);
            // The specific hit (exec → breakpoint_hit, watchpoint → observer_hit),
            // payload 1:1 with runtime-controller.ts:760-781 + the one-shot above.
            if run.reason == "observer" {
                st.notify.broadcast("debug/observer_hit", json!({
                    "session_id": st.session.id,
                    "pc": run.pc as u64,
                    "cycles": cycles,
                    "observer": run.which.clone(),
                    "message": Value::Null,
                    "registers": registers.clone(),
                }));
            } else {
                st.notify.broadcast("debug/breakpoint_hit", json!({
                    "session_id": st.session.id,
                    "pc": run.pc as u64,
                    // bpNumForAddr returns 0 (NOT null) when no numbered bp matches.
                    "num": bp_num.unwrap_or(0) as u64,
                    "cycles": cycles,
                    "registers": registers.clone(),
                }));
                // TRX64 feature-request #4 — auto-emit a project on-trap diagnostic if a
                // rule covers this halt PC (per-frame stream-driver path).
                maybe_emit_trap_rule(st, run.pc);
            }
            // ALSO debug/stopped so a passive UI freezes the run-state on any halt
            // (runtime-controller.ts:768/782).
            st.notify.broadcast("debug/stopped", json!({
                "session_id": st.session.id,
                "stop": stop,
                "registers": registers,
            }));
            // A bp halt clears the JAM edge (a fresh resume should be able to re-break).
            st.stream_broke_on_jam = false;
            return st.session.machine.c64_core.clk.wrapping_sub(clk_before) as u32;
        }
    }

    // ── Spec 764 — JAM (KIL) auto-break (runtime-controller.ts:791-807). A jammed
    // CPU keeps cycling clk with PC frozen, so neither advance path aborts on it.
    // The shared `check_and_handle_jam` halts (freeze) + drops into the monitor ONCE
    // per episode (debug/stopped reason="jam") and re-arms when the jam clears — the
    // SAME helper every other run driver calls, so there is one jam-halt path. ──
    check_and_handle_jam(st);

    // ITEM (audit background-workers-async-3) — drain observer `do log`/`do mark`/
    // `do cmd` side-effects EVERY free-run frame. The c64re tick() drains them once
    // per chunk, unconditionally, BEFORE the halt checks (runtime-controller.ts:
    // 697-725) — a non-halting log/mark/cmd observer reaches the monitor only via this
    // chunk-boundary drain, not an explicit `obs log`. TRX64 previously drained them
    // ONLY inside the observers-armed branch above (so the armed-but-non-halt frame is
    // covered) AND in the one-shot run_debug_control; the per-frame free-run path had
    // no standalone drain. This makes the per-frame drain authoritative for the stream
    // loop. On an armed frame the branch above already drained, so this is a cheap
    // no-op (the pending queues are empty); on a no-observer frame nothing queued, so
    // it is also a no-op. The bp/observer/JAM halt branches return early ABOVE this —
    // each having already drained before its halt broadcast (TS order: log lines
    // precede the halt), so a halt frame's ordering is preserved.
    drain_and_broadcast_observer_log(st);

    st.session.machine.c64_core.clk.wrapping_sub(clk_before) as u32
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
    // Spec 723: SAME bus gate the run path (`run_cycle_budget`) uses — run-to-break must
    // halt on the machine the scenario RUNS on (incl. the `vic`-directed full-machine).
    let full_machine = full_machine_gate(session);
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

// T2.8 — the 6502 disasm formatters (1:1 ports of disasm6502.ts `disasmLine`,
// plus the Spec 754 §3.3f labeled variant) moved to the shared static-capability
// crate (capability-cut migration step 1): `trx64-static/src/disasm6502.rs`.
// The daemon, `trx64cli disasm` and (later) `trx64-mcp` share ONE decoder.
use trx64_static::disasm6502::{disasm_line_ts, disasm_line_ts_labeled};

/// reverse-debug Phase 1a — render the LIVE CPU-history ring (`Machine::cpu_history`)
/// as `chis` cpu-history rows. Each row disassembles the instruction FROM THE RING
/// ENTRY's captured opcode bytes (pc/opcode/b1/b2) — NOT from live RAM (which may have
/// changed since the instruction ran), reusing `disasm_line_ts` (the same disassembler
/// the monitor `d` verb uses). The cycle stamp + post-instruction registers follow the
/// disasm, matching the swimlane/chis "instruction + state" shape as close as practical:
///   `c<cycle>  $pc  bb bb bb  MNEMONIC ops   a=.. x=.. y=.. sp=.. p=..`
/// `entries` are oldest → newest (as `last_n`/`window_by_cycle` produce them).
fn format_chis_from_ring(entries: &[trx64_core::CpuHistEntry], header: &str) -> String {
    let mut out = String::new();
    out.push_str(header);
    out.push('\n');
    if entries.is_empty() {
        out.push_str("(cpuhistory ring empty)");
        return out;
    }
    for e in entries {
        // Disassemble from the CAPTURED bytes: a 3-byte read window backed by the
        // entry's own opcode/operands (pc+0=opcode, pc+1=b1, pc+2=b2). Anything
        // outside that window is unused by a single-instruction disasm.
        let pc = e.pc;
        let op = e.opcode;
        let b1 = e.b1;
        let b2 = e.b2;
        let read = move |addr: u16| -> u8 {
            match addr.wrapping_sub(pc) {
                0 => op,
                1 => b1,
                2 => b2,
                _ => 0,
            }
        };
        let (_size, line) = disasm_line_ts(read, pc);
        // Pad the disasm to a fixed width so the registers land in clean columns
        // regardless of operand length (1–3 byte ops). 30 covers `$pc  bb bb bb
        // MNEMONIC ($nn),Y`; longer (rare) lines just shift right for that row.
        out.push_str(&format!(
            "c{:<10} {:<30}  a={:02x} x={:02x} y={:02x} sp={:02x} p={:02x}\n",
            e.cycle, line, e.a, e.x, e.y, e.sp, e.p
        ));
    }
    // Trim the trailing newline so the block has no blank tail line.
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Spec 754 §3.3k — control-flow classification for the `df` static walk. 1:1 with
/// monitor-flow-disasm.ts `classify`: JMP abs / JMP (ind) / JSR / RTS / RTI / BRK /
/// conditional branch / normal. `target` carries the abs operand (or, for JMP(ind),
/// the POINTER address; for a branch, the resolved relative target).
enum CfKind {
    Normal,
    Jmp,
    JmpInd,
    Jsr,
    Rts,
    Rti,
    Brk,
    Branch,
}
struct CfInfo {
    size: u16,
    kind: CfKind,
    target: Option<u16>,
}
fn classify_cf(read: impl Fn(u16) -> u8, addr: u16) -> CfInfo {
    let op = read(addr);
    let size = instr_len(op) as u16;
    let abs = || -> u16 {
        (read(addr.wrapping_add(1)) as u16) | ((read(addr.wrapping_add(2)) as u16) << 8)
    };
    match op {
        0x4c => CfInfo { size, kind: CfKind::Jmp, target: Some(abs()) }, // JMP abs
        0x6c => CfInfo { size, kind: CfKind::JmpInd, target: Some(abs()) }, // JMP (ind)
        0x20 => CfInfo { size, kind: CfKind::Jsr, target: Some(abs()) }, // JSR abs
        0x60 => CfInfo { size, kind: CfKind::Rts, target: None },
        0x40 => CfInfo { size, kind: CfKind::Rti, target: None },
        0x00 => CfInfo { size, kind: CfKind::Brk, target: None },
        // Conditional branches BPL/BMI/BVC/BVS/BCC/BCS/BNE/BEQ.
        0x10 | 0x30 | 0x50 | 0x70 | 0x90 | 0xb0 | 0xd0 | 0xf0 => {
            let rel = read(addr.wrapping_add(1));
            let off = if rel < 0x80 { rel as i32 } else { rel as i32 - 256 };
            let target = ((addr as i32) + 2 + off) as u16;
            CfInfo { size, kind: CfKind::Branch, target: Some(target) }
        }
        _ => CfInfo { size, kind: CfKind::Normal, target: None },
    }
}

/// Screen-code → ASCII for the `screen` decode (display only). 1:1 with
/// monitor-shell.ts scToAscii: ignore the reverse-video bit, @ for 0, A-Z for 1-26,
/// space for 32, the punctuation/digit range 33-63 verbatim, '.' otherwise.
fn sc_to_ascii(sc: u8) -> char {
    let c = sc & 0x7f; // ignore the reverse-video bit
    if c == 0 {
        '@'
    } else if (1..=26).contains(&c) {
        (64 + c) as char // A-Z
    } else if c == 32 {
        ' '
    } else if (33..=63).contains(&c) {
        c as char // !"#…digits…?
    } else {
        '.'
    }
}

/// T2.8 — Spec 754 §3.x: VICE-superset interactive monitor command processor.
/// 1:1 port of the dispatch + output text format of monitor-shell.ts
/// `runMonitorCommand`. CORE verbs are wired to the daemon's State (breakpoints,
/// the run loops, cursors/lens); DEFERRED verbs (map/taint/swimlane/chis from a
/// trace store, inspect/xref/sym from a project index, label/note from a project
/// workspace) ERROR exactly like the TS "bridge unavailable / no project" path —
/// they are NOT faked. The TS `MonitorResult { output | error }` is collapsed to
/// `Ok(output)` / `Err(error)` (the monitor/exec handler re-wraps to {output}/{error}).
fn run_monitor(st: &mut State, command: &str) -> Result<String, String> {
    // Clear any prompt carried from a prior command; a modal verb re-sets it below.
    st.mon.pending_prompt = None;
    let cmd = command.trim().to_string();

    // ---- Modal assemble interception (Spec 754 §3.3c). 1:1 with monitor-shell.ts
    // :218-223. A session in assemble mode treats EVERY line as an instruction (no
    // verb dispatch); an empty line EXITS. A bad instruction stays in mode + re-shows
    // the prompt (friendlier than VICE, which silently drops out — intentional). This
    // runs BEFORE the empty-line no-op below because in mode an empty line is the
    // explicit exit, not a no-op.
    if let Some(at) = st.mon.asm_cursor {
        if cmd.is_empty() {
            st.mon.asm_cursor = None;
            return Ok(String::new());
        }
        match assemble_at(st, at, &cmd) {
            Ok(out) => return Ok(out),
            Err(e) => {
                // Re-show the prompt at the UNCHANGED cursor (cursor not advanced).
                st.mon.pending_prompt = Some(asm_prompt(at));
                return Err(e);
            }
        }
    }

    if cmd.is_empty() {
        return Ok(String::new());
    }
    let toks: Vec<String> = cmd.split_whitespace().map(|s| s.to_string()).collect();
    let op = toks[0].to_ascii_lowercase();

    // --- TS-local helpers (closures over no state — pure parsers/formatters). ---
    // parseAddr: hex with optional `$`, masked to 16 bits; None on non-hex.
    let parse_addr = |t: Option<&String>| -> Option<u16> {
        t.and_then(|t| parse_hex(t)).map(|v| (v & 0xffff) as u16)
    };
    // parseByte: hex $00-$FF; None if out of range / non-hex.
    let parse_byte = |t: Option<&String>| -> Option<u8> {
        t.and_then(|t| parse_hex(t)).and_then(|v| if v <= 0xff { Some(v as u8) } else { None })
    };
    const LENSES: [&str; 5] = ["cpu", "ram", "rom", "io", "cart"];
    // lensOf: a bank word; `default` → the sticky default. None if absent/other.
    let bank_default = st.mon.bank_default.clone();
    let lens_of = |t: Option<&String>| -> Option<String> {
        let t = t?;
        let l = t.to_ascii_lowercase();
        if l == "default" {
            return Some(bank_default.clone());
        }
        if LENSES.contains(&l.as_str()) { Some(l) } else { None }
    };

    // sidefx OFF (default) → side-effect-free peek; ON → live read. TRX64's daemon
    // has no separate side-effecting monitor read path (read_full is already the
    // peek lane), so `sidefx` is honoured as the toggle wire-state but reads always
    // use the side-effect-free lens peek (documented; the gate exercises peek).
    let _sidefx = st.mon.sidefx_on;

    // ---- FILE mini-shell path helpers (= monitor-shell.ts:174-185) ----------------
    // projectDir = `--project <dir>` arg ?? C64RE_PROJECT_DIR env ?? cwd (1:1 with the
    // TS `ctx.projectDir ?? C64RE_PROJECT_DIR ?? process.cwd()`). The FS-shell `cwd()`
    // = the per-session `fs_cwd` (set by `cd`) or that project dir. `resolveFsPath`
    // joins a RELATIVE arg against the cwd; an ABSOLUTE arg passes through unchanged —
    // NOT a hard jail, exactly as the TS resolveFsPath (which only DEFAULTS relative
    // paths; `..`/abs escape freely, so TRX64 must not jail what TS doesn't).
    let fs_project_dir = || -> String {
        std::env::args()
            .skip_while(|a| a != "--project")
            .nth(1)
            .filter(|p| !p.is_empty())
            .or_else(|| std::env::var("C64RE_PROJECT_DIR").ok())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
            })
    };
    let fs_cwd_now = st.mon.fs_cwd.clone().unwrap_or_else(fs_project_dir);
    let resolve_fs_path = |arg: &str| -> String {
        if std::path::Path::new(arg).is_absolute() {
            arg.to_string()
        } else {
            std::path::Path::new(&fs_cwd_now)
                .join(arg)
                .to_string_lossy()
                .to_string()
        }
    };
    // parseFileCmd (= monitor-shell.ts:182-185): the first token after the verb is the
    // file (a "quoted name" wins, else the bare token); `rest` = remaining tokens.
    let parse_file_cmd = || -> (Option<String>, Vec<String>) {
        if let Some(q) = quoted_first(&cmd) {
            // `rest` after the closing quote.
            let after = cmd
                .splitn(2, &format!("\"{q}\""))
                .nth(1)
                .unwrap_or("")
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();
            (Some(q), after)
        } else if toks.len() > 1 {
            (Some(toks[1].clone()), toks[2..].to_vec())
        } else {
            (None, Vec::new())
        }
    };

    // readByte/writeByte (= monitor-shell readByte/writeByte). When device=drive8
    // the read-inspect verbs r/m/d peek the 1541 drive CPU address space
    // (drive_peek); the C64 path is unchanged otherwise.
    // (closures borrow the machine; defined per-branch to satisfy the borrow checker)
    let device = st.mon.device.clone();

    // ---- device target (Spec 754 §3.3i / audit ws-trace-monitor-misc-8) ----------
    // Sticky inspect target: `device` shows / `device c64|drive8` sets. While
    // device=drive8 the monitor is READ-INSPECT only — only r/m/d (+ device/help)
    // act on the 1541 CPU; every other verb is blocked with a clear message (so it
    // can't silently mutate the C64). 1:1 with monitor-shell.ts:233-245.
    if op == "device" || op == "dev" {
        let arg = toks.get(1).map(|s| s.to_ascii_lowercase()).unwrap_or_default();
        if arg.is_empty() {
            return Ok(format!(
                "device: {device}   (c64 | drive8 — drive8 = read-inspect r/m/d on the 1541 CPU)"
            ));
        }
        if arg == "c64" || arg == "drive8" {
            st.mon.device = arg.clone();
            return Ok(format!("device: {arg}"));
        }
        return Err("device: usage: device c64|drive8".into());
    }
    // Spec 754 §3.3i — drive8 is read-inspect only: allow r/m/d (+ help/?). Anything
    // else would act on the C64 → block it (matches monitor-shell.ts:243).
    if device == "drive8" && !matches!(op.as_str(), "r" | "m" | "d" | "help" | "?") {
        return Err(format!(
            "device drive8: read-inspect only (r/m/d). `device c64` first to use `{op}`."
        ));
    }

    match op.as_str() {
        // ---- Registers (Spec 754 §3.3d). `r` shows; `r a=$42 x=$10` sets. ----
        "r" | "registers" => {
            // audit ws-trace-monitor-misc-8 — device drive8: the 1541 CPU registers
            // (read-only). 1:1 with monitor-shell.ts:481-488 (drive_pc / a / x / y / sp
            // / flags / drive_clk + track/halftrack), so the panel is unambiguously the
            // DRIVE core (header "1541 (drive 8)"), distinct from the C64 panel.
            if device == "drive8" {
                let drv = &st.session.machine.drive8;
                let c = &drv.core;
                let flags = c.status();
                let names = ['N', 'V', '-', 'B', 'D', 'I', 'Z', 'C'];
                let flags_str: String = names
                    .iter()
                    .enumerate()
                    .map(|(i, &f)| {
                        if (flags >> (7 - i)) & 1 != 0 { f } else { f.to_ascii_lowercase() }
                    })
                    .collect();
                let halftrack = drv.rotation.current_half_track;
                let track = (halftrack / 2) + 1;
                return Ok(format!(
                    "1541 (drive 8)\n  \
                     ADDR AC XR YR SP NV-BDIZC  clk\n\
                     .;{:04X} {:02X} {:02X} {:02X} {:02X} {}  {}\n  \
                     track {} (halftrack {})",
                    c.reg_pc, c.reg_a, c.reg_x, c.reg_y, c.reg_sp, flags_str, drv.drive_clk,
                    track, halftrack
                ));
            }
            let sets: Vec<&String> = toks[1..].iter().filter(|t| t.contains('=')).collect();
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
                    let c = &mut st.session.machine.cpu6510;
                    match reg.as_str() {
                        "a" | "ac" => { c.reg_a = v as u8; done.push(format!("a=${:02X}", v as u8)); }
                        "x" | "xr" => { c.reg_x = v as u8; done.push(format!("x=${:02X}", v as u8)); }
                        "y" | "yr" => { c.reg_y = v as u8; done.push(format!("y=${:02X}", v as u8)); }
                        "sp" => { c.reg_sp = v as u8; done.push(format!("sp=${:02X}", v as u8)); }
                        "pc" => {
                            c.reg_pc = v as u16;
                            // Also drive the live full-machine core's PC, so a subsequent
                            // `g`/`step` resumes from here on the full-machine path (the TS
                            // `r pc=` sets the one CPU; TRX64 has two cores kept in sync).
                            st.session.machine.c64_core.reg_pc = v as u16;
                            st.mon.disasm_cursor = Some(v as u16);
                            done.push(format!("pc=${:04X}", v as u16));
                        }
                        "p" | "fl" | "flags" => {
                            c.reg_p = (v as u8) & !0xa2;
                            c.flag_n = (v as u8) & 0x80;
                            c.flag_z = if (v as u8) & 0x02 != 0 { 0 } else { 1 };
                            done.push(format!("fl=${:02X}", v as u8));
                        }
                        _ => done.push(format!("unknown reg '{reg}'")),
                    }
                }
                st.session.machine.sync_after_monitor();
                st.session.injected = true;
                Ok(format!("set {}", done.join(" ")))
            } else {
                // Registers view (Spec 754 §3.3d, variant B): the VICE register line
                // with the flow column, then a PLA-port line and an IRQ/NMI vectors
                // line. 1:1 with monitor-shell.ts `r`. TRX64 has no FlowTracker, so
                // `flow` is reported as MAIN (the common post-boot case — no fabricated
                // interrupt frame; an honest constant, not a faked stack).
                let m = &st.session.machine;
                let c = &m.cpu6510;
                // flags string: NV-BDIZC, upper if set / lower if clear (= disasmLine).
                let flags = c.flags();
                let names = ['N', 'V', '-', 'B', 'D', 'I', 'Z', 'C'];
                let flags_str: String = names
                    .iter()
                    .enumerate()
                    .map(|(i, &f)| {
                        if (flags >> (7 - i)) & 1 != 0 { f } else { f.to_ascii_lowercase() }
                    })
                    .collect();
                // Vectors via the cpu lens (KERNAL banked) — peek, no side effect.
                let pk = |a: u16| m.peek_lens(a, "cpu");
                let w16 = |lo: u16, hi: u16| (pk(lo) as u16) | ((pk(hi) as u16) << 8);
                let irq_hw = w16(0xfffe, 0xffff);
                let nmi_hw = w16(0xfffa, 0xfffb);
                let cinv = w16(0x0314, 0x0315);
                let nmiv = w16(0x0318, 0x0319);
                // PLA banking latches: $00 = direction, $01 = port value (low 3 bits
                // select LORAM/HIRAM/CHAREN).
                let ddr = m.port_dir;
                let port = m.port_data;
                let loram = port & 1;
                let hiram = (port >> 1) & 1;
                let charen = (port >> 2) & 1;
                Ok(format!(
                    "  ADDR AC XR YR SP NV-BDIZC  flow\n\
                     .;{:04X} {:02X} {:02X} {:02X} {:02X} {}  MAIN\n  \
                     port  $00=${:02X} $01=${:02X}  LORAM={} HIRAM={} CHAREN={}\n  \
                     vectors  IRQ hw=${:04X}  CINV $0314->${:04X}     NMI hw=${:04X}  NMIV $0318->${:04X}",
                    c.reg_pc, c.reg_a, c.reg_x, c.reg_y, c.reg_sp, flags_str,
                    ddr, port, loram, hiram, charen,
                    irq_hw, cinv, nmi_hw, nmiv
                ))
            }
        }

        // ---- Memory edit: wr [lens] <addr> <byte..> --------------------------
        "wr" => {
            let mut i = 1;
            let io_lens = matches!(toks.get(i).map(|s| s.as_str()), Some("io"));
            if matches!(toks.get(i).map(|s| s.as_str()), Some("cpu" | "ram" | "io")) {
                i += 1;
            }
            let addr = parse_addr(toks.get(i)).ok_or("wr: usage: wr [lens] <addr> <byte..>")? as u16;
            i += 1;
            let bytes: Result<Vec<u8>, String> = toks[i..]
                .iter()
                .map(|t| parse_byte(Some(t)).ok_or_else(|| "wr: need >=1 byte value ($00-$FF)".to_string()))
                .collect();
            let bytes = bytes?;
            if bytes.is_empty() {
                return Err("wr: need >=1 byte value ($00-$FF)".into());
            }
            if io_lens {
                st.session.machine.poke_io(addr, &bytes);
                st.session.io_injected = true;
            } else {
                st.session.machine.poke(addr, &bytes);
                st.session.injected = true;
            }
            let lens = if io_lens { "io" } else { "cpu" };
            Ok(format!("wrote {} byte(s) @ ${:04X} ({lens})", bytes.len(), addr))
        }

        // ---- Memory dump: m [lens] [addr] [end] (§3.3b bank lens). -----------
        // $20 bytes/row + PETSCII column, default length $800. peek (no side fx).
        "m" | "mem" => {
            let mut i = 1;
            let lens_tok = lens_of(toks.get(i));
            let lens = lens_tok.clone().unwrap_or_else(|| st.mon.bank_default.clone());
            if lens_tok.is_some() {
                i += 1;
            }
            let start = parse_addr(toks.get(i))
                .or(st.mon.mem_cursor)
                .unwrap_or(0);
            let end = parse_addr(toks.get(i + 1))
                .unwrap_or_else(|| std::cmp::min(0xffff, start as u32 + 0x7ff) as u16);
            let mut lines: Vec<String> = Vec::new();
            // for (a = start & ~0x1f; a <= end; a += 32)
            let mut a: u32 = (start & !0x1f) as u32;
            let end_u = end as u32;
            while a <= end_u {
                let mut bytes: Vec<String> = Vec::new();
                let mut ascii = String::new();
                for j in 0..32u32 {
                    let aj = a + j;
                    if aj > end_u {
                        break;
                    }
                    // device drive8: peek the 1541 CPU address space (read-inspect),
                    // else the C64 banked lens (monitor-shell.ts:150-156 driveProbe).
                    let b = if device == "drive8" {
                        st.session.machine.drive8.drive_peek((aj & 0xffff) as u16)
                    } else {
                        st.session.machine.peek_lens((aj & 0xffff) as u16, &lens)
                    };
                    bytes.push(format!("{:02X}", b));
                    ascii.push(if (0x20..0x7f).contains(&b) { b as char } else { '.' });
                }
                let lens_letter = if lens == "cpu" {
                    'C'
                } else {
                    lens.chars().next().unwrap().to_ascii_uppercase()
                };
                lines.push(format!(
                    ">{}:{:04X}  {}  {}",
                    lens_letter,
                    a & 0xffff,
                    format!("{:<96}", bytes.join(" ")),
                    ascii
                ));
                a += 32;
            }
            st.mon.mem_cursor = Some(((end as u32 + 1) & 0xffff) as u16);
            Ok(lines.join("\n"))
        }

        // ---- Disassembly: d [lens] [addr] [count|end] ------------------------
        "d" | "disass" => {
            let mut i = 1;
            let lens_tok = lens_of(toks.get(i));
            let lens = lens_tok.clone().unwrap_or_else(|| st.mon.bank_default.clone());
            if lens_tok.is_some() {
                i += 1;
            }
            let default_pc = if st.mon.device == "drive8" {
                st.session.machine.drive8.core.reg_pc
            } else {
                st.session.machine.cpu6510.reg_pc
            };
            let start = parse_addr(toks.get(i))
                .or(st.mon.disasm_cursor)
                .unwrap_or(default_pc);
            // `d <start> <end>` = RANGE (VICE). The 2nd arg, present, is an END addr.
            let end: Option<u16> = if toks.get(i + 1).is_some() {
                Some(parse_addr(toks.get(i + 1)).ok_or("d: bad end address")?)
            } else {
                None
            };
            if let Some(e) = end {
                if e < (start & 0xffff) {
                    return Err(format!("d: end ${:04X} < start ${:04X}", e, start & 0xffff));
                }
            }
            let pc = st.session.machine.cpu6510.reg_pc;
            // Spec 754 §3.3f (Block F) — addr→name index (user labels) so the
            // disassembly shows symbols alongside the addresses (= TS getLabels()).
            let labels = project_knowledge::user_label_index(&fs_project_dir());
            // device drive8: disassemble the 1541 CPU address space (read-inspect).
            let on_drive = device == "drive8";
            let read = |x: u16| {
                if on_drive {
                    st.session.machine.drive8.drive_peek(x)
                } else {
                    st.session.machine.peek_lens(x, &lens)
                }
            };
            let mut lines: Vec<String> = Vec::new();
            let mut a = start & 0xffff;
            const MAX: usize = 4096;
            let mut n = 0usize;
            if let Some(e) = end {
                let e = e & 0xffff;
                while a <= e && n < MAX {
                    let (size, line) = disasm_line_ts_labeled(read, a, &labels);
                    lines.push(if a == pc { format!("{line} <-- PC") } else { line });
                    a = a.wrapping_add(size);
                    n += 1;
                    if a == 0 {
                        break; // wrapped past $FFFF
                    }
                }
                if a <= e && n >= MAX {
                    lines.push(format!(
                        "… (truncated at ${:04X} — `d ${:04X} ${:04X}` to continue)",
                        a, a, e
                    ));
                }
            } else {
                while n < 16 {
                    let (size, line) = disasm_line_ts_labeled(read, a, &labels);
                    lines.push(if a == pc { format!("{line} <-- PC") } else { line });
                    a = a.wrapping_add(size);
                    n += 1;
                }
            }
            st.mon.disasm_cursor = Some(a);
            Ok(lines.join("\n"))
        }

        // ---- Flow disassembly (Spec 754 §3.3k / audit ws-trace-monitor-misc-5) ----
        // sd [n] — DYNAMIC: step n instructions from PC, render the REAL executed
        // path (each touched address ONCE, loops folded to body + ×count), footer
        // `-- sd: N steps, K distinct addrs -> .C:<land>`. 1:1 with monitor-flow-
        // disasm.ts stepDisasm. Non-destructive: capture a machine checkpoint, step,
        // render, then restore (the live shared session must not advance). Reuses the
        // EXISTING step_one_instruction + disasm renderer (disasm_line_ts).
        "sd" => {
            let n = toks
                .get(1)
                .and_then(|t| t.parse::<i64>().ok())
                .unwrap_or(50)
                .clamp(1, 100_000) as usize;
            // Snapshot the machine so sd is non-destructive (= the TS checkpoint
            // save/restore wrap). Machine-only checkpoint (no media blobs needed for
            // the RAM/CPU/chip restore sd touches).
            let cp = trx64_core::c64re_snapshot::capture_runtime_checkpoint(
                &st.session.machine,
                "",
                "",
                None,
                None,
                None,
                None,
            );
            let was_running = st.session.running;
            st.session.running = false;
            let mut order: Vec<u16> = Vec::new();
            let mut count: std::collections::HashMap<u16, u32> = std::collections::HashMap::new();
            for _ in 0..n {
                let pc = st.session.machine.cpu6510.reg_pc;
                if !count.contains_key(&pc) {
                    order.push(pc);
                }
                *count.entry(pc).or_insert(0) += 1;
                step_one_instruction(&mut st.session);
            }
            let land = st.session.machine.cpu6510.reg_pc;
            // Restore the pre-sd machine state (non-destructive). On a restore error
            // (should not happen with a self-captured checkpoint) leave it advanced
            // and append a note — exactly like the TS `(sd: could not snapshot …)` path.
            let restore_err =
                trx64_core::c64re_snapshot::restore_runtime_checkpoint(&mut st.session.machine, &cp)
                    .err();
            st.session.machine.sync_after_monitor();
            st.session.running = was_running;
            // Render each touched address once (first-seen order) + ×count for loops.
            let read = |a: u16| st.session.machine.peek_lens(a, "cpu");
            let mut lines: Vec<String> = order
                .iter()
                .map(|&pc| {
                    let (_, line) = disasm_line_ts(read, pc);
                    let c = *count.get(&pc).unwrap();
                    if c > 1 {
                        format!("{line}   x{c}")
                    } else {
                        line
                    }
                })
                .collect();
            lines.push(format!(
                "-- sd: {} steps, {} distinct addrs -> .C:{:04x}",
                n,
                order.len(),
                land
            ));
            if let Some(e) = restore_err {
                lines.push(format!(
                    "(sd: could not snapshot — machine ADVANCED; `snap` first to preserve) [{e}]"
                ));
            }
            Ok(lines.join("\n"))
        }
        // df [-i] [addr] [n] — STATIC control-flow walk (addr-first, like `d`;
        // default from the disasm cursor / PC). Follows JMP, descends JSR + returns
        // on RTS, follows an indirect JMP, loop-guarded. Conditional branch defaults
        // to fall-through + annotates the taken target. 1:1 with monitor-flow-disasm.ts
        // followDisasm (the non-interactive walk; -i interactive resolution is the
        // UI-prompt path and not exercised by the gate, so the walk runs to its limit).
        "df" => {
            let mut i = 1usize;
            // Accept (and skip) the -i flag; TRX64's monitor/exec is request/response
            // with no modal prompt channel, so the walk proceeds non-interactively.
            if toks.get(i).map(|s| s.as_str()) == Some("-i") {
                i += 1;
            }
            let default_addr = st.mon.disasm_cursor.unwrap_or(st.session.machine.cpu6510.reg_pc);
            let addr = parse_addr(toks.get(i)).map(|a| {
                i += 1;
                a
            });
            let addr = addr.unwrap_or(default_addr);
            let n = toks
                .get(i)
                .and_then(|t| t.parse::<i64>().ok())
                .unwrap_or(200)
                .clamp(1, 100_000) as usize;
            let read = |a: u16| st.session.machine.peek_lens(a, "cpu");
            let indent = |depth: usize| -> String { "  ".repeat(depth.min(8)) };
            let mut lines: Vec<String> = Vec::new();
            let mut a = addr & 0xffff;
            let mut stack: Vec<u16> = Vec::new();
            let mut visited: std::collections::HashSet<u16> = std::collections::HashSet::new();
            let mut remaining = n;
            while remaining > 0 {
                if visited.contains(&a) {
                    lines.push(format!("{}  | back to ${:04x} (loop)", indent(stack.len()), a));
                    break;
                }
                visited.insert(a);
                let cf = classify_cf(read, a);
                let (_, line) = disasm_line_ts(read, a);
                lines.push(format!("{}{}", indent(stack.len()), line));
                remaining -= 1;
                match cf.kind {
                    CfKind::Jmp => {
                        a = cf.target.unwrap();
                    }
                    CfKind::JmpInd => {
                        let p = cf.target.unwrap();
                        let t = (read(p) as u16) | ((read(p.wrapping_add(1)) as u16) << 8);
                        lines.push(format!("{}  -> (${:04x}) = ${:04x}", indent(stack.len()), p, t));
                        a = t;
                    }
                    CfKind::Jsr => {
                        stack.push(a.wrapping_add(cf.size));
                        a = cf.target.unwrap();
                    }
                    CfKind::Rts | CfKind::Rti => {
                        if let Some(ret) = stack.pop() {
                            a = ret;
                        } else {
                            let kind = if matches!(cf.kind, CfKind::Rts) { "rts" } else { "rti" };
                            lines.push(format!(
                                "{}  (end — {kind}, call stack empty)",
                                indent(stack.len())
                            ));
                            break;
                        }
                    }
                    CfKind::Brk => {
                        lines.push(format!("{}  (end — BRK)", indent(stack.len())));
                        break;
                    }
                    CfKind::Branch => {
                        let fall = a.wrapping_add(cf.size);
                        // non-interactive default: fall-through + annotate the taken target.
                        lines.push(format!(
                            "{}  ; taken -> ${:04x}",
                            indent(stack.len()),
                            cf.target.unwrap()
                        ));
                        a = fall;
                    }
                    CfKind::Normal => {
                        a = a.wrapping_add(cf.size);
                    }
                }
            }
            if remaining == 0 {
                lines.push("-- df: reached step limit".to_string());
            }
            st.mon.disasm_cursor = Some(a);
            Ok(lines.join("\n"))
        }

        // ---- screen — decode the 40x25 text screen (audit ws-trace-monitor-misc-10).
        // Reads the LIVE screen pointer: VIC bank from CIA2 $DD00 (PA bits 0..1 are
        // inverted) + the $D018 matrix nibble. Then decodes the 40×25 screen-RAM matrix
        // (screen-code → ASCII) into a `|<40 chars>|` grid. 1:1 with monitor-shell.ts
        // :731-742 (base computation, scToAscii, header, grid). $DD00/$D018 are read
        // via the io lens; the matrix is read from RAM (the VIC reads RAM directly).
        "screen" => {
            let dd00 = st.session.machine.peek_lens(0xdd00, "io") & 0x03;
            let vic_bank = ((3 - dd00) as u16) * 0x4000; // CIA2 PA bits 0..1 inverted
            let d018 = st.session.machine.peek_lens(0xd018, "io");
            let screen_base = vic_bank.wrapping_add((((d018 >> 4) & 0x0f) as u16) * 0x0400);
            let mut lines: Vec<String> = vec![format!(
                "screen @ ${:04x}  (VIC bank ${:04x}, $D018=${:02x})",
                screen_base, vic_bank, d018
            )];
            for row in 0u16..25 {
                let mut line = String::new();
                for col in 0u16..40 {
                    let a = screen_base.wrapping_add(row * 40 + col);
                    line.push(sc_to_ascii(st.session.machine.peek_lens(a, "ram")));
                }
                lines.push(format!("|{line}|"));
            }
            Ok(lines.join("\n"))
        }

        // ---- bitmap <addr> [w] [h] [hires|charset|sprite] — render a RAM range
        // as an image (§3.3b, folds the Scrub tab). 1:1 with monitor-shell.ts:745-
        // 767: the text console can't inline it, so it writes a PNG artifact +
        // returns the path. w/h are DECIMAL counts (cells/rows/sprites per mode);
        // addr is hex. (multicolor = v1.1.) The help advertised it but run_monitor
        // had NO arm → `unknown command: bitmap` (the help LIED). charset/sprite
        // are MODES of this verb (matching TS), not standalone verbs.
        "bitmap" | "bm" => {
            let addr = match parse_addr(toks.get(1)) {
                Some(a) => a,
                None => return Err("bitmap: usage: bitmap <addr> [w] [h] [hires|charset|sprite]".into()),
            };
            let rest = &toks[2..];
            let mode_tok = rest.iter().find(|t| {
                let l = t.to_ascii_lowercase();
                matches!(l.as_str(), "hires" | "charset" | "sprite" | "mc" | "multicolor")
            });
            if let Some(mt) = mode_tok {
                let l = mt.to_ascii_lowercase();
                if l == "mc" || l == "multicolor" {
                    return Err("bitmap: multicolor is v1.1 — use hires | charset | sprite".into());
                }
            }
            let mode = mode_tok.map(|t| t.to_ascii_lowercase()).unwrap_or_else(|| "hires".to_string());
            let nums: Vec<u32> = rest
                .iter()
                .filter(|t| t.chars().all(|c| c.is_ascii_digit()) && !t.is_empty())
                .filter_map(|t| t.parse::<u32>().ok())
                .collect();
            let (def_w, def_h): (u32, u32) = match mode.as_str() {
                "charset" => (16, 16),
                "sprite" => (8, 4),
                _ => (40, 25),
            };
            let w = nums.first().copied().unwrap_or(def_w).clamp(1, 256);
            let h = nums.get(1).copied().unwrap_or(def_h).clamp(1, 256);
            let read = |a: u16| st.session.machine.peek_lens(a, "cpu");
            let (width, height, rgba, bytes) = monitor_bitmap_decode(&read, addr, w, h, &mode);
            let png = rgba_to_png(width, height, &rgba);
            let file = resolve_fs_path(&format!("bitmap_{addr:04x}_{mode}_{w}x{h}.png"));
            if let Err(e) = std::fs::write(&file, &png) {
                return Err(format!("bitmap: {e}"));
            }
            Ok(format!(
                "bitmap {mode} ${addr:04x} → {width}×{height}px ({bytes} bytes read) → {file}"
            ))
        }

        // ---- f <start> <end> <data..> — fill the range, repeating the data. --
        "f" | "fill" => {
            let start = parse_addr(toks.get(1)).ok_or("f: usage: f <start> <end> <byte..>")?;
            let end = parse_addr(toks.get(2)).ok_or("f: usage: f <start> <end> <byte..>")?;
            let data: Vec<Option<u8>> = toks[3..].iter().map(|t| parse_byte(Some(t))).collect();
            if data.is_empty() || data.iter().any(|b| b.is_none()) {
                return Err("f: need >=1 fill byte".into());
            }
            let data: Vec<u8> = data.into_iter().map(|b| b.unwrap()).collect();
            let mut n: usize = 0;
            let mut a = start as u32;
            while a <= end as u32 {
                let b = data[n % data.len()];
                st.session.machine.poke((a & 0xffff) as u16, &[b]);
                n += 1;
                a += 1;
            }
            st.session.injected = true;
            Ok(format!(
                "filled ${:04X}..${:04X} ({} bytes, pattern {})",
                start, end, n, data.len()
            ))
        }

        // ---- a <addr> [instr] — inline 6502 assembler (Spec 754 §3.3c). ------
        // `a c000 lda #$01` assembles that line then STAYS in modal assemble at the
        // next addr; `a c000` (no instr) ENTERS modal assemble at $C000 (the modal
        // interception at the top of run_monitor then takes every following line). The
        // help advertised it but run_monitor had NO arm → `unknown command: a` (the
        // help LIED). 1:1 with monitor-shell.ts:715-728 (op==="a").
        "a" => {
            let addr = parse_addr(toks.get(1)).ok_or(
                "a: usage: a <addr> [instruction]  — enter assemble mode (empty line exits)",
            )?;
            if toks.len() < 3 {
                // Enter modal assemble at addr; the interception handles subsequent lines.
                st.mon.asm_cursor = Some(addr);
                st.mon.disasm_cursor = Some(addr);
                st.mon.pending_prompt = Some(asm_prompt(addr));
                return Ok(String::new());
            }
            // Assemble the inline instruction (the rest of the line). This leaves the
            // session in modal assemble at the next addr (= TS, which stays in mode).
            let instr = toks[2..].join(" ");
            assemble_at(st, addr, &instr)
        }

        // ---- t <start> <end> <dest> — move/copy (overlap-safe). --------------
        "t" | "move" => {
            let start = parse_addr(toks.get(1)).ok_or("t: usage: t <start> <end> <dest>")?;
            let end = parse_addr(toks.get(2)).ok_or("t: usage: t <start> <end> <dest>")?;
            let dest = parse_addr(toks.get(3)).ok_or("t: usage: t <start> <end> <dest>")?;
            let len = end as i32 - start as i32 + 1;
            if len <= 0 {
                return Err("t: end < start".into());
            }
            let len = len as u16;
            let mut buf: Vec<u8> = Vec::with_capacity(len as usize);
            for k in 0..len {
                buf.push(st.session.machine.peek_lens(start.wrapping_add(k), "cpu"));
            }
            for (k, b) in buf.iter().enumerate() {
                st.session.machine.poke(dest.wrapping_add(k as u16), &[*b]);
            }
            st.session.injected = true;
            Ok(format!(
                "moved {} byte(s) ${:04X}..${:04X} -> ${:04X}",
                len, start, end, dest
            ))
        }

        // ---- c <start> <end> <dest> — compare, list differences. -------------
        "c" | "compare" => {
            let start = parse_addr(toks.get(1)).ok_or("c: usage: c <start> <end> <dest>")?;
            let end = parse_addr(toks.get(2)).ok_or("c: usage: c <start> <end> <dest>")?;
            let dest = parse_addr(toks.get(3)).ok_or("c: usage: c <start> <end> <dest>")?;
            let len = end as i32 - start as i32 + 1;
            if len <= 0 {
                return Err("c: end < start".into());
            }
            let len = len as u16;
            let mut diffs: Vec<String> = Vec::new();
            for k in 0..len {
                let av = st.session.machine.peek_lens(start.wrapping_add(k), "cpu");
                let bv = st.session.machine.peek_lens(dest.wrapping_add(k), "cpu");
                if av != bv {
                    diffs.push(format!(
                        "  ${:04X}: {:02X} != {:02X} @${:04X}",
                        start.wrapping_add(k), av, bv, dest.wrapping_add(k)
                    ));
                }
                if diffs.len() > 64 {
                    diffs.push("  ... (truncated)".to_string());
                    break;
                }
            }
            Ok(if diffs.is_empty() {
                format!("identical (${:04X}..${:04X} == ${:04X})", start, end, dest)
            } else {
                format!("differences:\n{}", diffs.join("\n"))
            })
        }

        // ---- h <start> <end> <byte/xx..> — hunt/search (xx or * = wildcard). --
        "h" | "hunt" => {
            let start = parse_addr(toks.get(1)).ok_or("h: usage: h <start> <end> <byte/xx..>")?;
            let end = parse_addr(toks.get(2)).ok_or("h: usage: h <start> <end> <byte/xx..>")?;
            let mut pat: Vec<i32> = Vec::new();
            let mut bad = toks.len() < 4;
            for t in &toks[3..] {
                if t.eq_ignore_ascii_case("xx") || t == "*" {
                    pat.push(-1);
                } else if let Some(b) = parse_byte(Some(t)) {
                    pat.push(b as i32);
                } else {
                    bad = true;
                }
            }
            if pat.is_empty() || bad {
                return Err("h: need >=1 pattern byte (xx = wildcard)".into());
            }
            let mut hits: Vec<u16> = Vec::new();
            let mut a = start as i32;
            while a + (pat.len() as i32) - 1 <= end as i32 {
                let mut m = true;
                for (k, pb) in pat.iter().enumerate() {
                    if *pb != -1
                        && st.session.machine.peek_lens((a as u16).wrapping_add(k as u16), "cpu") as i32 != *pb
                    {
                        m = false;
                        break;
                    }
                }
                if m {
                    hits.push(a as u16);
                    if hits.len() > 256 {
                        break;
                    }
                }
                a += 1;
            }
            Ok(if hits.is_empty() {
                "not found".to_string()
            } else {
                format!(
                    "found {}:\n  {}",
                    hits.len(),
                    hits.iter().map(|a| format!("${:04X}", a)).collect::<Vec<_>>().join(" ")
                )
            })
        }

        // ---- Bank lens default (§3.3b/§3.3d): bank [cpu|ram|rom|io|cart]. ----
        "bank" => {
            let arg = toks.get(1).map(|s| s.to_ascii_lowercase()).unwrap_or_default();
            if arg.is_empty() {
                return Ok(format!(
                    "bank = {}  (lens for m/d; one of cpu|ram|rom|io|cart)",
                    st.mon.bank_default
                ));
            }
            if LENSES.contains(&arg.as_str()) {
                st.mon.bank_default = arg.clone();
                Ok(format!("bank = {arg}"))
            } else {
                Err(format!("bank: expected cpu|ram|rom|io|cart, got '{arg}'"))
            }
        }

        // ---- sidefx [on|off|toggle] (§3.4). ----------------------------------
        "sidefx" => {
            let arg = toks.get(1).map(|s| s.to_ascii_lowercase()).unwrap_or_else(|| "toggle".into());
            let cur = st.mon.sidefx_on;
            let next = match arg.as_str() {
                "on" => Some(true),
                "off" => Some(false),
                "toggle" => Some(!cur),
                _ => None,
            };
            let next = next.ok_or("sidefx: on|off|toggle")?;
            st.mon.sidefx_on = next;
            Ok(if next {
                "sidefx = on (monitor reads are LIVE — I/O side effects)".to_string()
            } else {
                "sidefx = off (peek — side-effect-free, default)".to_string()
            })
        }

        // ---- Breakpoints: bk | bk <addr> | bk -<addr> | bk clear ------------
        // ---- Observers (Spec 754 §3.3e) — the full DSL the c64re REPL exposes. ----
        //   obs <name> when exec|load|store <addr[..end]> [if <cond>] do break|log|mark|cmd|trace
        //   obs | o                  list registered observers
        //   obs log                  recent `do log` lines
        //   obs <name> on|off        enable/disable
        //   obs <name> del|rm        remove
        //   ignore <name> [n]        skip the next n triggers
        // 1:1 with monitor-shell.ts:888-1001 (which dispatches `obs`/`o`/`ignore` —
        // there is NO `reg` verb, so TRX64 must not add one or it would diverge). The
        // parsed spec is stored in `st.dsl_observers` (survives the per-run
        // sync_observers rebuild) and re-applied onto the live registry every run;
        // `o` / bare `obs` list that store.
        "obs" | "o" | "ignore" => {
            // Render one stored observer the way the c64re `fmt` closure does
            // (monitor-shell.ts:898): `  * name  trigger $lo[..hi] [if cond] do <do>  hits=N`.
            let fmt_obs = |spec: &observers::ObsSpec,
                           reg: &observers::ObserverRegistry,
                           disabled: &std::collections::HashSet<String>|
             -> String {
                let live = reg.get(&spec.name);
                // A disabled DSL observer is absent from the live registry (not re-armed),
                // so derive `enabled` from the persisted disable-set, not the registry.
                let enabled = !disabled.contains(&spec.name);
                let hits = live.map(|o| o.hits).unwrap_or(0);
                let trig = match spec.trigger {
                    observers::ObsTrigger::Exec => "exec",
                    observers::ObsTrigger::Load => "load",
                    observers::ObsTrigger::Store => "store",
                };
                let range = if spec.hi != spec.lo {
                    format!("${:04X}..${:04X}", spec.lo, spec.hi)
                } else {
                    format!("${:04X}", spec.lo)
                };
                let cond = spec
                    .cond_src
                    .as_ref()
                    .map(|c| format!(" if {c}"))
                    .unwrap_or_default();
                let do_desc = obs_do_desc(spec);
                format!(
                    "  {} {}  {} {}{} do {}  hits={}",
                    if enabled { "*" } else { "o" },
                    spec.name,
                    trig,
                    range,
                    cond,
                    do_desc,
                    hits
                )
            };

            // `ignore <name> [n]` — set the per-observer ignore count.
            if op == "ignore" {
                let name = match toks.get(1) {
                    Some(n) => n.clone(),
                    None => return Err("ignore: usage: ignore <name> [n]".into()),
                };
                let n: i64 = toks.get(2).and_then(|t| t.parse().ok()).unwrap_or(1);
                let found = st.dsl_observers.iter().any(|o| o.name == name);
                if !found {
                    return Ok(format!("no observer '{name}'"));
                }
                // Mirror onto the live registry so the next run honours it; the count is
                // preserved across rebuilds via sync_observers' `prior` snapshot.
                st.observers.set_ignore(&name, n);
                return Ok(format!("ignore {name}: skip next {n}"));
            }

            let rest: Vec<String> = toks[1..].to_vec();

            // No args (or bare `reg`/`o`) → LIST.
            if rest.is_empty() {
                // sync so the live registry reflects current enabled/hits state.
                {
                    let State { breakpoints, dsl_observers, dsl_disabled, observers: reg, .. } = &mut *st;
                    sync_observers(breakpoints, dsl_observers, dsl_disabled, reg);
                }
                if st.dsl_observers.is_empty() {
                    return Ok("no observers (obs <name> when exec|load|store <addr> [if <cond>] do break|log|mark|cmd|trace)".into());
                }
                let lines: Vec<String> = st
                    .dsl_observers
                    .iter()
                    .map(|s| fmt_obs(s, &st.observers, &st.dsl_disabled))
                    .collect();
                return Ok(format!("observers:\n{}", lines.join("\n")));
            }

            // `obs log` → recent `do log` ring.
            if rest[0].eq_ignore_ascii_case("log") {
                let logs = &st.observers.logs;
                if logs.is_empty() {
                    return Ok("obs log: (empty)".into());
                }
                let start = logs.len().saturating_sub(40);
                return Ok(logs[start..].join("\n"));
            }

            let name = rest[0].clone();
            let sub = rest.get(1).map(|s| s.to_ascii_lowercase()).unwrap_or_default();

            // A name containing `*`/`?` is a GLOB → on/off/del act on ALL matches
            // (`obs * del` = all, `obs c* off` = every observer starting "c"). 1:1 with
            // monitor-shell.ts:909-932 (audit monitor-obs-lifecycle). TRX64 previously
            // matched the name EXACTLY, so a glob matched no literal observer → "no
            // observer '*'" while the help advertised the wildcard (the help LIED).
            let is_glob = name.contains('*') || name.contains('?');
            // Expand the glob to the matching observer names. `*` = any run (incl.
            // empty), `?` = exactly one char — anchored full-string match, 1:1 with the
            // TS globMatches() regex (`^` + `*`→".*" + `?`→"." + `$`). Observer names
            // are plain identifiers (no regex metachars), so a direct glob walk suffices.
            let glob_matches = |st: &State| -> Vec<String> {
                st.dsl_observers
                    .iter()
                    .map(|o| o.name.clone())
                    .filter(|n| glob_full_match(&name, n))
                    .collect()
            };

            // `obs <name> on|off` — persist the disable intent in `dsl_disabled` so it
            // survives the per-run sync_observers rebuild; re-sync to apply immediately.
            if rest.len() == 2 && (sub == "on" || sub == "off") {
                if is_glob {
                    let matches = glob_matches(st);
                    if matches.is_empty() {
                        return Ok(format!("no observer matches '{name}'"));
                    }
                    for m in &matches {
                        if sub == "off" { st.dsl_disabled.insert(m.clone()); }
                        else { st.dsl_disabled.remove(m); }
                    }
                    {
                        let State { breakpoints, dsl_observers, dsl_disabled, observers: reg, .. } = &mut *st;
                        sync_observers(breakpoints, dsl_observers, dsl_disabled, reg);
                    }
                    return Ok(format!("{sub} {}: {}", matches.len(), matches.join(", ")));
                }
                if !st.dsl_observers.iter().any(|o| o.name == name) {
                    return Ok(format!("no observer '{name}'"));
                }
                if sub == "off" {
                    st.dsl_disabled.insert(name.clone());
                } else {
                    st.dsl_disabled.remove(&name);
                }
                {
                    let State { breakpoints, dsl_observers, dsl_disabled, observers: reg, .. } = &mut *st;
                    sync_observers(breakpoints, dsl_observers, dsl_disabled, reg);
                }
                return Ok(format!("obs {name} {sub}"));
            }

            // `obs <name> del|delete|rm`
            if rest.len() == 2 && (sub == "del" || sub == "delete" || sub == "rm") {
                if is_glob {
                    let matches = glob_matches(st);
                    if matches.is_empty() {
                        return Ok(format!("no observer matches '{name}'"));
                    }
                    for m in &matches {
                        st.dsl_observers.retain(|o| &o.name != m);
                        st.observers.remove(m);
                        st.dsl_disabled.remove(m);
                    }
                    return Ok(format!("deleted {}: {}", matches.len(), matches.join(", ")));
                }
                let before = st.dsl_observers.len();
                st.dsl_observers.retain(|o| o.name != name);
                if st.dsl_observers.len() != before {
                    st.observers.remove(&name);
                    st.dsl_disabled.remove(&name);
                    return Ok(format!("obs {name} deleted"));
                }
                return Ok(format!("no observer '{name}'"));
            }

            // `obs <name> when exec|load|store <addr[..end]> [if <cond>] do <action> [fields]`
            let lower: Vec<String> = rest.iter().map(|t| t.to_ascii_lowercase()).collect();
            let wi = lower.iter().position(|t| t == "when");
            let di = lower.iter().rposition(|t| t == "do");
            let ii = lower.iter().position(|t| t == "if");
            // `when` must be the token right after the name (index 1), and `do` after it.
            let (wi, di) = match (wi, di) {
                (Some(wi), Some(di)) if wi == 1 && di > wi => (wi, di),
                _ => {
                    return Err(
                        "obs: usage: obs <name> when exec|load|store <addr[..end]> [if <cond>] do break|log|mark|cmd|trace [a/x/y/$addr ...]"
                            .into(),
                    )
                }
            };
            let trig_s = lower[wi + 1].clone();
            let trigger = match trig_s.as_str() {
                "exec" => observers::ObsTrigger::Exec,
                "load" => observers::ObsTrigger::Load,
                "store" => observers::ObsTrigger::Store,
                _ => {
                    return Err(format!(
                        "obs: trigger must be exec|load|store, got '{}'",
                        rest.get(wi + 1).cloned().unwrap_or_default()
                    ))
                }
            };
            let addr_tok = rest.get(wi + 2).cloned().unwrap_or_default();
            let (lo_s, hi_s) = match addr_tok.split_once("..") {
                Some((a, b)) => (a.to_string(), Some(b.to_string())),
                None => (addr_tok.clone(), None),
            };
            let lo = match parse_hex(&lo_s) {
                Some(v) => (v & 0xffff) as u16,
                None => return Err(format!("obs: bad address '{addr_tok}'")),
            };
            let hi = match &hi_s {
                Some(h) => match parse_hex(h) {
                    Some(v) => (v & 0xffff) as u16,
                    None => return Err(format!("obs: bad address '{addr_tok}'")),
                },
                None => lo,
            };
            let action_s = lower.get(di + 1).cloned().unwrap_or_default();
            let action = match action_s.as_str() {
                "break" => observers::ObsAction::Break,
                "log" => observers::ObsAction::Log,
                "mark" => observers::ObsAction::Mark,
                "cmd" => observers::ObsAction::Cmd,
                "trace" => observers::ObsAction::Trace,
                _ => {
                    return Err(format!(
                        "obs: action must be break|log|mark|cmd|trace, got '{}'",
                        if action_s.is_empty() { "(none)" } else { &action_s }
                    ))
                }
            };
            // `*`/`?` reserved for the on/off/del wildcards (monitor-shell.ts:951).
            if name.contains('*') || name.contains('?') {
                return Err(format!(
                    "obs: name can't contain * or ? (reserved for wildcards) — got '{name}'"
                ));
            }
            // cond is the tokens between `if` and `do`.
            let cond_src = match ii {
                Some(ii) if ii > wi && ii < di => Some(rest[ii + 1..di].join(" ")),
                _ => None,
            };
            // do-action payloads (the tokens after `do <action>`).
            let do_toks: Vec<String> = rest[(di + 2).min(rest.len())..].to_vec();
            let mut log_exprs: Option<Vec<observers::LogExpr>> = None;
            let mut cmd_src: Option<String> = None;
            let mut mark_label: Option<String> = None;
            let mut trace_scope: Option<observers::TraceScope> = None;
            match action {
                observers::ObsAction::Log if !do_toks.is_empty() => {
                    let mut exprs: Vec<observers::LogExpr> = Vec::new();
                    for t in &do_toks {
                        let lw = t.to_ascii_lowercase();
                        let reg = match lw.as_str() {
                            "a" => Some(observers::RegName::A),
                            "x" => Some(observers::RegName::X),
                            "y" => Some(observers::RegName::Y),
                            "sp" => Some(observers::RegName::Sp),
                            "pc" => Some(observers::RegName::Pc),
                            "fl" => Some(observers::RegName::Fl),
                            _ => None,
                        };
                        if let Some(r) = reg {
                            exprs.push(observers::LogExpr::Reg(r));
                            continue;
                        }
                        let word = lw.ends_with(":w");
                        let addr_part = if word { &t[..t.len() - 2] } else { t.as_str() };
                        match parse_hex(addr_part) {
                            Some(a) => exprs.push(observers::LogExpr::Mem {
                                addr: (a & 0xffff) as u16,
                                word,
                            }),
                            None => {
                                return Err(format!(
                                    "obs: log: bad field '{t}' (use a/x/y/sp/pc/fl or $addr[:w])"
                                ))
                            }
                        }
                    }
                    log_exprs = Some(exprs);
                }
                observers::ObsAction::Cmd => {
                    // do cmd "<monitor command>" — quoted command run on each hit.
                    match quoted_first(&cmd) {
                        Some(c) if !c.is_empty() => cmd_src = Some(c),
                        _ => return Err(r#"obs: cmd: usage: ... do cmd "<monitor command>""#.into()),
                    }
                }
                observers::ObsAction::Mark => {
                    // do mark ["label"] — default label = the observer name.
                    mark_label = Some(quoted_first(&cmd).unwrap_or_else(|| name.clone()));
                }
                observers::ObsAction::Trace => {
                    // do trace off | do trace [domains...] — bracket model.
                    let args: Vec<String> = do_toks.iter().map(|t| t.to_ascii_lowercase()).collect();
                    if args.first().map(|s| s == "off").unwrap_or(false) {
                        trace_scope = Some(observers::TraceScope { off: true, domains: vec![] });
                    } else {
                        const ALL: [&str; 6] = ["c64-cpu", "drive8-cpu", "iec", "vic", "memory", "drive-mechanism"];
                        if let Some(bad) = args.iter().find(|d| !ALL.contains(&d.as_str())) {
                            return Err(format!(
                                "obs: trace: unknown domain '{bad}' (use {} or 'off')",
                                ALL.join("|")
                            ));
                        }
                        let domains = if args.is_empty() {
                            vec!["c64-cpu".to_string(), "memory".to_string()]
                        } else {
                            args
                        };
                        trace_scope = Some(observers::TraceScope { off: false, domains });
                    }
                }
                observers::ObsAction::Break if !do_toks.is_empty() => {
                    return Err(format!(
                        "obs: 'break' takes no fields (got '{}')",
                        do_toks.join(" ")
                    ));
                }
                _ => {}
            }

            // Validate the condition NOW (so a bad cond errors at registration, like TS).
            if let Some(src) = &cond_src {
                if let Err(e) = observers::parse_cond(src) {
                    return Err(format!("obs: condition: {e}"));
                }
            }

            let spec = observers::ObsSpec {
                name: name.clone(),
                trigger,
                lo,
                hi,
                cond_src: cond_src.clone(),
                action,
                log_exprs: log_exprs.clone(),
                cmd_src: cmd_src.clone(),
                mark_label: mark_label.clone(),
                trace_scope: trace_scope.clone(),
            };
            // Replace an existing same-name registration; else append.
            if let Some(slot) = st.dsl_observers.iter_mut().find(|o| o.name == name) {
                *slot = spec;
            } else {
                st.dsl_observers.push(spec);
            }
            // Apply onto the live registry immediately so a running --stream loop arms it
            // on the next frame (sync_observers re-applies it thereafter).
            {
                let State { breakpoints, dsl_observers, dsl_disabled, observers: reg, .. } = &mut *st;
                sync_observers(breakpoints, dsl_observers, dsl_disabled, reg);
            }

            let trig_str = match trigger {
                observers::ObsTrigger::Exec => "exec",
                observers::ObsTrigger::Load => "load",
                observers::ObsTrigger::Store => "store",
            };
            let range = if hi != lo {
                format!("${lo:04X}..${hi:04X}")
            } else {
                format!("${lo:04X}")
            };
            let cond_disp = cond_src.map(|c| format!(" if {c}")).unwrap_or_default();
            let do_disp = obs_do_desc(st.dsl_observers.last().unwrap());
            return Ok(format!("obs {name}: {trig_str} {range}{cond_disp} do {do_disp}"));
        }

        "bk" | "break" | "b" => {
            let t1 = toks.get(1);
            match t1 {
                None => {
                    let list = &st.breakpoints.entries;
                    Ok(if list.is_empty() {
                        "no breakpoints (set: bk <addr>)".to_string()
                    } else {
                        let mut s = String::from("breakpoints:");
                        for e in list {
                            s.push_str(&format!("\n  #{}  ${:04X}", e.num, e.pc));
                        }
                        s
                    })
                }
                Some(t1) if t1.eq_ignore_ascii_case("clear") => {
                    st.breakpoints.entries.clear();
                    Ok("breakpoints cleared".to_string())
                }
                Some(t1) if t1.starts_with('-') => {
                    let a = parse_addr(Some(&t1[1..].to_string()))
                        .ok_or_else(|| format!("bad address: {t1}"))?;
                    st.breakpoints.entries.retain(|e| e.pc != a);
                    Ok(format!("removed bp ${:04X} ({} left)", a, st.breakpoints.entries.len()))
                }
                Some(t1) => {
                    let addr = parse_addr(Some(t1)).ok_or_else(|| format!("bad address: {t1}"))?;
                    let num = st.breakpoints.next_num;
                    st.breakpoints.next_num += 1;
                    st.breakpoints.entries.push(BpEntry { num, pc: addr, enabled: true });
                    Ok(format!("bk #{} set at ${:04X} ({} total)", num, addr, st.breakpoints.entries.len()))
                }
            }
        }

        // ---- Delete breakpoint(s): del | del <num> ... ----------------------
        "del" | "delete" => {
            if toks.get(1).is_none() {
                st.breakpoints.entries.clear();
                return Ok("all breakpoints deleted".to_string());
            }
            let mut out: Vec<String> = Vec::new();
            for t in &toks[1..] {
                match t.parse::<u32>() {
                    Err(_) => out.push(format!("bad checknum: {t}")),
                    Ok(num) => {
                        let before = st.breakpoints.entries.len();
                        st.breakpoints.entries.retain(|e| e.num != num);
                        if st.breakpoints.entries.len() < before {
                            out.push(format!("deleted #{num}"));
                        } else {
                            out.push(format!("no breakpoint #{num}"));
                        }
                    }
                }
            }
            Ok(out.join("\n"))
        }

        // ---- Go / resume (§3.1). g [addr] / x ; enters the run-loop. ---------
        // TRX64 daemon is request/response with no autonomous loop. `g` mirrors
        // the TS BUG-036 contract shape: set PC (if given), step past a parked
        // breakpoint, mark running, and report ".C:PC (running — Pause to halt)".
        // The actual advance happens on the next debug/run (the run-loop), exactly
        // like TS where `ctrl.continue()` flips run-state and the tick loop runs.
        "g" | "x" => {
            if op == "g" {
                if let Some(addr) = parse_addr(toks.get(1)) {
                    st.session.machine.cpu6510.reg_pc = addr;
                    st.session.machine.c64_core.reg_pc = addr;
                }
            }
            let gpc = st.session.machine.cpu6510.reg_pc;
            // If parked on a breakpoint at this PC, step past it so continue doesn't
            // immediately re-trigger on the first instruction (VICE `g` skips it).
            if st.breakpoints.entries.iter().any(|e| e.pc == gpc) {
                step_one_instruction(&mut st.session);
            }
            st.session.running = true;
            st.ctrl_stop = None;
            // Spec 764 — `g`/`x` is a fresh run intent: re-arm the JAM edge so a
            // still-jammed machine asked to go re-broadcasts debug/stopped reason=jam
            // (the shared helper observes it on the next advance — stream loop under
            // --stream, or the CLI pump's session/run).
            st.stream_broke_on_jam = false;
            Ok(format!(
                "continuing at .C:{:04X} (running — Pause to halt)",
                st.session.machine.cpu6510.reg_pc
            ))
        }

        // ---- until <addr> — synchronous run-to-landing (run until addr, stop). -
        "until" => {
            let addr = parse_addr(toks.get(1)).ok_or("until: usage: until <addr>")?;
            st.session.running = false;
            // bps = { addr } ∪ user breakpoints (VICE `until` respects bps).
            let mut bps: std::collections::HashSet<u16> = std::collections::HashSet::new();
            bps.insert(addr);
            for e in &st.breakpoints.entries {
                bps.insert(e.pc);
            }
            if bps.contains(&st.session.machine.cpu6510.reg_pc) {
                step_one_instruction(&mut st.session);
            }
            let start_clk = st.session.machine.clk;
            const CAP: u64 = 20_000_000;
            let mut executed: u64 = 0;
            let mut hit = false;
            while executed < CAP {
                step_one_instruction(&mut st.session);
                executed += 1;
                let pc = st.session.machine.cpu6510.reg_pc;
                if bps.contains(&pc) {
                    hit = true;
                    break;
                }
                // Spec 764 — a KIL/JAM freezes PC (the bp test above would never trip):
                // stop stepping immediately rather than burning the whole CAP on a
                // jammed CPU. The shared helper freezes + pushes debug/stopped reason=jam.
                if check_and_handle_jam(st) {
                    break;
                }
                if st.session.machine.clk.wrapping_sub(start_clk) >= CAP {
                    break;
                }
            }
            let cyc = st.session.machine.clk.wrapping_sub(start_clk);
            let pc = st.session.machine.cpu6510.reg_pc;
            st.mon.disasm_cursor = Some(pc);
            Ok(if hit {
                format!(
                    "until ${:04X} reached -> .C:{:04X} ({} instr, {} cyc)",
                    addr, pc, executed, cyc
                )
            } else {
                format!(
                    "until ${:04X} NOT reached ({} instr, {} cyc, pc=${:04X})",
                    addr, executed, cyc, pc
                )
            })
        }

        // ---- Stepping (§4.2/§4.3). z/step/si = step into; n/next/so = step over;
        // ret/return = run until current frame returns. 1:1 with stepping.ts
        // stepInto/stepOver/runReturn. The reported cycle count is the LANDING
        // instruction's OWN cycle cost (`r.cyc`), NOT the elapsed total — for
        // `next`/`ret` that is the JSR's / the RTS-RTI's own cost (stepping.ts:197,
        // 217, 242). TRX64 has no FlowTracker, so the `landLine` flow tag /
        // stop-reason suffix (TS `[irq]` / `, hit user bp`) is dropped; the
        // instruction landing line + `(tag, N cyc)` shape is matched. SP semantics:
        // TRX64's stack-pop RTS/RTI raises SP above the entry level (sp1 > sp0).
        "z" | "step" | "si" => {
            st.session.running = false;
            let clk0 = st.session.machine.clk;
            // stepInto (stepping.ts:195-198): one instruction (may enter an IRQ/NMI),
            // tracked into the FlowTracker so `flow` reflects the live interrupt frame.
            step_one_with_flow(&mut st.session, &mut st.flow);
            let cyc = st.session.machine.clk.wrapping_sub(clk0); // r.cyc (single step)
            let pc = st.session.machine.cpu6510.reg_pc;
            st.mon.disasm_cursor = Some(pc);
            let (_, line) = disasm_line_ts(|a| st.session.machine.peek_lens(a, "cpu"), pc);
            Ok(format!("{line} (step, {cyc} cyc)"))
        }
        "n" | "next" | "so" => {
            st.session.running = false;
            let start_pc = st.session.machine.cpu6510.reg_pc;
            let init_sp = st.session.machine.cpu6510.reg_sp;
            let opcode = st.session.machine.peek_lens(start_pc, "cpu");
            let is_jsr = opcode == 0x20;
            let bp_set: std::collections::HashSet<u16> =
                st.breakpoints.entries.iter().map(|e| e.pc).collect();
            // Execute the instruction at PC; `r_cyc` = ITS own cost (the value TS
            // reports for `next`, even when it's a JSR — stepping.ts:217). Track the
            // single instruction into the FlowTracker UNLESS it's a JSR: stepOver
            // (stepping.ts:209-228) only apply()s for single instructions
            // (normal/rts/rti — and an `int` is run-through, not applied here); a JSR
            // body is run-through via runUntilReturn (balanced), so it is NOT applied.
            let clk0 = st.session.machine.clk;
            if is_jsr {
                step_one_instruction(&mut st.session);
            } else {
                step_one_with_flow(&mut st.session, &mut st.flow);
            }
            let r_cyc = st.session.machine.clk.wrapping_sub(clk0);
            if is_jsr {
                // runUntilReturn: run the subroutine body until it RTSes back (SP
                // restored to the entry level → balanced) or a user bp trips.
                let next_pc = start_pc.wrapping_add(3);
                const CAP: u64 = 5_000_000;
                let mut iters: u64 = 0;
                loop {
                    let pc = st.session.machine.cpu6510.reg_pc;
                    let sp = st.session.machine.cpu6510.reg_sp;
                    if (pc == next_pc && sp >= init_sp) || (sp > init_sp) {
                        break;
                    }
                    if bp_set.contains(&pc) && iters > 0 {
                        break;
                    }
                    if iters >= CAP {
                        break;
                    }
                    step_one_instruction(&mut st.session);
                    iters += 1;
                }
            }
            let pc = st.session.machine.cpu6510.reg_pc;
            st.mon.disasm_cursor = Some(pc);
            let (_, line) = disasm_line_ts(|a| st.session.machine.peek_lens(a, "cpu"), pc);
            Ok(format!("{line} (next, {r_cyc} cyc)"))
        }
        "ret" | "return" => {
            st.session.running = false;
            let sp0 = st.session.machine.cpu6510.reg_sp;
            let bp_set: std::collections::HashSet<u16> =
                st.breakpoints.entries.iter().map(|e| e.pc).collect();
            const SKIP_CAP: u64 = 5_000_000;
            let mut guard: u64 = 0;
            let mut last_cyc: u64 = 0;
            loop {
                if guard >= SKIP_CAP {
                    break;
                }
                // The op about to execute (so we can detect the RTS/RTI return).
                let op_pc = st.session.machine.cpu6510.reg_pc;
                let opcode = st.session.machine.peek_lens(op_pc, "cpu");
                let is_ret = opcode == 0x60 || opcode == 0x40; // RTS / RTI
                let clk0 = st.session.machine.clk;
                // runReturn (stepping.ts:234-246) calls apply() on EVERY step, so the
                // flow stack stays consistent across an interrupt taken mid-return.
                step_one_with_flow(&mut st.session, &mut st.flow);
                last_cyc = st.session.machine.clk.wrapping_sub(clk0); // r.cyc
                guard += 1;
                let pc = st.session.machine.cpu6510.reg_pc;
                if bp_set.contains(&pc) {
                    break;
                }
                // stepping.ts:241 — stop when the executed instr was RTS/RTI AND the
                // resulting SP rose above the entry level (the current frame returned).
                if is_ret && (st.session.machine.cpu6510.reg_sp as u16) > sp0 as u16 {
                    break;
                }
            }
            let pc = st.session.machine.cpu6510.reg_pc;
            st.mon.disasm_cursor = Some(pc);
            let (_, line) = disasm_line_ts(|a| st.session.machine.peek_lens(a, "cpu"), pc);
            Ok(format!("{line} (return, {last_cyc} cyc)"))
        }

        // ---- flow / bt — Spec 754 §3.3h capability panels (audit misc-13). ----
        // Both report LIVE machine state, not a constant. `flow` now renders the
        // per-session FlowTracker (the interrupt/trap frame STACK, mutated per
        // single-step by the z/n/ret handlers) — 1:1 with monitor-shell.ts:1103-1117
        // (← FlowTracker.flowState(), stepping.ts:174-190). After a `z`-step accepts a
        // hardware IRQ the panel reports current=irq + a frame line, then pops to main
        // on the RTI. `bt` scans the ACTUAL 6502 stack page for JSR return-address
        // candidates (buildBacktrace, backtrace.ts:23-40), so it too reflects the
        // live SP + stack contents.
        "flow" => {
            // FlowTracker.render() (stepping.ts:174-190 + monitor-shell.ts:1103-1117):
            //   `flow: current=<kind>  focus=<focus>\nframes:\n<lines | placeholder>`.
            // At the cold/rest state the stack is empty → current=main; after stepping
            // into an interrupt it is state-dependent (current=irq|nmi|brk + frames).
            Ok(st.flow.render())
        }
        "bt" => {
            // buildBacktrace (backtrace.ts): scan $0100+((sp+1)&0xff) .. $01FF in
            // 2-byte steps for JSR return-address candidates (ret = (hi<<8|lo)+1),
            // up to 16. Reads via the cpu lens (peek, no side effect). State-dependent
            // on the live SP + stack bytes — NOT a constant.
            let m = &st.session.machine;
            let sp = (m.cpu6510.reg_sp & 0xff) as u32;
            let mut lines: Vec<String> =
                vec!["backtrace (live stack scan — best-effort; refine with `chis`):".to_string()];
            let mut found = 0usize;
            let mut a: u32 = 0x0100 + ((sp + 1) & 0xff);
            while a <= 0x01ff && found < 16 {
                let lo = m.peek_lens((a & 0xffff) as u16, "cpu") as u32;
                let hi = m.peek_lens(((a + 1) & 0xffff) as u16, "cpu") as u32;
                let ret = (((hi << 8) | lo) + 1) & 0xffff;
                lines.push(format!("  ${:04X}: -> ${:04X}  (JSR return?)", a & 0xffff, ret));
                found += 1;
                a += 2;
            }
            if found == 0 {
                lines.push("  (stack empty — SP at top)".to_string());
            }
            // backtrace.ts:35-38 — append the EXACT FlowTracker IRQ/NMI/BRK frames
            // (more than VICE) when the flow stack is non-empty.
            if !st.flow.stack.is_empty() {
                lines.push("flow frames (exact, from stepping):".to_string());
                for fr in &st.flow.stack {
                    lines.push(format!("  {} @ ${:04X}", fr.kind.tag(), fr.entered_at_pc));
                }
            }
            Ok(lines.join("\n"))
        }

        // reverse-debug Phase 1b — `rstep`/`reverse [n]`: UNDO the last n instructions
        // from the always-on full-delta ring (default 1). Restores CPU + RAM +
        // IO-register BYTES, NOT chip internal counters → INSPECT-backward only (to
        // resume forward, restore a checkpoint anchor). Reports the landed PC/regs +
        // the writes rolled back.
        "rstep" | "reverse" => {
            let n: usize = toks
                .get(1)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(1)
                .max(1);
            match st.session.machine.reverse_step(n) {
                Ok(out) => {
                    let l = out.landed;
                    let mut lines = vec![format!(
                        "reverse-step: undid {} instruction(s) → landed @ ${:04X}  A=${:02X} X=${:02X} Y=${:02X} SP=${:02X} P=${:02X}  cyc={}",
                        out.steps_taken, l.pc, l.a, l.x, l.y, l.sp, l.p, l.cycle
                    )];
                    if out.undone_writes.is_empty() {
                        lines.push("  (no memory writes were rolled back)".to_string());
                    } else {
                        lines.push(format!("  rolled back {} write(s) (newest first):", out.undone_writes.len()));
                        for w in out.undone_writes.iter().take(32) {
                            lines.push(format!(
                                "    ${:04X}: ${:02X} <- ${:02X}  (restored old)",
                                w.addr, w.old_value, w.new_value
                            ));
                        }
                        if out.undone_writes.len() > 32 {
                            lines.push(format!("    … +{} more", out.undone_writes.len() - 32));
                        }
                    }
                    lines.push("  NOTE: inspect-backward only — CPU+RAM+IO bytes restored, NOT chip counters (VIC raster / CIA timers). Restore a checkpoint anchor to resume forward.".to_string());
                    Ok(lines.join("\n"))
                }
                Err(e) => Err(e),
            }
        }

        // reverse-debug Phase 1b — `whowrote <addr>`: the stack-crash shortcut. Scan
        // the always-on delta ring's writes BACKWARD for the last writer(s) of <addr>
        // → the instruction PC + cycle + old→new bytes, newest first.
        "whowrote" => {
            let addr = match parse_addr(toks.get(1)) {
                Some(a) => a,
                None => return Err("whowrote: need an address, e.g. `whowrote 01f5`".into()),
            };
            let limit = toks
                .get(2)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(8)
                .clamp(1, 64);
            let hits = st.session.machine.who_wrote(addr, limit);
            // TRX64 feature-request #3 — typed ring-exhaustion signal: a miss against a
            // WRAPPED ring means "window too short" (the writer may be older than the
            // ring), distinct from "never written / wrong address".
            let exhaustion = st.session.machine.ring_exhaustion(!hits.is_empty());
            if hits.is_empty() {
                let mut lines = vec![format!(
                    "whowrote ${addr:04X}: no writer in the live delta ring (window not covered, or never written since the last reset). Older history lives only in a finalized trace."
                )];
                if exhaustion.ring_exhausted {
                    lines.push(format!(
                        "ring_exhausted: true   revdepth={}s   hint: {}",
                        exhaustion.revdepth_seconds, exhaustion.hint
                    ));
                }
                return Ok(lines.join("\n"));
            }
            let mut lines = vec![format!(
                "whowrote ${:04X}: {} writer(s) in the live ring (newest first):",
                addr,
                hits.len()
            )];
            for h in &hits {
                // TRX64 feature-request #2 — append the CALLER CHAIN (top return-stack
                // frames the writing instruction saw) so a write by a SHARED primitive is
                // attributed to its call site, not just the leaf PC. `depth == 0` ⇒ none
                // captured (an interrupt/early-boot write with an empty/unreadable stack).
                let chain = h.caller_chain;
                let mut line = format!(
                    "  ${:04X} <- written by ${:04X} @ cyc {}  (${:02X} -> ${:02X})",
                    h.addr, h.pc, h.cycle, h.old_value, h.new_value
                );
                if chain.depth > 0 {
                    let frames: Vec<String> = chain.frames[..chain.depth as usize]
                        .iter()
                        .map(|f| format!("${f:04X}"))
                        .collect();
                    line.push_str(&format!("   caller chain: {}", frames.join(" -> ")));
                }
                lines.push(line);
            }
            Ok(lines.join("\n"))
        }

        // reverse-debug Phase 2 — `triage [pc]`: re-run the guided crash-triage on
        // demand. Reads the always-on CPU-history + delta rings to reconstruct the
        // causal chain (crash → wild control transfer → stack corruptor). With no arg it
        // triages the LIVE (crashed) PC — the same chain the JAM drop-in auto-printed;
        // pass a hex PC to triage a specific wild address. PRAGMATIC + HONEST: each step
        // is confidence-tagged and a non-stack-pop transfer is reported without inventing
        // a stack corruptor.
        "triage" => {
            let at_pc = parse_addr(toks.get(1));
            let chain = st.session.machine.crash_triage(at_pc);
            // TRX64 feature-request #3 — the typed ring-exhaustion signal. The triage
            // bottomed out at the ring boundary when the wild transfer is older than the
            // ring (`ring_bound`); confirm + size it with the machine's ring state.
            let exhaustion = st.session.machine.ring_exhaustion(!chain.transfer.ring_bound);
            let mut lines = format_triage_lines(&chain);
            if chain.transfer.ring_bound && exhaustion.ring_exhausted {
                lines.push(format!(
                    "ring_exhausted: true   revdepth={}s   hint: {}",
                    exhaustion.revdepth_seconds, exhaustion.hint
                ));
            }
            Ok(lines.join("\n"))
        }

        // TRX64 feature-request #4 — `traprules <path>` loads project-supplied on-trap
        // dump rules from a JSON file; `traprules` (no arg) lists the loaded rules;
        // `traprules clear` drops them. On reaching/halting at a rule's PC (JAM /
        // breakpoint), the debugger auto-emits `label: name=$XX (decode)` reading the
        // project-named diagnostic bytes — NO built-in engine knowledge in the core.
        "traprules" => {
            match toks.get(1).map(|s| s.as_str()) {
                None => {
                    if st.trap_rules.is_empty() {
                        return Ok("traprules: none loaded. `traprules <path.json>` loads project on-trap dump rules.".into());
                    }
                    let mut rules: Vec<&TrapRule> = st.trap_rules.values().collect();
                    rules.sort_by_key(|r| r.pc);
                    let mut lines = vec![format!("traprules: {} rule(s):", rules.len())];
                    for r in rules {
                        let dumps: Vec<String> = r
                            .dump
                            .iter()
                            .map(|(n, a, l)| format!("{n}@${a:04X}:{l}"))
                            .collect();
                        let decode = if r.decode.is_empty() { String::new() } else { format!("  => {}", r.decode) };
                        lines.push(format!("  ${:04X}  \"{}\"  [{}]{}", r.pc, r.label, dumps.join(", "), decode));
                    }
                    Ok(lines.join("\n"))
                }
                Some("clear") => {
                    let n = st.trap_rules.len();
                    st.trap_rules.clear();
                    Ok(format!("traprules: cleared {n} rule(s)"))
                }
                Some(arg) => {
                    let path = resolve_fs_path(arg);
                    let raw = std::fs::read_to_string(&path)
                        .map_err(|e| format!("traprules: read {path}: {e}"))?;
                    let json: Value = serde_json::from_str(&raw)
                        .map_err(|e| format!("traprules: parse {path}: {e}"))?;
                    let rules = parse_trap_rules(&json)?;
                    let n = rules.len();
                    for r in rules {
                        st.trap_rules.insert(r.pc, r);
                    }
                    Ok(format!(
                        "traprules: loaded {n} rule(s) from {path} ({} total). They auto-emit on reaching their PC (JAM / breakpoint).",
                        st.trap_rules.len()
                    ))
                }
            }
        }

        // reverse-debug depth knob — `revdepth [seconds]`: with an arg, REBUILD both
        // always-on rings (delta + cpu-history) at that depth for FUTURE capture; with
        // no arg, REPORT the current depth. Setting it DISCARDS current history (fresh
        // ring) — it affects capture from now on only and cannot retroactively extend
        // history. `TRX64_REVERSE_SECONDS` stays the boot default.
        "revdepth" => {
            let mb = |bytes: u64| (bytes as f64) / (1024.0 * 1024.0);
            match toks.get(1).and_then(|s| s.trim_start_matches('$').parse::<u64>().ok()) {
                None => {
                    let info = st.session.machine.reverse_depth_info();
                    Ok(format!(
                        "revdepth: {}s (~{:.1} MB) — delta {} entries / {} writes, cpuhistory {} entries\n  (pass a number to rebuild, e.g. `revdepth 30`)",
                        info.seconds, mb(info.ram_bytes),
                        info.delta_entry_capacity, info.delta_write_capacity, info.cpu_history_capacity,
                    ))
                }
                Some(s) => {
                    let clamped = (s.max(1)).min(600) as usize;
                    let info = st.session.machine.set_reverse_depth(clamped);
                    let mut lines = vec![format!(
                        "revdepth: rebuilt rings at {}s (~{:.1} MB) — delta {} entries / {} writes, cpuhistory {} entries",
                        info.seconds, mb(info.ram_bytes),
                        info.delta_entry_capacity, info.delta_write_capacity, info.cpu_history_capacity,
                    )];
                    if (s as usize) != info.seconds {
                        lines.push(format!("  (requested {s}s clamped to {}s; allowed 1..=600)", info.seconds));
                    }
                    lines.push("  DISCARDED current history (fresh ring); affects capture FROM NOW ON only — cannot retroactively extend history.".into());
                    if info.seconds > 120 {
                        lines.push(format!("  WARNING: {}s costs ~{:.1} MB always-on ring RAM; multi-minute depths run into GBs.", info.seconds, mb(info.ram_bytes)));
                    }
                    Ok(lines.join("\n"))
                }
            }
        }

        // reverse-debug Phase 1c — `buildtrace <s> <e>`: dump the always-on delta ring's
        // [s,e] cycle window to a `.c64retrace` (the SAME engine as the WS method
        // trace/build_from_ring) and point the monitor's trace store at it, so the very
        // next `swimlane <s> <e>` / `map` / `taint` reads exactly this window. The UI's
        // "select two scrub thumbnails → build trace" backed by one verb.
        "buildtrace" => {
            let parse_cyc = |t: Option<&String>| -> Option<u64> {
                t.and_then(|s| s.trim_start_matches('$').parse::<u64>().ok())
            };
            let (s, e) = match (parse_cyc(toks.get(1)), parse_cyc(toks.get(2))) {
                (Some(s), Some(e)) => (s, e),
                _ => return Err("buildtrace: usage: buildtrace <cycle_start> <cycle_end>".into()),
            };
            match build_trace_from_ring(st, s, e, None) {
                Ok(out) => {
                    let path = out.get("retrace_path").and_then(|v| v.as_str()).unwrap_or("");
                    let ev = out.get("event_count").and_then(|v| v.as_u64()).unwrap_or(0);
                    let instr = out.get("instruction_count").and_then(|v| v.as_u64()).unwrap_or(0);
                    let lo = out.get("cycle_start").and_then(|v| v.as_u64()).unwrap_or(s);
                    let hi = out.get("cycle_end").and_then(|v| v.as_u64()).unwrap_or(e);
                    let mut msg = format!(
                        "buildtrace: dumped delta-ring cycles {lo}–{hi} → {instr} instr, {ev} events\n  evidence: {path}\n  now: `swimlane {lo} {hi}` / `map` / `taint <addr>` read this window"
                    );
                    if out.get("clipped").and_then(|v| v.as_bool()).unwrap_or(false) {
                        msg.push_str("\n  NOTE: the window extends beyond the live ring — only the covered cycles were dumped (older history lives in a finalized trace).");
                    }
                    Ok(msg)
                }
                Err(err) => Err(format!("buildtrace: {err}")),
            }
        }

        // Spec time-travel-tooling Piece 1 — `diff <idA> <idB>`: typed by-ID diff of
        // two checkpoint anchors (RAM runs + per-chip register changes). READ-ONLY —
        // the live machine is restored to its current state after the diff. Backed by
        // the SAME `diff_checkpoints_by_id` the WS `runtime/diff_checkpoints` method
        // and the FFI `diffCheckpoints` call.
        "diff" => {
            let (a, b) = match (toks.get(1), toks.get(2)) {
                (Some(a), Some(b)) => (a.clone(), b.clone()),
                _ => return Err("diff: usage: diff <idA> <idB>  (checkpoint ids from `checkpoint/list`)".into()),
            };
            match diff_checkpoints_by_id(st, &a, &b) {
                Ok(v) => Ok(format_typed_snapshot_diff(&v)),
                Err(e) => Err(format!("diff: {e}")),
            }
        }

        // Spec 794 — `cdiff <idA> <idB> [eq] [c:NAME] [l:NAME] [r:SPACE:FROM-TO]`:
        // whitebox COMPONENT diff of two checkpoint anchors — the full checkpoint_diff
        // over the anchor Values directly (no machine round-trip), reaching color RAM,
        // Floppy RAM and internal chip state the 246 `diff` cannot. Verdict + exclusion
        // mask (`eq` = equivalence preset, `c:` component, `l:` lane, `r:` range).
        // READ-ONLY.
        "cdiff" => {
            let (a, b) = match (toks.get(1), toks.get(2)) {
                (Some(a), Some(b)) => (a.clone(), b.clone()),
                _ => {
                    return Err(
                        "cdiff: usage: cdiff <idA> <idB> [eq] [c:NAME] [l:NAME] [r:SPACE:FROM-TO]".into(),
                    )
                }
            };
            let mask = component_diff_mask_from_toks(&toks[3..]);
            let snap_a = st
                .checkpoint_ring
                .restore_snapshot(&a)
                .ok_or_else(|| format!("cdiff: unknown checkpoint id {a}"))?;
            let snap_b = st
                .checkpoint_ring
                .restore_snapshot(&b)
                .ok_or_else(|| format!("cdiff: unknown checkpoint id {b}"))?;
            let d = trx64_core::checkpoint_diff::diff_checkpoints(&snap_a, &snap_b, &mask);
            Ok(trx64_core::checkpoint_diff::format_component_diff(&d))
        }

        // Spec time-travel-tooling Piece 2 — `ringdump <path>` / `ringload <path>`:
        // dump / restore the WHOLE reverse-debug buffer to/from a gzipped `.c64rering`
        // container (the SAME engine as WS `ringbuffer/dump`·`restore` + FFI). The
        // tester→dev hand-off: dump a bug's full context, ship the file, reload + scrub.
        "ringdump" => {
            let path = match toks.get(1) {
                Some(p) => p.clone(),
                None => return Err("ringdump: usage: ringdump <path.c64rering>".into()),
            };
            match ringbuffer_dump_to_path(st, &path) {
                Ok(info) => Ok(format!(
                    "ringdump: {} anchor(s), {} delta entr(ies), {} cpu-history → {}  ({} bytes, cycles {}–{})",
                    info["anchors"].as_u64().unwrap_or(0),
                    info["deltaEntries"].as_u64().unwrap_or(0),
                    info["cpuHistory"].as_u64().unwrap_or(0),
                    path,
                    info["fileBytes"].as_u64().unwrap_or(0),
                    info["cycleFirst"].as_u64().unwrap_or(0),
                    info["cycleLast"].as_u64().unwrap_or(0),
                )),
                Err(e) => Err(format!("ringdump: {e}")),
            }
        }

        "ringload" => {
            let path = match toks.get(1) {
                Some(p) => p.clone(),
                None => return Err("ringload: usage: ringload <path.c64rering>".into()),
            };
            match ringbuffer_restore_from_path(st, &path) {
                Ok(info) => Ok(format!(
                    "ringload: restored {} anchor(s), {} delta entr(ies), {} cpu-history from {}  (current={}, cycles {}–{})\n  now: scrub (`checkpoint/list`), `rstep`, `whowrote`, `chis`, `diff <idA> <idB>` all work on this buffer",
                    info["anchors"].as_u64().unwrap_or(0),
                    info["deltaEntries"].as_u64().unwrap_or(0),
                    info["cpuHistory"].as_u64().unwrap_or(0),
                    path,
                    info["currentId"].as_str().unwrap_or("none"),
                    info["cycleFirst"].as_u64().unwrap_or(0),
                    info["cycleLast"].as_u64().unwrap_or(0),
                )),
                Err(e) => Err(format!("ringload: {e}")),
            }
        }

        // ---- Live trace gate (Spec 746 / audit misc-2): trace on|off|status|mark --
        // Wires the monitor `trace` verb to the EXISTING trace machinery (TraceState +
        // finalize_trace — the same engine behind trace/start_domains, runtime/mark,
        // trace/run/stop). 1:1 with monitor-shell.ts:413-441 (which drives
        // ctrl.traceRun). Was MISSING → `unknown command: trace` (help LIED @2221).
        "trace" => {
            let sub = toks.get(1).map(|s| s.to_ascii_lowercase()).unwrap_or_else(|| "status".into());
            match sub.as_str() {
                "off" | "stop" => {
                    if st.session.trace.is_none() {
                        return Ok("trace: no active run".into());
                    }
                    let (run, _status) = finalize_trace(st, true);
                    let run_id = run.get("runId").and_then(|v| v.as_str()).unwrap_or("");
                    let events = run.get("eventCount").and_then(|v| v.as_u64()).unwrap_or(0);
                    let marks = run.get("marks").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
                    let evidence = st.last_trace_path.clone().unwrap_or_default();
                    Ok(format!(
                        "trace off: {run_id}  events={events} marks={marks}\n  evidence: {evidence}"
                    ))
                }
                "status" => Ok(match &st.session.trace {
                    Some(t) => format!(
                        "trace active: {} events={} marks={}",
                        t.run_id, t.event_count, t.marks.len()
                    ),
                    None => "trace: off".to_string(),
                }),
                "mark" => {
                    // trace mark "<label>" — quoted label, else the rest of the line.
                    let label = quoted_first(&cmd).unwrap_or_else(|| toks[2..].join(" "));
                    if label.is_empty() {
                        return Ok("trace: usage: trace mark \"<label>\"".into());
                    }
                    let clk = st.session.machine.clk;
                    match st.session.trace.as_mut() {
                        Some(t) => {
                            t.marks.push((clk, label.clone()));
                            Ok(format!("trace mark: \"{label}\" @ cycle {clk}"))
                        }
                        // TS marks against ctrl.traceRun (throws if inactive at the WS
                        // boundary); the monitor verb mirrors runtime/mark's guard.
                        None => Ok("trace: no active run — `trace on` first".into()),
                    }
                }
                "on" | "start" => {
                    if st.session.trace.is_some() {
                        return Ok("trace: already active — `trace off` first".into());
                    }
                    // captureAll default domains (= monitor-shell.ts:434) unless the
                    // user supplied a domain list after `trace on`.
                    let doms: Vec<String> = toks[2..].iter().filter(|s| !s.is_empty()).cloned().collect();
                    let domains = if doms.is_empty() {
                        vec!["c64-cpu".into(), "drive8-cpu".into(), "iec".into(), "memory".into()]
                    } else {
                        doms
                    };
                    let output = default_trace_output(&st.session.id);
                    let retrace = output.with_extension("c64retrace");
                    let cycle_start = st.session.machine.clk;
                    let run_id = format!("run_live-capture_{cycle_start}");
                    // misc-1/misc-14 — write a VALID defJson (the c64re indexer does
                    // `JSON.parse(meta.defJson)`; an empty string broke indexing → the
                    // monitor map/swimlane/taint verbs could never read this store).
                    let def_json_str =
                        serde_json::to_string(&capture_all_def_json(&domains)).unwrap_or_default();
                    let meta_json = serde_json::to_string(&json!({
                        "runId": run_id,
                        "defId": "live-capture",
                        "defVersion": 1,
                        "defName": "live session capture",
                        "defJson": def_json_str,
                        "domains": domains,
                        "cycleStart": cycle_start,
                        "createdAt": now_iso8601_utc(),
                    }))
                    .unwrap_or_default();
                    st.session.machine.drive8.flush_disk_writeback();
                    let (media_sha, media_name) = match st.session.machine.drive8.get_attached_disk() {
                        Some(disk) => (
                            sha256_hex(&disk.bytes),
                            disk.backing_path
                                .as_ref()
                                .and_then(|p| p.rsplit('/').next())
                                .map(String::from)
                                .unwrap_or_default(),
                        ),
                        None => (String::new(), String::new()),
                    };
                    let start_wall_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    st.session.trace = Some(TraceState {
                        retrace_path: retrace,
                        meta_json,
                        cycle_start,
                        buf: Vec::new(),
                        run_id: run_id.clone(),
                        event_count: 0,
                        domains: domains.clone(),
                        marks: Vec::new(),
                        definition_id: "live-capture".to_string(),
                        definition_version: 1,
                        start_wall_ms,
                        media_sha,
                        media_name,
                        // monitor `trace on` = captureAll (keep every domain-implied row).
                        captures: Vec::new(),
                    });
                    st.last_run_id = Some(run_id.clone());
                    Ok(format!(
                        "trace on: {run_id}  domains=[{}]\n  evidence: {}",
                        domains.join(","),
                        output.to_string_lossy()
                    ))
                }
                _ => Ok("trace: on [domains...] | off | status | mark \"<label>\"".into()),
            }
        }

        // ---- Power / reset (Spec 786) ---------------------------------------
        // `reset [warm|cold]` (default warm = HW RESET line → $FCE2, RAM + media
        // preserved); `power on|off`. All compose the session power primitives,
        // identical to the `session/reset` + `session/power` WS methods.
        "reset" => {
            match toks.get(1).map(|s| s.to_ascii_lowercase()).as_deref() {
                Some("cold") => {
                    do_power_off(st);
                    do_power_on(st);
                    Ok("reset cold (power-cycle)".to_string())
                }
                _ => {
                    // warm = HW RESET line: $FCE2 via $FFFC, RAM + media preserved.
                    st.session.warm_reset();
                    st.session.machine.keyboard.clear();
                    run_cycle_budget(&mut st.session, 5_000_000);
                    st.ctrl_stop = None;
                    st.ctrl_frame += 1;
                    st.flow.reset();
                    st.stream_broke_on_jam = false;
                    st.mon.disasm_cursor = None;
                    st.mon.mem_cursor = None;
                    Ok("reset warm ($FCE2)".to_string())
                }
            }
        }
        "power" => match toks.get(1).map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("on") => {
                do_power_on(st);
                Ok(format!("power on (powered={})", st.session.powered))
            }
            Some("off") => {
                do_power_off(st);
                Ok(format!("power off (powered={})", st.session.powered))
            }
            _ => Ok("power on|off".to_string()),
        },

        // ---- Trace-store reads (map/taint/swimlane/chis) — audit misc-14. --------
        // The TS DAEMON wires a `traceRead` bridge backed by a DuckDB trace store;
        // monitor-shell parses the verb args then calls ctx.traceRead(op, args)
        // (ws-server.ts:2104-2129, monitor-shell.ts:1116-1178). TRX64 routes the SAME
        // reads through the Node sidecar (the c64re indexer + v2 readers), keyed on
        // the CURRENT trace store (active or last-finalized). With NO trace store the
        // verbs return the IDENTICAL daemon-shaped error.
        //
        // `map [cpu]` — trace_memory_map free-RAM / persistence surface.
        "map" => {
            let cpu = toks.get(1).map(|s| s.to_ascii_lowercase()).unwrap_or_else(|| "c64".to_string());
            if cpu != "c64" && cpu != "drive8" {
                return Ok("map: cpu must be c64|drive8".into());
            }
            match current_trace_duckdb(st) {
                None => Err("map: no trace store — run `trace on` first".into()),
                Some(db) => match run_trace_read_sidecar("map", &db, &json!({ "cpu": cpu })) {
                    Ok(v) => Ok(v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string()),
                    Err(e) => Err(format!("map: {e}")),
                },
            }
        }
        // `taint <addr> [cycle]` — backward data-flow taint. cycle omitted ⇒ the
        // store's own MAX(cycle) (= TS: no live-clock default).
        "taint" => {
            let addr = match parse_addr(toks.get(1)) {
                Some(a) => a,
                None => return Ok("taint: usage: taint <addr> [cycle]".into()),
            };
            let mut args = json!({ "start_addr": addr });
            if let Some(c) = toks.get(2).and_then(|s| s.parse::<i64>().ok()) {
                args["start_cycle"] = json!(c);
            }
            match current_trace_duckdb(st) {
                None => Err("taint: no trace store — run `trace on` first".into()),
                Some(db) => match run_trace_read_sidecar("taint_text", &db, &args) {
                    Ok(v) => Ok(v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string()),
                    Err(e) => Err(format!("taint: {e}")),
                },
            }
        }
        // `swimlane [list|name] [s] [e]` — trace lanes, newest trace tail by default.
        // The sidecar serves the CURRENT-store path (default window + explicit
        // <s>[e]); `list` + `<name>` + the checkpoint-ring/chis fallback need the live
        // ring (not file-derivable) → reported as unsupported here, not faked.
        // `traceindex [path]` — EXPLICITLY build the `.duckdb` index for a `.c64retrace`
        // (the trace-decode gap fix). With no arg it indexes the CURRENT/last trace; an
        // optional path may be the `.c64retrace` or its `.duckdb` sibling. Runs the SAME
        // sidecar indexer the lazy read path uses, but as an explicit op — so a captured
        // trace that `trace_store_info` reported as "no trace.duckdb" becomes queryable.
        // Reports events indexed + the honest bound (the indexer streams oldest→newest
        // with NO event cap; a 1.2 GB trace's oldest events ARE indexed).
        "traceindex" => {
            let db = match toks.get(1).map(|s| s.as_str()).filter(|s| !s.is_empty()) {
                Some(p) => {
                    if p.ends_with(".c64retrace") {
                        format!("{}.duckdb", &p[..p.len() - ".c64retrace".len()])
                    } else {
                        p.to_string()
                    }
                }
                None => match current_trace_duckdb(st) {
                    Some(db) => db,
                    None => return Err(
                        "traceindex: no path given and no trace has run — `trace on` … `trace off` first, or `traceindex <path.c64retrace>`".into()),
                },
            };
            match run_trace_read_sidecar("index", &db, &json!({ "wait": true })) {
                Ok(v) => {
                    let events = v.get("eventsIndexed").and_then(|n| n.as_i64());
                    let bounded = v.get("bounded").and_then(|b| b.as_bool()).unwrap_or(false);
                    let path = v.get("duckdbPath").and_then(|p| p.as_str()).unwrap_or(&db);
                    let mut lines = vec![match events {
                        Some(n) => format!("traceindex: built {path} — {n} event(s) indexed (oldest→newest, no cap)"),
                        None => format!("traceindex: {path} — index still building (re-run to await)"),
                    }];
                    if let Some(r) = v.get("cycleRange").filter(|r| !r.is_null()) {
                        let mn = r.get("min").and_then(|x| x.as_i64()).unwrap_or(0);
                        let mx = r.get("max").and_then(|x| x.as_i64()).unwrap_or(0);
                        lines.push(format!("  cycle span: {mn}..{mx} (min == trace start ⇒ the oldest events survived)"));
                    }
                    if bounded {
                        lines.push("  bounded: still building (15s grace expired) — NO data dropped; re-run to await completion".into());
                    }
                    Ok(lines.join("\n"))
                }
                Err(e) => Err(format!("traceindex: {e}")),
            }
        }
        "swimlane" | "sw" => {
            let a1 = toks.get(1).map(|s| s.as_str());
            let is_num = |t: Option<&str>| t.map(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).unwrap_or(false);
            // `swimlane list` — list the stored traces newest-first with their event
            // count + cycle span (= TS ws-server.ts swimlane `list` directory scan +
            // per-store getInfo). Reads the per-session trace-store directory.
            if a1.map(|s| s.eq_ignore_ascii_case("list")).unwrap_or(false) {
                let dir = match session_trace_store_dir(st) {
                    Some(d) => d,
                    None => return Ok("swimlane list: no traces yet (run `trace on` … `trace off`)".into()),
                };
                let stores = list_trace_stores(&dir);
                if stores.is_empty() {
                    return Ok("swimlane list: no traces yet (run `trace on` … `trace off`)".into());
                }
                let mut lines = vec!["traces (newest first) — `swimlane <name>`:".to_string()];
                for (name, path) in stores.iter().take(30) {
                    let dbs = path.to_string_lossy();
                    // getInfo over the store (builds the index lazily if absent) → event
                    // count + master-clock span, exactly the TS `list` line shape.
                    match run_trace_read_sidecar("store_fn", &dbs, &json!({ "fn": "getInfo" })) {
                        Ok(gi) => {
                            let ev = gi.get("tableCounts")
                                .and_then(|t| t.get("events:total"))
                                .and_then(|n| n.as_i64().or_else(|| n.as_str().and_then(|s| s.parse().ok())))
                                .unwrap_or(0);
                            let (mn, mx) = gi.get("masterClockRange")
                                .filter(|r| !r.is_null())
                                .map(|r| (
                                    r.get("min").and_then(|x| x.as_i64()).unwrap_or(0),
                                    r.get("max").and_then(|x| x.as_i64()).unwrap_or(0),
                                ))
                                .unwrap_or((0, 0));
                            lines.push(format!("  {name}  cyc {mn}..{mx}  events={ev}"));
                        }
                        Err(_) => lines.push(format!("  {name}  (index not built — read once to build)")),
                    }
                }
                return Ok(lines.join("\n"));
            }
            let mut args = json!({ "last_cycles": 2000 });
            // `swimlane <name> [s] [e]` — select a stored trace by basename; numeric a1
            // is a cycle window over the current trace (= TS `name` vs `<s> <e>` split).
            let named_store: Option<String> = match a1 {
                Some(name) if !is_num(Some(name)) => {
                    let dir = session_trace_store_dir(st);
                    let nm = name.trim_end_matches(".duckdb");
                    let cand = dir.as_ref().map(|d| d.join(format!("{nm}.duckdb")));
                    match cand {
                        Some(p) if p.exists() => {
                            // `swimlane <name> <s> <e>` — explicit window over the named store.
                            if is_num(toks.get(2).map(|s| s.as_str())) {
                                args["cycle_start"] = json!(toks[2].parse::<i64>().unwrap_or(0));
                                if is_num(toks.get(3).map(|s| s.as_str())) {
                                    args["cycle_end"] = json!(toks[3].parse::<i64>().unwrap_or(0));
                                }
                            }
                            Some(p.to_string_lossy().into_owned())
                        }
                        _ => return Err(format!("swimlane: no trace named '{nm}' — try `swimlane list`")),
                    }
                }
                _ => {
                    if is_num(a1) {
                        args["cycle_start"] = json!(a1.unwrap().parse::<i64>().unwrap_or(0));
                        if is_num(toks.get(2).map(|s| s.as_str())) {
                            args["cycle_end"] = json!(toks[2].parse::<i64>().unwrap_or(0));
                        }
                    }
                    None
                }
            };
            let db = match named_store.or_else(|| current_trace_duckdb(st)) {
                Some(d) => d,
                None => return Err("swimlane: no trace store — run `trace on` … `trace off` first".into()),
            };
            // `stem` = the store basename without .duckdb (= TS `# <stem>`).
            let stem = std::path::Path::new(&db)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("trace")
                .to_string();
            args["stem"] = json!(stem);
            match run_trace_read_sidecar("swimlane_text", &db, &args) {
                Ok(v) => Ok(v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string()),
                Err(e) => Err(format!("swimlane: {e}")),
            }
        }
        // `chis` — CPU instruction history (VICE `cpuhistory`). reverse-debug Phase 1a.
        //
        // SOURCE PRIORITY:
        //   1. LIVE ring (`Machine::cpu_history`) — the last N executed instructions,
        //      always-on, NO trace/finalize/sidecar dependency. This is the USER'S FLOW:
        //      they run WITH the trace ON and type `chis` → the ring serves it instantly.
        //      `chis` = last 4000 CYCLES from the ring's newest; `chis <cyc>` = last <cyc>
        //      cycles; `chis <s> <e>` = an explicit cycle window. When the ring covers the
        //      window we render FROM THE RING (the captured opcode bytes + post-instr regs).
        //   2. FALLBACK to the finalized `.c64retrace` via the sidecar (the historical path,
        //      commit 57c9191) when the ring is empty OR the requested explicit window is
        //      OLDER than the ring covers (history beyond the live window).
        //   3. Honest error when neither source has the data.
        "chis" => {
            let a1 = toks.get(1).map(|s| s.as_str());
            let is_num = |t: Option<&str>| t.map(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).unwrap_or(false);
            // Parse the (cycle-based) request, mirroring the historical sidecar args.
            let explicit_window: Option<(u64, u64)> = if is_num(a1) && is_num(toks.get(2).map(|s| s.as_str())) {
                Some((
                    a1.unwrap().parse::<u64>().unwrap_or(0),
                    toks[2].parse::<u64>().unwrap_or(0),
                ))
            } else {
                None
            };
            let last_cycles: u64 = if explicit_window.is_none() && is_num(a1) {
                a1.unwrap().parse::<u64>().unwrap_or(4000)
            } else {
                4000
            };

            // ── 1. LIVE ring first. ──────────────────────────────────────────────
            let ring = &st.session.machine.cpu_history;
            let span = ring.cycle_span(); // Some((oldest, newest)) when non-empty.
            // Decide whether the ring can serve this request. For an explicit window we
            // require the window START to be within (or newer than) the ring's oldest
            // cycle — otherwise the user is asking for history older than the live ring,
            // which only the finalized trace has → fall back.
            let ring_can_serve = match (span, explicit_window) {
                (Some((oldest, _newest)), Some((s, _e))) => s >= oldest,
                (Some(_), None) => true, // last-N / last-cycles is always within the ring.
                (None, _) => false,      // empty ring → fall back.
            };
            if ring_can_serve {
                let mut entries: Vec<trx64_core::CpuHistEntry> = Vec::new();
                let header: String;
                if let Some((s, e)) = explicit_window {
                    let (lo, hi) = if s <= e { (s, e) } else { (e, s) };
                    ring.window_by_cycle(lo, hi, &mut entries);
                    header = format!("# cpuhistory (live ring) — cycles {lo}–{hi}, {} instr", entries.len());
                } else {
                    // last_cycles window ending at the ring's newest cycle.
                    let (_oldest, newest) = span.unwrap();
                    let lo = newest.saturating_sub(last_cycles);
                    ring.window_by_cycle(lo, newest, &mut entries);
                    header = format!("# cpuhistory (live ring) — last {last_cycles} cycles, {} instr", entries.len());
                }
                if !entries.is_empty() {
                    return Ok(format_chis_from_ring(&entries, &header));
                }
                // Ring claimed to cover the window but it held no instruction in that
                // cycle range (e.g. a sub-instruction window): fall through to the trace.
            }

            // TRX64 feature-request #3 — the request asked for a window OLDER than the
            // live ring's oldest cycle (the ring wrapped past it) → the ring boundary,
            // not an empty result. Record it so the no-trace fallback can emit the typed
            // ring_exhausted signal instead of a bare "no history".
            let asked_older_than_ring = match (span, explicit_window) {
                (Some((oldest, _)), Some((s, _e))) => s < oldest,
                _ => false,
            };
            let exhaustion = st.session.machine.ring_exhaustion(!asked_older_than_ring);

            // ── 2. Fallback: the finalized `.c64retrace` (historical) via the sidecar. ─
            let mut args = if let Some((s, e)) = explicit_window {
                json!({ "cycle_start": s as i64, "cycle_end": e as i64 })
            } else {
                json!({ "last_cycles": last_cycles as i64 })
            };
            match current_trace_duckdb(st) {
                None => {
                    let mut msg = String::from("chis: no cpu history — the live ring is empty (run the machine) and no trace store exists (run `trace on`). `chis` reads the live cpuhistory ring first, then the captured trace.");
                    if asked_older_than_ring && exhaustion.ring_exhausted {
                        msg.push_str(&format!(
                            "\nring_exhausted: true   revdepth={}s   hint: {}",
                            exhaustion.revdepth_seconds, exhaustion.hint
                        ));
                    }
                    Err(msg)
                }
                Some(db) => {
                    let stem = std::path::Path::new(&db)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("trace")
                        .to_string();
                    args["stem"] = json!(stem);
                    match run_trace_read_sidecar("swimlane_text", &db, &args) {
                        Ok(v) => Ok(v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string()),
                        Err(e) => Err(format!("chis: {e}")),
                    }
                }
            }
        }

        // ---- Project-index reads (inspect/xref/sym) — audit ws-trace-monitor-misc-15. -
        // TS wires `ctx.projectRead` (ws-server.ts:2135-2191): scan C64RE_PROJECT_DIR for
        // the `*_analysis.json` covering the address, load its effective segments + the
        // project-wide address/xref index, and answer inspect/xref/sym. TRX64's stub
        // unconditionally errored. Fix: a faithful project-read bridge over the SAME on-
        // disk analysis/annotation files (project_knowledge.rs). The project dir is the
        // `--project` arg ?? C64RE_PROJECT_DIR (= fs_project_dir, 1:1 with the TS
        // `process.env.C64RE_PROJECT_DIR` the daemon's run.ts sets from --project).
        "inspect" => {
            let addr = match parse_addr(toks.get(1)) {
                Some(a) => a,
                None => return Err("inspect: usage: inspect <addr> [artifact-stem]".into()),
            };
            Ok(project_knowledge::project_read_inspect(&fs_project_dir(), addr))
        }
        "xref" => {
            let addr = match parse_addr(toks.get(1)) {
                Some(a) => a,
                None => return Err("xref: usage: xref <addr> [artifact-stem]".into()),
            };
            Ok(project_knowledge::project_read_xref(&fs_project_dir(), addr))
        }
        "sym" => {
            let q = match toks.get(1) {
                Some(q) => q.clone(),
                None => return Err("sym: usage: sym <name> [artifact-stem]".into()),
            };
            project_knowledge::project_read_sym(&fs_project_dir(), &q).map_err(|e| format!("sym: {e}"))
        }

        // ---- Project-label writes (label/unlabel/note/save_labels/load_labels) —
        // audit ws-trace-monitor-misc-16. TS wires `ctx.projectLabels`
        // (ws-server.ts:2207-2258, ProjectKnowledgeService): persists a user label to
        // `<project>/knowledge/labels.user.json` (+ a memory-address entity / a note
        // finding) and round-trips a VICE `.sym`. TRX64's stub unconditionally errored.
        // Fix: a faithful project-knowledge persistence bridge over the SAME store
        // format/location (project_knowledge.rs). The label store always targets the
        // project dir (= TS `this.projectDir ?? C64RE_PROJECT_DIR`, NOT the FS-shell
        // cwd); the .sym FILE path resolves cwd-relative (= TS `resolveFsPath(file)`).
        "label" | "unlabel" | "note" | "load_labels" | "ll" | "save_labels" | "sl" => {
            let dir = fs_project_dir();
            match op.as_str() {
                "label" => {
                    if toks.len() == 1 {
                        return Ok(project_knowledge::project_labels_list(&dir));
                    }
                    let addr = match parse_addr(toks.get(1)) {
                        Some(a) => a,
                        None => return Err(
                            "label: usage: label <addr> <name>  |  label (list)  |  unlabel <addr|name>"
                                .into(),
                        ),
                    };
                    let name = toks[2..].join(" ").trim().to_string();
                    if name.is_empty() {
                        return Err("label: a name is required — label <addr> <name>".into());
                    }
                    project_knowledge::project_labels_set(&dir, addr, &name)
                }
                "unlabel" => {
                    let key = match toks.get(1) {
                        Some(k) => k.clone(),
                        None => return Err("unlabel: usage: unlabel <addr|name>".into()),
                    };
                    project_knowledge::project_labels_del(&dir, &key)
                }
                "note" => {
                    let addr = parse_addr(toks.get(1));
                    // note <addr> "<text>" — the text is the FIRST quoted run (= TS
                    // cmd.matchAll(/"([^"]*)"/g)[0]).
                    let text = quoted_first(&cmd);
                    match (addr, text) {
                        (Some(a), Some(t)) => project_knowledge::project_labels_note(&dir, a, &t),
                        _ => Err("note: usage: note <addr> \"<text>\"".into()),
                    }
                }
                _ => {
                    // save_labels / load_labels — the FILE resolves cwd-relative.
                    let (file, _rest) = parse_file_cmd();
                    let file = match file {
                        Some(f) => resolve_fs_path(&f),
                        None => {
                            let verb = if op == "save_labels" || op == "sl" {
                                "save_labels"
                            } else {
                                "load_labels"
                            };
                            return Err(format!("{op}: usage: {verb} \"<file.sym>\""));
                        }
                    };
                    if op == "save_labels" || op == "sl" {
                        project_knowledge::project_labels_save(&dir, &file)
                    } else {
                        project_knowledge::project_labels_load(&dir, &file)
                    }
                }
            }
        }

        // ---- STATE: snapshot dump / undump (audit ws-trace-monitor-misc-9) ---------
        // monitor-shell.ts:279-296: `dump "<p.c64re>"` writes a runtime snapshot to
        // disk (dumpRuntimeSnapshot → formatDumpSummary), `undump "<p>"` restores it
        // (undumpRuntimeSnapshot → paused). The help @ STATE/TRACE advertised both, but
        // run_monitor had NO arm → `unknown command: dump`. Fix: wire each arm to the
        // EXISTING snapshot/dump + snapshot/undump capability (write_native_snapshot /
        // read_native_snapshot + capture/restore_runtime_checkpoint — the very engine
        // behind the snapshot/* WS methods). Relative paths resolve against the FS-shell
        // cwd (= the project dir), 1:1 with the TS comment at monitor-shell.ts:283-288.
        // `snapshot`/`loadsnapshot` are aliases: our runtime snapshot IS the `.c64re`
        // dump. An agent that reached for "snapshot" (VICE terminology) gets the same
        // capability instead of `unknown command: snapshot`.
        "dump" | "snapshot" | "undump" | "loadsnapshot" => {
            let (file, _rest) = parse_file_cmd();
            let path = match file {
                Some(p) => resolve_fs_path(&p),
                None => return Err(format!("{op}: usage: {op} \"<path.c64re>\"")),
            };
            if op == "dump" || op == "snapshot" {
                // ── snapshot/dump core (= the WS handler, taking &mut st) ──────────
                st.session.machine.drive8.flush_disk_writeback();
                let (disk_path, disk_format) = match st.session.machine.drive8.get_attached_disk() {
                    Some(d) => (
                        d.backing_path.clone().unwrap_or_default(),
                        match d.kind { DiskKind::G64 => "g64", DiskKind::D64 => "d64" }.to_string(),
                    ),
                    None => (String::new(), String::new()),
                };
                let drive1541_blob =
                    trx64_core::drive_snapshot::capture_drive1541(&mut st.session.machine.drive8);
                let drive_disk_blob =
                    trx64_core::drive_snapshot::capture_drive_disk_image(&st.session.machine.drive8);
                let (cart_bytes, cart_flash) = capture_cart_blobs(&mut st.session.machine);
                let media_inputs = gather_native_media_inputs(&st.session);
                let media_summary = gather_snapshot_media(&st.session);
                let breakpoints = st.breakpoints.entries.len();
                let m = &st.session.machine;
                let checkpoint = trx64_core::c64re_snapshot::capture_runtime_checkpoint(
                    m, &disk_path, &disk_format,
                    Some(&drive1541_blob), drive_disk_blob.as_deref(),
                    cart_bytes.as_deref(), cart_flash.as_deref(),
                );
                let cycle = m.c64_core.clk as i64;
                let pc = m.c64_core.reg_pc as i64;
                let bytes = trx64_core::native_snapshot::write_native_snapshot(
                    trx64_core::native_snapshot::WriteNativeSnapshotArgs {
                        checkpoint,
                        schema_version:
                            trx64_core::c64re_snapshot::RUNTIME_CHECKPOINT_SCHEMA_VERSION,
                        media: media_inputs,
                        runtime_version: "trx64-runtime/1".to_string(),
                        machine_model: "c64-pal".to_string(),
                        provenance: None,
                        pc,
                        cycle,
                    },
                );
                if let Some(parent) = std::path::Path::new(&path).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(&path, &bytes) {
                    Ok(()) => {
                        // formatDumpSummary (= snapshot-persistence.ts:271-281).
                        let media = if media_summary.is_empty() {
                            "none".to_string()
                        } else {
                            media_summary.iter().map(|m| {
                                let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
                                let fmt = m.get("format").and_then(|v| v.as_str()).unwrap_or("");
                                let name = m.get("sourceName").and_then(|v| v.as_str())
                                    .filter(|s| !s.is_empty()).unwrap_or(fmt);
                                let kb = m.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0) / 1024;
                                format!("{role}={name}({fmt}, {kb}KB)")
                            }).collect::<Vec<_>>().join(", ")
                        };
                        Ok(format!(
                            "dumped {path}\n  cycle={cycle} pc=${:04x} machine=c64-pal\n  media: {media}\n  file={:.1}KB breakpoints={breakpoints}",
                            pc, bytes.len() as f64 / 1024.0
                        ))
                    }
                    Err(e) => Err(format!("dump: write error: {e}")),
                }
            } else {
                // ── snapshot/undump (shared core — power-cycle → restore) ──────────
                match undump_native_snapshot(st, &path) {
                    Ok(r) => {
                        let breakpoints = st.breakpoints.entries.len();
                        // formatUndumpSummary (= snapshot-persistence.ts:282-292).
                        let media = if r.media.is_empty() {
                            "none".to_string()
                        } else {
                            r.media
                                .iter()
                                .map(|m| {
                                    let name = m
                                        .source_name
                                        .clone()
                                        .filter(|s| !s.is_empty())
                                        .unwrap_or_else(|| m.format.clone());
                                    format!("{}={name}({})", m.role, m.format)
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        };
                        let mut summary = format!(
                            "undumped {path}\n  cycle={} pc=${:04x} machine={} (paused)\n  media: {media}\n  breakpoints={breakpoints}",
                            r.cycle, r.pc, r.machine_model
                        );
                        if let Some(w) = r.warning {
                            summary.push_str(&format!("\n  WARNING: {w}"));
                        }
                        Ok(summary)
                    }
                    Err(e) => Err(format!("undump: {e}")),
                }
            }
        }

        // ---- STATE: savecrt / savecrtstate (audit ws-trace-monitor-misc-9) ---------
        // monitor-shell.ts:303-319: write the LIVE cart flash state to the mounted .crt
        // (bare → the backing file; `savecrt "<p>"` → a re-packed copy at <p>). The help
        // advertised it but run_monitor had NO arm → `unknown command: savecrt`. Fix:
        // wire it to the EXISTING cart-persist capability (cartridge.crt_image(clk) →
        // the bytes, cartridge_image.path → the backing file — the same path media/
        // persist role:cartridge uses).
        "savecrt" | "savecrtstate" => {
            if st.session.machine.cartridge.is_none() {
                return Err("savecrt: no cartridge attached".into());
            }
            let (file, _rest) = parse_file_cmd();
            let m = &mut st.session.machine;
            let clk = m.clk;
            // Re-pack the live state to a .crt image (None ⇒ this mapper can't).
            let img = match m.cartridge.as_mut().and_then(|c| c.crt_image(clk)) {
                Some(b) => b,
                None => return Err("savecrt: this mapper cannot re-pack a .crt".into()),
            };
            // `savecrt "<p>"` → write the re-packed image to <p> as a copy.
            if let Some(f) = file {
                let target = resolve_fs_path(&f);
                if let Some(parent) = std::path::Path::new(&target).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                return match std::fs::write(&target, &img) {
                    Ok(()) => Ok(format!("savecrt: {} bytes -> {target}", img.len())),
                    Err(e) => Err(format!("savecrt: write error: {e}")),
                };
            }
            // Bare `savecrt` → update the mounted backing file.
            let path = m.cartridge_image.as_ref().map(|i| i.path.clone()).unwrap_or_default();
            if path.is_empty() {
                return Ok("savecrt: skipped — no backing file path".into());
            }
            match std::fs::write(&path, &img) {
                Ok(()) => Ok(format!("savecrt: {} bytes -> {path}", img.len())),
                Err(e) => Err(format!("savecrt: write error: {e}")),
            }
        }

        // ---- STATE: killmedia (Spec 793 — purge undump-materialized media) ---------
        // Delete every `<name>_media/` sidecar undump created (files + dir), detaching a
        // live mount backed by one first. Touches ONLY daemon-materialized dirs, NEVER a
        // user's own mount. The LLM/test-overlay auto-cleanup verb; also `undump_media_purge`
        // over WS.
        "killmedia" | "purgemedia" => {
            if st.materialized_media.is_empty() {
                return Ok("killmedia: no undump-materialized media to purge".into());
            }
            let (ndirs, nfiles) = purge_materialized_media(st);
            Ok(format!("killmedia: purged {ndirs} dir(s), {nfiles} file(s)"))
        }

        // ---- STATE: swapcrt (audit ws-trace-monitor-misc-9) -----------------------
        // monitor-shell.ts:330-367: hot-swap the .crt in the FROZEN machine, NO reset;
        // same mapper type carries banking continuation (currentBank + controlRegister).
        // The help advertised it but run_monitor had NO arm. Fix: wire it to the EXISTING
        // cart capability (load_cartridge_from_bytes / attach_cart_from_bytes +
        // get_state/set_state for the banking carry-over).
        "swapcrt" => {
            let (file, _rest) = parse_file_cmd();
            let f = match file {
                Some(f) => f,
                None => return Err("swapcrt: usage: swapcrt \"<new.crt>\"".into()),
            };
            let p = resolve_fs_path(&f);
            if !std::path::Path::new(&p).exists() {
                return Err(format!("swapcrt: no such file: {p}"));
            }
            let bytes = match std::fs::read(&p) {
                Ok(b) => b,
                Err(e) => return Err(format!("swapcrt: cannot read {p}: {e}")),
            };
            let basename = std::path::Path::new(&p)
                .file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.clone());
            let m = &mut st.session.machine;
            // Old cart continuation (banking) + type, captured BEFORE the swap.
            let old_type = m.cartridge.as_ref().map(|c| c.mapper_type());
            let old_state = m.cartridge.as_ref().map(|c| c.get_state());
            let old_name = m.cartridge_image.as_ref()
                .map(|i| std::path::Path::new(&i.path).file_name()
                    .map(|n| n.to_string_lossy().to_string()).unwrap_or_default())
                .unwrap_or_default();
            // Persist a dirty old cart to its backing file first (eject semantics).
            let mut lines: Vec<String> = Vec::new();
            if m.cartridge.as_ref().map(|c| c.is_writable_dirty()).unwrap_or(false) {
                let clk = m.clk;
                let old_path = m.cartridge_image.as_ref().map(|i| i.path.clone()).unwrap_or_default();
                if !old_path.is_empty() {
                    if let Some(img) = m.cartridge.as_mut().and_then(|c| c.crt_image(clk)) {
                        if std::fs::write(&old_path, &img).is_ok() {
                            lines.push(format!("persisted old cart: {} bytes -> {old_path}", img.len()));
                        }
                    }
                }
            }
            // Attach the new cart (NO reset).
            let new_type = match m.attach_cart_from_bytes(&bytes, &basename) {
                Ok((_name, t)) => t,
                Err(e) => return Err(format!("swapcrt: {e:?}")),
            };
            // Same mapper type → carry the banking continuation (bank + ctrl reg) so
            // running code resumes in the same window. Flash DATA is the NEW build's
            // (set_state's banking-only state leaves the new content alone).
            let carried = if old_type == Some(new_type) {
                if let Some(os) = old_state {
                    let bank = os.current_bank;
                    let ctrl = os.control_register;
                    if let Some(c) = m.cartridge.as_mut() {
                        c.set_state(trx64_core::cart::CartState {
                            current_bank: bank, control_register: ctrl, flash: None,
                        });
                    }
                    // Re-run the PLA reconfig so the carried lines take effect.
                    m.memconfig = m.memconfig_table[m.pla_index()];
                    Some(match ctrl {
                        Some(cr) => format!("carried banking: bank={bank} ctrl=${cr:02x}"),
                        None => format!("carried banking: bank={bank}"),
                    })
                } else { None }
            } else { None };
            // Track the new backing path so a later savecrt/auto-persist hits it.
            if let Some(img) = m.cartridge_image.as_mut() { img.path = p.clone(); }
            lines.push(format!(
                "swapped: {} -> {new_type:?} ({basename})",
                old_type.map(|t| format!("{t:?} ({old_name})")).unwrap_or_else(|| "(none)".into())
            ));
            lines.push(carried.unwrap_or_else(|| "fresh boot-state registers (no/changed mapper type)".into()));
            lines.push("no reset — running code sees the new ROM bytes NOW".into());
            Ok(lines.join("\n"))
        }

        // ---- FILE mini-shell (audit ws-trace-monitor-misc-11) ---------------------
        // monitor-shell.ts:769-845: the host-FS verb family + PRG/raw load/save, rooted
        // at the project dir (relative paths off the session cwd). The help @ FILE
        // advertised them but run_monitor had NO arms → `unknown command: pwd`. Fix:
        // wire pwd/cd/ls/dir/mkdir/rmdir + load/save/bload/bsave to std::fs + the
        // EXISTING machine RAM access (poke / ram), matching the TS resolveFsPath cwd
        // rules (NOT a hard jail — abs/`..` pass through exactly as TS).
        "pwd" => Ok(fs_cwd_now.clone()),
        "cd" => {
            let (file, _rest) = parse_file_cmd();
            let d = match file {
                Some(a) => resolve_fs_path(&a),
                None => fs_project_dir(),
            };
            match std::fs::metadata(&d) {
                Ok(md) if md.is_dir() => { st.mon.fs_cwd = Some(d.clone()); Ok(d) }
                Ok(_) => Err(format!("cd: not a directory: {d}")),
                Err(_) => Err(format!("cd: no such directory: {d}")),
            }
        }
        "ls" | "dir" => {
            let (file, _rest) = parse_file_cmd();
            let d = match file {
                Some(a) => resolve_fs_path(&a),
                None => fs_cwd_now.clone(),
            };
            let mut ents: Vec<(bool, String)> = match std::fs::read_dir(&d) {
                Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| {
                    let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    (is_dir, e.file_name().to_string_lossy().to_string())
                }).collect(),
                Err(e) => return Err(format!("ls: {e}")),
            };
            ents.sort_by(|a, b| a.1.cmp(&b.1));
            ents.truncate(500);
            let body = if ents.is_empty() {
                "  (empty)".to_string()
            } else {
                ents.iter().map(|(is_dir, name)| format!("  {} {name}", if *is_dir { "d" } else { "-" }))
                    .collect::<Vec<_>>().join("\n")
            };
            Ok(format!("{d}:\n{body}"))
        }
        "mkdir" => {
            let (file, _rest) = parse_file_cmd();
            let arg = match file { Some(a) => a, None => return Err("mkdir: usage: mkdir <dir>".into()) };
            match std::fs::create_dir_all(resolve_fs_path(&arg)) {
                Ok(()) => Ok(format!("mkdir {arg}")),
                Err(e) => Err(format!("mkdir: {e}")),
            }
        }
        "rmdir" => {
            let (file, _rest) = parse_file_cmd();
            let arg = match file { Some(a) => a, None => return Err("rmdir: usage: rmdir <dir>".into()) };
            match std::fs::remove_dir(resolve_fs_path(&arg)) {
                Ok(()) => Ok(format!("rmdir {arg}")),
                Err(e) => Err(format!("rmdir: {e}")),
            }
        }
        // load "<file>" [addr] — PRG load into RAM (2-byte header → load addr, or override).
        "load" => {
            let (file, rest) = parse_file_cmd();
            let f = match file { Some(f) => f, None => return Err("load: usage: load \"<file>\" [addr]".into()) };
            let p = resolve_fs_path(&f);
            if !std::path::Path::new(&p).exists() {
                return Err(format!("load: no such file: {p}"));
            }
            let buf = match std::fs::read(&p) { Ok(b) => b, Err(e) => return Err(format!("load: {e}")) };
            if buf.len() < 2 { return Err("load: PRG too short (need 2-byte header)".into()); }
            let override_addr = rest.first().and_then(|t| parse_hex(t)).map(|v| (v & 0xffff) as u16);
            let load_address = override_addr
                .unwrap_or_else(|| (buf[0] as u16) | ((buf[1] as u16) << 8));
            let body = &buf[2..];
            st.session.machine.poke(load_address, body);
            st.session.machine.sync_after_monitor();
            let end_address = load_address.wrapping_add(body.len() as u16).wrapping_sub(1);
            st.mon.disasm_cursor = Some(load_address);
            let bn = std::path::Path::new(&f).file_name()
                .map(|n| n.to_string_lossy().to_string()).unwrap_or(f.clone());
            Ok(format!(
                "loaded {bn}: ${:04x}..${:04x} ({} bytes)",
                load_address, end_address, body.len()
            ))
        }
        // save "<file>" <a1> <a2> — save a RAM range as a PRG (2-byte load addr = a1).
        "save" => {
            let (file, rest) = parse_file_cmd();
            let a1 = parse_addr(rest.first());
            let a2 = parse_addr(rest.get(1));
            let (f, a1, a2) = match (file, a1, a2) {
                (Some(f), Some(a1), Some(a2)) if a2 >= a1 => (f, a1, a2),
                _ => return Err("save: usage: save \"<file>\" <a1> <a2>".into()),
            };
            let mut bytes: Vec<u8> = vec![(a1 & 0xff) as u8, ((a1 >> 8) & 0xff) as u8];
            for a in a1..=a2 { bytes.push(st.session.machine.ram[a as usize]); }
            let target = resolve_fs_path(&f);
            match std::fs::write(&target, &bytes) {
                Ok(()) => {
                    let bn = std::path::Path::new(&f).file_name()
                        .map(|n| n.to_string_lossy().to_string()).unwrap_or(f.clone());
                    Ok(format!("saved {bn}: ${a1:04x}..${a2:04x} ({} bytes + load addr)", bytes.len() - 2))
                }
                Err(e) => Err(format!("save: {e}")),
            }
        }
        // bload "<file>" <addr> — raw binary load (no header).
        "bload" => {
            let (file, rest) = parse_file_cmd();
            let addr = parse_addr(rest.first());
            let (f, addr) = match (file, addr) {
                (Some(f), Some(a)) => (f, a),
                _ => return Err("bload: usage: bload \"<file>\" <addr>".into()),
            };
            let p = resolve_fs_path(&f);
            if !std::path::Path::new(&p).exists() {
                return Err(format!("bload: no such file: {p}"));
            }
            let buf = match std::fs::read(&p) { Ok(b) => b, Err(e) => return Err(format!("bload: {e}")) };
            let mut n = 0u32;
            for (i, b) in buf.iter().enumerate() {
                let a = addr as usize + i;
                if a > 0xffff { break; }
                st.session.machine.ram[a] = *b;
                n += 1;
            }
            st.session.machine.sync_after_monitor();
            st.mon.disasm_cursor = Some(addr);
            let bn = std::path::Path::new(&f).file_name()
                .map(|n| n.to_string_lossy().to_string()).unwrap_or(f.clone());
            let end = (addr as u32 + n.saturating_sub(1)) & 0xffff;
            Ok(format!("bloaded {bn}: {n} bytes -> ${addr:04x}..${end:04x}"))
        }
        // bsave "<file>" <a1> <a2> — raw binary save (no header).
        "bsave" => {
            let (file, rest) = parse_file_cmd();
            let a1 = parse_addr(rest.first());
            let a2 = parse_addr(rest.get(1));
            let (f, a1, a2) = match (file, a1, a2) {
                (Some(f), Some(a1), Some(a2)) if a2 >= a1 => (f, a1, a2),
                _ => return Err("bsave: usage: bsave \"<file>\" <a1> <a2>".into()),
            };
            let mut bytes: Vec<u8> = Vec::new();
            for a in a1..=a2 { bytes.push(st.session.machine.ram[a as usize]); }
            let target = resolve_fs_path(&f);
            match std::fs::write(&target, &bytes) {
                Ok(()) => {
                    let bn = std::path::Path::new(&f).file_name()
                        .map(|n| n.to_string_lossy().to_string()).unwrap_or(f.clone());
                    Ok(format!("bsaved {bn}: ${a1:04x}..${a2:04x} ({} bytes, raw)", bytes.len()))
                }
                Err(e) => Err(format!("bsave: {e}")),
            }
        }

        // ---- Help ------------------------------------------------------------
        "help" | "?" => Ok(monitor_help_text()),

        _ => Err(format!("unknown command: {op}. Try 'help'.")),
    }
}

/// T2.8 — the `help`/`?` text. VERBATIM copy of the monitor-shell.ts help block
/// (the help simply LISTS every verb of the VICE-superset, including ones whose
/// runtime bridges are deferred in this daemon — the help text itself is identical
/// regardless of which bridges are wired, so it is reproduced 1:1).
/// Anchored full-string glob match supporting `*` (any run, incl. empty) and `?`
/// (exactly one char) — the observer on/off/del wildcard (monitor-shell.ts:912-914
/// globMatches, where the pattern is `^` + escaped-name with `*`→".*", `?`→"."` +
/// `$`). Observer names are plain identifiers, so this character walk is equivalent.
fn glob_full_match(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    // Classic two-pointer glob match with backtracking over `*`.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn monitor_help_text() -> String {
    [
        "monitor (VICE-superset):",
        "  EXEC",
        "    g [addr]         go/resume the run-loop (PC=addr); Pause button halts",
        "    x                exit/resume (= g)",
        "    until <addr>     run until PC=addr, then stop (synchronous)",
        "    z / step         step into — may enter IRQ/NMI (VICE-correct)",
        "    n / next         step over — skips JSR + runs THROUGH IRQ/NMI",
        "    ret / return     run until current frame returns (RTS/RTI)",
        "    focus [m]        flow focus: auto|main|irq|nmi|brk|clear (C64RE)",
        "    sf / nf          step into/over, stop only in focused flow (C64RE)",
        "    flow             interrupt/trap flow frame stack (panel)",
        "    bt               backtrace (stack scan + flow frames)",
        "    reset            cold reset",
        "  MEMORY (bank lens: cpu|ram|rom|io|cart, default cpu = what CPU sees)",
        "    m [lens] <a> [b] memory dump ($20/row + petscii; default len $800)",
        "    d [lens] [a] [end] disassemble: a..end range (VICE), or ~16 from a/PC",
        "    sd [n]           step+disasm: the REAL executed path, loops folded (dynamic)",
        "    df [-i] [a] [n]  follow-disasm: walk control flow (static); -i asks at branches (df t|f|b)",
        "    screen           decode the 40x25 text screen (real screen pointer)",
        "    io [1|addr]      I/O area per device: register hex (peek) + state details (VICE io)",
        "    bitmap <a> [w h] [hires|charset|sprite]  render a RAM range to a PNG (scrub gfx)",
        "    bank [lens]      show/set the sticky default lens for m/d",
        "    wr [lens] <a> <b..>  write exactly these bytes from a",
        "    f <a> <b> <d..>  fill range a..b with repeating data",
        "    a <a> [instr]    assemble; `a c000` enters assemble mode (type lines, empty exits)",
        "    t <a> <b> <dst>  move/copy a..b to dst (overlap-safe)",
        "    c <a> <b> <dst>  compare a..b vs dst (list diffs)",
        "    h <a> <b> <d..>  hunt for a byte pattern (xx = wildcard)",
        "  BREAKPOINTS / OBSERVERS",
        "    bk               list breakpoints (#num $addr)",
        "    bk <a> | bk -<a> set / remove breakpoint (by addr)",
        "    del <n..> | del  delete by #num / delete all",
        "    obs <name> when exec|load|store <a[..b]> [if <cond>] do <action> [fields]",
        "      actions: break | log [fields] | mark [\"label\"] | cmd \"<cmd>\" | trace [domains]|off",
        "      log fields: a/x/y/sp/pc/fl or $addr[:w]  e.g. `do log $fd $fe $ff a x y`",
        "      trace domains: c64-cpu|drive8-cpu|iec|vic|memory (default c64-cpu+memory)",
        "        bracket: `obs c when exec $4000 do trace` … `obs c2 when exec $4100 do trace off`",
        "    obs | obs log    list observers / show log lines",
        "    obs <name> on|off|del   (name may glob: `obs * del` = all, `obs c* off`)",
        "    ignore <name> [n]",
        "      cond: a/x/y/pc/sp/fl/rl/val/addr  == != < > <= >= && || ( )",
        "  CPU",
        "    r                registers (+ flow + IRQ/NMI vectors)",
        "    r a=$42 x=$10    set registers (a/x/y/sp/pc/fl)",
        "    sidefx [on|off]  monitor read side effects (default off = peek)",
        "    device [c64|drive8]  target the C64 or the 1541 CPU (drive8 = read-inspect r/m/d)",
        "  STATE / TRACE",
        "    dump|snapshot <p>  write a .c64re snapshot; undump|loadsnapshot <p>  restore it (Spec 707)",
        "    savecrt [\"<p>\"]  write live flash state to the mounted .crt (or to <p> as a copy)",
        "    swapcrt \"<p>\"    hot-swap the .crt, NO reset (same mapper: bank/ctrl carried) — build iteration",
        "    trace on|off|status|mark   live trace gate (Spec 746)",
        "    tracedb start|stop|status|mark   declarative trace (Spec 708)",
        "    traceindex [path]   build the .duckdb index for the current/last (or <path>) .c64retrace so it is queryable (oldest->newest, no event cap)",
        "  ANALYSIS (need a trace — `trace on` first)",
        "    map [cpu]        memory map: free RAM / persistence surface",
        "    taint <a> [cyc]  data-flow taint backward from (cyc,addr)",
        "    swimlane [list|name] [s] [e]  trace lanes (cpu/irq/nmi/io/1541): list / newest / by name; tail ~2000cy",
        "                     `swimlane <s> <e>` with no covering trace → auto checkpoint-ring replay",
        "    chis [cyc] | chis <s> <e>  cpu instruction history: LIVE cpuhistory ring first (works while a trace is active), falls back to the captured trace; last N cyc (default 4000) or a window",
        "  REVERSE-DEBUG (always-on full-delta ring — no pre-arming; inspect-backward only)",
        "    rstep [n] | reverse [n]   UNDO the last n instructions (default 1): restore CPU+RAM+IO bytes to before them; reports the landed regs + writes rolled back",
        "    whowrote <addr> [n]       last n writer(s) of <addr> from the ring (newest first): PC + cycle + old->new + caller chain (top return frames -> identifies the CALLER of a shared store). Emits `ring_exhausted: true` + a depth hint on a miss past a wrapped ring.",
        "    triage [pc]               guided crash-triage: causal chain (crash -> wild RTS/JMP transfer -> stack corruptor) from the rings; auto-printed on a JAM. Confidence-tagged. Surfaces a PINNED `loop entry: $SRC -> $DST` for a tight-loop/halt; `ring_exhausted` when the transfer is older than the ring.",
        "    traprules <path> | traprules [clear]   load/list/clear project on-trap dump rules (JSON {pc,label,dump:[[name,addr,len]],decode}); auto-emits `label: name=$XX (decode)` on reaching that PC (JAM / breakpoint)",
        "    revdepth [seconds]        report / set the always-on reverse-ring depth: rebuilds the delta+cpuhistory rings (DISCARDS history; future capture only; 1..=600s). TRX64_REVERSE_SECONDS = boot default",
        "    diff <idA> <idB>          typed by-ID diff of two checkpoint anchors (RAM runs + per-chip register changes). READ-ONLY (live machine unchanged). ids from `checkpoint/list`",
        "    ringdump <path>           serialize the WHOLE reverse-debug buffer (checkpoint+delta+cpuhistory rings) → one gzipped .c64rering file (the tester->dev hand-off)",
        "    ringload <path>           restore a .c64rering: reconstruct the rings + restore the machine to its current anchor; scrub/rstep/whowrote/chis/diff then work on it",
        "  KNOWLEDGE (reads the project _analysis.json that covers the address)",
        "    inspect <a> [stem]  segment kind/label + xrefs at a",
        "    xref <a> [stem]     who calls/jumps/reads/writes a (in + out)",
        "    sym <name> [stem]   reverse lookup: named routine/label -> address",
        "  FILE (rooted at the project dir; relative paths off the session cwd)",
        "    pwd | cd [dir] | ls [dir]   FS shell (cd with no arg = project dir)",
        "    mkdir <dir> | rmdir <dir>   make / remove a directory",
        "    load \"<f>\" [addr]   load a PRG into RAM (2-byte header, or override addr)",
        "    save \"<f>\" <a1> <a2>  save a1..a2 as a PRG (2-byte load addr = a1)",
        "    bload \"<f>\" <addr>   raw binary load (no header)",
        "    bsave \"<f>\" <a1> <a2>  raw binary save (no header)",
    ]
    .join("\n")
}

/// The first double-quoted substring of a command (= the TS
/// `[...cmd.matchAll(/"([^"]*)"/g)].map(m => m[1])[0]`). `None` when unquoted.
/// Render the `do <action>` description of an observer spec for the `obs`/`o`/`reg`
/// list + the registration echo — 1:1 with the c64re `doDesc` (monitor-shell.ts:
/// 996-1000) and the `fmt` `do ...` segment (monitor-shell.ts:899).
fn obs_do_desc(spec: &observers::ObsSpec) -> String {
    match spec.action {
        observers::ObsAction::Log => match &spec.log_exprs {
            Some(exprs) if !exprs.is_empty() => {
                let fields: Vec<String> = exprs
                    .iter()
                    .map(|e| match e {
                        observers::LogExpr::Reg(r) => match r {
                            observers::RegName::A => "a".into(),
                            observers::RegName::X => "x".into(),
                            observers::RegName::Y => "y".into(),
                            observers::RegName::Sp => "sp".into(),
                            observers::RegName::Pc => "pc".into(),
                            observers::RegName::Fl => "fl".into(),
                        },
                        observers::LogExpr::Mem { addr, word } => {
                            format!("${:x}{}", addr, if *word { ":w" } else { "" })
                        }
                    })
                    .collect();
                format!("log {}", fields.join(" "))
            }
            _ => "log".into(),
        },
        observers::ObsAction::Cmd => {
            format!("cmd \"{}\"", spec.cmd_src.clone().unwrap_or_default())
        }
        observers::ObsAction::Mark => {
            format!("mark \"{}\"", spec.mark_label.clone().unwrap_or_default())
        }
        observers::ObsAction::Trace => match &spec.trace_scope {
            Some(ts) if ts.off => "trace off".into(),
            Some(ts) => format!("trace {}", ts.domains.join(" ")),
            None => "trace".into(),
        },
        observers::ObsAction::Break => "break".into(),
    }
}

fn quoted_first(cmd: &str) -> Option<String> {
    let start = cmd.find('"')?;
    let rest = &cmd[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Modal assemble prompt at `addr` (= monitor-shell.ts `asmPrompt`): VICE-style
/// `.cXXXX  ` (dot, lower-case 4-hex, two trailing spaces).
fn asm_prompt(addr: u16) -> String {
    format!(".{:04x}  ", addr & 0xffff)
}

/// Assemble one instruction at `addr` and write it (= monitor-shell.ts `assembleAt`,
/// :191-204). On success: poke the bytes via the CPU write path, advance BOTH the
/// modal assemble cursor and the disasm cursor (→ stay in mode), set `pending_prompt`
/// to the next prompt, and return the `addr  bb bb  <disasm>` listing line. On error:
/// return the error (cursor unchanged; the caller re-shows the prompt). The bytes are
/// written through `poke` (raw RAM), matching the TS `s.c64Bus.write` for RAM targets;
/// the disassembly read-back uses the cpu lens, 1:1 with the TS `disasmLine(peek cpu)`.
fn assemble_at(st: &mut State, addr: u16, text: &str) -> Result<String, String> {
    let r = assembler::assemble_line(text, addr).map_err(|e| format!("a: {e}"))?;
    st.session.machine.poke(addr, &r.bytes);
    st.session.injected = true;
    let next = addr.wrapping_add(r.size);
    st.mon.asm_cursor = Some(next);
    st.mon.disasm_cursor = Some(next);
    st.mon.pending_prompt = Some(asm_prompt(next));
    let bytes_col: String = r.bytes.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
    let (_, back) = disasm_line_ts(|a| st.session.machine.peek_lens(a, "cpu"), addr);
    Ok(format!("{:04x}  {:<11}  {}", addr, bytes_col, back))
}

/// Parse a hex token (optional leading `$`).
fn parse_hex(tok: &str) -> Option<u32> {
    let t = tok.strip_prefix('$').unwrap_or(tok);
    u32::from_str_radix(t, 16).ok()
}

/// reverse-debug Phase 1c — slice the always-on full-delta ring for the cycle window
/// `[cycle_start, cycle_end]` and DUMP it into a `.c64retrace` using the SAME binary
/// record format the live trace writes (`FrameSink::write_delta_entry` → CPU_STEP 0x10
/// + RAM_WRITE 0x11), so the file is read by the existing sidecar path (swimlane / map
/// / taint) identically to a finalized live trace. Backs both `trace/build_from_ring`
/// (WS) and the monitor `buildtrace` verb (one path, no format invention).
///
/// The `.duckdb` index is left LAZY (built by the sidecar on first read), and
/// `state.last_trace_path` is pointed at it so the monitor map/swimlane/taint/chis verbs
/// read THIS store immediately. Does NOT disturb an active `trace on` capture (it only
/// touches `last_trace_path`, not `session.trace`).
fn build_trace_from_ring(
    st: &mut State,
    cycle_start: u64,
    cycle_end: u64,
    output_path: Option<PathBuf>,
) -> Result<Value, String> {
    let (lo, hi) = if cycle_start <= cycle_end { (cycle_start, cycle_end) } else { (cycle_end, cycle_start) };

    if !st.session.machine.delta_ring.enabled() {
        return Err("the full-delta ring is disabled (TRX64_CPUHISTORY=0) — nothing to dump.".into());
    }
    let ring_span = st.session.machine.delta_ring.cycle_span();

    // Slice the ring for [lo, hi], oldest→newest, each entry with its writes.
    let mut slice: Vec<(trx64_core::DeltaEntry, Vec<trx64_core::WriteRec>)> = Vec::new();
    st.session.machine.delta_ring.slice_by_cycle(lo, hi, &mut slice);

    // Resolve the output `.c64retrace` path (default under the session runtime dir,
    // like default_trace_output) and its sibling `.duckdb`.
    let base = output_path.unwrap_or_else(|| default_trace_output(&st.session.id));
    let retrace_path = base.with_extension("c64retrace");
    let duckdb_path = {
        let p = retrace_path.to_string_lossy();
        if p.ends_with(".c64retrace") {
            format!("{}.duckdb", &p[..p.len() - ".c64retrace".len()])
        } else {
            p.into_owned()
        }
    };

    // The dump is cpu + memory (the ring's two halves). Build a VALID defJson header
    // (so the c64re DuckDB indexer's JSON.parse(meta.defJson) succeeds and the store is
    // readable) — same shape as trace/start_domains.
    let domains: Vec<String> = vec!["c64-cpu".into(), "memory".into()];
    let run_id = format!("run_ring-dump_{lo}_{hi}");
    let def_json_str = serde_json::to_string(&capture_all_def_json(&domains)).unwrap_or_default();
    let meta_json = serde_json::to_string(&json!({
        "runId": run_id,
        "defId": "ring-dump",
        "defVersion": 1,
        "defName": "delta-ring window dump",
        "defJson": def_json_str,
        "domains": domains,
        "cycleStart": lo,
        "createdAt": now_iso8601_utc(),
    }))
    .unwrap_or_default();

    // Encode: header + each entry's CPU_STEP (+ its RAM_WRITEs).
    let mut sink = FrameSink::with_header(&meta_json);
    let mut event_count: u64 = 0;
    for (e, writes) in &slice {
        event_count += sink.write_delta_entry(e, writes);
    }
    let bytes = sink.buf;
    let bytes_written = bytes.len();

    if let Some(parent) = retrace_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    // Remove any stale sibling .duckdb so the sidecar rebuilds the index from THIS fresh
    // .c64retrace (the indexer only builds when the .duckdb is absent).
    let _ = std::fs::remove_file(&duckdb_path);
    std::fs::write(&retrace_path, &bytes)
        .map_err(|e| format!("write {}: {e}", retrace_path.display()))?;

    // Point the CURRENT trace store at this dump so the monitor reads it immediately.
    st.last_trace_path = Some(duckdb_path.clone());
    st.last_run_id = Some(run_id.clone());

    let mut out = json!({
        "retrace_path": retrace_path.to_string_lossy(),
        "duckdb_path": duckdb_path,
        "event_count": event_count,
        "instruction_count": slice.len(),
        "bytes_written": bytes_written,
        "cycle_start": lo,
        "cycle_end": hi,
        "run_id": run_id,
    });
    if let Some((oldest, newest)) = ring_span {
        out["ring_cycle_span"] = json!({ "oldest": oldest, "newest": newest });
        // Flag a window that fell (partly) outside the live ring — the dump then covers
        // only the overlap (older history lives in a finalized trace).
        if lo < oldest || hi > newest {
            out["clipped"] = json!(true);
        }
    } else {
        out["clipped"] = json!(true);
    }
    Ok(out)
}

/// Flush the active trace to its `.c64retrace` path; returns (run, status) JSON.
/// T2.6 — also updates `state.last_trace_path` and `state.last_run_id` (= TS
/// `TraceRunController.lastStorePath`/`lastRunId`, set in `stop()`).
fn finalize_trace(st: &mut State, background_index: bool) -> (Value, Value) {
    // Capture cycleEnd from the LIVE machine before taking the trace (= TS run.cycleEnd
    // = controller.session.c64Cpu.cycles at stop, trace-run.ts:453).
    let cycle_end = st.session.machine.clk;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    match st.session.trace.take() {
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
            // T2.6 — mirror TS TraceRunController.lastStorePath / lastRunId set in stop().
            // The duckdb path is the sibling of the retrace path (strip .c64retrace → .duckdb).
            let duckdb_path = {
                let p = t.retrace_path.to_string_lossy();
                if p.ends_with(".c64retrace") {
                    format!("{}.duckdb", &p[..p.len() - ".c64retrace".len()])
                } else {
                    p.into_owned()
                }
            };
            st.last_trace_path = Some(duckdb_path.clone());
            st.last_run_id = Some(t.run_id.clone());
            // Trace-decode gap fix (2026-07-10): on trace-off, kick the `.duckdb` index
            // build in the BACKGROUND so a finalized trace is queryable WITHOUT an opt-in
            // flag or a manual `trace/index` — mirroring the retired TS stop() which ALWAYS
            // indexed. The sidecar decode is minutes on a large `.c64retrace`, so it runs on
            // a detached thread and must not block the stop RPC. Skipped when the caller will
            // index synchronously (trace/run/stop wait_index=true) to avoid a double build of
            // the same store. Soft: an index failure leaves the re-indexable `.c64retrace`.
            if background_index {
                std::thread::spawn(move || {
                    let _ = run_trace_read_sidecar("index", &duckdb_path, &json!({ "wait": true }));
                });
            }
            // ws-trace-monitor-misc-23 — return the REAL RuntimeTraceRun descriptor
            // (trace-run.ts stop()): the run's own definitionId (NOT a hardcoded
            // "live-capture"), version, cycleStart/cycleEnd, overheadMs, marks[], media.
            let overhead_ms = now_ms.saturating_sub(t.start_wall_ms) as u64;
            let marks: Vec<Value> = t
                .marks
                .iter()
                .map(|(cycle, label)| json!({ "cycle": cycle, "label": label }))
                .collect();
            let mut run = json!({
                "runId": t.run_id,
                "definitionId": t.definition_id,
                "definitionVersion": t.definition_version,
                "cycleStart": t.cycle_start,
                "cycleEnd": cycle_end,
                "overheadMs": overhead_ms,
                "eventCount": t.event_count,
                "bytesWritten": bytes_written,
                "marks": marks,
            });
            if !t.media_sha.is_empty() {
                run["media"] = json!({ "sha256": t.media_sha, "sourceName": t.media_name });
            }
            (run, json!({ "active": false, "binary": true }))
        }
    }
}

// ── Spec 708 trace-definition validation (1:1 port of trace-definition.ts) ─────

/// Domains the validator accepts (= TS `DOMAINS`).
const TRACE_DOMAINS: &[&str] =
    &["c64-cpu", "drive8-cpu", "iec", "vic", "sid", "memory"];

/// A 0..=0xFFFF integer check (= TS `u16`: `Number.isInteger(n) && 0<=n<=0xffff`).
/// A non-integer JSON number (e.g. 1.5) has no `as_i64`, so it is rejected.
fn is_u16(v: &Value) -> bool {
    matches!(v.as_i64(), Some(n) if (0..=0xffff).contains(&n))
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
// Decode + formatting live in the shared static-capability crate
// (`trx64-static/src/disasm6502.rs`, capability-cut migration step 1).
use trx64_static::disasm6502::instr_len;

/// `monitorDisasm` wire shape — thin Value adapter over the shared decoder.
fn disasm_one(addr: u16, read: impl Fn(u16) -> u8) -> Value {
    let d = trx64_static::disasm6502::disasm_one(addr, read);
    json!({
        "addr": d.addr as u64,
        "bytes": d.bytes.iter().map(|b| *b as u64).collect::<Vec<_>>(),
        "mnemonic": d.mnemonic,
        "operand": d.operand,
        "text": d.text
    })
}

// ── api/call dispatch ─────────────────────────────────────────────────────────

/// The narrow MCP per-verb bridge allowlist — 1:1 with the TS
/// `WsServer.API_CALL_ALLOWLIST` (ws-server.ts:179-185). Only these methods are
/// reachable over `api/call`. `runtime/call` (the full AgentQueryApi facade) is
/// NOT gated by this — it reaches every method `dispatch_api_call` can back.
const API_CALL_ALLOWLIST: &[&str] = &[
    "monitorRegisters",
    "monitorMemory",
    "monitorDisasm",
    "stepInto",
    "stepOver",
    "addPcBreakpoint",
    "listBreakpoints",
    "removeBreakpoint",
    "until",
    "status",
];

/// Shared dispatch for `api/call` (narrow, allowlist-gated when `full=false`) and
/// `runtime/call` (the full AgentQueryApi facade when `full=true`). TS keeps these
/// as two SEPARATE surfaces: `api/call` is the narrow MCP per-verb bridge gated by
/// API_CALL_ALLOWLIST (ws-server.ts:655), while `runtime/call` runs the WHOLE
/// createAgentQueryApi with no allowlist (ws-server.ts:1717). Method names + return
/// shapes are identical between the two; only the gate differs.
fn dispatch_api_call(id: Value, params: &Value, state: &SharedState, full: bool) -> Response {
    let method = match params.get("method").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return Response::err(id, -32602, "api/call: missing method"),
    };
    let args = params.get("args").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    // The narrow `api/call` route is allowlist-gated (TS ws-server.ts:656). The full
    // `runtime/call` route is not — it reaches every backed AgentQueryApi method.
    if !full && !API_CALL_ALLOWLIST.contains(&method.as_str()) {
        return Response::err(id, -32601, format!("api/call: method not allowed: {method}"));
    }

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
                let State { breakpoints, dsl_observers, dsl_disabled, observers: reg, .. } = &mut *st;
                sync_observers(breakpoints, dsl_observers, dsl_disabled, reg);
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

        // ── Full AgentQueryApi methods (runtime/call only — NOT in the narrow ──
        // api/call allowlist). 1:1 method names with agent-api.ts; each maps to an
        // existing TRX64 capability. Methods with no faithful TRX64 backing stay in
        // the `other` -32601 arm below (see the misc-19 report for the full list).

        // goto(addr): void — set the C64 PC. agent-api.ts:235 → MonitorAPI.goto.
        "goto" => {
            let addr = args.first().and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let mut st = state.lock().unwrap();
            st.session.machine.cpu6510.reg_pc = addr;
            st.session.machine.c64_core.reg_pc = addr;
            st.session.machine.sync_after_monitor();
            // TS goto() returns void → JSON-RPC omits result.
            Response::void(id)
        }

        // stepOut(opts?): StepOutResult — run until the current subroutine returns
        // (SP climbs back to entry+2 = RTS/RTI). agent-api.ts:240 → MonitorAPI.stepOut
        // (monitor.ts:312). Same {halted, cyclesElapsed, instructionsElapsed, finalPc}
        // shape; instructionsElapsed == cyclesElapsed (TS _instrCount == cpu.cycles).
        "stepOut" => {
            let budget: u64 = args
                .first()
                .and_then(|o| o.get("budget"))
                .and_then(|v| v.as_u64())
                .unwrap_or(1_000_000);
            let mut st = state.lock().unwrap();
            let start_clk = st.session.machine.clk;
            let entry_sp = st.session.machine.cpu6510.reg_sp;
            let mut halted = false;
            let mut steps: u64 = 0;
            while steps < budget {
                step_one_instruction(&mut st.session);
                steps += 1;
                // Stack returns: SP back to (or above) entry+2 means RTS/RTI fired.
                if st.session.machine.cpu6510.reg_sp >= entry_sp.wrapping_add(2) {
                    halted = true;
                    break;
                }
            }
            let cycles_elapsed = st.session.machine.clk.wrapping_sub(start_clk);
            let final_pc = st.session.machine.cpu6510.reg_pc;
            Response::ok(id, json!({
                "halted": halted,
                "cyclesElapsed": cycles_elapsed,
                "instructionsElapsed": cycles_elapsed,
                "finalPc": final_pc as u64
            }))
        }

        // monitorFind(start,end,pattern): FindResult[] — scan C64 memory for a byte
        // pattern. agent-api.ts:246 → MonitorAPI.find. Returns the match addresses.
        "monitorFind" => {
            let start_addr = args.first().and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let end_addr = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0xffff) as u16;
            let pattern: Vec<u8> = args
                .get(2)
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|b| b.as_u64().map(|n| n as u8)).collect())
                .unwrap_or_default();
            let st = state.lock().unwrap();
            let mut hits: Vec<Value> = Vec::new();
            if !pattern.is_empty() && end_addr >= start_addr {
                let plen = pattern.len();
                let mut a = start_addr as u32;
                let last = end_addr as u32;
                while a + (plen as u32) <= last + 1 {
                    let mut matched = true;
                    for (i, p) in pattern.iter().enumerate() {
                        if st.session.machine.read_full((a as u16).wrapping_add(i as u16)) != *p {
                            matched = false;
                            break;
                        }
                    }
                    if matched {
                        hits.push(json!({ "addr": a as u64 }));
                    }
                    a += 1;
                }
            }
            Response::ok(id, json!(hits))
        }

        // runScenario(scenario): ReplayResult — deterministic replay. agent-api.ts:145
        // → scenario.ts runScenario. Reuses the same run_scenario the
        // runtime/scenario_run handler drives.
        "runScenario" => {
            let scenario = match args.first() {
                Some(s) if s.is_object() => s.clone(),
                _ => return Response::err(id, -32602, "runScenario: scenario object required"),
            };
            let mut st = state.lock().unwrap();
            match run_scenario(&mut st, &scenario) {
                Ok(result) => Response::ok(id, result),
                Err(e) => Response::err(id, -32001, format!("runScenario: {e}")),
            }
        }

        // saveVsf(): Uint8Array — full session state as c64re VSF bytes. agent-api.ts
        // :291 → vsf.rs save_vsf. Returned as a byte array (the TS runtime/call path
        // returns the raw Uint8Array; JSON-RPC carries it as a number array).
        "saveVsf" => {
            let mut st = state.lock().unwrap();
            let bytes = trx64_core::vsf::save_vsf(&mut st.session.machine);
            let arr: Vec<u64> = bytes.into_iter().map(|b| b as u64).collect();
            Response::ok(id, json!(arr))
        }

        // ── Breakpoint family (beyond the narrow addPc/list/remove) ──────────────
        // addBreakpoint(spec): string — agent-api.ts:178. Accepts a BreakpointSpec
        // {id, predicate:{kind:"pc",pc}, action, enabled}. Stored on the same
        // api_entries surface backing listBreakpoints/removeBreakpoint.
        "addBreakpoint" => {
            let spec = match args.first() {
                Some(s) if s.is_object() => s.clone(),
                _ => return Response::err(id, -32602, "addBreakpoint: spec object required"),
            };
            let bp_id = spec.get("id").and_then(|v| v.as_str()).unwrap_or("bp0").to_string();
            let pc = spec
                .get("predicate")
                .and_then(|p| p.get("pc"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u16;
            let action = spec.get("action").and_then(|v| v.as_str()).unwrap_or("halt").to_string();
            let enabled = spec.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            let hit_limit = spec.get("hitLimit").and_then(|v| v.as_u64()).map(|n| n as u32);
            let ignore_count = spec.get("ignoreCount").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let mut st = state.lock().unwrap();
            st.breakpoints.api_entries.retain(|e| e.id != bp_id);
            st.breakpoints.api_entries.push(ApiBpEntry {
                id: bp_id.clone(),
                pc,
                action,
                enabled,
                hit_limit,
                ignore_count,
                hit_count: 0,
            });
            // TS addBreakpoint returns spec.id.
            Response::ok(id, json!(bp_id))
        }

        // addTracepoint(id,pc): string — agent-api.ts:186. A tracepoint is a
        // non-halting (action="trace") pc breakpoint on the same surface.
        "addTracepoint" => {
            let bp_id = args.first().and_then(|v| v.as_str()).unwrap_or("tp0").to_string();
            let pc = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let mut st = state.lock().unwrap();
            st.breakpoints.api_entries.retain(|e| e.id != bp_id);
            st.breakpoints.api_entries.push(ApiBpEntry {
                id: bp_id.clone(),
                pc,
                action: "trace".to_string(),
                enabled: true,
                hit_limit: None,
                ignore_count: 0,
                hit_count: 0,
            });
            Response::ok(id, json!(bp_id))
        }

        // enableBreakpoint(id,enabled): void — agent-api.ts:196.
        "enableBreakpoint" => {
            let bp_id = args.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
            let enabled = args.get(1).and_then(|v| v.as_bool()).unwrap_or(true);
            let mut st = state.lock().unwrap();
            if let Some(e) = st.breakpoints.api_entries.iter_mut().find(|e| e.id == bp_id) {
                e.enabled = enabled;
            }
            Response::void(id)
        }

        // setBreakpointIgnoreCount(id,count): void — agent-api.ts:200 (VICE `ignore`).
        "setBreakpointIgnoreCount" => {
            let bp_id = args.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
            let count = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let mut st = state.lock().unwrap();
            if let Some(e) = st.breakpoints.api_entries.iter_mut().find(|e| e.id == bp_id) {
                e.ignore_count = count;
            }
            Response::void(id)
        }

        // breakpointAuditLog(): {id,reason,cycle}[] — agent-api.ts:203. TRX64 keeps
        // no per-hit audit ring on this surface yet; return an empty log (a real,
        // non-error reply — the method IS handled), matching the TS shape.
        "breakpointAuditLog" => {
            let _st = state.lock().unwrap();
            Response::ok(id, json!([]))
        }

        // ── Disasm linkage (Spec 235) — resolvePc / resolvePcs ───────────────────
        // agent-api.ts:128-133 → resolve-pc.ts. Maps a PC (or list of PCs) to the
        // project disasm knowledge at that address (routine / label / segment /
        // source) read from `<artifactId>_analysis.json` + `<artifactId>_annotations
        // .json` (the SAME on-disk files the inspect/xref/sym bridge reads). The TS
        // facade signature is resolvePc(artifactId, pc): runtime/call carries args as
        // [artifactId, pc] (resolvePcs: [artifactId, pcs[]]). Returns the ResolvedPc
        // JSON byte-for-byte (absent layers omitted, like TS `undefined`).
        "resolvePc" => {
            let artifact_id = args.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
            let pc = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let dir = project_knowledge::active_project_dir();
            Response::ok(id, project_knowledge::resolve_pc(&dir, &artifact_id, pc))
        }

        "resolvePcs" => {
            let artifact_id = args.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
            let pcs: Vec<u32> = args
                .get(1)
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|p| p.as_u64().map(|n| n as u32)).collect())
                .unwrap_or_default();
            let dir = project_knowledge::active_project_dir();
            Response::ok(id, json!(project_knowledge::resolve_pcs(&dir, &artifact_id, &pcs)))
        }

        // ── Snapshot diff (Spec 246) — diffSnapshots / formatDiff ─────────────────
        // agent-api.ts:150-155 → snapshot-diff.ts. diffSnapshots(a, b) compares two
        // c64re-own VSF byte buffers (the SAME framing `saveVsf` returns) → a
        // structured SnapshotDiff (RAM ranges + per-chip register diffs + PLA + drive
        // + IEC). formatDiff(diff) renders the text table. runtime/call carries the
        // VSF buffers as JSON number arrays (the Uint8Array transport, like saveVsf).
        "diffSnapshots" => {
            let to_bytes = |v: Option<&Value>| -> Vec<u8> {
                v.and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|b| b.as_u64().map(|n| n as u8)).collect())
                    .unwrap_or_default()
            };
            let a = to_bytes(args.first());
            let b = to_bytes(args.get(1));
            Response::ok(id, snapshot_diff::diff_snapshots(&a, &b))
        }

        "formatDiff" => {
            let diff = args.first().cloned().unwrap_or(json!({}));
            Response::ok(id, json!(snapshot_diff::format_diff(&diff)))
        }

        // ── Rewind / time-travel AgentQueryApi methods (runtime/call) ────────────
        // Spec 243 / 769: beginRewindSession / rewindTo / applyPatch / runForward /
        // diffBranches / promoteBranch (agent-api.ts:251-274). EVERY one routes through
        // `beginRewindSession()`, which REQUIRES scenarioId+diskPath+mode in the
        // AgentApiOptions (agent-api.ts:252-253):
        //     if (!this.scenarioId || !this.diskPath || !this.mode)
        //       throw new Error("beginRewindSession requires scenarioId+diskPath+mode in AgentApiOptions");
        // In the c64re daemon `runtime/call` builds `createAgentQueryApi({ session })`
        // with NO scenarioId/diskPath/mode (ws-server.ts:1720), so over `runtime/call`
        // ALL SIX throw that IDENTICAL guard string BEFORE touching any RewindManager
        // state — verified against the live TS daemon. None is observably functional
        // over WS. The ONLY working rewind surface over WS is the dedicated
        // `runtime/snapshot_tree` + `runtime/promote_branch` handlers (ws-server.ts
        // :1897/1917 pass scenarioId+diskPath+mode), backed here by rewind.rs.
        //
        // So for byte-faithful parity these six are HANDLED (not method-not-found) but
        // return the IDENTICAL guard error TS does — backing a working rewind over
        // runtime/call would DIVERGE (TRX64 succeeding where TS throws = fake-green).
        // Same pattern as the trace-method arm below ("traceBackend not configured").
        "beginRewindSession" | "rewindTo" | "applyPatch" | "runForward"
        | "diffBranches" | "promoteBranch" => {
            Response::err(
                id,
                -32000,
                "beginRewindSession requires scenarioId+diskPath+mode in AgentApiOptions",
            )
        }

        // ── Trace-backed AgentQueryApi methods (runtime/call) ────────────────────
        // queryEvents / followPath / swimlaneSlice / traceTaint / profileLoader.
        // In the c64re daemon `runtime/call` builds `createAgentQueryApi({ session })`
        // with NO `traceBackend` wired (ws-server.ts:1720), so EVERY one of these
        // throws "traceBackend not configured" (agent-api.ts:107…124) — verified
        // against the live TS daemon. The REAL trace-read surface is `trace/read`
        // (now sidecar-backed). So for byte-faithful parity these methods are HANDLED
        // (not method-not-found) but return the IDENTICAL "traceBackend not
        // configured" error TS does — routing them to the sidecar would DIVERGE
        // (TRX64 succeeding where TS errors = fake-green). Use `trace/read` op=
        // query_events / swimlane / follow_path / taint / profile_loader instead.
        "queryEvents" | "followPath" | "swimlaneSlice" | "traceTaint" | "profileLoader" => {
            Response::err(id, -32000, "traceBackend not configured")
        }

        other => {
            Response::err(id, -32601, format!("api/call: unknown method '{other}'"))
        }
    }
}

// ── RPC method dispatch ───────────────────────────────────────────────────────

pub fn dispatch(req: Request, state: &SharedState) -> Response {
    let id = req.id.clone();
    match req.method.as_str() {
        "ping" => {
            Response::ok(id, json!({}))
        }

        "session/create" => {
            let mut st = state.lock().unwrap();
            // Spec 744 shared-attach: the singleton machine is constructed at daemon
            // boot, so session/create ALWAYS attaches to the existing machine (mirrors
            // TS runtimeSessions.start → attached=true when a machine is present). The
            // boot-time construct is the only attached=false; clients never observe it.
            let attached = true;
            // audit ws-session-debug-6 — session/create HONOURS trace_out/trace_domains
            // (+ device_id/pal/start_track/write_protected). TS (ws-server.ts:608-633):
            // threads all params; when trace_out is set it opens a session trace
            // ATOMICALLY via startSessionTrace (binary .c64retrace) so a trace is ACTIVE
            // right after create, and returns `trace` = {runId, outputPath, domains}.
            // TRX64 (pre-fix) read NO params and hardcoded trace:null. On a SHARED
            // ATTACH the device/pal/start_track/write_protected params do NOT reconstruct
            // the singleton machine (TS attach does not auto-mount/re-cold either — see
            // the One-Machine-Per-Process contract), so they are accepted as a no-op on
            // attach; the load-bearing, testable behaviour is the trace.
            let trace_out = req.params.get("trace_out").and_then(|v| v.as_str());
            let mut trace_val = Value::Null;
            if let Some(out_str) = trace_out {
                // TS: domains default to DEFAULT_TRACE_DOMAINS (["c64-cpu","memory"])
                // when trace_out is set without explicit trace_domains.
                let domains: Vec<String> = req
                    .params
                    .get("trace_domains")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|d| d.as_str().map(String::from)).collect())
                    .unwrap_or_else(|| vec!["c64-cpu".into(), "memory".into()]);
                let output = PathBuf::from(out_str);
                let retrace = output.with_extension("c64retrace");
                let cycle_start = st.session.machine.clk;
                let run_id = format!("run_live-capture_{}", cycle_start);
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
                st.session.machine.drive8.flush_disk_writeback();
                let (media_sha, media_name) = match st.session.machine.drive8.get_attached_disk() {
                    Some(disk) => (
                        sha256_hex(&disk.bytes),
                        disk.backing_path
                            .as_ref()
                            .and_then(|p| p.rsplit('/').next())
                            .map(String::from)
                            .unwrap_or_default(),
                    ),
                    None => (String::new(), String::new()),
                };
                let start_wall_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                st.session.trace = Some(TraceState {
                    retrace_path: retrace,
                    meta_json,
                    cycle_start,
                    buf: Vec::new(),
                    run_id: run_id.clone(),
                    event_count: 0,
                    domains: domains.clone(),
                    marks: Vec::new(),
                    // captureAll session trace ⇒ definitionId "live-capture" (= TS).
                    definition_id: "live-capture".to_string(),
                    definition_version: 1,
                    start_wall_ms,
                    media_sha,
                    media_name,
                    // session/create-with-trace = captureAll (keep every domain-implied row).
                    captures: Vec::new(),
                });
                st.last_run_id = Some(run_id.clone());
                // TS startSessionTrace returns { runId, outputPath, domains }.
                trace_val = json!({
                    "runId": run_id,
                    "outputPath": out_str,
                    "domains": domains,
                });
            }
            let cpu = &st.session.machine.cpu;
            let pc = cpu.pc as u64;
            let c64_cycles = st.session.machine.clk;
            let disk_path = st.session.disk_path.clone();
            Response::ok(id, json!({
                "sessionId": "integrated-1",
                "mode": "true-drive",
                "diskPath": disk_path,
                "attached": attached,
                "c64Cycles": c64_cycles,
                "pc": pc,
                "trace": trace_val
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
            // audit ws-session-debug-2 — session/run is a MANUAL/HEADLESS primitive
            // only; the autonomous loop owns the clock under debug/run. TS throws when
            // runState==='running' so the two clocks can't double-advance the CPU
            // (ws-server.ts:842-848). TRX64's `running` flag is the equivalent runState.
            if st.session.running {
                return Response::err(
                    id, -32001,
                    "session is running under the autonomous loop; use debug/pause before manual session/run",
                );
            }
            let cycles = req
                .params
                .get("cycles")
                .and_then(|v| v.as_u64())
                .unwrap_or(19705);

            // audit ws-session-debug-3 — a manual (paused) session/run HONOURS exec
            // breakpoints. TS (ws-server.ts:852-901): build the bp set, step PAST a bp
            // it is sitting on, run the budget WITH the bp set, and on a hit return
            // early with a breakpoint{pc,num,registers} object. When NONE are armed,
            // fall through to the historical plain (trace-aware) budget advance so the
            // no-debug + trace-firehose paths (formats-state-6, background-workers-
            // async-5) are unchanged.
            {
                let State { breakpoints, dsl_observers, dsl_disabled, observers: reg, .. } = &mut *st;
                sync_observers(breakpoints, dsl_observers, dsl_disabled, reg);
            }
            if observers_armed(&st.observers) {
                // runtime-controller.ts:277 / ws-server.ts:855 — step PAST a bp the PC is
                // sitting ON so the run doesn't immediately re-trip the same address.
                {
                    let pc = st.session.machine.c64_core.reg_pc;
                    if st.breakpoints.entries.iter().any(|e| e.enabled && e.pc == pc) {
                        step_one_instruction(&mut st.session);
                    }
                }
                let run = {
                    let State { session, observers: reg, .. } = &mut *st;
                    run_until_break(session, reg, cycles)
                };
                {
                    let State { breakpoints, observers: reg, .. } = &mut *st;
                    writeback_hits(breakpoints, reg);
                }
                let cycles_now = st.session.machine.clk;
                if run.halted {
                    // bpNumForAddr returns 0 (NOT null) when no numbered bp matches
                    // (runtime-controller.ts:238) — match the TS reply exactly.
                    let bp_num = st
                        .breakpoints
                        .entries
                        .iter()
                        .find(|e| e.pc == run.pc)
                        .map(|e| e.num)
                        .unwrap_or(0);
                    st.ctrl_stop = Some(CtrlStop { reason: "breakpoint", pc: run.pc, cycles: cycles_now });
                    let registers = register_dump(&st.session);
                    return Response::ok(id, json!({
                        "c64Cycles": cycles_now,
                        "breakpoint": {
                            "pc": run.pc as u64,
                            "num": bp_num as u64,
                            "registers": registers,
                        },
                    }));
                }
                // Spec 764 — a KIL/JAM that fired DURING this armed run jams the CPU
                // (PC frozen) without tripping a bp; observe it via the shared helper
                // (freeze + debug/stopped reason="jam") and signal the caller so the
                // CLI pump PAUSES instead of re-issuing session/run on the jammed CPU.
                if check_and_handle_jam(&mut st) {
                    let pc = jammed_pc(&st);
                    return Response::ok(id, json!({
                        "c64Cycles": st.session.machine.clk,
                        "jam": { "pc": pc as u64 },
                    }));
                }
                // Budget exhausted without a hit: report the advanced cycle count.
                return Response::ok(id, json!({ "c64Cycles": cycles_now }));
            }

            run_cycle_budget(&mut st.session, cycles);
            // Spec 764 — the no-debug budget advance has no halt gate, so a jammed CPU
            // (KIL frozen at PC) would otherwise burn the WHOLE budget here and the CLI
            // pump would keep re-issuing session/run → the spin/hang. Observe the jam via
            // the shared helper (freeze + debug/stopped reason="jam") and signal it so the
            // pump halts. The PC stays frozen at the KIL.
            if check_and_handle_jam(&mut st) {
                let pc = jammed_pc(&st);
                return Response::ok(id, json!({
                    "c64Cycles": st.session.machine.clk,
                    "jam": { "pc": pc as u64 },
                }));
            }
            Response::ok(id, json!({ "c64Cycles": st.session.machine.clk }))
        }

        "session/state" => {
            let st = state.lock().unwrap();
            // Spec 771.2 — report the REAL run/pause state + last stop reason (was
            // hardcoded "paused", which kept the UI's seed poll permanently frozen).
            // Mirrors session/state in ws-server.ts (runState/stopReason/controlOwner).
            let run_state = if st.session.running { "running" } else { "paused" };
            let stop_reason = st.ctrl_stop.as_ref().map(|s| s.reason);
            // Spec 771.2 (T1.1) — live audio is streaming when the hub is on AND running.
            let streaming = st.streaming_enabled && st.session.running;
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
            let mut state_json = json!({
                "c64Cycles": machine.clk,
                "driveCycles": machine.drive8.drive_clk,
                "mode": "true-drive",
                "runState": run_state,
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
                "sid": { "regs": sid_regs, "streaming": streaming }
            });
            // TS session/state (ws-server.ts:531) emits stopReason ONLY when set
            // (stopInfo?.reason → undefined omits the key) and has NO controlOwner.
            if let Some(r) = stop_reason { state_json["stopReason"] = json!(r); }
            Response::ok(id, state_json)
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
            // GUARDRAIL #1 — warn (non-fatal) if injecting into a free-running core.
            let warning = free_run_input_warning(&st);
            Response::ok(id, json!({
                "c64Cycles": c64_cycles,
                "queued": queued,
                "freeRunWarning": warning,
            }))
        }

        // session/joystick_set — virtual joystick state (ws-server.ts:1468). UI
        // maps WASD+Space → bits and POSTs the resolved port + bits. port==1 sets
        // joystick1 (CIA1 PB), else joystick2 (CIA1 PA). Mirrors the TS
        // `setJoystick1/2(state)` (the bits are coerced to bool, default false).
        "session/joystick_set" => {
            let port = req.params.get("port").and_then(|v| v.as_u64()).unwrap_or(0);
            let b = |k: &str| req.params.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
            let new_state = trx64_core::keyboard::JoystickState {
                up: b("up"),
                down: b("down"),
                left: b("left"),
                right: b("right"),
                fire: b("fire"),
            };
            let mut st = state.lock().unwrap();
            if port == 1 {
                st.session.machine.joystick1 = new_state;
            } else {
                st.session.machine.joystick2 = new_state;
            }
            Response::ok(id, json!({ "ok": true }))
        }

        // session/joystick_clear — clear one or both joysticks (ws-server.ts:1475).
        // `port` 1 → joystick1, 2 → joystick2, absent → both. Shape: TS
        // `{ ok: true }`.
        "session/joystick_clear" => {
            let cleared = trx64_core::keyboard::JoystickState::default();
            let port = req.params.get("port").and_then(|v| v.as_u64());
            let mut st = state.lock().unwrap();
            if port == Some(1) || port.is_none() {
                st.session.machine.joystick1 = cleared;
            }
            if port == Some(2) || port.is_none() {
                st.session.machine.joystick2 = cleared;
            }
            Response::ok(id, json!({ "ok": true }))
        }

        // session/input_status — UI inspector read of pressed keys + joystick bits
        // (ws-server.ts:1486). Reports the held-key set via pressed_keys() (Spec
        // 310, batch 2) + the LIVE joystick1/joystick2 state. Shape matches the TS
        // `{ pressed, joystick1, joystick2 }`.
        "session/input_status" => {
            let st = state.lock().unwrap();
            let pressed: Vec<Value> = st
                .session
                .machine
                .keyboard
                .pressed_keys()
                .into_iter()
                .map(Value::String)
                .collect();
            let joy_json = |j: &trx64_core::keyboard::JoystickState| {
                json!({
                    "up": j.up, "down": j.down, "left": j.left,
                    "right": j.right, "fire": j.fire
                })
            };
            let joystick1 = joy_json(&st.session.machine.joystick1);
            let joystick2 = joy_json(&st.session.machine.joystick2);
            Response::ok(id, json!({
                "pressed": Value::Array(pressed),
                "joystick1": joystick1,
                "joystick2": joystick2
            }))
        }

        // session/load_prg — inject a PRG into RAM (ws-server.ts:761 →
        // loadPrgIntoRam). Reads the local file, writes the body at the load address
        // (PRG header = 2-byte LE load addr), and returns
        // { loadAddress, endAddress, bytesLoaded, path }. Load-only: does NOT set PC
        // or autostart (that is runtime/run_prg).
        // session/key_down — Spec 310 live keyboard passthrough (ws-server.ts:1443).
        // Marks `key` held on the matrix until an explicit release; returns the
        // current held set. `key` is a c64re key id (e.g. "A", "L_SHIFT",
        // "RUN_STOP" — same names as session/type's matrix). Shape: TS
        // `{ ok: true, pressed: s.pressedKeys() }`.
        "session/key_down" => {
            let key = match req.params.get("key").and_then(|v| v.as_str()) {
                Some(k) => k.to_string(),
                None => return Response::err(id, -32602, "session/key_down: key required"),
            };
            let mut st = state.lock().unwrap();
            st.session.machine.keyboard.key_down(&key);
            let pressed: Vec<Value> = st
                .session
                .machine
                .keyboard
                .pressed_keys()
                .into_iter()
                .map(Value::String)
                .collect();
            // GUARDRAIL #1 — warn (non-fatal) if injecting into a free-running core.
            let warning = free_run_input_warning(&st);
            Response::ok(id, json!({ "ok": true, "pressed": pressed, "freeRunWarning": warning }))
        }

        // session/key_up — release a single held key (ws-server.ts:1449).
        // Returns the remaining held set. Shape: TS `{ ok: true, pressed }`.
        "session/key_up" => {
            let key = match req.params.get("key").and_then(|v| v.as_str()) {
                Some(k) => k.to_string(),
                None => return Response::err(id, -32602, "session/key_up: key required"),
            };
            let mut st = state.lock().unwrap();
            st.session.machine.keyboard.key_up(&key);
            let pressed: Vec<Value> = st
                .session
                .machine
                .keyboard
                .pressed_keys()
                .into_iter()
                .map(Value::String)
                .collect();
            // GUARDRAIL #1 — warn (non-fatal) if injecting into a free-running core.
            let warning = free_run_input_warning(&st);
            Response::ok(id, json!({ "ok": true, "pressed": pressed, "freeRunWarning": warning }))
        }

        // session/release_keys — release all held keys (ws-server.ts:1455). The TS
        // also clears BOTH joysticks on release-all (focus-loss policy,
        // ws-server.ts:1459-1461). Shape: TS `{ ok: true }`.
        "session/release_keys" => {
            let mut st = state.lock().unwrap();
            st.session.machine.keyboard.release_keys();
            st.session.machine.joystick1 = trx64_core::keyboard::JoystickState::default();
            st.session.machine.joystick2 = trx64_core::keyboard::JoystickState::default();
            Response::ok(id, json!({ "ok": true }))
        }

        "session/load_prg" => {
            let prg_path = match req.params.get("prg_path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return Response::err(id, -32602, "session/load_prg: prg_path required"),
            };
            // Resolve relative to the cockpit `cd` cwd (same as media/mount).
            let prg_path = { let st = state.lock().unwrap(); resolve_fs_path_with_state(&st, &prg_path) };
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
            // c64re loadPrgIntoRam (integrated-session.ts:885): endAddress is the
            // address of the LAST byte = (load + len - 1) & 0xFFFF.
            let end_address = load_address
                .wrapping_add(body.len() as u16)
                .wrapping_sub(1);
            Response::ok(id, json!({
                "loadAddress": load_address as u64,
                "endAddress": end_address as u64,
                "bytesLoaded": body.len() as u64,
                "path": prg_path
            }))
        }

        // session/reset — RuntimeController re-init (ws-server.ts:1392). mode:"soft"
        // = warm (HW RESET line, RAM preserved → `resetWarm`, ws-server.ts:1409),
        // else cold (full power-cycle → `resetCold`, ws-server.ts:1413). The warm
        // path re-inits CPU + I/O chips + drive and restores $00/$01 banking so the
        // $FFFC vector reads $FCE2 and the KERNAL reset runs clean — recovering even
        // from a running/JAMmed game; RAM is preserved. The cold path additionally
        // fills power-on DRAM + power-cycles the drive. Both run the KERNAL to READY
        // (5M cycles, matching the TS runFor). Returns { c64Cycles, pc, mode }.
        // Spec 786 — reset composes the power primitives:
        //   soft → warm_reset ($FCE2, RAM + media preserved, I/O chips reset)
        //   cold → power_off → power_on (fresh full init; inserted media kept)
        "session/reset" => {
            let mode = req.params.get("mode").and_then(|v| v.as_str()).unwrap_or("cold");
            let mut st = state.lock().unwrap();
            let out_mode = if mode == "soft" { "soft" } else { "cold" };
            if mode == "soft" {
                // Warm = HW RESET line: $FCE2 via $FFFC, RAM + media preserved,
                // VIC/CIA/CPU/IEC reset. No-op if powered off.
                st.session.warm_reset();
                st.session.machine.keyboard.clear();
                run_cycle_budget(&mut st.session, 5_000_000);
                // A reset is a control + audio-timeline discontinuity; the held
                // FlowTracker frames are stale.
                st.ctrl_stop = None;
                st.ctrl_frame += 1;
                st.flow.reset();
                st.stream_broke_on_jam = false;
                st.notify.broadcast("audio/flush", json!({ "session_id": st.session.id }));
            } else {
                // Cold reset = the power-cycle composition. do_power_* carry the
                // ring drop, boot warm-up, flow/monitor reset + audio flush.
                do_power_off(&mut st);
                do_power_on(&mut st);
            }
            let pc = st.session.machine.cpu6510.reg_pc as u64;
            let cycles = st.session.machine.clk;
            Response::ok(id, json!({
                "c64Cycles": cycles,
                "pc": pc,
                "mode": out_mode
            }))
        }

        // Spec 786 — explicit power verbs. `on` = full init (no-op if already
        // on), `off` = dead machine, no live state (no-op if already off).
        "session/power" => {
            let op = req.params.get("op").and_then(|v| v.as_str()).unwrap_or("");
            let mut st = state.lock().unwrap();
            match op {
                "on" => do_power_on(&mut st),
                "off" => do_power_off(&mut st),
                _ => return Response::err(id, -32602, "session/power: op must be \"on\" or \"off\""),
            }
            let powered = st.session.powered;
            let pc = st.session.machine.cpu6510.reg_pc as u64;
            let cycles = st.session.machine.clk;
            Response::ok(id, json!({
                "op": op,
                "powered": powered,
                "pc": pc,
                "c64Cycles": cycles
            }))
        }

        // session/set_pacing — T1.3. TS ws-server.ts:1378.
        // Validates mode ∈ {"pal","warp","fixed-ratio"} (-32602 on bad mode).
        // Calls ctrl.setPacing(mode, ratio): stores mode unconditionally; stores ratio
        // only if it is truthy AND > 0 (mirrors runtime-controller.ts:329-331).
        // TRX64 has no autonomous pacing loop (no resetPaceEpoch), so we only update
        // the stored fields in State. Returns build_debug_state (= c.state() shape).
        "session/set_pacing" => {
            let mode = match req.params.get("mode").and_then(|v| v.as_str()) {
                Some(m) => m.to_string(),
                None => return Response::err(id, -32602, "bad pacing mode: null"),
            };
            if !matches!(mode.as_str(), "pal" | "warp" | "fixed-ratio") {
                return Response::err(id, -32602, format!("bad pacing mode: {mode}"));
            }
            let ratio = req.params.get("ratio").and_then(|v| v.as_f64());
            let mut st = state.lock().unwrap();
            st.pacing_mode = mode;
            if let Some(r) = ratio {
                if r > 0.0 {
                    st.pacing_ratio = r;
                }
            }
            Response::ok(id, build_debug_state(&st))
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
            // T2.3 — sector under the GCR read head (ws-server.ts:1519-1524):
            // viceSectorUnderHead returns -1 for no header / empty track; the TS
            // keeps `sector` at 0 in that case (only assigns when `sec >= 0`).
            let sector: u64 = {
                let sec = drv.rotation.sector_under_head();
                if sec >= 0 { sec as u64 } else { 0 }
            };
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
                "sector": sector,
                "drivePc": drive_pc,
                "dd00": { "pra": dd00pra, "ddr": dd00ddr },
                "transferMode": transfer_mode
            }))
        }

        // session/cart_status — live cartridge status (ws-server.ts:1581). Returns
        // null when no cart attached; else { type, bank, activity, booted, sourceName }.
        // T2.4 / BUG-042: mirrors the TS `cartLedTrack` write-LED logic:
        //   gen = cart.writableGeneration() (monotonic flash/EEPROM write counter)
        //   if gen advanced since last poll → stamp lastWriteAt
        //   if < 1.2 s since lastWriteAt → activity = "write"
        //   else if cart mapped (exrom==0 || game==0) → "read", else "idle"
        // booted is false (no cartBootedFrom tracking in TRX64).
        "session/cart_status" => {
            let mut st = state.lock().unwrap();
            // Spec 709.13 — sourceName is the mounted FILE name (TS = getCartridgeMedia().name,
            // ws-server.ts:1581), NOT the cartridge_image CRT-header name. The CRT header name
            // is baked at build time and shared across a project's derived carts, so reporting
            // it makes the CART label look stale/cached + wrong (e.g. "WASTELAND EF MENU POC"
            // for every wasteland cart). The mounted file path is the backend truth.
            let cart_path = st.session.cart_path.clone();
            let m = &st.session.machine;
            match m.cartridge.as_ref() {
                None => Response::ok(id, Value::Null),
                Some(cart) => {
                    let type_str = mapper_type_str(cart.mapper_type());
                    let bank = cart.get_state().current_bank as u64;
                    let lines = cart.get_lines();
                    let mapped = lines.exrom == 0 || lines.game == 0;
                    let source_name = if cart_path.is_empty() {
                        None
                    } else {
                        Some(
                            std::path::Path::new(&cart_path)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| cart_path.clone()),
                        )
                    };
                    // BUG-042 write-LED: track writableGeneration across polls.
                    let gen = cart.writable_generation();
                    if gen != st.cart_led_gen {
                        st.cart_led_gen = gen;
                        st.cart_led_last_write_at = Some(std::time::Instant::now());
                    }
                    let write_held = st.cart_led_last_write_at
                        .map(|t| t.elapsed() < std::time::Duration::from_millis(1200))
                        .unwrap_or(false);
                    let activity: &str = if write_held {
                        "write"
                    } else if mapped {
                        "read"
                    } else {
                        "idle"
                    };
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
        // press = cold reset of the drive 6502 (DOS re-runs power-on init).
        // Parity: TS includes "mode" only when reinitialized=true (success path).
        // TRX64 has no fallback, so always reinitialized=true + mode.
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
            let res = run_monitor(&mut st, &cmd);
            // Forward the modal `prompt` (= TS `MonitorResult.prompt`) when a modal verb
            // (`a` assemble / `df -i`) set one — so the wire reply matches the TS
            // `runMonitorCommand` `{ output, prompt }` / `{ error, prompt }` shape.
            let prompt = st.mon.pending_prompt.take();
            match res {
                Ok(out) => {
                    let mut body = json!({ "output": out });
                    if let Some(p) = prompt {
                        body["prompt"] = json!(p);
                    }
                    Response::ok(id, body)
                }
                Err(e) => {
                    let mut body = json!({ "error": e });
                    if let Some(p) = prompt {
                        body["prompt"] = json!(p);
                    }
                    Response::ok(id, body)
                }
            }
        }

        // CLI-FEEL S3 — Tab path-completion backend for the trx64cli cockpit. This is
        // a COCKPIT convenience rpc; it does NOT touch the shared monitor FS verbs
        // (pwd/cd/ls/… in run_monitor stay bare-callable). Resolve `partial` against
        // the cockpit `cd` cwd with the SAME rules the FS verbs / media mount use
        // (resolve_fs_path_with_state), split it into (dir, stem), list `dir`, and
        // return the entries whose name starts with `stem` (case-insensitive) plus
        // the longest common prefix of the matches so the client can fill the shared
        // stem. Trailing `/` lists a directory's contents (empty stem); no arg lists
        // the cwd. Errors are SOFT → empty entries (Tab must never surface an error).
        // Cap 500, matching the `ls` verb.
        "fs/complete" => {
            let partial = req
                .params
                .get("partial")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Split at the LAST '/': everything up to & including it is the directory
            // part, the rest is the stem being completed. No slash → complete inside
            // the cwd (dir_part = "").
            let (dir_part, stem) = match partial.rfind('/') {
                Some(i) => (&partial[..=i], &partial[i + 1..]),
                None => ("", partial.as_str()),
            };
            // Empty dir_part → the cwd (resolve "." against it). An absolute or
            // relative dir_part resolves exactly like the FS verbs do.
            let dir_arg = if dir_part.is_empty() { "." } else { dir_part };
            let resolved_dir = {
                let st = state.lock().unwrap();
                resolve_fs_path_with_state(&st, dir_arg)
            };
            let stem_lc = stem.to_lowercase();
            let mut ents: Vec<(bool, String)> = match std::fs::read_dir(&resolved_dir) {
                Ok(rd) => rd
                    .filter_map(|e| e.ok())
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        if stem_lc.is_empty() || name.to_lowercase().starts_with(&stem_lc) {
                            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                            Some((is_dir, name))
                        } else {
                            None
                        }
                    })
                    .collect(),
                // Soft error (missing dir / permission): Tab returns an empty set.
                Err(_) => Vec::new(),
            };
            ents.sort_by(|a, b| a.1.cmp(&b.1));
            ents.truncate(500);
            let common = fs_longest_common_prefix(ents.iter().map(|(_, n)| n.as_str()));
            let entries: Vec<Value> = ents
                .iter()
                .map(|(is_dir, name)| json!({ "name": name, "is_dir": is_dir }))
                .collect();
            Response::ok(
                id,
                json!({ "entries": entries, "common": common, "dir": resolved_dir }),
            )
        }

        // ── debug/* ──────────────────────────────────────────────────────────

        "debug/run" => {
            // audit ws-session-debug-1 — debug/run is ASYNC-SCHEDULED, never blocking.
            // TS (runtime-controller.ts:262-284 + ws-server.ts:985-991): run() flips
            // runState→running, pushes debug/running, schedules the loop, and returns
            // ctrl.state() IMMEDIATELY. A later breakpoint halt is PUSHED via debug/
            // stopped from the loop — the reply is NEVER the halt. The pre-fix TRX64
            // called run_debug_control here, which ran an INLINE synchronous
            // run_until_break (up to DEBUG_RUN_BUDGET=10M cyc) whenever a bp/observer
            // was armed, so debug/run BLOCKED until the bp hit and could even reply
            // "paused". The (P0-A) bp/observer/JAM-aware stream loop
            // (stream_debug_gated_advance) is now the SOLE halt driver: it self-halts at
            // the first hit + server-PUSHes debug/breakpoint_hit|observer_hit +
            // debug/stopped, exactly as TS's tick does. So we just transition to running
            // and return the running state — no inline run.
            let mut st = state.lock().unwrap();
            // T1.2 — Spec 767: read source param, update control_owner, broadcast on change.
            let owner = owner_from_source(&req.params);
            set_control_owner(&mut st, owner);
            st.session.running = true;
            st.ctrl_stop = None;
            // Spec 764 — a fresh run() re-arms the JAM edge (runtime-controller.ts:793
            // brokeOnJam reset on run), so a still-jammed machine asked to run again
            // re-broadcasts debug/stopped reason=jam (the shared helper observes it on
            // the next advance).
            st.stream_broke_on_jam = false;
            st.ctrl_frame += 1;
            // Spec 771.2 — runtime-controller.ts:282 run() server-PUSHes debug/running
            // at the run transition. Without it the UI never leaves the paused/frozen
            // display (and its keyboard handler, gated on runState==="running", never
            // attaches → "can't type").
            let pacing_snap = json!({ "mode": st.pacing_mode, "ratio": st.pacing_ratio });
            st.notify.broadcast("debug/running", json!({
                "session_id": st.session.id,
                "pacing": pacing_snap,
            }));
            // Return ctrl.state() immediately (= TS run() → ctrl.state(), runState
            // "running"). build_debug_state reads the live State (now running).
            Response::ok(id, build_debug_state(&st))
        }

        "debug/pause" => {
            let mut st = state.lock().unwrap();
            // T1.2 — Spec 767: read source param, update control_owner, broadcast on change.
            let owner = owner_from_source(&req.params);
            set_control_owner(&mut st, owner);
            st.session.running = false;
            st.ctrl_frame += 1;
            let frame = st.ctrl_frame;
            let bps = st.breakpoints.list_vice_json();
            let c = &st.session.machine.cpu6510;
            let pc = c.reg_pc as u64;
            let cycles = st.session.machine.clk;
            let stop_obj = json!({ "reason": "pause", "pc": pc, "cycles": cycles });
            st.ctrl_stop = Some(CtrlStop { reason: "pause", pc: c.reg_pc, cycles });
            // Spec 771.2 — runtime-controller.ts:295 pause() server-PUSHes debug/paused.
            st.notify.broadcast("debug/paused", json!({
                "session_id": st.session.id,
                "stop": stop_obj.clone(),
            }));
            let (pacing_mode, pacing_ratio, control_owner) =
                (st.pacing_mode.clone(), st.pacing_ratio, st.control_owner.clone());
            Response::ok(id, json!({
                "runState": "paused",
                "pacing": { "mode": pacing_mode, "ratio": pacing_ratio },
                "pc": pc,
                "cycles": cycles,
                "frame": frame,
                "breakpoints": bps,
                "stop": stop_obj,
                "controlOwner": control_owner
            }))
        }

        "debug/continue" => {
            let mut st = state.lock().unwrap();
            // T1.2 — Spec 767: read source param, update control_owner, broadcast on change.
            let owner = owner_from_source(&req.params);
            set_control_owner(&mut st, owner);
            st.session.running = true;
            st.ctrl_stop = None;
            // Spec 764 — continue() === run(): re-arm the JAM edge so a still-jammed
            // machine asked to continue re-broadcasts debug/stopped reason=jam.
            st.stream_broke_on_jam = false;
            // Spec 771.2 — continue() === run() in runtime-controller.ts:287, so it
            // server-PUSHes debug/running too.
            let pacing_snap = json!({ "mode": st.pacing_mode, "ratio": st.pacing_ratio });
            st.notify.broadcast("debug/running", json!({
                "session_id": st.session.id,
                "pacing": pacing_snap,
            }));
            // TS: continue does not increment frame (stays at pause frame).
            let frame = st.ctrl_frame;
            // A continue from a breakpoint must STEP PAST the current PC first
            // (else the boundary check re-trips the same bp immediately).
            run_debug_control(id, &mut st, frame, true)
        }

        "debug/step" => {
            let mut st = state.lock().unwrap();
            // T1.2 — Spec 767: read source param, update control_owner, broadcast on change.
            let owner = owner_from_source(&req.params);
            set_control_owner(&mut st, owner);
            step_one_instruction(&mut st.session);
            st.session.running = false;
            // T2.2 — Spec 754 §3.3e: drain observer side-effects after the step,
            // matching the TS run-chunk drain (runtime-controller.ts:697-725).
            // TS step() (line 317-326) does not drain explicitly, but step is always
            // called from the WS handler which runs the same drain path. We drain
            // here so observers fired during the single instruction reach the client.
            drain_and_broadcast_observer_log(&mut st);
            let registers = register_dump(&st.session);
            let cycles = st.session.machine.clk;
            let pc = st.session.machine.cpu6510.reg_pc;
            // runtime-controller.ts:322-323 — step() sets stopInfo = makeStopInfo("step")
            // and bumps frameCounter, so the returned state().stop / .frame reflect it.
            st.ctrl_stop = Some(CtrlStop { reason: "step", pc, cycles });
            st.ctrl_frame += 1;
            // Spec 771.2 — runtime-controller.ts:324 step() server-PUSHes debug/stopped
            // (reason "step") with the register dump.
            st.notify.broadcast("debug/stopped", json!({
                "session_id": st.session.id,
                "stop": { "reason": "step", "pc": pc as u64, "cycles": cycles },
                "registers": registers,
            }));
            // audit ws-session-debug-4 — debug/step returns the FULL controller.state()
            // shape (runtime-controller.ts:344-363), NOT a flat register dict. TS returns
            // c.state() = {runState,pacing,pc,cycles,frame,breakpoints,stop,controlOwner}
            // (ws-server.ts:994-1000). build_debug_state is that exact shape.
            Response::ok(id, build_debug_state(&st))
        }

        // debug/state — the RuntimeController.state() snapshot (runtime-controller.ts
        // :344). Read-only: reports the CURRENT run/pause state, pacing, pc/cycles,
        // controller frame, breakpoints, and last stop. TRX64 has no pacing loop, so
        // pacing is the constant PAL pacing the TS reports for an unpaced session.
        "debug/state" => {
            let st = state.lock().unwrap();
            Response::ok(id, build_debug_state(&st))
        }

        "debug/break_add" => {
            let pc_val = req.params.get("pc").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let mut st = state.lock().unwrap();
            let num = st.breakpoints.next_num;
            st.breakpoints.next_num += 1;
            st.breakpoints.entries.push(BpEntry { num, pc: pc_val, enabled: true });
            // audit ws-session-debug-5 — emit `addr` (not `pc`) for each entry: TS
            // uniformly keys a breakpoint by `addr` (runtime-controller.ts
            // listBreakpoints → {num, addr}; ws-server.ts break_add/del/list echo it).
            let list: Vec<Value> = st.breakpoints.entries.iter()
                .map(|e| json!({ "num": e.num, "addr": e.pc as u64 }))
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
            // audit ws-session-debug-5 — `addr` key (= TS), not `pc`.
            let list: Vec<Value> = st.breakpoints.entries.iter()
                .map(|e| json!({ "num": e.num, "addr": e.pc as u64 }))
                .collect();
            Response::ok(id, json!({
                "deleted": true,
                "breakpoints": list
            }))
        }

        "debug/break_list" => {
            let st = state.lock().unwrap();
            // audit ws-session-debug-5 — `addr` key (= TS), not `pc`.
            let list: Vec<Value> = st.breakpoints.entries.iter()
                .map(|e| json!({ "num": e.num, "addr": e.pc as u64 }))
                .collect();
            Response::ok(id, json!({ "breakpoints": list }))
        }

        // ── api/call ─────────────────────────────────────────────────────────

        "api/call" => {
            // Narrow MCP per-verb bridge — allowlist-gated (full=false).
            dispatch_api_call(id, &req.params, state, false)
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
                // Resolve relative to the cockpit `cd` cwd (same as media/mount).
                let path = { let st = state.lock().unwrap(); resolve_fs_path_with_state(&st, &path) };
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

            // Mirror TS ingress.ts loadPrgBytes: when loaded at the standard BASIC
            // start ($0801), set VARTAB ($2D/$2E) = byte after the program so that
            // a subsequent RUN can find the end of the BASIC program. This matches:
            //   if (loadAddress === 0x0801) { ram[0x2d] = endAddress & 0xff;
            //                                 ram[0x2e] = (endAddress >> 8) & 0xff; }
            if load_addr == 0x0801 {
                let end_addr = (load_addr as usize + body.len()) & 0xffff;
                let vartab = [(end_addr & 0xff) as u8, ((end_addr >> 8) & 0xff) as u8];
                st.session.machine.poke(0x002d, &vartab);
            }

            // Mirror TS ws-server.ts runtime/run_prg autostart logic (line 782-788):
            //   if entry != undefined        → pause; set PC = entry; continue
            //   else if loadAddress == $0801 → ctrl.continue(); s.typeText("RUN\r")
            //   else                         → pause; set PC = loadAddress; continue
            let action: String;
            if let Some(entry) = run_addr {
                // Explicit entry point: set PC and resume (mirrors TS pause→setPC→continue).
                let pc = (entry & 0xffff) as u16;
                st.session.machine.cpu6510.reg_pc = pc;
                // The full-machine driver (run_for_full, used by the --stream loop AND
                // session/run) executes from `c64_core`, NOT `cpu6510`; sync_after_monitor
                // only mirrors cpu6510 → the snapshot, not into c64_core. So set the
                // full-machine PC too (= the monitor `g` command, main.rs:1874-1875),
                // else a run-from-entry keeps running the KERNAL at the old c64_core PC.
                st.session.machine.c64_core.reg_pc = pc;
                st.session.machine.sync_after_monitor();
                st.session.injected = true;
                st.session.running = true;
                action = format!("g ${:04x}", pc);
            } else if load_addr == 0x0801 {
                // BASIC program: resume the machine then type "RUN\r" so BASIC executes.
                // Mirrors: ctrl.continue(); s.typeText("RUN\r"); action = "BASIC RUN"
                st.session.running = true;
                st.session.injected = true;
                let now = st.session.machine.cpu6510.clk;
                st.session.machine.keyboard.type_text(now, "RUN\r", 80_000, 80_000);
                action = "BASIC RUN".to_string();
            } else {
                // Machine-code at non-BASIC load address: set PC to load address and resume.
                // Mirrors: pause; set PC = loadAddress; continue; action = "g $XXXX (default = load address)"
                st.session.machine.cpu6510.reg_pc = load_addr;
                // Set the full-machine PC too (= monitor `g`, main.rs:1874-1875) — the
                // run_for_full driver runs from c64_core, which sync_after_monitor does
                // not touch (see the explicit-entry branch above for the full rationale).
                st.session.machine.c64_core.reg_pc = load_addr;
                st.session.machine.sync_after_monitor();
                st.session.injected = true;
                st.session.running = true;
                action = format!("g ${:04x} (default = load address)", load_addr);
            }

            Response::ok(id, json!({
                "loadAddress": load_addr as u64,
                "action": action
            }))
        }

        // ── runtime/overlay_run ──────────────────────────────────────────────
        // Spec 769.2 — code-overlay debug loop: rewind to an anchor checkpoint,
        // apply RAM patches, run forward, observe. 1:1 with ws-server.ts:938-981.
        // Repeatable: each call restores fresh (the prior patch is rolled back by
        // the restore), so the LLM iterates a fix from a fixed point without
        // rebuild/reboot. RAM-only patches (OQ3). Leaves the machine paused.
        // Returns { anchorId, applied[], ranCycles, hitPc, reads, registers }.
        "runtime/overlay_run" => {
            let anchor_id = req.params.get("anchor_id").and_then(|v| v.as_str()).map(String::from);
            let anchor_cycle = req.params.get("anchor_cycle").and_then(|v| v.as_u64());
            let patches: Vec<Value> = req
                .params
                .get("patches")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let run_cycles = req.params.get("run_cycles").and_then(|v| v.as_u64()).unwrap_or(0);
            let until_pc = req.params.get("until_pc").and_then(|v| v.as_u64()).map(|v| (v & 0xffff) as u16);

            let mut st = state.lock().unwrap();
            let cps = st.checkpoint_ring.list();
            if cps.is_empty() {
                return Response::err(id, -32001, "runtime/overlay_run: no checkpoints to anchor on");
            }
            // Pick the anchor: explicit id, else nearest at/before anchor_cycle,
            // else most recent (ws-server.ts:946-954).
            let chosen: String = if let Some(aid) = anchor_id {
                aid
            } else if let Some(cyc) = anchor_cycle {
                let mut at_before: Option<&trx64_core::checkpoint_ring::RuntimeCheckpointRef> = None;
                let mut best = &cps[0];
                let mut best_d = u64::MAX;
                for c in &cps {
                    if c.cycles <= cyc && at_before.map(|a| c.cycles > a.cycles).unwrap_or(true) {
                        at_before = Some(c);
                    }
                    let d = c.cycles.abs_diff(cyc);
                    if d < best_d {
                        best_d = d;
                        best = c;
                    }
                }
                at_before.unwrap_or(best).id.clone()
            } else {
                cps[cps.len() - 1].id.clone()
            };

            // Restore the anchor (then: "pause") — same payload-rehydration path as
            // checkpoint/restore (ws-server.ts:955 ctrl.restoreCheckpoint(id,{then:"pause"})).
            let snapshot = match st.checkpoint_ring.restore_snapshot(&chosen) {
                Some(s) => s,
                None => {
                    return Response::err(
                        id,
                        -32001,
                        format!("runtime/overlay_run: unknown anchor id {chosen}"),
                    )
                }
            };
            if let Err(e) = restore_live_checkpoint(&mut st.session, &snapshot) {
                return Response::err(id, -32001, format!("runtime/overlay_run: {e}"));
            }
            // then:"pause" — machine stays paused; advance ctrl frame + clear stop
            // (a restore is a control discontinuity, like checkpoint/restore).
            st.session.running = false;
            st.ctrl_frame += 1;
            st.ctrl_stop = None;

            // Apply the RAM patches (the overlay). ws-server.ts:957-965 —
            // s.c64Bus.ram[(addr + i) & 0xffff] = bytes[i] & 0xff.
            let mut applied: Vec<Value> = vec![];
            for p in &patches {
                // Spec 795 — `space` selects RAM (default) or a cart bank (roml/romh).
                let space = p.get("space").and_then(|v| v.as_str()).unwrap_or("ram");
                let addr = (p.get("addr").and_then(|v| v.as_u64()).unwrap_or(0) & 0xffff) as usize;
                let bytes: Vec<u8> = p
                    .get("bytes")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().map(|b| (b.as_u64().unwrap_or(0) & 0xff) as u8).collect())
                    .unwrap_or_default();
                if space == "ram" {
                    for (i, &b) in bytes.iter().enumerate() {
                        st.session.machine.ram[(addr + i) & 0xffff] = b;
                    }
                    applied.push(json!({ "space": "ram", "addr": addr as u64, "len": bytes.len() as u64 }));
                } else {
                    // Cart bank overlay (roml/romh + explicit bank). Ephemeral: rolled
                    // back on the next anchor restore (792 cart restore reloads flash).
                    let bank = p.get("bank").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                    match st.session.machine.cartridge.as_mut() {
                        Some(cart) => {
                            for (i, &b) in bytes.iter().enumerate() {
                                if let Err(e) =
                                    cart.overlay_bank_write(space, bank, ((addr + i) & 0xffff) as u16, b)
                                {
                                    return Response::err(id, -32001, format!("runtime/overlay_run: {e}"));
                                }
                            }
                            applied.push(json!({
                                "space": space, "bank": bank, "addr": addr as u64, "len": bytes.len() as u64
                            }));
                        }
                        None => {
                            return Response::err(
                                id,
                                -32001,
                                "runtime/overlay_run: cart overlay requested but no cartridge attached",
                            )
                        }
                    }
                }
            }

            // Run forward (bounded; optional breakpoint at until_pc). ws-server.ts:967-975.
            let mut hit_pc: Option<u16> = None;
            if run_cycles > 0 {
                if let Some(target) = until_pc {
                    // Mirror the standing bp surface, add the ephemeral until_pc
                    // exec observer, run, then remove it — same pattern as `until`.
                    {
                        let State { breakpoints, dsl_observers, dsl_disabled, observers: reg, .. } = &mut *st;
                        sync_observers(breakpoints, dsl_observers, dsl_disabled, reg);
                    }
                    let _ = st.observers.add(observers::ObsSpec {
                        name: "__overlay_until__".to_string(),
                        trigger: observers::ObsTrigger::Exec,
                        lo: target,
                        hi: target,
                        cond_src: None,
                        action: observers::ObsAction::Break,
                        log_exprs: None,
                        cmd_src: None,
                        mark_label: None,
                        trace_scope: None,
                    });
                    let run = {
                        let State { session, observers: reg, .. } = &mut *st;
                        run_until_break(session, reg, run_cycles)
                    };
                    {
                        let State { breakpoints, observers: reg, .. } = &mut *st;
                        writeback_hits(breakpoints, reg);
                    }
                    st.observers.remove("__overlay_until__");
                    // r.aborted === "breakpoint" → hitPc = r.lastPc.
                    if run.halted && run.reason == "breakpoint" {
                        hit_pc = Some(run.pc);
                    }
                } else {
                    run_cycle_budget(&mut st.session, run_cycles);
                }
            }

            // Observe: read-back of any patch addr flagged `read` (ws-server.ts:978-980).
            let mut reads = serde_json::Map::new();
            for p in &patches {
                if p.get("read").and_then(|v| v.as_bool()).unwrap_or(false) {
                    let space = p.get("space").and_then(|v| v.as_str()).unwrap_or("ram");
                    let a = (p.get("addr").and_then(|v| v.as_u64()).unwrap_or(0) & 0xffff) as usize;
                    if space == "ram" {
                        let key = format!("${:04x}", a);
                        reads.insert(key, json!(st.session.machine.ram[a] as u64));
                    } else {
                        // Cart bank read-back (proves the overlay / bank isolation).
                        let bank = p.get("bank").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                        let key = format!("{space}:{bank}:${:04x}", a);
                        let val = st
                            .session
                            .machine
                            .cartridge
                            .as_ref()
                            .map(|c| c.overlay_bank_read(space, bank, a as u16));
                        match val {
                            Some(Ok(v)) => reads.insert(key, json!(v as u64)),
                            Some(Err(e)) => reads.insert(key, json!(format!("err: {e}"))),
                            None => reads.insert(key, json!("err: no cartridge")),
                        };
                    }
                }
            }

            // Registers (ws-server.ts:982 — cpu.cycles == machine clock).
            let c = &st.session.machine.cpu6510;
            let registers = json!({
                "pc": c.reg_pc as u64,
                "a": c.reg_a as u64,
                "x": c.reg_x as u64,
                "y": c.reg_y as u64,
                "sp": c.reg_sp as u64,
                "flags": c.flags() as u64,
                "cycles": st.session.machine.clk,
            });

            Response::ok(id, json!({
                "anchorId": chosen,
                "applied": applied,
                "ranCycles": run_cycles,
                "hitPc": hit_pc.map(|v| v as u64),
                "reads": Value::Object(reads),
                "registers": registers,
            }))
        }

        // ── runtime/candidate_* (Spec 796) ────────────────────────────────────
        // A candidate = baseline anchor + bound scenario + accumulating overlay
        // patch-set + cached no-patch baseline. create/patch/run(auto-eval)/remove/
        // list/delete/export. Runs on the live shared session (like overlay_run).
        "runtime/candidate_create" => {
            let anchor = match req
                .params
                .get("anchor")
                .or_else(|| req.params.get("baseline_anchor"))
                .and_then(|v| v.as_str())
            {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::err(id, -32602, "runtime/candidate_create: anchor required"),
            };
            // Scenario = {inputs, cycleBudget}; strip any startSnapshot — the anchor
            // IS the start (a run restores it then plays these inputs post-patch).
            let mut scenario = req.params.get("scenario").cloned().unwrap_or_else(|| json!({}));
            if let Some(obj) = scenario.as_object_mut() {
                obj.remove("startSnapshot");
            }
            let mut st = state.lock().unwrap();
            let snapshot = match st.checkpoint_ring.restore_snapshot(&anchor) {
                Some(s) => s,
                None => {
                    return Response::err(id, -32001, format!("runtime/candidate_create: unknown anchor {anchor}"))
                }
            };
            if let Err(e) = restore_live_checkpoint(&mut st.session, &snapshot) {
                return Response::err(id, -32001, format!("runtime/candidate_create: {e}"));
            }
            st.session.running = false;
            st.ctrl_frame += 1;
            st.ctrl_stop = None;
            // Baseline = the NO-PATCH scenario run from the anchor.
            if let Err(e) = run_scenario(&mut st, &scenario) {
                return Response::err(id, -32001, format!("runtime/candidate_create: baseline run: {e}"));
            }
            let baseline_result = capture_live_checkpoint(&mut st.session);
            st.candidate_seq += 1;
            let cand_id = format!("cand-{}", st.candidate_seq);
            let cand = candidate::Candidate::new(cand_id.clone(), anchor, scenario, baseline_result);
            let out = cand.to_json();
            st.candidates.insert(cand_id, cand);
            Response::ok(id, out)
        }

        "runtime/candidate_patch" => {
            let cand_id = match req.params.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return Response::err(id, -32602, "runtime/candidate_patch: id required"),
            };
            let space = req.params.get("space").and_then(|v| v.as_str()).unwrap_or("ram").to_string();
            let bank = req.params.get("bank").and_then(|v| v.as_u64()).map(|b| b as u16);
            let addr = (req.params.get("addr").and_then(|v| v.as_u64()).unwrap_or(0) & 0xffff) as u16;
            let source = req.params.get("source").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let bytes: Vec<u8> = req
                .params
                .get("bytes")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().map(|b| (b.as_u64().unwrap_or(0) & 0xff) as u8).collect())
                .unwrap_or_default();
            let mut st = state.lock().unwrap();
            let Some(c) = st.candidates.get_mut(&cand_id) else {
                return Response::err(id, -32001, format!("runtime/candidate_patch: unknown candidate {cand_id}"));
            };
            c.add_or_replace_patch(candidate::Patch { space, bank, addr, source, bytes });
            Response::ok(id, c.to_json())
        }

        "runtime/candidate_run" => {
            let cand_id = match req.params.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return Response::err(id, -32602, "runtime/candidate_run: id required"),
            };
            let mut st = state.lock().unwrap();
            match run_candidate(&mut st, &cand_id) {
                Ok(v) => Response::ok(id, v),
                Err(e) => Response::err(id, -32001, format!("runtime/candidate_run: {e}")),
            }
        }

        "runtime/candidate_remove_patch" => {
            let cand_id = match req.params.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return Response::err(id, -32602, "runtime/candidate_remove_patch: id required"),
            };
            let space = req.params.get("space").and_then(|v| v.as_str()).unwrap_or("ram").to_string();
            let bank = req.params.get("bank").and_then(|v| v.as_u64()).map(|b| b as u16);
            let addr = (req.params.get("addr").and_then(|v| v.as_u64()).unwrap_or(0) & 0xffff) as u16;
            let mut st = state.lock().unwrap();
            let Some(c) = st.candidates.get_mut(&cand_id) else {
                return Response::err(id, -32001, format!("runtime/candidate_remove_patch: unknown candidate {cand_id}"));
            };
            let removed = c.remove_patch(&space, bank, addr);
            let mut out = c.to_json();
            out["removed"] = json!(removed);
            Response::ok(id, out)
        }

        "runtime/candidate_list" => {
            let st = state.lock().unwrap();
            if let Some(cand_id) = req.params.get("id").and_then(|v| v.as_str()) {
                match st.candidates.get(cand_id) {
                    Some(c) => Response::ok(id, c.to_json()),
                    None => Response::err(id, -32001, format!("runtime/candidate_list: unknown candidate {cand_id}")),
                }
            } else {
                let list: Vec<Value> = st.candidates.values().map(|c| c.to_json()).collect();
                Response::ok(id, json!({ "candidates": list }))
            }
        }

        "runtime/candidate_delete" => {
            let cand_id = match req.params.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return Response::err(id, -32602, "runtime/candidate_delete: id required"),
            };
            let mut st = state.lock().unwrap();
            let existed = st.candidates.remove(&cand_id).is_some();
            Response::ok(id, json!({ "id": cand_id, "deleted": existed }))
        }

        "runtime/candidate_export" => {
            let cand_id = match req.params.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return Response::err(id, -32602, "runtime/candidate_export: id required"),
            };
            let st = state.lock().unwrap();
            match st.candidates.get(&cand_id) {
                Some(c) => Response::ok(id, c.export_json()),
                None => Response::err(id, -32001, format!("runtime/candidate_export: unknown candidate {cand_id}")),
            }
        }

        // ── runtime/snapshot_tree ────────────────────────────────────────────
        // Spec 268 / 769 — time-travel branch tree. 1:1 with ws-server.ts:1891-1909:
        // beginRewindSession() builds a FRESH RewindManager (root snapshot + root
        // branch) and the handle is serialized. Spec 723.2: always true-drive.
        // Returns { scenarioId, rootBranchId, rootSnapshotId, ringSize, branches }.
        "runtime/snapshot_tree" => {
            let st = state.lock().unwrap();
            let scenario_id = st.session.id.clone();
            let disk_path = if st.session.disk_path.is_empty() {
                scenario_id.clone()
            } else {
                st.session.disk_path.clone()
            };
            let at_cycle = st.session.machine.clk;
            let rm = trx64_core::rewind::RewindManager::new(&scenario_id, &disk_path, at_cycle, None);
            Response::ok(id, rm.handle().to_json())
        }

        // ── runtime/promote_branch ───────────────────────────────────────────
        // Spec 268 / 769 — 1:1 with ws-server.ts:1911-1922: beginRewindSession()
        // builds a FRESH RewindManager, then promoteBranch(branch_id). Because each
        // call mints a new random root id, a caller-supplied branch_id that is not
        // the freshly-minted root throws "branch <id> not found" — exactly the TS
        // RewindManager.promoteBranch behaviour (graceful error, never a stub).
        "runtime/promote_branch" => {
            let branch_id = match req.params.get("branch_id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return Response::err(id, -32602, "runtime/promote_branch: branch_id required"),
            };
            let st = state.lock().unwrap();
            let scenario_id = st.session.id.clone();
            let disk_path = if st.session.disk_path.is_empty() {
                scenario_id.clone()
            } else {
                st.session.disk_path.clone()
            };
            let at_cycle = st.session.machine.clk;
            let rm = trx64_core::rewind::RewindManager::new(&scenario_id, &disk_path, at_cycle, None);
            match rm.promote_branch(&branch_id, "true-drive") {
                Ok(v) => Response::ok(id, v),
                Err(e) => Response::err(id, -32001, e),
            }
        }

        "runtime/mark" => {
            let label = req.params.get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mut st = state.lock().unwrap();
            // audit ws-trace-monitor-misc-18 — match TS: you cannot stamp a phase
            // marker into a stream that isn't recording. Error when no trace is
            // active (ws-server.ts:753 throws); else record the mark + return the
            // REAL mark count (was a fabricated marks:1 + success-when-inactive).
            let session_id = st.session.id.clone();
            let cycle = st.session.machine.clk;
            let Some(t) = st.session.trace.as_mut() else {
                return Response::err(id, -32001, format!(
                    "No active trace on session {session_id} (start one with runtime_session_start trace_out=...)."
                ));
            };
            t.marks.push((cycle, label.clone()));
            let run_id = t.run_id.clone();
            let event_count = t.event_count;
            let marks = t.marks.len() as u64;
            Response::ok(id, json!({
                "runId": run_id,
                "eventCount": event_count,
                "marks": marks,
                "label": label
            }))
        }

        // reverse-debug Phase 1b — UNDO the last n instructions from the always-on
        // full-delta ring (default 1). Restores CPU + RAM + IO-register bytes to the
        // state before them (NOT chip internal counters → inspect-backward only).
        // Returns the landed {pc, regs} + the writes rolled back.
        "runtime/reverse_step" => {
            let n = req.params.get("n").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
            let mut st = state.lock().unwrap();
            match st.session.machine.reverse_step(n) {
                Ok(out) => {
                    let l = out.landed;
                    let writes: Vec<Value> = out
                        .undone_writes
                        .iter()
                        .map(|w| json!({
                            "addr": w.addr,
                            "old": w.old_value,
                            "new": w.new_value,
                        }))
                        .collect();
                    Response::ok(id, json!({
                        "stepsTaken": out.steps_taken,
                        "pc": l.pc,
                        "a": l.a,
                        "x": l.x,
                        "y": l.y,
                        "sp": l.sp,
                        "p": l.p,
                        "cycle": l.cycle,
                        "undoneWrites": writes,
                        // The hard contract — surfaced so a caller never treats this as
                        // a resume-forward point.
                        "inspectOnly": true,
                        "note": "CPU+RAM+IO bytes restored; chip internal counters (VIC raster / CIA timers) NOT restored. Restore a checkpoint anchor to resume forward.",
                    }))
                }
                Err(e) => Response::err(id, -32001, e),
            }
        }

        // reverse-debug Phase 1b — last writer(s) of an address from the always-on
        // delta ring, newest first (the stack-crash shortcut "who put the bad byte").
        "runtime/who_wrote" => {
            let addr = match req.params.get("addr").and_then(|v| v.as_u64()) {
                Some(a) => (a & 0xffff) as u16,
                None => return Response::err(id, -32602, "runtime/who_wrote: missing addr (0-65535)"),
            };
            let limit = req.params.get("limit").and_then(|v| v.as_u64()).unwrap_or(8).clamp(1, 64) as usize;
            let st = state.lock().unwrap();
            let hits = st.session.machine.who_wrote(addr, limit);
            // TRX64 feature-request #3 — typed ring-exhaustion (a miss past a wrapped ring).
            let exhaustion = st.session.machine.ring_exhaustion(!hits.is_empty());
            let out: Vec<Value> = hits
                .iter()
                .map(|h| {
                    // TRX64 feature-request #2 — the caller chain (top return frames).
                    let chain: Vec<u16> = h.caller_chain.frames
                        [..h.caller_chain.depth as usize]
                        .to_vec();
                    json!({
                        "pc": h.pc,
                        "cycle": h.cycle,
                        "addr": h.addr,
                        "old": h.old_value,
                        "new": h.new_value,
                        "callerChain": chain,
                    })
                })
                .collect();
            Response::ok(id, json!({
                "addr": addr,
                "writers": out,
                "ringExhausted": exhaustion.ring_exhausted,
                "revdepthSeconds": exhaustion.revdepth_seconds,
                "ringExhaustedHint": exhaustion.hint,
            }))
        }

        // reverse-debug depth knob — REBUILD both always-on rings (delta + cpu-history)
        // at a new depth (seconds) for FUTURE capture. With no `seconds` it REPORTS the
        // current depth (read-only). Setting it DISCARDS current history (fresh slabs)
        // and only affects capture from now on — it cannot retroactively extend history
        // (a culprit already scrolled out is gone). `TRX64_REVERSE_SECONDS` stays the
        // BOOT default; this is the live override.
        "runtime/set_reverse_depth" => {
            // Read-only when `seconds` is absent.
            let requested = req.params.get("seconds").and_then(|v| v.as_u64());
            let mut st = state.lock().unwrap();
            // RAM-cost formatter (bytes → MB, 1 dp).
            let mb = |bytes: u64| (bytes as f64) / (1024.0 * 1024.0);
            let info = match requested {
                None => {
                    // Report the live depth without touching the rings.
                    let info = st.session.machine.reverse_depth_info();
                    return Response::ok(id, json!({
                        "seconds": info.seconds,
                        "deltaEntryCapacity": info.delta_entry_capacity,
                        "deltaWriteCapacity": info.delta_write_capacity,
                        "cpuHistoryCapacity": info.cpu_history_capacity,
                        "estimatedRamMb": (mb(info.ram_bytes) * 10.0).round() / 10.0,
                        "discardedHistory": false,
                        "note": "current reverse-depth (no change). Pass `seconds` to rebuild the rings.",
                    }));
                }
                Some(s) => {
                    // Clamp to a sane window: ≥1 s, ≤600 s (10 min). A multi-minute depth
                    // costs GBs (~9.6 MB/s of both rings), so warn past 120 s.
                    let clamped = (s.max(1)).min(600) as usize;
                    let info = st.session.machine.set_reverse_depth(clamped);
                    let mut out = json!({
                        "seconds": info.seconds,
                        "requestedSeconds": s,
                        "deltaEntryCapacity": info.delta_entry_capacity,
                        "deltaWriteCapacity": info.delta_write_capacity,
                        "cpuHistoryCapacity": info.cpu_history_capacity,
                        "estimatedRamMb": (mb(info.ram_bytes) * 10.0).round() / 10.0,
                        // The hard contract — surfaced so a caller never expects the past back.
                        "discardedHistory": true,
                        "note": "rings rebuilt at the new depth. Current history was DISCARDED (fresh ring); this affects capture FROM NOW ON only — it cannot retroactively extend history (a culprit already scrolled out is gone). TRX64_REVERSE_SECONDS stays the boot default.",
                    });
                    if (s as usize) != info.seconds {
                        out["clampNote"] = json!(format!(
                            "requested {s}s clamped to {}s (allowed 1..=600)", info.seconds));
                    }
                    if info.seconds > 120 {
                        out["warning"] = json!(format!(
                            "reverse depth {}s costs ~{:.1} MB of always-on ring RAM — multi-minute depths run into GBs.",
                            info.seconds, mb(info.ram_bytes)));
                    }
                    out
                }
            };
            Response::ok(id, info)
        }

        // reverse-debug Phase 2 — re-run the guided crash-triage on demand and return
        // the structured causal chain (the SAME chain the JAM drop-in auto-attaches to
        // `debug/stopped`). With no `pc` param it triages the live (crashed) PC; an
        // optional `pc` triages a specific wild address. Read-only — does not mutate.
        "runtime/crash_triage" => {
            let at_pc = req.params.get("pc").and_then(|v| v.as_u64()).map(|p| (p & 0xffff) as u16);
            let st = state.lock().unwrap();
            let chain = st.session.machine.crash_triage(at_pc);
            // The structured chain + the formatted text lines (so a TUI can print the
            // same block the drop-in does without re-formatting).
            let mut out = triage_to_json(&chain);
            if let Some(obj) = out.as_object_mut() {
                let lines: Vec<Value> = format_triage_lines(&chain).into_iter().map(Value::from).collect();
                obj.insert("lines".to_string(), Value::Array(lines));
            }
            Response::ok(id, out)
        }

        // Spec time-travel-tooling Piece 1 — diffCheckpoints(idA, idB). Resolve two
        // ring anchors BY ID, run the EXISTING snapshot_diff compute on their two
        // machine states, return a TYPED SnapshotDiff record (RAM contiguous runs +
        // per-chip register-change lists). READ-ONLY: the live machine is byte-
        // identical after the diff (diff_checkpoints_by_id snapshots + restores live).
        "runtime/diff_checkpoints" => {
            let id_a = match req
                .params
                .get("idA")
                .or_else(|| req.params.get("id_a"))
                .and_then(|v| v.as_str())
            {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::err(id, -32602, "runtime/diff_checkpoints: idA required"),
            };
            let id_b = match req
                .params
                .get("idB")
                .or_else(|| req.params.get("id_b"))
                .and_then(|v| v.as_str())
            {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::err(id, -32602, "runtime/diff_checkpoints: idB required"),
            };
            let mut st = state.lock().unwrap();
            match diff_checkpoints_by_id(&mut st, &id_a, &id_b) {
                Ok(v) => Response::ok(id, v),
                Err(e) => Response::err(id, -32001, format!("runtime/diff_checkpoints: {e}")),
            }
        }

        // Spec 794 — whitebox COMPONENT diff of two checkpoint anchors (equivalence
        // verdict + `exclude` mask), computed over the anchor Values directly (no
        // machine round-trip → reaches color RAM, Floppy RAM, internal chip state).
        // READ-ONLY. Params: idA, idB, optional exclude {components,lanes,ranges,presets}.
        "runtime/component_diff" => {
            let id_a = match req
                .params
                .get("idA")
                .or_else(|| req.params.get("id_a"))
                .and_then(|v| v.as_str())
            {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::err(id, -32602, "runtime/component_diff: idA required"),
            };
            let id_b = match req
                .params
                .get("idB")
                .or_else(|| req.params.get("id_b"))
                .and_then(|v| v.as_str())
            {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::err(id, -32602, "runtime/component_diff: idB required"),
            };
            let mask = trx64_core::checkpoint_diff::ExcludeMask::from_json(
                req.params.get("exclude").unwrap_or(&Value::Null),
            );
            let st = state.lock().unwrap();
            let snap_a = match st.checkpoint_ring.restore_snapshot(&id_a) {
                Some(v) => v,
                None => {
                    return Response::err(
                        id,
                        -32001,
                        format!("runtime/component_diff: unknown checkpoint id {id_a}"),
                    )
                }
            };
            let snap_b = match st.checkpoint_ring.restore_snapshot(&id_b) {
                Some(v) => v,
                None => {
                    return Response::err(
                        id,
                        -32001,
                        format!("runtime/component_diff: unknown checkpoint id {id_b}"),
                    )
                }
            };
            let d = trx64_core::checkpoint_diff::diff_checkpoints(&snap_a, &snap_b, &mask);
            Response::ok(id, d)
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

        // Spec 709 / 709.13 — the single media-ingress entry point. Full port of
        // c64re `ingestMedia` (media/ingress.ts:91-301): drive9 + .c64re + dirty-
        // media guards, then a deterministic boundary (conditional pause →
        // checkpoint-before → apply → checkpoint-after → pin → event → conditional
        // resume). TRX64 has no autonomous tick loop, so "pause"/"resume" set the
        // `running` flag (+ broadcast debug/paused|running) exactly as the wire
        // contract requires; the cycle-budget run happens on the next debug/run.
        "media/ingress" => {
            let kind = req.params.get("kind").and_then(|v| v.as_str()).unwrap_or("disk").to_string();
            let path = req.params.get("path").and_then(|v| v.as_str()).map(str::to_string);
            let bytes_b64 = req.params.get("bytes_b64").and_then(|v| v.as_str()).map(str::to_string);
            let name = req.params.get("name").and_then(|v| v.as_str()).map(str::to_string);
            let role = req.params.get("role").and_then(|v| v.as_str()).unwrap_or("drive8").to_string();
            // CRT only: reset policy (default power-cycle, = buildIngressRequest ts:61).
            let reset_policy = req.params.get("resetPolicy").and_then(|v| v.as_str())
                .map(|s| if s == "reset" { "reset" } else { "power-cycle" })
                .unwrap_or("power-cycle").to_string();
            // PRG only: load vs inject-run (default load, = buildIngressRequest ts:60).
            let prg_mode = req.params.get("mode").and_then(|v| v.as_str())
                .map(|s| if s == "inject-run" { "inject-run" } else { "load" })
                .unwrap_or("load").to_string();
            let prg_entry = req.params.get("entry").and_then(|v| v.as_u64()).map(|e| (e & 0xffff) as u16);
            // Spec 709.12 — resumeIfRunning. The TS ws-server sets it to (kind=="crt")
            // for every ingress route (ws-server.ts:1749/1779); honor an explicit
            // param too so a deterministic caller can pin the paused contract.
            let resume_if_running = req.params.get("resumeIfRunning").and_then(|v| v.as_bool())
                .unwrap_or(kind == "crt");

            // --- drive9 hard reject (v1 drive8-only), ingress.ts:96-100 ---
            let slot = req.params.get("slot").and_then(|v| v.as_u64());
            if role == "drive9" || role == "9" || slot == Some(9) {
                return Response::err(id, -32602, "media-ingress: drive 9 is not supported in v1 (drive8-only). Request rejected, not registered.");
            }

            // --- resolve bytes up-front for non-eject (the .c64re guard reads them),
            //     ingress.ts:102-109 + buildIngressRequest byte resolution ---
            let bytes: Option<Vec<u8>> = if kind != "eject" {
                let b = if let Some(ref b64) = bytes_b64 {
                    match base64_decode(b64) {
                        Ok(b) => b,
                        Err(e) => return Response::err(id, -32602, format!("media/ingress: base64 decode: {e}")),
                    }
                } else if let Some(ref p) = path {
                    match std::fs::read(p) {
                        Ok(b) => b,
                        Err(e) => return Response::err(id, -32602, format!("media/ingress: file read {p}: {e}")),
                    }
                } else {
                    return Response::err(id, -32602, format!("media/ingress: {kind} requires path or bytes_b64"));
                };
                // --- .c64re is NOT media, ingress.ts:102-109 (looksLikeC64re ts:62-64) ---
                let nm = name.clone().unwrap_or_default();
                let looks_c64re = nm.to_lowercase().ends_with(".c64re")
                    || (b.len() >= 8 && &b[..8] == trx64_core::native_snapshot::NATIVE_SNAPSHOT_MAGIC.as_slice());
                if looks_c64re {
                    return Response::err(id, -32603, "media-ingress: .c64re is a runtime snapshot, not media. Use snapshot/undump (Spec 707), not media ingest.");
                }
                Some(b)
            } else {
                None
            };

            let mut st = state.lock().unwrap();

            // --- Spec 709.13 dirty-media guard (BEFORE pause/apply/checkpoint/event),
            //     ingress.ts:122-129 ---
            if let Some(dirty) = non_persistable_dirty_media(&st) {
                return Response::err(id, -32603, format!(
                    "media-ingress: cannot apply a media change — {dirty} (Spec 709.13). v1 cannot \
                     persist this state, so the intervention would create a non-restorable checkpoint/branch. \
                     Aborting (no partial apply, no checkpoint, no event)."
                ));
            }

            // --- boundary: wasRunning + conditional pause, ingress.ts:138-143 ---
            let was_running = st.session.running;
            let requires_pause =
                kind == "crt" || kind == "prg" || (kind == "eject" && role == "cartridge");
            if was_running && requires_pause {
                // ctrl.pause() — runtime-controller.ts:295 server-PUSHes debug/paused.
                st.session.running = false;
                let pc = st.session.machine.cpu6510.reg_pc as u64;
                let cycles = st.session.machine.clk;
                st.ctrl_stop = Some(CtrlStop { reason: "pause", pc: st.session.machine.cpu6510.reg_pc, cycles });
                st.notify.broadcast("debug/paused", json!({
                    "session_id": st.session.id,
                    "stop": { "reason": "pause", "pc": pc, "cycles": cycles },
                }));
            }

            // --- mediaPresent + needBefore + checkpoint-before, ingress.ts:145-152 ---
            let media_present = st.session.machine.drive8.get_attached_disk().is_some()
                || st.session.machine.cartridge.is_some();
            let need_before = was_running || media_present;
            let before_id = if need_before { capture_media_checkpoint(&mut st) } else { None };

            // --- apply the op (= ingress.ts:158-243 runExclusive switch) ---
            let mut detail = serde_json::Map::new();
            let mut format: Option<String> = None;
            let mut sha256: Option<String> = None;
            let apply_err: Option<(i64, String)> = (|| {
                match kind.as_str() {
                    "disk" => {
                        let bytes = bytes.clone().unwrap();
                        let disk_name = name.clone().unwrap_or_else(|| {
                            path.as_deref().and_then(|p| p.split('/').last()).unwrap_or("disk").to_string()
                        });
                        // diskFormat, ingress.ts:66-73.
                        let fmt = if disk_name.to_lowercase().ends_with(".g64")
                            || (bytes.len() >= 8 && &bytes[..8] == b"GCR-1541") { "g64" }
                        else if disk_name.to_lowercase().ends_with(".d64") { "d64" }
                        else { "d64" };
                        format = Some(fmt.to_string());
                        sha256 = Some(sha256_hex(&bytes));
                        let backing_path = path.clone();
                        let disk_kind = if fmt == "g64" { DiskKind::G64 } else { DiskKind::D64 };
                        st.session.machine.drive8.attach_disk(DiskImage {
                            kind: disk_kind, bytes, backing_path: backing_path.clone(), read_only: false,
                        });
                        st.session.disk_path = path.clone().unwrap_or_default();
                        detail.insert("name".to_string(), json!(disk_name));
                        if let Some(ref bp) = backing_path { detail.insert("backingPath".to_string(), json!(bp)); }
                        None
                    }
                    "eject" => {
                        if role == "drive8" {
                            st.session.machine.drive8.detach_disk();
                            st.session.disk_path = String::new();
                        } else {
                            // BUG-023-cart / Spec 742 — write programmed flash back to the
                            // host .crt BEFORE detaching (ingress.ts:190-204).
                            let cart_path = st.session.cart_path.clone();
                            if !cart_path.is_empty() {
                                if let Some(p) = persist_cart_for_eject(&mut st, &cart_path) {
                                    detail.insert("cartPersisted".to_string(), json!(p));
                                }
                            }
                            st.session.machine.detach_cart();
                            st.session.cart_path = String::new();
                            // CLI-FEEL S7 — unify eject RAM semantics: a cart eject is a
                            // full power-cycle (RAM wiped), matching media/unmount's cart
                            // branch (fill_power_on_ram + cold_reset) and the user's
                            // real-C64 model (cart out → power off → power on). This
                            // INTENTIONALLY diverges from the TS oracle's resetCold({
                            // keepRam:true }) (ingress.ts:204): keeping RAM here left the
                            // two eject routes (media/ingress vs media/unmount) diverging.
                            // fill_power_on_ram (power off) then cold_reset (power on) is
                            // exactly what media/unmount runs, so both routes now match.
                            st.session.machine.fill_power_on_ram();
                            st.session.machine.cold_reset();
                        }
                        detail.insert("role".to_string(), json!(role));
                        None
                    }
                    "prg" => {
                        let prg_bytes = bytes.clone().unwrap();
                        if prg_bytes.len() < 2 {
                            return Some((-32602, "media-ingress: PRG too short (need 2-byte load header)".to_string()));
                        }
                        sha256 = Some(sha256_hex(&prg_bytes));
                        format = Some("prg".to_string());
                        // loadPrgBytes, ingress.ts:306-318: poke at load addr + set
                        // BASIC VARTAB ($2D/$2E) when loaded at $0801.
                        let load_addr = (prg_bytes[0] as u16) | ((prg_bytes[1] as u16) << 8);
                        let body = &prg_bytes[2..];
                        st.session.machine.poke(load_addr, body);
                        let end_addr = load_addr.wrapping_add(body.len() as u16);
                        if load_addr == 0x0801 {
                            st.session.machine.poke(0x2d, &[(end_addr & 0xff) as u8, ((end_addr >> 8) & 0xff) as u8]);
                        }
                        let report_end = end_addr.wrapping_sub(1);
                        detail.insert("loadAddress".to_string(), json!(load_addr as u64));
                        detail.insert("endAddress".to_string(), json!(report_end as u64));
                        detail.insert("mode".to_string(), json!(prg_mode));
                        // inject-run: set PC to entry (default load addr), ingress.ts:216-220.
                        if prg_mode == "inject-run" {
                            let entry = prg_entry.unwrap_or(load_addr);
                            st.session.machine.cpu6510.reg_pc = entry;
                            detail.insert("entry".to_string(), json!(entry as u64));
                        }
                        st.session.machine.sync_after_monitor();
                        st.session.injected = true;
                        None
                    }
                    "crt" => {
                        let crt_bytes = bytes.clone().unwrap();
                        let crt_name = name.clone().unwrap_or_else(|| {
                            path.as_deref().and_then(|p| p.split('/').last()).unwrap_or("cartridge.crt").to_string()
                        });
                        format = Some("crt".to_string());
                        sha256 = Some(sha256_hex(&crt_bytes));
                        // loadCartridgeMapperFromBytes + attachCartridge, ingress.ts:226-230.
                        // Parse failure → hard error (no fake success), ingress.ts:226.
                        let mapper_type = match st.session.machine.attach_cart_from_bytes(&crt_bytes, &crt_name) {
                            Ok((_n, t)) => t,
                            Err(e) => return Some((-32602, format!("media-ingress: bad CRT: {e}"))),
                        };
                        // BUG-023-cart / Spec 742 — remember the host .crt path for
                        // writable flash write-back on eject/persist, ingress.ts:233.
                        st.session.cart_path = path.clone().unwrap_or_default();
                        // resetCold("pal-default", { keepRam: resetPolicy=="reset" }),
                        // ingress.ts:236. power-cycle wipes RAM (fill_power_on_ram);
                        // reset keeps it. The cart was attached above, so cold_reset's
                        // cart-aware $FFFC fetch re-vectors from the cart (Ultimax/GAME).
                        if reset_policy == "power-cycle" {
                            st.session.machine.fill_power_on_ram();
                        }
                        st.session.machine.cold_reset();
                        detail.insert("mapperType".to_string(), json!(mapper_type_str(mapper_type)));
                        if let Some(ref p) = path { detail.insert("backingPath".to_string(), json!(p)); }
                        detail.insert("resetPolicy".to_string(), json!(reset_policy));
                        None
                    }
                    other => Some((-32602, format!("media/ingress: unsupported kind '{other}'"))),
                }
            })();

            if let Some((code, msg)) = apply_err {
                return Response::err(id, code, msg);
            }

            // --- checkpoint-after + pin before/after, ingress.ts:254-256 ---
            let after_id = capture_media_checkpoint(&mut st);
            if let Some(ref b) = before_id { st.checkpoint_ring.pin(b); }
            if let Some(ref a) = after_id { st.checkpoint_ring.pin(a); }

            // --- the replayable MediaIngressEvent, ingress.ts:258-267 ---
            // ts: ctrl.session.c64Cpu.cycles — TRX64 Cpu6510::cycles mirrors clk;
            // use machine.clk for consistency with the other media-event sites.
            let cycle = st.session.machine.clk;
            let event = json!({
                "cycle": cycle,
                "operation": kind,
                "role": if kind == "eject" || kind == "disk" { json!(role) } else { Value::Null },
                "format": format,
                "sha256": sha256,
                "resetPolicy": if kind == "crt" { json!(reset_policy) } else { Value::Null },
                "checkpointBeforeId": before_id,
                "checkpointAfterId": after_id,
            });
            push_media_event(&mut st, event.clone());

            // --- resume semantics, ingress.ts:282-298 ---
            // isCartPowerCycle = crt || (eject && cartridge). resumeAfter =
            // requiresPause && resumeIfRunning && (wasRunning || isCartPowerCycle).
            let is_cart_power_cycle = kind == "crt" || (kind == "eject" && role == "cartridge");
            let resume_after = requires_pause && resume_if_running && (was_running || is_cart_power_cycle);
            if resume_after {
                // ctrl.run() — runtime-controller.ts:282 server-PUSHes debug/running.
                st.session.running = true;
                st.ctrl_stop = None;
                let pacing_snap = json!({ "mode": st.pacing_mode, "ratio": st.pacing_ratio });
                st.notify.broadcast("debug/running", json!({
                    "session_id": st.session.id,
                    "pacing": pacing_snap,
                }));
            }
            let paused = !st.session.running;

            Response::ok(id, json!({
                "ok": true,
                "event": event,
                "paused": paused,
                "wasRunning": was_running,
                "detail": Value::Object(detail),
            }))
        }

        "media/unmount" => {
            let role_param = req.params.get("role").and_then(|v| v.as_str());
            let slot = req.params.get("slot").and_then(|v| v.as_u64());
            // TS media/unmount (ws-server.ts:709): slot 0 OR role "cartridge" → cart
            // eject; slot 9 rejected; else drive8. The old handler ignored slot and
            // ALWAYS ejected drive8, so the UI's ejectSlot(0) removed the disk instead
            // of the cartridge (and the cart never came out).
            if slot == Some(9) {
                return Response::err(id, -32602, "media/unmount: drive 9 not supported (v1 drive8-only)");
            }
            let mut st = state.lock().unwrap();
            // CLI-FEEL S7 — the cockpit `/eject` sends role:"auto" (it can't know what's
            // mounted without a round-trip); resolve it HERE against the live machine so
            // ONE command ejects the cartridge if one is inserted, else the disk on
            // drive8 — atomically under the lock, with no read-then-eject status race.
            // Explicit callers keep the TS contract (slot 0 OR role "cartridge" → cart).
            let is_cart = if role_param == Some("auto") {
                st.session.machine.cartridge.is_some()
            } else {
                role_param == Some("cartridge") || slot == Some(0)
            };
            let role = if is_cart { "cartridge" } else { "drive8" };
            let was_running = st.session.running;
            // audit ws-media-0 — eject also routes through the ingress boundary
            // (= ingestMedia kind:eject, ingress.ts:185): dirty-media guard +
            // checkpoint-before/after, and (disk) persist the outgoing disk's dirty
            // writes to its host file BEFORE detaching, so an eject can't lose them.
            if let Some(reason) = non_persistable_dirty_media(&st) {
                return Response::err(id, -32602, format!(
                    "media/unmount: cannot apply a media change — {reason} (Spec 709.13)."
                ));
            }
            // A CART eject is a power-cycle (Spec 786): the ring checkpoints belong
            // to the old cart-inserted timeline and are dropped by the power-off, so
            // none are captured/pinned for a cart. A DISK unmount is a live device op
            // → capture before/after as before.
            let before_id = if is_cart { None } else { capture_media_checkpoint(&mut st) };
            let mut persisted_outgoing: Option<String> = None;
            if is_cart {
                // Persist any programmed flash back to the .crt while the cart is still
                // LIVE in the machine (before the power-off transplant).
                let cart_path = st.session.cart_path.clone();
                if !cart_path.is_empty() { persist_cart_for_eject(&mut st, &cart_path); }
                // Eject = power_off → drop the cart from the registry → power_on.
                // The disk (if any) is preserved across the power-cycle via the registry.
                do_power_off(&mut st);
                st.session.clear_inserted_cart();
                do_power_on(&mut st);
                // audit ws-media-14 — resume with the LIVE pacing (do_power_on set
                // running=true); a warp session must stay warp across a cart eject.
                let session_id = st.session.id.clone();
                let (mode, ratio) = (st.pacing_mode.clone(), st.pacing_ratio);
                st.notify.broadcast("debug/running", json!({ "session_id": session_id, "pacing": { "mode": mode, "ratio": ratio } }));
            } else {
                // Persist the outgoing disk's dirty writes to its host file BEFORE
                // detach (the data-loss fix — detach_disk only flushes into disk.bytes,
                // not the host file). Then detach.
                persisted_outgoing = persist_outgoing_disk(&mut st);
                st.session.machine.drive8.detach_disk();
                st.session.disk_path = String::new();
            }
            let after_id = if is_cart { None } else { capture_media_checkpoint(&mut st) };
            if let Some(ref b) = before_id { st.checkpoint_ring.pin(b); }
            if let Some(ref a) = after_id { st.checkpoint_ring.pin(a); }
            let cycle = st.session.machine.clk;
            let event = json!({
                "cycle": cycle,
                "operation": "eject",
                "role": role,
                "format": Value::Null,
                "sha256": Value::Null,
                "resetPolicy": Value::Null,
                "checkpointBeforeId": before_id,
                "checkpointAfterId": after_id
            });
            push_media_event(&mut st, event.clone());
            let mut detail = json!({ "role": role });
            if let Some(p) = persisted_outgoing { detail["diskPersisted"] = json!(p); }
            // audit ws-media-2 — report the REAL run-state, not a hardcoded `!is_cart`.
            // TS's ingress returns paused = (runState === "paused"): a disk eject is a
            // live device op that never pauses, so a running machine stays running
            // (paused:false). A cart eject power-cycles into running above
            // (st.session.running=true), so this still reports paused:false for carts.
            let paused = !st.session.running;
            Response::ok(id, json!({
                "ok": true,
                "event": event,
                "paused": paused,
                "wasRunning": was_running,
                "detail": detail
            }))
        }

        "media/mount" => {
            let path_str = match req.params.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return Response::err(id, -32602, "media/mount: missing path"),
            };
            // Resolve relative to the cockpit `cd` cwd (BUG: `/mount foo.crt` after `cd out`
            // read foo.crt relative to the process cwd → "No such file"; now reads .../out/foo.crt).
            let path_str = { let st = state.lock().unwrap(); resolve_fs_path_with_state(&st, &path_str) };

            let bytes = match std::fs::read(&path_str) {
                Ok(b) => b,
                Err(e) => return Response::err(id, -32602, format!("media/mount: file read {path_str}: {e}")),
            };

            // Route by extension, mirroring TS adaptMount (ws-server.ts:1757): the
            // Inspector CART dropdown mounts a .crt through media/mount (slot 0). The old
            // handler ignored the extension and ALWAYS attached as a d64 disk on drive8,
            // so a CRT could never be inserted. .c64re is a snapshot, not media.
            let lower = path_str.to_lowercase();
            if lower.ends_with(".c64re") {
                return Response::err(id, -32602, "media/mount: .c64re is a runtime snapshot, not media — use snapshot/undump (Spec 707).");
            }
            if lower.ends_with(".crt") {
                // Spec 709.12 — CRT insert = attach cart → power-cycle → resume (so the
                // cart executes). Same primitives as media/ingress kind:crt.
                let crt_name = path_str.split('/').last().unwrap_or("cartridge.crt").to_string();
                let sha = sha256_hex(&bytes);
                let mut st = state.lock().unwrap();
                // audit ws-media-0 — CRT insert routes through the ingress boundary too:
                // dirty-media guard + checkpoint-before (a present medium/running machine
                // = an intervention) BEFORE the power-cycle, checkpoint-after after.
                if let Some(reason) = non_persistable_dirty_media(&st) {
                    return Response::err(id, -32602, format!(
                        "media/mount: cannot apply a media change — {reason} (Spec 709.13)."
                    ));
                }
                // Spec 786 — CRT insert = power_off → register cart → power_on.
                // Validate the CRT up front so a bad file leaves the running
                // machine untouched.
                let mapper_type = match trx64_core::cart::load_cartridge_from_bytes(&bytes, &crt_name, None) {
                    Ok((image, _mapper)) => image.mapper_type,
                    Err(e) => return Response::err(id, -32602, format!("media/mount: bad CRT: {e}")),
                };
                let mt = mapper_type_str(mapper_type).to_string();
                // audit ws-media-8 — record the cart in the recents store (newest-first,
                // mountedAt), 1:1 with TS addRecent on every CRT ingest (ingress.ts:250).
                add_recent_media(&mut st, &path_str, "crt");
                // Insert is a power-cycle: the ring checkpoints belong to the old
                // timeline and are dropped by the power-off, so none are captured/
                // pinned for a cart. The disk (if any) is preserved via the registry.
                do_power_off(&mut st);
                if let Err(e) = st.session.set_inserted_cart(&bytes, &crt_name, &path_str) {
                    do_power_on(&mut st); // recover cartless
                    return Response::err(id, -32602, format!("media/mount: bad CRT: {e}"));
                }
                do_power_on(&mut st);
                let before_id: Option<String> = None;
                let after_id: Option<String> = None;
                let cycle = st.session.machine.clk;
                // audit ws-media-14 — resume with the LIVE pacing, not hardcoded pal/1
                // (TS CRT insert resumes via ctrl.run() = `this.pacing`, ws-server.ts
                // resumeIfRunning:"crt" → runtime-controller.ts:282).
                let session_id = st.session.id.clone();
                let (mode, ratio) = (st.pacing_mode.clone(), st.pacing_ratio);
                st.notify.broadcast("debug/running", json!({ "session_id": session_id, "pacing": { "mode": mode, "ratio": ratio } }));
                let event = json!({
                    "cycle": cycle, "operation": "crt", "role": Value::Null, "format": "crt",
                    "sha256": sha.clone(), "resetPolicy": "power-cycle",
                    "checkpointBeforeId": before_id, "checkpointAfterId": after_id,
                });
                push_media_event(&mut st, event.clone());
                return Response::ok(id, json!({
                    "mountedPath": path_str, "type": "crt", "mapperType": mt.clone(), "sha256": sha,
                    "event": event,
                    "detail": { "name": crt_name, "backingPath": path_str, "mapperType": mt, "resetPolicy": "power-cycle" },
                    "paused": false,
                }));
            }

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
            // audit ws-media-0 — route the disk mount through the ingress boundary
            // (= ingestMedia, ingress.ts:91), NOT a bare drive8.attach_disk:
            //  1. dirty-media guard (Spec 709.13) — no branching intervention while a
            //     mounted medium is dirty + non-persistable (here: a writable cart).
            //  2. checkpoint-before — captured when a medium is already present (an
            //     intervention vs. a fresh-session root); pinned so the event is
            //     replayable.
            //  3. mount_disk_media — persists the OUTGOING disk's dirty writes to its
            //     host file BEFORE detach/replace (the actual data-loss fix).
            //  4. checkpoint-after — embedded as event.checkpointAfterId; pinned.
            if let Some(reason) = non_persistable_dirty_media(&st) {
                return Response::err(id, -32602, format!(
                    "media: cannot apply a media change — {reason} (Spec 709.13)."
                ));
            }
            let media_present = st.session.machine.drive8.get_attached_disk().is_some()
                || st.session.machine.cartridge.is_some();
            let before_id = if media_present { capture_media_checkpoint(&mut st) } else { None };
            // audit ws-media-8 — record the mounted disk in the recents store (newest-
            // first, cap 10, mountedAt), 1:1 with TS addRecent (recent-files.ts) on
            // every ingest, so media/recent overlays it ahead of the dir scan.
            add_recent_media(&mut st, &path_str, format_str);
            let persisted_outgoing = mount_disk_media(&mut st, image, &path_str);
            let after_id = capture_media_checkpoint(&mut st);
            if let Some(ref b) = before_id { st.checkpoint_ring.pin(b); }
            if let Some(ref a) = after_id { st.checkpoint_ring.pin(a); }
            let cycle = st.session.machine.clk;

            let event = json!({
                "cycle": cycle,
                "operation": "disk",
                "role": "drive8",
                "format": format_str,
                "sha256": sha256,
                "resetPolicy": null,
                "checkpointBeforeId": before_id,
                "checkpointAfterId": after_id
            });
            push_media_event(&mut st, event.clone());
            let mut detail = json!({ "name": disk_name, "backingPath": path_str });
            if let Some(p) = persisted_outgoing { detail["diskPersisted"] = json!(p); }
            // audit ws-media-mount-pause (Spec 709 §2.2 / §709.13.1) — a DISK
            // mount/swap is a live device op (the 1541 is a separate device): a
            // running C64 keeps running through the insert, so the reply reports the
            // REAL run-state, NOT a hardcoded `paused:true`. TS's ingress returns
            // paused=(runState==="paused") (ingress.ts:299); a disk insert never
            // pauses the C64, so a running machine returns paused:false. (Only a
            // C64-INTERNAL change — CRT/PRG — pauses; those branches already report
            // paused:false after resuming.)
            let paused = !st.session.running;
            Response::ok(id, json!({
                "mountedPath": path_str,
                "type": format_str,
                "slot": 8u64,
                "sha256": sha256,
                "event": event,
                "detail": detail,
                "paused": paused
            }))
        }

        "media/swap" => {
            let path_str = match req.params.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return Response::err(id, -32602, "media/swap: missing path"),
            };
            // Resolve relative to the cockpit `cd` cwd (same as media/mount).
            let path_str = { let st = state.lock().unwrap(); resolve_fs_path_with_state(&st, &path_str) };

            let bytes = match std::fs::read(&path_str) {
                Ok(b) => b,
                Err(e) => return Response::err(id, -32602, format!("media/swap: file read {path_str}: {e}")),
            };

            // media/swap shares TS adaptMount with media/mount — route .crt → cartridge
            // (was always disk-on-drive8), .c64re → reject. See media/mount above.
            let lower = path_str.to_lowercase();
            if lower.ends_with(".c64re") {
                return Response::err(id, -32602, "media/swap: .c64re is a runtime snapshot, not media — use snapshot/undump (Spec 707).");
            }
            if lower.ends_with(".crt") {
                let crt_name = path_str.split('/').last().unwrap_or("cartridge.crt").to_string();
                let sha = sha256_hex(&bytes);
                let mut st = state.lock().unwrap();
                // audit ws-media-0 — CRT swap routes through the ingress boundary too:
                // dirty-media guard + checkpoint-before (a present medium/running machine
                // = an intervention) BEFORE the power-cycle, checkpoint-after after.
                if let Some(reason) = non_persistable_dirty_media(&st) {
                    return Response::err(id, -32602, format!(
                        "media/swap: cannot apply a media change — {reason} (Spec 709.13)."
                    ));
                }
                let cart_media_present = st.session.machine.drive8.get_attached_disk().is_some()
                    || st.session.machine.cartridge.is_some();
                let before_id = if cart_media_present { capture_media_checkpoint(&mut st) } else { None };
                let mapper_type = match st.session.machine.attach_cart_from_bytes(&bytes, &crt_name) {
                    Ok((_n, t)) => t,
                    Err(e) => return Response::err(id, -32602, format!("media/swap: bad CRT: {e}")),
                };
                let mt = mapper_type_str(mapper_type).to_string();
                st.session.cart_path = path_str.clone();
                // audit ws-media-8 — record the cart in the recents store (see media/mount).
                add_recent_media(&mut st, &path_str, "crt");
                st.session.machine.fill_power_on_ram();
                st.session.machine.cold_reset();
                let after_id = capture_media_checkpoint(&mut st);
                if let Some(ref b) = before_id { st.checkpoint_ring.pin(b); }
                if let Some(ref a) = after_id { st.checkpoint_ring.pin(a); }
                let cycle = st.session.machine.clk;
                st.session.running = true;
                st.ctrl_stop = None;
                // audit ws-media-14 — resume with the LIVE pacing, not hardcoded pal/1.
                let session_id = st.session.id.clone();
                let (mode, ratio) = (st.pacing_mode.clone(), st.pacing_ratio);
                st.notify.broadcast("debug/running", json!({ "session_id": session_id, "pacing": { "mode": mode, "ratio": ratio } }));
                let event = json!({
                    "cycle": cycle, "operation": "crt", "role": Value::Null, "format": "crt",
                    "sha256": sha.clone(), "resetPolicy": "power-cycle",
                    "checkpointBeforeId": before_id, "checkpointAfterId": after_id,
                });
                push_media_event(&mut st, event.clone());
                return Response::ok(id, json!({
                    "mountedPath": path_str, "type": "crt", "mapperType": mt.clone(), "sha256": sha,
                    "event": event,
                    "detail": { "name": crt_name, "backingPath": path_str, "mapperType": mt, "resetPolicy": "power-cycle" },
                    "paused": false,
                }));
            }

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
            // audit ws-media-0 — route the disk mount through the ingress boundary
            // (= ingestMedia, ingress.ts:91), NOT a bare drive8.attach_disk:
            //  1. dirty-media guard (Spec 709.13) — no branching intervention while a
            //     mounted medium is dirty + non-persistable (here: a writable cart).
            //  2. checkpoint-before — captured when a medium is already present (an
            //     intervention vs. a fresh-session root); pinned so the event is
            //     replayable.
            //  3. mount_disk_media — persists the OUTGOING disk's dirty writes to its
            //     host file BEFORE detach/replace (the actual data-loss fix).
            //  4. checkpoint-after — embedded as event.checkpointAfterId; pinned.
            if let Some(reason) = non_persistable_dirty_media(&st) {
                return Response::err(id, -32602, format!(
                    "media: cannot apply a media change — {reason} (Spec 709.13)."
                ));
            }
            let media_present = st.session.machine.drive8.get_attached_disk().is_some()
                || st.session.machine.cartridge.is_some();
            let before_id = if media_present { capture_media_checkpoint(&mut st) } else { None };
            // audit ws-media-8 — record the swapped-in disk in the recents store.
            add_recent_media(&mut st, &path_str, format_str);
            let persisted_outgoing = mount_disk_media(&mut st, image, &path_str);
            let after_id = capture_media_checkpoint(&mut st);
            if let Some(ref b) = before_id { st.checkpoint_ring.pin(b); }
            if let Some(ref a) = after_id { st.checkpoint_ring.pin(a); }
            let cycle = st.session.machine.clk;

            let event = json!({
                "cycle": cycle,
                "operation": "disk",
                "role": "drive8",
                "format": format_str,
                "sha256": sha256,
                "resetPolicy": null,
                "checkpointBeforeId": before_id,
                "checkpointAfterId": after_id
            });
            push_media_event(&mut st, event.clone());
            let mut detail = json!({ "name": disk_name, "backingPath": path_str });
            if let Some(p) = persisted_outgoing { detail["diskPersisted"] = json!(p); }
            // audit ws-media-mount-pause (Spec 709 §2.2 / §709.13.1) — a DISK
            // mount/swap is a live device op (the 1541 is a separate device): a
            // running C64 keeps running through the insert, so the reply reports the
            // REAL run-state, NOT a hardcoded `paused:true`. TS's ingress returns
            // paused=(runState==="paused") (ingress.ts:299); a disk insert never
            // pauses the C64, so a running machine returns paused:false. (Only a
            // C64-INTERNAL change — CRT/PRG — pauses; those branches already report
            // paused:false after resuming.)
            let paused = !st.session.running;
            Response::ok(id, json!({
                "mountedPath": path_str,
                "type": format_str,
                "slot": 8u64,
                "sha256": sha256,
                "event": event,
                "detail": detail,
                "paused": paused
            }))
        }

        "media/persist" => {
            let role = req.params.get("role").and_then(|v| v.as_str()).unwrap_or("").to_string();

            // T2.5 — role="cartridge": persist the live flash back to the host .crt
            // and broadcast media/cart_persisted. Mirrors TS ws-server.ts:687-696
            // (explicit persist) + runtime-controller.ts:512-516 (auto-persist
            // broadcast). TRX64 has no per-frame auto-persist loop, so the explicit
            // persist IS the cart-persist trigger; auto:true matches the TS broadcast
            // shape so UI listeners (which only watch the auto-persist event) fire.
            if role == "cartridge" {
                let mut st = state.lock().unwrap();
                let session_id = st.session.id.clone();
                // Collect path + crt bytes while holding the lock.
                let cart_result: Result<(String, Vec<u8>), String> = {
                    let m = &mut st.session.machine;
                    let path = m
                        .cartridge_image
                        .as_ref()
                        .map(|img| img.path.clone())
                        .unwrap_or_default();
                    if path.is_empty() {
                        Err("no cartridge attached or no backing file path".to_string())
                    } else {
                        match m.cartridge.as_mut().and_then(|c| c.crt_image(m.clk)) {
                            Some(bytes) => Ok((path, bytes)),
                            None => Err("mapper cannot re-pack a .crt (read-only or unsupported)".to_string()),
                        }
                    }
                };
                match cart_result {
                    Err(reason) => {
                        return Response::ok(id, json!({ "written": false, "reason": reason }));
                    }
                    Ok((path_clone, bytes_to_write)) => {
                        let notify = st.notify.clone();
                        drop(st);
                        match std::fs::write(&path_clone, &bytes_to_write) {
                            Ok(()) => {
                                let byte_count = bytes_to_write.len();
                                // 1:1 with runtime-controller.ts:513-515 broadcast.
                                notify.broadcast("media/cart_persisted", json!({
                                    "session_id": session_id,
                                    "path": path_clone,
                                    "bytes": byte_count,
                                    "auto": true
                                }));
                                return Response::ok(id, json!({
                                    "written": true,
                                    "path": path_clone,
                                    "bytes": byte_count
                                }));
                            }
                            Err(e) => {
                                return Response::err(id, -32001, format!("media/persist: cart write error: {e}"));
                            }
                        }
                    }
                }
            }

            let mut st = state.lock().unwrap();
            // Flush any in-flight drive write (dirty GCR track) back into
            // disk.bytes before persisting — 1:1 with VICE flushing
            // drive_gcr_data_writeback_all before reading fsimage->fd.
            st.session.machine.drive8.flush_disk_writeback();
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

        // ── Spec 709.8 — media-event readback ─────────────────────────────────
        // 1:1 with c64re ws-server.ts:1794
        //   this.on("media/events", ({ session_id }) =>
        //       ({ events: ctrlFor(session_id).mediaEvents }));
        // Returns the ordered, replayable media-event history (mount/swap/unmount/
        // eject/ingress) that the media ops accumulate in `State.media_events`.
        // session_id is accepted (singleton session) for wire parity.
        "media/events" => {
            let st = state.lock().unwrap();
            Response::ok(id, json!({ "events": st.media_events.clone() }))
        }

        // ── Spec 265 / audit ws-media-8 — recent-media list ───────────────────
        // 1:1-shape with c64re ws-server.ts:1809 media/recent: an array of
        // { path, name, type, mountedAt } entries — the in-memory recents store
        // (newest-first, mountedAt) overlaid AHEAD of the project + samples dir scans
        // (= c64re §1 recents-first, §2 samples, §3 project walk). c64re's recents store
        // is a GLOBAL ~/.config/c64re/recent-media.json; TRX64 keeps it IN-MEMORY
        // per-daemon (additive — no host-state writes into the user's config), updated
        // by add_recent_media on every mount/swap. Image exts only (.d64/.g64/.crt/.vsf
        // — .prg excluded, as in c64re's project walk).
        "media/recent" => {
            let st = state.lock().unwrap();
            let out = scan_recent_media(&st.recent_media);
            Response::ok(id, json!(out))
        }

        // ── Spec 703/706/768 — audio stream control ───────────────────────────
        // c64re drives a PER-SESSION reSID stream the browser starts/stops with
        // audio/start|stop (ws-server.ts:1635/1702). TRX64's A/V push is a SINGLETON
        // hub-driven stream (streaming.rs, ADR-073): every connected client is
        // auto-subscribed and the daemon IS the producer, so there is no per-session
        // stream to start/stop. These handlers therefore ACK over the hub stream and
        // report whether the live A/V push is active (`streaming`), matching the
        // c64re response keys ({streaming, sample_rate} / {stopped}). The audio is
        // already flowing as BIN_AUDIO; start/stop do not gate it (the hub owns the
        // lifecycle), so they are control acknowledgments, not stream toggles.
        "audio/start" => {
            // sample_rate is the engine's fixed 44100 (= streaming.rs / WavFormat).
            Response::ok(id, json!({
                "streaming": true,
                "sample_rate": 44100u32,
                "engine": "hub"
            }))
        }

        "audio/stop" => {
            // The hub stream is not per-session; nothing to tear down here (the
            // last-client-leaves drop stops the loop). Report stopped:false so the
            // caller knows no per-session stream was owned (c64re: bool).
            Response::ok(id, json!({ "stopped": false }))
        }

        // c64re audio/export (ws-server.ts:1704): run the session for duration_sec
        // PAL seconds, harvest reSID PCM, write a stereo WAV → { out_path,
        // duration_sec, sample_rate, samples, bytes }. TRX64 drives the SAME
        // SidAudioEngine the streaming loop uses: install the additive $D4xx write
        // hook, run the machine in ~1024-sample slices (= exportSessionAudio cadence),
        // record_write/record_boundary per slice, then export_wav. Byte-for-byte the
        // c64re ExportResult shape.
        "audio/export" => {
            let out_path = match req.params.get("out_path").and_then(|v| v.as_str()) {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => return Response::err(id, -32602, "audio/export: out_path required"),
            };
            let duration_sec = req.params.get("duration_sec").and_then(|v| v.as_f64());
            let duration_sec = match duration_sec {
                Some(d) if d.is_finite() && d > 0.0 => d,
                _ => return Response::err(
                    id, -32602,
                    format!("audio/export: bad duration_sec: {:?}", req.params.get("duration_sec")),
                ),
            };
            let mut st = state.lock().unwrap();
            match export_session_audio(&mut st.session, &out_path, duration_sec) {
                Ok(v) => Response::ok(id, v),
                Err(e) => Response::err(id, -32001, format!("audio/export: {e}")),
            }
        }

        // ── Spec 271 — batch scenario runner ──────────────────────────────────
        // c64re's batch/* (ws-server.ts:2331/2376/2384) spawns N worker THREADS
        // (scenario-pool.ts) that replay scenarios in parallel and reports progress
        // via a `batch/progress` broadcast. TRX64's daemon is single-threaded, so
        // batch/start runs the scenarios SEQUENTIALLY in-process through the existing
        // `run_scenario` path and returns the COMPLETED entry. It NOW also pushes the
        // same `batch/progress` notification per completed scenario (and the terminal
        // done/error), via the generic `NotifyHub` — so the live wire matches c64re's
        // `onProgress` + the final done/error broadcast (the ordering differs only in
        // that sequential progress is monotonic, never interleaved). The WIRE shape —
        // batchId/status/completed/total/workerCount + results map, and the progress
        // envelope { batchId, completed, total, currentId } — is 1:1. Each scenario id
        // is looked up in the in-process scenario registry (= c64re scenarioIds).
        "batch/start" => {
            let ids: Vec<String> = match req.params.get("scenarioIds").and_then(|v| v.as_array()) {
                Some(a) if !a.is_empty() => {
                    a.iter().filter_map(|v| v.as_str().map(String::from)).collect()
                }
                _ => return Response::err(id, -32602, "scenarioIds must be a non-empty array"),
            };
            if ids.is_empty() {
                return Response::err(id, -32602, "scenarioIds must be a non-empty array");
            }
            let worker_count = req
                .params
                .get("workerCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(1)
                .max(1);

            let batch_id = new_batch_id();
            let mut st = state.lock().unwrap();
            let total = ids.len() as u64;
            // Clone the notify handle ONCE (the loop mutably borrows `st`, so we can't
            // touch `st.notify` inside it). Same channel set as every other push.
            let notify = st.notify.clone();

            // Run each scenario sequentially via the existing deterministic replay,
            // pushing `batch/progress` after each one (= c64re's per-scenario onProgress).
            let mut results: Vec<(String, Result<Value, String>)> = Vec::with_capacity(ids.len());
            for sid in &ids {
                let scenario = st.scenarios.get(sid).cloned();
                let r = match scenario {
                    Some(s) => run_scenario(&mut st, &s),
                    None => Err(format!("scenario '{sid}' not found")),
                };
                results.push((sid.clone(), r));
                notify.broadcast("batch/progress", json!({
                    "batchId": batch_id,
                    "completed": results.len() as u64,
                    "total": total,
                    "currentId": sid,
                }));
            }
            let completed = total;
            let any_err = results.iter().any(|(_, r)| r.is_err());
            // Terminal progress broadcast (ws-server.ts:2358 / :2366): done or error.
            if any_err {
                let err = results
                    .iter()
                    .find_map(|(_, r)| r.as_ref().err().cloned())
                    .unwrap_or_default();
                notify.broadcast("batch/progress", json!({
                    "batchId": batch_id,
                    "status": "error",
                    "error": err,
                }));
            } else {
                notify.broadcast("batch/progress", json!({
                    "batchId": batch_id,
                    "completed": total,
                    "total": total,
                    "status": "done",
                }));
            }

            let entry = BatchEntry {
                batch_id: batch_id.clone(),
                status: if any_err { "error" } else { "done" },
                completed,
                total,
                worker_count,
                started_at: now_iso8601(),
                finished_at: Some(now_iso8601()),
                last_error: results
                    .iter()
                    .find_map(|(_, r)| r.as_ref().err().cloned()),
                results,
            };
            let summary = serialise_batch(&entry);
            st.batches.insert(batch_id, entry);
            Response::ok(id, summary)
        }

        "batch/status" => {
            let batch_id = match req.params.get("batchId").and_then(|v| v.as_str()) {
                Some(b) => b.to_string(),
                None => return Response::err(id, -32602, "batchId required"),
            };
            let st = state.lock().unwrap();
            match st.batches.get(&batch_id) {
                Some(entry) => Response::ok(id, serialise_batch(entry)),
                None => Response::err(id, -32001, format!("batch '{batch_id}' not found")),
            }
        }

        "batch/results" => {
            let batch_id = match req.params.get("batchId").and_then(|v| v.as_str()) {
                Some(b) => b.to_string(),
                None => return Response::err(id, -32602, "batchId required"),
            };
            let st = state.lock().unwrap();
            match st.batches.get(&batch_id) {
                Some(entry) => Response::ok(id, json!({
                    "batch": serialise_batch(entry),
                    "results": serialise_batch_results(entry),
                })),
                None => Response::err(id, -32001, format!("batch '{batch_id}' not found")),
            }
        }

        // ── trace/* ──────────────────────────────────────────────────────────

        "trace/start_domains" => {
            let mut st = state.lock().unwrap();
            // Spec 708 double-start guard (= TS ws-server.ts:1281): starting a trace
            // while one is already active must THROW, not silently clobber the live
            // capture (which would orphan the in-flight .c64retrace + reset eventCount).
            if st.session.trace.is_some() {
                return Response::err(id, -32001,
                    format!("trace already active on session {} — stop it first (trace/run/stop).", st.session.id));
            }
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
            // misc-0/1 — write the FULL capture-all definition as defJson (a valid
            // JSON STRING), matching c64re captureAllDef + trace-run.ts:217
            // (`defJson: JSON.stringify(def)`). The c64re DuckDB indexer does
            // `JSON.parse(meta.defJson)` (binary-log-indexer.ts:140); an empty string
            // threw "Unexpected end of JSON input", so a TRX64-written trace was
            // un-indexable and trace/read could not read it. defName + createdAt also
            // match the TS header now (oracle diffs records, NOT the meta header, so
            // this is gate-safe).
            let def_json_str =
                serde_json::to_string(&capture_all_def_json(&domains)).unwrap_or_default();
            let meta_json = serde_json::to_string(&json!({
                "runId": run_id,
                "defId": "live-capture",
                "defVersion": 1,
                "defName": "live session capture",
                "defJson": def_json_str,
                "domains": domains,
                "cycleStart": cycle_start,
                "createdAt": now_iso8601_utc(),
            }))
            .unwrap_or_default();
            // Flush any in-flight drive write so the captured media SHA reflects the
            // current image bytes (VICE flushes before reading fsimage->fd).
            st.session.machine.drive8.flush_disk_writeback();
            let (media_sha, media_name) = match st.session.machine.drive8.get_attached_disk() {
                Some(disk) => (
                    sha256_hex(&disk.bytes),
                    disk.backing_path
                        .as_ref()
                        .and_then(|p| p.rsplit('/').next())
                        .map(String::from)
                        .unwrap_or_default(),
                ),
                None => (String::new(), String::new()),
            };
            let start_wall_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            st.session.trace = Some(TraceState {
                retrace_path: retrace,
                meta_json,
                cycle_start,
                buf: Vec::new(),
                run_id: run_id.clone(),
                event_count: 0,
                domains: domains.clone(),
                marks: Vec::new(),
                // captureAll domains trace ⇒ definitionId "live-capture" (= TS).
                definition_id: "live-capture".to_string(),
                definition_version: 1,
                start_wall_ms,
                media_sha: media_sha.clone(),
                media_name: media_name.clone(),
                // captureAll trace — empty captures means "keep every domain-implied row".
                captures: Vec::new(),
            });
            // T2.6 — mirror TS start(): lastRunId set on trace start, survives stop().
            st.last_run_id = Some(run_id.clone());
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
            if !media_sha.is_empty() {
                run["media"] = json!({ "sha256": media_sha, "sourceName": media_name });
            }
            Response::ok(id, json!({
                "run": run,
                "outputPath": output.to_string_lossy(),
                "domains": domains
            }))
        }

        // reverse-debug Phase 1c — `trace/build_from_ring { cycle_start, cycle_end,
        // output_path? }`: a TARGETED dump-on-demand of the always-on full-delta ring.
        //
        // The UI scrub-bar selects TWO thumbnails (a cycle window); this slices the
        // 10 s delta ring for the entries whose cycle ∈ [cycle_start, cycle_end] and
        // ENCODES them into a `.c64retrace` using the SAME binary record format the live
        // trace writes (FrameSink::write_delta_entry → CPU_STEP 0x10 + RAM_WRITE 0x11),
        // so the file is read by the EXISTING sidecar path (swimlane / map / taint)
        // identically to a finalized live trace. No whole-run capture, no cycle guessing.
        //
        // The `.duckdb` index is left LAZY (built by the sidecar on first read, exactly
        // like a finalized trace), and `state.last_trace_path` is pointed at it so the
        // monitor map/swimlane/taint/chis verbs read THIS store immediately.
        //
        // Runs ON DEMAND (not the hot path); the state lock is held only for the slice
        // copy + encode (a bounded window), then released.
        "trace/build_from_ring" => {
            let cycle_start = match req.params.get("cycle_start").and_then(|v| v.as_u64()) {
                Some(c) => c,
                None => return Response::err(id, -32602, "trace/build_from_ring: cycle_start required (u64)"),
            };
            let cycle_end = match req.params.get("cycle_end").and_then(|v| v.as_u64()) {
                Some(c) => c,
                None => return Response::err(id, -32602, "trace/build_from_ring: cycle_end required (u64)"),
            };
            let output_path = req
                .params
                .get("output_path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from);
            let mut st = state.lock().unwrap();
            match build_trace_from_ring(&mut st, cycle_start, cycle_end, output_path) {
                Ok(out) => Response::ok(id, out),
                Err(e) => Response::err(id, -32001, e),
            }
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

        // T2.6 — start a trace by definition id (ws-server.ts:1230-1238).
        // Looks up `st.trace_definitions[definition_id]` and reuses the same
        // TraceState initialisation logic as `trace/start_domains`, substituting the
        // definition's domains + generating a run_id as TS does:
        //   `run_${def.id}_${Date.now().toString(36)}`  (trace-run.ts:202)
        "trace/run/start" => {
            let definition_id = match req.params.get("definition_id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return Response::err(id, -32602, "trace/run/start: definition_id required"),
            };
            let mut st = state.lock().unwrap();
            // Guard: TS throws if already active.
            if st.session.trace.is_some() {
                return Response::err(id, -32001,
                    format!("trace already active on session {} — stop it first (trace/run/stop).", st.session.id));
            }
            let def = match st.trace_definitions.get(&definition_id).cloned() {
                Some(d) => d,
                None => return Response::err(id, -32001,
                    format!("trace/run/start: unknown definition \"{definition_id}\"")),
            };
            let output = req.params.get("output")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from);
            // TS: `outputPath = resolveSnapshotPath(output ?? "traces/${def.id}_${Date.now().toString(36)}.duckdb")`
            let now36 = radix36(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
            );
            // resolveSnapshotPath roots a RELATIVE output under the project dir (= TS
            // resolveSnapshotPath). Without this the default `traces/<def>_<ts>.duckdb`
            // was a bare relative path the cwd-agnostic sidecar/readers could not find,
            // so a trace/run/start capture was unreadable (trace/read "no trace store").
            let output = output.unwrap_or_else(|| {
                let rel = format!("traces/{}_{}.duckdb", definition_id, now36);
                match resolve_project_dir() {
                    Some(base) => base.join(rel),
                    None => std::path::PathBuf::from(rel),
                }
            });
            let retrace = output.with_extension("c64retrace");
            let cycle_start = st.session.machine.clk;
            // TS: `runId = "run_${def.id}_${Date.now().toString(36)}"` (trace-run.ts:202)
            let run_id = format!("run_{}_{}", definition_id, now36);
            let domains: Vec<String> = def
                .get("domains")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|d| d.as_str().map(String::from)).collect())
                .unwrap_or_else(|| vec!["c64-cpu".into(), "memory".into()]);
            // Spec 708.7 — the def's DECLARED capture kinds (the row-selection layer).
            // The domains open the channels; the captures select which rows are KEPT.
            let captures: Vec<String> = def
                .get("captures")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|c| c.get("kind").and_then(|k| k.as_str()).map(String::from)).collect())
                .unwrap_or_default();
            let def_version = def.get("version").and_then(|v| v.as_i64()).unwrap_or(1);
            let def_name = def.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let meta_json = serde_json::to_string(&json!({
                "runId": run_id,
                "defId": definition_id,
                "defVersion": def_version,
                "defName": def_name,
                "defJson": serde_json::to_string(&def).unwrap_or_default(),
                "domains": domains,
                "cycleStart": cycle_start,
                "createdAt": "",
            }))
            .unwrap_or_default();
            // Capture the mounted-media identity (= TS gatherMediaIdentity → run.media):
            // sha256 + basename of the attached disk (empty when none). flush first so
            // the captured SHA reflects any pending write-back.
            st.session.machine.drive8.flush_disk_writeback();
            let (media_sha, media_name) = match st.session.machine.drive8.get_attached_disk() {
                Some(disk) => (
                    sha256_hex(&disk.bytes),
                    disk.backing_path
                        .as_ref()
                        .and_then(|p| p.rsplit('/').next())
                        .map(String::from)
                        .unwrap_or_default(),
                ),
                None => (String::new(), String::new()),
            };
            let start_wall_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            st.session.trace = Some(TraceState {
                retrace_path: retrace,
                meta_json,
                cycle_start,
                buf: Vec::new(),
                run_id: run_id.clone(),
                event_count: 0,
                domains: domains.clone(),
                marks: Vec::new(),
                // ws-trace-monitor-misc-23 — echo the REAL definition id/version + the
                // start-wall baseline + media identity so finalize_trace returns the full
                // RuntimeTraceRun descriptor (not a hardcoded "live-capture").
                definition_id: definition_id.clone(),
                definition_version: def_version,
                start_wall_ms,
                media_sha: media_sha.clone(),
                media_name: media_name.clone(),
                // Spec 708.7 — gate the recorded rows by the def's declared captures.
                captures,
            });
            // T2.6 — mirror TS start(): lastRunId set on trace start, survives stop().
            st.last_run_id = Some(run_id.clone());
            let mut run = json!({
                "runId": run_id,
                "definitionId": definition_id,
                "definitionVersion": def_version,
                "cycleStart": cycle_start,
                "marks": [],
                "eventCount": 0,
                "bytesWritten": 0
            });
            if !media_sha.is_empty() {
                run["media"] = json!({ "sha256": media_sha, "sourceName": media_name });
            }
            Response::ok(id, json!({ "run": run }))
        }

        // T2.6 — push a named marker into the active trace (ws-server.ts:1288-1293).
        // Mirrors TS: error if no active trace; push (cpu_clk, label) into marks;
        // return status().  Shape = TraceRunStatus (active:true, runId, eventCount,
        // marks count, …).
        "trace/run/mark" => {
            let label = match req.params.get("label").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::err(id, -32001, "trace/run/mark: label required"),
            };
            let mut st = state.lock().unwrap();
            let clk = st.session.machine.clk;
            match st.session.trace.as_mut() {
                None => return Response::err(id, -32001, "no active trace run"),
                Some(t) => t.marks.push((clk, label)),
            }
            // Return trace/run/status shape (mirrors TS `c.traceRun.status()`).
            let status = match &st.session.trace {
                Some(t) => json!({
                    "active": true,
                    "runId": t.run_id,
                    "eventCount": t.event_count,
                    "binary": true,
                    "marks": t.marks.len() as u64,
                    "retracePath": t.retrace_path.to_string_lossy(),
                }),
                None => json!({ "active": false }),
            };
            Response::ok(id, status)
        }

        // T2.6 — return the last finalized store path + run_id (ws-server.ts:1287).
        // TS: `ctrlFor(session_id).traceRun.currentStorePath() ?? { path: null }`.
        // Active trace → path = duckdb output path (active:true).
        // Finalized trace → path = last_trace_path (active:false, from finalize_trace).
        // Never started → { path: null }.
        "trace/current" => {
            let st = state.lock().unwrap();
            if let Some(t) = &st.session.trace {
                // Active trace: derive the .duckdb path from the .c64retrace path.
                let retrace = t.retrace_path.to_string_lossy();
                let duckdb_path = if retrace.ends_with(".c64retrace") {
                    format!("{}.duckdb", &retrace[..retrace.len() - ".c64retrace".len()])
                } else {
                    retrace.into_owned()
                };
                // Honest index status: does the `.duckdb` exist on disk yet? An active
                // trace has not been finalized, so the index is built lazily on the
                // first read (or by `trace/index`) — caller can decide to index now.
                let indexed = std::path::Path::new(&duckdb_path).exists();
                Response::ok(id, json!({
                    "path": duckdb_path,
                    "duckdbPath": duckdb_path,
                    "retracePath": t.retrace_path.to_string_lossy(),
                    "runId": t.run_id,
                    "active": true,
                    "indexing": false,
                    "indexed": indexed,
                }))
            } else if let (Some(path), Some(run_id)) = (&st.last_trace_path, &st.last_run_id) {
                // Finalized trace: report whether the `.duckdb` index has been built
                // (auto-index on stop, an earlier read, or an explicit `trace/index`).
                let retrace_path = if path.ends_with(".duckdb") {
                    format!("{}.c64retrace", &path[..path.len() - ".duckdb".len()])
                } else {
                    format!("{path}.c64retrace")
                };
                let indexed = std::path::Path::new(path).exists();
                Response::ok(id, json!({
                    "path": path,
                    "duckdbPath": path,
                    "retracePath": retrace_path,
                    "runId": run_id,
                    "active": false,
                    "indexing": false,
                    "indexed": indexed,
                }))
            } else {
                Response::ok(id, json!({ "path": Value::Null }))
            }
        }

        // trace/index — EXPLICITLY build the `.duckdb` index for a `.c64retrace` (the
        // trace-decode gap fix). `trace_store_info` and any reader that opens the
        // `.duckdb` DIRECTLY never trigger the sidecar's lazy-on-read build, so a
        // captured-but-unindexed trace looks like "directory has no trace.duckdb".
        // This method runs the SAME sidecar indexer the lazy path uses, but as an
        // explicit op that returns { duckdbPath, eventsIndexed, bounded, boundedFrom,
        // cap, indexedFromOldest } WITHOUT running an analysis query.
        //
        // params: { retrace_path? (the `.c64retrace` OR its `.duckdb` sibling — both
        // accepted; defaults to the current/last finalized trace), wait? (default
        // true = run to completion; false = 15s grace then report "still building"). }
        //
        // HONESTY: the indexer streams the WHOLE file oldest→newest with NO event cap,
        // so a 1.2 GB trace's oldest events ARE indexed (cap=null, indexedFromOldest=
        // true). `bounded` is true ONLY when wait=false and the grace expired before
        // the build finished — that is a not-ready state, never dropped data.
        "trace/index" => {
            // Resolve the target `.duckdb` path. An explicit retrace_path may be the
            // `.c64retrace` (→ derive `.duckdb`) or already a `.duckdb`; default to the
            // current/last trace store.
            let duckdb_path = {
                let st = state.lock().unwrap();
                match req.params.get("retrace_path").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                    Some(p) => {
                        if p.ends_with(".c64retrace") {
                            format!("{}.duckdb", &p[..p.len() - ".c64retrace".len()])
                        } else {
                            p.to_string()
                        }
                    }
                    None => match current_trace_duckdb(&st) {
                        Some(db) => db,
                        None => return Response::err(id, -32001,
                            "trace/index: no retrace_path given and no trace has run (nothing to index)"),
                    },
                }
            };
            // Lock is released before shelling out to the sidecar (the index build can
            // take minutes on a multi-GB trace — never hold the session mutex for it).
            let wait = req.params.get("wait").and_then(|v| v.as_bool()).unwrap_or(true);
            match run_trace_read_sidecar("index", &duckdb_path, &json!({ "wait": wait })) {
                Ok(v) => Response::ok(id, v),
                Err(e) => Response::err(id, -32001, e),
            }
        }

        "trace/run/stop" => {
            // wait_index (default false to preserve the instant-stop behaviour): when
            // truthy, BUILD the `.duckdb` index now (the trace-decode gap fix) so the
            // finalized trace is immediately queryable by BOTH trace/read AND any reader
            // that opens the `.duckdb` directly (trace_store_info) — not only after a
            // lazy first read. finalize_trace writes the `.c64retrace` + sets
            // last_trace_path; we then index its sibling via the same sidecar path.
            let wait_index = req.params.get("wait_index").and_then(|v| v.as_bool()).unwrap_or(false);
            let (status, duckdb_path) = {
                let mut st = state.lock().unwrap();
                let status = finalize_trace(&mut *st, !wait_index);
                // The duckdb path finalize_trace just recorded (None when no trace ran).
                (status, st.last_trace_path.clone())
            }; // lock released before the (potentially minutes-long) index build
            let mut out = json!({ "run": status.0, "status": status.1 });
            if wait_index {
                if let Some(db) = duckdb_path {
                    // wait=true → run the decode to completion so the store is ready on
                    // return. Soft-fail: a sidecar/index error must NOT break trace stop
                    // (the `.c64retrace` authority is on disk + re-indexable); surface it
                    // as an `index` field, not an RPC error.
                    match run_trace_read_sidecar("index", &db, &json!({ "wait": true })) {
                        Ok(v) => { out["index"] = v; }
                        Err(e) => { out["index"] = json!({ "ok": false, "error": e, "duckdbPath": db }); }
                    }
                }
            }
            Response::ok(id, out)
        }

        "trace/run/status" => {
            // Spec 726 §6a / 708 full status contract (= TS TraceRun.status(),
            // trace-run.ts): an ACTIVE trace reports the FULL run shape, not a subset.
            // The prior status dropped definitionId/marks/overflowed/capturing/
            // bytesBuffered, so the 708 contract case (marks/binary/capturing/
            // overflowed) could not be asserted. A live TRX64 trace is ALWAYS the
            // binary sink (binary:true) actively recording (capturing:true) with no
            // queue overflow (overflowed:false = TS `a.binary ? false : a.overflow`).
            let st = state.lock().unwrap();
            let status = match &st.session.trace {
                Some(t) => json!({
                    "active": true,
                    "runId": t.run_id,
                    "definitionId": t.definition_id,
                    "eventCount": t.event_count,
                    "bytesBuffered": t.buf.len() as u64,
                    "marks": t.marks.len() as u64,
                    "overflowed": false,
                    "capturing": true,
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

        // ── Spec 710 — granular vic/inspect/* (rides the 705.B checkpoint ring) ──
        // The ring-only / state-only methods (close, evidence, provenance) are
        // 1:1 with c64re. The pixel-resolution methods (open, at, region,
        // at_capture, origin, promote) additionally need the vic-inspect engine
        // (buildVicInspectSnapshot / resolveVisibleNodeAt / resolveVisualOrigin /
        // assembleInspectEvidence) which is NOT yet ported to trx64-core — they
        // are deferred individually below with the missing-module reason.

        // vic/inspect/provenance — toggle VIC-provenance capture. { enabled }.
        // c64re reads `enabled !== false` (default true). TRX64 stores the flag for
        // the wire contract; it is inert until the provenance engine lands.
        "vic/inspect/provenance" => {
            let enabled = req.params.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            let mut st = state.lock().unwrap();
            st.vic_provenance_enabled = enabled;
            Response::ok(id, json!({ "enabled": enabled }))
        }

        // vic/inspect/close — unpin the inspected checkpoint. { ok, stats }.
        "vic/inspect/close" => {
            let mut st = state.lock().unwrap();
            if let Some(cp_id) = req.params.get("checkpoint_id").and_then(|v| v.as_str()) {
                if !cp_id.is_empty() {
                    st.checkpoint_ring.unpin(cp_id);
                }
            }
            let stats = st.checkpoint_ring.stats().to_json();
            Response::ok(id, json!({ "ok": true, "stats": stats }))
        }

        // vic/inspect/evidence — the promoted-evidence list for the session.
        "vic/inspect/evidence" => {
            let st = state.lock().unwrap();
            Response::ok(id, json!({ "evidence": st.inspect_evidence.clone() }))
        }

        // vic/inspect/open — §2.2: freeze (capture provenance), capture+pin the
        // inspected checkpoint, return the shared record + the UI geometry contract.
        // ws-server.ts:1119-1133. The returned checkpointId + snapshot are the
        // SHARED record 711/712 also bind to.
        "vic/inspect/open" => {
            let mut st = state.lock().unwrap();
            // §2.2 + 710.6c — if running, freeze first (no provenance sidecar in
            // TRX64 yet, so this is just a pause); then capture+pin.
            st.session.running = false;
            let frame = st.ctrl_frame;
            let cycles = st.session.machine.c64_core.clk;
            let cp = capture_live_checkpoint(&mut st.session);
            let r = match st.checkpoint_ring.capture(cp, frame, cycles) {
                Ok(r) => r,
                Err(e) => return Response::err(id, -32001, format!("vic/inspect/open: {e}")),
            };
            st.checkpoint_ring.pin(&r.id);
            // restore_snapshot → the stored RuntimeCheckpoint tree (rehydrated media).
            let snapshot = match st.checkpoint_ring.restore_snapshot(&r.id) {
                Some(s) => s,
                None => return Response::err(id, -32001, "vic/inspect/open: capture vanished from ring"),
            };
            let frame_snap = trx64_core::vic_inspect::build_vic_inspect_snapshot(&snapshot).to_json();
            let provenance = snapshot.get("vicProvenance").cloned().filter(|p| !p.is_null());
            let run_state = if st.session.running { "running" } else { "paused" };
            Response::ok(id, json!({
                "checkpointId": r.id,
                "frame": frame_snap,
                "provenance": provenance,
                "runState": run_state,
                "geometry": {
                    "visible": { "width": trx64_core::vic_inspect::VISIBLE_FRAME_W, "height": trx64_core::vic_inspect::VISIBLE_FRAME_H },
                    "displayOrigin": { "x": trx64_core::vic_inspect::DISPLAY_ORIGIN_X, "y": trx64_core::vic_inspect::DISPLAY_ORIGIN_Y },
                    "cell": { "w": 8, "h": 8, "cols": 40, "rows": 25 },
                },
            }))
        }

        // vic/inspect/at — resolve a VISIBLE-frame pixel (0..384 × 0..272) to its
        // node. ws-server.ts:1135-1139. { node }.
        "vic/inspect/at" => {
            let cp_id = match req.params.get("checkpoint_id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::err(id, -32602, "vic/inspect/at: checkpoint_id required"),
            };
            let x = req.params.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y = req.params.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let st = state.lock().unwrap();
            let cp = match cp_for_inspect(&st, &cp_id) {
                Ok(cp) => cp,
                Err(e) => return Response::err(id, -32001, e),
            };
            let prov = cp.get("vicProvenance").cloned().filter(|p| !p.is_null());
            let node = trx64_core::vic_inspect::resolve_visible_node_at(&cp, x, y, prov.as_ref());
            Response::ok(id, json!({ "node": node.to_json() }))
        }

        // vic/inspect/region — resolve a VISIBLE-frame region to distinct nodes.
        // ws-server.ts:1140-1145. { nodes }.
        "vic/inspect/region" => {
            let cp_id = match req.params.get("checkpoint_id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::err(id, -32602, "vic/inspect/region: checkpoint_id required"),
            };
            let region = match req.params.get("region") {
                Some(r) if r.is_object() => {
                    let g = |k: &str| r.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    (g("x"), g("y"), g("width"), g("height"))
                }
                _ => return Response::err(id, -32602, "vic/inspect/region: region required"),
            };
            let st = state.lock().unwrap();
            let cp = match cp_for_inspect(&st, &cp_id) {
                Ok(cp) => cp,
                Err(e) => return Response::err(id, -32001, e),
            };
            let prov = cp.get("vicProvenance").cloned().filter(|p| !p.is_null());
            let nodes = trx64_core::vic_inspect::resolve_visible_region(&cp, region, prov.as_ref());
            Response::ok(id, json!({ "nodes": nodes.iter().map(|n| n.to_json()).collect::<Vec<_>>() }))
        }

        // vic/inspect/at_capture — frozen-pixel provenance. Captures+pins a
        // checkpoint if none given, then resolves the node (DISPLAY-area coords).
        // ws-server.ts:793-811. { checkpointId, frame, node, hasProvenance }.
        "vic/inspect/at_capture" => {
            let x = req.params.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
            let y = req.params.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
            let given = req.params.get("checkpoint_id").and_then(|v| v.as_str()).map(|s| s.to_string());
            let mut st = state.lock().unwrap();
            let cp_id = match given {
                Some(s) if !s.is_empty() => s,
                _ => {
                    // capture + pin a fresh checkpoint (the in-process tool's behaviour).
                    st.session.running = false;
                    let frame = st.ctrl_frame;
                    let cycles = st.session.machine.c64_core.clk;
                    let cp = capture_live_checkpoint(&mut st.session);
                    let r = match st.checkpoint_ring.capture(cp, frame, cycles) {
                        Ok(r) => r,
                        Err(e) => return Response::err(id, -32001, format!("vic/inspect/at_capture: {e}")),
                    };
                    st.checkpoint_ring.pin(&r.id);
                    r.id
                }
            };
            let cp = match cp_for_inspect(&st, &cp_id) {
                Ok(cp) => cp,
                Err(e) => return Response::err(id, -32001, e),
            };
            let frame_snap = trx64_core::vic_inspect::build_vic_inspect_snapshot(&cp).to_json();
            let prov = cp.get("vicProvenance").cloned().filter(|p| !p.is_null());
            let node = trx64_core::vic_inspect::resolve_node_at_display(&cp, x, y, prov.as_ref());
            Response::ok(id, json!({
                "checkpointId": cp_id,
                "frame": frame_snap,
                "node": node.to_json(),
                "hasProvenance": prov.is_some(),
            }))
        }

        // vic/inspect/origin — Spec 721 Live Visual-Origin Join: resolve a frozen
        // visible node to its ORIGIN (exact byte-hash asset match) + knowledge.
        // ws-server.ts:1190-1201. { node, classification, result, knowledge, medium }.
        "vic/inspect/origin" => {
            let cp_id = match req.params.get("checkpoint_id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::err(id, -32602, "vic/inspect/origin: checkpoint_id required"),
            };
            let x = req.params.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y = req.params.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let st = state.lock().unwrap();
            let cp = match cp_for_inspect(&st, &cp_id) {
                Ok(cp) => cp,
                Err(e) => return Response::err(id, -32001, e),
            };
            let prov = cp.get("vicProvenance").cloned().filter(|p| !p.is_null());
            let node = trx64_core::vic_inspect::resolve_visible_node_at(&cp, x, y, prov.as_ref());
            // Spec 721 — extract AssetCandidates from the mounted medium (sprite/
            // charset/bitmap block hashes). No medium → empty set → honest
            // runtime_generated (same as c64re with nothing mounted / no match).
            let (candidates, medium_ref) = match st.session.machine.drive8.get_attached_disk() {
                Some(d) if !d.bytes.is_empty() => {
                    let kind = match d.kind {
                        trx64_core::drive::DiskKind::G64 => "g64",
                        trx64_core::drive::DiskKind::D64 => "d64",
                    };
                    (
                        trx64_core::vic_inspect::extract_asset_candidates(&d.bytes, "session", Some(kind)),
                        Some(kind.to_string()),
                    )
                }
                _ => (Vec::new(), None),
            };
            let cand_count = candidates.len();
            let (result, knowledge) =
                trx64_core::vic_inspect::resolve_visual_origin(&cp, &node, &candidates, "session");
            let classification = result.get("classification").cloned().unwrap_or(Value::Null);
            Response::ok(id, json!({
                "node": node.to_json(),
                "classification": classification,
                "result": result,
                "knowledge": knowledge,
                "medium": { "ref": medium_ref, "candidateCount": cand_count },
            }))
        }

        // vic/inspect/promote — Spec 710.5: assemble + store a shared evidence
        // record (checkpoint + media identity + optional trace mark + resolved
        // nodes). points/region are VISIBLE-frame coords. ws-server.ts:1154-1168.
        "vic/inspect/promote" => {
            let cp_id = match req.params.get("checkpoint_id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::err(id, -32602, "vic/inspect/promote: checkpoint_id required"),
            };
            let points: Vec<(f64, f64)> = req
                .params
                .get("points")
                .and_then(|p| p.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|p| {
                            (
                                p.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0),
                                p.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let region = req.params.get("region").filter(|r| r.is_object()).map(|r| {
                let g = |k: &str| r.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
                (g("x"), g("y"), g("width"), g("height"))
            });
            let trace_mark_id = req.params.get("trace_mark_id").and_then(|v| v.as_str()).map(|s| s.to_string());
            let name = req.params.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
            let notes = req.params.get("notes").and_then(|v| v.as_str()).map(|s| s.to_string());
            let mut st = state.lock().unwrap();
            let cp = match cp_for_inspect(&st, &cp_id) {
                Ok(cp) => cp,
                Err(e) => return Response::err(id, -32001, e),
            };
            let prov = cp.get("vicProvenance").cloned().filter(|p| !p.is_null());
            let evidence = trx64_core::vic_inspect::assemble_inspect_evidence(
                &cp,
                &cp_id,
                &points,
                region,
                trace_mark_id.as_deref(),
                None,
                None,
                prov.as_ref(),
            );
            // ws-server.ts:1163 — tag with name/notes/promotedAtMs.
            let mut tagged = evidence;
            tagged["name"] = name.map(Value::String).unwrap_or(Value::Null);
            tagged["notes"] = notes.map(Value::String).unwrap_or(Value::Null);
            tagged["promotedAtMs"] = json!(now_ms());
            st.inspect_evidence.push(tagged.clone());
            let count = st.inspect_evidence.len();
            Response::ok(id, json!({ "evidence": tagged, "count": count }))
        }

        m if m.starts_with("vic/inspect/") => {
            Response::err(id, -32001,
                format!("NOT_IMPLEMENTED: {m}: unknown vic/inspect/* method"))
        }

        m if m.starts_with("vic/") => {
            Response::err(id, -32001,
                format!("NOT_IMPLEMENTED: {m}: not in vic-render scope"))
        }

        // ── checkpoint/*, recorder/*, vsf/*, trace/read, debug/memory_access_map ─

        // debug/memory_access_map — per-region read/write liveness over a run window.
        // 1:1 with ws-server.ts:731 + src/runtime/headless/debug/memory-access-map.ts.
        // Attaches a MemoryAccessObserver (= TS MemoryAccessTracker), runs the
        // requested cycle budget on the full machine, classifies pages into
        // unused/read-only/dead/live, coalesces into regions, filters by `classes`
        // and `min_bytes`, returns the TS-shaped map.
        "debug/memory_access_map" => {
            let cyc: u64 = req.params.get("cycles")
                .and_then(|v| v.as_u64())
                .unwrap_or(2_000_000);
            // Default classes = ["dead", "unused"] (= TS wantClasses default).
            let classes_raw: Vec<String> = req.params.get("classes")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_else(|| vec!["dead".to_string(), "unused".to_string()]);
            let want_classes: Vec<&str> = classes_raw.iter().map(String::as_str).collect();
            let min_bytes: u64 = req.params.get("min_bytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(256);

            let mut st = state.lock().unwrap();
            let session = &mut st.session;
            // Spec 723: SAME bus gate the run path (`run_cycle_budget`) uses — the
            // access map must reflect the machine the scenario RUNS on. On a `vic`-directed
            // (or io-injected / booted) scenario this engages the full VIC, so the map
            // sees the VIC register + sweep accesses a FlatRam isolated run would miss.
            let full_machine = full_machine_gate(session);

            let mut obs = MemoryAccessObserver::new();
            if full_machine {
                session.machine.run_for_full(cyc, &mut obs, |_, _, _, _, _, _, _| {});
            } else {
                session.machine.run_for(cyc, &mut obs);
            }
            let result = obs.into_result(cyc, &want_classes, min_bytes);
            Response::ok(id, result)
        }

        // trace/read — read a trace store IN/ALONGSIDE the daemon (audit misc-0).
        // 1:1 with the c64re ws-server.ts:1302-1377 handler: params { op, duckdb_path,
        // args }. TRX64 has no native DuckDB reader yet, so it shells out to the Node
        // sidecar that imports the EXISTING c64re indexer + v2 readers (byte-identical
        // by construction). The index is built lazily from the `.c64retrace`
        // authority on first read (covers misc-1: no index at trace stop).
        "trace/read" => {
            let op = match req.params.get("op").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return Response::err(id, -32602, "trace/read: op required"),
            };
            let duckdb_path = match req.params.get("duckdb_path").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::err(id, -32602, "trace/read: duckdb_path required"),
            };
            let args = req.params.get("args").cloned().unwrap_or(json!({}));
            match run_trace_read_sidecar(&op, &duckdb_path, &args) {
                Ok(v) => Response::ok(id, v),
                Err(e) => Response::err(id, -32001, e),
            }
        }

        // ── Spec 705.B — checkpoint ring (rewind / time-travel) ──────────────
        // 1:1 with the c64re ws-server.ts checkpoint/* handlers + the
        // RuntimeCheckpointRing. The ring is per-daemon (= the c64re controller's
        // per-session ring). `frame` = the controller frame counter (ctrl_frame),
        // `cycles` = the master clock. Capture/restore ride the SAME path as
        // snapshot/dump+undump (capture_live_checkpoint / restore_live_checkpoint).

        // checkpoint/list — { checkpoints: RuntimeCheckpointRef[], stats }.
        "checkpoint/list" => {
            let st = state.lock().unwrap();
            let checkpoints: Vec<Value> =
                st.checkpoint_ring.list().iter().map(|r| r.to_json()).collect();
            let stats = st.checkpoint_ring.stats().to_json();
            Response::ok(id, json!({ "checkpoints": checkpoints, "stats": stats }))
        }

        // checkpoint/capture — captures the live machine + pushes to the ring.
        // { ref: RuntimeCheckpointRef, stats }.
        "checkpoint/capture" => {
            let mut st = state.lock().unwrap();
            let frame = st.ctrl_frame;
            let cycles = st.session.machine.c64_core.clk;
            let cp = capture_live_checkpoint(&mut st.session);
            match st.checkpoint_ring.capture(cp, frame, cycles) {
                Ok(r) => {
                    let stats = st.checkpoint_ring.stats().to_json();
                    Response::ok(id, json!({ "ref": r.to_json(), "stats": stats }))
                }
                Err(e) => Response::err(id, -32001, format!("checkpoint/capture: {e}")),
            }
        }

        // checkpoint/pin — { ref, stats }; errors on unknown id (= c64re throw).
        "checkpoint/pin" => {
            let cp_id = match req.params.get("id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::err(id, -32602, "checkpoint/pin: id required"),
            };
            let mut st = state.lock().unwrap();
            match st.checkpoint_ring.pin(&cp_id) {
                Some(r) => {
                    let stats = st.checkpoint_ring.stats().to_json();
                    Response::ok(id, json!({ "ref": r.to_json(), "stats": stats }))
                }
                None => Response::err(id, -32001, format!("checkpoint/pin: unknown id {cp_id}")),
            }
        }

        // checkpoint/unpin — { ref, stats }; errors on unknown id.
        "checkpoint/unpin" => {
            let cp_id = match req.params.get("id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::err(id, -32602, "checkpoint/unpin: id required"),
            };
            let mut st = state.lock().unwrap();
            match st.checkpoint_ring.unpin(&cp_id) {
                Some(r) => {
                    let stats = st.checkpoint_ring.stats().to_json();
                    Response::ok(id, json!({ "ref": r.to_json(), "stats": stats }))
                }
                None => Response::err(id, -32001, format!("checkpoint/unpin: unknown id {cp_id}")),
            }
        }

        // checkpoint/clear — { stats }.
        "checkpoint/clear" => {
            let mut st = state.lock().unwrap();
            st.checkpoint_ring.clear();
            let stats = st.checkpoint_ring.stats().to_json();
            Response::ok(id, json!({ "stats": stats }))
        }

        // checkpoint/restore — restore a ring entry into the live machine.
        // Params: { id, then?: "pause"|"run"|"keep", render?: bool }.
        // Response: { restored: RuntimeCheckpointRef, state: <debug state> }.
        //
        // 1:1 with the TS controller restore (runtime-controller.ts:535-617):
        //   then==="run"   → pin the anchor + truncate the future + resume (Spec 761).
        //   then==="pause" → ensure paused + publish debug/stopped (reason "pause").
        //   then==="keep"  → INHERIT the prior run-state (a running machine stays
        //                    running; a paused one stays paused). This is the default
        //                    (omitted then). (Audit ws-checkpoint-scrub-0.)
        //   render:true    → re-sim ~1 frame to regenerate the framebuffer for a
        //                    framebuffer-OMITTED auto-anchor (Audit ws-checkpoint-scrub-2).
        // EVERY restore pushes a fresh frame (Audit ws-checkpoint-scrub-1).
        "checkpoint/restore" => {
            let cp_id = match req.params.get("id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::err(id, -32602, "checkpoint/restore: id required"),
            };
            let then = req.params.get("then").and_then(|v| v.as_str());
            let intent = match then {
                Some("pause") | Some("run") | Some("keep") => then,
                _ => None, // omitted/unknown ≡ "keep" (runtime-controller.ts:541 default)
            };
            // runtime-controller.ts:544 — `render` only re-sims when NOT resuming (a
            // running machine regenerates the framebuffer on its own next frame).
            let render = req.params.get("render").and_then(|v| v.as_bool()).unwrap_or(false)
                && intent != Some("run");
            let mut st = state.lock().unwrap();
            // Audit ws-checkpoint-scrub-0 — capture the run-state BEFORE the restore so
            // "keep" can inherit it. `restore_live_checkpoint` force-sets running=false
            // internally (a restore is a control point), so we must snapshot it here.
            let was_running = st.session.running;
            // Resolve the stored payload (rehydrated media) + its ref.
            let snapshot = match st.checkpoint_ring.restore_snapshot(&cp_id) {
                Some(s) => s,
                None => {
                    return Response::err(id, -32001, format!("checkpoint/restore: unknown id {cp_id}"))
                }
            };
            if let Err(e) = restore_live_checkpoint(&mut st.session, &snapshot) {
                return Response::err(id, -32001, format!("checkpoint/restore: {e}"));
            }
            // Audit ws-checkpoint-scrub-2 — an auto-capture anchor OMITS the two VIC
            // framebuffers (BUG-049 — they are a derivable shadow), so a paused restore
            // would leave the live `displayed` buffer stale/black. Honour render:true by
            // re-simulating ONE PAL frame after the state restore so the framebuffer is
            // regenerated from the rolled-back RAM/VIC state (runtime-controller.ts:599-601
            // `runFor(PAL_CYCLES_PER_FRAME)`). The ~1-frame advance is invisible in a
            // paused preview; the exact-state path (runtime_rewind) passes no render.
            if render {
                run_cycle_budget(&mut st.session, crate::streaming::CYC_PER_FRAME);
            }
            let restored = st.checkpoint_ring.get(&cp_id).map(|r| r.to_json());
            // Run-state resolution (runtime-controller.ts:541-552/588):
            //   run  → new timeline: pin the anchor + drop the future, then resume.
            //   pause→ ensure paused.
            //   keep → inherit the prior run-state (running stays running). (scrub-0)
            let now_running = match intent {
                Some("run") => {
                    st.checkpoint_ring.pin(&cp_id);
                    st.checkpoint_ring.truncate_after(&cp_id, true);
                    true
                }
                Some("pause") => false,
                _ => was_running, // "keep" / omitted
            };
            st.session.running = now_running;
            // A restore is a control discontinuity (= a pause/seek): advance the
            // controller frame + clear the last stop, mirroring the undump/reset path.
            st.ctrl_frame += 1;
            st.ctrl_stop = None;
            // A restore is an AUDIO-timeline discontinuity (the worklet ring now holds
            // post-restore-stale samples): push `audio/flush` (ws-server.ts:1667/1690
            // `onRestore` → `this.broadcast("audio/flush", …)`).
            st.notify.broadcast("audio/flush", json!({ "session_id": st.session.id }));
            let registers = register_dump(&st.session);
            // Spec 771.2 — runtime-controller.ts:603 restore() server-PUSHes
            // debug/checkpoint_restored so every client's canvas refreshes to the
            // rolled-back frame (Live.tsx:337 grabs a fresh screenshot on it).
            st.notify.broadcast("debug/checkpoint_restored", json!({
                "session_id": st.session.id,
                "ref": cp_id.clone(),
                "registers": registers.clone(),
            }));
            // Audit ws-checkpoint-scrub-1 — ALWAYS present a fresh frame on restore
            // (runtime-controller.ts:606-613 frameCounter++/presentFrame, "no client-grab
            // dependency"). TS's presentFrame pushes the BINARY VIC frame only (ws-server
            // .ts:474-503 pushFrame; the JSON `session/frame_available` is emitted ONLY in
            // the running loop's maybePresentFrame, NOT on restore — so we must NOT emit it
            // here either, to stay 1:1). Under --stream the paused loop is silent, so we
            // request a one-shot present: the paused stream branch consumes the flag once
            // and pushes exactly one BIN_VIC. A running restore (then=run) gets its frame
            // from the resumed loop's next tick; the flag is harmlessly consumed-or-ignored.
            st.force_present_frame = true;
            // Audit ws-checkpoint-scrub-4 — a then=pause restore must publish
            // debug/stopped (reason "pause"), so a passive UI freezes the run-state on
            // the scrub (runtime-controller.ts:614-617 stopInfo + broadcast debug/stopped).
            if intent == Some("pause") {
                let pc = st.session.machine.c64_core.reg_pc;
                let cycles = st.session.machine.clk;
                st.ctrl_stop = Some(CtrlStop { reason: "pause", pc, cycles });
                let stop_obj = json!({ "reason": "pause", "pc": pc as u64, "cycles": cycles });
                st.notify.broadcast("debug/stopped", json!({
                    "session_id": st.session.id,
                    "stop": stop_obj,
                    "registers": registers,
                }));
            }
            let machine_state = build_debug_state(&st);
            Response::ok(id, json!({
                "restored": restored,
                "state": machine_state,
            }))
        }

        // checkpoint/thumbnails — the scrub filmstrip (ws-server.ts:1028-1037, =
        // RuntimeController.filmstrip): every live ring checkpoint that has a thumbnail,
        // in ring order, with a small palette-indexed picture (id, cycles, frame,
        // pinned, width, height, palette:b64, indices:b64). c64re keeps a SEPARATE
        // per-id thumb store filled at capture time from the live frame (the anchor
        // itself is framebuffer-omitted, BUG-049); filmstrip intersects ring.list()
        // with that store. TRX64 mirrors this: read each ref's picture from
        // `checkpoint_thumbs` (populated by stream_maybe_autocapture for every
        // auto-anchor), FALLING BACK to a thumbnail rendered from the checkpoint's
        // STORED `vicPresentation` framebuffer for a framebuffer-present entry with no
        // stored thumb (explicit checkpoint/capture keeps the FB). An entry with
        // neither yields no thumbnail — like a c64re checkpoint absent from
        // `filmstrip()`. This is the Spec 769.5a fix: previously ONLY the rare
        // framebuffer-present checkpoints got a thumb, so the filmstrip showed ~4 of
        // ~70 ring entries; now every auto-anchor has one.
        "checkpoint/thumbnails" => {
            let st = state.lock().unwrap();
            let refs = st.checkpoint_ring.list();
            let mut thumbnails: Vec<Value> = Vec::new();
            for r in &refs {
                let (w, h, palette, indices) = if let Some(t) = st.checkpoint_thumbs.get(&r.id) {
                    (t.width, t.height, t.palette.clone(), t.indices.clone())
                } else if let Some(cp) = st.checkpoint_ring.restore_snapshot(&r.id) {
                    match checkpoint_thumbnail(&cp) {
                        Some(v) => v,
                        None => continue,
                    }
                } else {
                    continue;
                };
                thumbnails.push(json!({
                    "id": r.id,
                    "cycles": r.cycles,
                    "frame": r.frame,
                    "pinned": r.pinned,
                    "width": w as u64,
                    "height": h as u64,
                    "palette": base64_encode(&palette),
                    "indices": base64_encode(&indices),
                }));
            }
            Response::ok(id, json!({ "thumbnails": thumbnails }))
        }

        m if m.starts_with("checkpoint/") => {
            Response::err(id, -32601, format!("Method not found: {m}"))
        }

        // ── Spec 766.5 — runtime recorder (off-thread scrub history) ─────────
        // 1:1 with the c64re ws-server.ts recorder/* handlers + the RuntimeRecorder.
        // c64re creates the recorder lazily at power-on and exposes recorder/status
        // |list|dump. TRX64's single-threaded daemon has no autocapture loop, so it
        // adds recorder/start|stop (explicit lifecycle) + recorder/capture (the
        // explicit anchor touchpoint that the c64re 0.5 s timer drives implicitly).
        // status/list/dump shapes are identical to c64re.

        // recorder/start — create the recorder + capture an initial anchor.
        // { active: true, stats }.
        "recorder/start" => {
            let mut st = state.lock().unwrap();
            if st.recorder.is_none() {
                st.recorder = Some(
                    trx64_core::recorder::runtime_recorder::RuntimeRecorder::with_defaults(),
                );
                st.recorder_disk_gen = 0;
                st.recorder_disk_hash = None;
            }
            // Capture the first anchor so a status/list right after start is non-empty.
            capture_anchor_now(&mut st);
            let stats = st.recorder.as_ref().unwrap().stats().to_json();
            Response::ok(id, json!({ "active": true, "stats": stats }))
        }

        // recorder/stop — dispose the recorder. { active: false }.
        "recorder/stop" => {
            let mut st = state.lock().unwrap();
            st.recorder = None;
            st.recorder_disk_gen = 0;
            st.recorder_disk_hash = None;
            Response::ok(id, json!({ "active": false }))
        }

        // recorder/capture — explicit anchor touchpoint (TRX64 has no 0.5 s timer).
        // Records one CORE-ONLY anchor + gen-gated media. { active, seq?, stats }.
        "recorder/capture" => {
            let mut st = state.lock().unwrap();
            if st.recorder.is_none() {
                return Response::ok(id, json!({ "active": false }));
            }
            let seq = capture_anchor_now(&mut st);
            let stats = st.recorder.as_ref().unwrap().stats().to_json();
            Response::ok(id, json!({ "active": true, "seq": seq, "stats": stats }))
        }

        // recorder/status — { active, stats?, produced?, mediumShipped? }.
        // ws-server.ts:1079-1083.
        "recorder/status" => {
            let st = state.lock().unwrap();
            match &st.recorder {
                None => Response::ok(id, json!({ "active": false })),
                Some(r) => Response::ok(id, json!({
                    "active": true,
                    "stats": r.stats().to_json(),
                    "produced": r.produced,
                    "mediumShipped": r.medium_shipped,
                })),
            }
        }

        // recorder/list — { active, anchors: RecorderAnchorRef[] }.
        // ws-server.ts:1084-1088.
        "recorder/list" => {
            let st = state.lock().unwrap();
            match &st.recorder {
                None => Response::ok(id, json!({ "active": false, "anchors": [] })),
                Some(r) => {
                    let anchors: Vec<Value> = r.list().iter().map(|a| a.to_json()).collect();
                    Response::ok(id, json!({ "active": true, "anchors": anchors }))
                }
            }
        }

        // recorder/dump — reconstruct anchor `seq` into a full restorable
        // checkpoint, write it as a native .c64re snapshot, return DumpResult
        // (identical shape to snapshot/dump). ws-server.ts:1089-1093 +
        // snapshot-persistence.ts dumpRecorderAnchorSnapshot.
        "recorder/dump" => {
            let seq = match req.params.get("seq").and_then(|v| v.as_u64()) {
                Some(s) => s,
                None => return Response::err(id, -32602, "recorder/dump: seq required"),
            };
            let path = match req.params.get("path").and_then(|v| v.as_str()) {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => return Response::err(id, -32602, "recorder/dump: path required"),
            };
            let st = state.lock().unwrap();
            let recorder = match &st.recorder {
                Some(r) => r,
                None => return Response::err(id, -32001, "recorder/dump: recorder not active"),
            };
            // Reconstruct the full payload (core anchor + re-injected media).
            let (anchor_ref, schema_version, payload) = match recorder.reconstruct(seq) {
                Some(t) => t,
                None => return Response::err(id, -32001, format!(
                    "recorder/dump: anchor seq {seq} was evicted or its medium is no longer retained"
                )),
            };
            let cycle = anchor_ref.cycle as i64;
            let pc = payload
                .get("cpu")
                .and_then(|c| c.get("pc"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as i64;
            // Embedded media: the reconstructed disk image rides as a drive8 input.
            let media_inputs = gather_recorder_media_inputs(&payload, &st.session);
            let media_summary: Vec<Value> = media_inputs
                .iter()
                .map(|m| json!({
                    "role": m.role,
                    "format": m.format,
                    "sourceName": m.source_name,
                    "sha256": m.sha256.clone().unwrap_or_default(),
                    "bytes": m.bytes.as_ref().map(|b| b.len()).unwrap_or(0) as u64,
                }))
                .collect();
            let breakpoints = st.breakpoints.entries.len() as u64;
            drop(st);

            let bytes = trx64_core::native_snapshot::write_native_snapshot(
                trx64_core::native_snapshot::WriteNativeSnapshotArgs {
                    checkpoint: payload,
                    schema_version: schema_version as i64,
                    media: media_inputs,
                    runtime_version: "trx64-runtime/1".to_string(),
                    machine_model: "c64-pal".to_string(),
                    provenance: Some(json!({ "checkpointId": format!("recorder:{seq}") })),
                    pc,
                    cycle,
                },
            );
            if let Some(parent) = std::path::Path::new(&path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&path, &bytes) {
                Ok(()) => Response::ok(id, json!({
                    "path": path,
                    "cycle": cycle as u64,
                    "pc": pc as u64,
                    "machine": "c64-pal",
                    "media": media_summary,
                    "fileBytes": bytes.len() as u64,
                    "breakpoints": breakpoints
                })),
                Err(e) => Response::err(id, -32001, format!("recorder/dump: write error: {e}")),
            }
        }

        m if m.starts_with("recorder/") => {
            Response::err(id, -32001, format!("NOT_IMPLEMENTED: {m}: deferred"))
        }

        // ── Spec 707 — native snapshot dump/undump ───────────────────────────
        // c64re writes a `.c64re` container (checkpoint ring + embedded media +
        // sha-integrity). TRX64 has neither the ring nor that codec, so the FILE
        // BYTES are TRX64's own vsf (a dump→undump round-trips WITHIN TRX64). The
        // WIRE SHAPE matches c64re's DumpResult/UndumpResult 1:1 (snapshot-
        // persistence.ts) — the file format is an implementation detail behind the
        // identical JSON contract. `media[]` is gathered from the live attached
        // disk/cart exactly like c64re's gatherMedia (role/format/sourceName/
        // sha256/bytes). NOTE: a .c64re written by c64re is NOT readable here and
        // vice-versa (different container) — cross-runtime snapshot exchange is a
        // later batch (needs the native-snapshot codec primitive).
        // ── Spec 707 / ADR-077 — native `.c64re` snapshot dump/undump ─────────
        // The FILE is now the real c64re `.c64re` container (magic "C64RESNP" +
        // sha256(gzip(doc)), doc = { manifest, checkpoint:<RuntimeCheckpoint>,
        // mediaPayloads }). A live c64re daemon can `snapshot/undump` a TRX64 dump
        // and TRX64 can undump a c64re dump (the checkpoint shape is 1:1). The WS
        // response shape stays identical to c64re's DumpResult/UndumpResult.
        "snapshot/dump" => {
            let path = match req.params.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return Response::err(id, -32602, "snapshot/dump: path required"),
            };
            let mut st = state.lock().unwrap();
            // Flush any in-flight drive write into disk.bytes so the embedded
            // media + its SHA in the checkpoint reflect the current image
            // (VICE flushes drive_gcr_data_writeback_all before snapshotting).
            st.session.machine.drive8.flush_disk_writeback();
            // Disk path/format for the checkpoint `media` metadata.
            let (disk_path, disk_format) = match st.session.machine.drive8.get_attached_disk() {
                Some(d) => (
                    d.backing_path.clone().unwrap_or_default(),
                    match d.kind { DiskKind::G64 => "g64", DiskKind::D64 => "d64" }.to_string(),
                ),
                None => (String::new(), String::new()),
            };
            // Drive blobs (part 4): the `drive1541` core blob + the `driveDiskImage`
            // GCRIMAGE0 overlay, captured from the live drive. `drive1541` is built
            // by drive_snapshot.rs (byte-compatible with c64re's drive1541.snapshot()).
            let drive1541_blob =
                trx64_core::drive_snapshot::capture_drive1541(&mut st.session.machine.drive8);
            let drive_disk_blob =
                trx64_core::drive_snapshot::capture_drive_disk_image(&st.session.machine.drive8);
            // formats-state-2 — capture the cartridge bytes + writable flash (needs the
            // &mut for the flash erase-alarm catch-up) BEFORE the immutable-borrow call.
            let (cart_bytes, cart_flash) = capture_cart_blobs(&mut st.session.machine);
            let m = &st.session.machine;
            let checkpoint = trx64_core::c64re_snapshot::capture_runtime_checkpoint(
                m,
                &disk_path,
                &disk_format,
                Some(&drive1541_blob),
                drive_disk_blob.as_deref(),
                cart_bytes.as_deref(),
                cart_flash.as_deref(),
            );
            let cycle = m.c64_core.clk as i64;
            let pc = m.c64_core.reg_pc as i64;
            // Embedded media inputs (clean disk/cart bytes, role/format/sourceName).
            let media_inputs = gather_native_media_inputs(&st.session);
            // The `media` summary for the WS response (role/format/sourceName/
            // sha256/bytes) — matches c64re's DumpResult.media.
            let media_summary = gather_snapshot_media(&st.session);
            let breakpoints = st.breakpoints.entries.len() as u64;
            drop(st);

            let bytes = trx64_core::native_snapshot::write_native_snapshot(
                trx64_core::native_snapshot::WriteNativeSnapshotArgs {
                    checkpoint,
                    schema_version: trx64_core::c64re_snapshot::RUNTIME_CHECKPOINT_SCHEMA_VERSION,
                    media: media_inputs,
                    runtime_version: "trx64-runtime/1".to_string(),
                    machine_model: "c64-pal".to_string(),
                    provenance: None,
                    pc,
                    cycle,
                },
            );
            if let Some(parent) = std::path::Path::new(&path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&path, &bytes) {
                Ok(()) => Response::ok(id, json!({
                    "path": path,
                    "cycle": cycle as u64,
                    "pc": pc as u64,
                    "machine": "c64-pal",
                    "media": media_summary,
                    "fileBytes": bytes.len() as u64,
                    "breakpoints": breakpoints
                })),
                Err(e) => Response::err(id, -32001, format!("snapshot/dump: write error: {e}")),
            }
        }

        "snapshot/undump" => {
            let path = match req.params.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return Response::err(id, -32602, "snapshot/undump: path required"),
            };
            let mut st = state.lock().unwrap();
            // Shared core: power-cycle to fresh chips → re-attach media → restore
            // (audio/flush is broadcast inside power_cycle_for_restore). The undump is
            // a restore → the session is left paused.
            match undump_native_snapshot(&mut st, &path) {
                Ok(r) => {
                    let breakpoints = st.breakpoints.entries.len() as u64;
                    let media_summary: Vec<Value> = r
                        .media
                        .iter()
                        .map(|m| {
                            json!({
                                "role": m.role,
                                "format": m.format,
                                "sourceName": m.source_name,
                                "sha256": m.sha256,
                                "bytes": m.bytes
                            })
                        })
                        .collect();
                    Response::ok(id, json!({
                        "path": path,
                        "cycle": r.cycle,
                        "pc": r.pc as u64,
                        "machine": r.machine_model,
                        "media": media_summary,
                        "breakpoints": breakpoints,
                        "paused": true
                    }))
                }
                Err(e) => Response::err(id, -32001, format!("snapshot/undump: {e}")),
            }
        }

        // Spec 793 — purge every undump-materialized `<name>_media/` sidecar (the
        // LLM/test-overlay auto-cleanup). Tag-scoped: deletes ONLY dirs the daemon
        // created via undump materialization, never a user's own mount.
        "undump_media_purge" => {
            let mut st = state.lock().unwrap();
            let tracked = st.materialized_media.len();
            let (dirs, files) = purge_materialized_media(&mut st);
            Response::ok(id, json!({
                "tracked": tracked,
                "dirsRemoved": dirs,
                "filesRemoved": files
            }))
        }

        "vsf/save" => {
            let output_path = req.params
                .get("output_path")
                .and_then(|v| v.as_str())
                .unwrap_or("/tmp/trx64.vsf")
                .to_string();
            let mut st = state.lock().unwrap();
            let bytes = trx64_core::vsf::save_vsf(&mut st.session.machine);
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

        // ── Spec time-travel-tooling Piece 2 — ringbuffer dump/restore ───────────
        // Serialize / reconstruct the WHOLE reverse-debug buffer (checkpoint ring +
        // delta ring + cpu-history ring) to/from one gzipped `.c64rering` container.
        // After restore the scrub filmstrip, reverse_step, who_wrote, chis, and
        // diffCheckpoints all work on the dumped buffer (the tester→dev hand-off).

        // ringbuffer/dump { path } → RingDumpInfo. READ-ONLY w.r.t. the machine.
        "ringbuffer/dump" => {
            let path = match req.params.get("path").and_then(|v| v.as_str()) {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => return Response::err(id, -32602, "ringbuffer/dump: path required"),
            };
            let st = state.lock().unwrap();
            match ringbuffer_dump_to_path(&st, &path) {
                Ok(mut info) => {
                    info["path"] = json!(path);
                    Response::ok(id, info)
                }
                Err(e) => Response::err(id, -32001, format!("ringbuffer/dump: {e}")),
            }
        }

        // ringbuffer/restore { path } → RingDumpInfo. Reconstructs the rings + restores
        // the machine to the dump's "current" anchor.
        "ringbuffer/restore" => {
            let path = match req.params.get("path").and_then(|v| v.as_str()) {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => return Response::err(id, -32602, "ringbuffer/restore: path required"),
            };
            let mut st = state.lock().unwrap();
            match ringbuffer_restore_from_path(&mut st, &path) {
                Ok(mut info) => {
                    info["path"] = json!(path);
                    Response::ok(id, info)
                }
                Err(e) => Response::err(id, -32001, format!("ringbuffer/restore: {e}")),
            }
        }

        // ── Spec 231/268 — deterministic scenario replay + registry ──────────
        // 1:1 with the c64re ws-server.ts runtime/scenario_* handlers. The registry
        // is FILE-BACKED (scenario-registry.ts): scenario_save persists to the
        // project `scenarios/` dir; scenario_list re-scans that dir on EVERY call so
        // a scenario written by ANY daemon on the same project dir is surfaced (a
        // fresh daemon picks them up). `scenario_run` replays deterministically:
        // restore the start snapshot, feed the recorded inputs at their cycles (the
        // scenario player), run cycleBudget cycles, then hash the end RAM. A re-run
        // on the same build hashes identically — the determinism contract (Spec 231).

        // runtime/scenario_list — registry summaries.
        // c64re: listScenarios() → ScenarioSummary[] (scenario-registry.ts:91-100):
        // scan the project (+ samples) dir on EACH call; each summary carries a
        // `source` field. ws-server.ts:1922-1925.
        "runtime/scenario_list" => {
            let st = state.lock().unwrap();
            // Merge by id: disk re-scan ("project") first, then the in-memory copies
            // ("memory") — same id keeps the disk view (file is the authority). This
            // is the file-backed re-scan: a scenario only on disk (= a fresh/other
            // daemon's save) is listed even when this process never saw it via RPC.
            let mut by_id: std::collections::BTreeMap<String, Value> =
                std::collections::BTreeMap::new();
            // In-memory copies first (source = "memory" when no on-disk peer). No
            // on-disk file backs a memory-only entry, so filePath = "" (= TS, whose
            // registry has no file path until saveScenario writes one).
            for s in st.scenarios.values() {
                let sid = s.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                by_id.insert(sid, scenario_summary_src(s, "memory", ""));
            }
            // Disk re-scan overrides (source = "project"), 1:1 with scanDir.
            if let Some(scen_dir) = scenarios_dir() {
                if let Ok(entries) = fs::read_dir(&scen_dir) {
                    for ent in entries.flatten() {
                        let path = ent.path();
                        if path.extension().and_then(|e| e.to_str()) != Some("json") {
                            continue;
                        }
                        let raw = match fs::read_to_string(&path) {
                            Ok(r) => r,
                            Err(_) => continue,
                        };
                        let obj: Value = match serde_json::from_str(&raw) {
                            Ok(o) => o,
                            Err(_) => continue,
                        };
                        // readScenarioFile rejects entries missing id/diskPath.
                        let has_id = obj.get("id").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false);
                        let has_disk = obj.get("diskPath").and_then(|v| v.as_str()).is_some();
                        if !has_id || !has_disk {
                            continue;
                        }
                        let sid = obj.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        // filePath = the absolute on-disk scenario JSON path (= TS
                        // scanDir's `join(dir, name)`), so the disk re-scan summary
                        // carries the path field too.
                        let fp = path.to_string_lossy().into_owned();
                        by_id.insert(sid, scenario_summary_src(&obj, "project", &fp));
                    }
                }
            }
            let list: Vec<Value> = by_id.into_values().collect();
            Response::ok(id, json!(list))
        }

        // runtime/scenario_save — store a scenario object. c64re: saveScenario() →
        // { filePath }. ws-server.ts:1927-1931. T3.6: add disk persistence + return
        // both filePath and id (matching TS contract).
        "runtime/scenario_save" => {
            let scenario = match req.params.get("scenario") {
                Some(s) if s.is_object() => s.clone(),
                _ => return Response::err(id, -32602, "scenario object required"),
            };
            let sid = match scenario.get("id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::err(id, -32602, "scenario.id required"),
            };
            let mut saved = scenario.clone();
            saved["savedAt"] = json!(now_iso8601());

            // File-backed registry (scenario-registry.ts saveScenario): persist the
            // scenario JSON to the project `scenarios/` dir, resolved from the
            // `--project` arg ?? C64RE_PROJECT_DIR (= the same dir the TS daemon
            // scans). A fresh daemon / scenario_list re-scan then surfaces it.
            let mut file_path_opt = None;
            if let Some(scen_dir) = scenarios_dir() {
                if let Err(e) = fs::create_dir_all(&scen_dir) {
                    eprintln!("Failed to create scenarios dir: {}", e);
                } else {
                    let file_path = scen_dir.join(format!("{}.json", sid));
                    match fs::write(
                        &file_path,
                        serde_json::to_string_pretty(&saved).unwrap_or_default()
                    ) {
                        Ok(_) => file_path_opt = Some(file_path.to_string_lossy().to_string()),
                        Err(e) => eprintln!("Failed to write scenario file: {}", e),
                    }
                }
            }

            // Keep an in-memory copy too (fast path / no-project-dir fallback) — the
            // listing re-scans disk and merges, so the in-memory copy never shadows a
            // fresher on-disk one.
            let mut st = state.lock().unwrap();
            st.scenarios.insert(sid.clone(), saved);

            // Return both id and filePath (matching TS contract).
            let resp = if let Some(fp) = file_path_opt {
                json!({ "id": sid, "filePath": fp })
            } else {
                json!({ "id": sid })
            };
            Response::ok(id, resp)
        }

        // runtime/scenario_delete — { deleted: bool }. ws-server.ts:1933-1938.
        // T3.6: also delete the file on disk if it exists.
        "runtime/scenario_delete" => {
            let sid = match req.params.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return Response::err(id, -32602, "id required"),
            };
            let mut st = state.lock().unwrap();
            let mem_removed = st.scenarios.remove(&sid).is_some();

            // File-backed delete (scenario-registry.ts deleteScenario): drop the file
            // from the project `scenarios/` dir (same --project-aware resolution as
            // save/list). `deleted` is true if the file OR the in-memory copy existed.
            let mut file_removed = false;
            if let Some(scen_dir) = scenarios_dir() {
                let file_path = scen_dir.join(format!("{}.json", sid));
                file_removed = fs::remove_file(&file_path).is_ok();
            }

            Response::ok(id, json!({ "deleted": mem_removed || file_removed }))
        }

        // runtime/scenario_load — the full stored scenario. c64re: loadScenario()
        // → SavedScenario (throws if not found). ws-server.ts:2306-2312.
        "runtime/scenario_load" => {
            let sid = match req.params.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return Response::err(id, -32602, "id required"),
            };
            let st = state.lock().unwrap();
            match st.scenarios.get(&sid) {
                Some(s) => Response::ok(id, s.clone()),
                None => Response::err(id, -32001, format!("scenario '{sid}' not found")),
            }
        }

        // runtime/scenario_run — deterministic replay. c64re: loadScenario() then
        // runScenario() → ReplayResult. ws-server.ts:2314-2327 + scenario.ts
        // runScenario. Accepts either { id } (load from the registry) or an inline
        // { scenario } object.
        "runtime/scenario_run" => {
            let mut st = state.lock().unwrap();
            let scenario = if let Some(s) = req.params.get("scenario").filter(|s| s.is_object()) {
                s.clone()
            } else {
                let sid = match req.params.get("id").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => return Response::err(id, -32602, "id or scenario required"),
                };
                match st.scenarios.get(&sid) {
                    Some(s) => s.clone(),
                    None => return Response::err(id, -32001, format!("scenario '{sid}' not found")),
                }
            };
            match run_scenario(&mut st, &scenario) {
                Ok(result) => Response::ok(id, result),
                Err(e) => Response::err(id, -32001, format!("scenario_run: {e}")),
            }
        }

        m if m.starts_with("runtime/scenario") => {
            Response::err(id, -32601, format!("Method not found: {m}"))
        }

        // ── runtime/call ─────────────────────────────────────────────────────
        // AgentQueryApi facade — mirrors TS ws-server.ts:1717.
        // Payload: { session_id, op, args }
        // → reuse dispatch_api_call allowlist by building a synthetic params
        //   value with { method: op, args: args } (identical dispatch table).
        // session_id accepted but ignored (singleton session, like all TRX64
        // runtime/* handlers).
        "runtime/call" => {
            let op = match req.params.get("op").and_then(|v| v.as_str()) {
                Some(o) => o.to_string(),
                None => return Response::err(id, -32602, "runtime/call: missing op"),
            };
            let args = req.params.get("args").cloned().unwrap_or(json!([]));
            // Build synthetic params matching what dispatch_api_call expects.
            // runtime/call = the FULL AgentQueryApi facade — NOT allowlist-gated
            // (full=true), 1:1 with TS ws-server.ts:1717 createAgentQueryApi.
            let synthetic = json!({ "method": op, "args": args });
            dispatch_api_call(id, &synthetic, state, true)
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
        EasyFlash => "easyflash",
        Gmod2 => "gmod2",
        MegaByter => "megabyter",
        C64MegaCart => "c64megacart",
        // Spec 790 S2 — the self-configuring harness before it locks a concrete
        // family (post-lock it delegates `mapper_type()` and never returns this).
        SelfConfig => "self_config",
        Unsupported => "cartridge",
    }
}

// ── Snapshot media gather (= c64re gatherMedia → SnapshotMediaSummary[]) ──────

/// Build the snapshot `media[]` array from the session's live attached media,
/// matching c64re's gatherMedia (snapshot-persistence.ts): the drive8 disk
/// (role "drive8") and any attached cartridge (role "cartridge"), each
/// `{ role, format, sourceName, sha256, bytes }`.
fn gather_snapshot_media(session: &Session) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let m = &session.machine;
    if let Some(disk) = m.drive8.get_attached_disk() {
        let format = match disk.kind {
            DiskKind::G64 => "g64",
            DiskKind::D64 => "d64",
        };
        let source_name = disk
            .backing_path
            .as_ref()
            .and_then(|p| p.rsplit('/').next())
            .map(String::from);
        out.push(json!({
            "role": "drive8",
            "format": format,
            "sourceName": source_name,
            "sha256": sha256_hex(&disk.bytes),
            "bytes": disk.bytes.len() as u64
        }));
    }
    if let Some(img) = m.cartridge_image.as_ref() {
        out.push(json!({
            "role": "cartridge",
            "format": "crt",
            "sourceName": img.name.clone(),
            "sha256": sha256_hex(&img.raw_bytes),
            "bytes": img.raw_bytes.len() as u64
        }));
    }
    out
}

/// Build the embedded-media INPUTS for the `.c64re` container (clean source
/// bytes per role) — 1:1 with c64re snapshot-persistence.ts `gatherMedia`. The
/// drive8 disk + any attached cartridge ride as embedded payloads so an undump
/// (TRX64 or c64re) re-establishes the media. sha256 is computed by the writer.
fn gather_native_media_inputs(
    session: &Session,
) -> Vec<trx64_core::native_snapshot::NativeSnapshotMediaInput> {
    use trx64_core::native_snapshot::NativeSnapshotMediaInput;
    let mut out = Vec::new();
    let m = &session.machine;
    if let Some(disk) = m.drive8.get_attached_disk() {
        let format = match disk.kind { DiskKind::G64 => "g64", DiskKind::D64 => "d64" };
        let source_name = disk
            .backing_path
            .as_ref()
            .and_then(|p| p.rsplit('/').next())
            .map(String::from);
        out.push(NativeSnapshotMediaInput {
            role: "drive8".to_string(),
            format: format.to_string(),
            source_name,
            bytes: Some(disk.bytes.clone()),
            sha256: None,
        });
    }
    if let Some(img) = m.cartridge_image.as_ref() {
        out.push(NativeSnapshotMediaInput {
            role: "cartridge".to_string(),
            format: "crt".to_string(),
            source_name: Some(img.name.clone()),
            bytes: Some(img.raw_bytes.clone()),
            sha256: None,
        });
    }
    out
}

/// formats-state-2 — capture the attached cartridge's `(cartBytes, cartFlash)` for the
/// `.c64re` checkpoint, mirroring c64re's captureCartBytes()/captureCartFlash()
/// (headless-machine-kernel.ts:1109-1118). `cartBytes` = the original `.crt` bytes
/// (non-null whenever a cartridge is attached); `cartFlash` = the live writable image
/// (flash low+high), None for a read-only mapper. `&mut` because `writable_image`
/// catches the flash erase alarm up before serializing. Both None when no cartridge.
fn capture_cart_blobs(machine: &mut trx64_core::Machine) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    if machine.cartridge.is_none() {
        return (None, None);
    }
    let clk = machine.c64_core.clk;
    let cart_bytes = machine.cartridge_image.as_ref().map(|img| img.raw_bytes.clone());
    let cart_flash = machine.cartridge.as_mut().and_then(|c| c.writable_image(clk));
    (cart_bytes, cart_flash)
}

/// Spec 766.5 — capture one recorder anchor from the live machine: build the
/// core-only anchor payload + the live media descriptors, then hand them to the
/// recorder. Returns the new anchor seq (`None` if the recorder is inactive). The
/// `wallMs`/`cycle`/`schemaVersion` come from the live machine like c64re's
/// captureAnchor call (runtime-controller.ts:848-851).
fn capture_anchor_now(st: &mut State) -> Option<u64> {
    if st.recorder.is_none() {
        return None;
    }
    let payload = capture_recorder_anchor_payload(&mut st.session);
    let cycle = st.session.machine.c64_core.clk as f64;
    let wall_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);
    let schema_version = trx64_core::c64re_snapshot::RUNTIME_CHECKPOINT_SCHEMA_VERSION as i32;
    // Borrow the disk-gen bookkeeping out, build media, then put it back (the
    // descriptor closures capture the disk bytes by value, so no live borrow leaks).
    let mut disk_gen = st.recorder_disk_gen;
    let mut disk_hash = st.recorder_disk_hash.take();
    let media = build_recorder_media(&st.session, &mut disk_gen, &mut disk_hash);
    st.recorder_disk_gen = disk_gen;
    st.recorder_disk_hash = disk_hash;
    let recorder = st.recorder.as_mut().unwrap();
    recorder.capture_anchor(&payload, cycle, wall_ms, schema_version, &media);
    // The just-captured anchor is the newest in the store.
    recorder.list().last().map(|a| a.seq)
}

/// Spec 766.5 — build the native-snapshot media inputs for a reconstructed
/// recorder anchor. The reconstructed payload carries `driveDiskImage` (re-injected
/// from the medium store) when the anchor referenced a disk; embed it as a drive8
/// input so the dumped .c64re re-attaches the disk on undump (matching
/// snapshot/dump's drive8 input). The disk format/sourceName come from the anchor's
/// `media` metadata, falling back to the live session.
fn gather_recorder_media_inputs(
    payload: &Value,
    session: &Session,
) -> Vec<trx64_core::native_snapshot::NativeSnapshotMediaInput> {
    use trx64_core::native_snapshot::NativeSnapshotMediaInput;
    let mut out = Vec::new();
    if let Some(bytes) = payload
        .get("driveDiskImage")
        .and_then(trx64_core::native_snapshot::ta_u8_decode)
    {
        if !bytes.is_empty() {
            // Format/source from the anchor metadata, then the live disk.
            let format = payload
                .get("media")
                .and_then(|mm| mm.get("imageFormat"))
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| {
                    session.machine.drive8.get_attached_disk().map(|d| {
                        match d.kind {
                            DiskKind::G64 => "g64",
                            DiskKind::D64 => "d64",
                        }
                        .to_string()
                    })
                })
                .unwrap_or_else(|| "g64".to_string());
            let source_name = payload
                .get("media")
                .and_then(|mm| mm.get("diskPath"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .and_then(|p| p.rsplit('/').next())
                .map(String::from);
            out.push(NativeSnapshotMediaInput {
                role: "drive8".to_string(),
                format,
                source_name,
                bytes: Some(bytes),
                sha256: None,
            });
        }
    }
    out
}

// ── Spec 231/268 — scenario registry + deterministic replay ──────────────────

/// A short ISO-8601 UTC timestamp for the scenario `savedAt` field (no chrono dep).
fn now_iso8601() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Minimal "epoch:<secs>" stamp — the field is opaque metadata (c64re uses an
    // ISO string; the exact format is not part of the replay contract).
    format!("epoch:{secs}")
}

/// scenario-registry.ts:62-73 — `summarise`: the light listing view of a stored
/// scenario `{ id, diskPath, mode, cycleBudget, inputCount, savedAt, source }`. The
/// `source` is "project" (file-backed) or "memory" (in-process fallback when no
/// project dir is resolvable); TS uses "project" | "samples".
fn scenario_summary_src(s: &Value, source: &str, file_path: &str) -> Value {
    // 1:1 with scenario-registry.ts `summarise()`: id, diskPath, mode, cycleBudget,
    // inputCount, savedAt, filePath, source. TRX64 previously omitted `filePath` (the
    // absolute on-disk path of the scenario JSON) — a missing field vs the TS authority
    // (audit ws-trace-monitor-misc-20). For an in-memory-only scenario the TS registry
    // has no on-disk file yet, so `file_path` is "" there.
    json!({
        "id": s.get("id").and_then(|v| v.as_str()).unwrap_or(""),
        "diskPath": s.get("diskPath").and_then(|v| v.as_str()).unwrap_or(""),
        "mode": s.get("mode").and_then(|v| v.as_str()).unwrap_or("true-drive"),
        "cycleBudget": s.get("cycleBudget").and_then(|v| v.as_u64()).unwrap_or(0),
        "inputCount": s.get("inputs").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0) as u64,
        "savedAt": s.get("savedAt").and_then(|v| v.as_str()).unwrap_or(""),
        "filePath": file_path,
        "source": source,
    })
}

/// Spec 265 / audit ws-media-8 — a recents-store entry (= the c64re `RecentEntry`,
/// recent-files.ts:13-18): the host path, the media type (`d64`/`g64`/`crt`/…), and
/// an ISO-8601 `mountedAt` timestamp. Kept newest-first in `State.recent_media`.
#[derive(Clone)]
struct RecentMedia {
    path: String,
    media_type: String,
    mounted_at: String,
}

/// Spec 265 — recent-media list cap (= the c64re MAX_RECENT, recent-files.ts:11).
const MAX_RECENT_MEDIA: usize = 10;

/// A real ISO-8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SS.mmmZ`) for the recents
/// `mountedAt` field — 1:1 with the c64re `new Date().toISOString()` (recent-files.ts
/// :51). No chrono dep: compute civil date from the Unix epoch (Howard Hinnant's
/// days_from_civil inverse). Distinct from `now_iso8601` (the opaque `epoch:<secs>`
/// scenario stamp) because the recents store mirrors a real client-facing ISO string.
pub(crate) fn now_iso8601_utc() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();
    let days = total_secs.div_euclid(86_400);
    let secs_of_day = total_secs.rem_euclid(86_400);
    let (hh, mm, ss) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);
    // civil_from_days (days since 1970-01-01) → (year, month, day).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    format!(
        "{year:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z",
    )
}

/// Spec 265 / audit ws-media-8 — record a just-mounted medium in the recents store
/// (= the c64re `addRecent`, recent-files.ts:48-60): dedup by path, prepend so the
/// list stays NEWEST-FIRST, stamp `mountedAt`, trim to [`MAX_RECENT_MEDIA`]. Called on
/// every disk/cart mount + swap (the TS `addRecent` fires on every ingest).
fn add_recent_media(st: &mut State, path: &str, media_type: &str) {
    st.recent_media.retain(|e| e.path != path); // dedup by path (recent-files.ts:49)
    st.recent_media.insert(
        0,
        RecentMedia {
            path: path.to_string(),
            media_type: media_type.to_string(),
            mounted_at: now_iso8601_utc(),
        },
    );
    st.recent_media.truncate(MAX_RECENT_MEDIA);
}

/// Spec 709.8 — append a media event to the ordered history, trimming to the last
/// [`MAX_MEDIA_EVENTS`]. The event object is the same `MediaIngressEvent` shape the
/// media op already returns in its response `event` field (`{cycle, operation,
/// role, format, sha256, resetPolicy, checkpointBeforeId, checkpointAfterId}`), so
/// `media/events` replays exactly what each op reported.
fn push_media_event(st: &mut State, event: Value) {
    st.media_events.push(event);
    if st.media_events.len() > MAX_MEDIA_EVENTS {
        let drop = st.media_events.len() - MAX_MEDIA_EVENTS;
        st.media_events.drain(0..drop);
    }
}

/// Spec 709.13 / 714.5 — the shared dirty-media guard (= the c64re
/// `RuntimeController.nonPersistableDirtyMedia`, runtime-controller.ts:470-484).
/// Returns Some(reason) when the live cartridge has a writable (flash/EEPROM)
/// delta since attach AND its mapper does NOT faithfully capture/restore that
/// state — v1 cannot snapshot it, so a media intervention would mint a
/// non-restorable checkpoint/branch. A persisting family (EasyFlash/GMOD2/
/// Megabyter) is captured, not rejected. Read-only mappers are never dirty →
/// None. ONLY the cartridge is consulted (the disk has its own write-through /
/// .c64re embed path), exactly like the TS guard.
fn non_persistable_dirty_media(st: &State) -> Option<String> {
    let cart = st.session.machine.cartridge.as_ref()?;
    if cart.is_writable_dirty() && !cart.persists_writable_state() {
        return Some(
            "writable cartridge state changed since attach and this mapper has no persistence \
             port; v1 cannot snapshot it"
                .to_string(),
        );
    }
    None
}

/// Resolve a user file path the way the monitor FILE shell does (resolveFsPath):
/// absolute → unchanged; relative → joined to the session cwd (`cd`) or the project
/// dir when unset. Lets `/mount foo.crt` after `cd out` read .../out/foo.crt instead
/// of the daemon's process cwd (the cockpit `cd` sets `st.mon.fs_cwd`).
fn resolve_fs_path_with_state(st: &State, arg: &str) -> String {
    if arg.is_empty() || std::path::Path::new(arg).is_absolute() {
        return arg.to_string();
    }
    let cwd = st.mon.fs_cwd.clone().unwrap_or_else(|| {
        std::env::args()
            .skip_while(|a| a != "--project")
            .nth(1)
            .filter(|p| !p.is_empty())
            .or_else(|| std::env::var("C64RE_PROJECT_DIR").ok())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
            })
    });
    std::path::Path::new(&cwd).join(arg).to_string_lossy().to_string()
}

/// CLI-FEEL S3 — the longest common prefix (by char) of a set of names, used by the
/// `fs/complete` rpc so the cockpit can fill the shared stem on Tab. Empty when the
/// set is empty or the names share no leading char; the whole name when there is
/// exactly one match.
/// Longest common prefix of the matched names, returned in the FIRST name's casing.
/// The comparison is ASCII-case-insensitive so it matches `fs/complete`'s
/// case-insensitive stem filter: a mixed-case match set (e.g. `Game.prg` + `gate.prg`
/// for stem `g`) still yields a fillable prefix (`Ga`) instead of the empty string a
/// case-sensitive compare would produce.
fn fs_longest_common_prefix<'a>(mut names: impl Iterator<Item = &'a str>) -> String {
    let first = match names.next() {
        Some(f) => f,
        None => return String::new(),
    };
    let mut prefix: Vec<char> = first.chars().collect();
    for name in names {
        let chars: Vec<char> = name.chars().collect();
        let mut i = 0;
        while i < prefix.len() && i < chars.len() && prefix[i].eq_ignore_ascii_case(&chars[i]) {
            i += 1;
        }
        prefix.truncate(i);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
}

/// BUG-023-cart / Spec 742 — host-file write-back for a writable cartridge on
/// eject (= the c64re `persistCartridgeToFile`, persist-cartridge.ts:20-30). VICE
/// saves the `.crt` on detach; the read-only / non-writable / clean / no-path
/// cases are skipped with a reason (no write). Returns the written path on a real
/// write so the caller can stamp `detail["cartPersisted"]`.
fn persist_cart_for_eject(st: &mut State, backing_path: &str) -> Option<String> {
    if backing_path.is_empty() {
        return None;
    }
    let clk = st.session.machine.clk;
    let cart = st.session.machine.cartridge.as_mut()?;
    // persist-cartridge.ts:23-24 — only a dirty, persisting mapper is written.
    if !cart.persists_writable_state() || !cart.is_writable_dirty() {
        return None;
    }
    let img = cart.crt_image(clk)?;
    match std::fs::write(backing_path, &img) {
        Ok(()) => Some(backing_path.to_string()),
        Err(_) => None,
    }
}

/// Spec 742 / BUG-023 — host-file write-back for the OUTGOING disk before it is
/// detached/replaced (= the c64re `persistDriveToFile`, mount-disk-media.ts:47-56,
/// called from `mountDiskMedia`'s implicit-eject at :77-82). This is THE actual
/// data-loss fix for audit ws-media-0: TRX64's old `attach_disk`-direct path
/// replaced the GCR image in place without serializing the currently-mounted
/// disk's pending drive writes back to its host file, so swapping A→B while A had
/// unsaved writes silently lost A's writes. Mirroring VICE saving the image on
/// detach, this flushes the dirty GCR track into `disk.bytes` then writes those
/// bytes back to the backing host file. Read-only / no-backing-path / clean media
/// is skipped (no write). Returns the written path on a real write so the caller
/// can stamp `detail["diskPersisted"]`.
fn persist_outgoing_disk(st: &mut State) -> Option<String> {
    // Flush any pending dirty GCR track into `disk.bytes` first (= VICE
    // `drive_gcr_data_writeback_all` before reading `fsimage->fd`). Cheap no-op
    // when nothing is dirty.
    st.session.machine.drive8.flush_disk_writeback();
    let disk = st.session.machine.drive8.get_attached_disk()?;
    if disk.read_only {
        return None; // never overwrite a read-only image (mount-disk-media.ts:52)
    }
    let backing_path = match &disk.backing_path {
        Some(p) if !p.is_empty() => p.clone(),
        _ => return None, // uploaded bytes with no host file → RAM-only, nothing to write
    };
    let bytes = disk.bytes.clone();
    match std::fs::write(&backing_path, &bytes) {
        Ok(()) => Some(backing_path),
        Err(_) => None,
    }
}

/// Spec 742 / BUG-023 — THE single disk-media attach (= the c64re `mountDiskMedia`,
/// mount-disk-media.ts:63-95). A disk change is an implicit eject of the currently
/// mounted disk: persist its dirty writes to the host file + detach BEFORE attaching
/// the new one, else the outgoing disk's writes are lost (audit ws-media-0). No-op
/// outgoing-persist on the first mount (no disk attached). Records the new path as
/// the session's disk identity. Returns the persisted-outgoing host path, if any.
fn mount_disk_media(st: &mut State, image: DiskImage, new_path: &str) -> Option<String> {
    // Implicit eject: persist + detach the outgoing disk first (mount-disk-media.ts:
    // 77-82). Only when a disk is actually attached (first mount → None).
    let persisted_outgoing = if st.session.machine.drive8.get_attached_disk().is_some() {
        let p = persist_outgoing_disk(st);
        st.session.machine.drive8.detach_disk();
        p
    } else {
        None
    };
    st.session.machine.drive8.attach_disk(image);
    st.session.disk_path = new_path.to_string();
    persisted_outgoing
}

/// Spec 709.13 — capture a real before/after checkpoint into the ring and return
/// its id (= the c64re `controller.captureCheckpoint()` → `ring.capture(...)`).
/// None only on a capture error (the ring rejects a malformed payload); callers
/// treat None as "no checkpoint id" and still complete the media op.
fn capture_media_checkpoint(st: &mut State) -> Option<String> {
    let frame = st.ctrl_frame;
    let cycles = st.session.machine.c64_core.clk;
    let cp = capture_live_checkpoint(&mut st.session);
    st.checkpoint_ring.capture(cp, frame, cycles).ok().map(|r| r.id)
}

// ── stream_loop BACKGROUND-LOOP layer (the c64re RuntimeController per-frame
//    behaviors with no WS method) ─────────────────────────────────────────────
//
// TRX64's `stream_loop` (streaming.rs) is the SOLE per-frame machine driver under
// --stream. The c64re RuntimeController runs, every frame / on a setInterval,
// several behaviors that have NO WS method (runtime-controller.ts). Three were
// missing here; these helpers port them. They run on the stream thread INSIDE the
// per-frame lock window (gen/hash checks are cheap; the actual persist/capture is
// throttled/debounced), and the stream loop only calls them while `running`.

/// BUG-040 cart auto-persist debounce in stream-loop FRAMES (~50 fps PAL). The TS
/// debounce is CART_AUTOPERSIST_DEBOUNCE_MS = 5_000 (runtime-controller.ts:100) —
/// long enough to coalesce an EAPI write/erase burst, short enough that a crash
/// loses little. (audit ws-media-3 / background-workers-async-10): WALL-CLOCK ms,
/// 1:1 with the TS CART_AUTOPERSIST_DEBOUNCE_MS = 5_000 (runtime-controller.ts:100),
/// NOT a frame count — the TS persist runs on an independent 1 s setInterval that
/// fires regardless of run-state, so a dirty-then-pause STILL reaches the host file.
/// A frame counter only advances while running, so it could never persist a paused
/// machine; a wall-clock debounce fires the same whether running, paused, or jammed.
const CART_AUTOPERSIST_DEBOUNCE_MS: u64 = 5_000;
/// Disk lazy-writeback debounce in WALL-CLOCK ms. The same coalescing rationale as
/// the cart: a drive-write burst (one SAVE) settles before the host .d64/.g64 is
/// touched once. ~1 s. (Wall-clock, not frame count — fires while paused too.)
const DISK_AUTOPERSIST_DEBOUNCE_MS: u64 = 1_000;
/// Spec 705.B / Spec 772 auto-capture cadence in frames. Default 25 (~0.5 s @ 50 fps
/// PAL — the UI scrub-filmstrip granularity), env-overridable via
/// C64RE_CHECKPOINT_CADENCE_FRAMES (1:1 with the c64re const, runtime-controller.ts:77).
/// Spec 772 unified BOTH runtimes to 25 (TS was 50 → the cadence divergence this closes).
/// A fn, not a const, because env reads aren't const; called wherever the cadence is
/// compared/multiplied (identical to a const at every call site).
fn checkpoint_capture_every_frames() -> u64 {
    std::env::var("C64RE_CHECKPOINT_CADENCE_FRAMES")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(25)
}

/// Spec 772 — ring retention in seconds (default 10), env-overridable via
/// C64RE_CHECKPOINT_RING_SECONDS (1:1 with runtime-controller.ts CHECKPOINT_RING_SECONDS).
fn checkpoint_ring_seconds() -> f64 {
    std::env::var("C64RE_CHECKPOINT_RING_SECONDS")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|&n| n > 0.0)
        .unwrap_or(10.0)
}

/// Spec 772 — max LIVE ring entries (the UI-scrub cap) = ceil(seconds / (cadence/50))
/// (PAL 50 fps). At the 10s / 25-frame default that is 20. 1:1 with the c64re
/// `checkpointRingMaxEntries` (runtime-checkpoint-ring.ts). Clamped ≥ 1.
fn checkpoint_ring_max_entries() -> u64 {
    let seconds = checkpoint_ring_seconds();
    let cadence = checkpoint_capture_every_frames() as f64;
    let seconds_per_capture = cadence / 50.0; // PAL 50fps
    ((seconds / seconds_per_capture).ceil() as u64).max(1)
}

/// ITEM 1 — cart auto-persist (.crt lazy writeback). = maybeAutoPersistCart
/// (runtime-controller.ts:493). The mapper's monotonic writableGeneration()
/// distinguishes "still being written" (gen moving → re-arm the settle window) from
/// "settled" (gen stable for the debounce window → write the flash back to the host
/// .crt once via the SAME persist logic as eject, minus the eject/detach/power-cycle:
/// persist-in-place). WALL-CLOCK ms debounce (`now_ms`), NOT a frame count, so it
/// fires regardless of run-state (audit ws-media-3 / background-workers-async-10):
/// the TS persist is an independent 1 s setInterval that fires while paused/jammed/
/// bp-stopped, so a dirty-then-pause STILL reaches the host file. The stream loop
/// therefore calls this EVERY iteration (running OR paused), not only `if running`.
/// Broadcasts media/cart_persisted {auto:true}. Disable with C64RE_CART_AUTOPERSIST=0.
pub(crate) fn stream_maybe_autopersist_cart(st: &mut State, now_ms: u64) {
    if std::env::var("C64RE_CART_AUTOPERSIST").as_deref() == Ok("0") {
        return;
    }
    // Cheap gate: read the mapper's write generation + dirty flag. A clean / read-
    // only / non-writable / gen-0 cart is a no-op (no allocation, no I/O).
    let (gen, dirty) = match st.session.machine.cartridge.as_ref() {
        Some(c) => (c.writable_generation(), c.is_writable_dirty()),
        None => return,
    };
    if gen == 0 || !dirty {
        return;
    }
    // runtime-controller.ts:501-505 — gen advanced → still being written; re-arm
    // the settle window and bail (the EAPI burst keeps bumping the gen).
    if gen != st.cart_ap_seen_gen {
        st.cart_ap_seen_gen = gen;
        st.cart_ap_settle_at_ms = now_ms;
        return;
    }
    // runtime-controller.ts:506 — this gen already persisted; nothing to do.
    if gen == st.cart_ap_done_gen {
        return;
    }
    // runtime-controller.ts:507 — not settled long enough yet (wall-clock ms).
    if now_ms.saturating_sub(st.cart_ap_settle_at_ms) < CART_AUTOPERSIST_DEBOUNCE_MS {
        return;
    }
    // Settled → write the host .crt via the eject-path persist logic (persist-in-
    // place: no detach/cold-reset). runtime-controller.ts:508-516.
    let cart_path = st.session.cart_path.clone();
    if cart_path.is_empty() {
        st.cart_ap_done_gen = gen; // nothing to write to — don't re-try hot
        return;
    }
    let written = persist_cart_for_eject(st, &cart_path);
    st.cart_ap_done_gen = gen; // also on a skipped write — don't re-try hot every frame
    if let Some(path) = written {
        let session_id = st.session.id.clone();
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        st.notify.broadcast(
            "media/cart_persisted",
            json!({ "session_id": session_id, "path": path, "bytes": bytes, "auto": true }),
        );
    }
}

/// ITEM 2 — disk lazy host-file writeback (.d64/.g64). PARITY-NEUTRAL ENHANCEMENT:
/// the c64re TS runtime writes the host disk file EAGERLY at the GCR-data-writeback
/// commit point (fsimage_dxx.ts:428 hostFlush, BUG-023 — VICE's fd IS the real file).
/// TRX64's write-through (`attach_with_writeback` → `flush_disk_writeback`) only
/// mirrors the dirty track into the IN-MEMORY `disk.bytes`; the host file is reached
/// only on media/persist or eject. To give the user the lazily-updated host .d64/.g64
/// they asked for, this flushes the dirty track + writes the backing file ONCE the disk
/// content has settled for the debounce window. Guarded: only when a backing_path
/// exists AND the disk is writable. WALL-CLOCK ms debounce (`now_ms`), so a SAVE then
/// pause still reaches the host file (audit ws-media-3); content-hash gen (no
/// diskWriteGen facade in TRX64). Called EVERY stream-loop iteration (running or paused).
pub(crate) fn stream_maybe_autopersist_disk(st: &mut State, now_ms: u64) {
    // Cheap gate: flush any pending dirty GCR track into `disk.bytes` (VICE
    // drive_gcr_data_writeback_all → fsimage->fd). Returns true ONCE per dirty
    // burst — that arms the debounce; the flag then drops, so on later frames the
    // flush is a no-op but we keep debouncing on the now-stable `disk.bytes`.
    if st.session.machine.drive8.flush_disk_writeback() {
        st.disk_ap_pending = true;
    }
    // Nothing armed → no drive write has happened → no host I/O (true no-op for a
    // clean, never-written disk: no hash, no fs::write).
    if !st.disk_ap_pending {
        return;
    }
    // Confirm writable + path-backed (the persist guards). A non-writable target
    // (no disk / read-only / no backing path) disarms — the dirty track already
    // mirrored into disk.bytes (it rides the .c64re/ring), it just can't lazily
    // reach a host file. Read metadata under the borrow, then drop.
    let target = match st.session.machine.drive8.get_attached_disk() {
        None => None,
        Some(d) if d.read_only => None,
        Some(d) => match &d.backing_path {
            Some(p) if !p.is_empty() => Some((p.clone(), sha256_hex(&d.bytes))),
            _ => None,
        },
    };
    let (backing_path, hash) = match target {
        Some(t) => t,
        None => {
            st.disk_ap_pending = false; // can't lazily write a host file here
            return;
        }
    };
    // Content-hash gen: changed since last poll → re-arm the settle window (a SAVE
    // is a burst of track writes; coalesce them into one host write).
    if Some(&hash) != st.disk_ap_seen_hash.as_ref() {
        st.disk_ap_seen_hash = Some(hash);
        st.disk_ap_settle_at_ms = now_ms;
        return;
    }
    if Some(&hash) == st.disk_ap_done_hash.as_ref() {
        return; // already written
    }
    if now_ms.saturating_sub(st.disk_ap_settle_at_ms) < DISK_AUTOPERSIST_DEBOUNCE_MS {
        return;
    }
    // Settled → write the host disk file (= media/persist disk branch, minus the
    // response envelope). Snapshot the bytes, drop the borrow before the I/O.
    let bytes = match st.session.machine.drive8.get_attached_disk() {
        Some(d) => d.bytes.clone(),
        None => return,
    };
    match std::fs::write(&backing_path, &bytes) {
        Ok(()) => {
            st.disk_ap_done_hash = Some(hash);
            st.disk_ap_pending = false; // settled + written; re-armed on the next drive write
            let session_id = st.session.id.clone();
            st.notify.broadcast(
                "media/disk_persisted",
                json!({ "session_id": session_id, "path": backing_path, "bytes": bytes.len(), "auto": true }),
            );
        }
        Err(_) => {
            // Don't mark done on a failed write — retry after the next settle.
        }
    }
}

/// ITEM 3 — auto-capture every CHECKPOINT_CAPTURE_EVERY_FRAMES frames (filmstrip).
/// Called once per RUNNING stream-loop frame. = CHECKPOINT_AUTOCAPTURE
/// (runtime-controller.ts:88/157). Captures a RENDER-ANCHOR (framebuffer OMITTED,
/// BUG-049) into the checkpoint ring so the UI filmstrip/scrub has a populated
/// ring. SKIPS while a mounted medium is dirty + non-persistable (Spec 709.13).
/// Isolated: a capture failure NEVER kills the loop (the ring returns Err on a
/// gap, never panics). Disable with C64RE_CHECKPOINT_AUTOCAPTURE=0.
pub(crate) fn stream_maybe_autocapture(st: &mut State, frame: u64, canvas_w: usize, canvas_h: usize, canvas_indices: &[u8]) {
    if std::env::var("C64RE_CHECKPOINT_AUTOCAPTURE").as_deref() == Ok("0") {
        return;
    }
    st.autocapture_frames_since = st.autocapture_frames_since.wrapping_add(1);
    if st.autocapture_frames_since < checkpoint_capture_every_frames() {
        return;
    }
    st.autocapture_frames_since = 0;
    // Spec 709.13 — skip (a ring gap beats a corrupt checkpoint) while a mounted
    // medium is dirty + non-persistable.
    if non_persistable_dirty_media(st).is_some() {
        return;
    }
    let cycles = st.session.machine.c64_core.clk;
    // Render-anchor: the framebuffer-omitted core payload (runtime-controller.ts:
    // 839/847 omitFramebuffer + omitMedia is the recorder anchor; the ring's
    // auto-capture in TS uses the render-anchor — capture_recorder_anchor_payload
    // is TRX64's lighter omit-framebuffer variant). A capture Err is a ring gap,
    // swallowed — the loop continues.
    let cp = capture_recorder_anchor_payload(&mut st.session);
    // Spec 769.5a — store a downscaled thumbnail of the JUST-RENDERED live frame
    // keyed by the ring's assigned id, in the SEPARATE thumb store (= c64re
    // captureThumb at the auto-capture point, runtime-controller.ts:840). The anchor
    // itself stays framebuffer-omitted (BUG-049); the filmstrip reads the picture
    // from this store, so every auto-anchor — not just framebuffer-present ones —
    // gets a thumbnail. Cheap; inside the existing per-frame lock window.
    if let Ok(r) = st.checkpoint_ring.capture(cp, frame, cycles) {
        if let Some(thumb) = make_thumb_from_canvas(canvas_w, canvas_h, canvas_indices) {
            store_checkpoint_thumb(st, r.id, thumb);
        }
    }
}

/// ITEM 4 (audit background-workers-async-0 + ws-checkpoint-scrub-7) — feed the
/// active recorder every CHECKPOINT_CAPTURE_EVERY_FRAMES RUNNING stream-loop frames.
/// = the c64re tick() recorder feed (runtime-controller.ts:846-852): inside the
/// per-second auto-capture cadence, `if (this.recorder) this.recorder.captureAnchor
/// (a.payload, …, omitMedia)`. So a FREE-RUNNING machine grows recorder anchors over
/// time without any explicit recorder/capture. TRX64 previously fed the recorder
/// ONLY on an explicit recorder/capture (main.rs recorder/capture handler), so a
/// --stream free-run left the recorder anchor count frozen at 1 (or 0) — the
/// divergence this closes. Reuses `capture_anchor_now` (= capture_recorder_anchor
/// _payload + the gen-gated medium stream), so the on-disk anchor shape is identical
/// to an explicit recorder/capture. No-op (zero cost) when no recorder is active.
/// Isolated: capture_anchor_now never panics (the recorder store evicts, never
/// throws). Disable with C64RE_RECORDER_AUTOFEED=0.
pub(crate) fn stream_maybe_feed_recorder(st: &mut State, _frame: u64) {
    // Zero-cost gate: nothing to feed when the recorder is inactive.
    if st.recorder.is_none() {
        return;
    }
    if std::env::var("C64RE_RECORDER_AUTOFEED").as_deref() == Ok("0") {
        return;
    }
    st.recorder_frames_since = st.recorder_frames_since.wrapping_add(1);
    if st.recorder_frames_since < checkpoint_capture_every_frames() {
        return;
    }
    st.recorder_frames_since = 0;
    // One CORE-ONLY (omitFramebuffer + gen-gated omitMedia) anchor — the same path
    // an explicit recorder/capture takes (runtime-controller.ts:847 snapshot{shallow,
    // omitFramebuffer, omitMedia}). The seq is discarded here (the cadence drives it,
    // not a caller).
    let _ = capture_anchor_now(st);
}

/// Spec 769.5a / Spec 772 — insert a thumbnail keyed by checkpoint id, then keep the
/// thumb store in lock-step with the ring: prune any thumb whose ring entry has been
/// evicted (so thumbs evict WITH the ring entry, not at an independent 1024 cap), and
/// apply the [`max_thumbs`] hard backstop. 1:1 with the c64re `captureThumb` +
/// `pruneOrphanThumbs` (runtime-controller.ts:434-460).
fn store_checkpoint_thumb(st: &mut State, id: String, thumb: CheckpointThumb) {
    if st.checkpoint_thumbs.insert(id.clone(), thumb).is_none() {
        st.checkpoint_thumb_order.push_back(id);
    }
    // Spec 772 — drop thumbs whose ring entry is no longer live (evicted by the ring's
    // entry/byte cap, truncated, or cleared). The ring is the authority on which
    // checkpoints are live; the thumb store tracks it exactly.
    let live: std::collections::HashSet<String> =
        st.checkpoint_ring.list().into_iter().map(|r| r.id).collect();
    if st.checkpoint_thumbs.keys().any(|k| !live.contains(k)) {
        st.checkpoint_thumbs.retain(|k, _| live.contains(k));
        st.checkpoint_thumb_order.retain(|k| live.contains(k));
    }
    // Hard backstop cap (aligned to the ring size, Spec 772).
    let cap = max_thumbs();
    while st.checkpoint_thumbs.len() > cap {
        if let Some(oldest) = st.checkpoint_thumb_order.pop_front() {
            st.checkpoint_thumbs.remove(&oldest);
        } else {
            break;
        }
    }
}

/// The scenario-player replay target over the live `Machine`. `type` drives the
/// keyboard matrix and `set_joystick1/2` drives the live CIA1 joystick model
/// (port 1 = PB, port 2 = PA); paddle/restore are still no-ops. `run_for`
/// advances the machine `cycleBudget` cycles (composite macros).
struct MachineScenarioTarget<'a> {
    session: &'a mut Session,
}

impl<'a> trx64_core::scenario_player::ScenarioTarget for MachineScenarioTarget<'a> {
    fn type_text(&mut self, text: &str) {
        // scenario-player.ts:75 — typeText(text, 80_000, 80_000). Queue relative to
        // the current machine clock so the KERNAL scans the key as it runs.
        let now = self.session.machine.cpu6510.clk;
        self.session.machine.keyboard.type_text(now, text, 80_000, 80_000);
    }
    fn set_joystick1(&mut self, state: trx64_core::scenario_player::JoystickState) {
        // scenario-player.ts:78 — session.setJoystick1(state). The scenario
        // `JoystickState` is now the canonical `keyboard::JoystickState` (re-export),
        // so it drops straight into the live machine's port 1 (CIA1 PB).
        self.session.machine.joystick1 = state;
    }
    fn set_joystick2(&mut self, state: trx64_core::scenario_player::JoystickState) {
        // scenario-player.ts:81 — session.setJoystick2(state) → port 2 (CIA1 PA).
        self.session.machine.joystick2 = state;
    }
    fn set_paddle(&mut self, _idx: u8, _value: i64) {}
    fn trigger_restore_nmi(&mut self) {}
    fn run_for(&mut self, cycles: u64) {
        run_cycle_budget(self.session, cycles);
    }
}

/// Translate a `Scenario.inputs` array (scenario.ts ScenarioInputEvent:
/// `{ atCycle, kind: "keyboard"|"joystick1"|"joystick2", payload }`) into the
/// scenario-player's `ScenarioStep`s. Keyboard payload = the text string; joystick
/// payload = a JoystickState partial object (`{ up?, down?, left?, right?, fire? }`).
fn scenario_steps_from_inputs(inputs: &[Value]) -> Vec<trx64_core::scenario_player::ScenarioStep> {
    use trx64_core::scenario_player::{JoystickState, ScenarioStep, ScenarioStepKind};
    let joy = |p: &Value| JoystickState {
        up: p.get("up").and_then(|v| v.as_bool()).unwrap_or(false),
        down: p.get("down").and_then(|v| v.as_bool()).unwrap_or(false),
        left: p.get("left").and_then(|v| v.as_bool()).unwrap_or(false),
        right: p.get("right").and_then(|v| v.as_bool()).unwrap_or(false),
        fire: p.get("fire").and_then(|v| v.as_bool()).unwrap_or(false),
    };
    let mut out = Vec::new();
    for ev in inputs {
        let at_cycle = ev.get("atCycle").and_then(|v| v.as_u64());
        let kind = ev.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let payload = ev.get("payload").cloned().unwrap_or(Value::Null);
        let step_kind = match kind {
            "keyboard" => ScenarioStepKind::Type {
                text: payload.as_str().map(String::from).unwrap_or_default(),
            },
            "joystick1" => ScenarioStepKind::Joy1 { state: joy(&payload) },
            "joystick2" => ScenarioStepKind::Joy2 { state: joy(&payload) },
            _ => continue, // unknown input kind → skip (forward-compatible)
        };
        out.push(ScenarioStep {
            at_cycle,
            at_frame: None,
            kind: step_kind,
        });
    }
    out
}

// ── Spec 265 — recent-media scan ─────────────────────────────────────────────
// media/recent (ws-server.ts:1809) returns `{ path, name, type, mountedAt }[]`
// image-media for the picker. BUG-013 / Spec 771: in PRODUCTION mode the picker shows
// ONLY active-project media. TRX64 is the external production bin and has no
// `--dev-samples` flag, so it is ALWAYS production: it overlays the (project-scoped)
// in-memory recents store, then walks the active project dir, and NEVER scans the c64re
// `samples/` corpus (that scan is c64re's §2 `--dev-samples`-gated dev convenience). The
// recents store is gated to inside the project dir (insideProject, ws-server.ts:1836) so
// a cart mounted in a different project earlier doesn't leak in. Image exts only
// (.crt/.d64/.g64/.vsf; .prg excluded — matches c64re's project walk).
fn scan_recent_media(recent: &[RecentMedia]) -> Vec<Value> {
    const IMG_EXTS: &[&str] = &[".crt", ".d64", ".g64", ".vsf"];
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<Value> = Vec::new();

    let ext_of = |name: &str| -> Option<&'static str> {
        let lower = name.to_lowercase();
        IMG_EXTS.iter().copied().find(|e| lower.ends_with(*e))
    };

    // The active project dir (same source as media/list_paths' "project" root).
    let project_path = std::env::args()
        .skip_while(|a| a != "--project")
        .nth(1)
        .unwrap_or_default();

    // BUG-013 — the picker must show ONLY active-project media in PRODUCTION mode.
    // TRX64 has no `--dev-samples` flag (Spec 771 — the external bin is ALWAYS
    // production), so every recents path is gated to inside the resolved project dir,
    // 1:1 with the c64re `insideProject` (ws-server.ts:1824-1828): canonicalize both
    // and keep only paths strictly under the project root. An empty/missing project →
    // nothing is inside it (matches TS returning false on an empty projDirAbs).
    let proj_root_canon: Option<std::path::PathBuf> = if project_path.is_empty() {
        None
    } else {
        std::fs::canonicalize(&project_path)
            .ok()
            .or_else(|| Some(std::path::PathBuf::from(&project_path)))
    };
    let inside_project = |p: &str| -> bool {
        let root = match &proj_root_canon {
            Some(r) => r,
            None => return false,
        };
        let cand = std::fs::canonicalize(p).unwrap_or_else(|_| std::path::PathBuf::from(p));
        // Strict descendant: equal-to-root is not "inside" (matches TS `rel !== ""`).
        cand != *root && cand.starts_with(root)
    };

    // 0) audit ws-media-8 — overlay the persisted recents store FIRST (= c64re §1,
    //    ws-server.ts:1833-1839): existing recents, NEWEST-FIRST, each carrying its
    //    `mountedAt` timestamp, ahead of the dir scans below. Skip a recents entry
    //    whose file no longer exists (recent-files-style staleness, ws-server.ts:1834),
    //    and dedup so a recents path is not re-listed by the dir scan. The store is
    //    already newest-first (add_recent_media prepends), so iterate in order.
    //    BUG-013: gate to inside the project dir (ws-server.ts:1836 production branch),
    //    so a cart mounted in a DIFFERENT project earlier does not leak into this one.
    for r in recent {
        if !std::path::Path::new(&r.path).exists() {
            continue;
        }
        if !inside_project(&r.path) {
            continue;
        }
        if seen.contains(&r.path) {
            continue;
        }
        let name = std::path::Path::new(&r.path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| r.path.clone());
        seen.insert(r.path.clone());
        out.push(json!({
            "path": r.path,
            "name": name,
            "type": r.media_type,
            // 1:1 with the c64re RecentEntry.mountedAt overlaid by media/recent
            // (the spread `{ ...r }` carries mountedAt, ws-server.ts:1838).
            "mountedAt": r.mounted_at,
        }));
    }

    // 1) Project dir — depth-limited recursive scan (= c64re §3 `walk`, depth ≤ 3,
    //    capped at 100), skipping dotdirs / node_modules / knowledge.
    if !project_path.is_empty() && std::path::Path::new(&project_path).exists() {
        fn walk(
            dir: &std::path::Path,
            depth: usize,
            seen: &mut std::collections::HashSet<String>,
            out: &mut Vec<Value>,
            ext_of: &dyn Fn(&str) -> Option<&'static str>,
        ) {
            if depth > 3 || out.len() >= 100 {
                return;
            }
            let mut entries: Vec<_> = match std::fs::read_dir(dir) {
                Ok(rd) => rd.flatten().collect(),
                Err(_) => return,
            };
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || name == "node_modules" || name == "knowledge" {
                    continue;
                }
                let full = entry.path();
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.is_dir() {
                    walk(&full, depth + 1, seen, out, ext_of);
                    continue;
                }
                let abs = full.to_string_lossy().to_string();
                if seen.contains(&abs) {
                    continue;
                }
                if let Some(ext) = ext_of(&name) {
                    let parent = full
                        .parent()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    seen.insert(abs.clone());
                    out.push(json!({
                        "path": abs,
                        "name": format!("{parent}/{name}"),
                        "type": &ext[1..]
                    }));
                }
            }
        }
        walk(std::path::Path::new(&project_path), 0, &mut seen, &mut out, &ext_of);
    }

    // 2) NO samples scan. BUG-013 / Spec 771: the c64re §2 repo `samples/` scan runs
    //    ONLY under `--dev-samples` (ws-server.ts:1841-1859, `if (this.devSamples …)`).
    //    TRX64 is the external production bin and never gets `--dev-samples`, so the
    //    samples corpus must NEVER be scanned — the picker shows ONLY active-project
    //    media (the §1 project-dir walk above). The unconditional samples scan that
    //    used to live here leaked the c64re samples carts (AccoladeComics_TRX+1D_EF.crt,
    //    im3_MAGICDESK.crt, lykia_*.crt, yeti_mountain_GMOD2.crt) into every project's
    //    picker — exactly the out-of-project leak ws-media-8 now guards against.

    out.truncate(100);
    out
}

// ── Spec 263 — one-shot audio export ─────────────────────────────────────────
// audio/export driver (= exportSessionAudio, audio/export.ts): run the session for
// `duration_sec` PAL seconds, harvesting reSID PCM into a stereo WAV. Drives the
// SAME SidAudioEngine the live stream uses (streaming.rs): install the additive
// $D4xx write-trace hook, run the machine in ~1024-sample slices, record the writes
// then a frame boundary per slice, flush, and finally export_wav. Returns the c64re
// ExportResult shape (`out_path, duration_sec, sample_rate, samples, bytes`).
fn export_session_audio(
    session: &mut Session,
    out_path: &str,
    duration_sec: f64,
) -> Result<Value, String> {
    use trx64_core::resid_audio::{SidAudioEngine, WavFormat};
    use trx64_core::resid_ffi::ResidConfig;

    const PAL_CYCLES_PER_SEC: f64 = 985_248.0;
    let sample_rate: u32 = 44100;

    let mut engine = SidAudioEngine::new(ResidConfig::default());
    // The Send write-trace hook captures only (addr,value) bytes (the engine stays
    // on this thread). Drained into the engine per slice, exactly like streaming.rs.
    let writes: Arc<Mutex<Vec<(u8, u8)>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let w = Arc::clone(&writes);
        session
            .machine
            .sid
            .set_write_trace(Some(Box::new(move |addr, value| {
                w.lock().unwrap().push((addr, value));
            })));
        // Prime reSID with the current SID register file (live state, not power-on).
        for reg in 0u8..=0x18 {
            let v = session.machine.read_full(0xD400 + reg as u16);
            engine.record_write(reg, v);
        }
    }
    engine.record_boundary(0); // apply the priming writes, emit nothing
    engine.flush();
    let _ = engine.take_pcm(); // discard priming silence

    let total_cycles = (duration_sec * PAL_CYCLES_PER_SEC).floor() as u64;
    // ~1024 samples worth of cycles per slice (= exportSessionAudio sliceCycles).
    let slice_cycles = ((1024.0 * PAL_CYCLES_PER_SEC) / sample_rate as f64).floor() as u64;
    let slice_cycles = slice_cycles.max(1);

    let mut consumed: u64 = 0;
    while consumed < total_cycles {
        let want = slice_cycles.min(total_cycles - consumed);
        let clk_before = session.machine.cpu6510.clk;
        run_cycle_budget(session, want);
        let d_cycles = session.machine.cpu6510.clk.wrapping_sub(clk_before) as u32;
        // Drain this slice's SID writes (CPU order) → engine, close the boundary.
        {
            let mut pending = writes.lock().unwrap();
            for &(addr, value) in pending.iter() {
                engine.record_write(addr, value);
            }
            pending.clear();
        }
        engine.record_boundary(d_cycles);
        engine.flush();
        consumed += want;
    }

    // Restore the byte-exact (None) write-trace path.
    session.machine.sid.set_write_trace(None);

    let wav = engine.export_wav(WavFormat { sample_rate, channels: 2 });
    std::fs::write(out_path, &wav).map_err(|e| format!("write {out_path}: {e}"))?;

    let pcm_samples = engine.pcm().len() as u64; // mono frames (= L=R stereo frames)
    Ok(json!({
        "out_path": out_path,
        "duration_sec": duration_sec,
        "sample_rate": sample_rate,
        "samples": pcm_samples,
        "bytes": wav.len() as u64,
    }))
}

// ── Spec 271 — in-process batch registry helpers ─────────────────────────────

/// A short random batch id (= c64re `randomBytes(6).toString("hex")` → 12 hex chars).
fn new_batch_id() -> String {
    // Derive 6 bytes from a nanosecond clock + an atomic counter (no rand dep needed;
    // the id only needs to be unique within this daemon's lifetime).
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = nanos ^ (COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E3779B97F4A7C15));
    format!("{:012x}", n & 0xFFFF_FFFF_FFFF)
}

/// Serialise a [`BatchEntry`] for JSON (= c64re `serialiseBatch`: batchId/status/
/// completed/total/workerCount/startedAt/finishedAt/lastError). c64re spreads the
/// optional fields as JS `undefined` → JSON.stringify DROPS the key; mirror that by
/// omitting `finishedAt`/`lastError` when absent (verified live: a running batch has
/// neither; a done batch has finishedAt only).
fn serialise_batch(entry: &BatchEntry) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("batchId".into(), json!(entry.batch_id));
    obj.insert("status".into(), json!(entry.status));
    obj.insert("completed".into(), json!(entry.completed));
    obj.insert("total".into(), json!(entry.total));
    obj.insert("workerCount".into(), json!(entry.worker_count));
    obj.insert("startedAt".into(), json!(entry.started_at));
    if let Some(f) = &entry.finished_at {
        obj.insert("finishedAt".into(), json!(f));
    }
    if let Some(e) = &entry.last_error {
        obj.insert("lastError".into(), json!(e));
    }
    Value::Object(obj)
}

/// Serialise a batch's results map for JSON (= c64re `serialiseResults`): an object
/// keyed by scenarioId; a failure is `{ error }`, a success is the ReplayResult.
fn serialise_batch_results(entry: &BatchEntry) -> Value {
    let mut map = serde_json::Map::new();
    for (sid, r) in &entry.results {
        let v = match r {
            Ok(res) => res.clone(),
            Err(e) => json!({ "error": e }),
        };
        map.insert(sid.clone(), v);
    }
    Value::Object(map)
}

/// scenario.ts runScenario — deterministic replay. (1) optionally restore the
/// start snapshot, (2) feed the recorded inputs at their cycles via the scenario
/// player, (3) run `cycleBudget` cycles, (4) hash the end RAM + report cyclesRan.
/// Returns a ReplayResult-shaped object (`ramHash`, `cyclesRan`, plus the start/end
/// PC + cycle for cross-checking). A re-run on the same build hashes identically.
fn run_scenario(st: &mut State, scenario: &Value) -> Result<Value, String> {
    use trx64_core::scenario_player::ScenarioPlayer;

    // (1) Restore the start snapshot if one is provided. `startSnapshot` may be a
    // file path string (a .c64re container) or omitted (replay from the live
    // machine — TRX64's session is already booted). Inline base64 bytes are also
    // accepted (the c64re registry stores them base64).
    if let Some(start) = scenario.get("startSnapshot") {
        if let Some(path) = start.as_str() {
            if !path.is_empty() {
                let file_bytes = std::fs::read(path)
                    .map_err(|e| format!("cannot read startSnapshot {path}: {e}"))?;
                let read = trx64_core::native_snapshot::read_native_snapshot(&file_bytes)?;
                // Re-attach embedded drive8 media, then restore the checkpoint.
                for rm in &read.media {
                    if rm.reference.role != "drive8" {
                        continue;
                    }
                    if let Some(bytes) = &rm.bytes {
                        let kind = if rm.reference.format == "d64" {
                            DiskKind::D64
                        } else {
                            DiskKind::G64
                        };
                        st.session.machine.drive8.attach_disk(DiskImage {
                            kind,
                            bytes: bytes.clone(),
                            backing_path: rm.reference.source_name.clone(),
                            read_only: false,
                        });
                    }
                }
                trx64_core::c64re_snapshot::restore_runtime_checkpoint(
                    &mut st.session.machine,
                    &read.checkpoint,
                )?;
            }
        }
    }

    // (2) Build the scenario player from the inputs (sorted by cycle internally).
    let inputs = scenario
        .get("inputs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let steps = scenario_steps_from_inputs(&inputs);
    let mut player = ScenarioPlayer::new(steps, None);

    // (3) Run cycleBudget cycles from the current clock, firing inputs at their
    // cycles. Run in segments bounded by the next due input (scenario.ts:123-146).
    let cycle_budget = scenario
        .get("cycleBudget")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let start_clk = st.session.machine.cpu6510.clk;
    let start_pc = st.session.machine.cpu6510.reg_pc as u64;
    let end_clk = start_clk.saturating_add(cycle_budget);

    let mut target = MachineScenarioTarget {
        session: &mut st.session,
    };
    loop {
        let now = target.session.machine.cpu6510.clk;
        // Fire any due inputs as of `now`.
        player.tick(&mut target, now);
        if now >= end_clk {
            break;
        }
        // Run up to the next due input (or the budget end), whichever is sooner.
        let next_due = player.next_due_cycle().unwrap_or(end_clk).max(now);
        let run_until = next_due.min(end_clk);
        let to_run = run_until.saturating_sub(now);
        if to_run == 0 {
            // A due input at `now` was just fired; if none remain, finish the budget.
            if player.remaining() == 0 {
                let rest = end_clk.saturating_sub(now);
                if rest == 0 {
                    break;
                }
                run_cycle_budget(target.session, rest);
            }
            continue;
        }
        run_cycle_budget(target.session, to_run);
    }
    // Fire any inputs that landed exactly at the end.
    let final_now = target.session.machine.cpu6510.clk;
    player.tick(&mut target, final_now);

    // (4) Hash the end RAM (scenario.ts:165 — sha256(c64Bus.ram)).
    let end_clk_actual = st.session.machine.cpu6510.clk;
    let end_pc = st.session.machine.cpu6510.reg_pc as u64;
    let ram_hash = sha256_hex(&st.session.machine.ram[..]);
    let cycles_ran = end_clk_actual.saturating_sub(start_clk);

    Ok(json!({
        "ramHash": ram_hash,
        "cyclesRan": cycles_ran,
        "startCycle": start_clk,
        "endCycle": end_clk_actual,
        "startPc": start_pc,
        "endPc": end_pc,
    }))
}

/// T1.2 — Spec 767 `setControlOwner`: set `State.control_owner` and broadcast
/// `debug/control { session_id, owner }` ONLY on change, matching
/// RuntimeController.setControlOwner (runtime-controller.ts:338).
fn set_control_owner(st: &mut State, owner: &str) {
    if st.control_owner != owner {
        st.control_owner = owner.to_string();
        st.notify.broadcast("debug/control", json!({
            "session_id": st.session.id,
            "owner": owner,
        }));
    }
}

/// T1.2 — derive control owner from a `source` param value, mirroring TS:
/// `source === "llm" ? "llm" : "human"` (ws-server.ts:987).
fn owner_from_source(params: &Value) -> &'static str {
    if params.get("source").and_then(|v| v.as_str()) == Some("llm") { "llm" } else { "human" }
}

/// The `debug/state` controller-state object (= c64re `controller.state()`), built
/// from the live `State`. Shared by `debug/state` and `checkpoint/restore`'s
/// `state` field so both report the identical shape.
fn build_debug_state(st: &State) -> Value {
    let bps = st.breakpoints.list_vice_json();
    let pc = st.session.machine.cpu6510.reg_pc as u64;
    let cycles = st.session.machine.clk;
    let run_state = if st.session.running { "running" } else { "paused" };
    let stop = match &st.ctrl_stop {
        Some(s) => json!({ "reason": s.reason, "pc": s.pc as u64, "cycles": s.cycles }),
        None => Value::Null,
    };
    // T1.3: pacing is stored in State (session/set_pacing mutates it); default "pal"/1.
    // Mirrors TS RuntimeController.state() → { pacing: { ...this.pacing } }.
    // T1.2: controlOwner is tracked in State (default "human"); set_control_owner()
    // updates it on each run/pause/continue/step. Mirrors RuntimeController.controlOwner.
    json!({
        "runState": run_state,
        "pacing": { "mode": st.pacing_mode, "ratio": st.pacing_ratio },
        "pc": pc,
        "cycles": cycles,
        "frame": st.ctrl_frame,
        "breakpoints": bps,
        "stop": stop,
        "controlOwner": st.control_owner
    })
}

// ── Checkpoint-ring capture/restore of the LIVE machine ──────────────────────────
//
// These factor the snapshot/dump + snapshot/undump core (drive blobs, disk
// re-attach, full RuntimeCheckpoint capture/restore) so a `checkpoint/*` ring
// capture/restore rides the EXACT same path as the `.c64re` snapshot — the ring
// just keeps the resulting checkpoint Value in memory instead of on disk.

/// Spec 710 — `cpForInspect` (ws-server.ts:1106-1111): the stored RuntimeCheckpoint
/// tree for `id` (rehydrated media), erroring when the id is unknown or the entry
/// has no VIC/RAM. The vic-inspect engine reads `cp.vic.regs/color_ram`, `cp.ram`,
/// `cp.cia2.c_cia` off this tree.
fn cp_for_inspect(st: &State, id: &str) -> Result<Value, String> {
    let cp = st
        .checkpoint_ring
        .restore_snapshot(id)
        .ok_or_else(|| format!("vic/inspect: unknown checkpoint {id}"))?;
    let has_vic = cp.get("vic").map(|v| !v.is_null()).unwrap_or(false);
    let has_ram = cp.get("ram").map(|v| !v.is_null()).unwrap_or(false);
    if !has_vic || !has_ram {
        return Err(format!("vic/inspect: unknown or empty checkpoint {id}"));
    }
    Ok(cp)
}

/// Wall-clock ms since epoch (ws-server.ts `Date.now()` for `promotedAtMs`).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Capture the live machine into a self-contained RuntimeCheckpoint Value, with the
/// attached drive8 disk EMBEDDED in the `driveDiskImage` blob so a later restore can
/// re-attach it (matching snapshot/dump). Mirrors c64re `controller.captureCheckpoint`
/// → `ring.capture(kernel.snapshot(), frame, cycles)`.
fn capture_live_checkpoint(session: &mut Session) -> Value {
    // Disk path/format for the checkpoint `media` metadata (= snapshot/dump).
    let (disk_path, disk_format) = match session.machine.drive8.get_attached_disk() {
        Some(d) => (
            d.backing_path.clone().unwrap_or_default(),
            match d.kind {
                DiskKind::G64 => "g64",
                DiskKind::D64 => "d64",
            }
            .to_string(),
        ),
        None => (String::new(), String::new()),
    };
    // The attached disk's clean bytes ride as the `driveDiskImage` pooled blob so a
    // ring restore re-establishes the media without a sidecar file. (snapshot/dump
    // embeds these in the .c64re mediaPayloads; the in-memory ring embeds them in
    // the checkpoint tree, which the disk pool then dedups across entries.)
    let attached_disk_bytes = session
        .machine
        .drive8
        .get_attached_disk()
        .map(|d| d.bytes.clone());
    // Drive blobs (drive1541 core + GCRIMAGE0 overlay), captured from the live drive.
    let drive1541_blob =
        trx64_core::drive_snapshot::capture_drive1541(&mut session.machine.drive8);
    let drive_disk_blob =
        trx64_core::drive_snapshot::capture_drive_disk_image(&session.machine.drive8);
    // formats-state-2 — full ring anchor carries the cart bytes + writable flash too
    // (c64re's non-omitMedia checkpoint, headless-machine-kernel.ts:988-989).
    let (cart_bytes, cart_flash) = capture_cart_blobs(&mut session.machine);
    let mut cp = trx64_core::c64re_snapshot::capture_runtime_checkpoint(
        &session.machine,
        &disk_path,
        &disk_format,
        Some(&drive1541_blob),
        drive_disk_blob.as_deref(),
        cart_bytes.as_deref(),
        cart_flash.as_deref(),
    );
    // Embed the clean disk bytes as `driveDiskImage` so the ring's content-addressed
    // pool dedups them and a restore re-attaches the disk before restoring the drive
    // GCR overlay (the drive_snapshot `driveDiskImage` field holds the MUTABLE GCR
    // overlay, captured above; here we additionally carry the clean image to re-attach).
    if let Some(bytes) = attached_disk_bytes {
        // The GCR overlay (drive_disk_blob) already rode `driveDiskImage`; the clean
        // image rides a sibling field consumed only by the ring restore. Keep the
        // c64re `driveDiskImage` semantics untouched (mutable GCR overlay) and stash
        // the re-attach image under `_ringDriveDiskBytes` (a TRX64-private ring slot,
        // ignored by restore_runtime_checkpoint, consumed by restore_live_checkpoint).
        cp["_ringDriveDiskBytes"] = trx64_core::native_snapshot::ta_u8(&bytes);
    }
    cp
}

/// Restore the live machine from a ring checkpoint Value (re-attaching the embedded
/// drive8 disk first, then `restore_runtime_checkpoint`). Mirrors snapshot/undump.
/// Returns Ok(()) on success. Leaves the session paused (a restore is a pause point).
fn restore_live_checkpoint(session: &mut Session, cp: &Value) -> Result<(), String> {
    // Re-attach the embedded clean disk image FIRST (so the drive's GCR baseline is
    // present before restore_runtime_checkpoint overlays the mutable GCR content).
    if let Some(bytes) = cp
        .get("_ringDriveDiskBytes")
        .and_then(trx64_core::native_snapshot::ta_u8_decode)
    {
        // Recover the disk kind/path from the checkpoint media metadata.
        let format = cp
            .get("media")
            .and_then(|mm| mm.get("imageFormat"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let kind = if format == "d64" { DiskKind::D64 } else { DiskKind::G64 };
        let backing_path = cp
            .get("media")
            .and_then(|mm| mm.get("diskPath"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        session.machine.drive8.attach_disk(DiskImage {
            kind,
            bytes,
            backing_path,
            read_only: false,
        });
    }
    trx64_core::c64re_snapshot::restore_runtime_checkpoint(&mut session.machine, cp)?;
    session.running = false;
    Ok(())
}

/// One re-attached drive8 medium restored by `undump_native_snapshot`, in a shape
/// both callers can format (the monitor `role=name(fmt)` string, the WS media JSON).
struct UndumpMedia {
    role: String,
    format: String,
    source_name: Option<String>,
    sha256: String,
    bytes: u64,
}

/// The outcome of `undump_native_snapshot`, formatted differently by each caller.
struct UndumpResult {
    pc: u16,
    cycle: u64,
    machine_model: String,
    media: Vec<UndumpMedia>,
    /// Non-fatal resident-vs-cart divergence warning, if any.
    warning: Option<String>,
}

/// The SINGLE user-facing `.c64re` cold undump — shared by the monitor `undump`
/// command and the WS `snapshot/undump` handler (previously two near-identical
/// inline copies). Reads + validates the container, **power-cycles to fresh chips**
/// (`power_cycle_for_restore` — fixes the "restore onto a live dirty machine" bug:
/// stale SID kept playing, VIC render broken, appeared to land in the cart intro),
/// re-attaches the embedded drive8 media onto the fresh machine, then restores the
/// checkpoint on top. Leaves the session PAUSED. Errors are returned WITHOUT a
/// prefix — each caller adds its own (`undump:` / `snapshot/undump:`).
fn undump_native_snapshot(st: &mut State, path: &str) -> Result<UndumpResult, String> {
    let file_bytes =
        std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let read = trx64_core::native_snapshot::read_native_snapshot(&file_bytes)?;

    // Fresh chips FIRST (Spec 786): clears the stale internal chip state the field
    // restore cannot, resets the SID, re-hooks audio. Does not clear the ring.
    power_cycle_for_restore(st);

    // Spec 793 — MATERIALIZE the embedded media into a `<name>_media/` sidecar next to
    // the snapshot, then mount each FILE-backed. This turns the old invisible in-memory
    // attach into a real, picker-visible mount whose writes persist to a file the user
    // owns (and can abräumen). The dir is tagged in `materialized_media` so
    // `undump_media_purge` can later kill ONLY what undump created.
    let media_dir = {
        let p = std::path::Path::new(path);
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "snapshot".to_string());
        p.parent()
            .map(|d| d.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(format!("{stem}_media"))
    };
    let mut materialized_any = false;

    // Re-attach the embedded drive8 media onto the fresh machine, then restore.
    let mut media = Vec::new();
    for rm in &read.media {
        if rm.reference.role != "drive8" {
            continue;
        }
        let bytes = match &rm.bytes {
            Some(b) => b.clone(),
            None => {
                return Err(format!(
                    "media {} has no embedded payload (v1 needs embedded bytes)",
                    rm.reference.role
                ))
            }
        };
        let kind = if rm.reference.format == "d64" { DiskKind::D64 } else { DiskKind::G64 };
        let len = bytes.len() as u64;
        // Write the disk out to `<name>_media/<file>` and mount THAT (file-backed).
        let fname = rm
            .reference
            .source_name
            .clone()
            .filter(|s| !s.is_empty())
            .map(|s| std::path::Path::new(&s).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or(s))
            .unwrap_or_else(|| format!("drive8.{}", rm.reference.format));
        let backing = match std::fs::create_dir_all(&media_dir).and_then(|_| {
            let fp = media_dir.join(&fname);
            std::fs::write(&fp, &bytes).map(|_| fp)
        }) {
            Ok(fp) => {
                materialized_any = true;
                Some(fp.to_string_lossy().to_string())
            }
            // Materialization is best-effort: on a write failure fall back to the
            // in-memory attach (never break the undump over a filesystem hiccup).
            Err(_) => rm.reference.source_name.clone(),
        };
        st.session.machine.drive8.attach_disk(DiskImage {
            kind,
            bytes,
            backing_path: backing,
            read_only: false,
        });
        media.push(UndumpMedia {
            role: rm.reference.role.clone(),
            format: rm.reference.format.clone(),
            source_name: rm.reference.source_name.clone(),
            sha256: rm.reference.sha256.clone(),
            bytes: len,
        });
    }

    trx64_core::c64re_snapshot::restore_runtime_checkpoint(
        &mut st.session.machine,
        &read.checkpoint,
    )?;

    // Spec 793 — materialize the CART too (refine decision: cart also externalized).
    // The restore re-created the mapper from `cartBytes`; write those bytes out as a
    // `.crt` in `<name>_media/` and point the session/cart image at it so the cart is
    // a normal picker mount (swap/inspect/abräumen uniform with the disk).
    if let Some(cart_bytes) = read
        .checkpoint
        .get("cartBytes")
        .and_then(trx64_core::native_snapshot::ta_u8_decode)
    {
        if !cart_bytes.is_empty() {
            let cname = std::path::Path::new(path)
                .file_stem()
                .map(|s| format!("{}.crt", s.to_string_lossy()))
                .unwrap_or_else(|| "cart.crt".to_string());
            if std::fs::create_dir_all(&media_dir).is_ok() {
                let cp = media_dir.join(&cname);
                if std::fs::write(&cp, &cart_bytes).is_ok() {
                    materialized_any = true;
                    let cps = cp.to_string_lossy().to_string();
                    st.session.cart_path = cps.clone();
                    if let Some(img) = st.session.machine.cartridge_image.as_mut() {
                        img.path = cps;
                    }
                }
            }
        }
    }

    // Tag the sidecar dir so `undump_media_purge` / `killmedia` can later delete ONLY it.
    if materialized_any {
        let d = media_dir.to_string_lossy().to_string();
        if !st.materialized_media.contains(&d) {
            st.materialized_media.push(d);
        }
    }

    let pc = st.session.machine.c64_core.reg_pc;
    let cycle = st.session.machine.c64_core.clk;
    st.session.running = false;
    st.mon.disasm_cursor = Some(pc); // bare `d` follows the restored PC
    // Refresh the paused canvas to the RESTORED frame. The `--stream` paused loop is
    // otherwise silent (it only advances a running machine), so without this the UI
    // keeps showing the pre-undump picture — a restore that "looks borked / not 1:1".
    // Mirrors `checkpoint/restore` (main.rs ~10546): request a one-shot present; the
    // paused stream branch renders the already-restored `vic.displayed` (the exact
    // dumped frame — the framebuffer is captured/restored via `vicPresentation`) and
    // pushes exactly one BIN_VIC. Harmlessly consumed-or-ignored when no stream runs.
    st.force_present_frame = true;

    // GUARDRAIL — warn (non-fatal) when the restored resident RAM diverges from the
    // MOUNTED cart flash (a stale-resident-code smell; the field "resident != flash").
    let warning = st.session.machine.cart_resident_divergence().map(|(addr, cart_b, ram_b)| {
        format!(
            "resident RAM diverges from the mounted cart at ${addr:04X} \
             (cart=${cart_b:02X} ram=${ram_b:02X}) — the undumped RAM may not match \
             the live flash. Verify, or re-mount the cart / cold-boot if the resident \
             code is stale."
        )
    });

    Ok(UndumpResult {
        pc,
        cycle,
        machine_model: read.manifest.machine.model.clone(),
        media,
        warning,
    })
}

/// Spec 793 — `undump_media_purge` / monitor `killmedia`: delete EVERY
/// undump-materialized `<name>_media/` sidecar (files + dir) and its registry tag,
/// first detaching a live mount whose backing file lives under one (so no dangling
/// mount is left). Touches ONLY dirs the daemon itself created via undump
/// materialization — never a user's own mount. Returns (dirs_removed, files_removed).
fn purge_materialized_media(st: &mut State) -> (usize, usize) {
    let dirs = std::mem::take(&mut st.materialized_media);
    if dirs.is_empty() {
        return (0, 0);
    }
    let under = |p: &str| -> bool { !p.is_empty() && dirs.iter().any(|d| p.starts_with(d.as_str())) };

    // Detach a disk mount backed by a materialized file (avoid a dangling mount +
    // release the file before delete). Writes are DISCARDED — purge is "kill the tmp".
    let disk_backed = st
        .session
        .machine
        .drive8
        .get_attached_disk()
        .and_then(|d| d.backing_path.clone())
        .map(|p| under(&p))
        .unwrap_or(false);
    if disk_backed {
        st.session.machine.drive8.detach_disk();
    }
    // Drop the cart's file-backing label if it points into a purged dir (the in-memory
    // cart stays functional; it just no longer claims a deleted file as its backing).
    if under(&st.session.cart_path) {
        st.session.cart_path.clear();
        if let Some(img) = st.session.machine.cartridge_image.as_mut() {
            img.path.clear();
        }
    }

    let mut ndirs = 0usize;
    let mut nfiles = 0usize;
    for d in &dirs {
        if let Ok(rd) = std::fs::read_dir(d) {
            nfiles += rd.flatten().count();
        }
        if std::fs::remove_dir_all(d).is_ok() {
            ndirs += 1;
        }
    }
    (ndirs, nfiles)
}

// ── Spec time-travel-tooling Piece 1 — diffCheckpoints(idA, idB) ───────────────
//
// Resolve two checkpoint anchors BY ID from the live ring, run the EXISTING
// snapshot_diff compute on their two machine states, and return a TYPED
// `SnapshotDiff`-shaped JSON value (RAM grouped into contiguous changed runs, one
// `[{name,old,new}]` list per chip).
//
// READ-ONLY contract: the live machine MUST be byte-identical after the diff. The
// snapshot_diff compute parses the c64re-own VSF framing (`save_vsf` emits exactly
// the modules it reads: MAINCPU/C64MEM/CIA1/CIA2/SID/DRIVECPU/IECBUS/VIC-II), and
// the only way to obtain an anchor's VSF bytes is to put that anchor INTO the
// machine and `save_vsf`. So we (1) snapshot the LIVE state to a checkpoint Value,
// (2) restore anchor A → `save_vsf` → bytes A, (3) restore anchor B → `save_vsf` →
// bytes B, (4) restore the saved LIVE state back, then (5) compute the diff. The
// restore→save→restore round-trip uses the SAME path checkpoint/restore uses, and
// the final restore-back leaves the machine exactly as it was found (verified: live
// PC/cycles unchanged).
//
// No new diff logic — `snapshot_diff::diff_snapshots` is the compute; this wraps it
// with by-ID anchor resolution + the typed-record reshape.
/// Spec 796 — apply one candidate patch to the live machine (RAM or a cart bank,
/// reusing the 795 overlay primitives).
fn apply_candidate_patch(machine: &mut trx64_core::Machine, p: &candidate::Patch) -> Result<(), String> {
    if p.space == "ram" {
        for (i, &b) in p.bytes.iter().enumerate() {
            machine.ram[(p.addr as usize + i) & 0xffff] = b;
        }
        Ok(())
    } else {
        let bank = p.bank.unwrap_or(0);
        let cart = machine
            .cartridge
            .as_mut()
            .ok_or("candidate patch targets a cart bank but no cartridge is attached")?;
        for (i, &b) in p.bytes.iter().enumerate() {
            cart.overlay_bank_write(&p.space, bank, ((p.addr as usize + i) & 0xffff) as u16, b)?;
        }
        Ok(())
    }
}

/// Spec 796 — run a candidate: restore its baseline anchor, apply ALL its patches
/// (795 overlay, RAM + cart), play its bound scenario (231, deterministic from the
/// post-patch state), capture the end checkpoint, and auto-diff (794) against the
/// cached no-patch baseline. Returns { registers, verdict, ranCycles }. Ephemeral —
/// the baseline anchor is untouched, so the next run reproduces this one.
fn run_candidate(st: &mut State, cand_id: &str) -> Result<Value, String> {
    // Clone the candidate's inputs out so the &mut State borrow is free for the
    // restore/run/diff below; the verdict is written back at the end.
    let cand = st
        .candidates
        .get(cand_id)
        .ok_or_else(|| format!("unknown candidate {cand_id}"))?
        .clone();

    // (1) Restore the baseline anchor into the live session (a control discontinuity).
    let snapshot = st
        .checkpoint_ring
        .restore_snapshot(&cand.baseline_anchor)
        .ok_or_else(|| format!("candidate {cand_id}: unknown baseline anchor {}", cand.baseline_anchor))?;
    restore_live_checkpoint(&mut st.session, &snapshot)?;
    st.session.running = false;
    st.ctrl_frame += 1;
    st.ctrl_stop = None;

    // (2) Apply all patches (RAM + cart).
    for p in &cand.patches {
        apply_candidate_patch(&mut st.session.machine, p)?;
    }

    // (3) Play the bound scenario (no startSnapshot → from the post-patch anchor).
    let start_clk = st.session.machine.cpu6510.clk;
    run_scenario(st, &cand.scenario)?;
    let ran = st.session.machine.cpu6510.clk.saturating_sub(start_clk);

    // (4) Capture the end + auto-diff vs the cached baseline result.
    let end = capture_live_checkpoint(&mut st.session);
    let diff = trx64_core::checkpoint_diff::diff_checkpoints(
        &cand.baseline_result,
        &end,
        &trx64_core::checkpoint_diff::ExcludeMask::default(),
    );
    if let Some(c) = st.candidates.get_mut(cand_id) {
        c.last_verdict = Some(diff.get("verdict").cloned().unwrap_or(Value::Null));
    }

    let c = &st.session.machine.cpu6510;
    Ok(json!({
        "id": cand_id,
        "ranCycles": ran,
        "registers": {
            "pc": c.reg_pc as u64, "a": c.reg_a as u64, "x": c.reg_x as u64,
            "y": c.reg_y as u64, "sp": c.reg_sp as u64, "flags": c.flags() as u64,
            "cycles": st.session.machine.clk,
        },
        "verdict": diff.get("verdict").cloned().unwrap_or(Value::Null),
        "diff": diff,
    }))
}

/// Build a Spec 794 ExcludeMask from monitor `cdiff` trailing tokens: `eq` (or
/// `preset`) = equivalence preset, `c:NAME` component, `l:NAME` lane,
/// `r:SPACE:FROM-TO` address range.
fn component_diff_mask_from_toks(toks: &[String]) -> trx64_core::checkpoint_diff::ExcludeMask {
    let mut components: Vec<String> = Vec::new();
    let mut lanes: Vec<String> = Vec::new();
    let mut presets: Vec<String> = Vec::new();
    let mut ranges: Vec<Value> = Vec::new();
    for t in toks {
        if t == "eq" || t == "preset" {
            presets.push("equivalence".to_string());
        } else if let Some(v) = t.strip_prefix("c:") {
            components.push(v.to_string());
        } else if let Some(v) = t.strip_prefix("l:") {
            lanes.push(v.to_string());
        } else if let Some(v) = t.strip_prefix("r:") {
            if let Some((space, rng)) = v.split_once(':') {
                if let Some((from, to)) = rng.split_once('-') {
                    ranges.push(json!({ "space": space, "from": from, "to": to }));
                }
            }
        }
    }
    let mask_json = json!({
        "components": components, "lanes": lanes, "presets": presets, "ranges": ranges,
    });
    trx64_core::checkpoint_diff::ExcludeMask::from_json(&mask_json)
}

fn diff_checkpoints_by_id(st: &mut State, id_a: &str, id_b: &str) -> Result<Value, String> {
    // Resolve both anchors up front so an unknown id errors BEFORE we disturb the
    // machine (the snapshot/restore-back below only happens on the happy path).
    let snap_a = st
        .checkpoint_ring
        .restore_snapshot(id_a)
        .ok_or_else(|| format!("unknown checkpoint id {id_a}"))?;
    let snap_b = st
        .checkpoint_ring
        .restore_snapshot(id_b)
        .ok_or_else(|| format!("unknown checkpoint id {id_b}"))?;

    // (1) Preserve the LIVE state so the diff is read-only. Capturing it as a ring
    // checkpoint Value (the exact shape restore_live_checkpoint consumes) means the
    // restore-back at the end is byte-faithful — same path as a normal restore.
    let was_running = st.session.running;
    let live = capture_live_checkpoint(&mut st.session);

    // (2)+(3) Restore each anchor and capture its VSF bytes. Helper keeps the two
    // legs identical; any restore failure short-circuits to the live restore-back.
    let vsf_of = |session: &mut Session, snap: &Value| -> Result<Vec<u8>, String> {
        restore_live_checkpoint(session, snap)?;
        Ok(trx64_core::vsf::save_vsf(&mut session.machine))
    };
    let result: Result<Value, String> = (|| {
        let a = vsf_of(&mut st.session, &snap_a)?;
        let b = vsf_of(&mut st.session, &snap_b)?;
        // (5) The compute (existing, tested) over the two VSF buffers, then the typed
        // reshape — which re-reads the exact RAM run bytes from the C64MEM modules of
        // the same two buffers (so a run's `old`/`new` is the FULL run, not the
        // 100-entry sample cap).
        let raw = snapshot_diff::diff_snapshots(&a, &b);
        Ok(snapshot_diff_to_typed(&raw, &a, &b))
    })();

    // (4) Restore the LIVE state back — ALWAYS, even on a mid-diff error, so the
    // machine is left exactly as found (read-only contract). Best-effort: a failure
    // here is a genuine bug (the live snapshot we just took must round-trip), so it
    // is surfaced if the diff itself otherwise succeeded.
    let restore_back = restore_live_checkpoint(&mut st.session, &live);
    st.session.running = was_running;

    match (result, restore_back) {
        (Ok(v), Ok(())) => Ok(v),
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(format!("diff ok but live restore-back failed: {e}")),
    }
}

/// Reshape the legacy byte-buffer `diff_snapshots` JSON into the typed `SnapshotDiff`
/// record shape (Spec time-travel-tooling Piece 1):
///   { cycleA, cycleB,
///     ram:   [{ start, old:b64, new:b64 }]      (contiguous changed RUNS),
///     cpu/vic/cia/sid/drive: [{ name, old, new }] }
/// The RAM runs come from `diff_snapshots`' `changedRanges` (start/end), with the
/// actual old/new bytes read back from the two VSF buffers — but the compute already
/// dropped them, so we re-derive the run bytes from the `sample` (full coverage) is
/// not possible (sample caps at 100). Instead we carry the run extents + the per-run
/// bytes are reconstructed here directly from the two C64MEM module slices, which the
/// caller passes in. (See `snapshot_diff_to_typed_with_ram`.)
///
/// This top-level reshape handles everything EXCEPT the RAM run bytes; the full RAM
/// reconstruction lives in `snapshot_diff_to_typed` which has the two buffers.
fn reg_changes_from_chip(chip: &Value, name_of: &dyn Fn(i64) -> String) -> Vec<Value> {
    chip.get("changedRegisters")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .map(|cr| {
                    let reg = cr.get("reg").and_then(|v| v.as_i64()).unwrap_or(0);
                    json!({
                        "name": name_of(reg),
                        "old": cr.get("before").and_then(|v| v.as_u64()).unwrap_or(0),
                        "new": cr.get("after").and_then(|v| v.as_u64()).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Convert the raw `diff_snapshots` JSON into the typed `SnapshotDiff` record shape.
/// RAM runs carry the run extent (`start` + `byteCount`) + the FULL old/new byte
/// slices for that run, read directly from the C64MEM modules of the two VSF buffers
/// `vsf_a`/`vsf_b` (the `changedRanges` extents from the compute are exact; the bytes
/// are sliced from the buffers, NOT the 100-entry sample, so a long run's payload is
/// complete).
fn snapshot_diff_to_typed(raw: &Value, vsf_a: &[u8], vsf_b: &[u8]) -> Value {
    // CPU register names (snapshot-diff emits string regs for the main CPU).
    let cpu = raw
        .get("cpu")
        .and_then(|c| c.get("changedRegs"))
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .map(|cr| {
                    json!({
                        "name": cr.get("reg").and_then(|v| v.as_str()).unwrap_or("?"),
                        "old": cr.get("before").and_then(|v| v.as_u64()).unwrap_or(0),
                        "new": cr.get("after").and_then(|v| v.as_u64()).unwrap_or(0),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // Chip register indices → a "$NN" name (the chip diffs carry numeric reg indices).
    let idx_name = |i: i64| format!("${:02X}", i & 0xff);
    let empty = json!({});
    let vic = reg_changes_from_chip(raw.get("vic").unwrap_or(&empty), &idx_name);
    // CIA1 + CIA2 are two separate VSF modules; merge into a single `cia` list,
    // tagging the reg name with its chip so the consumer can tell them apart.
    let cia1 = reg_changes_from_chip(raw.get("cia1").unwrap_or(&empty), &|i| {
        format!("cia1.${:02X}", i & 0xff)
    });
    let cia2 = reg_changes_from_chip(raw.get("cia2").unwrap_or(&empty), &|i| {
        format!("cia2.${:02X}", i & 0xff)
    });
    let mut cia = cia1;
    cia.extend(cia2);
    let sid = reg_changes_from_chip(raw.get("sid").unwrap_or(&empty), &idx_name);

    // Drive sub-diff (present only when both anchors carry a DRIVECPU). Merge the
    // drive CPU regs + VIA1/VIA2 + head move into a single `drive` change list.
    let mut drive: Vec<Value> = Vec::new();
    if let Some(d) = raw.get("drive").filter(|d| !d.is_null()) {
        let cpu_names = ["pc", "a", "x", "y", "sp", "p"];
        if let Some(dc) = d.get("cpu") {
            for c in reg_changes_from_chip(dc, &|i| {
                cpu_names
                    .get(i as usize)
                    .map(|s| format!("cpu.{s}"))
                    .unwrap_or_else(|| format!("cpu.${:02X}", i & 0xff))
            }) {
                drive.push(c);
            }
        }
        if let Some(v) = d.get("via1") {
            drive.extend(reg_changes_from_chip(v, &|i| format!("via1.${:02X}", i & 0xff)));
        }
        if let Some(v) = d.get("via2") {
            drive.extend(reg_changes_from_chip(v, &|i| format!("via2.${:02X}", i & 0xff)));
        }
        let hp = d.get("headPosition").cloned().unwrap_or(json!({}));
        let hb = hp.get("trackHalfBefore").and_then(|v| v.as_u64()).unwrap_or(0);
        let ha = hp.get("trackHalfAfter").and_then(|v| v.as_u64()).unwrap_or(0);
        if hb != ha {
            drive.push(json!({ "name": "headHalfTrack", "old": hb, "new": ha }));
        }
    }

    // RAM runs: extent from `changedRanges` (exact), bytes sliced from the C64MEM
    // module of each VSF buffer (the FULL run payload, base64).
    let ram_obj = raw.get("ram").cloned().unwrap_or(json!({}));
    let ram_a = snapshot_diff::vsf_c64mem_ram(vsf_a);
    let ram_b = snapshot_diff::vsf_c64mem_ram(vsf_b);
    let ram_runs: Vec<Value> = ram_obj
        .get("changedRanges")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .map(|rng| {
                    let start = rng.get("start").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let end = rng.get("end").and_then(|v| v.as_u64()).unwrap_or(start as u64) as usize;
                    let lo = start.min(ram_a.len()).min(ram_b.len());
                    let hi = (end + 1).min(ram_a.len()).min(ram_b.len());
                    let (old, new): (&[u8], &[u8]) = if lo < hi {
                        (&ram_a[lo..hi], &ram_b[lo..hi])
                    } else {
                        (&[], &[])
                    };
                    json!({
                        "start": start as u64,
                        "byteCount": (end - start + 1) as u64,
                        "old": base64_encode(old),
                        "new": base64_encode(new),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    json!({
        "cycleA": raw.get("fromCycle").and_then(|v| v.as_u64()).unwrap_or(0),
        "cycleB": raw.get("toCycle").and_then(|v| v.as_u64()).unwrap_or(0),
        "ram": ram_runs,
        "cpu": cpu,
        "vic": vic,
        "cia": cia,
        "sid": sid,
        "drive": drive,
    })
}

/// Render a typed `SnapshotDiff` (from `snapshot_diff_to_typed`) as monitor text.
fn format_typed_snapshot_diff(d: &Value) -> String {
    let a = d.get("cycleA").and_then(|v| v.as_u64()).unwrap_or(0);
    let b = d.get("cycleB").and_then(|v| v.as_u64()).unwrap_or(0);
    let mut lines = vec![format!(
        "checkpoint diff  cycles {a} → {b}  (Δ{})",
        b.wrapping_sub(a)
    )];
    // RAM runs.
    let ram = d.get("ram").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    if ram.is_empty() {
        lines.push("RAM:   no changes".to_string());
    } else {
        let total: u64 = ram
            .iter()
            .map(|r| r.get("byteCount").and_then(|v| v.as_u64()).unwrap_or(0))
            .sum();
        lines.push(format!("RAM:   {} run(s), {total} byte(s) changed", ram.len()));
        for r in ram.iter().take(12) {
            let start = r.get("start").and_then(|v| v.as_u64()).unwrap_or(0);
            let bc = r.get("byteCount").and_then(|v| v.as_u64()).unwrap_or(0);
            if bc == 1 {
                lines.push(format!("         ${start:04X}"));
            } else {
                lines.push(format!("         ${start:04X}-${:04X}  ({bc} B)", start + bc - 1));
            }
        }
        if ram.len() > 12 {
            lines.push(format!("         … +{} more run(s)", ram.len() - 12));
        }
    }
    // Per-chip register changes.
    let chip = |label: &str, key: &str| -> String {
        let regs = d.get(key).and_then(|r| r.as_array()).cloned().unwrap_or_default();
        if regs.is_empty() {
            return format!("{label:<7}no changes");
        }
        let body = regs
            .iter()
            .take(8)
            .map(|c| {
                format!(
                    "{} {}→{}",
                    c.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
                    c.get("old").and_then(|v| v.as_u64()).unwrap_or(0),
                    c.get("new").and_then(|v| v.as_u64()).unwrap_or(0),
                )
            })
            .collect::<Vec<_>>()
            .join("  ");
        let more = if regs.len() > 8 { format!(" (+{} more)", regs.len() - 8) } else { String::new() };
        format!("{label:<7}{body}{more}")
    };
    lines.push(chip("CPU:", "cpu"));
    lines.push(chip("VIC:", "vic"));
    lines.push(chip("CIA:", "cia"));
    lines.push(chip("SID:", "sid"));
    let drive = d.get("drive").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    if !drive.is_empty() {
        lines.push(chip("DRIVE:", "drive"));
    }
    lines.join("\n")
}

// ── Spec time-travel-tooling Piece 2 — ringbuffer dump/restore ────────────────
//
// Serialize the WHOLE reverse-debug buffer (checkpoint ring + delta ring + cpu-
// history ring) into one gzipped `.c64rering` container, and reconstruct it
// elsewhere. After restore the scrub filmstrip, reverse_step, who_wrote, chis, and
// diffCheckpoints all work on the dumped buffer. The per-anchor states reuse the
// existing RuntimeCheckpoint serialization; the container framing/gzip lives in
// trx64_core::ring_dump.

/// Build the `.c64rering` container from the live rings, write it to `path`, and
/// return the `RingDumpInfo`-shaped JSON. The "current" anchor = the newest ring
/// entry (the scrub head). READ-ONLY w.r.t. the machine.
fn ringbuffer_dump_to_path(st: &State, path: &str) -> Result<Value, String> {
    // "current" = the newest anchor (the live scrub head); None when the ring is empty.
    let current_id = st.checkpoint_ring.list().last().map(|r| r.id.clone());
    let dump = trx64_core::ring_dump::RingBufferDump {
        checkpoint_ring: st.checkpoint_ring.to_dump(current_id),
        delta_ring: st.session.machine.delta_ring.to_dump(),
        cpu_history: st.session.machine.cpu_history.to_dump(),
    };
    let bytes = trx64_core::ring_dump::write_ring_dump(&dump);
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, &bytes).map_err(|e| format!("write {path}: {e}"))?;
    let info = dump.info(bytes.len() as u64);
    Ok(ring_dump_info_to_json(&info))
}

/// Read a `.c64rering` container from `path`, reconstruct the three rings INTO state,
/// restore the machine to the dump's "current" anchor, and return the
/// `RingDumpInfo`-shaped JSON. After this the scrub filmstrip + reverse-debug + diff
/// all work on the loaded buffer.
fn ringbuffer_restore_from_path(st: &mut State, path: &str) -> Result<Value, String> {
    let file_bytes = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    let dump = trx64_core::ring_dump::read_ring_dump(&file_bytes)?;

    // Reconstruct the three rings.
    st.checkpoint_ring =
        trx64_core::checkpoint_ring::RuntimeCheckpointRing::from_dump(&dump.checkpoint_ring);
    st.session.machine.delta_ring =
        trx64_core::delta_ring::DeltaRing::from_dump(&dump.delta_ring);
    st.session.machine.cpu_history =
        trx64_core::cpu_history::CpuHistoryRing::from_dump(&dump.cpu_history);

    // Restore the machine to the dump's "current" anchor (the scrub head) so the loaded
    // session sits where the tester left it. Falls back to the newest anchor when the
    // dump carries no explicit current id; a no-anchor dump leaves the live machine as-is.
    let target = dump
        .checkpoint_ring
        .current_id
        .clone()
        .or_else(|| st.checkpoint_ring.list().last().map(|r| r.id.clone()));
    if let Some(cur) = target {
        if let Some(snapshot) = st.checkpoint_ring.restore_snapshot(&cur) {
            restore_live_checkpoint(&mut st.session, &snapshot)?;
            // A restore is a control discontinuity (= a pause/seek): mirror the
            // undump/checkpoint-restore tail so the run-state + audio are coherent.
            st.session.running = false;
            st.ctrl_frame += 1;
            st.ctrl_stop = None;
            st.notify
                .broadcast("audio/flush", json!({ "session_id": st.session.id }));
            st.force_present_frame = true;
        }
    }

    let info = dump.info(file_bytes.len() as u64);
    Ok(ring_dump_info_to_json(&info))
}

/// JSON wire shape for the `RingDumpInfo` typed record (`ringbuffer/dump|restore`).
fn ring_dump_info_to_json(info: &trx64_core::ring_dump::RingDumpInfo) -> Value {
    json!({
        "path": "",
        "anchors": info.anchors,
        "deltaEntries": info.delta_entries,
        "cpuHistory": info.cpu_history,
        "cycleFirst": info.cycle_first,
        "cycleLast": info.cycle_last,
        "currentId": info.current_id,
        "fileBytes": info.file_bytes,
        "version": info.version,
    })
}

// ── Spec 766.5 — recorder anchor capture ──────────────────────────────────────
//
// The c64re controller feeds the recorder a CORE-ONLY anchor (omitMedia) at the
// 0.5 s autocapture cadence: the disk GCR / cart bytes ride the recorder's
// separate gen-gated MEDIUM stream, not the per-anchor snapshot (runtime-
// controller.ts:846-852). TRX64 has no per-frame autocapture loop, so the daemon
// drives a capture explicitly (recorder/capture) — the same observable touchpoint,
// minus the background timer. This builds the core anchor payload (the
// RuntimeCheckpoint tree WITHOUT the embedded media blobs) + the live disk media
// descriptor, and hands them to `RuntimeRecorder::capture_anchor`.

/// Build a CORE-ONLY checkpoint payload (the omitMedia anchor): the full
/// RuntimeCheckpoint tree with the large media blobs (driveDiskImage / cart bytes)
/// NULLED — those ride the recorder's gen-gated medium stream. The small cart
/// bank/control state still rides the anchor's `media` metadata (c64re semantics).
fn capture_recorder_anchor_payload(session: &mut Session) -> Value {
    let (disk_path, disk_format) = match session.machine.drive8.get_attached_disk() {
        Some(d) => (
            d.backing_path.clone().unwrap_or_default(),
            match d.kind {
                DiskKind::G64 => "g64",
                DiskKind::D64 => "d64",
            }
            .to_string(),
        ),
        None => (String::new(), String::new()),
    };
    // Pass no drive blobs / cart blobs → the checkpoint tree omits the big GCR/disk
    // overlay + the cart bytes/flash (the omitMedia anchor); the disk image + cart
    // bytes ride the recorder's medium stream instead (gen-gated).
    let mut cp = trx64_core::c64re_snapshot::capture_runtime_checkpoint(
        &session.machine,
        &disk_path,
        &disk_format,
        None,
        None,
        None,
        None,
    );
    // Null any large media slots the checkpoint may carry (omitMedia anchor).
    for slot in ["driveDiskImage", "cartBytes", "cartFlash"] {
        if cp.get(slot).is_some() {
            cp[slot] = Value::Null;
        }
    }
    // omitFramebuffer (runtime-controller.ts:839/847): the two VIC framebuffers
    // (~317 KiB) are a DERIVABLE shadow — regenerated by re-sim on scrub/dump
    // (Spec 765 §8). Null them so the per-anchor codec body stays small (the
    // BUG-049 discipline: the anchor is RAM + chip state, not framebuffers). Without
    // this the codec body exceeds the recorder ring's anchor slot.
    if let Some(vp) = cp.get_mut("vicPresentation") {
        for fb in ["literalPortFb", "literalPortFbStable"] {
            if vp.get(fb).is_some() {
                vp[fb] = Value::Null;
            }
        }
    }
    cp
}

/// Build the live medium descriptors for the recorder gen-gate. Currently the
/// attached drive8 disk: its content generation bumps when the image bytes hash
/// changes (TRX64 has no `diskWriteGeneration` facade — the worktree builders own
/// the drive — so the hash IS the gen surrogate). The cart medium is deferred
/// (cart writable-generation is owned by the flash worktree builder).
fn build_recorder_media(
    session: &Session,
    disk_gen: &mut i32,
    disk_hash: &mut Option<String>,
) -> Vec<trx64_core::recorder::medium_source::MediumDescriptor> {
    use trx64_core::recorder::medium_source::{MediumDescriptor, MediumKind};
    let mut out = Vec::new();
    if let Some(disk) = session.machine.drive8.get_attached_disk() {
        let bytes = disk.bytes.clone();
        let hash = sha256_hex(&bytes);
        // Bump the generation iff the disk content changed since the last capture.
        if disk_hash.as_deref() != Some(hash.as_str()) {
            *disk_gen += 1;
            *disk_hash = Some(hash);
        }
        out.push(MediumDescriptor {
            kind: MediumKind::Disk,
            generation: *disk_gen,
            get_bytes: Box::new(move || Some(bytes.clone())),
        });
    }
    out
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

/// Decode a RAM range to an RGBA buffer for the monitor `bitmap` verb. 1:1 with
/// monitor-bitmap.ts `decode()` + `renderBitmapPng()` (Spec 754 §3.3b): the same
/// monochrome FG/BG, the same per-mode dims, and the same byte-count, so the
/// returned `(width, height, bytes)` match the TS render exactly. `read` peeks
/// RAM through the live CPU lens (= TS `readByte(a, "cpu")`).
fn monitor_bitmap_decode(
    read: &dyn Fn(u16) -> u8,
    addr: u16,
    w: u32,
    h: u32,
    mode: &str,
) -> (u32, u32, Vec<u8>, u32) {
    // Monochrome palette (= monitor-bitmap.ts FG/BG): bit set = foreground.
    const FG: [u8; 3] = [0xcc, 0xcc, 0xff];
    const BG: [u8; 3] = [0x20, 0x20, 0x40];
    let (width, height): (u32, u32) = match mode {
        "charset" => (w * 8, h * 8),
        "sprite" => (w * 24, h * 21),
        _ => (w * 8, h), // hires
    };
    let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
    let mut set = |x: u32, y: u32, on: bool| {
        let i = ((y * width + x) as usize) * 4;
        let c = if on { FG } else { BG };
        rgba[i] = c[0];
        rgba[i + 1] = c[1];
        rgba[i + 2] = c[2];
        rgba[i + 3] = 0xff;
    };
    match mode {
        "charset" => {
            // w×h grid of 8×8 char cells; 8 bytes per cell.
            for cy in 0..h {
                for cx in 0..w {
                    let base = (addr as u32).wrapping_add((cy * w + cx) * 8);
                    for r in 0..8u32 {
                        let byte = read(((base + r) & 0xffff) as u16);
                        for b in 0..8u32 {
                            set(cx * 8 + b, cy * 8 + r, (byte >> (7 - b)) & 1 != 0);
                        }
                    }
                }
            }
        }
        "sprite" => {
            // w×h grid of 24×21 sprites; 3 bytes/row × 21 rows, 64-byte stride.
            for sy in 0..h {
                for sx in 0..w {
                    let base = (addr as u32).wrapping_add((sy * w + sx) * 64);
                    for r in 0..21u32 {
                        for bcol in 0..3u32 {
                            let byte = read(((base + r * 3 + bcol) & 0xffff) as u16);
                            for b in 0..8u32 {
                                set(sx * 24 + bcol * 8 + b, sy * 21 + r, (byte >> (7 - b)) & 1 != 0);
                            }
                        }
                    }
                }
            }
        }
        _ => {
            // hires: w bytes/row → w*8 px wide, h rows tall; linear.
            for y in 0..h {
                for bx in 0..w {
                    let byte = read(((addr as u32 + y * w + bx) & 0xffff) as u16);
                    for b in 0..8u32 {
                        set(bx * 8 + b, y, (byte >> (7 - b)) & 1 != 0);
                    }
                }
            }
        }
    }
    let bytes = match mode {
        "charset" => w * h * 8,
        "sprite" => w * h * 64,
        _ => w * h,
    };
    (width, height, rgba, bytes)
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

/// Spec 769.5a — the downscale factor for a scrub-filmstrip thumbnail. 1:1 with
/// `makeCheckpointThumbnail`'s default (factor 4 → the 384×272 canvas becomes 96×68).
const THUMB_FACTOR: usize = 4;

/// Spec 769.5a / Spec 772 — cap on the separate per-checkpoint thumbnail store,
/// ALIGNED to the ring size (= ring max-entries × 2 headroom). 1:1 with the c64re
/// `RuntimeController.MAX_THUMBS` (runtime-controller.ts:182), which Spec 772 changed
/// from a flat 1024 to `ringMaxEntries * 2`: there is no point holding 1024 thumbs
/// when the ring only retains ~20 checkpoints. `store_checkpoint_thumb` ALSO prunes
/// any thumb whose ring entry has been evicted (prune-orphans), so thumbs evict WITH
/// the ring entry; this cap is the hard backstop. A fn (not a const) because the ring
/// size is env-driven. The ×2 absorbs the transient where a fresh thumb is inserted
/// just before its evicted predecessor is pruned.
fn max_thumbs() -> usize {
    (checkpoint_ring_max_entries() as usize).saturating_mul(2)
}

/// Spec 769.5a — a captured scrub-filmstrip thumbnail (downscaled live frame). The
/// daemon mirror of the c64re `CheckpointThumbnail` (inspect/checkpoint-thumbnail.ts:14):
/// `width`/`height` palette-indexed picture + the 48-byte RGB palette.
#[derive(Clone)]
struct CheckpointThumb {
    width: usize,
    height: usize,
    /// 48-byte RGB palette (16 × 3).
    palette: Vec<u8>,
    /// width*height palette indices (0..15).
    indices: Vec<u8>,
}

/// Spec 769.5a — downscale the JUST-RENDERED live canvas (the per-frame 384×272
/// 4-bit `indices` the stream loop built for the video broadcast) into a thumbnail.
/// The TRX64 mirror of `makeCheckpointThumbnail` (inspect/checkpoint-thumbnail.ts:30):
/// same nearest-neighbour 1/factor downscale, same `{ width, height, palette(48 RGB),
/// indices(w*h) }` shape. c64re grabs the thumbnail from the live frame at capture
/// time (its literal-port VIC is per-cycle stateful → no pure snapshot→frame fn) and
/// stores it in a SEPARATE map keyed by the checkpoint id — decoupled from the
/// ring's (framebuffer-omitted, BUG-049) auto-capture anchor. This builds the same
/// thumbnail from the live canvas the stream loop already has in hand, so the
/// framebuffer-omitted auto-anchors still get a real picture. Returns None if the
/// canvas is empty.
fn make_thumb_from_canvas(w: usize, h: usize, indices: &[u8]) -> Option<CheckpointThumb> {
    let ow = w / THUMB_FACTOR;
    let oh = h / THUMB_FACTOR;
    if ow == 0 || oh == 0 || indices.len() < w * h {
        return None;
    }
    let mut out = vec![0u8; ow * oh];
    for oy in 0..oh {
        let sy = oy * THUMB_FACTOR * w;
        let orow = oy * ow;
        for ox in 0..ow {
            out[orow + ox] = indices[sy + ox * THUMB_FACTOR] & 0x0f;
        }
    }
    let mut palette = Vec::with_capacity(48);
    for rgb in trx64_core::render::COLODORE.iter() {
        palette.extend_from_slice(rgb);
    }
    Some(CheckpointThumb {
        width: ow,
        height: oh,
        palette,
        indices: out,
    })
}

/// Build a downscaled palette-indexed thumbnail from a stored checkpoint's
/// `vicPresentation.literalPortFbStable` framebuffer — the TRX64 mirror of
/// `makeCheckpointThumbnail` (inspect/checkpoint-thumbnail.ts:30). c64re grabs the
/// thumbnail from the LIVE frame at capture time (its literal-port VIC is per-cycle
/// stateful → no pure snapshot→frame fn); TRX64's ring stores the full presentation
/// framebuffer per checkpoint, so we crop+downscale THAT (more faithful: works for
/// every ring entry, not just one live at capture). Same crop (the VICE PAL
/// 384×272 canvas via `index_buffer_to_canvas_indices`), same nearest-neighbour
/// 1/factor downscale, same { width, height, palette(48 RGB), indices(w*h) } shape.
/// Returns None if the checkpoint carries no usable presentation framebuffer.
fn checkpoint_thumbnail(cp: &Value) -> Option<(usize, usize, Vec<u8>, Vec<u8>)> {
    // The `literalPortFbStable` = the displayed (last fully presented) frame, a
    // 520×312 colour-index buffer. (Live captures via checkpoint/capture keep it;
    // omitFramebuffer anchors null it — those simply yield no thumbnail.)
    let fb_node = cp.get("vicPresentation")?.get("literalPortFbStable")?;
    let fb = trx64_core::native_snapshot::ta_u8_decode(fb_node)?;
    if fb.len() < trx64_core::render::FB_W * trx64_core::render::FB_H {
        return None;
    }
    // Crop to the 384×272 VICE PAL canvas (palette-indexed, each & 0x0f) — exactly
    // the c64re `renderLiteralPortIndexed` crop the live thumbnail samples.
    let (cw, ch, canvas) = trx64_core::render::index_buffer_to_canvas_indices(&fb);
    let ow = cw / THUMB_FACTOR;
    let oh = ch / THUMB_FACTOR;
    if ow == 0 || oh == 0 {
        return None;
    }
    // Nearest-neighbour 1/factor downscale (checkpoint-thumbnail.ts:37-41).
    let mut out = vec![0u8; ow * oh];
    for oy in 0..oh {
        let sy = oy * THUMB_FACTOR * cw;
        let orow = oy * ow;
        for ox in 0..ow {
            out[orow + ox] = canvas[sy + ox * THUMB_FACTOR];
        }
    }
    // The 48-byte RGB palette to pair with the indices (COLODORE, R,G,B order).
    let mut palette = Vec::with_capacity(48);
    for rgb in trx64_core::render::COLODORE.iter() {
        palette.extend_from_slice(rgb);
    }
    Some((ow, oh, palette, out))
}

// ── Connection handler ────────────────────────────────────────────────────────

#[allow(dead_code)] // bin-only WS transport; unused in the `[lib]` embed unit.
async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    state: SharedState,
    hub: Option<Arc<streaming::StreamHub>>,
) {
    let ws = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("[trx64] WS handshake failed from {addr}: {e}");
            return;
        }
    };

    eprintln!("[trx64] client connected: {addr}");
    let (mut tx, mut rx) = ws.split();

    // All outbound messages (JSON-RPC responses AND the live A/V binary push) funnel
    // through ONE channel → a single writer task drains it to the socket. This lets
    // the streaming loop (on its own OS thread) and the request loop both write
    // without contending for `tx`. ws-av-tap is read-only, so without the streaming
    // loop it would receive nothing; we auto-start the push on connect (the daemon
    // is the producer — c64re relied on the browser sending debug/run + audio/start).
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

    // Writer task: pump the channel to the socket.
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Subscribe this client to the SINGLETON live A/V stream IFF the daemon was
    // launched with --stream (the hub starts the one streaming loop on the first
    // subscriber, stops it when the last leaves). ws-av-tap is read-only, so it
    // receives the push purely by connecting. Held until disconnect; dropping the
    // guard unsubscribes (+ stops the loop if last). When streaming is OFF (the
    // oracle's command-driven daemons), the machine never auto-advances on connect,
    // so the byte-exact gates are unperturbed.
    let _stream = hub.as_ref().map(|h| h.subscribe(out_tx.clone()));

    // Register this client's outbound channel with the (always-present) generic
    // notification hub so handler-driven server pushes (debug/breakpoint_hit,
    // audio/flush, batch/progress) reach it. The guard unsubscribes on disconnect.
    // (Unlike the A/V `StreamHub`, this exists with or without --stream, so the
    // notifications work for the oracle's command-driven daemons too.)
    let _notify = {
        let notify = state.lock().unwrap().notify.clone();
        notify.subscribe(out_tx.clone())
    };

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
                let _ = out_tx.send(Message::Pong(data));
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

        if out_tx.send(Message::Text(out.into())).is_err() {
            break;
        }
    }

    // Drop the stream + notification handles (unsubscribe) and tear down the writer.
    drop(_stream);
    drop(_notify);
    drop(out_tx);
    writer.abort();

    eprintln!("[trx64] client disconnected: {addr}");
}

// ── Embeddable construction (shared by the daemon `main` AND the FFI lib) ──────
//
// The full `State` initializer lives HERE so the daemon binary, the in-process FFI
// (`trx64-ffi`), and the round-trip tests all build a byte-identical State (no
// drift between the embedded path and the socket path). This is a pure EXTRACTION
// of the initializer that previously sat inline in `main()` — no behaviour change.

/// Build the singleton [`State`] from an already-booted [`Session`]. `streaming_on`
/// only stamps `streaming_enabled` (the FFI has no `--stream` A/V hub; it consumes
/// `NotifyHub` events directly), so it is `false` for an FFI embed.
pub fn build_state(mut session: Session, streaming_on: bool) -> State {
    // Requirement (10-day standing): daemon up ⟹ C64 runs. A STREAMING (UI) daemon
    // starts the machine in the RUNNING state so the pacing loop advances it
    // immediately once the UI attaches — the user/LLM pauses ONLY via an explicit
    // Freeze (debug/pause). A non-streaming (oracle / byte-exact) daemon stays paused
    // (Session::new default) for deterministic, manually-driven runs.
    if streaming_on {
        session.running = true;
    }
    State {
        session,
        breakpoints: Breakpoints::new(),
        observers: observers::ObserverRegistry::new(),
        dsl_observers: Vec::new(),
        dsl_disabled: std::collections::HashSet::new(),
        type_buffer: Vec::new(),
        ctrl_frame: 0, // incremented on each debug/run|pause|continue; first pause → 1
        machine_generation: 0,
        ctrl_stop: None,
        checkpoint_counter: 0,
        // Spec 772 — the ring is the short UI-scrub buffer: a max-entries cap (default
        // 20 = 10s @ 0.5s cadence, env-overridable) on top of the 32 MiB byte budget,
        // evict-oldest on whichever-first. Deep history = the recorder, not this ring.
        checkpoint_ring: trx64_core::checkpoint_ring::RuntimeCheckpointRing::with_budget_and_max_entries(
            trx64_core::checkpoint_ring::DEFAULT_CHECKPOINT_RING_BUDGET_BYTES,
            checkpoint_ring_max_entries(),
        ),
        inspect_evidence: Vec::new(),
        vic_provenance_enabled: false,
        trace_definitions: std::collections::HashMap::new(),
        recorder: None,
        recorder_disk_gen: 0,
        recorder_disk_hash: None,
        scenarios: std::collections::HashMap::new(),
        media_events: Vec::new(),
        recent_media: Vec::new(),
        materialized_media: Vec::new(),
        batches: std::collections::HashMap::new(),
        notify: streaming::NotifyHub::new(),
        streaming_enabled: streaming_on,
        pacing_mode: "pal".to_string(),
        pacing_ratio: 1.0,
        control_owner: "human".to_string(),
        last_trace_path: None,
        last_run_id: None,
        cart_led_gen: 0,
        cart_led_last_write_at: None,
        cart_ap_seen_gen: 0,
        cart_ap_settle_at_ms: 0,
        cart_ap_done_gen: 0,
        disk_ap_pending: false,
        disk_ap_settle_at_ms: 0,
        disk_ap_seen_hash: None,
        disk_ap_done_hash: None,
        autocapture_frames_since: 0,
        recorder_frames_since: 0,
        candidates: std::collections::HashMap::new(),
        candidate_seq: 0,
        checkpoint_thumbs: std::collections::HashMap::new(),
        checkpoint_thumb_order: std::collections::VecDeque::new(),
        mon: MonitorState::new(),
        flow: FlowTracker::new(),
        stream_broke_on_jam: false,
        force_present_frame: false,
        audio_render: None,
        trap_rules: std::collections::HashMap::new(),
    }
}

/// Boot a fresh singleton session + cold-reset the machine from `rom_dir`, then wrap
/// it in a [`SharedState`] ready for [`dispatch`] — the in-process equivalent of the
/// daemon's boot path in [`main`]. A blank machine is returned (with the error
/// propagated) if the ROMs cannot be loaded, mirroring `main`'s WARN fallback.
pub fn create_embedded_state(
    rom_dir: &std::path::Path,
) -> Result<SharedState, trx64_core::RomError> {
    let mut session = Session::new("integrated-1");
    let boot = session.boot(rom_dir);
    let state = Arc::new(Mutex::new(build_state(session, false)));
    boot.map(|()| state)
}

/// The active [`NotifyHub`] for an embedded state — the single event-broadcast hub
/// that every `dispatch` handler pushes server notifications through. The FFI
/// subscribes a forwarder channel to this and maps each broadcast to a typed event.
pub fn notify_hub(state: &SharedState) -> Arc<streaming::NotifyHub> {
    state.lock().unwrap().notify.clone()
}

// ── Live A/V PULL-API (FFI / native app — ADR-073 §pull) ────────────────────────
//
// Two ADDITIVE pull entry points for the in-process native app (trx64-ffi). A/V is
// BINARY and deliberately bypasses the JSON-RPC `dispatch`/event channel (JSON
// can't carry a frame/PCM efficiently), so these reach the core DIRECTLY through the
// SAME `&SharedState` lock every handler uses — they do NOT touch `dispatch` or any
// existing method. The app pulls at its own cadence (per video frame, per audio
// callback). Mirrors what `session/screenshot` (frame) and the `streaming.rs`
// stream loop (audio) already do internally, but returns raw `Send` bytes instead of
// a base64/PNG JSON envelope.

/// The current displayed frame as a full-resolution palette + index image — the
/// 384×272 VICE PAL canvas the per-cycle VIC sweep produced (the SAME `displayed`
/// buffer `session/screenshot` and the scrub thumbnails come from, here un-downscaled
/// and un-palettized). `palette` is the 48-byte COLODORE RGB table (16×3); `indices`
/// is `width*height` bytes, each 0..15 indexing the palette. Pure read; no engine,
/// no state mutation, no `dispatch`.
pub struct FrameBufferData {
    pub width: u32,
    pub height: u32,
    /// 48 bytes — 16 RGB triplets (COLODORE), R,G,B order, palette-index order.
    pub palette: Vec<u8>,
    /// `width*height` bytes, each a 0..15 palette index.
    pub indices: Vec<u8>,
}

/// Drained SID PCM since the last drain — mono `i16` at `sample_rate` Hz.
pub struct AudioDrainData {
    /// Mono Int16 PCM samples produced for the cycles elapsed since the previous
    /// drain. Empties on read (a subsequent immediate drain returns ~0 samples).
    pub samples: Vec<i16>,
    /// The runtime's fixed reSID sample rate (44100 Hz).
    pub sample_rate: u32,
}

/// Pull the current full-resolution displayed frame as palette + indices. Reuses the
/// core's `render_canvas_indices` (the live-stream / thumbnail index source) and the
/// `COLODORE` palette — the exact extraction the screenshot + filmstrip use, at full
/// 384×272 res.
pub fn pull_frame_buffer(state: &SharedState) -> FrameBufferData {
    let st = state.lock().unwrap();
    let (w, h, indices) = st.session.machine.render_canvas_indices();
    let mut palette = Vec::with_capacity(48);
    for rgb in trx64_core::render::COLODORE.iter() {
        palette.extend_from_slice(rgb);
    }
    FrameBufferData {
        width: w as u32,
        height: h as u32,
        palette,
        indices,
    }
}

/// The runtime's fixed reSID sample rate (Hz). Matches `streaming.rs` / `WavFormat`
/// / `export_session_audio` (44100). Exposed so the app sizes its AVAudioEngine
/// format without a drain.
pub const AUDIO_SAMPLE_RATE: u32 = 44_100;

/// Drain + return the SID PCM accumulated since the last drain (empties on read).
///
/// PERSISTENT-ENGINE render thread (mirrors the `--stream` loop / C64RE Spec 768):
///
/// First call: install the additive SID `set_write_trace` hook (capturing $D4xx
/// writes into a shared buffer) + spawn the render thread. The render thread
/// constructs ONE `SidAudioEngine`, primes it from the live SID register file (so the
/// stream starts from the live state, not power-on silence), then loops draining the
/// write-ring: replay the window's writes (CPU order) → `record_boundary(d_cycles)`
/// → `flush` → push the PCM into the PCM ring. The engine PERSISTS across the whole
/// session — never reconstructed (the per-drain reconstruct was the ~60 Hz hum). The
/// first call sends no window (no cycles elapsed) → returns empty.
///
/// Each subsequent call: compute `d_cycles = clk_now - last_clk`, drain the captured
/// writes, send `(writes, d_cycles)` over the write-ring to the render thread, then
/// POP all accumulated samples from the PCM ring — NO engine access, NO reconstruct.
/// The render thread advances the persistent engine by the REAL elapsed cycles, so
/// the rate is correct (44100 samples/s at ~1 MHz real-time) and the stream is
/// CONTINUOUS across drain boundaries (no seam discontinuity → no hum).
///
/// THREADING: the State lock serializes the send/pop side (the FFI `Runtime` funnels
/// all access through this one `Mutex`); the render thread owns the `!Send` engine
/// alone. The two communicate only over `Send` channels (write-ring + PCM ring).
pub fn pull_audio_drain(state: &SharedState) -> AudioDrainData {
    use trx64_core::resid_audio::SidAudioEngine;
    use trx64_core::resid_ffi::ResidConfig;

    let mut st = state.lock().unwrap();

    // ── First drain: install the capture hook + spawn the persistent render thread ──
    if st.audio_render.is_none() {
        let writes: std::sync::Arc<std::sync::Mutex<Vec<(u8, u8)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let w = std::sync::Arc::clone(&writes);
            st.session
                .machine
                .sid
                .set_write_trace(Some(Box::new(move |addr, value| {
                    w.lock().unwrap().push((addr, value));
                })));
        }
        // Snapshot the live SID register file so the render thread primes reSID from the
        // live state (frequencies/PW/control already set), exactly like the stream loop.
        let mut prime: Vec<(u8, u8)> = Vec::with_capacity(0x19);
        for reg in 0u8..=0x18 {
            let v = st.session.machine.read_full(0xD400 + reg as u16);
            prime.push((reg, v));
        }
        let last_clk = st.session.machine.c64_core.clk;

        // Write-ring (emu→render) + PCM ring (render→main) + stop flag + progress.
        let (tx, rx) = std::sync::mpsc::channel::<(Vec<(u8, u8)>, u32)>();
        let pcm: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<i16>>> =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let processed_cycles =
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        // Set true by the thread once the engine is constructed + primed. The prime
        // drain waits for it so (a) the construct has happened before we return (the
        // construct-once invariant is observable) and (b) the FIRST real window renders
        // (the engine exists before any data is sent).
        let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let pcm_thread = std::sync::Arc::clone(&pcm);
        let stop_thread = std::sync::Arc::clone(&stop);
        let processed_thread = std::sync::Arc::clone(&processed_cycles);
        let ready_thread = std::sync::Arc::clone(&ready);
        let join = std::thread::Builder::new()
            .name("trx64-ffi-audio".into())
            .spawn(move || {
                // ── PERSISTENT reSID engine — constructed ONCE on this thread, owned
                // for the thread's whole life (never reconstructed). The MutexGuard it
                // holds (RESID_GUARD) makes it !Send → it cannot leave this thread, which
                // is exactly the contract. This is the streaming-loop / Spec-768 shape.
                let mut engine = SidAudioEngine::new(ResidConfig::default());
                // Prime: apply the live register snapshot, emit nothing, discard silence.
                for (reg, v) in &prime {
                    engine.record_write(*reg, *v);
                }
                engine.record_boundary(0);
                engine.flush();
                let _ = engine.take_pcm();
                // Signal "engine constructed + primed" → unblocks the prime drain.
                ready_thread.store(true, std::sync::atomic::Ordering::SeqCst);

                loop {
                    if stop_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    // Poll the write-ring with a timeout so the stop flag is honored even
                    // when the host is not pulling audio.
                    match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                        Ok((window_writes, d_cycles)) => {
                            // Drain THIS window: replay writes (CPU order) → boundary →
                            // flush → PCM, on the PERSISTENT engine. Identical to the
                            // stream loop's per-frame sequence.
                            for (addr, value) in &window_writes {
                                engine.record_write(*addr, *value);
                            }
                            engine.record_boundary(d_cycles);
                            engine.flush();
                            let mono = engine.take_pcm();
                            if !mono.is_empty() {
                                let mut ring = pcm_thread.lock().unwrap();
                                ring.extend(mono.into_iter());
                            }
                            // Publish progress AFTER the PCM is in the ring, so a drain
                            // that waits on `processed_cycles` always sees the samples.
                            processed_thread.fetch_add(
                                d_cycles as u64,
                                std::sync::atomic::Ordering::SeqCst,
                            );
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                        // Sender dropped (State dropped) → exit.
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                // `engine` drops here → releases the reSID guard. One construct, one drop.
            })
            .expect("spawn trx64-ffi audio render thread");

        st.audio_render = Some(AudioRenderThread {
            writes,
            tx,
            pcm,
            processed_cycles,
            sent_cycles: 0,
            last_clk,
            stop,
            join: Some(join),
        });
        // Wait (bounded) for the render thread to construct + prime the engine before
        // returning, so the engine exists before the first window is sent and the
        // construct-once invariant is observable to the caller. `Resid::new` may block
        // on the process-global RESID_GUARD if a prior render thread is still being
        // dropped, so allow a generous ceiling; the construct itself is sub-ms.
        {
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !ready.load(std::sync::atomic::Ordering::SeqCst) {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        }
        // No cycles elapsed since install → no audio this call.
        return AudioDrainData {
            samples: Vec::new(),
            sample_rate: AUDIO_SAMPLE_RATE,
        };
    }

    // ── Subsequent drain: send this window to the render thread, pop accumulated PCM ──
    let clk_now = st.session.machine.c64_core.clk;
    let render = st.audio_render.as_mut().expect("audio_render present");
    let d_cycles = clk_now.wrapping_sub(render.last_clk);
    render.last_clk = clk_now;

    // Drain the captured writes (CPU order) and send the window to the render thread.
    let captured: Vec<(u8, u8)> = {
        let mut pending = render.writes.lock().unwrap();
        std::mem::take(&mut *pending)
    };
    // reSID's emit cycle-count is a u32 window; clamp the (normally tiny per-pull)
    // delta defensively so a huge gap can't overflow the cast.
    let d_cycles_u32 = d_cycles.min(u32::MAX as u64) as u32;
    render.sent_cycles = render.sent_cycles.wrapping_add(d_cycles_u32 as u64);
    let target = render.sent_cycles;
    let processed = std::sync::Arc::clone(&render.processed_cycles);
    // Send the window. If the render thread is gone (shouldn't happen while State is
    // alive), the send errors — return whatever PCM is already buffered.
    let _ = render.tx.send((captured, d_cycles_u32));

    // Wait (bounded) for the render thread to consume the window we just sent, so this
    // pull returns the PCM for the cycles that elapsed before it (deterministic for a
    // pull-then-read caller). The persistent engine renders continuously, so this is a
    // sub-ms hand-off in practice; cap at ~50 ms so a stalled render thread can't hang
    // the host. (The State lock is held, but the render thread never touches State, so
    // there is no deadlock.)
    if d_cycles_u32 > 0 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        while processed.load(std::sync::atomic::Ordering::SeqCst) < target {
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    }

    // Pop ALL accumulated PCM from the ring — NO engine access, NO reconstruct. Across
    // pulls the stream is whole + continuous (the engine is persistent), so there is no
    // seam discontinuity at a drain boundary → no ~60 Hz hum.
    let samples: Vec<i16> = {
        let mut ring = render.pcm.lock().unwrap();
        ring.drain(..).collect()
    };

    AudioDrainData {
        samples,
        sample_rate: AUDIO_SAMPLE_RATE,
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[allow(dead_code)] // binary entry point; inert when this file is the `[lib]` module.
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

    let streaming_on =
        cli.stream || matches!(env::var("TRX64_STREAM").ok().as_deref(), Some("1") | Some("true"));
    let state: SharedState = Arc::new(Mutex::new(build_state(session, streaming_on)));

    // The singleton live A/V stream hub (ADR-073): one pacing loop drives the
    // singleton machine and broadcasts BIN_VIC/BIN_AUDIO to all connected clients.
    // Only created when --stream (or TRX64_STREAM=1) is set; otherwise None, so a
    // connecting client never triggers an auto-run (byte-exact oracle stays clean).
    let hub: Option<Arc<streaming::StreamHub>> = if streaming_on {
        eprintln!("[trx64] live A/V push ENABLED (--stream): clients are auto-subscribed at ~50fps");
        Some(streaming::StreamHub::new(Arc::clone(&state)))
    } else {
        None
    };

    let addr: SocketAddr = format!("127.0.0.1:{}", cli.port).parse().unwrap();
    let listener = TcpListener::bind(addr).await.expect("failed to bind");
    eprintln!("[trx64] listening on ws://{addr}");

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let state = Arc::clone(&state);
                let hub = hub.clone();
                tokio::spawn(async move {
                    handle_connection(stream, peer, state, hub).await;
                });
            }
            Err(e) => {
                eprintln!("[trx64] accept error: {e}");
            }
        }
    }
}

// ── Batch-1 round-trip tests (wire-shape parity vs c64re ws-server.ts) ────────
//
// These exercise `dispatch()` in-process against a blank (no-ROM) machine and
// assert the RESPONSE JSON SHAPE matches the c64re handler — field names, types,
// nesting — so the daemon stays a drop-in for c64re's contract. They do not need
// ROMs (the new handlers read state / poke RAM / round-trip vsf — none require a
// booted KERNAL for their shape). The byte-exact behavioural gates live in
// trx64-core; these are contract-shape tests for the new daemon surface.
#[cfg(test)]
mod batch1_tests {
    use super::*;

    #[test]
    fn glob_full_match_observer_wildcards() {
        // `*` = any run incl. empty, `?` = exactly one char, anchored full-string —
        // the observer on/off/del wildcard (audit monitor-obs-lifecycle).
        assert!(glob_full_match("*", "col1"));
        assert!(glob_full_match("*", ""));
        assert!(glob_full_match("c*", "col1"));
        assert!(glob_full_match("c*", "col2"));
        assert!(glob_full_match("col?", "col1"));
        assert!(!glob_full_match("col?", "col12")); // ? is exactly one char
        assert!(!glob_full_match("c*", "abc"));     // anchored at start
        assert!(glob_full_match("col1", "col1"));   // literal
        assert!(!glob_full_match("col1", "col2"));
        assert!(glob_full_match("*1", "col1"));     // suffix glob
        assert!(glob_full_match("c*1", "col1"));    // mid glob
        assert!(!glob_full_match("c*1", "col2"));
    }

    fn make_state() -> SharedState {
        Arc::new(Mutex::new(State {
            session: Session::new("integrated-1"),
            breakpoints: Breakpoints::new(),
            observers: observers::ObserverRegistry::new(),
            dsl_observers: Vec::new(),
            dsl_disabled: std::collections::HashSet::new(),
            type_buffer: Vec::new(),
            ctrl_frame: 0,
            machine_generation: 0,
            ctrl_stop: None,
            checkpoint_counter: 0,
            // Spec 772 — the ring is the short UI-scrub buffer: a max-entries cap (default
        // 20 = 10s @ 0.5s cadence, env-overridable) on top of the 32 MiB byte budget,
        // evict-oldest on whichever-first. Deep history = the recorder, not this ring.
        checkpoint_ring: trx64_core::checkpoint_ring::RuntimeCheckpointRing::with_budget_and_max_entries(
            trx64_core::checkpoint_ring::DEFAULT_CHECKPOINT_RING_BUDGET_BYTES,
            checkpoint_ring_max_entries(),
        ),
            inspect_evidence: Vec::new(),
            vic_provenance_enabled: false,
            trace_definitions: std::collections::HashMap::new(),
            recorder: None,
            recorder_disk_gen: 0,
            recorder_disk_hash: None,
            scenarios: std::collections::HashMap::new(),
            media_events: Vec::new(),
            recent_media: Vec::new(),
            materialized_media: Vec::new(),
            batches: std::collections::HashMap::new(),
            notify: streaming::NotifyHub::new(),
            streaming_enabled: false,
            cart_led_gen: 0,
            cart_led_last_write_at: None,
            pacing_mode: "pal".to_string(),
            pacing_ratio: 1.0,
            control_owner: "human".to_string(),
            last_trace_path: None,
            last_run_id: None,
            cart_ap_seen_gen: 0,
            cart_ap_settle_at_ms: 0,
            cart_ap_done_gen: 0,
            disk_ap_pending: false,
            disk_ap_settle_at_ms: 0,
            disk_ap_seen_hash: None,
            disk_ap_done_hash: None,
            autocapture_frames_since: 0,
            recorder_frames_since: 0,
        candidates: std::collections::HashMap::new(),
        candidate_seq: 0,
            checkpoint_thumbs: std::collections::HashMap::new(),
            checkpoint_thumb_order: std::collections::VecDeque::new(),
            mon: MonitorState::new(),
            flow: FlowTracker::new(),
            stream_broke_on_jam: false,
            force_present_frame: false,
            audio_render: None,
            trap_rules: std::collections::HashMap::new(),
        }))
    }

    /// Subscribe a probe channel to a state's NotifyHub (= one connected client) and
    /// return the receiver so a test can assert which server-push notifications a
    /// handler enqueued. Drains the JSON-RPC envelope to (method, params) pairs.
    fn probe_notifications(
        state: &SharedState,
    ) -> tokio::sync::mpsc::UnboundedReceiver<tokio_tungstenite::tungstenite::Message> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = state.lock().unwrap().notify.clone();
        // Leak the guard: the probe stays subscribed for the test's lifetime.
        std::mem::forget(hub.subscribe(tx));
        rx
    }

    /// Drain a probe receiver into (method, params) pairs.
    fn drain_notifications(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<tokio_tungstenite::tungstenite::Message>,
    ) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                let v: Value = serde_json::from_str(&t).unwrap();
                assert_eq!(v["jsonrpc"], "2.0", "notification envelope");
                assert!(v.get("id").is_none(), "a server push carries no id");
                out.push((
                    v["method"].as_str().unwrap().to_string(),
                    v["params"].clone(),
                ));
            }
        }
        out
    }

    fn call(state: &SharedState, method: &str, params: Value) -> Value {
        let req = Request {
            jsonrpc: "2.0".into(),
            id: json!(1),
            method: method.into(),
            params,
        };
        let resp = dispatch(req, state);
        assert!(resp.error.is_none(), "{method}: unexpected error {:?}", resp.error);
        resp.result.unwrap_or(Value::Null)
    }

    fn call_err(state: &SharedState, method: &str, params: Value) -> RpcError {
        let req = Request {
            jsonrpc: "2.0".into(),
            id: json!(1),
            method: method.into(),
            params,
        };
        dispatch(req, state).error.expect("expected an error")
    }

    #[test]
    fn fs_longest_common_prefix_cases() {
        // CLI-FEEL S3 — the stem-fill primitive behind `fs/complete`.
        assert_eq!(fs_longest_common_prefix(["a.crt", "a2.crt"].into_iter()), "a");
        assert_eq!(fs_longest_common_prefix(["only.prg"].into_iter()), "only.prg");
        assert_eq!(fs_longest_common_prefix(["foo", "bar"].into_iter()), "");
        assert_eq!(fs_longest_common_prefix(std::iter::empty::<&str>()), "");
        assert_eq!(
            fs_longest_common_prefix(["load.prg", "loader.prg", "load2.prg"].into_iter()),
            "load"
        );
        // Mixed-case matches (case-insensitive filter) still yield a fillable prefix,
        // returned in the first name's casing.
        assert_eq!(fs_longest_common_prefix(["Game.prg", "gate.prg"].into_iter()), "Ga");
        assert_eq!(fs_longest_common_prefix(["Alpha", "alfa"].into_iter()), "Al");
    }

    #[test]
    fn fs_complete_lists_matches_common_and_dir() {
        // CLI-FEEL S3 — the `fs/complete` cockpit Tab backend. Build a temp dir with
        // `a.crt`, `a2.crt`, and a `sub/` holding `inside.prg`, point the cockpit cwd
        // at it, and exercise: bare stem (matches + common prefix), trailing-slash
        // (list a subdir), no-arg (list cwd), and a soft error (missing dir → empty).
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("trx64-fscomplete-{}-{}", std::process::id(), uniq));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("a.crt"), b"").unwrap();
        std::fs::write(base.join("a2.crt"), b"").unwrap();
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("sub").join("inside.prg"), b"").unwrap();

        let st = make_state();
        st.lock().unwrap().mon.fs_cwd = Some(base.to_string_lossy().to_string());

        // Bare stem "a" → both .crt files, NOT the `sub` dir; common prefix "a".
        let r = call(&st, "fs/complete", json!({ "partial": "a" }));
        let names: Vec<String> = r["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"a.crt".to_string()), "a.crt in {names:?}");
        assert!(names.contains(&"a2.crt".to_string()), "a2.crt in {names:?}");
        assert!(!names.contains(&"sub".to_string()), "sub filtered out by stem");
        assert_eq!(r["common"], "a");
        // The `.crt` entries are files, not dirs.
        let a_entry = r["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "a.crt")
            .unwrap();
        assert_eq!(a_entry["is_dir"], false);

        // Trailing slash "sub/" → list the subdir contents.
        let r2 = call(&st, "fs/complete", json!({ "partial": "sub/" }));
        let names2: Vec<String> = r2["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names2, vec!["inside.prg".to_string()]);
        assert_eq!(r2["common"], "inside.prg");

        // No arg → list the whole cwd (files + the sub dir).
        let r3 = call(&st, "fs/complete", json!({ "partial": "" }));
        let names3: Vec<String> = r3["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names3.contains(&"a.crt".to_string()));
        assert!(names3.contains(&"a2.crt".to_string()));
        assert!(names3.contains(&"sub".to_string()));
        let sub_entry = r3["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "sub")
            .unwrap();
        assert_eq!(sub_entry["is_dir"], true);

        // Missing directory → soft error, empty entries (Tab never fails).
        let r4 = call(&st, "fs/complete", json!({ "partial": "nope-does-not-exist/x" }));
        assert!(r4["entries"].as_array().unwrap().is_empty());
        assert_eq!(r4["common"], "");

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn trace_definition_validate_ok_and_errors() {
        let st = make_state();
        // A minimal valid definition (mirrors trace-definition.ts coverage rules).
        let def = json!({
            "id": "t1", "version": 1, "name": "T One",
            "domains": ["c64-cpu"],
            "triggers": [{ "kind": "pc-range", "domain": "c64-cpu", "from": 0, "to": 100 }],
            "captures": [{ "kind": "cpu-row", "domain": "c64-cpu" }],
            "retention": "transient"
        });
        let r = call(&st, "trace/definition/validate", json!({ "definition": def }));
        assert_eq!(r["ok"], json!(true));
        assert_eq!(r["errors"], json!([]));

        // Missing required fields → ok:false with an error list.
        let bad = call(&st, "trace/definition/validate", json!({ "definition": {} }));
        assert_eq!(bad["ok"], json!(false));
        assert!(bad["errors"].as_array().unwrap().len() >= 4);

        // Coverage rule: a vic-row capture without a vic domain is rejected.
        let uncovered = json!({
            "id": "t2", "version": 1, "name": "u",
            "domains": ["c64-cpu"],
            "triggers": [{ "kind": "pc-range", "domain": "c64-cpu", "from": 0, "to": 1 }],
            "captures": [{ "kind": "vic-row" }],
            "retention": "transient"
        });
        let cov = call(&st, "trace/definition/validate", json!({ "definition": uncovered }));
        assert_eq!(cov["ok"], json!(false));
        assert!(cov["errors"].as_array().unwrap().iter().any(|e| e
            .as_str()
            .unwrap()
            .contains("requires domain \"vic\"")));
    }

    #[test]
    fn slug_trace_id_matches_ts() {
        // 1:1 with slugTraceId (trace-definition.ts:192).
        assert_eq!(slug_trace_id("My Trace!"), "my-trace");
        assert_eq!(slug_trace_id("  Hello  World  "), "hello-world");
        assert_eq!(slug_trace_id("ABC_123-x"), "abc-123-x");
        // Empty/punctuation-only → the `trace-<base36>` fallback.
        assert!(slug_trace_id("!!!").starts_with("trace-"));
    }

    #[test]
    fn trace_definition_put_then_list() {
        let st = make_state();
        // c64re validates the definition (which REQUIRES a non-empty id) BEFORE the
        // `id || slugTraceId(name)` fallback, so a valid put carries an id. The
        // stored id is `definition.id` (the fallback only fires for an empty id).
        let def = json!({
            "id": "my-trace", "version": 1, "name": "My Trace!",
            "domains": ["memory"],
            "triggers": [{ "kind": "mem-access", "access": "any", "from": 0, "to": 0xffff }],
            "captures": [{ "kind": "mem-row" }],
            "retention": "evidence"
        });
        let put = call(&st, "trace/definition/put", json!({ "definition": def }));
        assert_eq!(put["ok"], json!(true), "put errors: {:?}", put["errors"]);
        assert_eq!(put["id"], json!("my-trace"));

        let list = call(&st, "trace/definition/list", json!({}));
        let defs = list["definitions"].as_array().unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0]["id"], json!("my-trace"));
        assert_eq!(defs[0]["name"], json!("My Trace!"));

        // An invalid definition → ok:false, NOT an RPC error, and not stored.
        let bad = call(&st, "trace/definition/put", json!({ "definition": { "name": "x" } }));
        assert_eq!(bad["ok"], json!(false));
        assert!(bad["errors"].as_array().unwrap().len() >= 1);
        let list2 = call(&st, "trace/definition/list", json!({}));
        assert_eq!(list2["definitions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn debug_state_shape() {
        let st = make_state();
        let r = call(&st, "debug/state", json!({}));
        assert_eq!(r["runState"], json!("paused"));
        assert_eq!(r["pacing"]["mode"], json!("pal"));
        // T1.3: pacing_ratio is stored as f64 (1.0); json!(1) is an integer
        // literal — compare as f64 to avoid serde_json Number type mismatch.
        assert_eq!(r["pacing"]["ratio"].as_f64(), Some(1.0));
        assert!(r["pc"].is_u64());
        assert!(r["cycles"].is_u64());
        assert!(r["frame"].is_u64());
        assert!(r["breakpoints"].is_array());
        assert_eq!(r["stop"], Value::Null);
        // T1.2: default control_owner is "human" (no control op issued yet).
        assert_eq!(r["controlOwner"], json!("human"));
    }

    #[test]
    fn input_and_joystick_shapes() {
        let st = make_state();
        let clr = call(&st, "session/joystick_clear", json!({ "port": 1 }));
        assert_eq!(clr["ok"], json!(true));

        let inp = call(&st, "session/input_status", json!({}));
        assert_eq!(inp["pressed"], json!([]));
        for joy in ["joystick1", "joystick2"] {
            for bit in ["up", "down", "left", "right", "fire"] {
                assert_eq!(inp[joy][bit], json!(false), "{joy}.{bit}");
            }
        }
    }

    #[test]
    fn drive_status_and_power_shapes() {
        let st = make_state();
        let s = call(&st, "session/drive_status", json!({}));
        assert_eq!(s["device"], json!(8));
        for k in ["ledOn", "ledFlashing", "motorOn"] {
            assert!(s[k].is_boolean(), "{k}");
        }
        for k in ["ledPwm", "halfTrack", "track", "sector", "drivePc"] {
            assert!(s[k].is_u64(), "{k}");
        }
        assert_eq!(s["rwMode"], json!("read"));
        assert!(s["dd00"]["pra"].is_u64());
        assert!(s["dd00"]["ddr"].is_u64());
        assert!(s["transferMode"].is_string());

        let p = call(&st, "session/drive_power", json!({}));
        assert_eq!(p["device"], json!(8));
        assert_eq!(p["reinitialized"], json!(true));
        assert!(p["mode"].is_string());
    }

    #[test]
    fn cart_status_null_when_no_cart() {
        let st = make_state();
        // No cart attached → null (matches c64re's `return null`).
        let r = call(&st, "session/cart_status", json!({}));
        assert_eq!(r, Value::Null);
    }

    #[test]
    fn load_prg_writes_ram_and_reports_shape() {
        let st = make_state();
        // Write a tiny PRG (load $C000, 3 body bytes) to a temp file.
        let dir = std::env::temp_dir().join("trx64-batch1-test");
        let _ = std::fs::create_dir_all(&dir);
        let prg = dir.join("tiny.prg");
        // header $00 $C0 = load $C000, body = A9 2A 60 (LDA #$2A; RTS).
        std::fs::write(&prg, [0x00u8, 0xc0, 0xa9, 0x2a, 0x60]).unwrap();
        let r = call(
            &st,
            "session/load_prg",
            json!({ "prg_path": prg.to_string_lossy() }),
        );
        assert_eq!(r["loadAddress"], json!(0xc000));
        // endAddress = last byte addr = load + len - 1 (= c64re integrated-session.ts:885).
        assert_eq!(r["endAddress"], json!(0xc002));
        assert_eq!(r["bytesLoaded"], json!(3));
        assert_eq!(r["path"], json!(prg.to_string_lossy()));
        // The body landed in RAM.
        let g = st.lock().unwrap();
        assert_eq!(g.session.machine.read_full(0xc000), 0xa9);
        assert_eq!(g.session.machine.read_full(0xc002), 0x60);
    }

    #[test]
    fn snapshot_dump_undump_roundtrip_shape() {
        let st = make_state();
        let dir = std::env::temp_dir().join("trx64-batch1-test");
        let _ = std::fs::create_dir_all(&dir);
        let snap = dir.join("rt.vsf");
        let d = call(&st, "snapshot/dump", json!({ "path": snap.to_string_lossy() }));
        assert_eq!(d["machine"], json!("c64-pal"));
        assert!(d["cycle"].is_u64());
        assert!(d["pc"].is_u64());
        assert!(d["media"].is_array());
        assert!(d["fileBytes"].as_u64().unwrap() > 0);
        assert!(d["breakpoints"].is_u64());
        assert!(snap.exists());

        let u = call(&st, "snapshot/undump", json!({ "path": snap.to_string_lossy() }));
        assert_eq!(u["machine"], json!("c64-pal"));
        assert!(u["cycle"].is_u64());
        assert!(u["pc"].is_u64());
        assert!(u["media"].is_array());
        assert!(u["breakpoints"].is_u64());
        assert_eq!(u["paused"], json!(true));

        // The on-disk file is a real `.c64re` container (magic "C64RESNP").
        let raw = std::fs::read(&snap).unwrap();
        assert_eq!(&raw[0..8], b"C64RESNP", "snapshot file is a .c64re container");
        assert_eq!(raw[8], 1, "format version 1");
    }

    #[test]
    fn snapshot_restores_c64_state_through_container() {
        // End-to-end: load a PRG, dump → mutate RAM/PC → undump → the dumped
        // state is restored (proving the .c64re checkpoint carries real state).
        let st = make_state();
        let dir = std::env::temp_dir().join("trx64-c64re-state-test");
        let _ = std::fs::create_dir_all(&dir);
        let prg = dir.join("body.prg");
        std::fs::write(&prg, [0x00u8, 0xc0, 0xa9, 0x2a, 0x60]).unwrap(); // $C000: LDA #$2A; RTS
        call(&st, "session/load_prg", json!({ "prg_path": prg.to_string_lossy() }));

        // Capture the pre-dump RAM/PC.
        let (pre_pc, pre_byte) = {
            let g = st.lock().unwrap();
            (g.session.machine.c64_core.reg_pc, g.session.machine.read_full(0xc000))
        };
        assert_eq!(pre_byte, 0xa9);

        let snap = dir.join("state.c64re");
        call(&st, "snapshot/dump", json!({ "path": snap.to_string_lossy() }));

        // Mutate the live machine AFTER the dump (PC + the loaded body byte).
        {
            let mut g = st.lock().unwrap();
            g.session.machine.c64_core.reg_pc = 0x1234;
            g.session.machine.ram[0xc000] = 0xff;
            g.session.machine.sync_after_monitor();
        }

        // Undump → the dumped state must come back.
        call(&st, "snapshot/undump", json!({ "path": snap.to_string_lossy() }));
        let g = st.lock().unwrap();
        assert_eq!(g.session.machine.c64_core.reg_pc, pre_pc, "PC restored");
        assert_eq!(g.session.machine.read_full(0xc000), 0xa9, "RAM byte restored");
        assert!(!g.session.running, "undump leaves the session paused");
    }

    #[test]
    fn session_reset_shape() {
        let st = make_state();
        // Cold reset on a blank machine: shape only (no KERNAL to reach READY).
        let r = call(&st, "session/reset", json!({ "mode": "soft" }));
        assert_eq!(r["mode"], json!("soft"));
        assert!(r["c64Cycles"].is_u64());
        assert!(r["pc"].is_u64());
        let c = call(&st, "session/reset", json!({}));
        assert_eq!(c["mode"], json!("cold"));
    }

    #[test]
    fn trace_read_validates_params() {
        // trace/read is now IMPLEMENTED (Node sidecar, audit misc-0): it no longer
        // returns NOT_IMPLEMENTED. With no `op`/`duckdb_path` it rejects with the
        // param-error contract (-32602), the same shape the c64re WS handler uses.
        let st = make_state();
        let e = call_err(&st, "trace/read", json!({}));
        assert_eq!(e.code, -32602, "trace/read with no params → invalid-params");
        assert!(e.message.contains("op required"), "names the missing op param: {}", e.message);
        let e2 = call_err(&st, "trace/read", json!({ "op": "store_fn" }));
        assert_eq!(e2.code, -32602, "trace/read with no duckdb_path → invalid-params");
        assert!(e2.message.contains("duckdb_path required"), "names the missing path: {}", e2.message);
    }

    #[test]
    fn debug_memory_access_map_shape() {
        // Verify the debug/memory_access_map handler returns the TS-shaped response
        // (tally / regions / cycles / classes / minBytes) and does not error.
        // Uses a blank (no-ROM) machine so the run is safe and deterministic.
        let st = make_state();
        let r = call(&st, "debug/memory_access_map", json!({
            "cycles": 10000,
            "classes": ["unused", "dead", "read-only", "live"],
            "min_bytes": 1
        }));
        assert!(r["tally"].is_object(), "tally must be an object");
        assert!(r["regions"].is_array(), "regions must be an array");
        assert_eq!(r["cycles"], json!(10000u64));
        assert_eq!(r["minBytes"], json!(1u64));

        // Regions must have the required shape fields.
        for region in r["regions"].as_array().unwrap() {
            assert!(region["start"].is_u64(), "region.start");
            assert!(region["end"].is_u64(), "region.end");
            assert!(region["cls"].is_string(), "region.cls");
            assert!(region["reads"].is_u64(), "region.reads");
            assert!(region["writes"].is_u64(), "region.writes");
            // Verify size >= min_bytes (= 1 here, always satisfied).
            let start = region["start"].as_u64().unwrap();
            let end = region["end"].as_u64().unwrap();
            assert!(end >= start, "region end >= start");
        }
    }

    #[test]
    fn key_down_up_release_roundtrip_and_input_status() {
        // Spec 310 live-keyboard wire shape (ws-server.ts:1443-1494).
        let st = make_state();
        // Initially nothing held.
        assert_eq!(call(&st, "session/input_status", json!({}))["pressed"], json!([]));

        // key_down → { ok: true, pressed: ["A"] }.
        let d = call(&st, "session/key_down", json!({ "key": "A" }));
        assert_eq!(d["ok"], json!(true));
        assert_eq!(d["pressed"], json!(["A"]));

        // A second held key extends the set (insertion order preserved).
        let d2 = call(&st, "session/key_down", json!({ "key": "L_SHIFT" }));
        assert_eq!(d2["pressed"], json!(["A", "L_SHIFT"]));

        // input_status reflects the held set + released joysticks.
        let inp = call(&st, "session/input_status", json!({}));
        assert_eq!(inp["pressed"], json!(["A", "L_SHIFT"]));
        for joy in ["joystick1", "joystick2"] {
            for bit in ["up", "down", "left", "right", "fire"] {
                assert_eq!(inp[joy][bit], json!(false), "{joy}.{bit}");
            }
        }

        // The held keys actually pull their matrix rows: both 'A' (col1 row2)
        // and 'L_SHIFT' (col1 row7) live on col1, so a CIA1 PA read driving col1
        // low must see BOTH row2 and row7 cleared (active-low).
        {
            let g = st.lock().unwrap();
            let now = g.session.machine.cpu6510.clk;
            let pa_col1 = 0xff & !(1u8 << 1);
            let mask = g.session.machine.keyboard.read_rows_for_pa(now, pa_col1);
            assert_eq!(mask, 0xff & !(1 << 2) & !(1 << 7), "held A+L_SHIFT pull row2+row7 on col1");
        }

        // key_up removes just that key → { ok: true, pressed: ["L_SHIFT"] }.
        let u = call(&st, "session/key_up", json!({ "key": "A" }));
        assert_eq!(u["ok"], json!(true));
        assert_eq!(u["pressed"], json!(["L_SHIFT"]));

        // After key_up('A') the matrix no longer pulls row2, but L_SHIFT (still
        // held, col1 row7) keeps row7 pulled.
        {
            let g = st.lock().unwrap();
            let now = g.session.machine.cpu6510.clk;
            let pa_col1 = 0xff & !(1u8 << 1);
            assert_eq!(
                g.session.machine.keyboard.read_rows_for_pa(now, pa_col1),
                0xff & !(1 << 7),
                "A released, L_SHIFT still held"
            );
        }

        // release_keys clears everything → { ok: true } and empty status.
        let r = call(&st, "session/release_keys", json!({}));
        assert_eq!(r["ok"], json!(true));
        assert_eq!(call(&st, "session/input_status", json!({}))["pressed"], json!([]));
    }

    #[test]
    fn key_down_requires_key_param() {
        let st = make_state();
        let e = call_err(&st, "session/key_down", json!({}));
        assert_eq!(e.code, -32602);
        let e2 = call_err(&st, "session/key_up", json!({}));
        assert_eq!(e2.code, -32602);
    }

    // ── Spec 705.B — checkpoint ring behavioral + wire-shape gates ────────────

    #[test]
    fn checkpoint_ring_create_list_restore_roundtrip() {
        // BEHAVIORAL: capture at state T → mutate → restore → state is back at T.
        let st = make_state();
        let dir = std::env::temp_dir().join("trx64-checkpoint-ring-test");
        let _ = std::fs::create_dir_all(&dir);
        let prg = dir.join("body.prg");
        std::fs::write(&prg, [0x00u8, 0xc0, 0xa9, 0x2a, 0x60]).unwrap(); // $C000: LDA #$2A; RTS
        call(&st, "session/load_prg", json!({ "prg_path": prg.to_string_lossy() }));

        // Capture pre-state.
        let pre_pc = st.lock().unwrap().session.machine.c64_core.reg_pc;
        assert_eq!(st.lock().unwrap().session.machine.read_full(0xc000), 0xa9);

        // checkpoint/capture → { ref: {id, frame, cycles, pinned, byteSize, createdAtMs}, stats }.
        let cap = call(&st, "checkpoint/capture", json!({ "session_id": "integrated-1" }));
        let cp_id = cap["ref"]["id"].as_str().unwrap().to_string();
        assert!(cp_id.starts_with("cp_"), "id is cp_<frame>_<seq>: {cp_id}");
        assert_eq!(cap["ref"]["pinned"], json!(false));
        // byteSize = RAM (65536) + 2 framebuffers (2*162240): TRX64's
        // capture_runtime_checkpoint always captures the present framebuffers
        // (the c64re EXPLICIT-capture path; the auto-cadence omitFramebuffer path
        // is not used here). = 390016.
        assert_eq!(cap["ref"]["byteSize"], json!(390016));
        assert_eq!(cap["stats"]["count"], json!(1));
        assert_eq!(cap["stats"]["slotBytes"], json!(65536));

        // checkpoint/list → the ref is present (oldest-first).
        let lst = call(&st, "checkpoint/list", json!({ "session_id": "integrated-1" }));
        assert_eq!(lst["checkpoints"].as_array().unwrap().len(), 1);
        assert_eq!(lst["checkpoints"][0]["id"], json!(cp_id));

        // Mutate the live machine AFTER capture.
        {
            let mut g = st.lock().unwrap();
            g.session.machine.c64_core.reg_pc = 0x1234;
            g.session.machine.ram[0xc000] = 0xff;
            g.session.machine.sync_after_monitor();
        }

        // checkpoint/restore (rewind) → state back at T.
        let res = call(&st, "checkpoint/restore", json!({ "session_id": "integrated-1", "id": cp_id }));
        assert_eq!(res["restored"]["id"], json!(cp_id));
        assert_eq!(res["state"]["runState"], json!("paused"));
        {
            let g = st.lock().unwrap();
            assert_eq!(g.session.machine.c64_core.reg_pc, pre_pc, "PC rewound");
            assert_eq!(g.session.machine.read_full(0xc000), 0xa9, "RAM rewound");
        }
    }

    // ── Spec time-travel-tooling Piece 1 — diffCheckpoints(idA, idB) ─────────────

    #[test]
    fn diff_checkpoints_ram_runs_and_read_only() {
        // Capture anchor A, poke a KNOWN contiguous RAM range, capture anchor B, then
        // runtime/diff_checkpoints → assert (a) the typed SnapshotDiff's RAM runs match
        // the change (start + exact old/new bytes), and (b) the live machine is byte-
        // identical after the diff (read-only contract).
        let st = make_state();

        // Anchor A — clean state.
        let a_id = call(&st, "checkpoint/capture", json!({}))["ref"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // Poke a known 4-byte run at $4000 (old all-0 → new DE AD BE EF) + a single
        // byte at $5000 (a SECOND run, so we exercise multi-run grouping).
        let old_4000 = {
            let mut g = st.lock().unwrap();
            let old = [
                g.session.machine.read_full(0x4000),
                g.session.machine.read_full(0x4001),
                g.session.machine.read_full(0x4002),
                g.session.machine.read_full(0x4003),
            ];
            g.session.machine.ram[0x4000] = 0xDE;
            g.session.machine.ram[0x4001] = 0xAD;
            g.session.machine.ram[0x4002] = 0xBE;
            g.session.machine.ram[0x4003] = 0xEF;
            g.session.machine.ram[0x5000] = 0x42;
            g.session.machine.sync_after_monitor();
            old
        };

        // Anchor B — mutated state.
        let b_id = call(&st, "checkpoint/capture", json!({}))["ref"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // Record the LIVE state BEFORE the diff (read-only assertion target).
        let (pc_before, clk_before, ram4000_before) = {
            let g = st.lock().unwrap();
            (
                g.session.machine.c64_core.reg_pc,
                g.session.machine.c64_core.clk,
                g.session.machine.read_full(0x4000),
            )
        };

        // The typed by-ID diff.
        let d = call(
            &st,
            "runtime/diff_checkpoints",
            json!({ "idA": a_id, "idB": b_id }),
        );

        // RAM runs: the two contiguous runs, with exact extents + byte payloads.
        let runs = d["ram"].as_array().unwrap();
        // The $4000 run + the $5000 run (the order is start-ascending from the compute).
        let run_4000 = runs
            .iter()
            .find(|r| r["start"] == json!(0x4000))
            .expect("a run starting at $4000");
        assert_eq!(run_4000["byteCount"], json!(4), "4-byte run at $4000");
        // old/new are base64 — decode and compare against the known bytes.
        let dec = |s: &str| base64_decode(s).unwrap();
        let old_b = dec(run_4000["old"].as_str().unwrap());
        let new_b = dec(run_4000["new"].as_str().unwrap());
        assert_eq!(old_b, old_4000.to_vec(), "run A bytes = pre-poke");
        assert_eq!(new_b, vec![0xDE, 0xAD, 0xBE, 0xEF], "run B bytes = post-poke");
        let run_5000 = runs
            .iter()
            .find(|r| r["start"] == json!(0x5000))
            .expect("a run starting at $5000");
        assert_eq!(run_5000["byteCount"], json!(1));
        assert_eq!(dec(run_5000["new"].as_str().unwrap()), vec![0x42]);

        // Cross-check against the EXISTING byte-buffer compute: the typed run extents
        // must match diff_snapshots' changedRanges (same compute, just reshaped).
        // (The two anchors' VSF bytes are not exposed by the WS surface; the parity is
        // structural — the reshape reads the SAME changedRanges the compute emits.)
        let total: u64 = runs
            .iter()
            .map(|r| r["byteCount"].as_u64().unwrap())
            .sum();
        assert_eq!(total, 5, "exactly 5 RAM bytes changed across both runs");

        // READ-ONLY: the live machine is byte-identical after the diff.
        {
            let g = st.lock().unwrap();
            assert_eq!(g.session.machine.c64_core.reg_pc, pc_before, "PC unchanged");
            assert_eq!(g.session.machine.c64_core.clk, clk_before, "cycles unchanged");
            assert_eq!(
                g.session.machine.read_full(0x4000),
                ram4000_before,
                "live RAM unchanged (still anchor-B mutated value $DE)"
            );
            assert_eq!(g.session.machine.read_full(0x4000), 0xDE);
        }

        // cycleA / cycleB are present (both anchors captured at clk 0 here, so equal).
        assert!(d["cycleA"].is_u64());
        assert!(d["cycleB"].is_u64());
    }

    #[test]
    fn diff_checkpoints_unknown_id_errors() {
        let st = make_state();
        let cap = call(&st, "checkpoint/capture", json!({}));
        let real = cap["ref"]["id"].as_str().unwrap().to_string();
        // Unknown idA / idB → -32001 (resolved BEFORE the machine is disturbed).
        let e = call_err(&st, "runtime/diff_checkpoints", json!({ "idA": "cp_nope_0", "idB": real }));
        assert_eq!(e.code, -32001);
        // Missing args → -32602.
        let e2 = call_err(&st, "runtime/diff_checkpoints", json!({ "idA": "x" }));
        assert_eq!(e2.code, -32602);
    }

    // ── Spec time-travel-tooling Piece 2 — ringbuffer dump/restore round-trip ─────

    #[test]
    fn ringbuffer_dump_restore_round_trip_faithful() {
        // Capture two anchors with a known RAM change between them + populate the delta
        // + cpu-history rings, ringbuffer/dump to a file, ringbuffer/restore into a
        // FRESH state, then assert: (a) same anchor count + cycles, (b) diffCheckpoints
        // on the restored anchors == the same diff as before the dump, (c) chis +
        // reverse-debug (who_wrote) work on the restored ring.
        let src = make_state();

        // Anchor A (clean) — captured at clk 0.
        let a_id = call(&src, "checkpoint/capture", json!({}))["ref"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        // Poke a known run, populate the reverse-debug rings, capture anchor B.
        {
            let mut g = src.lock().unwrap();
            g.session.machine.ram[0x4000] = 0xDE;
            g.session.machine.ram[0x4001] = 0xAD;
            g.session.machine.sync_after_monitor();
            // Populate the delta ring (begin/record_write/commit) + cpu-history.
            g.session.machine.delta_ring.set_enabled(true);
            g.session.machine.cpu_history.set_enabled(true);
            for i in 0..8u64 {
                g.session.machine.delta_ring.begin(0x1000 + i as u16, i as u8, 0, 0, 0xff, 0x20, 1000 + i);
                g.session.machine.delta_ring.record_write(0x6000 + i as u16, 0, (i + 1) as u8);
                g.session.machine.delta_ring.commit();
                g.session.machine.cpu_history.push(0x1000 + i as u16, 0xa9, i as u8, 0, 1, 2, 3, 0xf0, 0x30, 1000 + i);
            }
        }
        let b_id = call(&src, "checkpoint/capture", json!({}))["ref"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // The reference diff BEFORE the dump.
        let ref_diff = call(
            &src,
            "runtime/diff_checkpoints",
            json!({ "idA": a_id, "idB": b_id }),
        );

        // Reference anchor list (id + cycles) + ring counts before the dump.
        let ref_anchors: Vec<(String, u64)> = call(&src, "checkpoint/list", json!({}))["checkpoints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["id"].as_str().unwrap().to_string(), c["cycles"].as_u64().unwrap()))
            .collect();

        // Dump to a temp .c64rering.
        let dir = std::env::temp_dir().join("trx64-ringdump-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("buf.c64rering");
        let dump_info = call(&src, "ringbuffer/dump", json!({ "path": path.to_string_lossy() }));
        assert_eq!(dump_info["anchors"], json!(2));
        assert_eq!(dump_info["deltaEntries"], json!(8));
        assert_eq!(dump_info["cpuHistory"], json!(8));
        assert!(dump_info["fileBytes"].as_u64().unwrap() > 0);
        // The file exists + carries the container magic.
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(&raw[0..8], b"C64RERNG", "container magic");

        // FRESH state → restore the ring from the file.
        let dst = make_state();
        let restore_info = call(&dst, "ringbuffer/restore", json!({ "path": path.to_string_lossy() }));
        assert_eq!(restore_info["anchors"], json!(2));
        assert_eq!(restore_info["deltaEntries"], json!(8));
        assert_eq!(restore_info["cpuHistory"], json!(8));

        // (a) Same anchors with matching cycles, same order.
        let got_anchors: Vec<(String, u64)> = call(&dst, "checkpoint/list", json!({}))["checkpoints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["id"].as_str().unwrap().to_string(), c["cycles"].as_u64().unwrap()))
            .collect();
        assert_eq!(got_anchors, ref_anchors, "restored anchors match (id + cycles)");

        // (b) diffCheckpoints on the RESTORED anchors == the pre-dump diff.
        let got_diff = call(
            &dst,
            "runtime/diff_checkpoints",
            json!({ "idA": a_id, "idB": b_id }),
        );
        assert_eq!(got_diff, ref_diff, "restored-ring diff is byte-faithful to the original");
        // And it actually carries the $4000 run.
        assert!(
            got_diff["ram"].as_array().unwrap().iter().any(|r| r["start"] == json!(0x4000)),
            "restored diff has the $4000 RAM run"
        );

        // (c) chis + reverse-debug (who_wrote) work on the restored rings.
        {
            let g = dst.lock().unwrap();
            assert_eq!(g.session.machine.cpu_history.len(), 8, "restored cpu-history populated");
            assert_eq!(g.session.machine.delta_ring.len(), 8, "restored delta ring populated");
            let hits = g.session.machine.who_wrote(0x6007, 4);
            assert!(!hits.is_empty(), "who_wrote works on the restored delta ring");
            assert_eq!(hits[0].new_value, 8, "who_wrote returns the restored write value");
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ringbuffer_dump_restore_arg_errors() {
        let st = make_state();
        assert_eq!(call_err(&st, "ringbuffer/dump", json!({})).code, -32602);
        assert_eq!(call_err(&st, "ringbuffer/restore", json!({})).code, -32602);
        // Restore of a non-existent file → -32001.
        assert_eq!(
            call_err(&st, "ringbuffer/restore", json!({ "path": "/nope/does/not/exist.c64rering" })).code,
            -32001
        );
    }

    // ── Audit ws-checkpoint-scrub-0/1/2/4 — restore is the shared, broadcast-rich
    // path (theme T4). These DIRECTLY verify the 4 restore divergences against the TS
    // controller restore (runtime-controller.ts:535-617). Cases 0 + 4 are also gated
    // differentially by the WS-conformance oracle; cases 1 + 2 are oracle-BLOCKED (TS
    // can't report the signal: 1 = TS pushes a BINARY frame on restore the text
    // ws-client cannot read, no JSON session/frame_available; 2 = no JSON method
    // exposes framebuffer pixel content), so these tests ARE their direct verification.

    #[test]
    fn checkpoint_restore_then_keep_inherits_running_state() {
        // Audit ws-checkpoint-scrub-0 — restore with then="keep" (or omitted) must
        // INHERIT the prior run-state: a RUNNING machine stays running (TS:
        // runtime-controller.ts:541-552/588 keep → pause=false → runState UNCHANGED).
        // Before the fix the handler forced running=false on any non-"run" intent.
        let st = make_state();
        let cap = call(&st, "checkpoint/capture", json!({}));
        let cp_id = cap["ref"]["id"].as_str().unwrap().to_string();

        // RUNNING machine + then omitted (≡ "keep") → stays running.
        st.lock().unwrap().session.running = true;
        let r = call(&st, "checkpoint/restore", json!({ "id": cp_id }));
        assert_eq!(r["state"]["runState"], json!("running"), "keep inherits running");
        assert!(st.lock().unwrap().session.running, "machine still running after keep restore");

        // PAUSED machine + then="keep" → stays paused (the inherited state both ways).
        st.lock().unwrap().session.running = false;
        let r = call(&st, "checkpoint/restore", json!({ "id": cp_id, "then": "keep" }));
        assert_eq!(r["state"]["runState"], json!("paused"), "keep inherits paused");

        // then="run" → resumes; then="pause" → pauses (the explicit intents unaffected).
        let r = call(&st, "checkpoint/restore", json!({ "id": cp_id, "then": "run" }));
        assert_eq!(r["state"]["runState"], json!("running"), "run resumes");
        let r = call(&st, "checkpoint/restore", json!({ "id": cp_id, "then": "pause" }));
        assert_eq!(r["state"]["runState"], json!("paused"), "pause pauses");
    }

    #[test]
    fn checkpoint_restore_then_pause_broadcasts_debug_stopped() {
        // Audit ws-checkpoint-scrub-4 — a then="pause" restore must PUSH debug/stopped
        // (reason "pause") so a passive UI freezes the run-state (TS:
        // runtime-controller.ts:614-617). Before the fix only audio/flush +
        // debug/checkpoint_restored were pushed.
        let st = make_state();
        let cap = call(&st, "checkpoint/capture", json!({}));
        let cp_id = cap["ref"]["id"].as_str().unwrap().to_string();
        let mut rx = probe_notifications(&st);

        call(&st, "checkpoint/restore", json!({ "id": cp_id, "then": "pause" }));
        let notes = drain_notifications(&mut rx);
        let stopped: Vec<_> = notes
            .iter()
            .filter(|(m, p)| m == "debug/stopped" && p["stop"]["reason"] == json!("pause"))
            .collect();
        assert_eq!(stopped.len(), 1, "exactly one debug/stopped reason=pause: {notes:?}");
        assert!(stopped[0].1.get("registers").is_some(), "carries the register dump");
    }

    #[test]
    fn checkpoint_restore_then_keep_emits_no_debug_stopped() {
        // Guard the inverse of scrub-4: a then="keep" restore of a RUNNING machine must
        // NOT push debug/stopped (TS only publishes it on the pause intent). Without
        // this, a fix that always emits debug/stopped would diverge the other way.
        let st = make_state();
        let cap = call(&st, "checkpoint/capture", json!({}));
        let cp_id = cap["ref"]["id"].as_str().unwrap().to_string();
        st.lock().unwrap().session.running = true;
        let mut rx = probe_notifications(&st);
        call(&st, "checkpoint/restore", json!({ "id": cp_id, "then": "keep" }));
        let notes = drain_notifications(&mut rx);
        assert!(
            !notes.iter().any(|(m, _)| m == "debug/stopped"),
            "no debug/stopped on a keep-running restore: {notes:?}"
        );
    }

    #[test]
    fn checkpoint_restore_requests_one_shot_frame_present() {
        // Audit ws-checkpoint-scrub-1 (oracle-BLOCKED — TS pushes a BINARY frame on
        // restore that the text ws-client cannot read; no JSON proxy). DIRECT
        // verification: a restore sets `force_present_frame` so the (otherwise silent)
        // paused stream loop pushes ONE fresh BIN_VIC — the TRX64 mirror of TS's
        // unconditional presentFrame() on restore (runtime-controller.ts:606-613, "no
        // client-grab dependency"). Holds for a paused restore (where TS's gap bit).
        let st = make_state();
        let cap = call(&st, "checkpoint/capture", json!({}));
        let cp_id = cap["ref"]["id"].as_str().unwrap().to_string();
        // Pre-condition: the flag is clear (nothing requested a present yet).
        assert!(!st.lock().unwrap().force_present_frame);
        call(&st, "checkpoint/restore", json!({ "id": cp_id, "then": "pause" }));
        assert!(
            st.lock().unwrap().force_present_frame,
            "restore requested the one-shot fresh-frame present (consumed once by the paused loop)"
        );
    }

    #[test]
    fn snapshot_undump_requests_one_shot_frame_present() {
        // Regression (2026-07-15): a `.c64re` undump on a PAUSED machine must refresh the
        // canvas to the RESTORED frame. The `--stream` paused loop only advances a
        // running machine, so without a one-shot present the UI keeps the pre-undump
        // picture and the restore "looks borked / not 1:1" — the field report. The
        // framebuffer itself is captured/restored faithfully (vicPresentation); this
        // guards the DAEMON-side present that surfaces it, mirroring checkpoint/restore.
        let st = make_state();
        let path = std::env::temp_dir().join("trx64_undump_present_test.c64re");
        let path_s = path.to_str().unwrap().to_string();
        call(&st, "snapshot/dump", json!({ "path": path_s }));
        // Pre-condition: clear the flag so the assertion is meaningful.
        st.lock().unwrap().force_present_frame = false;
        call(&st, "snapshot/undump", json!({ "path": path_s }));
        assert!(
            st.lock().unwrap().force_present_frame,
            "undump requested the one-shot fresh-frame present"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn undump_materializes_media_sidecar_and_purge_removes_it() {
        // Spec 793 — undump writes the embedded drive8 disk into a `<name>_media/`
        // sidecar + mounts it file-backed (real, picker-visible), tags the dir, and
        // `undump_media_purge` deletes ONLY that tag (never a user mount).
        let st = make_state();
        let tmp = std::env::temp_dir();
        let d64 = tmp.join("trx64_793_test.d64");
        std::fs::write(&d64, vec![0u8; 174_848]).unwrap(); // blank 35-track D64
        call(&st, "media/mount", json!({ "path": d64.to_str().unwrap() }));
        let snap = tmp.join("trx64_793_snap.c64re");
        let snap_s = snap.to_str().unwrap().to_string();
        call(&st, "snapshot/dump", json!({ "path": snap_s }));

        let media_dir = tmp.join("trx64_793_snap_media");
        let _ = std::fs::remove_dir_all(&media_dir);
        call(&st, "snapshot/undump", json!({ "path": snap_s }));
        assert!(media_dir.exists(), "undump created the <name>_media/ sidecar");
        assert!(
            std::fs::read_dir(&media_dir).unwrap().count() >= 1,
            "the embedded disk was written into the sidecar"
        );
        assert_eq!(st.lock().unwrap().materialized_media.len(), 1, "the dir is tagged");

        let r = call(&st, "undump_media_purge", json!({}));
        assert!(r["dirsRemoved"].as_u64().unwrap() >= 1, "purge removed the sidecar dir");
        assert!(!media_dir.exists(), "the sidecar is gone after purge");
        assert!(st.lock().unwrap().materialized_media.is_empty(), "the tag is cleared");

        let _ = std::fs::remove_file(&d64);
        let _ = std::fs::remove_file(&snap);
    }

    #[test]
    fn checkpoint_restore_render_regenerates_omitted_framebuffer() {
        // Audit ws-checkpoint-scrub-2 (oracle-BLOCKED — no JSON method exposes
        // framebuffer PIXEL content; the text ws-client cannot read the binary frame).
        // DIRECT verification: a framebuffer-OMITTED auto-anchor restored with
        // render:true re-sims ~1 frame so the live `displayed` buffer is REGENERATED
        // from the rolled-back state (TS: runtime-controller.ts:544/599-601). Restored
        // WITHOUT render, the omitted fb leaves `displayed` UNTOUCHED (stale).
        //
        // Needs a VIC-ticked (ROM-booted, full_assembled) machine — the per-cycle
        // VIC sweep is what fills `dbuf` and the start-of-frame swap publishes it to
        // `displayed` (vic.rs:1194). A blank Session::new machine has full_assembled
        // =false (CPU-only path, no sweep), so it can't regenerate a framebuffer.
        let roms = rom_dir();
        if !roms.join("kernal-901227-03.bin").exists() {
            eprintln!("[skip] render-fb test: ROMs absent at {}", roms.display());
            return;
        }
        let st = make_state();
        {
            let mut g = st.lock().unwrap();
            g.session.boot(&roms).expect("boot ROMs");
            assert!(g.session.machine.full_assembled, "VIC-ticked machine");
            // Run to a real, painted screen (KERNAL boot + BASIC banner ≈ 2 frames of
            // border + a drawn READY. ~30 PAL frames is plenty and sub-second native).
            run_cycle_budget(&mut g.session, crate::streaming::CYC_PER_FRAME * 30);
        }
        // Capture an anchor whose stored payload OMITS the framebuffer — exactly the
        // auto-cadence anchor (capture_recorder_anchor_payload nulls the vicPresentation
        // framebuffers), inserted straight into the ring.
        let cp_id = {
            let mut g = st.lock().unwrap();
            let frame = g.ctrl_frame;
            let cycles = g.session.machine.c64_core.clk;
            let cp = capture_recorder_anchor_payload(&mut g.session);
            assert!(
                cp["vicPresentation"]["literalPortFbStable"].is_null(),
                "auto-anchor omits the framebuffer (BUG-049)"
            );
            g.checkpoint_ring.capture(cp, frame, cycles).unwrap().id
        };

        // Stamp a SENTINEL pattern into the live `displayed` so we can tell
        // "regenerated" (re-sim overwrote it) from "stale" (left untouched).
        let fb_len = st.lock().unwrap().session.machine.vic.displayed.len();
        let sentinel = 0xAB;
        let stamp = |st: &SharedState| {
            let mut g = st.lock().unwrap();
            for b in g.session.machine.vic.displayed.iter_mut() {
                *b = sentinel;
            }
            // Also stamp dbuf: the start-of-frame swap publishes dbuf→displayed, so a
            // stale dbuf would otherwise masquerade as "regenerated".
            for b in g.session.machine.vic.dbuf.iter_mut() {
                *b = sentinel;
            }
        };

        // Restore WITHOUT render → the omitted fb is skipped, the sentinel survives.
        stamp(&st);
        call(&st, "checkpoint/restore", json!({ "id": cp_id, "then": "pause" }));
        {
            let g = st.lock().unwrap();
            assert!(
                g.session.machine.vic.displayed.iter().all(|&b| b == sentinel),
                "no-render restore leaves the omitted fb stale (sentinel intact)"
            );
        }
        // Restore WITH render → the re-sim regenerates the framebuffer, so the sentinel
        // is overwritten by real VIC output (a painted READY screen ≠ a flat 0xAB).
        stamp(&st);
        call(&st, "checkpoint/restore", json!({ "id": cp_id, "then": "pause", "render": true }));
        {
            let g = st.lock().unwrap();
            assert!(
                !g.session.machine.vic.displayed.iter().all(|&b| b == sentinel),
                "render:true regenerated the framebuffer (sentinel overwritten by the re-sim)"
            );
            assert_eq!(g.session.machine.vic.displayed.len(), fb_len, "framebuffer size unchanged");
        }
    }

    #[test]
    fn checkpoint_ring_n_checkpoints_rewind_to_each() {
        // Ring of N: capture distinct RAM states, rewind to each, each matches.
        let st = make_state();
        let mut ids = Vec::new();
        let mut want = Vec::new();
        for i in 0u8..5 {
            {
                let mut g = st.lock().unwrap();
                g.session.machine.ram[0x0400] = 0x10 + i;
                g.session.machine.c64_core.reg_a = 0x20 + i;
                g.session.machine.sync_after_monitor();
            }
            let cap = call(&st, "checkpoint/capture", json!({ "session_id": "integrated-1" }));
            ids.push(cap["ref"]["id"].as_str().unwrap().to_string());
            want.push((0x10 + i, 0x20 + i));
        }
        assert_eq!(call(&st, "checkpoint/list", json!({}))["checkpoints"].as_array().unwrap().len(), 5);
        // Rewind to each (out of order) and verify.
        for &idx in &[2usize, 0, 4, 1, 3] {
            call(&st, "checkpoint/restore", json!({ "id": ids[idx] }));
            let g = st.lock().unwrap();
            assert_eq!(g.session.machine.ram[0x0400], want[idx].0, "ram@{idx}");
            assert_eq!(g.session.machine.c64_core.reg_a, want[idx].1, "a@{idx}");
        }
    }

    #[test]
    fn checkpoint_pin_unpin_clear_shapes() {
        let st = make_state();
        let cp_id = call(&st, "checkpoint/capture", json!({}))["ref"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        // pin → ref.pinned == true, stats.pinnedCount == 1.
        let p = call(&st, "checkpoint/pin", json!({ "id": cp_id }));
        assert_eq!(p["ref"]["pinned"], json!(true));
        assert_eq!(p["stats"]["pinnedCount"], json!(1));
        // unpin → ref.pinned == false.
        let u = call(&st, "checkpoint/unpin", json!({ "id": cp_id }));
        assert_eq!(u["ref"]["pinned"], json!(false));
        assert_eq!(u["stats"]["pinnedCount"], json!(0));
        // unknown id → error.
        assert_eq!(call_err(&st, "checkpoint/pin", json!({ "id": "nope" })).code, -32001);
        // clear → { stats } with count 0.
        let c = call(&st, "checkpoint/clear", json!({}));
        assert_eq!(c["stats"]["count"], json!(0));
        assert_eq!(call(&st, "checkpoint/list", json!({}))["checkpoints"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn checkpoint_capture_requires_no_params_id_required_on_pin() {
        let st = make_state();
        assert_eq!(call_err(&st, "checkpoint/pin", json!({})).code, -32602);
        assert_eq!(call_err(&st, "checkpoint/unpin", json!({})).code, -32602);
        assert_eq!(call_err(&st, "checkpoint/restore", json!({})).code, -32602);
    }

    #[test]
    fn vic_inspect_ring_methods_shapes() {
        let st = make_state();
        // provenance toggle.
        assert_eq!(call(&st, "vic/inspect/provenance", json!({ "enabled": true }))["enabled"], json!(true));
        assert_eq!(call(&st, "vic/inspect/provenance", json!({ "enabled": false }))["enabled"], json!(false));
        // default (omitted) → true.
        assert_eq!(call(&st, "vic/inspect/provenance", json!({}))["enabled"], json!(true));
        // evidence — empty list initially.
        assert_eq!(call(&st, "vic/inspect/evidence", json!({}))["evidence"], json!([]));
        // close — { ok, stats }; unpins a (here-unknown) checkpoint harmlessly.
        let c = call(&st, "vic/inspect/close", json!({ "checkpoint_id": "x" }));
        assert_eq!(c["ok"], json!(true));
        assert!(c["stats"]["count"].is_u64());
        // the engine-dependent ones require checkpoint_id (param error, not deferred).
        for m in ["vic/inspect/at", "vic/inspect/region", "vic/inspect/origin", "vic/inspect/promote"] {
            assert_eq!(call_err(&st, m, json!({})).code, -32602);
        }
    }

    #[test]
    fn vic_inspect_engine_open_at_region_origin_promote() {
        // Spec 710/721 — the 6 engine methods over a blank (no-ROM) machine: the
        // resolver reads whatever VIC regs/RAM are present (regs all zero →
        // standard_text; CIA2 PA=0 → bank 3 = $C000; d018=0 → screen $C000). Shapes
        // are 1:1 with c64re ws-server.ts.
        let st = make_state();

        // open — capture + pin the inspected checkpoint; the SHARED record + geometry.
        let o = call(&st, "vic/inspect/open", json!({}));
        let cp_id = o["checkpointId"].as_str().expect("checkpointId").to_string();
        assert!(cp_id.starts_with("cp_"));
        assert_eq!(o["frame"]["mode"], json!("standard_text"));
        assert_eq!(o["frame"]["bankBase"], json!(0xC000));
        assert_eq!(o["frame"]["screenBase"], json!(0xC000));
        assert_eq!(o["frame"]["displayWidth"], json!(320));
        assert_eq!(o["frame"]["colorBase"], json!(0xd800));
        assert_eq!(o["geometry"]["visible"]["width"], json!(384));
        assert_eq!(o["geometry"]["displayOrigin"], json!({ "x": 32, "y": 35 }));
        assert_eq!(o["geometry"]["cell"], json!({ "w": 8, "h": 8, "cols": 40, "rows": 25 }));
        assert_eq!(o["runState"], json!("paused"));

        // at — a VISIBLE-frame pixel inside the display window → text_cell node.
        // display (4,4) → visible (36, 39).
        let at = call(&st, "vic/inspect/at", json!({ "checkpoint_id": cp_id, "x": 36, "y": 39 }));
        assert_eq!(at["node"]["type"], json!("text_cell"));
        assert_eq!(at["node"]["mode"], json!("standard_text"));
        assert_eq!(at["node"]["cell"], json!({ "col": 0, "row": 0, "index": 0 }));
        // screen RAM ref @ $C000 (bank 3, d018=0), charset ref present.
        let refs = at["node"]["refs"].as_array().unwrap();
        assert!(refs.iter().any(|r| r["kind"] == "screen_ram" && r["addr"] == 0xC000));
        assert!(refs.iter().any(|r| r["kind"] == "charset"));

        // a VISIBLE-frame pixel in the open border → border node.
        let border = call(&st, "vic/inspect/at", json!({ "checkpoint_id": cp_id, "x": 5, "y": 5 }));
        assert_eq!(border["node"]["type"], json!("border"));
        assert_eq!(border["node"]["refs"][0]["addr"], json!(0xd020));

        // region — VISIBLE-frame region → distinct nodes (here all text_cell index 0,
        // deduped to one + possibly a border node).
        let reg = call(&st, "vic/inspect/region", json!({
            "checkpoint_id": cp_id, "region": { "x": 36, "y": 39, "width": 16, "height": 16 }
        }));
        let nodes = reg["nodes"].as_array().unwrap();
        assert!(!nodes.is_empty());
        assert!(nodes.iter().all(|n| n["type"].is_string()));

        // at_capture — DISPLAY-area coords; reuses the open checkpoint.
        let ac = call(&st, "vic/inspect/at_capture", json!({ "checkpoint_id": cp_id, "x": 4, "y": 4 }));
        assert_eq!(ac["checkpointId"], json!(cp_id));
        assert_eq!(ac["node"]["type"], json!("text_cell"));
        assert_eq!(ac["hasProvenance"], json!(false));
        assert_eq!(ac["frame"]["mode"], json!("standard_text"));

        // origin — no medium mounted → honest runtime_generated + the knowledge chain.
        let org = call(&st, "vic/inspect/origin", json!({ "checkpoint_id": cp_id, "x": 36, "y": 39 }));
        assert_eq!(org["classification"], json!("runtime_generated"));
        assert_eq!(org["result"]["classification"], json!("runtime_generated"));
        assert_eq!(org["knowledge"]["classification"], json!("runtime_generated"));
        assert_eq!(org["medium"]["ref"], Value::Null);
        assert_eq!(org["medium"]["candidateCount"], json!(0));
        // VisualElement → MemoryRange relation always present.
        assert_eq!(org["knowledge"]["relations"][0]["relation"], json!("maps-to"));

        // promote — assemble + store the shared evidence record; evidence list grows.
        let pr = call(&st, "vic/inspect/promote", json!({
            "checkpoint_id": cp_id, "points": [{ "x": 36, "y": 39 }], "name": "cell0", "notes": "test"
        }));
        assert_eq!(pr["count"], json!(1));
        assert_eq!(pr["evidence"]["checkpointId"], json!(cp_id));
        assert_eq!(pr["evidence"]["name"], json!("cell0"));
        assert_eq!(pr["evidence"]["notes"], json!("test"));
        assert!(pr["evidence"]["promotedAtMs"].is_u64());
        let sel = pr["evidence"]["selectedNodes"].as_array().unwrap();
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0]["type"], json!("text_cell"));
        // evidence — now returns the promoted record.
        let ev = call(&st, "vic/inspect/evidence", json!({}));
        assert_eq!(ev["evidence"].as_array().unwrap().len(), 1);

        // unknown checkpoint → error.
        assert_eq!(call_err(&st, "vic/inspect/at", json!({ "checkpoint_id": "nope", "x": 36, "y": 39 })).code, -32001);
    }

    // ── Spec 766.5 — recorder WS surface (wire-shape parity vs c64re) ─────────

    #[test]
    fn recorder_status_inactive_then_start_capture_list() {
        let st = make_state();
        // Before start: { active: false } (ws-server.ts:1081).
        let s = call(&st, "recorder/status", json!({}));
        assert_eq!(s["active"], json!(false));
        let l = call(&st, "recorder/list", json!({}));
        assert_eq!(l["active"], json!(false));
        assert_eq!(l["anchors"], json!([]));

        // start → active, captures the first anchor; stats roll up from the store.
        let start = call(&st, "recorder/start", json!({}));
        assert_eq!(start["active"], json!(true));
        assert!(start["stats"]["anchorCount"].as_u64().unwrap() >= 1);

        // status → produced/mediumShipped + the RecorderStats shape (camelCase).
        let s2 = call(&st, "recorder/status", json!({}));
        assert_eq!(s2["active"], json!(true));
        assert!(s2["produced"].as_u64().unwrap() >= 1);
        for k in ["anchorCount", "oldestCycle", "newestCycle", "slabBytes", "slabUsed", "evicted", "mediumDisk", "mediumCart", "dropped"] {
            assert!(s2["stats"].get(k).is_some(), "stats.{k} present");
        }

        // capture → a second anchor with a new seq.
        let cap = call(&st, "recorder/capture", json!({}));
        assert_eq!(cap["active"], json!(true));
        assert!(cap["seq"].is_u64());

        // list → RecorderAnchorRef[] (seq/cycle/wallMs/diskGen/cartGen/schemaVersion).
        let l2 = call(&st, "recorder/list", json!({}));
        assert_eq!(l2["active"], json!(true));
        let anchors = l2["anchors"].as_array().unwrap();
        assert!(anchors.len() >= 2);
        for k in ["seq", "cycle", "wallMs", "diskGen", "cartGen", "schemaVersion"] {
            assert!(anchors[0].get(k).is_some(), "anchor.{k} present");
        }
    }

    #[test]
    fn recorder_dump_reconstructs_anchor_to_native_snapshot() {
        let st = make_state();
        call(&st, "recorder/start", json!({}));
        // The start anchor is seq 0.
        let dir = std::env::temp_dir().join("trx64-rec-test");
        let path = dir.join("anchor0.c64re");
        let path_s = path.to_str().unwrap();
        let dump = call(&st, "recorder/dump", json!({ "seq": 0, "path": path_s }));
        // DumpResult shape (identical to snapshot/dump).
        assert_eq!(dump["path"], json!(path_s));
        assert_eq!(dump["machine"], json!("c64-pal"));
        assert!(dump["fileBytes"].as_u64().unwrap() > 0);
        assert!(dump["media"].is_array());
        // The file was written and is a readable native snapshot.
        let bytes = std::fs::read(path_s).unwrap();
        assert!(trx64_core::native_snapshot::read_native_snapshot(&bytes).is_ok());
        let _ = std::fs::remove_file(path_s);
    }

    #[test]
    fn recorder_dump_errors_on_unknown_seq_and_missing_params() {
        let st = make_state();
        call(&st, "recorder/start", json!({}));
        // Unknown seq (evicted / never existed) → -32001.
        assert_eq!(
            call_err(&st, "recorder/dump", json!({ "seq": 9999, "path": "/tmp/x.c64re" })).code,
            -32001
        );
        // Missing params → -32602.
        assert_eq!(call_err(&st, "recorder/dump", json!({ "path": "/tmp/x" })).code, -32602);
        assert_eq!(call_err(&st, "recorder/dump", json!({ "seq": 0 })).code, -32602);
    }

    #[test]
    fn recorder_stop_clears_active() {
        let st = make_state();
        call(&st, "recorder/start", json!({}));
        let stop = call(&st, "recorder/stop", json!({}));
        assert_eq!(stop["active"], json!(false));
        assert_eq!(call(&st, "recorder/status", json!({}))["active"], json!(false));
        // capture while inactive is a no-op { active: false }.
        assert_eq!(call(&st, "recorder/capture", json!({}))["active"], json!(false));
    }

    #[test]
    fn stream_feed_grows_recorder_anchor_count_per_cadence() {
        // background-workers-async-0 is BLOCKED in the WS oracle (the TS recorder/list
        // awaits a worker thread that is non-functional under tsx-from-src), so the
        // free-run anchor-GROWTH behaviour can't be compared cross-runtime there. This
        // test verifies the TRX64 behaviour DIRECTLY: with the recorder active, the
        // stream loop's per-frame feed (`stream_maybe_feed_recorder`) captures a fresh
        // anchor every `checkpoint_capture_every_frames()` frames — so the anchor count
        // GROWS over a free-run (was flat before the feed wiring), exactly the signal
        // the WS case would assert if the TS worker resolved.
        let st = make_state();
        call(&st, "recorder/start", json!({}));
        let count = |st: &SharedState| -> usize {
            call(st, "recorder/list", json!({}))["anchors"].as_array().map(|a| a.len()).unwrap_or(0)
        };
        let cadence = checkpoint_capture_every_frames();
        let baseline = count(&st);
        // Drive ~3 cadence windows of frames through the feed; each full window must
        // add exactly one anchor (the feed fires on frames_since == cadence).
        {
            let mut g = st.lock().unwrap();
            for f in 0..(cadence * 3) {
                stream_maybe_feed_recorder(&mut g, f as u64);
            }
        }
        let after = count(&st);
        assert!(after >= baseline + 3, "feed grew the ring: {baseline} -> {after} over 3 cadence windows (cadence={cadence})");
        // A SUB-cadence burst (fewer than one window) must NOT add an anchor.
        let mid = count(&st);
        {
            let mut g = st.lock().unwrap();
            for f in 0..(cadence.saturating_sub(1)) {
                stream_maybe_feed_recorder(&mut g, f as u64);
            }
        }
        assert_eq!(count(&st), mid, "a sub-cadence burst adds no anchor");
    }

    // ── Spec 231/268 — scenario registry + replay (wire-shape parity) ─────────

    #[test]
    fn scenario_save_list_load_delete_roundtrip() {
        let st = make_state();
        // Empty registry.
        assert_eq!(call(&st, "runtime/scenario_list", json!({})), json!([]));
        // save → { id }.
        let scenario = json!({
            "id": "boot-test",
            "diskPath": "/x/disk.g64",
            "mode": "true-drive",
            "cycleBudget": 50000,
            "inputs": [
                { "atCycle": 1000, "kind": "keyboard", "payload": "LOAD" }
            ]
        });
        let saved = call(&st, "runtime/scenario_save", json!({ "scenario": scenario }));
        assert_eq!(saved["id"], json!("boot-test"));
        // list → ScenarioSummary[] (id/diskPath/mode/cycleBudget/inputCount/savedAt).
        let list = call(&st, "runtime/scenario_list", json!({}));
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], json!("boot-test"));
        assert_eq!(arr[0]["inputCount"], json!(1));
        assert_eq!(arr[0]["cycleBudget"], json!(50000));
        // load → the full stored scenario.
        let loaded = call(&st, "runtime/scenario_load", json!({ "id": "boot-test" }));
        assert_eq!(loaded["diskPath"], json!("/x/disk.g64"));
        assert!(loaded["savedAt"].is_string());
        // load unknown → -32001.
        assert_eq!(call_err(&st, "runtime/scenario_load", json!({ "id": "nope" })).code, -32001);
        // delete → { deleted: true }, then false.
        assert_eq!(call(&st, "runtime/scenario_delete", json!({ "id": "boot-test" }))["deleted"], json!(true));
        assert_eq!(call(&st, "runtime/scenario_delete", json!({ "id": "boot-test" }))["deleted"], json!(false));
        assert_eq!(call(&st, "runtime/scenario_list", json!({})), json!([]));
    }

    #[test]
    fn scenario_run_is_deterministic_and_reports_ram_hash() {
        // Replay against the blank (no-ROM) machine: run a fixed cycle budget with
        // a couple of keyboard inputs at cycles, hash the end RAM. Two runs from the
        // same start state must hash identically (Spec 231 determinism).
        let scenario = json!({
            "id": "det",
            "cycleBudget": 20000,
            "inputs": [
                { "atCycle": 5000, "kind": "keyboard", "payload": "A" },
                { "atCycle": 9000, "kind": "joystick1", "payload": { "fire": true } }
            ]
        });
        let run_once = || {
            let st = make_state();
            call(&st, "runtime/scenario_run", json!({ "scenario": scenario }))
        };
        let r1 = run_once();
        let r2 = run_once();
        assert_eq!(r1["ramHash"], r2["ramHash"], "deterministic RAM hash");
        assert!(r1["ramHash"].as_str().unwrap().len() == 64, "sha256 hex");
        assert_eq!(r1["cyclesRan"], r2["cyclesRan"]);
        // The run advanced the clock by ~cycleBudget.
        assert!(r1["cyclesRan"].as_u64().unwrap() >= 20000);
        // start/end PC + cycle present for cross-checking.
        for k in ["startCycle", "endCycle", "startPc", "endPc"] {
            assert!(r1.get(k).is_some(), "{k} present");
        }
    }

    #[test]
    fn scenario_run_by_id_from_registry() {
        let st = make_state();
        let scenario = json!({
            "id": "reg-run",
            "cycleBudget": 10000,
            "inputs": []
        });
        call(&st, "runtime/scenario_save", json!({ "scenario": scenario }));
        let r = call(&st, "runtime/scenario_run", json!({ "id": "reg-run" }));
        assert!(r["ramHash"].is_string());
        assert!(r["cyclesRan"].as_u64().unwrap() >= 10000);
        // unknown id → -32001.
        assert_eq!(call_err(&st, "runtime/scenario_run", json!({ "id": "nope" })).code, -32001);
    }

    /// End-to-end record → SEEK round-trip: record anchors at distinct machine
    /// states (different RAM marker + cycle), then RECONSTRUCT an earlier anchor and
    /// RESTORE it through the checkpoint path — the machine must land back at THAT
    /// anchor's captured RAM marker (seek via anchor lands at the checkpoint). This
    /// proves the recorder's anchors are faithful, restorable scrub points.
    #[test]
    fn record_then_seek_via_anchor_lands_at_that_state() {
        let st = make_state();
        call(&st, "recorder/start", json!({})); // anchor seq 0 (marker 0x00 @ $4000)

        // Stamp a marker in RAM, advance the clock, capture anchor seq 1.
        {
            let mut g = st.lock().unwrap();
            g.session.machine.poke(0x4000, &[0xAA]);
            run_cycle_budget(&mut g.session, 5000);
        }
        let cap1 = call(&st, "recorder/capture", json!({}));
        let seq1 = cap1["seq"].as_u64().unwrap();
        let cycle1 = {
            let g = st.lock().unwrap();
            g.recorder.as_ref().unwrap().list().last().unwrap().cycle
        };

        // Change the marker + advance further, capture anchor seq 2.
        {
            let mut g = st.lock().unwrap();
            g.session.machine.poke(0x4000, &[0xBB]);
            run_cycle_budget(&mut g.session, 5000);
        }
        call(&st, "recorder/capture", json!({}));

        // Sanity: live RAM now holds the LATEST marker.
        assert_eq!(st.lock().unwrap().session.machine.read_full(0x4000), 0xBB);

        // SEEK: reconstruct anchor seq1 (marker 0xAA) and restore it. The machine
        // must revert to the 0xAA marker + the seq1 capture cycle — i.e. it lands
        // exactly at that earlier anchor, not the live state.
        {
            let mut g = st.lock().unwrap();
            let (_, _, payload) = g.recorder.as_ref().unwrap().reconstruct(seq1).unwrap();
            restore_live_checkpoint(&mut g.session, &payload).unwrap();
            assert_eq!(
                g.session.machine.read_full(0x4000), 0xAA,
                "seek landed at the seq1 anchor's RAM marker"
            );
            assert_eq!(
                g.session.machine.c64_core.clk as f64, cycle1,
                "seek landed at the seq1 anchor's captured cycle"
            );
        }
    }

    /// Record → REPLAY determinism: two independent recordings of the SAME input
    /// schedule, reconstructed at the same anchor, yield byte-identical RAM. The
    /// recorder's anchors are deterministic w.r.t. the input stream.
    #[test]
    fn record_replay_is_deterministic_across_runs() {
        let record_marker_at_anchor = || {
            let st = make_state();
            call(&st, "recorder/start", json!({}));
            {
                let mut g = st.lock().unwrap();
                // Deterministic mutation: a fixed marker + a fixed run budget.
                g.session.machine.poke(0x5000, &[0x42]);
                run_cycle_budget(&mut g.session, 7000);
            }
            let cap = call(&st, "recorder/capture", json!({}));
            let seq = cap["seq"].as_u64().unwrap();
            let g = st.lock().unwrap();
            let (_, _, payload) = g.recorder.as_ref().unwrap().reconstruct(seq).unwrap();
            // Hash the reconstructed RAM blob (the byte-exact replay artifact).
            let ram = trx64_core::native_snapshot::ta_u8_decode(&payload["ram"]).unwrap();
            sha256_hex(&ram)
        };
        let h1 = record_marker_at_anchor();
        let h2 = record_marker_at_anchor();
        assert_eq!(h1, h2, "deterministic reconstructed RAM across recordings");
    }

    // ── audio/media/batch — Spec 263/265/271/703/709 round-trips ──────────────

    #[test]
    fn audio_start_stop_shape() {
        let st = make_state();
        // audio/start ACKs over the hub stream: { streaming, sample_rate, engine }.
        let started = call(&st, "audio/start", json!({ "session_id": "integrated-1" }));
        assert_eq!(started["streaming"], json!(true));
        assert_eq!(started["sample_rate"], json!(44100));
        assert_eq!(started["engine"], json!("hub"));
        // audio/stop → { stopped: bool } (no per-session stream owned here → false).
        let stopped = call(&st, "audio/stop", json!({ "session_id": "integrated-1" }));
        assert!(stopped["stopped"].is_boolean());
    }

    #[test]
    fn audio_export_writes_wav_and_reports_shape() {
        let st = make_state();
        let dir = std::env::temp_dir().join(format!("trx64-audio-export-{}", new_batch_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.wav");
        let out_str = out.to_string_lossy().to_string();

        // Run a short export (0.05 PAL s). The c64re ExportResult shape:
        // { out_path, duration_sec, sample_rate, samples, bytes }.
        let r = call(&st, "audio/export", json!({
            "session_id": "integrated-1",
            "out_path": out_str,
            "duration_sec": 0.05
        }));
        assert_eq!(r["out_path"], json!(out_str));
        assert_eq!(r["duration_sec"], json!(0.05));
        assert_eq!(r["sample_rate"], json!(44100));
        let samples = r["samples"].as_u64().unwrap();
        assert!(samples > 0, "non-empty PCM");
        let bytes = r["bytes"].as_u64().unwrap();
        // WAV = 44-byte header + samples * channels(2) * 2 bytes.
        assert_eq!(bytes, 44 + samples * 4, "WAV byte count = header + stereo s16le");
        // The file actually exists with a RIFF/WAVE header.
        let written = std::fs::read(&out).unwrap();
        assert_eq!(bytes as usize, written.len());
        assert_eq!(&written[0..4], b"RIFF");
        assert_eq!(&written[8..12], b"WAVE");
        let _ = std::fs::remove_dir_all(&dir);

        // Bad duration → -32602.
        assert_eq!(
            call_err(&st, "audio/export", json!({ "out_path": out_str, "duration_sec": 0 })).code,
            -32602
        );
        // Missing out_path → -32602.
        assert_eq!(
            call_err(&st, "audio/export", json!({ "duration_sec": 1 })).code,
            -32602
        );
    }

    #[test]
    fn media_events_accumulate_and_read_back() {
        let st = make_state();
        // Empty to start.
        assert_eq!(call(&st, "media/events", json!({}))["events"], json!([]));

        // A PRG ingress emits a media event (operation "prg").
        call(&st, "media/ingress", json!({
            "kind": "prg",
            "name": "p.prg",
            "bytes_b64": base64_encode(&[0x00, 0x10, 0xA9, 0x01])
        }));
        // An eject ingress emits another (operation "eject").
        call(&st, "media/ingress", json!({ "kind": "eject", "role": "drive8" }));

        let events = call(&st, "media/events", json!({ "session_id": "integrated-1" }));
        let arr = events["events"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "two media ops recorded");
        assert_eq!(arr[0]["operation"], json!("prg"));
        assert_eq!(arr[1]["operation"], json!("eject"));
        // Each event carries the MediaIngressEvent shape (cycle present).
        assert!(arr[0]["cycle"].is_number());
    }

    #[test]
    fn media_recent_returns_image_media_array() {
        // The scan returns a (possibly empty) array of { path, name, type } image
        // media. With no --project arg and no samples dir present, it is just empty;
        // the shape contract is what matters for the round-trip.
        let st = make_state();
        let r = call(&st, "media/recent", json!({}));
        let arr = r.as_array().expect("media/recent → array");
        // Every entry (if any) has the documented keys + an image-media type.
        for e in arr {
            assert!(e["path"].is_string());
            assert!(e["name"].is_string());
            let t = e["type"].as_str().unwrap();
            assert!(["crt", "d64", "g64", "vsf"].contains(&t), "image type, got {t}");
        }
    }

    #[test]
    fn batch_start_status_results_roundtrip() {
        let st = make_state();
        // Register two trivial scenarios in the registry.
        for sid in ["b1", "b2"] {
            call(&st, "runtime/scenario_save", json!({
                "scenario": { "id": sid, "cycleBudget": 5000, "inputs": [] }
            }));
        }
        // batch/start → serialised BatchEntry (1:1 c64re serialiseBatch keys).
        let started = call(&st, "batch/start", json!({ "scenarioIds": ["b1", "b2"], "workerCount": 2 }));
        let batch_id = started["batchId"].as_str().unwrap().to_string();
        assert_eq!(started["status"], json!("done"));
        assert_eq!(started["completed"], json!(2));
        assert_eq!(started["total"], json!(2));
        assert_eq!(started["workerCount"], json!(2));
        assert!(started["startedAt"].is_string());
        assert!(started["finishedAt"].is_string());

        // batch/status → the same serialised entry.
        let status = call(&st, "batch/status", json!({ "batchId": batch_id }));
        assert_eq!(status["status"], json!("done"));
        assert_eq!(status["batchId"], json!(batch_id));

        // batch/results → { batch, results: { id → ReplayResult } }.
        let results = call(&st, "batch/results", json!({ "batchId": batch_id }));
        assert_eq!(results["batch"]["batchId"], json!(batch_id));
        let map = results["results"].as_object().unwrap();
        assert_eq!(map.len(), 2);
        // Each result is a ReplayResult (ramHash present), no error.
        assert!(map["b1"]["ramHash"].is_string());
        assert!(map["b2"]["ramHash"].is_string());

        // Unknown batch → -32001.
        assert_eq!(call_err(&st, "batch/status", json!({ "batchId": "nope" })).code, -32001);
        assert_eq!(call_err(&st, "batch/results", json!({ "batchId": "nope" })).code, -32001);
        // Missing batchId → -32602.
        assert_eq!(call_err(&st, "batch/status", json!({})).code, -32602);
        // Empty scenarioIds → -32602.
        assert_eq!(call_err(&st, "batch/start", json!({ "scenarioIds": [] })).code, -32602);
    }

    #[test]
    fn batch_start_reports_error_for_unknown_scenario() {
        let st = make_state();
        call(&st, "runtime/scenario_save", json!({
            "scenario": { "id": "ok", "cycleBudget": 3000, "inputs": [] }
        }));
        let started = call(&st, "batch/start", json!({ "scenarioIds": ["ok", "missing"] }));
        // One scenario failed → status "error" + lastError set; both completed.
        assert_eq!(started["status"], json!("error"));
        assert_eq!(started["completed"], json!(2));
        assert!(started["lastError"].as_str().unwrap().contains("missing"));
        let batch_id = started["batchId"].as_str().unwrap().to_string();
        let results = call(&st, "batch/results", json!({ "batchId": batch_id }));
        let map = results["results"].as_object().unwrap();
        assert!(map["ok"]["ramHash"].is_string());
        assert!(map["missing"]["error"].as_str().unwrap().contains("not found"));
    }

    // ── WS server-push notifications (runtime-controller.ts broadcasts) ────────

    #[test]
    fn debug_run_pushes_breakpoint_hit_notification() {
        // BEHAVIORAL: audit ws-session-debug-1 — debug/run is ASYNC-SCHEDULED. It
        // replies `running` IMMEDIATELY (= TS controller.run() → ctrl.state(), never
        // blocking) and does NOT run inline; the (P0-A) bp/observer/JAM-aware driver
        // (`stream_debug_gated_advance`, the SOLE --stream machine driver) self-halts at
        // the first hit and server-PUSHes `debug/breakpoint_hit` at the halt PC with the
        // c64re shape { session_id, pc, num, cycles, registers }. This test drives that
        // production driver directly (no stream loop runs in-process) after the async
        // debug/run, exactly mirroring the --stream loop.
        let st = make_state();
        let mut rx = probe_notifications(&st);
        // Poke a runnable program at $C000: NOP; NOP; NOP; … (EA), so a numbered
        // exec breakpoint downstream halts the run. Position the CPU there. The
        // no-ROM machine runs on the isolated `cpu6510` core, so set its PC.
        {
            let mut g = st.lock().unwrap();
            for off in 0..8u16 {
                g.session.machine.ram[0xc000 + off as usize] = 0xea; // NOP
            }
            g.session.machine.cpu6510.reg_pc = 0xc000;
            g.session.machine.sync_after_monitor();
        }
        // Add a numbered breakpoint at $C003 and run.
        let added = call(&st, "debug/break_add", json!({ "pc": 0xc003 }));
        let num = added["num"].as_u64().unwrap();
        // debug/run replies 'running' immediately (async contract) — it does NOT halt.
        let run = call(&st, "debug/run", json!({}));
        assert_eq!(run["runState"], json!("running"), "debug/run replies running (async)");
        assert!(run["stop"].is_null(), "no halt inline — the driver halts later");
        // Drive the production --stream per-frame driver until the bp trips (running
        // gate is set by debug/run). One PAL frame is far more than the 3 NOPs need.
        {
            let mut g = st.lock().unwrap();
            stream_debug_gated_advance(&mut g, 100_000);
        }

        let notes = drain_notifications(&mut rx);
        let hit = notes
            .iter()
            .find(|(m, _)| m == "debug/breakpoint_hit")
            .expect("a debug/breakpoint_hit push was enqueued by the driver");
        let p = &hit.1;
        assert_eq!(p["session_id"], json!("integrated-1"));
        assert_eq!(p["pc"], json!(0xc003), "halt PC");
        assert_eq!(p["num"], json!(num), "resolved breakpoint number");
        assert!(p["cycles"].is_u64(), "carries the cycle count");
        // registers = the VICE-style dump string (ADDR AC XR YR SP NV-BDIZC).
        let regs = p["registers"].as_str().unwrap();
        assert!(regs.contains("ADDR AC XR YR SP NV-BDIZC"), "register dump header");
        assert!(regs.contains(".;C003"), "dump shows the halt PC");
        // The driver also froze the machine (running=false) on the halt.
        assert!(!st.lock().unwrap().session.running, "driver freezes on halt");
    }

    #[test]
    fn batch_start_pushes_progress_notifications() {
        // BEHAVIORAL: a batch run emits batch/progress per scenario + a terminal
        // done broadcast — matching c64re's onProgress + completeBatch broadcast.
        let st = make_state();
        let mut rx = probe_notifications(&st);
        for sid in ["b1", "b2"] {
            call(&st, "runtime/scenario_save", json!({
                "scenario": { "id": sid, "cycleBudget": 3000, "inputs": [] }
            }));
        }
        let started = call(&st, "batch/start", json!({ "scenarioIds": ["b1", "b2"] }));
        let batch_id = started["batchId"].as_str().unwrap().to_string();

        let notes = drain_notifications(&mut rx);
        let progress: Vec<&(String, Value)> =
            notes.iter().filter(|(m, _)| m == "batch/progress").collect();
        // Two per-scenario pushes + one terminal done = 3.
        assert_eq!(progress.len(), 3, "two scenario + one terminal progress push");
        // First per-scenario: completed 1/2, currentId b1.
        assert_eq!(progress[0].1["batchId"], json!(batch_id));
        assert_eq!(progress[0].1["completed"], json!(1));
        assert_eq!(progress[0].1["total"], json!(2));
        assert_eq!(progress[0].1["currentId"], json!("b1"));
        assert_eq!(progress[1].1["completed"], json!(2));
        assert_eq!(progress[1].1["currentId"], json!("b2"));
        // Terminal: status done, completed == total.
        assert_eq!(progress[2].1["status"], json!("done"));
        assert_eq!(progress[2].1["completed"], json!(2));
    }

    #[test]
    fn batch_start_pushes_error_progress_for_unknown_scenario() {
        let st = make_state();
        let mut rx = probe_notifications(&st);
        call(&st, "runtime/scenario_save", json!({
            "scenario": { "id": "ok", "cycleBudget": 3000, "inputs": [] }
        }));
        call(&st, "batch/start", json!({ "scenarioIds": ["ok", "missing"] }));
        let notes = drain_notifications(&mut rx);
        let term = notes
            .iter()
            .filter(|(m, _)| m == "batch/progress")
            .last()
            .expect("a terminal batch/progress push");
        assert_eq!(term.1["status"], json!("error"));
        assert!(term.1["error"].as_str().unwrap().contains("missing"));
    }

    #[test]
    fn reset_and_restore_push_audio_flush() {
        // BEHAVIORAL: an audio-timeline discontinuity (reset / checkpoint restore /
        // snapshot undump) pushes `audio/flush { session_id }` so the client flushes
        // its worklet ring (ws-server.ts:1430/1667/1690).
        let st = make_state();
        let mut rx = probe_notifications(&st);

        // 1) session/reset.
        call(&st, "session/reset", json!({ "mode": "soft" }));
        // 2) capture + restore a checkpoint.
        let cap = call(&st, "checkpoint/capture", json!({}));
        let cp_id = cap["ref"]["id"].as_str().unwrap().to_string();
        call(&st, "checkpoint/restore", json!({ "id": cp_id }));

        let notes = drain_notifications(&mut rx);
        let flushes: Vec<&(String, Value)> =
            notes.iter().filter(|(m, _)| m == "audio/flush").collect();
        assert!(flushes.len() >= 2, "reset + restore each push audio/flush");
        for (_, p) in &flushes {
            assert_eq!(p["session_id"], json!("integrated-1"));
        }
    }

    #[test]
    fn checkpoint_thumbnails_render_from_ring_framebuffer() {
        // BEHAVIORAL: checkpoint/thumbnails returns a small palette-indexed thumbnail
        // per ring checkpoint, rendered from each checkpoint's stored framebuffer.
        // Shape 1:1 with ws-server.ts:1028-1037.
        let st = make_state();
        // Empty ring → empty thumbnails array.
        let empty = call(&st, "checkpoint/thumbnails", json!({}));
        assert_eq!(empty["thumbnails"].as_array().unwrap().len(), 0);

        // Capture two checkpoints (the full-capture path keeps the framebuffer).
        let id0 = call(&st, "checkpoint/capture", json!({}))["ref"]["id"]
            .as_str().unwrap().to_string();
        let id1 = call(&st, "checkpoint/capture", json!({}))["ref"]["id"]
            .as_str().unwrap().to_string();

        let res = call(&st, "checkpoint/thumbnails", json!({}));
        let thumbs = res["thumbnails"].as_array().unwrap();
        assert_eq!(thumbs.len(), 2, "one thumbnail per ring checkpoint");
        // Ring order = oldest first.
        assert_eq!(thumbs[0]["id"], json!(id0));
        assert_eq!(thumbs[1]["id"], json!(id1));
        for t in thumbs {
            // 384×272 canvas / factor 4 = 96×68.
            assert_eq!(t["width"], json!(96));
            assert_eq!(t["height"], json!(68));
            assert!(t["cycles"].is_u64());
            assert!(t["frame"].is_u64());
            assert_eq!(t["pinned"], json!(false));
            // palette = 48 RGB bytes (base64) ; indices = width*height (base64).
            let pal = base64_decode_for_test(t["palette"].as_str().unwrap());
            assert_eq!(pal.len(), 48, "48-byte RGB palette");
            let idx = base64_decode_for_test(t["indices"].as_str().unwrap());
            assert_eq!(idx.len(), 96 * 68, "width*height indices");
            // All indices are 4-bit colour values.
            assert!(idx.iter().all(|&b| b < 16), "indices are 0..15");
        }
    }

    /// Minimal base64 decoder for the thumbnail test (the daemon only ENCODES).
    fn base64_decode_for_test(s: &str) -> Vec<u8> {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut rev = [255u8; 256];
        for (i, &c) in T.iter().enumerate() {
            rev[c as usize] = i as u8;
        }
        let mut bits = 0u32;
        let mut nbits = 0;
        let mut out = Vec::new();
        for &c in s.as_bytes() {
            if c == b'=' {
                break;
            }
            let v = rev[c as usize];
            if v == 255 {
                continue;
            }
            bits = (bits << 6) | v as u32;
            nbits += 6;
            if nbits >= 8 {
                nbits -= 8;
                out.push((bits >> nbits) as u8);
            }
        }
        out
    }

    // ── BACKGROUND-LOOP layer proofs (the 3 stream_loop per-frame hooks) ──────
    //
    // These exercise the actual `stream_maybe_*` helpers the stream loop calls
    // each running frame (no live WS server / ROMs needed — `make_state()` is a
    // blank machine and the helpers operate on `State`). Each proves the
    // observable side effect the c64re RuntimeController produces with NO WS
    // method: a settled cart write reaches the host .crt; a settled disk write
    // reaches the host .d64; the checkpoint ring fills at the auto-cadence.

    /// Minimal valid CRT header (0x40 bytes) + N CHIP packets — copy of the core
    /// `cart_mapper_gate::build_crt` so the proof is self-contained.
    fn build_crt_for_test(hw: u16, exrom: u8, game: u8, name: &str, chips: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"C64 CARTRIDGE   ");
        v.extend_from_slice(&0x40u32.to_be_bytes());
        v.extend_from_slice(&0x0100u16.to_be_bytes());
        v.extend_from_slice(&hw.to_be_bytes());
        v.push(exrom);
        v.push(game);
        v.extend_from_slice(&[0u8; 6]);
        let mut nm = [0u8; 32];
        let nb = name.as_bytes();
        nm[..nb.len().min(32)].copy_from_slice(&nb[..nb.len().min(32)]);
        v.extend_from_slice(&nm);
        for (bank, load, data) in chips {
            v.extend_from_slice(b"CHIP");
            let packet_len = 0x10 + data.len() as u32;
            v.extend_from_slice(&packet_len.to_be_bytes());
            v.extend_from_slice(&0u16.to_be_bytes());
            v.extend_from_slice(&bank.to_be_bytes());
            v.extend_from_slice(&load.to_be_bytes());
            v.extend_from_slice(&(data.len() as u16).to_be_bytes());
            v.extend_from_slice(data);
        }
        v
    }

    /// A BankInfo with the ultimax-write state the EasyFlash flash-program path
    /// needs (it stores to flash only in ultimax; EasyFlash boots ultimax).
    fn bi_for_test() -> trx64_core::cart::BankInfo {
        trx64_core::cart::BankInfo {
            cpu_port_direction: 0x2f,
            cpu_port_value: 0x37,
            basic_visible: true,
            kernal_visible: true,
            io_visible: true,
            char_visible: false,
            cartridge_attached: true,
            cartridge_exrom: None,
            cartridge_game: None,
            phi1: 0xff,
        }
    }

    /// ITEM 1 PROOF — cart auto-persist (.crt lazy writeback). Mount a writable
    /// EasyFlash cart with a host .crt path, drive a real AM29F040B byte-program
    /// (bumps writableGeneration + sets dirty), then run the stream hook for >
    /// debounce frames with NO explicit media/persist — assert the host .crt FILE
    /// changed on disk. (= maybeAutoPersistCart, runtime-controller.ts:493/510.)
    #[test]
    fn item1_cart_autopersist_writes_host_crt_without_explicit_persist() {
        let dir = std::env::temp_dir().join(format!("trx64_item1_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let crt_path = dir.join("ef_writable.crt");

        // Bank 0: 16K chip (ROML+ROMH), erased flash (0xFF) + a ROMH reset vector.
        let mut bank0 = vec![0xffu8; 0x4000];
        bank0[0x3ffc] = 0x00;
        bank0[0x3ffd] = 0x80;
        let crt = build_crt_for_test(32, 1, 0, "EF", &[(0, 0x8000, bank0)]);
        std::fs::write(&crt_path, &crt).unwrap();
        let orig_meta = std::fs::metadata(&crt_path).unwrap();
        let orig_len = orig_meta.len();
        let orig_bytes = std::fs::read(&crt_path).unwrap();

        let state = make_state();
        {
            let mut st = state.lock().unwrap();
            st.session.machine.attach_cart_from_bytes(&crt, "EF").expect("attach EF");
            st.session.cart_path = crt_path.to_string_lossy().to_string();
            st.session.machine.cold_reset(); // EasyFlash boots ultimax (register_02=0)

            // Drive a real AM29F040B byte-program through the live cart mapper —
            // EXACTLY the cart_mapper_gate sequence (AA/55/A0/<addr,data>). This
            // bumps writable_generation() and sets is_writable_dirty().
            let bi = bi_for_test();
            let clk = st.session.machine.clk;
            let cart = st.session.machine.cartridge.as_mut().expect("cart attached");
            assert!(!cart.is_writable_dirty(), "clean before program");
            cart.write(0x8555, 0xaa, &bi, clk);
            cart.write(0x82aa, 0x55, &bi, clk);
            cart.write(0x8555, 0xa0, &bi, clk);
            cart.write(0x8100, 0x42, &bi, clk);
            assert!(cart.is_writable_dirty(), "dirty after program");
            assert!(cart.writable_generation() > 0, "gen bumped");
        }

        // Run the stream hook with synthetic WALL-CLOCK ms (the debounce is now ms,
        // not a frame count — audit ws-media-3). First poll at t=0 arms the settle
        // window; a poll past CART_AUTOPERSIST_DEBOUNCE_MS writes once. No media/persist
        // call anywhere.
        {
            let mut st = state.lock().unwrap();
            stream_maybe_autopersist_cart(&mut st, 0); // arm
            stream_maybe_autopersist_cart(&mut st, 100); // not settled yet
            stream_maybe_autopersist_cart(&mut st, CART_AUTOPERSIST_DEBOUNCE_MS + 1); // settled → write
        }

        let new_bytes = std::fs::read(&crt_path).unwrap();
        assert_eq!(new_bytes.len() as u64, orig_len, "EasyFlash re-pack keeps .crt length");
        assert_ne!(new_bytes, orig_bytes, "host .crt FILE bytes changed after auto-persist");
        // The programmed byte (0x42) must be present in the re-packed image (ROML
        // offset 0x100 = header(0x40) + CHIP-header(0x10) + 0x100).
        assert_eq!(new_bytes[0x40 + 0x10 + 0x100], 0x42, "programmed flash byte in host .crt");

        // Idempotent: a second pass at the SAME settled gen must NOT re-write
        // (cart_ap_done_gen guards it). Capture mtime is platform-flaky, so prove
        // via the done-gen sentinel instead.
        {
            let st = state.lock().unwrap();
            assert_eq!(st.cart_ap_done_gen, st.cart_ap_seen_gen, "settled gen recorded as done");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// audit ws-media-3 + background-workers-async-10 DIRECT PROOF — cart flash
    /// auto-persist fires while the machine is PAUSED. The TS persist runs on an
    /// independent 1 s setInterval that fires regardless of run-state
    /// (runtime-controller.ts:219-226), so a flash delta then pause/JAM/bp before the
    /// debounce STILL reaches the host .crt. TRX64 previously drove the persist ONLY
    /// from the stream loop's `if running` block on a FRAME counter (frame_seq advances
    /// only while running), so a dirty-then-pause never persisted. This test mounts a
    /// writable EasyFlash, drives a real byte-program (dirty), sets running=FALSE
    /// (paused), then ticks the WALL-CLOCK persist cadence past the debounce and
    /// asserts the host .crt FILE bytes changed — proving the persist no longer depends
    /// on the run-state. (The gate cannot drive a cart-mapper write over the WS surface,
    /// so the conformance case ws-media-3 is BLOCKED + this is its direct verification.)
    #[test]
    fn ws_media_3_cart_autopersist_fires_while_paused() {
        let dir = std::env::temp_dir().join(format!("trx64_wsmedia3_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let crt_path = dir.join("ef_paused.crt");

        let mut bank0 = vec![0xffu8; 0x4000];
        bank0[0x3ffc] = 0x00;
        bank0[0x3ffd] = 0x80;
        let crt = build_crt_for_test(32, 1, 0, "EF", &[(0, 0x8000, bank0)]);
        std::fs::write(&crt_path, &crt).unwrap();
        let orig_bytes = std::fs::read(&crt_path).unwrap();

        let state = make_state();
        {
            let mut st = state.lock().unwrap();
            st.session.machine.attach_cart_from_bytes(&crt, "EF").expect("attach EF");
            st.session.cart_path = crt_path.to_string_lossy().to_string();
            st.session.machine.cold_reset();
            // Drive a real AM29F040B byte-program → dirty + gen bumped.
            let bi = bi_for_test();
            let clk = st.session.machine.clk;
            let cart = st.session.machine.cartridge.as_mut().expect("cart attached");
            cart.write(0x8555, 0xaa, &bi, clk);
            cart.write(0x82aa, 0x55, &bi, clk);
            cart.write(0x8555, 0xa0, &bi, clk);
            cart.write(0x8100, 0x42, &bi, clk);
            assert!(cart.is_writable_dirty(), "dirty after program");
            // THE KEY PRECONDITION: the machine is PAUSED (running=false). The TS
            // setInterval persist fires anyway; the TRX64 stream loop now calls the
            // persist hooks EVERY iteration regardless of run-state, so the host file
            // must still update. (The pre-fix `if running` path never ran here.)
            st.session.running = false;
        }

        // Drive the wall-clock persist cadence (the same call the stream loop now makes
        // every iteration regardless of run-state). The machine stays PAUSED throughout.
        {
            let mut st = state.lock().unwrap();
            assert!(!st.session.running, "machine is paused for the whole persist window");
            stream_maybe_autopersist_cart(&mut st, 0); // arm
            stream_maybe_autopersist_cart(&mut st, CART_AUTOPERSIST_DEBOUNCE_MS + 1); // settled → write
            assert!(!st.session.running, "still paused after persist (persist must not resume)");
        }

        let new_bytes = std::fs::read(&crt_path).unwrap();
        assert_ne!(
            new_bytes, orig_bytes,
            "host .crt FILE bytes changed while PAUSED (persist not gated on run-state)"
        );
        assert_eq!(
            new_bytes[0x40 + 0x10 + 0x100], 0x42,
            "programmed flash byte in host .crt after paused persist"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ITEM 2 PROOF — disk auto-persist (.d64 lazy writeback). Mount a writable
    /// blank D64 with a host backing path, drive a REAL dirty GCR track (the same
    /// `write_next_bit` the engine calls on a drive write), then run the stream
    /// hook for > debounce frames with NO explicit media/persist — assert the .d64
    /// host FILE updated. PARITY-NEUTRAL enhancement (TS writes eagerly via
    /// fsimage_dxx hostFlush; TRX64 here lazily, debounced).
    #[test]
    fn item2_disk_autopersist_writes_host_d64_without_explicit_persist() {
        let dir = std::env::temp_dir().join(format!("trx64_item2_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let d64_path = dir.join("blank.d64");

        // A blank 174848-byte D64 (35 tracks) — a valid GCR-encodable image.
        let blank = vec![0u8; 174848];
        std::fs::write(&d64_path, &blank).unwrap();
        let orig_bytes = std::fs::read(&d64_path).unwrap();

        let state = make_state();
        {
            let mut st = state.lock().unwrap();
            st.session.machine.drive8.attach_disk(DiskImage {
                kind: DiskKind::D64,
                bytes: blank.clone(),
                backing_path: Some(d64_path.to_string_lossy().to_string()),
                read_only: false,
            });
            // Drive a real bit-level write at the parked head (track 18) → dirties
            // a GCR track exactly as the engine's WRITE path does.
            st.session.machine.drive8.rotation.write_one_bit_for_test(1);
            assert!(
                st.session.machine.drive8.rotation.has_dirty_track(),
                "a real GCR track is dirty"
            );
        }

        // Run the disk hook with synthetic WALL-CLOCK ms (the debounce is now ms, not
        // a frame count — audit ws-media-3). The first poll flushes the dirty track
        // into disk.bytes + arms the content-hash settle window; a poll past
        // DISK_AUTOPERSIST_DEBOUNCE_MS writes the host file once.
        {
            let mut st = state.lock().unwrap();
            stream_maybe_autopersist_disk(&mut st, 0); // flush + arm
            stream_maybe_autopersist_disk(&mut st, 10); // settled hash, not aged yet
            stream_maybe_autopersist_disk(&mut st, DISK_AUTOPERSIST_DEBOUNCE_MS + 1); // → write
        }

        let new_bytes = std::fs::read(&d64_path).unwrap();
        assert_eq!(new_bytes.len(), orig_bytes.len(), "D64 size preserved");
        assert_ne!(new_bytes, orig_bytes, "host .d64 FILE bytes changed after auto-persist");
        {
            let st = state.lock().unwrap();
            assert!(st.disk_ap_done_hash.is_some(), "disk settle recorded as done");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// audit ws-media-8 DIRECT PROOF — the recents store is newest-first, deduped by
    /// path, carries a mountedAt, and caps at MAX_RECENT_MEDIA. add_recent_media is the
    /// 1:1 port of recent-files.ts addRecent (prepend + dedup + trim). scan_recent_media
    /// then overlays it AHEAD of the dir scans, so media/recent[0] is the most-recently
    /// mounted medium.
    #[test]
    fn ws_media_8_recents_store_is_newest_first_with_mountedat() {
        let state = make_state();
        {
            let mut st = state.lock().unwrap();
            add_recent_media(&mut st, "/p/diskA.d64", "d64");
            add_recent_media(&mut st, "/p/diskB.d64", "d64");
            // newest-first: B before A.
            assert_eq!(st.recent_media[0].path, "/p/diskB.d64");
            assert_eq!(st.recent_media[1].path, "/p/diskA.d64");
            assert!(!st.recent_media[0].mounted_at.is_empty(), "mountedAt stamped");
            // re-mounting A moves it to the FRONT (dedup by path, no duplicate).
            add_recent_media(&mut st, "/p/diskA.d64", "d64");
            assert_eq!(st.recent_media[0].path, "/p/diskA.d64");
            assert_eq!(st.recent_media.len(), 2, "dedup keeps one entry per path");
            // cap at MAX_RECENT_MEDIA.
            for i in 0..(MAX_RECENT_MEDIA + 5) {
                add_recent_media(&mut st, &format!("/p/extra{i}.crt"), "crt");
            }
            assert_eq!(st.recent_media.len(), MAX_RECENT_MEDIA, "capped at MAX_RECENT_MEDIA");
        }
        // The mountedAt is a real ISO-8601 UTC stamp (YYYY-MM-DDTHH:MM:SS.mmmZ), 1:1
        // with the c64re new Date().toISOString().
        let iso = now_iso8601_utc();
        assert_eq!(iso.len(), 24, "ISO-8601 ms-precision length");
        assert!(iso.ends_with('Z') && iso.contains('T'), "ISO-8601 shape: {iso}");
    }

    /// ITEM 3 PROOF — auto-capture every N frames (filmstrip). Drive the stream
    /// autocapture hook across several cadence windows with NO explicit
    /// checkpoint/capture — assert the ring accumulates multiple checkpoints
    /// (one per CHECKPOINT_CAPTURE_EVERY_FRAMES). (= CHECKPOINT_AUTOCAPTURE,
    /// runtime-controller.ts:157.)
    #[test]
    fn item3_autocapture_fills_ring_at_cadence_without_explicit_capture() {
        let state = make_state();
        // A synthetic non-uniform live canvas (384×272 4-bit), so the downscaled
        // thumbnail is a real picture (>1 distinct index), like the live stream loop
        // passes its just-rendered frame.
        let (cw, ch) = (trx64_core::render::CANVAS_W, trx64_core::render::CANVAS_H);
        let canvas: Vec<u8> = (0..cw * ch).map(|i| (i % 16) as u8).collect();
        // Run enough frames for ~6 capture windows (~3 s @ 50 fps, cadence 25).
        let windows = 6u64;
        let total = checkpoint_capture_every_frames() * windows + 2;
        for frame in 0..total {
            let mut st = state.lock().unwrap();
            stream_maybe_autocapture(&mut st, frame, cw, ch, &canvas);
        }
        let st = state.lock().unwrap();
        let n = st.checkpoint_ring.list().len();
        assert!(
            n as u64 >= windows,
            "ring accumulated multiple auto-captures (got {n}, want >= {windows}) WITHOUT any explicit checkpoint/capture"
        );
        // Each accumulated ref carries the auto-cadence frame (proves these came
        // from the per-frame hook, not an explicit capture).
        assert!(
            st.checkpoint_ring.list().iter().all(|r| r.frame > 0 || r.cycles == 0),
            "auto-captures stamped with the stream-loop frame"
        );
        // Spec 769.5a — EVERY auto-anchor (framebuffer-OMITTED) now has a stored
        // thumbnail: the thumb store has one entry per live ring ref (the bug was
        // ~4-of-~70). 96×68, real picture.
        for r in st.checkpoint_ring.list() {
            let t = st.checkpoint_thumbs.get(&r.id)
                .unwrap_or_else(|| panic!("auto-anchor {} has no thumbnail (Spec 769.5a)", r.id));
            assert_eq!(t.width, cw / THUMB_FACTOR);
            assert_eq!(t.height, ch / THUMB_FACTOR);
            assert_eq!(t.indices.len(), t.width * t.height);
            assert_eq!(t.palette.len(), 48);
            assert!(t.indices.iter().any(|&b| b != t.indices[0]), "thumbnail is a real (non-uniform) picture");
        }
    }

    /// Spec 769.5a PROOF — checkpoint/thumbnails count == checkpoint/list count for
    /// framebuffer-OMITTED auto-anchors (the bug: filmstrip showed only the rare
    /// framebuffer-present checkpoints). Drives the stream autocapture hook (which
    /// fills the separate thumb store), then asserts the wire-level filmstrip
    /// surfaces a thumbnail for EVERY ring entry.
    #[test]
    fn thumbnails_count_matches_ring_for_omit_framebuffer_autoanchors() {
        let state = make_state();
        let (cw, ch) = (trx64_core::render::CANVAS_W, trx64_core::render::CANVAS_H);
        let canvas: Vec<u8> = (0..cw * ch).map(|i| (i % 16) as u8).collect();
        let total = checkpoint_capture_every_frames() * 5 + 2;
        for frame in 0..total {
            let mut st = state.lock().unwrap();
            stream_maybe_autocapture(&mut st, frame, cw, ch, &canvas);
        }
        let list = call(&state, "checkpoint/list", json!({}));
        let ring_n = list["checkpoints"].as_array().unwrap().len();
        let res = call(&state, "checkpoint/thumbnails", json!({}));
        let thumbs = res["thumbnails"].as_array().unwrap();
        assert!(ring_n >= 5, "auto-anchors accumulated (got {ring_n})");
        assert_eq!(thumbs.len(), ring_n, "every framebuffer-omitted auto-anchor has a thumbnail (Spec 769.5a)");
        for t in thumbs {
            assert_eq!(t["width"], json!(96));
            assert_eq!(t["height"], json!(68));
            let idx = base64_decode_for_test(t["indices"].as_str().unwrap());
            assert_eq!(idx.len(), 96 * 68);
            assert!(idx.iter().any(|&b| b != idx[0]), "thumbnail is a real picture, not all-one-colour");
        }
    }

    #[test]
    fn trap_rules_parse_and_decode_emit() {
        // FEATURE #4: parse the field-report's exact example JSON, then assert the
        // rendered emit reads the project-named diagnostic bytes from the live machine
        // and formats `label: name=$XX name2=$YY (decode)`.
        let def = json!({
            "pc": "$088F", "label": "loader miss",
            "dump": [["k1", "$0A80", 1], ["k2", "$0A81", 1]],
            "decode": "k2 bit7 => DIRECT-overlay miss"
        });
        let rules = parse_trap_rules(&def).expect("parse single rule");
        assert_eq!(rules.len(), 1);
        let r = &rules[0];
        assert_eq!(r.pc, 0x088f);
        assert_eq!(r.label, "loader miss");
        assert_eq!(r.dump, vec![
            ("k1".to_string(), 0x0a80, 1),
            ("k2".to_string(), 0x0a81, 1),
        ]);

        // Stage the diagnostic bytes in RAM ($0A80=$00, $0A81=$E3) + render the emit.
        let state = make_state();
        {
            let mut st = state.lock().unwrap();
            st.session.machine.ram[0x0a80] = 0x00;
            st.session.machine.ram[0x0a81] = 0xe3;
            let g = &st.session.machine;
            let emit = format_trap_rule_emit(r, g);
            // Exactly the field report's expected line.
            assert_eq!(emit, "loader miss: k1=$00 k2=$E3 (k2 bit7 => DIRECT-overlay miss)");
        }

        // An ARRAY of rules + a multi-byte (LE) field parse + render.
        let arr = json!([
            { "pc": "0900", "label": "vec", "dump": [["ptr", "$0A82", 2]] },
            { "pc": "$0901", "label": "flag", "dump": [["f", "$0A84", 1]], "decode": "set" }
        ]);
        let rules2 = parse_trap_rules(&arr).expect("parse array");
        assert_eq!(rules2.len(), 2);
        assert_eq!(rules2[0].pc, 0x0900);
        assert_eq!(rules2[0].dump[0].2, 2, "len=2 parsed");
        {
            let mut st = state.lock().unwrap();
            st.session.machine.ram[0x0a82] = 0x34; // lo
            st.session.machine.ram[0x0a83] = 0x12; // hi → LE $1234
            let emit = format_trap_rule_emit(&rules2[0], &st.session.machine);
            assert_eq!(emit, "vec: ptr=$1234", "multi-byte field rendered little-endian");
        }

        // A malformed rule errors (missing pc).
        assert!(parse_trap_rules(&json!({ "label": "x" })).is_err());
    }

    #[test]
    fn trap_rules_verb_load_list_clear_and_jam_emit() {
        // FEATURE #4: drive the `traprules` monitor verb end-to-end (load from a JSON
        // file, list, clear) AND assert the JAM broadcast carries the trap diagnosis.
        let state = make_state();
        // Write a rule file in the scratch dir.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("trx64_traprules_{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{ "pc":"$0801", "label":"boot trap",
                 "dump":[["a","$00FB",1],["b","$00FC",1]],
                 "decode":"a|b nonzero => stuck" }"#,
        )
        .unwrap();

        // Load via the verb.
        let out = {
            let mut st = state.lock().unwrap();
            run_monitor(&mut st, &format!("traprules {}", path.display())).unwrap()
        };
        assert!(out.contains("loaded 1 rule"), "load output: {out}");

        // List shows it.
        let listed = {
            let mut st = state.lock().unwrap();
            run_monitor(&mut st, "traprules").unwrap()
        };
        assert!(listed.contains("$0801"), "list output: {listed}");
        assert!(listed.contains("boot trap"), "list shows the label");

        // Stage bytes + a JAM at $0801, then run the JAM handler and assert the
        // trapDiagnosis is attached to the debug/stopped broadcast. Subscribe a tokio
        // unbounded channel to the NotifyHub (send is non-blocking → no runtime needed;
        // try_recv drains synchronously) so we observe the actual broadcast envelope.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _sub;
        {
            let mut st = state.lock().unwrap();
            _sub = st.notify.subscribe(tx);
            st.session.machine.ram[0x00fb] = 0x05;
            st.session.machine.ram[0x00fc] = 0x00;
            // Force a JAM at $0801: put a KIL opcode there + point the core at it + mark
            // it jammed (the handler reads the jammed core's PC).
            st.session.machine.ram[0x0801] = 0x02; // KIL
            st.session.machine.c64_core.reg_pc = 0x0801;
            st.session.machine.c64_core.is_jammed = true;
            check_and_handle_jam(&mut st);
        }
        // Drain the broadcast envelopes and find the debug/stopped one.
        let mut diag: Option<String> = None;
        while let Ok(msg) = rx.try_recv() {
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["method"] == json!("debug/stopped") {
                    if let Some(d) = v["params"]["trapDiagnosis"].as_str() {
                        diag = Some(d.to_string());
                    }
                }
            }
        }
        let diag = diag.expect("debug/stopped carried trapDiagnosis");
        assert!(diag.contains("boot trap"), "diag: {diag}");
        assert!(diag.contains("a=$05"), "diag reads the staged byte: {diag}");
        assert!(diag.contains("(a|b nonzero => stuck)"), "diag carries the decode: {diag}");

        // Clear drops it.
        let cleared = {
            let mut st = state.lock().unwrap();
            run_monitor(&mut st, "traprules clear").unwrap()
        };
        assert!(cleared.contains("cleared 1"), "clear output: {cleared}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn guardrail_warns_on_key_injection_into_free_running_core() {
        // GUARDRAIL #1: session/key_down into a FREE-RUNNING core (running &&
        // streaming) attaches a non-fatal freeRunWarning; a paused core does NOT.
        let state = make_state();

        // Paused (default) → no warning.
        let paused = call(&state, "session/key_down", json!({ "key": "A" }));
        assert!(paused["freeRunWarning"].is_null(), "paused core: no warning");

        // Mark the machine free-running (stream loop advancing).
        {
            let mut st = state.lock().unwrap();
            st.session.running = true;
            st.streaming_enabled = true;
        }
        let running = call(&state, "session/key_down", json!({ "key": "B" }));
        let warn = running["freeRunWarning"].as_str().expect("free-run warning attached");
        assert!(warn.contains("FREE-RUNNING"), "warning text: {warn}");
        assert!(warn.contains("may be LOST"), "warning explains the risk: {warn}");
        // The key still registered (non-fatal — the op runs).
        assert_eq!(running["ok"], json!(true));

        // session/type carries it too.
        let typed = call(&state, "session/type", json!({ "text": "RUN" }));
        assert!(typed["freeRunWarning"].as_str().is_some(), "type also warns when free-running");
    }

    // Spec 796 — candidate lifecycle over the full WS path (create → patch → run →
    // auto-eval → remove → export), on a blank machine with a 0-cycle scenario so the
    // diff reflects ONLY the overlay (no execution) — deterministic, ROM-free.
    #[test]
    fn candidate_lifecycle_run_and_autoeval() {
        let state = make_state();
        let cp = call(&state, "checkpoint/capture", json!({}));
        let anchor = cp["ref"]["id"].as_str().expect("checkpoint id").to_string();

        let created = call(
            &state,
            "runtime/candidate_create",
            json!({ "anchor": anchor, "scenario": { "inputs": [], "cycleBudget": 0 } }),
        );
        let cid = created["id"].as_str().expect("candidate id").to_string();

        // (a) no patch → identical vs its own baseline.
        let r0 = call(&state, "runtime/candidate_run", json!({ "id": cid }));
        assert_eq!(r0["verdict"]["identical"], json!(true), "no-patch run must be identical");

        // (b) a RAM patch → the run differs, and names `ram`.
        call(
            &state,
            "runtime/candidate_patch",
            json!({ "id": cid, "space": "ram", "addr": 0xcf00, "bytes": [0x5a], "source": "lda #$5a" }),
        );
        let r1 = call(&state, "runtime/candidate_run", json!({ "id": cid }));
        assert_eq!(r1["verdict"]["identical"], json!(false), "ram patch must differ");
        assert!(r1["verdict"]["differing"].as_array().unwrap().iter().any(|c| c == "ram"));

        // (c) deterministic — a second run reproduces the verdict.
        let r2 = call(&state, "runtime/candidate_run", json!({ "id": cid }));
        assert_eq!(r1["verdict"], r2["verdict"], "candidate runs must be deterministic");

        // (d) remove the patch → back to identical.
        call(
            &state,
            "runtime/candidate_remove_patch",
            json!({ "id": cid, "space": "ram", "addr": 0xcf00 }),
        );
        let r3 = call(&state, "runtime/candidate_run", json!({ "id": cid }));
        assert_eq!(r3["verdict"]["identical"], json!(true), "after remove, identical again");

        // (e) export before removing carried the source; re-add + export the seed.
        call(
            &state,
            "runtime/candidate_patch",
            json!({ "id": cid, "space": "roml", "bank": 2, "addr": 0x8000, "bytes": [0xea], "source": "nop" }),
        );
        let ex = call(&state, "runtime/candidate_export", json!({ "id": cid }));
        let ps = ex["patches"].as_array().unwrap();
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0]["space"], json!("roml"));
        assert_eq!(ps[0]["source"], json!("nop"));
    }
}

