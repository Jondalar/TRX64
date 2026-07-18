//! streaming.rs — live A/V binary push for `ws-av-tap.mjs` (ADR-073).
//!
//! The RuntimeController parity gap: the Node daemon's `ws-server.ts` pushes, per
//! emulated PAL frame, two BINARY WebSocket messages to every connected client —
//! a VIC video frame (`BIN_VIC = 0x01`, palette-indexed) and a reSID audio chunk
//! (`BIN_AUDIO = 0x02`, s16le stereo 44100 Hz). The TRX64 daemon had neither. This
//! module ports that push 1:1 so the user's read-only tap (which sends NO commands)
//! can decode the stream → ffmpeg → .mp4.
//!
//! WIRE FORMAT (matched byte-for-byte against ws-av-tap.mjs's decoder AND c64re's
//! `ws-server.ts` encoder):
//!
//!   Outer envelope (both message types): `[type:u8][seq:u32 LE]` then payload.
//!     The consumer does `data.subarray(5)` — exactly a 5-byte header.
//!
//!   BIN_VIC = 0x01 payload (fmt 1, palette-indexed):
//!     [w:u16 LE=384][h:u16 LE=272][fmt:u8=1][rsvd:u8=0][cycle:u32 LE]
//!     [48 B palette = 16×(R,G,B)][w*h index bytes, each & 0x0f]
//!     (palette at offset 10, indices at offset 58 — matches the tap's palOff/idxOff).
//!     `cycle` = the C64 CPU cycle counter (LE u32), NOT the frame number.
//!
//!   BIN_AUDIO = 0x02 payload: raw s16le STEREO PCM at 44100 Hz. reSID is MONO →
//!     each sample is duplicated into both channels (L,R) inline, little-endian.
//!
//! TRIGGER: c64re relies on the browser sending `debug/run` + `audio/start`; the
//! tap sends nothing. So THIS daemon auto-starts the stream when the first client
//! connects (the daemon IS the producer). The streaming loop owns the machine for
//! its lifetime and paces to real-time (~50 fps PAL).
//!
//! THREADING: `SidAudioEngine` holds the process-wide reSID `MutexGuard` and is
//! therefore `!Send`, so the streaming loop runs on a dedicated OS thread (not a
//! tokio task) that owns the engine locally. The SID `write_trace` hook captures
//! only a `Send` `(addr,value)` byte buffer; the loop drains it per frame into the
//! engine (verbatim the `scramble_av_record.rs` harness pattern). Built binary
//! messages are handed to the async WS writer via a tokio mpsc channel.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::Message;

use trx64_core::render::COLODORE;
use trx64_core::resid_audio::SidAudioEngine;
use trx64_core::resid_ffi::ResidConfig;

use crate::SharedState;

/// Binary message type codes (1:1 with c64re `ws-server.ts` + ws-av-tap.mjs).
pub const BIN_VIC: u8 = 0x01;
pub const BIN_AUDIO: u8 = 0x02;

/// fmt 1 = palette-indexed VIC frame (the only format the tap decodes).
const VIC_FMT_INDEXED: u8 = 1;

/// PAL Φ2 cycles per frame (312 rasterlines × 63 cycles = 19656). Canonical PAL
/// frame length used by the project's av-record harness; one frame per push.
/// Public so the `checkpoint/restore` render-flag path (main.rs) can re-sim exactly
/// one PAL frame to regenerate a framebuffer-omitted anchor's picture.
pub const CYC_PER_FRAME: u64 = 19656;
/// PAL Φ2 clock (Hz) — used to derive the real-time frame period from the cycle
/// budget, so the pace stays self-consistent with the cycles actually run.
const PAL_CYCLES_PER_SEC: f64 = 985_248.0;
/// Real-time wall period of one emulated frame (≈ 19.95 ms → ~50.12 fps PAL).
fn frame_period() -> Duration {
    Duration::from_secs_f64(CYC_PER_FRAME as f64 / PAL_CYCLES_PER_SEC)
}

/// Build a BIN_VIC (0x01) binary WS message from the machine's current displayed
/// framebuffer. `seq` = the monotonic frame counter; `cycle` = the C64 CPU cycle
/// count (LE u32, truncated) — matching c64re's `sess.c64Cpu.cycles >>> 0`.
pub fn build_vic_frame(seq: u32, cycle: u32, indices: &[u8], w: u16, h: u16) -> Vec<u8> {
    // envelope(5) + header(10) + palette(48) + indices(w*h)
    let mut buf = Vec::with_capacity(5 + 10 + 48 + indices.len());
    // ── outer envelope: [type:u8][seq:u32 LE] ──
    buf.push(BIN_VIC);
    buf.extend_from_slice(&seq.to_le_bytes());
    // ── payload header: [w:u16][h:u16][fmt:u8][rsvd:u8][cycle:u32], all LE ──
    buf.extend_from_slice(&w.to_le_bytes());
    buf.extend_from_slice(&h.to_le_bytes());
    buf.push(VIC_FMT_INDEXED); // fmt = 1
    buf.push(0); // rsvd = 0
    buf.extend_from_slice(&cycle.to_le_bytes());
    // ── 48-byte palette: 16 × (R,G,B) in COLODORE index order ──
    for rgb in COLODORE.iter() {
        buf.extend_from_slice(rgb);
    }
    // ── w*h colour indices (each already & 0x0f from render_canvas_indices) ──
    buf.extend_from_slice(indices);
    buf
}

/// Build a BIN_AUDIO (0x02) binary WS message from MONO reSID PCM. reSID is mono;
/// each sample is duplicated into both stereo channels (L,R), little-endian — so
/// the payload is interleaved s16le stereo, exactly what the tap pipes to ffmpeg
/// as `-f s16le -ac 2`.
pub fn build_audio_msg(seq: u32, mono: &[i16]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + mono.len() * 4);
    buf.push(BIN_AUDIO);
    buf.extend_from_slice(&seq.to_le_bytes());
    for &s in mono {
        let le = s.to_le_bytes();
        buf.extend_from_slice(&le); // L
        buf.extend_from_slice(&le); // R
    }
    buf
}

/// A registered subscriber: one connected client's outbound channel + a unique id.
struct Subscriber {
    id: u64,
    out: UnboundedSender<Message>,
}

// ── Generic JSON-notification push (ws-server.ts:258 `broadcast`) ──────────────
//
// `dispatch(req, &state)` is pure request→response and has NO access to a client's
// outbound channel, so a request handler cannot, on its own, also PUSH a server
// notification (c64re's `this.broadcast(method, params)`). c64re emits three such
// notifications from inside handlers — debug/breakpoint_hit (a run halts at a bp),
// audio/flush (the PCM timeline is discontinuous → flush the worklet ring), and
// batch/progress (per-scenario during a batch run) — by calling `this.broadcast`,
// which fans a JSON-RPC NOTIFICATION (no `id`) out to ALL connected clients.
//
// The `NotifyHub` is the TRX64 mirror of that fan-out. UNLIKE [`StreamHub`] (gated
// on `--stream`), it ALWAYS exists: it lives in the shared `State` so any handler
// can reach it, and every connection registers its `out_tx` (the same mpsc→writer
// channel that already carries responses + the BIN A/V frames, ADR-073). The
// notification rides that channel as a `Message::Text` carrying the c64re envelope
// `{ "jsonrpc": "2.0", "method": <method>, "params": <payload> }` — byte-identical
// to ws-server.ts:258-260, and with NO `id` field so a client distinguishes a
// server-push from a request reply exactly as in c64re.

/// The generic notification broadcaster — 1:1 with ws-server.ts's `broadcast`.
/// Holds every live client's outbound channel; `broadcast(method, payload)` fans a
/// JSON-RPC notification to all of them, pruning any whose channel has closed.
#[derive(Default)]
pub struct NotifyHub {
    inner: Mutex<NotifyInner>,
}

#[derive(Default)]
struct NotifyInner {
    subscribers: Vec<Subscriber>,
    next_id: u64,
}

impl NotifyHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a client's outbound channel. Returns a guard that unsubscribes on
    /// drop (mirrors the per-connection `clients` set add/remove in ws-server.ts).
    pub fn subscribe(self: &Arc<Self>, out: UnboundedSender<Message>) -> NotifySub {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.subscribers.push(Subscriber { id, out });
        NotifySub { hub: Arc::clone(self), id }
    }

    /// Send a JSON-RPC notification (no `id`) to every live client — the exact
    /// envelope ws-server.ts:259 builds: `{ jsonrpc:"2.0", method, params }`.
    /// Prunes any subscriber whose channel has closed. Returns the live count.
    pub fn broadcast(&self, method: &str, payload: serde_json::Value) -> usize {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": payload,
        });
        // Serialize once; clone the cheap `Message` per subscriber.
        let text = serde_json::to_string(&msg).unwrap_or_else(|_| String::from("{}"));
        let wire = Message::Text(text.into());
        let mut inner = self.inner.lock().unwrap();
        inner
            .subscribers
            .retain(|s| s.out.send(wire.clone()).is_ok());
        inner.subscribers.len()
    }

    fn unsubscribe(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.subscribers.retain(|s| s.id != id);
    }
}

/// Per-connection notification subscription guard. Dropping it unsubscribes.
pub struct NotifySub {
    hub: Arc<NotifyHub>,
    id: u64,
}

impl Drop for NotifySub {
    fn drop(&mut self) {
        self.hub.unsubscribe(self.id);
    }
}

/// The SINGLETON broadcaster. There is exactly ONE streaming loop driving the
/// (singleton) machine, regardless of how many clients connect — matching c64re's
/// model (one pacing loop, `broadcastFrame`/`broadcastAudioWire` to all clients).
/// Each connection [`subscribe`](StreamHub::subscribe)s its outbound channel; the
/// loop pushes every BIN_VIC/BIN_AUDIO to all live subscribers, pruning closed
/// ones. The loop starts lazily on the FIRST subscriber and STOPS (releasing the
/// machine + clearing the SID hook) when the LAST one leaves.
pub struct StreamHub {
    inner: Mutex<HubInner>,
    state: SharedState,
}

struct HubInner {
    subscribers: Vec<Subscriber>,
    next_id: u64,
    /// The running loop's stop flag (Some while a loop thread is alive).
    stop: Option<Arc<AtomicBool>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl StreamHub {
    pub fn new(state: SharedState) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HubInner {
                subscribers: Vec::new(),
                next_id: 1,
                stop: None,
                join: None,
            }),
            state,
        })
    }

    /// Register a client's outbound channel. Starts the loop if it's the first
    /// subscriber. Returns a [`StreamSub`] guard that unsubscribes on drop (and
    /// stops the loop when the last client leaves).
    pub fn subscribe(self: &Arc<Self>, out: UnboundedSender<Message>) -> StreamSub {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.subscribers.push(Subscriber { id, out });
        // First subscriber → start the loop.
        if inner.stop.is_none() {
            let stop = Arc::new(AtomicBool::new(false));
            let hub = Arc::clone(self);
            let stop_thread = Arc::clone(&stop);
            let join = std::thread::Builder::new()
                .name("trx64-av-stream".into())
                .spawn(move || stream_loop(hub, stop_thread))
                .expect("spawn av-stream thread");
            inner.stop = Some(stop);
            inner.join = Some(join);
        }
        StreamSub { hub: Arc::clone(self), id }
    }

    /// Broadcast a binary message to all live subscribers; prune any whose channel
    /// has closed (client gone). Returns the number of live subscribers after pruning.
    fn broadcast(&self, msg: Message) -> usize {
        let mut inner = self.inner.lock().unwrap();
        inner.subscribers.retain(|s| s.out.send(msg.clone()).is_ok());
        inner.subscribers.len()
    }

    fn unsubscribe(&self, id: u64) {
        // Take the loop thread to join OUTSIDE the lock (the loop also locks `inner`
        // via broadcast → joining under the lock would deadlock).
        let to_join = {
            let mut inner = self.inner.lock().unwrap();
            inner.subscribers.retain(|s| s.id != id);
            if inner.subscribers.is_empty() {
                if let Some(stop) = inner.stop.take() {
                    stop.store(true, Ordering::SeqCst);
                }
                inner.join.take()
            } else {
                None
            }
        };
        if let Some(j) = to_join {
            let _ = j.join();
        }
    }
}

/// Per-connection subscription guard. Dropping it unsubscribes; the last drop
/// stops the singleton loop.
pub struct StreamSub {
    hub: Arc<StreamHub>,
    id: u64,
}

impl Drop for StreamSub {
    fn drop(&mut self) {
        self.hub.unsubscribe(self.id);
    }
}

/// The streaming loop body. Runs on a dedicated OS thread (owns the reSID engine).
/// Pushes each frame's BIN_VIC + BIN_AUDIO to ALL of the hub's subscribers.
fn stream_loop(hub: Arc<StreamHub>, stop: Arc<AtomicBool>) {
    let state = hub.state.clone();
    // ── reSID audio engine (owned on this thread; !Send) + the Send write hook ──
    let mut engine = SidAudioEngine::new(ResidConfig::default());
    let writes: Arc<Mutex<Vec<(u8, u8)>>> = Arc::new(Mutex::new(Vec::new()));
    // Track the machine-rebuild generation (bumped by do_power_on / do_power_off).
    // Spec 786 rebuilds the machine (Machine::new() → a fresh SID with NO write-trace
    // hook) on every power-cycle, so the hook + reSID prime below is RE-RUN in the loop
    // whenever this changes — else audio goes permanently silent after the first
    // power-cycle (e.g. an EF CRT insert = off→on). Seeded at the initial install.
    let mut last_machine_generation = {
        // Install the additive SID write-trace hook so EVERY $D4xx write (in CPU
        // order) is captured for reSID. The hook must be Send → it captures only
        // the Arc<Mutex<Vec>> byte buffer (the engine itself stays on this thread).
        let w = Arc::clone(&writes);
        let mut st = state.lock().unwrap();
        st.session.machine.sid.set_write_trace(Some(Box::new(move |addr, value| {
            w.lock().unwrap().push((addr, value));
        })));
        // Prime reSID with the CURRENT SID register file so the stream starts from
        // the live state (frequencies/PW/control already set), not power-on silence.
        for reg in 0u8..=0x18 {
            let v = st.session.machine.read_full(0xD400 + reg as u16);
            engine.record_write(reg, v);
        }
        st.machine_generation
    };
    engine.record_boundary(0); // apply the priming writes, emit nothing
    engine.flush();
    let _ = engine.take_pcm(); // discard priming silence

    let period = frame_period();
    let mut frame_seq: u32 = 0;
    let mut audio_seq: u32 = 0;
    // Epoch-anchored pacing: schedule each frame at an absolute target time so the
    // pace doesn't drift (matches c64re's accumulated-target sleep). If we fall
    // far behind (>100 ms), re-base the epoch instead of spinning to catch up.
    let mut epoch = Instant::now();
    let mut frames_since_epoch: u32 = 0;
    // audit ws-media-3 / background-workers-async-10 — a WALL-CLOCK reference for the
    // media auto-persist cadence. The TS persist runs on an independent 1 s setInterval
    // with a Date.now() debounce that fires regardless of run-state, so a flash/disk
    // delta then pause/JAM/bp STILL reaches the host file. TRX64's frame counter only
    // advances while running, so the persist hooks now debounce on this monotonic
    // wall-clock instead — and run EVERY loop iteration, not only `if running`.
    let persist_epoch = Instant::now();

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }

        // Spec 771.2 — gate the machine ADVANCE + A/V push on the controller's
        // run-state. When paused/powered-off (running=false) the TS controller's tick
        // stops: the picture freezes on the last presented frame and audio goes silent.
        // TRX64's stream loop is the SOLE machine driver under --stream, so it MUST
        // honor `running` too — otherwise pause never freezes, power-off shows live
        // garbage, and a reset glitches under the continuous run.
        // Spec 771.2 (T1.3 wire) — honor the controller pacing mode: "warp" advances
        // 8× cycles per presented frame (fast-forward at 50fps video), else real-time.
        let (running, warp) = {
            let mut st = state.lock().unwrap();
            // Spec 786 audio fix — a power-cycle (cold reset / cart insert-eject /
            // /power) rebuilds the machine into a fresh SID with no write-trace hook.
            // Detect via machine_generation and RE-ATTACH the capture hook + re-prime
            // reSID from the new SID register file, so audio survives the rebuild
            // instead of going permanently silent.
            if st.machine_generation != last_machine_generation {
                last_machine_generation = st.machine_generation;
                let w = Arc::clone(&writes);
                st.session.machine.sid.set_write_trace(Some(Box::new(move |addr, value| {
                    w.lock().unwrap().push((addr, value));
                })));
                for reg in 0u8..=0x18 {
                    let v = st.session.machine.read_full(0xD400 + reg as u16);
                    engine.record_write(reg, v);
                }
            }
            (st.session.running, st.pacing_mode == "warp")
        };

        // ── MEDIA AUTO-PERSIST (audit ws-media-3 / background-workers-async-10) ──
        // Run the cart + disk auto-persist hooks EVERY iteration, REGARDLESS of
        // run-state, on the wall-clock cadence — 1:1 with the TS independent
        // setInterval (runtime-controller.ts:219-226) that fires while paused/jammed/
        // bp-stopped. A flash/disk delta written just before a pause must STILL reach
        // the host file after the debounce. The loop sleeps one frame per iteration
        // even when paused, so this polls at ~50 Hz; the gen/hash checks are cheap and
        // the actual host write is debounced (wall-clock ms), so this is a no-op for a
        // clean medium. (Previously these ran inside `if running` on a frame counter,
        // so a dirty-then-pause never persisted.)
        {
            let now_ms = persist_epoch.elapsed().as_millis() as u64;
            let mut st = state.lock().unwrap();
            crate::stream_maybe_autopersist_cart(&mut st, now_ms);
            crate::stream_maybe_autopersist_disk(&mut st, now_ms);
        }

        // ── Spec 767 (live-view) — control-owner idle release ──────────────────────
        // Revert control_owner "llm"→"human" once the LLM goes idle (no operating
        // command for the idle window), so the green border returns to human-owned when
        // the LLM stops driving. NOTE: not gated on `running` — under --stream the human
        // session free-runs (running=true) the whole time, so a `!running` gate would
        // never release. While the LLM drives its OWN bounded free-run, that path keeps
        // `last_llm_activity` fresh each frame (Slice 2), so this only fires on real idle.
        // Runs every loop iteration regardless of run-state (like the media-persist block).
        {
            let mut st = state.lock().unwrap();
            if st.control_owner == "llm" {
                if let Some(t) = st.last_llm_activity {
                    if t.elapsed() >= Duration::from_millis(3000) {
                        crate::set_control_owner(&mut st, "human");
                        st.last_llm_activity = None;
                        // Spec 767 — return the shared machine to the human's default: if the
                        // LLM left it paused by a capped run (a "budget" stop — NOT a human
                        // pause / breakpoint / jam), resume the free-run so the UI KEEPS
                        // RUNNING. The pump advances + streams again on the next iteration.
                        let capped_pause = !st.session.running
                            && st.ctrl_stop.as_ref().map(|s| s.reason) == Some("budget");
                        if capped_pause {
                            st.session.running = true;
                            st.ctrl_stop = None;
                            let sid = st.session.id.clone();
                            let pacing = serde_json::json!({ "mode": st.pacing_mode, "ratio": st.pacing_ratio });
                            st.notify.broadcast(
                                "debug/running",
                                serde_json::json!({ "session_id": sid, "pacing": pacing }),
                            );
                        }
                    }
                }
            }
        }

        // ── Spec 767 (insert-settle) — resume a briefly-held machine ────────────────
        // After a power-on with the A/V hub, do_power_on holds the machine paused (+ sets
        // resume_at + force_present_frame) so THIS pump first re-hooks the fresh SID (the
        // machine_generation check above) and presents the post-boot frame while paused.
        // Resume it once the settle window elapses, so the visible boot starts with the
        // framebuffer + audio already up (no lost first ~500ms). Takes effect next iteration
        // (the `running` local was read at the top), which is fine (~one frame).
        {
            let mut st = state.lock().unwrap();
            if let Some(at) = st.resume_at {
                if Instant::now() >= at {
                    st.resume_at = None;
                    if !st.session.running {
                        st.session.running = true;
                        let sid = st.session.id.clone();
                        let pacing =
                            serde_json::json!({ "mode": st.pacing_mode, "ratio": st.pacing_ratio });
                        st.notify.broadcast(
                            "debug/running",
                            serde_json::json!({ "session_id": sid, "pacing": pacing }),
                        );
                    }
                }
            }
        }

        if running {
        // ── Run one PAL frame + render + drain this frame's SID writes ──
        // Lock the shared State only for this window; release before sleeping so
        // other JSON-RPC requests (and disk mounts) interleave between frames.
        let (vic_msg, audio_msg) = {
            let mut st = state.lock().unwrap();

            // BREAKPOINT / OBSERVER / JAM-aware per-frame advance (audit
            // ws-session-debug-0). The stream loop is the SOLE machine driver under
            // --stream, so the free-run advance must gate breakpoints/observers/JAM
            // every frame exactly like the TS controller tick (runtime-controller.ts:
            // 670-806) — a bare `run_for_full` never halts, so a bp set on the live
            // machine never stopped it. `stream_debug_gated_advance` does the gated
            // advance and, on a halt, sets `running=false` (freezes the picture) +
            // server-PUSHes debug/breakpoint_hit|observer_hit + debug/stopped, then
            // returns the cycles ACTUALLY advanced (a halt may stop mid-frame, so
            // audio runs over exactly that window). When no bp/observer is armed it
            // is the historical plain advance (byte-identical).
            let budget = if warp { CYC_PER_FRAME * 8 } else { CYC_PER_FRAME };
            let d_cycles = crate::stream_debug_gated_advance(&mut st, budget);
            // Spec 767 slice 2 (live-view) — a capped LLM run auto-pauses here once the cap
            // clk is reached (this just-rendered frame is the last one streamed), and keeps
            // the owner-idle timer fresh while it runs. No-op when no cap is set.
            crate::maybe_autopause_capped_run(&mut st);
            let cpu_cycle = st.session.machine.c64_core.clk as u32;

            // Audio: drain this frame's writes (CPU order) into the engine, close
            // the frame boundary, flush → reSID PCM for exactly this window.
            {
                let mut pending = writes.lock().unwrap();
                for &(addr, value) in pending.iter() {
                    engine.record_write(addr, value);
                }
                pending.clear();
            }
            engine.record_boundary(d_cycles);
            engine.flush();
            let mono = engine.take_pcm();

            // Video: crop the per-cycle displayed buffer → 384×272 4-bit indices.
            let (w, h, indices) = st.session.machine.render_canvas_indices();

            // ── BACKGROUND-LOOP layer (the c64re RuntimeController per-frame
            // behaviors with no WS method — runtime-controller.ts). The stream loop
            // is the SOLE per-frame driver under --stream, so it hosts them here, in
            // the per-frame lock window. The gen/hash checks are CHEAP; only the
            // actual capture costs, and that is throttled by FRAME COUNT (`frame_seq`,
            // which advances only while running). A hook panic must never kill the
            // stream — but the helpers are total (Err is swallowed inside), so no
            // extra catch is needed.
            //   ITEM 3 — auto-capture every N frames (filmstrip), Spec 705.B.
            //   ITEM 4 — recorder auto-feed (below).
            // The cart/disk auto-persist (ITEMs 1+2) now run ABOVE this block, every
            // loop iteration regardless of run-state (audit ws-media-3 — a dirty-then-
            // pause must still persist), so they are NOT repeated here.
            // Skip the autocapture during WARP (8× fast-forward would flood the ring
            // and isn't the live-scrub use case).
            let frame_no = frame_seq as u64;
            if !warp {
                // Spec 769.5a — pass the JUST-RENDERED live canvas so the auto-capture
                // can store a downscaled thumbnail keyed by the new checkpoint id
                // (the ring anchor itself stays framebuffer-omitted, BUG-049).
                crate::stream_maybe_autocapture(&mut st, frame_no, w, h, &indices);
            }
            //   ITEM 4 — recorder auto-feed (audit background-workers-async-0 +
            //   ws-checkpoint-scrub-7). The c64re tick() feeds the active recorder one
            //   omitMedia anchor at the SAME per-second cadence as the ring auto-capture
            //   (runtime-controller.ts:846-852), so a free-running machine grows recorder
            //   anchors over time. Runs in BOTH warp + PAL (unlike the warp-skipped ring
            //   auto-capture) — the recorder is not the live-scrub filmstrip; it is the
            //   off-thread scrub history, fed regardless of pace. No-op unless a recorder
            //   is active (recorder/start).
            crate::stream_maybe_feed_recorder(&mut st, frame_no);

            // audit streaming-av-5 — push the lightweight JSON `session/frame_available`
            // notification on every PRESENTED frame, alongside the binary VIC frame, for
            // metadata-only consumers (= TS runtime-controller.ts maybePresentFrame →
            // broadcast("session/frame_available", {session_id, frame, c64Cycles}), 1:1
            // per presented frame since PAL_PRESENT_DIVISOR=1). c64Cycles is the full
            // master clock (TS `c64Cpu.cycles`), NOT the truncated u32 in the binary frame.
            st.notify.broadcast(
                "session/frame_available",
                serde_json::json!({
                    "session_id": st.session.id,
                    "frame": frame_seq as u64,
                    "c64Cycles": st.session.machine.clk,
                }),
            );

            // Drop the lock before building the (larger) wire buffers + sleeping.
            drop(st);

            let vic = build_vic_frame(frame_seq, cpu_cycle, &indices, w as u16, h as u16);
            // Warp: skip audio (8× PCM would garble; TS mutes in warp too).
            let audio = if warp || mono.is_empty() {
                None
            } else {
                Some(build_audio_msg(audio_seq, &mono))
            };
            (vic, audio)
        };

        // ── Broadcast BIN_VIC then BIN_AUDIO to all live subscribers. When the
        // last client leaves, `unsubscribe` flips `stop` and we exit at the top. ──
        hub.broadcast(Message::Binary(vic_msg.into()));
        frame_seq = frame_seq.wrapping_add(1);
        if let Some(audio) = audio_msg {
            hub.broadcast(Message::Binary(audio.into()));
            audio_seq = audio_seq.wrapping_add(1);
        }
        } else {
            // Paused/off advances nothing (frozen picture, silent) — EXCEPT a
            // one-shot present requested by `checkpoint/restore` (audit
            // ws-checkpoint-scrub-1). The TS controller ALWAYS presentFrame()s on a
            // restore so the paused canvas refreshes to the rolled-back picture with
            // "no client-grab dependency" (runtime-controller.ts:606-613). The paused
            // loop is otherwise silent, so the restore handler sets `force_present_frame`
            // and we consume it ONCE here: render the (already-restored) live frame,
            // push exactly one BIN_VIC (binary only — TS pushFrame on restore emits no
            // JSON), then clear the flag (no continuous push — the machine stays frozen).
            let vic_msg = {
                let mut st = state.lock().unwrap();
                if !st.force_present_frame {
                    None
                } else {
                    st.force_present_frame = false;
                    let cpu_cycle = st.session.machine.c64_core.clk as u32;
                    let (w, h, indices) = st.session.machine.render_canvas_indices();
                    // Binary VIC frame ONLY — 1:1 with TS pushFrame on restore (no JSON
                    // session/frame_available, which TS emits only in the running loop).
                    Some(build_vic_frame(frame_seq, cpu_cycle, &indices, w as u16, h as u16))
                }
            };
            if let Some(vic) = vic_msg {
                hub.broadcast(Message::Binary(vic.into()));
                frame_seq = frame_seq.wrapping_add(1);
            }
        } // end `if running` — paused/off advances nothing (frozen picture, silent)

        // ── Pace to real-time: sleep to the absolute target for this frame. ──
        frames_since_epoch = frames_since_epoch.wrapping_add(1);
        let target = epoch + period * frames_since_epoch;
        let now = Instant::now();
        if let Some(sleep) = target.checked_duration_since(now) {
            std::thread::sleep(sleep);
        } else if now.duration_since(target) > Duration::from_millis(100) {
            // Fell far behind (e.g. a heavy frame or a long disk mount): re-base
            // the epoch so we don't fast-forward to "catch up".
            epoch = Instant::now();
            frames_since_epoch = 0;
        }
    }

    // ── Teardown: clear the SID hook so the byte-exact (None) path is restored. ──
    let teardown = state.lock();
    if let Ok(mut st) = teardown {
        st.session.machine.sid.set_write_trace(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vic_frame_wire_layout_matches_tap() {
        // 2×2 indices for a compact check (the real frame is 384×272).
        let indices = [0x01, 0x06, 0x0e, 0x0f];
        let msg = build_vic_frame(0x11223344, 0xAABBCCDD, &indices, 384, 272);
        // envelope
        assert_eq!(msg[0], BIN_VIC);
        assert_eq!(&msg[1..5], &0x11223344u32.to_le_bytes());
        // payload header (data.subarray(5))
        let p = &msg[5..];
        assert_eq!(u16::from_le_bytes([p[0], p[1]]), 384); // w
        assert_eq!(u16::from_le_bytes([p[2], p[3]]), 272); // h
        assert_eq!(p[4], 1); // fmt
        assert_eq!(p[5], 0); // rsvd
        assert_eq!(u32::from_le_bytes([p[6], p[7], p[8], p[9]]), 0xAABBCCDD); // cycle
        // palette at offset 10: COLODORE[1] = white
        assert_eq!(&p[10..13], &[0x00, 0x00, 0x00]); // idx 0 black
        assert_eq!(&p[13..16], &[0xff, 0xff, 0xff]); // idx 1 white
        // indices at offset 58
        assert_eq!(&p[58..62], &indices);
        // total payload length = 10 + 48 + 4
        assert_eq!(p.len(), 10 + 48 + 4);
    }

    #[test]
    fn notify_hub_broadcasts_jsonrpc_notification_envelope() {
        // A subscribed client receives a JSON-RPC NOTIFICATION (no `id`) carrying
        // exactly the c64re `broadcast` envelope: { jsonrpc, method, params }.
        let hub = NotifyHub::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let _sub = hub.subscribe(tx);
        let live = hub.broadcast(
            "debug/breakpoint_hit",
            serde_json::json!({ "session_id": "integrated-1", "pc": 0xC000, "num": 1 }),
        );
        assert_eq!(live, 1);
        let msg = rx.try_recv().expect("notification enqueued");
        let text = match msg {
            Message::Text(t) => t.to_string(),
            other => panic!("expected text notification, got {other:?}"),
        };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "debug/breakpoint_hit");
        // A server-push has NO `id` — that is how a client tells it from a reply.
        assert!(v.get("id").is_none(), "notification must omit id");
        assert_eq!(v["params"]["session_id"], "integrated-1");
        assert_eq!(v["params"]["pc"], 0xC000);
        assert_eq!(v["params"]["num"], 1);
    }

    #[test]
    fn notify_hub_prunes_closed_subscribers() {
        let hub = NotifyHub::new();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let _sub = hub.subscribe(tx);
        drop(rx); // client gone → channel closed
        let live = hub.broadcast("audio/flush", serde_json::json!({ "session_id": "x" }));
        assert_eq!(live, 0, "closed subscriber is pruned");
    }

    #[test]
    fn audio_msg_is_stereo_s16le_duplicated() {
        let mono = [0i16, 100, -200, 32767];
        let msg = build_audio_msg(7, &mono);
        assert_eq!(msg[0], BIN_AUDIO);
        assert_eq!(&msg[1..5], &7u32.to_le_bytes());
        let p = &msg[5..];
        assert_eq!(p.len(), mono.len() * 4); // 4 bytes per mono sample (L+R s16le)
        // sample 1 = 100: L then R, both little-endian, identical.
        let s1 = 100i16.to_le_bytes();
        assert_eq!(&p[4..6], &s1); // L
        assert_eq!(&p[6..8], &s1); // R
        // sample 3 = 32767
        let s3 = 32767i16.to_le_bytes();
        assert_eq!(&p[12..14], &s3);
        assert_eq!(&p[14..16], &s3);
    }
}
