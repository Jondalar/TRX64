//! CIA time-of-day gate — the clock that never ran.
//!
//! `Cia::tick` used to bump a prescaler nothing read, so TOD stood still while its
//! registers round-tripped and looked alive. Invisible on a stock C64, where little
//! software reads TOD; fatal on the `u64` profile, where the timers and the raster are
//! CPU-clocked and TOD is the only thing left carrying real time. Reported by the UE2
//! session against five of Xander Mol's Ultimate projects — mandelbrot-upic sits in a TOD
//! wait loop for ever with turbo on.
//!
//! Ported from VICE `ciacore.c`. The mechanics are driven at the chip, the integration
//! through the CPU bus.

use trx64_core::cia::{
    Cia, CIAT_TABLEN, CIA_CRA_TODIN_50HZ, CIA_CRB_ALARM, CIA_ICR, CIA_IM_TOD, CIA_TOD_HR,
    CIA_TOD_MIN, CIA_TOD_SEC, CIA_TOD_TEN, PAL_CYCLES_PER_SEC,
};
use trx64_core::{Machine, NullSink};

/// TOD does not consult the timer transition table, so an empty one keeps this gate on
/// the clock under test instead of widening a module's visibility for a test's sake.
/// PAL mains is 50 Hz — always, whatever CRA says. A tenth therefore costs five mains
/// ticks when software picks the matching divider, and SIX when it picks the 60 Hz one,
/// which is why TOD then runs at 5/6. Deriving the frequency from CRA hid that, and these
/// two constants are what the difference looks like.
const TENTH_50HZ_DIVIDER: u32 = (PAL_CYCLES_PER_SEC / 50) * 5;
const TENTH_60HZ_DIVIDER: u32 = (PAL_CYCLES_PER_SEC / 50) * 6;

fn tab() -> Box<[u16; CIAT_TABLEN]> {
    Box::new([0u16; CIAT_TABLEN])
}

/// Run the chip for `cycles` PHI2 cycles.
fn run(cia: &mut Cia, tab: &[u16; CIAT_TABLEN], cycles: u32) {
    for _ in 0..cycles {
        cia.tick(tab);
    }
}

/// A CIA with its clock started, which is what writing tenths does.
fn started() -> Cia {
    let mut cia = Cia::new();
    let t = tab();
    // 60 Hz is the power-on default (CRA bit 7 clear).
    cia.write(0x08, 0, cia.clk, &t); // TEN — starts the clock
    cia
}

#[test]
fn a_fresh_cia_comes_up_stopped() {
    let mut cia = Cia::new();
    let t = tab();
    run(&mut cia, &t, PAL_CYCLES_PER_SEC); // a whole emulated second
    assert_eq!(cia.regs[CIA_TOD_TEN], 0, "a stopped clock does not advance");
    assert_eq!(cia.regs[CIA_TOD_SEC], 0);
}

#[test]
fn ten_tenths_make_a_second() {
    let mut cia = started();
    let t = tab();
    // One tenth at 60 Hz is six mains ticks of 985248/60 cycles each.
    let tenth = TENTH_60HZ_DIVIDER;
    run(&mut cia, &t, tenth);
    assert_eq!(cia.regs[CIA_TOD_TEN], 1, "one tenth");
    run(&mut cia, &t, tenth * 9);
    assert_eq!(cia.regs[CIA_TOD_TEN], 0, "the tenth wrapped");
    assert_eq!(cia.regs[CIA_TOD_SEC], 1, "and carried into seconds");
}

#[test]
fn fifty_hertz_uses_a_different_divider() {
    let mut cia = Cia::new();
    let t = tab();
    cia.write(0x0e, CIA_CRA_TODIN_50HZ, cia.clk, &t); // CRA: 50 Hz
    cia.write(0x08, 0, cia.clk, &t); // start
    // At 50 Hz a tenth is five mains ticks of 985248/50.
    let tenth = TENTH_50HZ_DIVIDER;
    run(&mut cia, &t, tenth);
    assert_eq!(cia.regs[CIA_TOD_TEN], 1, "50 Hz counts five, not six");
}

#[test]
fn writing_the_hour_stops_the_clock_and_tenths_start_it() {
    let mut cia = started();
    let t = tab();
    let tenth = TENTH_60HZ_DIVIDER;
    run(&mut cia, &t, tenth);
    assert_eq!(cia.regs[CIA_TOD_TEN], 1);

    cia.write(0x0b, 0x01, cia.clk, &t); // HR — stops
    run(&mut cia, &t, tenth * 5);
    assert_eq!(cia.regs[CIA_TOD_TEN], 1, "stopped while the time is being set");

    cia.write(0x08, 0, cia.clk, &t); // TEN — starts again
    run(&mut cia, &t, tenth);
    assert_eq!(cia.regs[CIA_TOD_TEN], 1, "restarted from the written value");
    run(&mut cia, &t, tenth);
    assert_eq!(cia.regs[CIA_TOD_TEN], 2);
}

#[test]
fn seconds_minutes_and_the_twelve_hour_wrap() {
    let mut cia = Cia::new();
    let t = tab();
    // 09:59:59.9, then one more tenth.
    cia.write(0x0b, 0x09, cia.clk, &t); // HR (stops)
    cia.write(0x0a, 0x59, cia.clk, &t); // MIN
    cia.write(0x09, 0x59, cia.clk, &t); // SEC
    cia.write(0x08, 0x09, cia.clk, &t); // TEN (starts)
    let tenth = TENTH_60HZ_DIVIDER;
    run(&mut cia, &t, tenth);
    assert_eq!(cia.regs[CIA_TOD_TEN], 0);
    assert_eq!(cia.regs[CIA_TOD_SEC], 0);
    assert_eq!(cia.regs[CIA_TOD_MIN], 0);
    assert_eq!(cia.regs[CIA_TOD_HR] & 0x1f, 0x10, "09 rolls to 10, not to 0x0A");
}

#[test]
fn crb_bit_seven_writes_the_alarm_and_a_match_raises_the_flag() {
    let mut cia = Cia::new();
    let t = tab();
    // Alarm at 00:00:00.2 — the write must not move the clock.
    cia.write(0x0f, CIA_CRB_ALARM, cia.clk, &t);
    cia.write(0x08, 0x02, cia.clk, &t);
    assert_eq!(cia.regs[CIA_TOD_TEN], 0, "an alarm write leaves the clock alone");
    cia.write(0x0f, 0, cia.clk, &t); // back to clock writes

    cia.regs[CIA_ICR] = CIA_IM_TOD; // unmask, as software would
    cia.write(0x08, 0, cia.clk, &t); // start
    let tenth = TENTH_60HZ_DIVIDER;
    run(&mut cia, &t, tenth);
    assert_eq!(cia.irqflags & CIA_IM_TOD, 0, "not yet");
    run(&mut cia, &t, tenth);
    assert_ne!(cia.irqflags & CIA_IM_TOD, 0, "the alarm matched at .2");
}

#[test]
fn reading_the_hour_latches_and_reading_tenths_releases() {
    let mut cia = started();
    let t = tab();
    let tenth = TENTH_60HZ_DIVIDER;
    run(&mut cia, &t, tenth * 3);
    // Reading the hour takes a coherent snapshot of all four registers. At this point
    // the clock stands at three tenths, so that is what the snapshot holds.
    let hr = cia.read(0x0b, cia.clk, &t);
    assert_eq!(hr & 0x1f, 0, "the hour itself is still 0");

    // The clock runs on underneath; the snapshot does not.
    run(&mut cia, &t, tenth * 4);
    assert_eq!(cia.read(0x09, cia.clk, &t), 0, "seconds still come from the snapshot");

    // Reading tenths answers from the snapshot AND releases it. The release takes effect
    // for the NEXT read — which is exactly what keeps a multi-register read coherent, and
    // is why software reads hours first and tenths last.
    assert_eq!(cia.read(0x08, cia.clk, &t), 3, "the releasing read still answers the snapshot");
    assert_eq!(cia.read(0x08, cia.clk, &t), 7, "and the next one shows the clock ran on");
}

#[test]
fn the_machine_advances_tod_through_the_bus() {
    // The integration, not the chip: a program starts the clock and the machine runs.
    // This is the shape of every TOD wait loop the Ultimate libraries use.
    let mut m = Machine::new();
    // LDA #$00 / STA $DC08 (start TOD) / JMP *
    let prog: &[u8] = &[0xa9, 0x00, 0x8d, 0x08, 0xdc, 0x4c, 0x05, 0xc0];
    m.poke(0xc000, prog);
    m.write_full(0x0001, 0x37);
    m.c64_core.reg_pc = 0xc000;
    m.run_for_full_capped(64, 2, &mut NullSink, |_, _, _, _, _, _, _| {});

    // Half an emulated second.
    m.run_for_full_capped(PAL_CYCLES_PER_SEC as u64 / 2, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
    let tenths = m.cia1.regs[CIA_TOD_TEN];
    assert!(tenths >= 4, "half a second is about five tenths, got {tenths}");
    assert_eq!(m.cia1.regs[CIA_TOD_SEC], 0, "and not yet a whole second");
}



// ── Die Lücke, die zwei Defekte durchgelassen hat ──────────────────────────────────
//
// Every case above drives the chip with tick() and checks an ABSOLUTE value — and an
// absolute value can be satisfied by a clock that is consistently wrong. UE2 caught both
// defects only because mandelbrot-upic runs with the screen ON and waits on TOD.
//
// These are DIFFERENCE tests: two runs that must agree. A divider that quietly loses
// cycles cannot pass them.

/// Run the machine for `cycles` with the screen in `d011`, return elapsed TOD tenths.
fn tod_tenths_after(d011: u8, cycles: u64) -> u32 {
    let mut m = Machine::new();
    // LDA #d011 / STA $D011 / LDA #$00 / STA $DC08 (start TOD) / JMP *
    let prog: &[u8] = &[0xa9, d011, 0x8d, 0x11, 0xd0, 0xa9, 0x00, 0x8d, 0x08, 0xdc, 0x4c, 0x0a, 0xc0];
    m.poke(0xc000, prog);
    m.write_full(0x0001, 0x37);
    m.c64_core.reg_pc = 0xc000;
    m.run_for_full_capped(256, 4, &mut NullSink, |_, _, _, _, _, _, _| {});
    m.run_for_full_capped(cycles, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
    let ten = m.cia1.regs[CIA_TOD_TEN] as u32;
    let sec = m.cia1.regs[CIA_TOD_SEC] as u32;
    (sec & 0x0f) * 10 + ((sec >> 4) & 0x07) * 100 + ten
}

#[test]
fn badlines_must_not_slow_the_clock() {
    // $1B = screen on, 25 badlines a frame steal 40 cycles each. $0B = blanked, none.
    // TOD hangs off the mains, so the two must agree — they differed by ~5 % while the
    // divider counted tick() calls and the steals arrived by assignment to cia.clk.
    let two_seconds = PAL_CYCLES_PER_SEC as u64 * 2;
    let on = tod_tenths_after(0x1b, two_seconds);
    let off = tod_tenths_after(0x0b, two_seconds);
    assert_eq!(on, off, "screen on gave {on} tenths, blanked gave {off}");
    assert!((15..=17).contains(&on), "two seconds at the 60 Hz divider on 50 Hz mains is about 16 tenths, got {on}");
}

#[test]
fn a_sixty_hertz_divider_on_pal_mains_runs_the_clock_slow() {
    // The mains is the MACHINE's: PAL is 50 Hz whatever CRA says. Software picking the
    // 60 Hz divider therefore needs six ticks where five would do, and TOD runs at 5/6 —
    // which is what real hardware does and what deriving the frequency from CRA hid.
    let mut fifty = Machine::new();
    let mut sixty = Machine::new();
    for (m, cra) in [(&mut fifty, CIA_CRA_TODIN_50HZ), (&mut sixty, 0u8)] {
        let prog: &[u8] = &[0xa9, cra, 0x8d, 0x0e, 0xdc, 0xa9, 0x00, 0x8d, 0x08, 0xdc, 0x4c, 0x0a, 0xc0];
        m.poke(0xc000, prog);
        m.write_full(0x0001, 0x37);
        m.c64_core.reg_pc = 0xc000;
        m.run_for_full_capped(256, 4, &mut NullSink, |_, _, _, _, _, _, _| {});
        m.run_for_full_capped(PAL_CYCLES_PER_SEC as u64 * 3, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
    }
    let a = fifty.cia1.regs[CIA_TOD_SEC];
    let b = sixty.cia1.regs[CIA_TOD_SEC];
    assert_eq!(a, 0x03, "the 50 Hz divider matches the grid: three seconds");
    assert_eq!(b, 0x02, "the 60 Hz divider on 50 Hz mains runs 5/6, so two");
}

