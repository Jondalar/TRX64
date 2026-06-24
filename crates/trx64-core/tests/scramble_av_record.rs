//! scramble_av_record.rs — AUDIO+VIDEO recording harness for SCRAMBLE INFINITY.
//!
//! Produces a raw RGBA video stream + s16le stereo PCM (TRX64's NATIVE reSID
//! audio) for the scramble title/demo, then (when ffmpeg is present) muxes them
//! into an H264(yuv420p)+AAC .mp4 the user can WATCH + LISTEN to in order to
//! evaluate the reSID audio quality.
//!
//! This is an ADDITIVE harness only: it touches NO emulation-core behavior. The
//! audio engine (`SidAudioEngine`, a reSID wrapper) is wired up here via the
//! strictly-additive `Sid6581::write_trace` hook (None on every byte-exact path)
//! — the in-tick fastsid register engine and the WS contract are untouched.
//!
//! By construction the A/V speed is correct: each PAL frame is `run_for_full`'d
//! for one frame's cycle budget, the framebuffer is rendered ONCE per frame, and
//! the reSID PCM for that SAME emulated window is emitted via `emit(dCycles)`.
//! N frames @ 50fps + the matching reSID PCM = the same emulated wall-time → the
//! mp4 audio and video are inherently in sync.
//!
//! Run with:
//!   cargo test -p trx64-core --test scramble_av_record -- --ignored --nocapture
//!
//! Tunables via env:
//!   SCRAMBLE_AV_FRAMES   number of PAL frames to record (default 6000 ≈ 120s)
//!   SCRAMBLE_AV_WARMUP   cycles to run after RUN before recording (default 20M)

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::resid_ffi::ResidConfig;
use trx64_core::resid_audio::SidAudioEngine;
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
const SAMPLE: &str =
    "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/scramble_infinity.d64";
const OUT_MP4: &str = "/Users/alex/Development/C64/Tools/TRX64/traces/scramble_trx64.mp4";
const V_RAW: &str = "/tmp/scramble_v.rgba";
const A_RAW: &str = "/tmp/scramble_a.pcm";
const FFMPEG: &str = "/opt/homebrew/bin/ffmpeg";

/// PAL Φ2 cycles per frame (985248 Hz / 50.12454 fps ≈ 19656). This is the
/// canonical PAL frame length (312 rasterlines × 63 cycles).
const CYC_PER_FRAME: u64 = 19656;
/// Sample rate of the reSID PCM (= ResidConfig default DEFAULT_SAMPLE_RATE).
const SAMPLE_RATE: u32 = 44100;
/// Video frame rate handed to ffmpeg (PAL ≈ 50.12 fps; 50 is close enough and
/// keeps the audio/video drift well under perceptible across 2 minutes).
const FPS: u32 = 50;

fn roms_present() -> bool {
    let d = Path::new(ROM_DIR);
    d.join("kernal-901227-03.bin").exists()
        && (d.join("dos1541-325302-01+901229-05.bin").exists() || d.join("1541.bin").exists())
}

fn inject_keys(m: &mut Machine, s: &[u8]) {
    for (i, b) in s.iter().enumerate() {
        m.poke(0x0277 + i as u16, &[*b]);
    }
    m.poke(0x00c6, &[s.len() as u8]);
}

#[test]
#[ignore = "A/V recording harness; run explicitly with --ignored --nocapture"]
fn scramble_av_record() {
    if !roms_present() {
        eprintln!("skip: ROMs absent");
        return;
    }
    let d64 = match std::fs::read(SAMPLE) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("skip: sample disk absent");
            return;
        }
    };

    let frames: usize = std::env::var("SCRAMBLE_AV_FRAMES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6000);
    let warmup_cyc: u64 = std::env::var("SCRAMBLE_AV_WARMUP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000_000);

    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut sink = NullSink;

    // ── Boot → mount → LOAD"*",8,1 → RUN (mirrors atn_irq_lag_probe.rs) ────────
    m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: d64.clone(),
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    m.run_for_full(500_000, &mut sink, |_, _, _, _, _, _, _| {});
    inject_keys(&mut m, b"LOAD\"*\",8,1\r");

    // Run the LOAD to BASIC-ready (editor idle, no pending keys).
    let mut load_done = false;
    let mut ready_streak = 0u32;
    for _ in 0..600 {
        m.run_for_full(50_000, &mut sink, |_, _, _, _, _, _, _| {});
        let pc = m.c64_core.reg_pc;
        if (0xE5C0..=0xE5F0).contains(&pc) && m.read_full(0x00c6) == 0 {
            ready_streak += 1;
            if ready_streak >= 3 {
                load_done = true;
                break;
            }
        } else {
            ready_streak = 0;
        }
    }
    if !load_done {
        eprintln!("LOAD did not return to BASIC ready — aborting record.");
        return;
    }
    eprintln!("LOAD complete (PC=${:04X}). Typing RUN.", m.c64_core.reg_pc);
    inject_keys(&mut m, b"RUN\r");

    // ── Warm up: let the loader complete + the title come up + the tune start ──
    // The reference renders a clean SCRAMBLE INFINITY title ~8–20M cycles after
    // RUN; we run the warmup WITHOUT the audio hook (no reSID load during the
    // loader's heavy serial bit-bang), then install the hook so the recording
    // captures the live title/demo tune cleanly.
    eprintln!("Warming up {warmup_cyc} cycles to reach the title...");
    let mut done = 0u64;
    while done < warmup_cyc {
        let chunk = 1_000_000.min(warmup_cyc - done);
        m.run_for_full(chunk, &mut sink, |_, _, _, _, _, _, _| {});
        done += chunk;
    }
    eprintln!(
        "Warmup done. PC=${:04X} D020=${:02X} D011=${:02X}",
        m.c64_core.reg_pc,
        m.read_full(0xD020),
        m.read_full(0xD011)
    );

    // ── Wire up the reSID AUDIO engine via the additive write_trace hook ───────
    // The `SidAudioEngine` owns the reSID FFI handle, which holds a process-wide
    // MutexGuard for its lifetime and is therefore `!Send`. The write_trace hook,
    // however, must be `Send`. So the hook captures ONLY a `Send` byte buffer of
    // raw `(addr, value)` writes (in CPU order); the per-frame loop — running on
    // this thread, where the engine lives — drains that buffer into the engine,
    // records the frame boundary (dCycles) and flushes → reSID PCM for the frame.
    let mut engine = SidAudioEngine::new(ResidConfig::default());
    let writes: Arc<Mutex<Vec<(u8, u8)>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let w = Arc::clone(&writes);
        m.sid.set_write_trace(Some(Box::new(move |addr, value| {
            w.lock().unwrap().push((addr, value));
        })));
    }
    // Prime reSID with the CURRENT SID register file so the engine starts from
    // the live title state (frequencies/PW/control already set by the loader)
    // rather than power-on silence. These are replayed in register order.
    for reg in 0u8..=0x18 {
        let v = m.read_full(0xD400 + reg as u16);
        engine.record_write(reg, v);
    }
    engine.record_boundary(0); // apply the priming writes, emit nothing
    engine.flush();

    // ── Record N PAL frames: video RGBA + reSID PCM, same emulated window ──────
    eprintln!("Recording {frames} PAL frames (~{:.1}s)...", frames as f64 / FPS as f64);
    let mut v_buf: Vec<u8> = Vec::with_capacity(frames * 384 * 272 * 4);
    let mut a_pcm: Vec<i16> = Vec::new(); // mono reSID samples
    let (mut w, mut h) = (0usize, 0usize);

    for fi in 0..frames {
        let clk_before = m.c64_core.clk;
        m.run_for_full(CYC_PER_FRAME, &mut sink, |_, _, _, _, _, _, _| {});
        let d_cycles = (m.c64_core.clk - clk_before) as u32;

        // Audio: drain this frame's writes (in CPU order) into the engine, close
        // the frame boundary, flush → PCM for exactly this emulated window.
        {
            let mut pending = writes.lock().unwrap();
            for &(addr, value) in pending.iter() {
                engine.record_write(addr, value);
            }
            pending.clear();
        }
        engine.record_boundary(d_cycles);
        engine.flush();
        a_pcm.extend_from_slice(&engine.take_pcm());

        // Video: render the framebuffer (RGBA 384×272) and append, converting to
        // the rgba byte order ffmpeg expects (R,G,B,A) — render_canvas_rgba
        // already emits R,G,B,A with alpha 0xFF, so it is appended verbatim.
        let (rw, rh, rgba) = m.render_canvas_rgba();
        w = rw;
        h = rh;
        v_buf.extend_from_slice(&rgba);

        if fi % 500 == 0 {
            eprintln!(
                "  frame {fi}/{frames}: PC=${:04X} d_cyc={d_cycles} pcm_total={}",
                m.c64_core.reg_pc,
                a_pcm.len()
            );
        }
    }

    // Drop the audio hook before we finish (good hygiene; the Machine is dropped
    // anyway, but this releases the Arc clone held in the closure).
    m.sid.set_write_trace(None);

    eprintln!(
        "Recorded {frames} frames @ {w}x{h}; mono PCM samples={} (~{:.2}s of audio)",
        a_pcm.len(),
        a_pcm.len() as f64 / SAMPLE_RATE as f64
    );

    // ── Audio diagnostics: RMS / peak — PROVE the title is not silent ──────────
    let (rms, peak) = pcm_stats(&a_pcm);
    eprintln!("AUDIO STATS: rms={rms:.2} peak={peak} (i16 full-scale=32767)");
    if peak == 0 {
        eprintln!(
            "WARNING: recorded audio is SILENT (peak=0). The title may be silent in \
             this window — increase SCRAMBLE_AV_WARMUP or SCRAMBLE_AV_FRAMES to reach \
             the tune."
        );
    }

    // ── Write raw streams (video rgba + STEREO s16le PCM) ──────────────────────
    std::fs::write(V_RAW, &v_buf).expect("write rgba");
    // reSID is MONO → duplicate L=R for stereo s16le.
    let mut a_bytes: Vec<u8> = Vec::with_capacity(a_pcm.len() * 4);
    for &s in &a_pcm {
        let le = s.to_le_bytes();
        a_bytes.extend_from_slice(&le); // L
        a_bytes.extend_from_slice(&le); // R
    }
    std::fs::write(A_RAW, &a_bytes).expect("write pcm");
    eprintln!(
        "Wrote {} ({} bytes) and {} ({} bytes, stereo s16le)",
        V_RAW,
        v_buf.len(),
        A_RAW,
        a_bytes.len()
    );

    // ── Mux with ffmpeg → H264(yuv420p)+AAC mp4 (QuickTime-playable) ───────────
    if !Path::new(FFMPEG).exists() {
        eprintln!("ffmpeg not at {FFMPEG}; raw streams written, skipping mux.");
        return;
    }
    let video_size = format!("{w}x{h}");
    let status = Command::new(FFMPEG)
        .args([
            "-y",
            "-f", "rawvideo",
            "-pixel_format", "rgba",
            "-video_size", &video_size,
            "-framerate", &FPS.to_string(),
            "-i", V_RAW,
            "-f", "s16le",
            "-ar", &SAMPLE_RATE.to_string(),
            "-ac", "2",
            "-i", A_RAW,
            "-c:v", "libx264",
            "-pix_fmt", "yuv420p",
            "-c:a", "aac",
            "-b:a", "192k",
            "-shortest",
            OUT_MP4,
        ])
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg mux failed");
    eprintln!("MUXED → {OUT_MP4}");

    // Quick verify: ffprobe streams.
    if let Ok(out) = Command::new("/opt/homebrew/bin/ffprobe")
        .args([
            "-v", "error",
            "-show_entries",
            "format=duration:stream=codec_type,codec_name,width,height,sample_rate,channels",
            "-of", "default=noprint_wrappers=1",
            OUT_MP4,
        ])
        .output()
    {
        eprintln!("== ffprobe ==\n{}", String::from_utf8_lossy(&out.stdout));
    }
}

/// RMS + peak |amplitude| over mono i16 PCM. Proof the audio is not silent.
fn pcm_stats(pcm: &[i16]) -> (f64, i16) {
    if pcm.is_empty() {
        return (0.0, 0);
    }
    let mut sumsq = 0.0f64;
    let mut peak = 0i16;
    for &s in pcm {
        sumsq += (s as f64) * (s as f64);
        let a = s.unsigned_abs() as i32;
        if a > peak as i32 {
            peak = a.min(i16::MAX as i32) as i16;
        }
    }
    ((sumsq / pcm.len() as f64).sqrt(), peak)
}
