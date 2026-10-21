//! streaming.rs — live A/V binary push for `ws-av-tap.mjs` (ADR-073).
//!
//! The RuntimeController parity gap: the Node daemon's `ws-server.ts` pushes, per
//! emulated frame, two BINARY WebSocket messages to every connected client —
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
//!     [w:u16 LE][h:u16 LE][fmt:u8=1][rsvd:u8=0][cycle:u32 LE]
//!     (the model's canvas: 384×272 PAL, 384×247 NTSC — Spec 863)
//!     [48 B palette = 16×(R,G,B)][w*h index bytes, each & 0x0f]
//!     (palette at offset 10, indices at offset 58 — matches the tap's palOff/idxOff).
//!     `cycle` = the C64 CPU cycle counter (LE u32), NOT the frame number.
//!
//!   BIN_AUDIO = 0x02 payload: raw s16le STEREO PCM at 44100 Hz. reSID is MONO →
//!     each sample is duplicated into both channels (L,R) inline, little-endian.
//!
//! TRIGGER: the daemon's run loop (Spec 910 D6) advances the machine from startup, whether or
//! not anybody is connected; THIS module's hub is the list of A/V subscribers, and the loop
//! pushes each presented frame to them. A client subscribes by connecting (the tap sends
//! NO commands; `?av=0` opts out), so the push needs no trigger of its own. The loop paces
//! to real time — the model's frame rate (50.12 fps PAL, 59.83 NTSC), read every frame so a
//! model switch changes the pace (Spec 863) — and runs as fast as possible only for a bounded
//! run that asked for `pace: "warp"`.
//!
//!   AV_HELLO (JSON notification `av/hello`): sent to a client when it subscribes and to
//!     every client when the machine's model changes — the model, the video standard,
//!     the chip, the frame geometry, the clock and the canvas the frames that follow
//!     have.
//!
//! THREADING: `SidAudioEngine` owns a reSID engine through a raw pointer and is
//! therefore `!Send`, so the run loop runs on a dedicated OS thread (not a tokio task) that
//! owns the engine locally, for as long as at least one A/V client is subscribed. The SID
//! `write_trace` hook captures only a `Send` `(addr,value)` byte buffer; the loop drains it
//! per frame into the engine (verbatim the `scramble_av_record.rs` harness pattern). Built
//! binary messages are handed to the async WS writer via a tokio mpsc channel.

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

/// Real-time wall period of one emulated frame: the model's frame over its clock
/// (19656 / 985248 ≈ 19.95 ms → ~50.12 fps PAL; 17095 / 1022730 ≈ 16.72 ms → ~59.83 fps
/// NTSC), so the pace stays self-consistent with the cycles actually run.
fn frame_period(t: &trx64_core::model::Timing) -> Duration {
    Duration::from_secs_f64(t.cycles_per_frame as f64 / t.cpu_hz as f64)
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
// The `NotifyHub` is the TRX64 mirror of that fan-out. UNLIKE [`StreamHub`] (A/V
// subscribers only), every connection is on it, `?av=0` ones too, and it ALWAYS exists: it lives in the shared `State` so any handler
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

/// The A/V subscriber list of the daemon. Every connection that wants the picture and the
/// sound [`subscribe`](StreamHub::subscribe)s its outbound channel; the run loop pushes
/// each presented frame's BIN_VIC/BIN_AUDIO to all of them, pruning closed ones. The hub
/// owns no thread and starts nothing: the machine advances in the run loop whether or not
/// anybody is subscribed, and the push is one more consumer of that loop (Spec 910 D6).
pub struct StreamHub {
    inner: Mutex<HubInner>,
    state: SharedState,
}

struct HubInner {
    subscribers: Vec<Subscriber>,
    next_id: u64,
}

impl StreamHub {
    pub fn new(state: SharedState) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HubInner { subscribers: Vec::new(), next_id: 1 }),
            state,
        })
    }

    /// A/V subscribers right now. Spec 887: a subscriber holds the idle clock; an RPC-only
    /// connection (`?av=0`) never subscribes.
    pub fn live(&self) -> usize {
        let mut inner = self.inner.lock().unwrap();
        inner.subscribers.retain(|s| !s.out.is_closed());
        inner.subscribers.len()
    }

    /// Register a client's outbound channel. Returns a [`StreamSub`] guard that
    /// unsubscribes on drop.
    pub fn subscribe(self: &Arc<Self>, out: UnboundedSender<Message>) -> StreamSub {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id += 1;
        // Spec 863 — the A/V hello: say what the frames that follow are (model, standard,
        // chip, frame, clock, canvas) before the first one arrives.
        {
            let mut st = self.state.lock().unwrap();
            let hello = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "av/hello",
                "params": crate::av_hello(&st),
            });
            let _ = out.send(Message::Text(hello.to_string()));
            // Spec 837 — a new client gets a picture even if the machine is paused.
            //
            // A running machine pushes a frame every presented frame, a PAUSED one pushes
            // nothing at all: a client arriving during a pause — a page reload, an `undump`,
            // which deliberately leaves the session paused — would never receive a first
            // frame and sit on "No frame yet" while every other reading says the machine is
            // healthy. A paused machine HAS a picture; it just is not changing. So ask for
            // exactly one present, on arrival.
            st.force_present_frame = true;
            st.av_subscribers = inner.subscribers.len() + 1;
        }
        inner.subscribers.push(Subscriber { id, out });
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
        let left = {
            let mut inner = self.inner.lock().unwrap();
            inner.subscribers.retain(|s| s.id != id);
            inner.subscribers.len()
        };
        self.state.lock().unwrap().av_subscribers = left;
    }
}

/// Per-connection subscription guard. Dropping it unsubscribes.
pub struct StreamSub {
    hub: Arc<StreamHub>,
    id: u64,
}

impl Drop for StreamSub {
    fn drop(&mut self) {
        self.hub.unsubscribe(self.id);
    }
}

/// The run loop's handle. Dropping it stops the loop and hands the machine back to a host
/// that clocks it itself.
pub struct RunLoopGuard {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    state: SharedState,
}

impl Drop for RunLoopGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        self.state.lock().unwrap().loop_thread = false;
    }
}

/// Start the daemon's run loop (Spec 910 D6): the one thing that advances a daemon's
/// machine. It runs from startup, on every daemon, whether a client is attached or not, and
/// is gated only on `running` and the pace. The A/V push (`hub`) is a consumer of it: frames
/// and audio go out only while a client is subscribed, and the loop owns the reSID engine
/// (`!Send`) for exactly that long.
pub fn start_run_loop(state: &SharedState, hub: Option<Arc<StreamHub>>) -> RunLoopGuard {
    let stop = Arc::new(AtomicBool::new(false));
    state.lock().unwrap().loop_thread = true;
    let join = {
        let state = Arc::clone(state);
        let stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("trx64-run-loop".into())
            .spawn(move || run_loop(state, hub, stop))
            .expect("spawn run-loop thread")
    };
    RunLoopGuard { stop, join: Some(join), state: Arc::clone(state) }
}

/// What the loop holds while at least one A/V client is subscribed: the reSID engine, the
/// buffer the SID write hook fills, and the sequence numbers of the stream.
struct AvOut {
    engine: SidAudioEngine,
    writes: Arc<Mutex<Vec<(u8, u8)>>>,
    /// The machine-rebuild generation the hook was installed for. A power cycle builds a
    /// fresh SID with no write-trace hook, so the hook and the reSID prime are redone when
    /// `machine_generation` moves (Spec 786) — else audio goes silent for good.
    generation: u64,
    frame_seq: u32,
    audio_seq: u32,
    /// The master clock of the last frame presented. A paused machine can still MOVE — the
    /// rewind transport steps it, a monitor `z`/`n` advances it — and comparing the clock
    /// is what tells "the machine moved" from "the machine is frozen".
    last_present_clk: u64,
}

impl AvOut {
    fn install(state: &SharedState) -> Self {
        let mut st = state.lock().unwrap();
        // Spec 863 D3 — reSID samples at the model's clock with its SID.
        let mut engine = SidAudioEngine::new(ResidConfig::for_model(st.session.machine.model()));
        let writes: Arc<Mutex<Vec<(u8, u8)>>> = Arc::new(Mutex::new(Vec::new()));
        Self::hook(&mut st, &mut engine, &writes);
        let generation = st.machine_generation;
        let last_present_clk = st.session.machine.clk;
        AvOut { engine, writes, generation, frame_seq: 0, audio_seq: 0, last_present_clk }
    }

    /// Install the additive SID write-trace hook so EVERY $D4xx write (in CPU order) is
    /// captured for reSID, and prime the engine with the live register file so the stream
    /// starts from the live state, not from power-on silence.
    fn hook(st: &mut crate::State, engine: &mut SidAudioEngine, writes: &Arc<Mutex<Vec<(u8, u8)>>>) {
        let w = Arc::clone(writes);
        st.session.machine.set_sid_write_trace(Some(Box::new(move |_chip, reg, value, _clk| {
            w.lock().unwrap().push((reg, value));
        })));
        for reg in 0u8..=0x18 {
            let v = st.session.machine.read_full(0xD400 + reg as u16);
            engine.record_write(reg, v);
        }
        engine.record_boundary(0); // apply the priming writes, emit nothing
        engine.flush();
        let _ = engine.take_pcm(); // discard priming silence
    }

    fn remove(self, state: &SharedState) {
        // Restore the byte-exact (None) write-trace path.
        state.lock().unwrap().session.machine.set_sid_write_trace(None);
    }
}

/// The run loop body. Runs on a dedicated OS thread (it owns the reSID engine while an A/V
/// client is subscribed). Per iteration, under the State lock: the wall-clock housekeeping,
/// then — if the machine is running — one frame of advance (eight in warp) through
/// [`crate::loop_iteration`], the one function every host advances the machine through.
/// The lock is released before the frame is pushed and before the pacing sleep, so other
/// requests, and a request parked on a stop, interleave between frames.
fn run_loop(state: SharedState, hub: Option<Arc<StreamHub>>, stop: Arc<AtomicBool>) {
    let mut av: Option<AvOut> = None;
    let mut timing = state.lock().unwrap().session.machine.timing();
    let mut period = frame_period(&timing);
    // Epoch-anchored pacing: schedule each frame at an absolute target time so the pace
    // doesn't drift. If we fall far behind (>100 ms), re-base the epoch instead of spinning
    // to catch up.
    let mut epoch = Instant::now();
    let mut frames_since_epoch: u32 = 0;
    // A WALL-CLOCK reference for the media auto-persist cadence: a flash/disk delta then
    // pause/JAM/bp must STILL reach the host file, so the persist hooks debounce on this
    // monotonic clock and run every iteration, not only while running.
    let persist_epoch = Instant::now();

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }

        // The A/V consumer exists exactly while a client is subscribed. Asked of the hub
        // before the State lock is taken: the hub takes that lock itself while subscribing.
        let live = hub.as_ref().map(|h| h.live()).unwrap_or(0);
        if live > 0 && av.is_none() {
            av = Some(AvOut::install(&state));
        } else if live == 0 {
            if let Some(a) = av.take() {
                a.remove(&state);
            }
        }

        let unthrottled: bool;
        let (vic_msg, audio_msg) = {
            let mut st = state.lock().unwrap();
            st.av_subscribers = live;
            // Spec 863 — a model switch (or a restore across one) moved the clock and the
            // frame: re-sample reSID at the new clock, and pace to the new frame from here.
            // The transport restores anchors from this loop, outside any request: a step
            // across a switch lands on the other model here, and the session and clients
            // learn it here too.
            crate::sync_model_identity(&mut st);
            let now_timing = st.session.machine.timing();
            if now_timing != timing {
                timing = now_timing;
                period = frame_period(&timing);
                epoch = Instant::now();
                frames_since_epoch = 0;
                if let Some(a) = av.as_mut() {
                    a.engine.set_clock_freq(timing.cpu_hz as f64);
                }
            }
            // Spec 786 audio fix — a power-cycle rebuilds the machine into a fresh SID with
            // no write-trace hook: re-attach it and re-prime reSID, or audio dies.
            if let Some(a) = av.as_mut() {
                if st.machine_generation != a.generation {
                    a.generation = st.machine_generation;
                    let AvOut { engine, writes, .. } = a;
                    AvOut::hook(&mut st, engine, writes);
                }
            }

            // Media auto-persist, every iteration regardless of run-state.
            let now_ms = persist_epoch.elapsed().as_millis() as u64;
            crate::stream_maybe_autopersist_cart(&mut st, now_ms);
            crate::stream_maybe_autopersist_disk(&mut st, now_ms);
            crate::release_idle_control_owner(&mut st);

            let running = st.session.running;
            let warp = st.pacing_mode == "warp";
            // A bounded run asked for as fast as possible has no pace to keep.
            unthrottled = running && warp && st.run_cap_clk.is_some();
            let out = if running {
                // ── One iteration of the one advance: the transport owns the frame while it
                // is rewound; otherwise the machine advances, frame-aligned, with the
                // per-frame work (anchor, recorder, observers, stop check, stop push) done
                // inside, once per frame of machine time (Spec 910 D9).
                let d_cycles = crate::loop_iteration(&mut st, warp);
                match av.as_mut() {
                    Some(a) => Some(a.present_running(&mut st, d_cycles, warp)),
                    None => None,
                }
            } else {
                // A paused machine advances nothing — except that the rewind transport moves
                // it while the emulation stands still: `play back` sets a DIRECTION and
                // leaves the stepping to this loop.
                if st.transport.playing.is_some() {
                    crate::transport_tick(&mut st, timing.cycles_per_frame);
                }
                match av.as_mut() {
                    Some(a) => a.present_paused(&mut st).map(|v| (v, None)),
                    None => None,
                }
            };
            st.loop_cv.notify_all();
            out
        }
        .map_or((None, None), |(v, a)| (Some(v), a));

        // ── Broadcast BIN_VIC then BIN_AUDIO to all live subscribers. ──
        if let (Some(h), Some(vic)) = (hub.as_ref(), vic_msg) {
            h.broadcast(Message::Binary(vic.into()));
            if let Some(audio) = audio_msg {
                h.broadcast(Message::Binary(audio.into()));
            }
        }

        // ── Pace: sleep to the absolute target for this frame. ──
        if unthrottled {
            std::thread::sleep(Duration::from_micros(100));
            epoch = Instant::now();
            frames_since_epoch = 0;
            continue;
        }
        frames_since_epoch = frames_since_epoch.wrapping_add(1);
        let target = epoch + period * frames_since_epoch;
        let now = Instant::now();
        if let Some(sleep) = target.checked_duration_since(now) {
            std::thread::sleep(sleep);
        } else if now.duration_since(target) > Duration::from_millis(100) {
            // Fell far behind (a heavy frame, a long disk mount, a run that held the lock):
            // re-base the epoch so we don't fast-forward to "catch up".
            epoch = Instant::now();
            frames_since_epoch = 0;
        }
    }

    // ── Teardown: clear the SID hook so the byte-exact (None) path is restored. ──
    if let Some(a) = av.take() {
        a.remove(&state);
    }
}

impl AvOut {
    /// The frame a running iteration presents: the reSID PCM for exactly the window that ran
    /// (a halt may stop mid-frame), the canvas, and the metadata notification.
    fn present_running(
        &mut self,
        st: &mut crate::State,
        d_cycles: u64,
        warp: bool,
    ) -> (Message, Option<Message>) {
        // Audio: drain this window's writes (CPU order) into the engine, close the frame
        // boundary, flush → reSID PCM for exactly this window.
        {
            let mut pending = self.writes.lock().unwrap();
            for &(addr, value) in pending.iter() {
                self.engine.record_write(addr, value);
            }
            pending.clear();
        }
        self.engine.record_boundary(d_cycles as u32);
        self.engine.flush();
        let mono = self.engine.take_pcm();

        // Video: crop the per-cycle displayed buffer → the model's canvas, 4-bit indices.
        let cpu_cycle = st.session.machine.c64_core.clk as u32;
        let (w, h, indices) = st.session.machine.render_canvas_indices();

        // The lightweight JSON `session/frame_available` notification on every PRESENTED
        // frame, alongside the binary VIC frame, for metadata-only consumers.
        st.notify.broadcast(
            "session/frame_available",
            serde_json::json!({
                "session_id": st.session.id,
                "frame": self.frame_seq as u64,
                "c64Cycles": st.session.machine.clk,
            }),
        );
        self.last_present_clk = st.session.machine.clk;

        let vic = build_vic_frame(self.frame_seq, cpu_cycle, &indices, w as u16, h as u16);
        self.frame_seq = self.frame_seq.wrapping_add(1);
        // Warp: skip audio (8× PCM would garble).
        let audio = if warp || mono.is_empty() {
            None
        } else {
            let m = build_audio_msg(self.audio_seq, &mono);
            self.audio_seq = self.audio_seq.wrapping_add(1);
            Some(Message::Binary(m.into()))
        };
        (Message::Binary(vic.into()), audio)
    }

    /// A PAUSED machine owes the clients a picture when it moved or when a handler asked
    /// for one (a `checkpoint/restore` to the same cycle, a monitor write): one BIN_VIC, no
    /// JSON — the machine stays frozen.
    fn present_paused(&mut self, st: &mut crate::State) -> Option<Message> {
        let clk = st.session.machine.clk;
        if !paused_present_due(st.force_present_frame, clk, self.last_present_clk) {
            return None;
        }
        st.force_present_frame = false;
        self.last_present_clk = clk;
        let cpu_cycle = st.session.machine.c64_core.clk as u32;
        let (w, h, indices) = st.session.machine.render_canvas_indices();
        let vic = build_vic_frame(self.frame_seq, cpu_cycle, &indices, w as u16, h as u16);
        self.frame_seq = self.frame_seq.wrapping_add(1);
        Some(Message::Binary(vic.into()))
    }
}

/// Does a PAUSED machine owe the clients a picture?
///
/// Two reasons, and the second is the one the transport keys needed. A forced
/// present is an explicit request from a handler that moved the machine without
/// moving the clock (`checkpoint/restore` to the same cycle, a monitor memory
/// write). A changed master clock is the general answer: the machine advanced or
/// was placed somewhere else, so what the client is showing is stale.
///
/// Deliberately NOT "present every iteration while paused": rendering the canvas
/// costs real time (the multicolour-bitmap path especially), and a frozen machine
/// would be paying it fifty times a second to redraw the identical picture.
fn paused_present_due(force: bool, clk: u64, last_presented_clk: u64) -> bool {
    force || clk != last_presented_clk
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec 808 — F9/F10/F12 moved the machine and the browser kept showing the frame
    /// from the moment of the pause, because the pump only pushed pictures in the
    /// running branch. The rule that fixes it, on its own.
    #[test]
    fn a_paused_machine_that_moved_owes_a_frame() {
        // Frozen: same clock, nothing forced → no render, no push.
        assert!(!paused_present_due(false, 1000, 1000));
        // Moved: the transport stepped it to another anchor.
        assert!(paused_present_due(false, 980, 1000));
        // Moved forward: `frame +1` at the head runs one frame live.
        assert!(paused_present_due(false, 1020, 1000));
        // Forced: a restore that landed on the SAME cycle still repaints.
        assert!(paused_present_due(true, 1000, 1000));
    }

    /// Spec 863 — real time is the model's frame rate: ~59.83 frames a second on NTSC, and
    /// on PAL exactly the period the loop always slept.
    #[test]
    fn the_pace_is_the_models_frame_rate() {
        let ntsc = trx64_core::model::resolve("c64-ntsc").unwrap().timing;
        let p = frame_period(&ntsc).as_secs_f64();
        assert!((1.0 / p - 59.826).abs() < 0.001, "{}", 1.0 / p);
        let pal = trx64_core::model::default_model().timing;
        assert_eq!(frame_period(&pal), Duration::from_secs_f64(19656.0 / 985_248.0));
    }

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
