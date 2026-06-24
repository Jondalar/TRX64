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
use trx64_core::NullSink;

use crate::SharedState;

/// Binary message type codes (1:1 with c64re `ws-server.ts` + ws-av-tap.mjs).
pub const BIN_VIC: u8 = 0x01;
pub const BIN_AUDIO: u8 = 0x02;

/// fmt 1 = palette-indexed VIC frame (the only format the tap decodes).
const VIC_FMT_INDEXED: u8 = 1;

/// PAL Φ2 cycles per frame (312 rasterlines × 63 cycles = 19656). Canonical PAL
/// frame length used by the project's av-record harness; one frame per push.
const CYC_PER_FRAME: u64 = 19656;
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

/// Handle to a running streaming loop. Dropping (or calling [`stop`](Self::stop))
/// signals the loop thread to exit and clears the SID audio hook.
pub struct StreamHandle {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl StreamHandle {
    /// Signal the loop to stop and join the thread.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Spawn the per-connection streaming loop. Returns a handle; while it lives the
/// loop runs one PAL frame, renders + drains audio, pushes BIN_VIC + BIN_AUDIO
/// down `out`, then sleeps to hold real-time (~50 fps). It exits when `out` closes
/// (the client disconnected), when [`StreamHandle::stop`] is called, or on the
/// internal stop flag.
///
/// The loop owns the `SidAudioEngine` locally (it is `!Send`). It locks the shared
/// `State` only for the brief window of running+rendering each frame, so other
/// JSON-RPC requests on the connection still interleave between frames.
pub fn spawn_stream(state: SharedState, out: UnboundedSender<Message>) -> StreamHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);

    let join = std::thread::Builder::new()
        .name("trx64-av-stream".into())
        .spawn(move || stream_loop(state, out, stop_thread))
        .expect("spawn av-stream thread");

    StreamHandle { stop, join: Some(join) }
}

/// The streaming loop body. Runs on a dedicated OS thread (owns the reSID engine).
fn stream_loop(state: SharedState, out: UnboundedSender<Message>, stop: Arc<AtomicBool>) {
    // ── reSID audio engine (owned on this thread; !Send) + the Send write hook ──
    let mut engine = SidAudioEngine::new(ResidConfig::default());
    let writes: Arc<Mutex<Vec<(u8, u8)>>> = Arc::new(Mutex::new(Vec::new()));
    {
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
    }
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

    loop {
        if stop.load(Ordering::SeqCst) || out.is_closed() {
            break;
        }

        // ── Run one PAL frame + render + drain this frame's SID writes ──
        // Lock the shared State only for this window; release before sleeping so
        // other JSON-RPC requests (and disk mounts) interleave between frames.
        let (vic_msg, audio_msg) = {
            let mut st = state.lock().unwrap();

            let clk_before = st.session.machine.c64_core.clk;
            {
                let mut sink = NullSink;
                st.session
                    .machine
                    .run_for_full(CYC_PER_FRAME, &mut sink, |_, _, _, _, _, _, _| {});
            }
            let d_cycles = st.session.machine.c64_core.clk.wrapping_sub(clk_before) as u32;
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

            // Drop the lock before building the (larger) wire buffers + sleeping.
            drop(st);

            let vic = build_vic_frame(frame_seq, cpu_cycle, &indices, w as u16, h as u16);
            let audio = if mono.is_empty() {
                None
            } else {
                Some(build_audio_msg(audio_seq, &mono))
            };
            (vic, audio)
        };

        // ── Push BIN_VIC then BIN_AUDIO. A closed channel = client gone → exit. ──
        if out.send(Message::Binary(vic_msg.into())).is_err() {
            break;
        }
        frame_seq = frame_seq.wrapping_add(1);
        if let Some(audio) = audio_msg {
            if out.send(Message::Binary(audio.into())).is_err() {
                break;
            }
            audio_seq = audio_seq.wrapping_add(1);
        }

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
    if let Ok(mut st) = state.lock() {
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
