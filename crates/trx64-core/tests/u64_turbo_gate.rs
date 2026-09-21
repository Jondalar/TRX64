//! Spec 851 gate — the U64 machine profile and a CPU that is actually faster.
//!
//! Everything runs through the CPU bus. The speed is measured by what a program gets done
//! in a fixed number of PHI2 cycles, and time itself — CIA timers, the raster — must not
//! notice the CPU got faster.

use std::path::Path;
use trx64_core::vic::{SpeedProfile, U64SpeedTable};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
/// PAL: 63 cycles × 312 lines.
const FRAME: u64 = 19_656;

fn u64_machine() -> Machine {
    let mut m = Machine::new();
    m.set_machine_profile(SpeedProfile::U64);
    m
}

fn run_at(m: &mut Machine, origin: u16, port01: u8, code: &[u8], instrs: u64) {
    m.poke(origin, code);
    m.write_full(0x0001, port01);
    m.c64_core.reg_pc = origin;
    m.run_for_full_capped(instrs * 64, instrs, &mut NullSink, |_, _, _, _, _, _, _| {});
}

/// `INC $FB / BNE +2 / INC $FC / JMP $C000` for `cycles` PHI2 cycles; returns the count.
fn loop_count(m: &mut Machine, cycles: u64) -> u32 {
    m.poke(0xc000, &[0xe6, 0xfb, 0xd0, 0x02, 0xe6, 0xfc, 0x4c, 0x00, 0xc0]);
    m.poke(0x00fb, &[0, 0]);
    m.write_full(0x0001, 0x37);
    m.c64_core.reg_pc = 0xc000;
    m.run_for_full_capped(cycles, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
    u32::from(m.read_full(0x00fb)) | (u32::from(m.read_full(0x00fc)) << 8)
}

#[test]
fn d031_80_is_one_mhz_with_badline_timing_and_not_turbo() {
    let mut m = u64_machine();
    run_at(&mut m, 0xc000, 0x37, &[0xa9, 0x80, 0x8d, 0x31, 0xd0], 2); // LDA #$80 / STA $D031
    assert!(!m.turbo_engaged(), "$80 is speed index 0 with badline timing");
    assert_eq!(m.vic.u64_speed(), (0, true));
    run_at(&mut m, 0xc000, 0x37, &[0xa9, 0x04, 0x8d, 0x31, 0xd0], 2); // LDA #$04 / STA $D031
    assert!(m.turbo_engaged(), "$04 is speed index 4");
    assert_eq!(m.vic.u64_speed(), (4, false));
    assert_eq!(m.turbo_divider(), 6, "index 4 on the Elite II / C64 Ultimate table");
}

#[test]
fn the_enable_word_decides_which_registers_answer() {
    let mut m = u64_machine();
    m.set_u64_turbo(0x00, 0x80);
    for addr in [0xd030, 0xd031, 0xd0bc] {
        assert_eq!(m.read_full(addr), 0xff, "${addr:04X} is open bus without its enable bit");
    }

    m.set_u64_turbo(0x05, 0x03); // TurboEnable bit mode, preferred 4 MHz
    run_at(&mut m, 0xc000, 0x37, &[0xa9, 0x01, 0x8d, 0x30, 0xd0], 2); // LDA #$01 / STA $D030
    assert!(m.turbo_engaged(), "$D030 bit 0 engages the preferred speed");
    assert_eq!(m.vic.u64_speed().0, 3);
    run_at(&mut m, 0xc000, 0x37, &[0xa9, 0x00, 0x8d, 0x30, 0xd0], 2); // LDA #$00 / STA $D030
    assert!(!m.turbo_engaged(), "and clearing it drops to 1 MHz");

    m.set_u64_turbo(0x02, 0x80); // SuperCPU detect only
    run_at(&mut m, 0xc000, 0x37, &[0xad, 0xbc, 0xd0, 0x8d, 0x00, 0x04], 2); // LDA $D0BC / STA $0400
    assert_ne!(m.read_full(0x0400), 0xff, "$D0BC answers the SuperCPU probe");
    assert_eq!(m.read_full(0xd07c), 0xff, "but only at its own address, not a VIC mirror");

    let mut plain = Machine::new();
    run_at(&mut plain, 0xc000, 0x37, &[0xad, 0xbc, 0xd0, 0x8d, 0x00, 0x04], 2);
    assert_eq!(plain.read_full(0x0400), 0xff, "a plain C64 reads open bus there");
}

#[test]
fn a_faster_cpu_gets_proportionally_more_done_per_frame() {
    // Display off: no badlines, so the ratio is the divider.
    let base = loop_count(&mut u64_machine(), FRAME);
    let mut fast = u64_machine();
    fast.set_u64_turbo(0x00, 0x83); // preferred 4 MHz, badline timing
    let quad = loop_count(&mut fast, FRAME);
    let ratio = f64::from(quad) / f64::from(base);
    eprintln!("1 MHz: {base} loops, 4 MHz: {quad} loops, ratio {ratio:.3}");
    assert!((3.9..=4.1).contains(&ratio), "4 MHz runs four times the loop: {ratio:.3}");
}

#[test]
fn without_badline_timing_a_turbo_cpu_runs_through_the_stall() {
    let run = |prefer: u8| {
        let mut m = u64_machine();
        m.set_u64_turbo(0x00, prefer);
        m.write_full(0xd011, 0x1b); // display on: badlines steal cycles
        loop_count(&mut m, FRAME)
    };
    let with = run(0x83);
    let without = run(0x03);
    eprintln!("4 MHz with badline timing: {with}, without: {without}");
    assert!(without > with, "skipping the BA stall gets more done");
}

#[test]
fn time_is_phi2_time_at_every_speed() {
    let sample = |prefer: u8| {
        let mut m = u64_machine();
        m.set_u64_turbo(0x00, prefer);
        m.write_full(0xdc04, 0xff);
        m.write_full(0xdc05, 0xff);
        m.write_full(0xdc0e, 0x01);
        let clk0 = m.c64_core.clk;
        loop_count(&mut m, FRAME + 12_345);
        let elapsed = m.c64_core.clk - clk0;
        let timer = u64::from(m.cia1.peek(0xdc04)) | (u64::from(m.cia1.peek(0xdc05)) << 8);
        (elapsed, timer + elapsed, m.vic.raster_line)
    };
    let (slow_e, slow_t, slow_r) = sample(0x80);
    let (fast_e, fast_t, fast_r) = sample(0x89); // 16 MHz
    // A run ends on an instruction boundary, which at 1 MHz can overshoot by a few cycles
    // and at 16 MHz by less than one; everything below is measured against that clk.
    assert!(slow_e.abs_diff(fast_e) < 8, "both ran the budget: {slow_e} vs {fast_e}");
    assert_eq!(slow_t, fast_t, "CIA1 timer A counted PHI2 cycles, not CPU cycles");
    assert!(slow_r.abs_diff(fast_r) <= 1, "the raster moved with PHI2: {slow_r} vs {fast_r}");
}

#[test]
fn a_raster_irq_is_taken_at_48_mhz() {
    let mut m = u64_machine();
    m.set_u64_turbo(0x00, 0x8e); // 48 MHz on the Elite II / C64 Ultimate table
    // $C000: SEI / LDA #$00 / STA $FFFE / LDA #$C1 / STA $FFFF / LDA #$80 / STA $D012
    //        / LDA #$01 / STA $D01A / CLI / JMP $C014
    let main = [
        0x78, 0xa9, 0x00, 0x8d, 0xfe, 0xff, 0xa9, 0xc1, 0x8d, 0xff, 0xff, 0xa9, 0x80, 0x8d, 0x12, 0xd0,
        0xa9, 0x01, 0x8d, 0x1a, 0xd0, 0x58, 0x4c, 0x15, 0xc0,
    ];
    // $C100: INC $0400 / LDA #$FF / STA $D019 / RTI
    m.poke(0xc100, &[0xee, 0x00, 0x04, 0xa9, 0xff, 0x8d, 0x19, 0xd0, 0x40]);
    m.poke(0xc000, &main);
    m.write_full(0x0001, 0x35); // KERNAL out: the vectors are RAM
    m.c64_core.reg_pc = 0xc000;
    m.run_for_full_capped(2 * FRAME, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert!(m.read_full(0x0400) >= 1, "the raster IRQ was taken");
    assert!((0xc015..=0xc017).contains(&m.c64_core.reg_pc), "and RTI came back to the loop");
    assert_eq!(m.turbo_divider(), 48);
}

#[test]
fn the_ultimate_settings_survive_a_c64_reset() {
    let mut m = u64_machine();
    m.set_u64_turbo(0x05, 0x83);
    m.set_u64_speed_table(U64SpeedTable::U64);
    run_at(&mut m, 0xc000, 0x37, &[0xa9, 0x05, 0x8d, 0x31, 0xd0], 2);
    m.warm_reset();
    assert_eq!(m.speed_profile(), SpeedProfile::U64);
    assert_eq!((m.vic.u64_regs_en, m.vic.u64_speed_prefer), (0x05, 0x83), "firmware settings kept");
    assert_eq!(m.vic.u64_speed_table, U64SpeedTable::U64);
    assert!(!m.vic.u64_d031_written, "the C64-side register is reset");
}

#[test]
fn the_profile_set_before_boot_answers_the_d031_probe() {
    if !Path::new(ROM_DIR).join("kernal-901227-03.bin").exists() {
        eprintln!("SKIP: ROMs absent ({ROM_DIR})");
        return;
    }
    let mut m = Machine::new();
    m.set_machine_profile(SpeedProfile::U64);
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    // LDA $D031 / STA $0400 — the probe a release makes in its boot stub
    run_at(&mut m, 0x0200, 0x37, &[0xad, 0x31, 0xd0, 0x8d, 0x00, 0x04], 2);
    assert_ne!(m.read_full(0x0400), 0xff, "a u64 machine answers $D031 from its first instruction");
    assert_eq!(m.speed_profile(), SpeedProfile::U64, "boot kept the profile");
}

#[test]
fn the_speed_tables_match_the_firmware() {
    assert_eq!(U64SpeedTable::U64.mhz(4), 5);
    assert_eq!(U64SpeedTable::U64II.mhz(4), 6);
    assert_eq!(U64SpeedTable::U64.mhz(15), 48);
    assert_eq!(U64SpeedTable::U64II.mhz(15), 64);
    assert_eq!(U64SpeedTable::U64II.mhz(0x7f), 64, "a speed index past the table clamps");
}

// ── What a speed change costs ───────────────────────────────────
//
// Spec 868, from the UE2 session reading UPic's row loop: every picture row begins by
// writing $D031 = $80 (index 0, 1 MHz) and immediately $8F (max). Aleksi calls it a
// resync and built it to cost almost nothing — the row's cycle budget has about 240
// turbo cycles of slack in 4032, so the four PHI2 cycles (256 at this divider) that 851
// charged did not fit: the row ran past its raster line, and from then on the program
// painted one picture row per two lines.
//
// This measures the charge with no program and no host involved: four instructions, one
// PHI2 delta.

/// The resync pair, measured — and under the current model it costs **nothing**.
///
/// 851 applied a `$D031` write from the next INSTRUCTION, chosen as a convenience with no
/// source behind it, and that charged this pair four PHI2 cycles — ~256 turbo-cycle
/// equivalents for two stores. 868 §9 replaced the model: the divider is adopted at the
/// next PHI2 EDGE, so both stores land inside one cycle and the CPU never runs slowly,
/// which is what Aleksi built the pair to do.
///
/// **How that was settled, with no hardware timing measurement available.** The charge is
/// the last link in a chain that ends in a missing half of a picture, and it took three
/// attempts to measure because the first two instruments changed the thing they measured:
///
/// - a hand count of the row loop said the row overruns its line — arithmetic, not a
///   measurement;
/// - an access watch that returned `true` from `on_access` halted the run on every hit,
///   and with two `$D012` reads per row that is a halt every few cycles. It reported 63
///   PHI2 per row and consecutive raster lines. The probe was measuring itself;
/// - a watch armed the same way but returning `false` — observe, never halt — reported
///   **126 PHI2 between row-loop reads, 593 of 600 samples**: two raster lines per picture
///   row, with the host's canvas agreeing independently at 132 rows carrying colour.
///
/// Then the arbiter, both models in one binary so the model was the only variable
/// (UE2 session, 2026-09-21): **63 PHI2 per row, 594 of 600 samples, 256 of 272 canvas
/// rows**, and the run-length signature that says the sub-PHI2 phase is still realigned
/// per row held — 62% of colour runs at three pixels or shorter, against 59% before. A
/// program written against the real machine renders correctly under this model and half a
/// picture under the other; that is the strongest evidence available to us, and it is what
/// took the number below from 4 to 0.
///
/// So: if you change the model, this test fails, and it should. Update the number
/// deliberately and say what settled it.
#[test]
fn the_speed_change_pair_costs_nothing() {
    let mut m = u64_machine();
    m.set_u64_speed_table(U64SpeedTable::U64II);
    m.vic.u64_regs_en = 0x01;

    m.vic.write_reg(0x31, 0x8f);
    m.poke(0xc000, &[0xea, 0xea]);
    m.c64_core.reg_pc = 0xc000;
    m.run_for_full_capped(64 * 4, 2, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert!(m.c64_core.turbo_div > 1, "the machine must be in turbo to measure this");

    // lda #$80 / ldx #$8f / sta $d031 / stx $d031 — UPic's resync, verbatim.
    m.poke(0xc100, &[0xa9, 0x80, 0xa2, 0x8f, 0x8d, 0x31, 0xd0, 0x8e, 0x31, 0xd0]);
    m.c64_core.reg_pc = 0xc100;

    let before = m.clk;
    m.run_for_full_capped(64 * 64, 4, &mut NullSink, |_, _, _, _, _, _, _| {});
    let phi2 = m.clk - before;

    assert_eq!(
        phi2, 0,
        "the divider is adopted at the PHI2 edge, so the pair stays inside one cycle"
    );
    assert!(m.c64_core.turbo_div > 1, "and the machine is still at full speed");
}
