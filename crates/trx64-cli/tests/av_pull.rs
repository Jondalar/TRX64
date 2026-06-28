//! Part 2 smoke (automatable slice): the A/V pull path the emulator window uses.
//!
//! The window itself needs a display + manual eyeballing (see the spec's manual
//! check), but the data path it blits/plays — `pull_frame_buffer` (video) and
//! `pull_audio_drain` (audio) — is headless-testable. Boot, run a few frames, and
//! assert a real 384×272 framebuffer + a draining audio path.

use std::path::Path;

use trx64_cli::{boot_engine, default_rom_dir, Engine};

fn engine_or_skip() -> Option<Engine> {
    let rom_dir = default_rom_dir();
    if !Path::new(&rom_dir).join("kernal-901227-03.bin").exists() {
        eprintln!("[skip] av_pull: ROMs absent at {}", rom_dir.display());
        return None;
    }
    boot_engine(&rom_dir).ok()
}

#[test]
fn frame_buffer_is_full_canvas_with_content() {
    let Some(engine) = engine_or_skip() else { return };
    engine.exec_line("/power on");
    // Run ~30 frames so the READY screen is fully drawn.
    for _ in 0..30 {
        engine.pump_frame(19_656);
    }

    let fb = trx64_daemon::pull_frame_buffer(engine.shared_state());
    assert_eq!(fb.width, 384, "canvas width");
    assert_eq!(fb.height, 272, "canvas height");
    assert_eq!(fb.palette.len(), 48, "16 RGB triplets");
    assert_eq!(fb.indices.len(), (fb.width * fb.height) as usize, "one index per pixel");
    // Every index must be a valid 0..15 palette entry.
    assert!(fb.indices.iter().all(|&i| i < 16), "indices are 4-bit palette refs");
    // The READY screen is not a single flat colour — there is border + text + bg.
    let first = fb.indices[0];
    assert!(
        fb.indices.iter().any(|&i| i != first),
        "frame has more than one colour (real content drawn)"
    );
}

#[test]
fn audio_drain_path_is_live() {
    let Some(engine) = engine_or_skip() else { return };
    engine.exec_line("/power on");

    // First drain installs the SID capture hook + spawns the persistent render thread
    // and returns empty (no cycles elapsed yet) — that's the documented contract.
    let first = trx64_daemon::pull_audio_drain(engine.shared_state());
    assert_eq!(first.sample_rate, 44_100, "fixed reSID rate");
    assert!(first.samples.is_empty(), "first drain returns empty (no cycles yet)");

    // The real audio producer (audio.rs) drains continuously every few ms. Mirror that:
    // run cycles + drain in a loop and assert PCM flows at the expected rate. The very
    // first window's hand-off can race the render thread's poll, so allow a few drains
    // to reach steady state (this is exactly what the cpal producer thread does).
    let mut total = 0usize;
    for _ in 0..6 {
        for _ in 0..10 {
            engine.pump_frame(19_656);
        }
        std::thread::sleep(std::time::Duration::from_millis(60));
        total += trx64_daemon::pull_audio_drain(engine.shared_state()).samples.len();
    }
    // ~60 frames of real cycles ≈ >1s of audio → many thousands of mono i16 samples.
    assert!(total > 4_000, "audio drain produced steady PCM after running (got {total})");
}
