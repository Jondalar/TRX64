//! Spec 868 — a colour per pixel, under a CPU fast enough to write one.
//!
//! Aleksi Eeben's UPic blanks the display and writes 384 values to `$D020` per raster
//! line, one per VIC pixel — eight CPU cycles per pixel, which fits at 64 MHz and cannot
//! exist on a 1 MHz 6510. Our VIC sampled the register once per PHI2 cycle, so the
//! picture arrived at one eighth of its horizontal detail.
//!
//! What this gate protects is not the feature so much as its BLAST RADIUS: the colour
//! resolve is on the path of every pixel of every frame of every game, so the override
//! must be unreachable unless a U64 is in turbo. The last two tests are the ones that
//! matter most.

use trx64_core::vic::{SubCycleColour, VicII};
use trx64_core::vic::{SpeedProfile, U64SpeedTable};
use trx64_core::NullSink;

/// §8.1 — the mapping from sub-cycle phase to pixel, with no slack in it.
#[test]
fn a_phase_names_the_pixel_it_belongs_to() {
    // 64 MHz: 64 CPU cycles per PHI2 cycle, 8 per pixel. UPic spends exactly 8 per pixel.
    assert_eq!(SubCycleColour::pixel_for(0, 64), 0);
    assert_eq!(SubCycleColour::pixel_for(7, 64), 0, "still inside the first pixel");
    assert_eq!(SubCycleColour::pixel_for(8, 64), 1, "the ninth cycle is the second pixel");
    assert_eq!(SubCycleColour::pixel_for(56, 64), 7);
    assert_eq!(SubCycleColour::pixel_for(63, 64), 7, "the last phase is the last pixel");

    // A divider of 8 gives one CPU cycle per pixel — the coarsest turbo that still
    // resolves per pixel at all.
    for phase in 0..8u32 {
        assert_eq!(SubCycleColour::pixel_for(phase, 8), phase as usize);
    }

    // Two: four pixels per CPU cycle, so a store covers pixel 0 or pixel 4 onwards.
    assert_eq!(SubCycleColour::pixel_for(0, 2), 0);
    assert_eq!(SubCycleColour::pixel_for(1, 2), 4);

    // No turbo: there is no sub-cycle position to speak of.
    assert_eq!(SubCycleColour::pixel_for(0, 1), 0);
    assert_eq!(SubCycleColour::pixel_for(9, 1), 0);
}

/// §8.2 — an unwritten slot is not "no colour", it is the previous one. A register holds
/// its value until something replaces it, and the slots have to say the same.
#[test]
fn a_store_holds_until_the_next_one() {
    let mut sc = SubCycleColour { reg: 0x20, slots: [0x0a; 8] };
    sc.set_from(3, 0x0b);
    assert_eq!(sc.slots, [0x0a, 0x0a, 0x0a, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b]);
    sc.set_from(6, 0x0c);
    assert_eq!(sc.slots, [0x0a, 0x0a, 0x0a, 0x0b, 0x0b, 0x0b, 0x0c, 0x0c]);
    // The last write wins for the rest of the cycle, including a write at pixel 0.
    sc.set_from(0, 0x01);
    assert_eq!(sc.slots, [0x01; 8]);
}

/// Eight stores in one cycle produce eight different pixels — the thing the single latch
/// could not hold, stated directly.
#[test]
fn eight_stores_in_one_cycle_are_eight_pixels() {
    let mut vic = VicII::new();
    vic.speed_profile = SpeedProfile::U64;
    vic.turbo_div = 64;

    for pixel in 0..8u32 {
        vic.turbo_phase = pixel * 8; // UPic's eight cycles per pixel
        vic.write_reg(0x20, (pixel as u8) + 1);
    }

    let sc = vic.subcycle_colour.expect("a turbo U64 writing $D020 arms the slots");
    assert_eq!(sc.reg, 0x20);
    assert_eq!(sc.slots, [1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(
        vic.colour_register(0x20), 8,
        "§8.4 — the register itself still holds the LAST write, for the monitor, \
         a snapshot, and the next cycle"
    );
}

/// §8.7 — turbo off, path off. This is the one that keeps every C64 byte-identical: the
/// override cannot be reached, so the colour resolve is the code it has always been.
#[test]
fn a_one_mhz_machine_never_arms_it() {
    let mut vic = VicII::new();
    vic.speed_profile = SpeedProfile::C64;
    vic.turbo_div = 1;
    vic.turbo_phase = 0;

    for v in 0..8u8 {
        vic.write_reg(0x20, v);
    }
    assert!(vic.subcycle_colour.is_none(), "a 6510 cannot write twice in a cycle");
    assert_eq!(vic.colour_register(0x20), 7);
}

/// The gate is on the profile AND the divider, and both halves are load-bearing: a U64
/// at 1 MHz is an ordinary C64 for this purpose, and a turbo divider on a C64 profile is
/// a machine that does not exist.
#[test]
fn both_halves_of_the_gate_are_needed() {
    let mut u64_at_1mhz = VicII::new();
    u64_at_1mhz.speed_profile = SpeedProfile::U64;
    u64_at_1mhz.turbo_div = 1;
    u64_at_1mhz.write_reg(0x20, 0x05);
    assert!(u64_at_1mhz.subcycle_colour.is_none(), "a U64 not in turbo draws like a C64");

    let mut c64_with_divider = VicII::new();
    c64_with_divider.speed_profile = SpeedProfile::C64;
    c64_with_divider.turbo_div = 64;
    c64_with_divider.write_reg(0x20, 0x05);
    assert!(
        c64_with_divider.subcycle_colour.is_none(),
        "the profile decides, so a stray divider cannot reach the resolve"
    );
}

/// Only the colour registers. `$D011`/`$D016`/`$D018` change what the display logic DOES,
/// and a sub-cycle write to one of those would need the whole draw sequence at sub-cycle
/// granularity — a different and much larger machine. The boundary is in the code as a
/// comment; this is it as a test.
#[test]
fn only_the_border_colour_is_sub_cycle_today() {
    let mut vic = VicII::new();
    vic.speed_profile = SpeedProfile::U64;
    vic.turbo_div = 64;

    vic.turbo_phase = 32;
    vic.write_reg(0x11, 0x1b); // display control
    vic.write_reg(0x16, 0x08);
    vic.write_reg(0x18, 0x14);
    vic.write_reg(0x21, 0x06); // background — §6 says second, not never
    assert!(
        vic.subcycle_colour.is_none(),
        "nothing but $D020 arms the slots in this release"
    );

    vic.write_reg(0x20, 0x02);
    assert!(vic.subcycle_colour.is_some());
}

// ── The resync ───────────────────────────────────────────────────────
//
// UPic's row loop writes $D031 = $80 (index 0, 1 MHz) and immediately $8F (max) at the
// top of every picture row, and Aleksi's own comment calls it a resync. It is not
// decoration: a technique built on one store per pixel needs the CPU's sub-cycle counter
// to start a row at a known place, or the 384 stores walk relative to the pixel clock and
// the picture shears.
//
// So the WRITE reloads the divider's counter — the phase restarts at the store — while the
// divider itself is adopted at the next PHI2 EDGE, which is why the pair costs nothing.
// The two halves stand or fall together and were measured together, on real U64 firmware
// with both models in one binary and the model the only variable (UE2 session,
// 2026-09-21): row period 126 → 63 PHI2, canvas 132 → 256 of 272 rows, and the run-length
// signature that only holds while the phase is realigned per row held — 62% of colour runs
// at three pixels or shorter, against 59%.
//
// The first version of this reset keyed on "the divider reached 1", which is arithmetic
// (at 1 MHz a CPU cycle IS a PHI2 cycle, so the phase there is zero by definition) but
// which cannot fire at all once a speed change is adopted at the edge: the machine never
// observes divider 1 at an instruction boundary. Keying on the write is what survived.

/// A U64 actually running at the top of its speed table, divider adopted.
fn u64_in_turbo() -> trx64_core::Machine {
    let mut m = trx64_core::Machine::new();
    m.set_machine_profile(SpeedProfile::U64);
    m.set_u64_speed_table(U64SpeedTable::U64II);
    m.vic.u64_regs_en = 0x01;
    m.vic.write_reg(0x31, 0x8f);
    m.poke(0xc000, &[0xea, 0xea]);
    m.c64_core.reg_pc = 0xc000;
    m.run_for_full_capped(64 * 4, 2, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert!(m.c64_core.turbo_div > 1, "these tests measure a machine that is in turbo");
    m
}

/// The phase a store is placed by must restart when the speed is written, or every row
/// after the first paints its pixels 1..63 cycles out of place.
#[test]
fn a_speed_change_restarts_the_sub_cycle_counter() {
    let mut m = u64_in_turbo();
    let fast = m.c64_core.turbo_div;
    // Leave the phase in the middle of a cycle, as a row's worth of stores would.
    m.c64_core.turbo_phase = 37;

    m.vic.write_reg(0x31, 0x80); // the resync: index 0 …
    m.sync_turbo_from_vic();
    assert_eq!(m.c64_core.turbo_phase, 0, "the write reloads the divider's counter");
    assert_eq!(m.c64_core.pending_turbo_div, 1, "index 0 is 1 MHz");
    assert_eq!(
        m.c64_core.turbo_div, fast,
        "… and it is not in force yet: the divider changes at the next PHI2 edge, which is \
         why both stores of the pair land inside one cycle and cost nothing"
    );

    m.vic.write_reg(0x31, 0x8f); // … and straight back to max
    m.sync_turbo_from_vic();
    assert_eq!(m.c64_core.pending_turbo_div, fast, "back to full speed");
    assert_eq!(
        m.c64_core.turbo_phase, 0,
        "and the row starts from a known place, which is what the resync buys"
    );
}

/// The reload keys on the WRITE, not on the value, so 64 → 16 restarts the counter exactly
/// as index 0 does. That case has not been measured on its own and this records it as a
/// CONSEQUENCE, not a finding — the alternative is a second mechanism, a reload that
/// happens for one written value and not another, with no evidence behind it and a divider
/// that would have to be built strangely to behave that way.
#[test]
fn a_change_between_two_turbo_speeds_restarts_it_too() {
    let mut m = u64_in_turbo();
    m.c64_core.turbo_phase = 21;

    m.vic.write_reg(0x31, 0x8a); // a slower turbo, still above 1 MHz
    m.sync_turbo_from_vic();
    assert!(m.c64_core.pending_turbo_div > 1, "still a turbo speed");
    assert_eq!(m.c64_core.turbo_phase, 0, "the counter is reloaded by the store");
}

/// Including a write of the value already in force: a register reload does not compare
/// first. Stated as a test because this is exactly where the next reader will assume a
/// no-op and add one.
#[test]
fn rewriting_the_same_speed_restarts_it_as_well() {
    let mut m = u64_in_turbo();
    m.c64_core.turbo_phase = 12;

    m.vic.write_reg(0x31, 0x8f);
    m.sync_turbo_from_vic();
    assert_eq!(m.c64_core.turbo_phase, 0, "the store reloads the counter, value or not");
}
