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
