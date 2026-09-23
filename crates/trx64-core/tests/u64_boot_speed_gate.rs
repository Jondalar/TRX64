//! BUG-061 — a C64 boots at 1 MHz, whatever speed the firmware prefers.
//!
//! On the Ultimate the turbo is the FIRMWARE's setting, and the firmware applies it to a
//! machine that has already come up. Measured on the owner's device, one program, one
//! boot, `$02A6` read by the program itself and two workloads back to back:
//!
//!            $02A6   work A   work B
//!     1 MHz   PAL     1.18 s   38.27 s
//!    64 MHz   PAL     1.18 s    2.00 s
//!
//! Work A is exactly as slow at 64 MHz as at 1 MHz; work B is 19x faster, in the same
//! boot. A 32-portion profile puts the changeover at 2.62 s after the program's first
//! instruction, reproducible to the hundredth of a second.
//!
//! Why it matters beyond a clock: the KERNAL decides whether it is a PAL or an NTSC
//! machine by racing the CPU against the raster (`$FF5E` — set the raster compare to line
//! 311, a line only a PAL frame has, poll `$D012` until its low byte reads 0, ask whether
//! the sticky latch fired in between). A C64 that boots at 16 MHz or faster wins that race
//! and programs CIA 1 Timer A with the NTSC latch `$4295` — 57.8 jiffies per second on a
//! PAL machine. The device never does this at any speed, five runs per step. We did, from
//! 16 MHz up.
//!
//! The model carries no timing constant. The device's ~4.5 s is one observation of one
//! firmware on one machine; what is modelled is the ARCHITECTURE — a reset drops the C64
//! to 1 MHz, and the speed takes effect when something APPLIES it.

use std::path::Path;
use trx64_core::vic::{SpeedProfile, U64SpeedTable};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const SECOND: u64 = 985_248;

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

/// The preferred speed is a SETTING, not a state of the machine: out of reset the CPU is
/// a 1 MHz 6510 until something applies it.
#[test]
fn a_reset_machine_runs_at_one_mhz_however_fast_the_firmware_prefers() {
    for mhz in [2, 8, 16, 64] {
        let mut m = u64_pal();
        m.vic.u64_regs_en = 0x01;
        m.vic.u64_speed_prefer = speed_byte(mhz);
        m.cold_reset();
        m.sync_turbo_from_vic();
        assert_eq!(
            m.vic.u64_speed().0,
            0,
            "a {mhz} MHz preference must not make a freshly reset C64 fast"
        );
        assert_eq!(m.c64_core.pending_turbo_div, 1, "and the divider agrees");
    }
}

/// Two things apply a speed, and both are somebody deciding: a program writing the
/// register, or the firmware strobing its setting.
#[test]
fn writing_d031_applies_the_speed() {
    let mut m = u64_pal();
    m.vic.u64_regs_en = 0x01;
    m.vic.u64_speed_prefer = speed_byte(16);
    m.cold_reset();
    assert_eq!(m.vic.u64_speed().0, 0, "1 MHz out of reset");

    m.vic.write_reg(0x31, speed_byte(16));
    m.sync_turbo_from_vic();
    assert!(m.c64_core.pending_turbo_div > 1, "the write engages it");
}

#[test]
fn the_firmware_strobe_applies_the_speed() {
    let mut m = u64_pal();
    m.cold_reset();
    m.set_u64_turbo(0x00, speed_byte(16)); // regs disabled: the firmware alone decides
    m.sync_turbo_from_vic();
    assert_eq!(m.vic.u64_speed().0, speed_byte(16) & 0x7f);
    assert!(m.c64_core.pending_turbo_div > 1);

    // …and a reset takes it away again, which is the whole point.
    m.cold_reset();
    m.sync_turbo_from_vic();
    assert_eq!(m.vic.u64_speed().0, 0, "the RESET line drops the C64 back to 1 MHz");
}

/// The defect itself. Red before the fix from 16 MHz up: `$02A6` = 00 and Timer A on the
/// NTSC latch. The Timer A latch is the honest instrument here — it is write-only, but the
/// counter walks down from it, so the largest value ever seen IS the latch.
#[test]
fn a_pal_machine_boots_pal_at_every_preferred_speed() {
    if !Path::new(ROM_DIR).join("kernal-901227-03.bin").exists() {
        eprintln!("skip: ROMs absent at {ROM_DIR}");
        return;
    }
    for mhz in [1, 8, 16, 64] {
        let mut m = u64_pal();
        m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
        m.run_for_full(SECOND * 5, &mut NullSink, |_, _, _, _, _, _, _| {});

        // The firmware's setting, then the reset it survives — the path the device takes
        // when its CPU-speed menu entry changes.
        m.vic.u64_regs_en = 0x01;
        m.vic.u64_speed_prefer = speed_byte(mhz);
        m.warm_reset();
        m.write_full(0x02a6, 0xa5); // sentinel: a 00 must be a decision, not cleared RAM
        m.run_for_full(SECOND * 6, &mut NullSink, |_, _, _, _, _, _, _| {});

        assert_eq!(
            m.read_full(0x02a6), 0x01,
            "{mhz} MHz preferred: the KERNAL must still see a PAL machine"
        );

        let mut latch = 0u16;
        for _ in 0..3000 {
            m.run_for_full(23, &mut NullSink, |_, _, _, _, _, _, _| {});
            let t = u16::from(m.read_full(0xdc04)) | (u16::from(m.read_full(0xdc05)) << 8);
            if t > latch {
                latch = t;
            }
        }
        assert!(
            (0x4000..=0x4025).contains(&latch),
            "{mhz} MHz preferred: Timer A should hold the PAL latch $4025, saw ${latch:04X}"
        );
    }
}
