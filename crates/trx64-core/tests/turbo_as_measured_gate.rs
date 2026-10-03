//! Turbo as measured on a C64 Ultimate — one test per measured fact.
//!
//! The facts come from the owner's C64 Ultimate (firmware 3.15, FPGA 125, core 1.50, PAL),
//! measured 2026-10-03 with `turbomeas.prg`. That program is in `fixtures/turbomeas/` with
//! its source, and these tests run it: the same program, the same mailbox at `$C000`, the
//! same arithmetic on what it reports. Only the REST driver is replaced — a menu change is
//! `Machine::set_u64_turbo`, which is what stands for the firmware's menu here.
//!
//! The machine is the U64 profile on the U64-II / C64 Ultimate speed table, booted with
//! ROMs (`TRX64_ROM_DIR`, else the sibling C64RE checkout) and run past the post-reset
//! hold before anything is measured, as the measurement did.

use std::path::PathBuf;
use trx64_core::vic::{SpeedProfile, U64SpeedTable};
use trx64_core::{Machine, NullSink};

const FRAME: u64 = 19_656;
const PHI2_HZ: f64 = 985_248.0;
const PRG: &[u8] = include_bytes!("fixtures/turbomeas/turbomeas.prg");

// The mailbox (turbomeas.asm).
const CMD: u16 = 0xc000;
const READY: u16 = 0xc00e;
const SEQ: u16 = 0xc00f;
const R_OUT: u16 = 0xc010;

fn rom_dir() -> PathBuf {
    std::env::var_os("TRX64_ROM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms")))
}

fn run(m: &mut Machine, cycles: u64) {
    m.run_for_full(cycles, &mut NullSink, |_, _, _, _, _, _, _| {});
}

fn rd(m: &Machine, addr: u16) -> u8 {
    m.read_full(addr)
}

/// A booted C64 Ultimate in "U64 Turbo Registers" mode, 3 s after power-on — past the
/// 2.06 s hold, at the BASIC prompt.
fn booted() -> Option<Machine> {
    let dir = rom_dir();
    if !dir.join("kernal-901227-03.bin").exists() {
        eprintln!("skip: ROMs absent at {}", dir.display());
        return None;
    }
    let mut m = Machine::new();
    m.set_machine_profile(SpeedProfile::U64);
    m.set_u64_speed_table(U64SpeedTable::U64II);
    m.boot_from_dir(&dir).expect("boot ROMs");
    run(&mut m, 3 * PHI2_HZ as u64);
    Some(m)
}

/// `run_prg` without the reset: the program into `$0801`, `SYS 2064`, wait for READY.
fn start_tm(m: &mut Machine) {
    m.poke(0x0801, &PRG[2..]);
    m.poke(READY, &[0]);
    m.c64_core.reg_pc = 0x0810;
    for _ in 0..50 {
        run(m, FRAME);
        if rd(m, READY) == 0xa5 {
            return;
        }
    }
    panic!("turbomeas did not start");
}

#[derive(Clone, Copy)]
struct Params {
    d031: Option<u8>,
    d011: u8,
    tenths: u8,
    k: u8,
    line: u8,
    samples: u8,
}

impl Default for Params {
    fn default() -> Self {
        Params { d031: None, d011: 0x1b, tenths: 20, k: 1, line: 0x35, samples: 32 }
    }
}

/// tm.py `command`: parameters into `$C001`, the command into `$C000`, run until SEQ moves.
fn command(m: &mut Machine, cmd: u8, p: Params) {
    let seq = rd(m, SEQ);
    m.poke(
        0xc001,
        &[p.d031.unwrap_or(0), u8::from(p.d031.is_some()), p.d011, p.tenths, p.k, p.line, p.samples],
    );
    m.poke(CMD, &[cmd]);
    for _ in 0..2000 {
        run(m, FRAME / 4);
        if rd(m, SEQ) != seq {
            return;
        }
    }
    panic!("command {cmd} timed out");
}

#[derive(Debug, Clone, Copy)]
struct Measure {
    /// CPU cycles per PHI2 cycle, by the program's own formula.
    x: f64,
    frames_per_s: f64,
    cia2_hz: f64,
    d031_before: u8,
    d031_after: u8,
    d030: u8,
}

/// tm.py `measure`.
fn measure(m: &mut Machine, p: Params) -> Measure {
    command(m, 1, p);
    let r: Vec<u8> = (0..0x13).map(|i| rd(m, R_OUT + i)).collect();
    let outer = u32::from(r[0]) | u32::from(r[1]) << 8 | u32::from(r[2]) << 16;
    let frames = u32::from(r[3]) | u32::from(r[4]) << 8;
    let be = |s: &[u8]| u32::from(s[0]) << 24 | u32::from(s[1]) << 16 | u32::from(s[2]) << 8 | u32::from(s[3]);
    let ticks = be(&r[5..9]).wrapping_sub(be(&r[9..13]));
    let secs = f64::from(p.tenths) / 10.0;
    Measure {
        x: f64::from(outer) * (f64::from(p.k) * 1286.0 + 32.0) / secs / PHI2_HZ,
        frames_per_s: f64::from(frames) / secs,
        cia2_hz: f64::from(ticks) / secs,
        d031_before: r[13],
        d031_after: r[14],
        d030: r[15],
    }
}

/// The measurement's own corrections, applied to one raw reading: on lines `$35`/`$33` the
/// poll matches line 309/307 first, which puts 56 lines (3528 cycles) between the two
/// readings; a reading that straddles the timer's low byte underflow is off by 256 (the
/// lo/hi read race); and the result is signed. Whichever correction leaves the smallest
/// magnitude is the reading — a latency the corrections cannot bring near zero stays large
/// and fails.
fn normalise(raw: i64) -> i64 {
    let mut best = raw;
    for base in [raw, raw - 19_656, raw - 3528] {
        for adj in [0, 256, -256] {
            let v = base + adj;
            if v.abs() < best.abs() {
                best = v;
            }
        }
    }
    best
}

/// tm.py `irq`: reference − IRQ reading per sample, in PHI2 cycles (`normalise`d).
fn irq_latency(m: &mut Machine, d031: u8, line: u8) -> Vec<i64> {
    command(m, 2, Params { d031: Some(d031), line, samples: 32, ..Params::default() });
    (0..32u16)
        .map(|i| {
            let r = i64::from(rd(m, 0xc100 + i)) | i64::from(rd(m, 0xc200 + i)) << 8;
            let q = i64::from(rd(m, 0xc300 + i)) | i64::from(rd(m, 0xc400 + i)) << 8;
            normalise((r - q).rem_euclid(19_656))
        })
        .collect()
}

fn histogram(v: &[i64]) -> std::collections::BTreeMap<i64, usize> {
    let mut h = std::collections::BTreeMap::new();
    for &x in v {
        *h.entry(x).or_insert(0) += 1;
    }
    h
}

/// The speed table as measured on the C64 Ultimate (index → CPU cycles per PHI2 cycle,
/// display off, bit 7 set).
const MEASURED: [f64; 16] = [1.0, 2.0, 3.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0, 16.0, 20.0, 24.0, 32.0, 40.0, 47.0, 63.0];
/// What the menu calls each index — the loop size the measurement used per index.
const MENU: [u8; 16] = [1, 2, 3, 4, 6, 8, 10, 12, 14, 16, 20, 24, 32, 40, 48, 64];

fn close(x: f64, want: f64, rel: f64) -> bool {
    (x - want).abs() <= want * rel
}

// ── 1. the speed table ───────────────────────────────────────────────────────────────────

/// Every index runs as many CPU cycles per PHI2 cycle as its menu label says — except 14
/// and 15, which run 47 and 63, not 48 and 64.
#[test]
fn a_ram_loop_runs_the_measured_ratio_at_every_index() {
    let Some(mut m) = booted() else { return };
    start_tm(&mut m);
    for i in 0..16u8 {
        let r = measure(&mut m, Params { d031: Some(0x80 | i), d011: 0x0b, tenths: 10, k: MENU[i as usize], ..Params::default() });
        eprintln!("index {i:2}: x = {:.3} (measured {})", r.x, MEASURED[i as usize]);
        assert!(close(r.x, MEASURED[i as usize], 0.004), "index {i}: x = {:.3}, the device runs {}", r.x, MEASURED[i as usize]);
        assert_eq!(r.d031_after, 0x80 | i, "and `$D031` reads back what was written");
    }
}

// ── 2. bit 7 of `$D031` ──────────────────────────────────────────────────────────────────

/// Bit 7 = 1 removes the badline stalls, bit 7 = 0 keeps them — at every index, 0 included.
/// The device: `$0F` 59.54 against `$8F` 62.93 with the display on (5.43 % lost, at 1, 16
/// and 64 MHz alike), 63.01 for both with it off; `$80` runs 1.00 and `$00` 0.945.
#[test]
fn bit_7_set_removes_the_badline_stalls_at_every_index() {
    let Some(mut m) = booted() else { return };
    start_tm(&mut m);
    let x = |m: &mut Machine, d031: u8, d011: u8| {
        let k = MENU[usize::from(d031 & 0x0f)];
        measure(m, Params { d031: Some(d031), d011, tenths: 10, k, ..Params::default() }).x
    };
    for i in [0u8, 9, 15] {
        let (on_stalls, on_free) = (x(&mut m, i, 0x1b), x(&mut m, 0x80 | i, 0x1b));
        let (off_stalls, off_free) = (x(&mut m, i, 0x0b), x(&mut m, 0x80 | i, 0x0b));
        let loss = 1.0 - on_stalls / on_free;
        eprintln!("index {i:2}: display on {on_stalls:.3} / {on_free:.3} (loss {:.2} %), off {off_stalls:.3} / {off_free:.3}", loss * 100.0);
        assert!((0.050..=0.058).contains(&loss), "index {i}: bit 7 = 0 loses the badlines, ~5.4 %: {:.2} %", loss * 100.0);
        assert!(close(on_free, MEASURED[i as usize], 0.004), "index {i}: bit 7 = 1 loses nothing to the display");
        assert!(close(off_stalls, off_free, 0.003), "index {i}: with the display off the bit changes nothing");
    }
    assert!(close(x(&mut m, 0x80, 0x1b), 1.00, 0.005), "`$80` is 1 MHz WITHOUT the stalls");
    let slow = x(&mut m, 0x00, 0x1b);
    assert!((0.94..=0.95).contains(&slow), "`$00` is a C64's 1 MHz, stalls and all: {slow:.3}");
}

// ── 3. time stays PHI2 time ──────────────────────────────────────────────────────────────

/// CIA 2's timers count PHI2 at every index — 985,250-985,950 ticks per TOD second on the
/// device, display on and off, both settings of bit 7 — and the raster runs in real time:
/// 100-101 frames per two TOD seconds.
#[test]
fn cia_timers_and_the_raster_count_phi2_at_every_index() {
    let Some(mut m) = booted() else { return };
    start_tm(&mut m);
    for i in 0..16u8 {
        let d031 = i | if i & 1 != 0 { 0x80 } else { 0 };
        let d011 = if i & 2 != 0 { 0x0b } else { 0x1b };
        let r = measure(&mut m, Params { d031: Some(d031), d011, tenths: 20, k: MENU[i as usize], ..Params::default() });
        eprintln!("$D031 ${d031:02X} $D011 ${d011:02X}: CIA2 {:.0} Hz, {:.1} frames/s", r.cia2_hz, r.frames_per_s);
        assert!((985_100.0..=986_000.0).contains(&r.cia2_hz), "${d031:02X}: CIA 2 counted {:.0} per TOD second", r.cia2_hz);
        assert!((50.0..=50.5).contains(&r.frames_per_s), "${d031:02X}: {:.1} frames per TOD second", r.frames_per_s);
    }
}

// ── 4. interrupt latency ─────────────────────────────────────────────────────────────────

/// A raster IRQ at 8, 16 and 64 MHz is taken within less than one PHI2 cycle of the raster
/// event, badline stalls on or off: the handler's first I/O read lands in the PHI2 cycle in
/// which a polling loop's read after the line change lands. On the device: 0 at 64 MHz in
/// every sample; 0 at 8 MHz (two samples of 64 at −1, line `$FA`); 0 or +1 at 16 MHz.
///
/// TRX64 under the 6502's own rule at the turbo clock, with every access at the turbo
/// clock: 0 at 64 MHz; at 8 and 16 MHz 0 or −1 — at 8 MHz all −1 on line `$FA` in one
/// alignment of the run — where the device gives 0 (8) and 0 or +1 (16). Which of the two
/// paths crosses a PHI2 edge first is decided on the device by what an I/O access costs at
/// turbo, which this model does not charge; the bound asserted here is "within one PHI2
/// cycle", the claim itself. A delay counted in PHI2 cycles gave 1-2 at every speed.
#[test]
fn a_raster_irq_at_turbo_is_taken_within_one_phi2_cycle() {
    let Some(mut m) = booted() else { return };
    start_tm(&mut m);
    for d031 in [0x85u8, 0x89, 0x8f, 0x0f, 0x05] {
        for line in [0x35u8, 0x33, 0xfa] {
            let v = irq_latency(&mut m, d031, line);
            let h = histogram(&v);
            eprintln!("$D031 ${d031:02X} line ${line:02X}: {h:?}");
            match d031 & 0x0f {
                0x0f => assert!(v.iter().all(|&x| x == 0), "${d031:02X} line ${line:02X}: always 0 at 64 MHz: {h:?}"),
                _ => assert!(v.iter().all(|&x| x.abs() <= 1), "${d031:02X} line ${line:02X}: within one PHI2 cycle: {h:?}"),
            }
        }
    }
}

/// At 1 MHz nothing changed: the same program, the same samples, the same latencies as
/// before the turbo interrupt rule existed (captured from the build before it, on this
/// path: a fresh boot, `$D031 = $00`, lines `$35`, `$33`, `$FA` in that order). The device's
/// spread is −4..+5 on a normal line and +38..+48 on a badline; this is that machine.
#[test]
fn at_one_mhz_the_interrupt_latency_is_unchanged() {
    let Some(mut m) = booted() else { return };
    start_tm(&mut m);
    let want: [(u8, &[(i64, usize)]); 3] = [
        (0x35, &[(-4, 31), (2, 1)]),
        (0x33, &[(38, 15), (44, 1), (47, 16)]),
        (0xfa, &[(-1, 6), (2, 21), (5, 5)]),
    ];
    for (line, hist) in want {
        let got: Vec<(i64, usize)> = histogram(&irq_latency(&mut m, 0x00, line)).into_iter().collect();
        assert_eq!(got, hist.to_vec(), "line ${line:02X} at 1 MHz with the stalls");
    }
}

// ── 5. last write wins ───────────────────────────────────────────────────────────────────

/// The device, "U64 Turbo Registers" mode, the program's monitor loop sampling every 0.5 s:
/// program writes `$85` → 8.0; menu CPU Speed 32 → 31.97 and `$D031` reads `$8C`; program
/// writes `$83` → 4.0; menu 32 AGAIN — the same value — → 31.97, `$8C`; menu Badline Timing
/// Enabled → 30.2 with the display on, `$D031` reads `$0C`.
#[test]
fn the_last_write_wins_between_the_program_and_the_menu() {
    let Some(mut m) = booted() else { return };
    start_tm(&mut m);
    const TENTHS: u8 = 5;
    const K: u8 = 16;
    let period = u64::from(TENTHS) * PHI2_HZ as u64 / 10;
    m.poke(0xc020, &[0, 0]);
    m.poke(0xc004, &[TENTHS, K]);
    m.poke(0xc008, &[0, 0]);
    m.poke(CMD, &[3]);
    let entries = |m: &Machine| u32::from(rd(m, 0xc021));
    // The latest complete monitor entry: (x, `$D031` as the program read it).
    let last = |m: &Machine| {
        let n = entries(m);
        let i = u16::from(((n + 63) % 64) as u8);
        let e: Vec<u8> = (0..4).map(|j| rd(m, 0xc600 + i * 4 + j)).collect();
        let outer = f64::from(u32::from(e[0]) | u32::from(e[1]) << 8 | u32::from(e[2]) << 16);
        (outer * (1286.0 * f64::from(K) + 32.0) / (f64::from(TENTHS) / 10.0) / PHI2_HZ, e[3])
    };
    // Two more entries after an event: the one in progress may straddle it (a poke is taken
    // only when the next sample starts), the second is wholly after it.
    let settle = |m: &mut Machine| {
        let n0 = entries(m);
        for _ in 0..40 {
            run(m, period / 4);
            if (entries(m) + 256 - n0) % 256 >= 2 {
                return;
            }
        }
        panic!("the monitor stopped sampling");
    };
    let poke = |m: &mut Machine, v: u8| m.poke(0xc008, &[1, v]);

    settle(&mut m);
    let (x, d) = last(&m);
    assert_eq!(d, 0x00, "after the boot `$D031` holds what a reset left");
    // One 20 KCycle loop per sample at k = 16: the device logged 0.96 here as well.
    assert!((0.94..=0.97).contains(&x), "1 MHz with the stalls: {x:.2}");

    let steps: [(&str, &dyn Fn(&mut Machine), f64, u8); 5] = [
        ("program writes $85", &|m| poke(m, 0x85), 8.0, 0x85),
        ("menu CPU Speed 32", &|m| m.set_u64_turbo(0x01, 0x0c), 31.97, 0x8c),
        ("program writes $83", &|m| poke(m, 0x83), 4.0, 0x83),
        ("menu CPU Speed 32 again, same value", &|m| m.set_u64_turbo(0x01, 0x0c), 31.97, 0x8c),
        ("menu Badline Timing Enabled", &|m| m.set_u64_turbo(0x01, 0x8c), 30.2, 0x0c),
    ];
    for (what, act, want_x, want_d) in steps {
        act(&mut m);
        settle(&mut m);
        let (x, d) = last(&m);
        eprintln!("{what}: x = {x:.2}, $D031 = ${d:02X}");
        assert_eq!(d, want_d, "{what}: `$D031` reads");
        assert!(close(x, want_x, 0.01), "{what}: x = {x:.2}, the device ran {want_x}");
    }
}

// ── 6. reset, by mode ────────────────────────────────────────────────────────────────────

fn reset_and_restart(m: &mut Machine) {
    m.warm_reset();
    run(m, 3 * PHI2_HZ as u64);
    start_tm(m);
}

/// "U64 Turbo Registers": after a reset `$D031` reads `$00` and the C64 runs 1 MHz WITH the
/// badline stalls (0.95) for as long as nobody writes `$D031` or changes the menu — over
/// 6 s on the device. The menu speed is not a power-on default. `$D030` reads `$FF`.
#[test]
fn a_reset_in_registers_mode_leaves_1_mhz_with_stalls_until_a_write_or_a_menu_set() {
    let Some(mut m) = booted() else { return };
    start_tm(&mut m);
    m.set_u64_turbo(0x01, 0x09); // menu 16 MHz, Badline Timing Disabled
    let r = measure(&mut m, Params { d011: 0x0b, tenths: 10, k: 16, ..Params::default() });
    assert_eq!((r.d031_before, r.d030), (0x89, 0xff), "no reset: the menu shows in `$D031` (device: $89, $FF)");
    assert!(close(r.x, 16.0, 0.004), "and runs it: {:.2}", r.x);

    reset_and_restart(&mut m);
    for wait in [0, 6] {
        run(&mut m, wait * PHI2_HZ as u64);
        let r = measure(&mut m, Params { tenths: 10, ..Params::default() });
        eprintln!("{wait} s after the restart: x = {:.3}, $D031 ${:02X}, $D030 ${:02X}", r.x, r.d031_before, r.d030);
        assert_eq!((r.d031_before, r.d030), (0x00, 0xff), "after a reset (device: $00, $FF)");
        assert!((0.94..=0.95).contains(&r.x), "1 MHz with the stalls, not the menu's 16: {:.3}", r.x);
    }
    let r = measure(&mut m, Params { d031: Some(0x85), d011: 0x0b, tenths: 10, k: 8, ..Params::default() });
    assert!(close(r.x, 8.0, 0.004), "until the program writes `$D031`: {:.2}", r.x);

    reset_and_restart(&mut m);
    m.set_u64_turbo(0x01, 0x09);
    let r = measure(&mut m, Params { d011: 0x0b, tenths: 10, k: 16, ..Params::default() });
    assert_eq!(r.d031_before, 0x89, "or the menu is set");
    assert!(close(r.x, 16.0, 0.004), "and then it runs the menu's speed: {:.2}", r.x);
}

/// "Manual": `$D030` and `$D031` read `$FF`; after a reset the C64 is held at 1 MHz
/// (BUG-061, 2.06 s) and then runs the menu speed. "Off": the same readbacks, a C64's
/// 1 MHz with its stalls.
#[test]
fn manual_and_off_read_ff_and_run_the_menu_speed() {
    let Some(mut m) = booted() else { return };
    m.set_u64_turbo(0x00, 0x89); // Manual, 16 MHz, Badline Timing Enabled
    m.warm_reset();
    run(&mut m, PHI2_HZ as u64);
    m.sync_turbo_from_vic();
    assert_eq!(m.c64_core.pending_turbo_div, 1, "Manual: 1 s after the reset the hold still holds");
    run(&mut m, 2 * PHI2_HZ as u64);
    start_tm(&mut m);
    let r = measure(&mut m, Params { tenths: 10, k: 16, ..Params::default() });
    assert_eq!((r.d031_before, r.d030), (0xff, 0xff), "Manual: both registers read $FF");
    assert!(close(r.x, 15.12, 0.006), "Manual: the menu's 16 MHz with stalls, display on (device 15.12): {:.2}", r.x);

    m.set_u64_turbo(0x00, 0x80); // Off
    let r = measure(&mut m, Params { tenths: 10, ..Params::default() });
    assert_eq!((r.d031_before, r.d030), (0xff, 0xff), "Off: both registers read $FF");
    assert!((0.94..=0.95).contains(&r.x), "Off: a C64's 1 MHz: {:.3}", r.x);
}

// ── 7. the turbo state in a snapshot ─────────────────────────────────────────────────────

fn dump(m: &Machine) -> Vec<u8> {
    use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, RUNTIME_CHECKPOINT_SCHEMA_VERSION};
    use trx64_core::native_snapshot::{write_native_snapshot, WriteNativeSnapshotArgs};
    write_native_snapshot(WriteNativeSnapshotArgs {
        checkpoint: capture_runtime_checkpoint(m, "", "d64", None, None, None, None),
        schema_version: RUNTIME_CHECKPOINT_SCHEMA_VERSION,
        media: Vec::new(),
        runtime_version: "test".into(),
        machine_model: "u64-pal".into(),
        provenance: None,
        pc: i64::from(m.c64_core.reg_pc),
        cycle: m.c64_core.clk as i64,
    })
}

fn turbo_state(m: &Machine) -> (u8, u8, u8, u8, u32, (u8, bool), u64) {
    (
        m.vic.u64_regs_en,
        m.vic.u64_speed_prefer,
        m.read_full(0xd031),
        m.read_full(0xd030),
        m.vic.u64_reset_hold,
        m.vic.u64_speed(),
        m.turbo_divider(),
    )
}

/// A `.c64re` of a C64 Ultimate carries its turbo state, and a restore puts it back — in
/// force at once. An older dump without it restores as a reset leaves "U64 Turbo
/// Registers" mode and says so.
#[test]
fn a_c64re_carries_the_turbo_state() {
    use trx64_core::c64re_snapshot::restore_runtime_checkpoint;
    use trx64_core::native_snapshot::read_native_snapshot;
    let Some(mut m) = booted() else { return };
    // TurboEnable-bit mode, so `$D030` has something to say: menu 40 MHz with stalls, the
    // program's `$85`, `$D030` bit 0 set.
    m.set_u64_turbo(0x05, 0x8d);
    m.write_full(0xd031, 0x85);
    m.write_full(0xd030, 0x01);
    run(&mut m, FRAME);
    let before = turbo_state(&m);
    assert_eq!(before.5, (5, false), "the program's speed is in force");
    let file = dump(&m);

    let read = read_native_snapshot(&file).expect("read .c64re");
    assert!(read.checkpoint.get("turbo").is_some(), "the checkpoint has a turbo node");
    let Some(mut other) = booted() else { return };
    other.set_u64_turbo(0x00, 0x89);
    restore_runtime_checkpoint(&mut other, &read.checkpoint).expect("restore");
    assert_eq!(turbo_state(&other), before, "the same turbo state after the round trip");
    assert_eq!(other.c64_core.turbo_div, 8, "and the CPU runs it at once, not one PHI2 later");
    assert!(other.restore_notes.is_empty(), "{:?}", other.restore_notes);

    // Inside the post-reset hold: the hold rides too.
    m.warm_reset();
    run(&mut m, FRAME);
    let held = turbo_state(&m);
    assert!(held.4 > 0, "the hold is running");
    let read = read_native_snapshot(&dump(&m)).expect("read .c64re");
    restore_runtime_checkpoint(&mut other, &read.checkpoint).expect("restore");
    assert_eq!(turbo_state(&other), held, "the hold comes back with what is left of it");

    // A dump from before the node existed.
    let mut old = read.checkpoint.clone();
    old.as_object_mut().unwrap().remove("turbo");
    other.set_u64_turbo(0x05, 0x8f);
    other.write_full(0xd031, 0x8f);
    restore_runtime_checkpoint(&mut other, &old).expect("restore an old dump");
    assert_eq!(
        turbo_state(&other),
        (0x01, 0x80, 0x00, 0xff, 0, (0, true), 1),
        "registers mode, `$D031 = $00`: 1 MHz with the stalls, as a reset leaves it"
    );
    assert!(
        other.restore_notes.iter().any(|n| n.starts_with("turbo:")),
        "and the restore says so: {:?}",
        other.restore_notes
    );
}

/// A plain C64 has no turbo state to carry: its checkpoint has no node and its restore no
/// note.
#[test]
fn a_c64_checkpoint_has_no_turbo_node() {
    use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, restore_runtime_checkpoint};
    let mut m = Machine::new();
    let cp = capture_runtime_checkpoint(&m, "", "d64", None, None, None, None);
    assert!(cp.get("turbo").is_none());
    restore_runtime_checkpoint(&mut m, &cp).expect("restore");
    assert!(m.restore_notes.iter().all(|n| !n.starts_with("turbo:")), "{:?}", m.restore_notes);
}
