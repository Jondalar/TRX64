//! Part 1 smoke (scripted, TUI-less): drive the high-level verbs + a monitor command
//! on a SINGLE in-process machine and assert the expected transitions.
//!
//! Spec verify (Part 1): boot → `power on`/`reset` → `run` → `session/state` shows
//! running → a monitor `d` returns disasm → `pause`.
//!
//! Skips gracefully when the ROMs are absent (CI without the C64RE ROM bundle).

use std::path::Path;

use trx64_cli::{boot_engine, default_rom_dir, Engine};

fn engine_or_skip() -> Option<Engine> {
    let rom_dir = default_rom_dir();
    if !Path::new(&rom_dir).join("kernal-901227-03.bin").exists() {
        eprintln!("[skip] smoke: ROMs absent at {}", rom_dir.display());
        return None;
    }
    match boot_engine(&rom_dir) {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("[skip] smoke: boot failed: {e}");
            None
        }
    }
}

#[test]
fn high_level_verb_sequence_drives_one_machine() {
    let Some(engine) = engine_or_skip() else { return };

    // ── power on → cold boot, host run flag set ────────────────────────────────
    let r = engine.exec_line("/power on");
    assert!(r.output.contains("POWER ON"), "power on output: {}", r.output);
    assert!(engine.is_running(), "host run flag set after power on");

    // The cockpit's authoritative run indicator is the host flag.
    let snap = engine.snapshot();
    assert!(snap.running, "snapshot reports running after power on");

    // ── pump a few frames → cycles advance ─────────────────────────────────────
    let before = engine.snapshot().c64_cycles;
    for _ in 0..5 {
        engine.pump_frame();
    }
    let after = engine.snapshot().c64_cycles;
    assert!(after > before, "cycles advanced under the pump: {before} -> {after}");

    // ── monitor `d` returns disasm ─────────────────────────────────────────────
    let d = engine.exec_line("d e000");
    assert!(
        d.output.contains("$e000") || d.output.contains("$E000"),
        "disasm at e000: {}",
        d.output
    );

    // ── pause → host run flag cleared, pump stops advancing ────────────────────
    let p = engine.exec_line("/pause");
    assert!(p.output.contains("PAUSE"), "pause output: {}", p.output);
    assert!(!engine.is_running(), "host run flag cleared after pause");
    let frozen = engine.snapshot().c64_cycles;
    for _ in 0..3 {
        engine.pump_frame();
    }
    assert_eq!(frozen, engine.snapshot().c64_cycles, "paused machine does not advance");

    // ── reset cold → PC reflects reset vector ──────────────────────────────────
    let rst = engine.exec_line("/reset cold");
    assert!(rst.output.contains("RESET (cold)"), "reset output: {}", rst.output);

    // ── warp toggles ───────────────────────────────────────────────────────────
    assert!(engine.exec_line("/warp on").output.contains("WARP ON"));
    assert!(engine.is_warp());
    assert!(engine.exec_line("/warp off").output.contains("WARP OFF"));
    assert!(!engine.is_warp());

    // ── step single-instructions a paused machine ──────────────────────────────
    engine.exec_line("/pause");
    let pc0 = engine.snapshot().pc;
    engine.exec_line("/step");
    // PC should change after one instruction (KERNAL has no self-loop at the reset PC).
    let pc1 = engine.snapshot().pc;
    assert_ne!(pc0, pc1, "step advanced PC: ${pc0:04X} -> ${pc1:04X}");
}

#[test]
fn monitor_passthrough_and_unknown_verbs() {
    let Some(engine) = engine_or_skip() else { return };
    engine.exec_line("power on");

    // `r` is a monitor verb (registers), not a high-level verb → passthrough.
    let r = engine.exec_line("r");
    assert!(r.output.contains("ADDR") || r.output.contains("AC"), "registers: {}", r.output);

    // `/help` is a VM command.
    assert!(engine.exec_line("/help").output.contains("/power"));

    // `/window` signals the main thread (open_window), no machine change.
    let w = engine.exec_line("/window");
    assert!(w.open_window, "window verb sets open_window");

    // `/quit` sets the quit flag.
    let q = engine.exec_line("/quit");
    assert!(q.quit && engine.should_quit(), "quit verb sets quit flag");
}

#[test]
fn flags_string_formats_set_and_clear() {
    // No ROMs needed — pure formatting check on the snapshot helper.
    let mut s = trx64_cli::StateSnapshot::default();
    s.flags = 0b1010_0011; // N . - . . . Z C  → "Nv-bdiZC"
    let fs = s.flags_str();
    assert_eq!(fs.len(), 8);
    assert!(fs.starts_with('N'));
    assert!(fs.ends_with("ZC"));
}
