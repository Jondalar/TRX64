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
use trx64_core::vic::SpeedProfile;

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
