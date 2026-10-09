//! C64RE #49 — an exec breakpoint on the FIRST instruction of an interrupt handler.
//!
//! The 6510 core folded the 7-cycle IRQ/NMI entry and the handler's first opcode into one
//! step, so the vector target (KERNAL `$FF48` PHA for the IRQ, `$FE43` SEI for the NMI) was
//! never an instruction boundary and a breakpoint there never fired, while `$FF49` did. A
//! debugger must be able to stop with PC = the target, after the entry and before the first
//! opcode executes. With nothing armed on the target the entry and the first opcode stay one
//! step, cycle-identical (`unarmed_runs_are_unchanged`).
//!
//! Booted machine, ROMs from `TRX64_ROM_DIR` or the sibling C64RE checkout.

use std::collections::HashSet;
use std::path::PathBuf;
use trx64_core::{Machine, NullSink, RunStop};

const FRAME: u64 = 19_656;
const ORG: u16 = 0xc100;

fn booted() -> Option<Machine> {
    let dir = std::env::var_os("TRX64_ROM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms")));
    if !dir.join("kernal-901227-03.bin").exists() {
        eprintln!("skip: ROMs absent at {}", dir.display());
        return None;
    }
    let mut m = Machine::new();
    m.boot_from_dir(&dir).expect("boot ROMs");
    m.run_for_full(3_000_000, &mut NullSink, |_, _, _, _, _, _, _| {});
    Some(m)
}

fn nop(_: u16, _: u8, _: u8, _: u8, _: u8, _: u8, _: u64) {}

/// Run until a breakpoint in `bps` stops it (or `frames` pass).
fn run_to(m: &mut Machine, bps: &HashSet<u16>, frames: u64) -> RunStop {
    m.run_for_full_capped_dbg(frames * FRAME, u64::MAX, Some(bps), None, None, &mut NullSink, nop)
}

/// CIA `cia` ($DC00/$DD00) Timer A interrupts every $1000 cycles; main loop spins at ORG.
/// The CIA1 case is taken through the ROM vector $FFFE→$FF48, the CIA2 case $FFFA→$FE43.
fn arm_timer(m: &mut Machine, cia: u16) {
    let icr = cia + 0x0d;
    let code = [
        0x78, // SEI
        0xa9, 0x7f, 0x8d, icr as u8, (icr >> 8) as u8, // mask all
        0xad, icr as u8, (icr >> 8) as u8, // acknowledge
        0xa9, 0x00, 0x8d, (cia + 4) as u8, (cia >> 8) as u8, // TA lo
        0xa9, 0x10, 0x8d, (cia + 5) as u8, (cia >> 8) as u8, // TA hi = $1000
        0xa9, 0x81, 0x8d, icr as u8, (icr >> 8) as u8, // TA interrupt on
        0xa9, 0x11, 0x8d, (cia + 0x0e) as u8, (cia >> 8) as u8, // load + start
        0x58, // CLI
        0x4c, 0x1e + ORG as u8, (ORG >> 8) as u8, // spin: JMP spin
    ];
    m.poke(ORG, &code);
    m.c64_core.reg_pc = ORG;
}

fn stop_at_entry(cia: u16, target: u16) {
    let Some(mut m) = booted() else { return };
    arm_timer(&mut m, cia);
    let bps: HashSet<u16> = [target].into();
    let sp0 = m.c64_core.reg_sp;
    let stop = run_to(&mut m, &bps, 4);
    assert_eq!(stop, RunStop::Breakpoint(target), "the run must stop AT ${target:04X}");
    assert_eq!(m.c64_core.reg_pc, target);
    // Entry pushed PCH, PCL, P (SP-3 from the spinning main loop); the first opcode has not run.
    assert_eq!(m.c64_core.reg_sp, sp0.wrapping_sub(3), "SP after the 7-cycle entry only");
    // Resume past the breakpoint: exactly the handler's first opcode runs (PHA / SEI).
    let mut null = NullSink;
    m.run_for_full_capped(999_999, 1, &mut null, nop);
    assert_eq!(m.c64_core.reg_pc, target + 1, "one step executes the first opcode");
    if target == 0xff48 {
        assert_eq!(m.c64_core.reg_sp, sp0.wrapping_sub(4), "PHA pushed A");
    }
}

#[test]
fn irq_vector_target_breakpoint_fires() {
    stop_at_entry(0xdc00, 0xff48);
}

#[test]
fn nmi_vector_target_breakpoint_fires() {
    stop_at_entry(0xdd00, 0xfe43);
}

/// The instruction after the target still fires (it always did).
#[test]
fn irq_second_instruction_breakpoint_still_fires() {
    let Some(mut m) = booted() else { return };
    arm_timer(&mut m, 0xdc00);
    let bps: HashSet<u16> = [0xff49].into();
    assert_eq!(run_to(&mut m, &bps, 4), RunStop::Breakpoint(0xff49));
}

/// Stopping at the entry and stepping over it changes nothing: the machine ends a fixed
/// cycle budget later in the exact state of a run that never stopped. And a breakpoint
/// elsewhere (never hit) leaves the entry folded, cycle-identical to no breakpoint at all.
#[test]
fn unarmed_runs_are_unchanged() {
    fn snap(m: &Machine) -> (u64, u16, [u8; 4], Vec<u8>) {
        let c = &m.c64_core;
        (m.clk, c.reg_pc, [c.reg_a, c.reg_x, c.reg_y, c.reg_sp], (0..=0xffffu32).map(|a| m.read_full(a as u16)).collect())
    }
    let Some(mut plain) = booted() else { return };
    let mut other = booted().unwrap();
    let mut stepped = booted().unwrap();
    for m in [&mut plain, &mut other, &mut stepped] {
        arm_timer(m, 0xdc00);
    }
    let budget = 6 * FRAME;
    plain.run_for_full(budget, &mut NullSink, nop);
    // Breakpoint armed on an address that is never executed.
    let never: HashSet<u16> = [0x1234].into();
    other.run_for_full_capped_dbg(budget, u64::MAX, Some(&never), None, None, &mut NullSink, nop);
    // Stop at every IRQ entry and step over it.
    let bps: HashSet<u16> = [0xff48].into();
    let start = stepped.clk;
    let mut stops = 0;
    loop {
        let left = budget.saturating_sub(stepped.clk - start);
        if left == 0 {
            break;
        }
        if let RunStop::Breakpoint(_) = stepped.run_for_full_capped_dbg(left, u64::MAX, Some(&bps), None, None, &mut NullSink, nop) {
            stops += 1;
            stepped.run_for_full_capped(999_999, 1, &mut NullSink, nop);
        }
    }
    assert!(stops > 5, "the IRQ entry was hit {stops} times");
    let p = snap(&plain);
    assert!(p == snap(&other), "an unrelated breakpoint must not change the machine");
    assert!(p == snap(&stepped), "stop-at-entry + step must land in the same state");
}

fn full_snap(m: &Machine) -> (u64, u16, [u8; 4], Vec<u8>) {
    let c = &m.c64_core;
    (m.clk, c.reg_pc, [c.reg_a, c.reg_x, c.reg_y, c.reg_sp], (0..=0xffffu32).map(|a| m.read_full(a as u16)).collect())
}

/// A checkpoint taken at the entry stop carries it: restoring and running N cycles lands in
/// exactly the state of resuming the uninterrupted machine for the same N. Without the flag
/// in the checkpoint the restore would run the interrupt dispatch again before the first opcode.
fn checkpoint_at_entry(cia: u16, target: u16) {
    use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, restore_runtime_checkpoint};
    let Some(mut m) = booted() else { return };
    arm_timer(&mut m, cia);
    let bps: HashSet<u16> = [target].into();
    assert_eq!(run_to(&mut m, &bps, 4), RunStop::Breakpoint(target));
    assert!(m.c64_core.entry_paused);
    let cp = capture_runtime_checkpoint(&m, "", "d64", None, None, None, None);
    assert_eq!(cp["cpu"]["entryPaused"], serde_json::json!(true), "the checkpoint records the entry stop");

    let go = |m: &mut Machine| {
        m.run_for_full_capped(999_999, 1, &mut NullSink, nop);
        m.run_for_full(3 * FRAME, &mut NullSink, nop);
    };
    go(&mut m);
    let reference = full_snap(&m);
    assert!(!m.c64_core.entry_paused);

    restore_runtime_checkpoint(&mut m, &cp).expect("restore");
    assert!(m.c64_core.entry_paused, "restore brings the entry stop back");
    assert_eq!(m.c64_core.reg_pc, target);
    go(&mut m);
    assert!(full_snap(&m) == reference, "restore at the entry stop must equal the uninterrupted run");

    // An older checkpoint (no field) reads as not paused, and a restore clears a live flag.
    let mut old = cp.clone();
    old["cpu"].as_object_mut().unwrap().remove("entryPaused");
    let mut m2 = booted().unwrap();
    arm_timer(&mut m2, cia);
    assert_eq!(run_to(&mut m2, &bps, 4), RunStop::Breakpoint(target));
    restore_runtime_checkpoint(&mut m2, &old).expect("restore old");
    assert!(!m2.c64_core.entry_paused);
}

#[test]
fn irq_checkpoint_at_entry_restores_exactly() {
    checkpoint_at_entry(0xdc00, 0xff48);
}

#[test]
fn nmi_checkpoint_at_entry_restores_exactly() {
    checkpoint_at_entry(0xdd00, 0xfe43);
}
