//! The RESTORE key drives the C64's /NMI line.
//!
//! On the real machine RESTORE is not in the keyboard matrix: it pulls /NMI, and the
//! 6510 takes one NMI per falling edge. VICE models a press as a pulse: the keyboard's NMI
//! source is set and cleared at once (`c64keyboard_restore_key`), the release is ignored.
//! So one press is one NMI, however long the key is held, and a held key does not hold the
//! line: another NMI source (CIA2) still gets its own edges.
//!
//! A scenario holds the key through `session/key_down "RESTORE"` (now or at a cycle) and
//! lets it go with `session/key_up`: the same keyboard held set these cases drive.
//!
//! No ROMs: the KERNAL is banked out and the NMI vector at `$FFFA` points into RAM.

use trx64_core::c64_6510core::{IK_NMI, INT_SRC_RESTORE};
use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, restore_runtime_checkpoint};
use trx64_core::{Machine, NullSink};

const MAIN: u16 = 0xc000;
const HANDLER: u16 = 0xc100;
const COUNTER: u16 = 0x0400;

/// `$C000`: LDA #<HANDLER / STA $FFFA / LDA #>HANDLER / STA $FFFB / JMP * (at `$C00A`).
/// `$C100`: INC $0400 / LDA $DD0D (acks CIA2) / RTI.
fn machine_waiting_for_nmi() -> Machine {
    let mut m = Machine::new();
    let main = [
        0xa9, HANDLER as u8, 0x8d, 0xfa, 0xff, 0xa9, (HANDLER >> 8) as u8, 0x8d, 0xfb, 0xff, 0x4c, 0x0a, 0xc0,
    ];
    m.poke(MAIN, &main);
    m.poke(HANDLER, &[0xee, COUNTER as u8, (COUNTER >> 8) as u8, 0xad, 0x0d, 0xdd, 0x40]);
    m.poke(COUNTER, &[0]);
    m.write_full(0x0001, 0x35); // KERNAL out: the vectors are RAM
    m.c64_core.reg_pc = MAIN;
    run(&mut m, 200);
    assert_eq!(m.read_full(COUNTER), 0, "no NMI before the key is pressed");
    m
}

fn run(m: &mut Machine, cycles: u64) {
    m.run_for_full(cycles, &mut NullSink, |_, _, _, _, _, _, _| {});
}

fn frame(m: &Machine) -> u64 {
    m.timing().cycles_per_frame
}

#[test]
fn a_held_restore_key_is_one_nmi() {
    let mut m = machine_waiting_for_nmi();
    let f = frame(&m);

    m.keyboard.key_down("RESTORE");
    run(&mut m, f);
    assert_eq!(m.read_full(COUNTER), 1, "the handler ran within a frame of the press");

    run(&mut m, 5 * f);
    assert_eq!(m.read_full(COUNTER), 1, "held for six frames: still one NMI");
    assert_eq!(m.c64_int.pending_int[INT_SRC_RESTORE] & IK_NMI, 0, "a press is a pulse: the source is not held");

    m.keyboard.key_up("RESTORE");
    run(&mut m, f);
    assert_eq!(m.read_full(COUNTER), 1, "the release is no NMI");
    assert_eq!(m.c64_int.pending_int[INT_SRC_RESTORE] & IK_NMI, 0, "nothing is held after the release");

    m.keyboard.key_down("RESTORE");
    run(&mut m, f);
    assert_eq!(m.read_full(COUNTER), 2, "a second press is a second NMI");
    m.keyboard.release_keys();
    run(&mut m, f);
    assert_eq!(m.c64_int.pending_int[INT_SRC_RESTORE] & IK_NMI, 0, "release_keys leaves no source held");
}

#[test]
fn a_restore_press_queued_for_a_cycle_raises_the_nmi_at_that_cycle() {
    let mut m = machine_waiting_for_nmi();
    let f = frame(&m);
    let press = m.clk + 1000;

    // What a scenario sends: key_down at a cycle, key_up three frames later.
    m.keyboard.schedule_key(press, "RESTORE", true);
    m.keyboard.schedule_key(press + 3 * f, "RESTORE", false);

    run(&mut m, 900);
    assert_eq!(m.read_full(COUNTER), 0, "not before its cycle");

    run(&mut m, f);
    assert_eq!(m.read_full(COUNTER), 1, "taken at its cycle");

    run(&mut m, 4 * f);
    assert_eq!(m.read_full(COUNTER), 1, "held three frames: one NMI");
    assert_eq!(m.c64_int.pending_int[INT_SRC_RESTORE] & IK_NMI, 0, "a pulse, nothing held");
}

/// CIA2 timer A, continuous, NMI on underflow, period `$0400` cycles.
fn start_cia2_nmi(m: &mut Machine) {
    m.write_full(0xdd0d, 0x7f);
    m.write_full(0xdd04, 0x00);
    m.write_full(0xdd05, 0x04);
    m.write_full(0xdd0d, 0x81);
    m.write_full(0xdd0e, 0x11);
}

#[test]
fn a_held_restore_key_does_not_block_a_cia2_nmi() {
    let mut m = machine_waiting_for_nmi();
    let f = frame(&m);

    m.keyboard.key_down("RESTORE");
    run(&mut m, f);
    assert_eq!(m.read_full(COUNTER), 1, "the press is one NMI");

    // RESTORE still held: CIA2 underflows keep raising their own edges.
    start_cia2_nmi(&mut m);
    run(&mut m, 2 * f);
    let n = m.read_full(COUNTER);
    assert!(n >= 10, "CIA2 NMIs got through while RESTORE was held: counter {n}");
}

#[test]
fn a_checkpoint_taken_with_restore_held_restores_without_a_phantom_nmi() {
    let mut m = machine_waiting_for_nmi();
    let f = frame(&m);

    m.keyboard.key_down("RESTORE");
    run(&mut m, f);
    assert_eq!(m.read_full(COUNTER), 1);

    let cp = capture_runtime_checkpoint(&m, "", "", None, None, None, None);
    let mut back = Machine::new();
    restore_runtime_checkpoint(&mut back, &cp).expect("restore");
    assert_eq!(back.keyboard.pressed_keys(), vec!["RESTORE".to_string()], "the held key travels");

    run(&mut back, 3 * f);
    assert_eq!(back.read_full(COUNTER), 1, "still held after the restore: no second NMI");

    back.keyboard.key_up("RESTORE");
    run(&mut back, 1000);
    back.keyboard.key_down("RESTORE");
    run(&mut back, f);
    assert_eq!(back.read_full(COUNTER), 2, "a fresh press after the restore is one NMI");
}
