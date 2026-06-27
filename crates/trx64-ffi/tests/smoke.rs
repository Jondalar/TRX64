//! Smoke test for the typed in-process FFI path.
//!
//! Proves the typed façade end-to-end against a real booted machine:
//! construct Runtime → createSession(pal:true) → state() returns pc/cycles →
//! monitorExec("d") returns disasm → step() advances → an event listener receives
//! a typed RuntimeEvent → destroy.
//!
//! ROMs: resolved from `C64RE_ROOT` (default the C64RE checkout). Skipped (passes
//! with a note) if the ROM dir is absent, so the test never blocks a ROM-less CI.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use trx64_ffi::{EventListener, JoystickState, Pacing, Runtime, RuntimeEvent};

fn rom_dir() -> std::path::PathBuf {
    let root = std::env::var("C64RE_ROOT")
        .unwrap_or_else(|_| "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP".to_string());
    std::path::PathBuf::from(root).join("resources").join("roms")
}

struct CountingListener {
    count: Arc<AtomicU64>,
}
impl EventListener for CountingListener {
    fn on_event(&self, _event: RuntimeEvent) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn typed_in_process_path() {
    let roms = rom_dir();
    if !roms.join("kernal-901227-03.bin").exists() {
        eprintln!("[smoke] ROMs not found at {} — skipping", roms.display());
        return;
    }

    // ── construct ──
    let rt = Runtime::new(roms.to_string_lossy().to_string()).expect("Runtime::new");

    // ── createSession(pal:true) ──
    let session = rt.create_session(true).expect("create_session");
    assert_eq!(session.session_id, "integrated-1");
    assert_eq!(session.mode, "true-drive");
    assert!(session.attached, "create attaches to the singleton machine");

    // ── state() returns pc/cycles ──
    // A fresh boot is COLD-RESET + PAUSED (no autonomous run): PC sits at the KERNAL
    // reset vector $FCE2, cycles=0 — exactly like a daemon at startup before a run.
    let st = rt.state().expect("state");
    assert_eq!(st.mode, "true-drive");
    assert!(st.run_state == "paused" || st.run_state == "running");
    eprintln!(
        "[smoke] boot state: pc=${:04X} a=${:02X} x=${:02X} y=${:02X} sp=${:02X} p=${:02X} cycles={} raster={}",
        st.cpu.pc, st.cpu.a, st.cpu.x, st.cpu.y, st.cpu.sp, st.cpu.flags, st.c64_cycles, st.vic.raster_line
    );
    assert_eq!(st.cpu.pc, 0xFCE2, "fresh boot sits at the KERNAL reset vector");
    assert!(st.sid.regs.len() >= 25, "SID register file present");

    // ── run_cycles advances the machine ──
    let run = rt.run_cycles(100_000).expect("run_cycles");
    eprintln!("[smoke] after run_cycles(100000): c64Cycles={}", run.c64_cycles);
    assert!(run.c64_cycles >= 100_000, "machine advanced ~100k cycles");
    let after = rt.state().expect("state after run").c64_cycles;
    assert!(after > 0, "machine advanced");

    // ── monitorExec("d") returns disasm ──
    let disasm = rt.monitor_exec("d".to_string()).expect("monitor d");
    eprintln!("[smoke] monitor d:\n{}", disasm.lines().take(3).collect::<Vec<_>>().join("\n"));
    assert!(!disasm.trim().is_empty(), "disasm output non-empty");

    // ── monitor register dump (cross-check) ──
    let regs = rt.monitor_exec("r".to_string()).expect("monitor r");
    assert!(!regs.trim().is_empty(), "register dump non-empty");

    // ── step() advances ──
    let before = rt.state().expect("state before").c64_cycles;
    let stepped = rt.step().expect("step");
    assert!(stepped.c64_cycles >= before, "step did not go backwards");

    // ── pacing round-trip ──
    let ds = rt
        .set_pacing(Pacing { mode: "warp".to_string(), ratio: 1.0 })
        .expect("set_pacing");
    assert_eq!(ds.pacing.mode, "warp");

    // ── input does not error ──
    rt.key_down("A".to_string()).expect("key_down");
    rt.key_up("A".to_string()).expect("key_up");
    rt.joystick(2, JoystickState { up: false, down: false, left: false, right: true, fire: true })
        .expect("joystick set");
    rt.joystick(2, JoystickState { up: false, down: false, left: false, right: false, fire: false })
        .expect("joystick clear");

    // ── escape hatch (raw JSON) ──
    let raw = rt.call("ping".to_string(), "{}".to_string());
    assert!(raw.contains("\"result\""), "ping via escape hatch: {raw}");

    // ── typed event listener round-trip ──
    let count = Arc::new(AtomicU64::new(0));
    rt.set_listener(Box::new(CountingListener { count: count.clone() }));
    // A reset broadcasts `audio/flush` (a NotifyHub event) → the forwarder delivers it.
    rt.reset(false).expect("warm reset");
    // Give the forwarder thread a moment to drain + deliver.
    std::thread::sleep(std::time::Duration::from_millis(150));
    let n = count.load(Ordering::SeqCst);
    eprintln!("[smoke] listener received {n} typed event(s)");
    assert!(n >= 1, "expected at least one typed event from reset's audio/flush");
    rt.clear_listener();

    eprintln!("[smoke] OK — typed in-process FFI path proven");
}

/// Live A/V pull-API smoke (ADR-073 §pull): construct → boot → `frameBuffer()` is the
/// full-res 384×272 palette+index frame; `audioDrain()` returns PCM after a frame of
/// running and DRAINS (an immediate re-drain returns fewer/zero samples).
#[test]
fn live_av_pull_api() {
    let roms = rom_dir();
    if !roms.join("kernal-901227-03.bin").exists() {
        eprintln!("[smoke] ROMs not found at {} — skipping", roms.display());
        return;
    }

    let rt = Runtime::new(roms.to_string_lossy().to_string()).expect("Runtime::new");
    rt.create_session(true).expect("create_session");
    // Boot to the READY screen: `reset(cold:)` runs the KERNAL to READY (the 5 M-cycle
    // run-to-READY the daemon's session/reset does), so the VIC has swept full
    // `displayed` frames showing the light-blue border/background + READY text — a real
    // multi-colour frame, not the power-on black.
    rt.reset(true).expect("cold reset to READY");

    // ── frameBuffer(): full-res palette + index image ──
    let fb = rt.frame_buffer();
    eprintln!(
        "[smoke] frameBuffer: {}x{} palette={}B indices={}B",
        fb.width,
        fb.height,
        fb.palette.len(),
        fb.indices.len()
    );
    assert_eq!(fb.width, 384, "VICE PAL canvas width");
    assert_eq!(fb.height, 272, "VICE PAL canvas height");
    assert_eq!(fb.palette.len(), 48, "16 RGB palette entries");
    assert_eq!(
        fb.indices.len(),
        (fb.width * fb.height) as usize,
        "one index byte per pixel"
    );
    assert!(fb.indices.iter().all(|&i| i < 16), "all indices in 0..15");
    // A booted READY screen is not all one colour (border + text) — sanity that the
    // extraction is a real frame, not a zeroed buffer.
    let distinct = {
        let mut seen = [false; 16];
        for &i in fb.indices.iter() {
            seen[i as usize] = true;
        }
        seen.iter().filter(|&&s| s).count()
    };
    // A booted READY screen is the light-blue border (14) + blue background (6) — >1
    // colour, proving the extraction is a real swept frame, not a zeroed buffer.
    assert!(distinct >= 2, "a booted frame uses >1 colour (got {distinct})");

    // ── audioDrain(): drains + reports rate ──
    assert_eq!(rt.audio_sample_rate(), 44_100, "fixed reSID rate");
    // First drain installs the capture hook + primes reSID → no cycles yet → empty.
    let primed = rt.audio_drain();
    eprintln!("[smoke] audioDrain #1 (prime): {} samples", primed.len());
    // Run a frame so SID cycles elapse, then drain → non-empty PCM for that window.
    rt.run_cycles(19_656).expect("run_cycles frame");
    let first = rt.audio_drain();
    eprintln!("[smoke] audioDrain #2 (after frame): {} samples", first.len());
    assert!(!first.is_empty(), "a frame of running yields PCM samples");
    // An IMMEDIATE second drain (no cycles run in between) drains → far fewer / zero.
    let second = rt.audio_drain();
    eprintln!("[smoke] audioDrain #3 (immediate): {} samples", second.len());
    assert!(
        second.len() < first.len(),
        "immediate re-drain returns fewer samples (proves it drained): {} !< {}",
        second.len(),
        first.len()
    );

    eprintln!("[smoke] OK — live A/V pull-API proven (frameBuffer + audioDrain)");
}

/// Persistent-engine audio render-thread contract (the ~60 Hz hum fix). Drives
/// several emulation windows feeding the render thread, drains repeatedly, and
/// asserts the three properties that the per-drain-reconstruct path violated:
///   (a) the PCM is CONTINUOUS across drain boundaries — no large sample-to-sample
///       jump at a seam that wasn't in the source (the hum was a per-drain seam
///       discontinuity);
///   (b) the sample count over a ~1 s real-time-equivalent run ≈ 44100 (rate
///       correct — the engine advances by the REAL elapsed cycles);
///   (c) the reSID engine is CONSTRUCTED ONCE for the whole session (no per-drain
///       reconstruct) — the construction counter grows by exactly 1 across all the
///       drains.
#[test]
fn audio_persistent_engine_continuity() {
    let roms = rom_dir();
    if !roms.join("kernal-901227-03.bin").exists() {
        eprintln!("[smoke] ROMs not found at {} — skipping", roms.display());
        return;
    }

    let rt = Runtime::new(roms.to_string_lossy().to_string()).expect("Runtime::new");
    rt.create_session(true).expect("create_session");
    rt.reset(true).expect("cold reset to READY");

    const PAL_CYCLES_PER_FRAME: u64 = 19_656; // ~50 Hz PAL frame (matches streaming.rs)
    const FRAMES: u64 = 50; // ~1 s of real-time-equivalent emulation
    const SAMPLE_RATE: u64 = 44_100;

    // First drain installs the hook + spawns the render thread (constructs reSID once)
    // → empty (no cycles yet). Snapshot the construction count AFTER the engine exists.
    let primed = rt.audio_drain();
    assert!(primed.is_empty(), "prime drain is empty (no cycles yet)");
    let constructs_after_prime = trx64_daemon::resid_construct_count();
    assert!(
        constructs_after_prime >= 1,
        "the render thread constructed reSID at least once"
    );

    // ── Drive FRAMES windows, draining each, KEEPING the per-window chunks ──
    // Keeping the chunks lets us distinguish a SEAM delta (last sample of window N →
    // first sample of window N+1) from an INTRA-window delta — the whole point of the
    // continuity check (a per-drain reconstruct injects an outlier at the seams only).
    let mut chunks: Vec<Vec<i16>> = Vec::new();
    let mut stream: Vec<i16> = Vec::new();
    for f in 0..FRAMES {
        rt.run_cycles(PAL_CYCLES_PER_FRAME).expect("run a PAL frame");
        let chunk = rt.audio_drain();
        assert!(
            !chunk.is_empty(),
            "frame {f}: a frame of running yields PCM (persistent engine renders the window)"
        );
        stream.extend_from_slice(&chunk);
        chunks.push(chunk);
    }

    // (c) CONSTRUCT-ONCE: the counter did NOT grow across the FRAMES drains — the
    // engine is the SAME persistent instance, never reconstructed per drain.
    let constructs_after_run = trx64_daemon::resid_construct_count();
    assert_eq!(
        constructs_after_run, constructs_after_prime,
        "reSID was reconstructed during draining (per-drain reconstruct = the hum): \
         {constructs_after_prime} → {constructs_after_run}"
    );

    // (b) RATE: ~44100 samples per ~1 s of real-time-equivalent cycles. Allow a ±5 %
    // band for reSID's fractional sample timing + the final partial window.
    let expected = SAMPLE_RATE as f64; // FRAMES * PAL_CYCLES_PER_FRAME ≈ 1 s @ ~985 kHz
    let got = stream.len() as f64;
    eprintln!(
        "[smoke] persistent-engine audio: {} samples over {} frames (~{:.0} expected); \
         constructs={}",
        stream.len(),
        FRAMES,
        expected,
        constructs_after_run
    );
    assert!(
        (got - expected).abs() / expected < 0.05,
        "sample rate off: got {got} samples, expected ~{expected} (±5 %)"
    );

    // (a) CONTINUITY at the seams. Compute, SEPARATELY:
    //   * `max_intra_step` = largest adjacent |Δ| WITHIN any single window (the natural
    //     ceiling of a continuous signal — never crosses a drain boundary), and
    //   * `max_seam_step` = largest |Δ| ACROSS a drain boundary (last of window N →
    //     first of window N+1).
    // A persistent engine produces seam deltas indistinguishable from intra-window
    // deltas (the signal is one continuous render, only chopped for delivery). The
    // per-drain reconstruct restarted reSID's resampler FIR every drain → a sharp seam
    // SPIKE far above the smooth in-window deltas (the audible ~60 Hz hum). So a real
    // guard: every seam step must be within the intra-window ceiling (+ a tiny margin).
    let mut max_intra_step: i32 = 0;
    for c in &chunks {
        for w in c.windows(2) {
            let d = (w[1] as i32 - w[0] as i32).abs();
            if d > max_intra_step {
                max_intra_step = d;
            }
        }
    }
    let mut max_seam_step: i32 = 0;
    for pair in chunks.windows(2) {
        if let (Some(&prev_last), Some(&this_first)) = (pair[0].last(), pair[1].first()) {
            let d = (this_first as i32 - prev_last as i32).abs();
            if d > max_seam_step {
                max_seam_step = d;
            }
        }
    }
    eprintln!(
        "[smoke] continuity: max seam step={} max intra-window step={} ({} boundaries)",
        max_seam_step,
        max_intra_step,
        chunks.len().saturating_sub(1)
    );
    // A persistent, continuous render keeps the seam step at or below the intra-window
    // ceiling. Allow a small margin (the seam crosses a slightly-longer cycle gap due
    // to the host pull cadence vs. a fixed sample stride). A reconstruct seam would be
    // many×; this catches the regression with headroom for benign timing jitter.
    let ceiling = max_intra_step + max_intra_step / 4 + 64; // intra + 25 % + floor
    assert!(
        max_seam_step <= ceiling,
        "a drain seam step ({max_seam_step}) is an OUTLIER vs the intra-window ceiling \
         ({ceiling}) — discontinuity at a boundary (the per-drain-reconstruct hum)"
    );

    eprintln!("[smoke] OK — persistent-engine continuity + rate + construct-once proven");
}
