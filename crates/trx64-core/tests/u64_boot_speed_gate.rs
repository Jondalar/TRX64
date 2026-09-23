//! BUG-061 — the Ultimate holds its C64 at 1 MHz for 2.06 s after a reset.
//!
//! Measured on the owner's C64 Ultimate (firmware 3.15):
//!
//!   - after `machine:reset` the machine stays at 1 MHz for 2.06 s, at 16 MHz and at
//!     64 MHz alike, stable over three runs each;
//!   - the firmware strobes the speed 445 cycles after reset release (`C64::reset()` →
//!     `effectuate_settings()` → `setCpuSpeed`), so a strobe inside the hold has no
//!     effect, and when the hold ends the last strobed speed applies;
//!   - a speed change WITHOUT a reset takes effect at once.
//!
//! The KERNAL decides PAL or NTSC by racing the CPU against the raster at `$FF5E`, about
//! 1.5 s after reset — inside the hold. Without the hold a turbo C64 wins that race from
//! 16 MHz up and CIA 1 Timer A gets the NTSC latch: 57.8 jiffies per second on a PAL
//! machine. The device detects PAL at every speed, five runs per step.

use std::path::Path;
use trx64_core::vic::{SpeedProfile, U64SpeedTable};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const SECOND: u64 = 985_248;
/// What the firmware does right after reset release.
const STROBE_AFTER_RELEASE: u64 = 445;

fn u64_pal() -> Machine {
    let mut m = Machine::new();
    m.set_machine_profile(SpeedProfile::U64);
    m.set_u64_speed_table(U64SpeedTable::U64II);
    m
}

fn speed_byte(mhz: u32) -> u8 {
    let idx = (0..16u8).find(|&i| U64SpeedTable::U64II.mhz(i) == mhz).expect("in table");
    0x80 | idx // bit 7 = badline timing on, as the menu writes it
}

fn run(m: &mut Machine, cycles: u64) {
    m.run_for_full(cycles, &mut NullSink, |_, _, _, _, _, _, _| {});
}

/// The firmware's order: reset, then the strobe 445 cycles later.
fn reset_like_the_firmware(m: &mut Machine, mhz: u32) {
    m.warm_reset();
    run(m, STROBE_AFTER_RELEASE);
    m.set_u64_turbo(0x00, speed_byte(mhz));
}

#[test]
fn a_strobe_right_after_reset_does_not_speed_the_c64_up() {
    for mhz in [16, 64] {
        let mut m = u64_pal();
        reset_like_the_firmware(&mut m, mhz);
        m.sync_turbo_from_vic();
        assert_eq!(m.vic.u64_speed().0, 0, "{mhz} MHz strobed inside the hold: still 1 MHz");
        run(&mut m, SECOND); // a second in: still inside
        assert_eq!(m.c64_core.turbo_div, 1, "{mhz} MHz: 1 s after reset the CPU is still slow");
    }
}

#[test]
fn when_the_hold_ends_the_last_strobe_applies_without_another() {
    let mut m = u64_pal();
    reset_like_the_firmware(&mut m, 64);
    run(&mut m, SECOND * 2 + SECOND / 10); // 2.1 s: past the 2.06 s hold
    assert!(m.c64_core.turbo_div > 1, "the strobe made 445 cycles after release now applies");
}

#[test]
fn without_a_reset_a_strobe_applies_at_once() {
    let mut m = u64_pal();
    reset_like_the_firmware(&mut m, 1);
    run(&mut m, SECOND * 3); // well past the hold, running at 1 MHz
    m.set_u64_turbo(0x00, speed_byte(64));
    run(&mut m, 64);
    assert!(m.c64_core.turbo_div > 1, "a change while running is not held");
}

/// A machine that was never reset is not held — the hold belongs to the reset, not to
/// the profile.
#[test]
fn no_reset_no_hold() {
    let mut m = u64_pal();
    m.vic.u64_regs_en = 0x01;
    m.vic.write_reg(0x31, speed_byte(16));
    m.sync_turbo_from_vic();
    assert!(m.c64_core.pending_turbo_div > 1);
}

/// The defect itself, driven the way the firmware drives it. Red without the hold from
/// 16 MHz up: `$02A6` = 00 and Timer A on the NTSC latch. The latch is write-only; the
/// counter walks down from it, so its high-water mark IS the latch.
#[test]
fn a_pal_machine_boots_pal_at_every_speed() {
    if !Path::new(ROM_DIR).join("kernal-901227-03.bin").exists() {
        eprintln!("skip: ROMs absent at {ROM_DIR}");
        return;
    }
    for mhz in [1, 8, 16, 64] {
        let mut m = u64_pal();
        m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
        run(&mut m, SECOND * 5);

        reset_like_the_firmware(&mut m, mhz);
        m.write_full(0x02a6, 0xa5); // sentinel: a 00 must be a decision, not cleared RAM
        run(&mut m, SECOND * 6);

        assert_eq!(m.read_full(0x02a6), 0x01, "{mhz} MHz: the KERNAL must see a PAL machine");
        let mut latch = 0u16;
        for _ in 0..3000 {
            run(&mut m, 23);
            let t = u16::from(m.read_full(0xdc04)) | (u16::from(m.read_full(0xdc05)) << 8);
            latch = latch.max(t);
        }
        assert!(
            (0x4000..=0x4025).contains(&latch),
            "{mhz} MHz: Timer A should hold the PAL latch $4025, saw ${latch:04X}"
        );
        if mhz > 1 {
            assert!(m.c64_core.turbo_div > 1, "{mhz} MHz: and after the hold it IS fast");
        }
    }
}
