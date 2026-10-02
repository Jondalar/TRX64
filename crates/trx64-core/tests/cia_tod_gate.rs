//! CIA time-of-day gate — the clock that never ran.
//!
//! The first CIA port bumped a prescaler nothing read, so TOD stood still while its
//! registers round-tripped and looked alive. Invisible on a stock C64, where little
//! software reads TOD; fatal on the `u64` profile, where the timers and the raster are
//! CPU-clocked and TOD is the only thing left carrying real time. Reported by the UE2
//! session against five of Xander Mol's Ultimate projects — mandelbrot-upic sits in a TOD
//! wait loop for ever with turbo on.
//!
//! The chip is VICE's `ciacore.c` (ciacore.rs): TOD ticks on its own mains alarm
//! (`ciacore_inttod`), whose period `ticks_per_sec / power_freq` is corrected tick by tick
//! so a second of mains is a second of clock. The mechanics are driven at the chip, the
//! integration through the CPU bus.

use trx64_core::c64cia::new_cia1;
use trx64_core::ciacore::{
    CiaCore, NoPorts, CIA_CRA_TODIN_50HZ, CIA_CRB_ALARM, CIA_ICR, CIA_IM_TOD, CIA_MODEL_6526, CIA_TOD_HR,
    CIA_TOD_MIN, CIA_TOD_SEC, CIA_TOD_TEN,
};
use trx64_core::{Machine, NullSink};

/// The PAL system clock this gate runs the chip at (the `c64-pal` row's).
const PAL_CYCLES_PER_SEC: u32 = 985_248;

/// PAL mains is 50 Hz — always, whatever CRA says. A tenth therefore costs five mains
/// ticks when software picks the matching divider, and SIX when it picks the 60 Hz one,
/// which is why TOD then runs at 5/6. These are the nominal lengths; VICE's per-tick
/// correction puts each tick within a cycle of `n · 985248 / 50`, so a check is made
/// `SLACK` past the nominal instant (or before it), far inside one 19 704-cycle tick.
const TENTH_50HZ_DIVIDER: u32 = (PAL_CYCLES_PER_SEC / 50) * 5;
const TENTH_60HZ_DIVIDER: u32 = (PAL_CYCLES_PER_SEC / 50) * 6;
const SLACK: u32 = 200;

/// A CIA1 of the default C64, reset at clock 0 (VICE: stopped, 01:00:00.0).
fn fresh() -> CiaCore {
    let mut cia = new_cia1(CIA_MODEL_6526, PAL_CYCLES_PER_SEC, 50);
    cia.clk = 0;
    cia.reset(&mut NoPorts);
    cia
}

/// Run the chip for `cycles` PHI2 cycles: the CPU loop's alarm dispatch, every cycle.
fn run(cia: &mut CiaCore, cycles: u32) {
    for _ in 0..cycles {
        let clk = cia.clk + 1;
        cia.process_alarms(&mut NoPorts, clk);
        cia.clk = clk;
    }
}

fn wr(cia: &mut CiaCore, addr: u16, v: u8) {
    cia.store(&mut NoPorts, addr, v);
}

fn rd(cia: &mut CiaCore, addr: u16) -> u8 {
    cia.read(&mut NoPorts, addr)
}

/// A CIA with its clock started, which is what writing tenths does.
fn started() -> CiaCore {
    let mut cia = fresh();
    // 60 Hz is the power-on default (CRA bit 7 clear).
    wr(&mut cia, 0x08, 0); // TEN — starts the clock
    cia
}

#[test]
fn a_fresh_cia_comes_up_stopped() {
    let mut cia = fresh();
    run(&mut cia, PAL_CYCLES_PER_SEC); // a whole emulated second
    assert_eq!(cia.c_cia[CIA_TOD_TEN], 0, "a stopped clock does not advance");
    assert_eq!(cia.c_cia[CIA_TOD_SEC], 0);
    assert_eq!(cia.c_cia[CIA_TOD_HR], 1, "VICE's reset leaves the hour at 1");
}

#[test]
fn ten_tenths_make_a_second() {
    let mut cia = started();
    // One tenth at the 60 Hz divider is six mains ticks of 985248/50 cycles each.
    let tenth = TENTH_60HZ_DIVIDER;
    run(&mut cia, tenth - SLACK);
    assert_eq!(cia.c_cia[CIA_TOD_TEN], 0, "not before the sixth tick");
    run(&mut cia, 2 * SLACK);
    assert_eq!(cia.c_cia[CIA_TOD_TEN], 1, "one tenth");
    run(&mut cia, tenth * 9);
    assert_eq!(cia.c_cia[CIA_TOD_TEN], 0, "the tenth wrapped");
    assert_eq!(cia.c_cia[CIA_TOD_SEC], 1, "and carried into seconds");
}

#[test]
fn fifty_hertz_uses_a_different_divider() {
    let mut cia = fresh();
    wr(&mut cia, 0x0e, CIA_CRA_TODIN_50HZ); // CRA: 50 Hz
    wr(&mut cia, 0x08, 0); // start
    // At the 50 Hz divider a tenth is five mains ticks of 985248/50.
    run(&mut cia, TENTH_50HZ_DIVIDER + SLACK);
    assert_eq!(cia.c_cia[CIA_TOD_TEN], 1, "50 Hz counts five, not six");
}

#[test]
fn writing_the_hour_stops_the_clock_and_tenths_start_it() {
    let mut cia = started();
    let tenth = TENTH_60HZ_DIVIDER;
    run(&mut cia, tenth + SLACK);
    assert_eq!(cia.c_cia[CIA_TOD_TEN], 1);

    wr(&mut cia, 0x0b, 0x01); // HR — stops
    run(&mut cia, tenth * 5);
    assert_eq!(cia.c_cia[CIA_TOD_TEN], 1, "stopped while the time is being set");

    wr(&mut cia, 0x08, 0); // TEN — starts again, the ring counter cleared
    run(&mut cia, tenth - SLACK);
    assert_eq!(cia.c_cia[CIA_TOD_TEN], 0, "restarted from the written value");
    run(&mut cia, 2 * SLACK);
    assert_eq!(cia.c_cia[CIA_TOD_TEN], 1);
}

#[test]
fn seconds_minutes_and_the_twelve_hour_wrap() {
    let mut cia = fresh();
    // 09:59:59.9, then one more tenth.
    wr(&mut cia, 0x0b, 0x09); // HR (stops)
    wr(&mut cia, 0x0a, 0x59); // MIN
    wr(&mut cia, 0x09, 0x59); // SEC
    wr(&mut cia, 0x08, 0x09); // TEN (starts)
    run(&mut cia, TENTH_60HZ_DIVIDER + SLACK);
    assert_eq!(cia.c_cia[CIA_TOD_TEN], 0);
    assert_eq!(cia.c_cia[CIA_TOD_SEC], 0);
    assert_eq!(cia.c_cia[CIA_TOD_MIN], 0);
    assert_eq!(cia.c_cia[CIA_TOD_HR] & 0x1f, 0x10, "09 rolls to 10, not to 0x0A");
}

#[test]
fn crb_bit_seven_writes_the_alarm_and_a_match_raises_the_flag() {
    let mut cia = fresh();
    wr(&mut cia, 0x0b, 0x00); // HR 0 (stops)
    // Alarm at 00:00:00.2 — the write must not move the clock.
    wr(&mut cia, 0x0f, CIA_CRB_ALARM);
    wr(&mut cia, 0x0b, 0x00);
    wr(&mut cia, 0x08, 0x02);
    assert_eq!(cia.c_cia[CIA_TOD_TEN], 0, "an alarm write leaves the clock alone");
    wr(&mut cia, 0x0f, 0); // back to clock writes
    // Setting the hour to 0 made the time equal the reset alarm (all zero) for a moment,
    // and VICE compares on every changed write: that latched the flag. Read it off, as
    // software setting an alarm does.
    rd(&mut cia, 0x0d);
    assert_eq!(cia.irqflags & CIA_IM_TOD, 0, "acknowledged");

    cia.c_cia[CIA_ICR] = CIA_IM_TOD as u8; // unmask, as software would
    wr(&mut cia, 0x08, 0); // start
    let tenth = TENTH_60HZ_DIVIDER;
    run(&mut cia, tenth + SLACK);
    assert_eq!(cia.irqflags & CIA_IM_TOD, 0, "not yet");
    run(&mut cia, tenth);
    assert_ne!(cia.irqflags & CIA_IM_TOD, 0, "the alarm matched at .2");
}

#[test]
fn reading_the_hour_latches_and_reading_tenths_releases() {
    let mut cia = started();
    wr(&mut cia, 0x0b, 0x00); // HR 0 stops the clock …
    wr(&mut cia, 0x08, 0x00); // … and tenths start it again
    let tenth = TENTH_60HZ_DIVIDER;
    run(&mut cia, tenth * 3 + SLACK);
    // Reading the hour takes a coherent snapshot of all four registers. At this point
    // the clock stands at three tenths, so that is what the snapshot holds.
    let hr = rd(&mut cia, 0x0b);
    assert_eq!(hr & 0x1f, 0, "the hour itself is 0");

    // The clock runs on underneath; the snapshot does not.
    run(&mut cia, tenth * 4);
    assert_eq!(rd(&mut cia, 0x09), 0, "seconds still come from the snapshot");

    // Reading tenths answers from the snapshot AND releases it. The release takes effect
    // for the NEXT read — which is exactly what keeps a multi-register read coherent, and
    // is why software reads hours first and tenths last.
    assert_eq!(rd(&mut cia, 0x08), 3, "the releasing read still answers the snapshot");
    assert_eq!(rd(&mut cia, 0x08), 7, "and the next one shows the clock ran on");
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
    let tenths = m.cia1.c_cia[CIA_TOD_TEN];
    assert!(tenths >= 4, "half a second is about five tenths, got {tenths}");
    assert_eq!(m.cia1.c_cia[CIA_TOD_SEC], 0, "and not yet a whole second");
}



// ── Die Lücke, die zwei Defekte durchgelassen hat ──────────────────────────────────
//
// Every case above drives the chip alone and checks an ABSOLUTE value — and an
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
    let ten = m.cia1.c_cia[CIA_TOD_TEN] as u32;
    let sec = m.cia1.c_cia[CIA_TOD_SEC] as u32;
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
    let a = fifty.cia1.c_cia[CIA_TOD_SEC];
    let b = sixty.cia1.c_cia[CIA_TOD_SEC];
    assert_eq!(a, 0x03, "the 50 Hz divider matches the grid: three seconds");
    assert_eq!(b, 0x02, "the 60 Hz divider on 50 Hz mains runs 5/6, so two");
}

