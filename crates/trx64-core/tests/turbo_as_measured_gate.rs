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
/// Past the post-reset hold (2^22 PHI2 cycles), with the KERNAL at its prompt.
const PAST_HOLD: u64 = (1 << 22) + 300_000;
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

/// A booted C64 Ultimate in "U64 Turbo Registers" mode, 4.5 s after power-on — past the
/// 4.26 s post-reset hold, at the BASIC prompt.
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
    run(&mut m, PAST_HOLD);
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
/// TRX64 with the 6502's own rule at the turbo clock (D4) and the CIA read completing at a
/// PHI2 edge (round 2, D9): 0 at 64 MHz, 0 at 16 MHz, 0 at 8 MHz with an occasional −1.
/// Without D9's bus cycle (round 1) 8 and 16 MHz gave 0 or −1, all −1 on `$FA` in one
/// alignment; with a delay counted in PHI2 cycles (851) 1-2 at every speed.
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
                0x05 => {
                    assert!(v.iter().all(|&x| x == 0 || x == -1), "${d031:02X} line ${line:02X}: {h:?}");
                    assert!(h.get(&0).copied().unwrap_or(0) >= 28, "${d031:02X} line ${line:02X}: 0 is the rule: {h:?}");
                }
                0x09 => assert!(v.iter().all(|&x| x == 0 || x == 1), "${d031:02X} line ${line:02X}: 0 or +1: {h:?}"),
                _ => assert!(v.iter().all(|&x| x == 0), "${d031:02X} line ${line:02X}: always 0 at 64 MHz: {h:?}"),
            }
        }
    }
}

/// At 1 MHz nothing changed: the same program, the same samples, the same latencies as
/// before the turbo interrupt rule existed (captured from the build before Spec 890 with
/// this file and this program: a fresh boot, `$D031 = $00`, lines `$35`, `$33`, `$FA` in
/// that order). The device's
/// spread is −4..+5 on a normal line and +38..+48 on a badline; this is that machine.
#[test]
fn at_one_mhz_the_interrupt_latency_is_unchanged() {
    let Some(mut m) = booted() else { return };
    start_tm(&mut m);
    let want: [(u8, &[(i64, usize)]); 3] = [
        (0x35, &[(-4, 31), (2, 1)]),
        (0x33, &[(38, 15), (44, 1), (47, 16)]),
        (0xfa, &[(-4, 1), (-1, 5), (2, 21), (5, 5)]),
    ];
    for (line, hist) in want {
        let got: Vec<(i64, usize)> = histogram(&irq_latency(&mut m, 0x00, line)).into_iter().collect();
        eprintln!("1 MHz line ${line:02X}: {got:?}");
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
    run(m, PAST_HOLD);
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
/// (2^22 PHI2 cycles) and then runs the menu speed. "Off": the same readbacks, a C64's
/// 1 MHz with its stalls.
#[test]
fn manual_and_off_read_ff_and_run_the_menu_speed() {
    let Some(mut m) = booted() else { return };
    m.set_u64_turbo(0x00, 0x89); // Manual, 16 MHz, Badline Timing Enabled
    m.warm_reset();
    run(&mut m, 4 * PHI2_HZ as u64);
    m.sync_turbo_from_vic();
    assert_eq!(m.c64_core.pending_turbo_div, 1, "Manual: 4 s after the reset the hold still holds");
    run(&mut m, PAST_HOLD - 4 * PHI2_HZ as u64);
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
    m.write_full(0xd030, 0x01); // loads the menu's 40 MHz …
    m.write_full(0xd031, 0x85); // … which the program then replaces
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

// ═════════════════════════════ Round 2 ═══════════════════════════════════════════════════
//
// The second measurement (2026-10-03, same device): `turbomeas.prg` command 5 runs a routine
// at `$4000` between two CIA 2 tick reads; `tm2.py` builds the routines. Its loops are
// rebuilt here byte for byte.

const LDA_ABS: u8 = 0xad;
const STA_ABS: u8 = 0x8d;
const NOP: u8 = 0xea;

/// tm2.py `Asm`, at `$4000`.
struct Asm {
    b: Vec<u8>,
}
impl Asm {
    fn new() -> Self {
        Asm { b: Vec::new() }
    }
    fn pc(&self) -> u16 {
        0x4000 + self.b.len() as u16
    }
    fn emit(&mut self, x: &[u8]) {
        self.b.extend_from_slice(x);
    }
    fn abs(&mut self, op: u8, addr: u16) {
        self.emit(&[op, addr as u8, (addr >> 8) as u8]);
    }
    fn jmp(&mut self, addr: u16) {
        self.abs(0x4c, addr);
    }
}

/// tm2.py `io_loop`: `rep` × 256 × `per_block` accesses (`op addr`), each followed by
/// `filler` NOPs. Returns the code and the access count.
fn io_loop(op: u8, addr: u16, val: u8, per_block: usize, filler: usize, rep: u8) -> (Vec<u8>, u64) {
    let mut a = Asm::new();
    a.emit(&[0xa9, val, 0xa0, rep]);
    let l1 = a.pc();
    a.emit(&[0xa2, 0x00]);
    let l2 = a.pc();
    for _ in 0..per_block {
        a.abs(op, addr);
        a.emit(&vec![NOP; filler]);
    }
    a.emit(&[0xca, 0xf0, 0x03]);
    a.jmp(l2);
    a.emit(&[0x88, 0xf0, 0x03]);
    a.jmp(l1);
    a.emit(&[0x60]);
    (a.b, u64::from(rep) * 256 * per_block as u64)
}

/// tm2.py `timed`: the routine at `$4000`, run by command 5; PHI2 ticks it took.
fn timed(m: &mut Machine, code: &[u8], d031: Option<u8>, d011: u8) -> u64 {
    m.poke(0x4000, code);
    command(m, 5, Params { d031, d011, ..Params::default() });
    let be = |a: u16| (0..4).fold(0u32, |v, i| v << 8 | u32::from(rd(m, a + i)));
    u64::from(be(0xc015).wrapping_sub(be(0xc019)))
}

fn phi2_per_access(m: &mut Machine, op: u8, addr: u16, val: u8, d031: u8, d011: u8, per_block: usize, filler: usize) -> f64 {
    let (code, n) = io_loop(op, addr, val, per_block, filler, 2);
    timed(m, &code, Some(d031), d011) as f64 / n as f64
}

// ── D9. what an access costs ─────────────────────────────────────────────────────────────

/// `data/io_cost.txt`: PHI2 cycles per `LDA abs` / `STA abs`, 64 to a block, display off.
/// CIA 1/2, UCI and IO1 are bus cycles (1.0001 at 63x); the VIC, SID and colour RAM run at
/// the turbo clock, a read — and a SID write — one CPU cycle dearer at 63x.
#[test]
fn d9_an_access_costs_what_it_costs_on_the_device() {
    let Some(mut m) = booted() else { return };
    start_tm(&mut m);
    // (what, op, address, value written, 63x, 16x)
    let rows: [(&str, u8, u16, u8, f64, f64); 23] = [
        ("RAM read $C0F0", LDA_ABS, 0xc0f0, 0, 0.0654, 0.2570),
        ("RAM write $C0F0", STA_ABS, 0xc0f0, 0, 0.0653, 0.2570),
        ("CIA1 read $DC00", LDA_ABS, 0xdc00, 0, 1.0001, 1.0002),
        ("CIA1 write $DC03", STA_ABS, 0xdc03, 0x00, 1.0001, 1.0002),
        ("CIA2 read $DD00", LDA_ABS, 0xdd00, 0, 1.0001, 1.0002),
        ("CIA2 write $DD03", STA_ABS, 0xdd03, 0x00, 1.0001, 1.0002),
        ("SID read $D41B", LDA_ABS, 0xd41b, 0, 0.0818, 0.2570),
        ("SID write $D418", STA_ABS, 0xd418, 0x00, 0.0818, 0.2570),
        ("VIC read $D011", LDA_ABS, 0xd011, 0, 0.0818, 0.2570),
        ("VIC read $D012", LDA_ABS, 0xd012, 0, 0.0818, 0.2570),
        ("VIC read $D019", LDA_ABS, 0xd019, 0, 0.0818, 0.2570),
        ("VIC write $D015", STA_ABS, 0xd015, 0x00, 0.0653, 0.2570),
        ("VIC write $D018", STA_ABS, 0xd018, 0x15, 0.0653, 0.2570),
        ("VIC write $D020", STA_ABS, 0xd020, 0x0e, 0.0653, 0.2570),
        ("VIC write $D021", STA_ABS, 0xd021, 0x06, 0.0653, 0.2570),
        ("VIC read $D020", LDA_ABS, 0xd020, 0, 0.0818, 0.2570),
        ("ColRAM read $D800", LDA_ABS, 0xd800, 0, 0.0818, 0.2570),
        ("ColRAM write $D800", STA_ABS, 0xd800, 0x0e, 0.0653, 0.2570),
        ("UCI read $DF1C", LDA_ABS, 0xdf1c, 0, 1.0001, 1.0157),
        ("IO1 read $DE00", LDA_ABS, 0xde00, 0, 1.0001, 1.0157),
        ("turbo read $D031", LDA_ABS, 0xd031, 0, 0.0818, 0.2570),
        ("BASIC ROM read $A000", LDA_ABS, 0xa000, 0, 0.0653, 0.2570),
        ("KERNAL ROM read $E000", LDA_ABS, 0xe000, 0, 0.0653, 0.2570),
    ];
    for (what, op, addr, val, at63, at16) in rows {
        let x63 = phi2_per_access(&mut m, op, addr, val, 0x8f, 0x0b, 64, 0);
        let x16 = phi2_per_access(&mut m, op, addr, val, 0x89, 0x0b, 64, 0);
        let x1 = phi2_per_access(&mut m, op, addr, val, 0x80, 0x0b, 64, 0);
        eprintln!("{what:<22} 63x {x63:.4} (device {at63}) | 16x {x16:.4} (device {at16}) | 1x {x1:.4}");
        assert!((x63 - at63).abs() <= 0.0015, "{what} at 63x: {x63:.4}, the device {at63}");
        // UCI and IO1 at 16x: the device's 1.0157 says one access in 64 — the loop's
        // overhead one — misses its PHI2 cycle, where a CIA read does not. Their lead is
        // longer than a CIA read's; by how much is not pinned. Here they are CIA reads.
        let tol16 = if at16 > 1.01 { 0.02 } else { 0.0015 };
        assert!((x16 - at16).abs() <= tol16, "{what} at 16x: {x16:.4}, the device {at16}");
        assert!((x1 - 4.1115).abs() <= 0.005, "{what} at 1x: {x1:.4}, the device 4.11");
    }
}

/// `data/io_cost_filler.txt`, "refine, 64 MHz, per_block 32": CPU work between two CIA 1
/// accesses is free while it fits. A read stays at one PHI2 cycle up to 54 cycles per
/// access (with the loop's 7 cycles on one access in 32, 1.0314 from 56); a write up to 44
/// (1.0314 at 46-50) and costs two from 52 — issued ~12 cycles before the edge a read is.
/// Badlines: with bit 7 = 1 a CIA read is not slowed; with bit 7 = 0 it costs 1.061.
#[test]
fn d9_a_cia_access_waits_for_its_phi2_cycle_and_a_write_needs_a_lead() {
    let Some(mut m) = booted() else { return };
    start_tm(&mut m);
    // (NOP filler, cycles per access incl. the 4-cycle op, read, write)
    let rows: [(usize, u32, f64, f64); 13] = [
        (17, 38, 1.0002, 1.0003),
        (18, 40, 1.0002, 1.0003),
        (19, 42, 1.0002, 1.0003),
        (20, 44, 1.0002, 1.0003),
        (21, 46, 1.0002, 1.0314),
        (22, 48, 1.0002, 1.0314),
        (23, 50, 1.0003, 1.0314),
        (24, 52, 1.0003, 2.0002),
        (25, 54, 1.0003, 2.0002),
        (26, 56, 1.0314, 2.0002),
        (27, 58, 1.0314, 2.0002),
        (28, 60, 1.0314, 2.0002),
        (29, 62, 1.0314, 2.0002),
    ];
    for (filler, cyc, rd_dev, wr_dev) in rows {
        let r = phi2_per_access(&mut m, LDA_ABS, 0xdc00, 0, 0x8f, 0x0b, 32, filler);
        let w = phi2_per_access(&mut m, STA_ABS, 0xdc03, 0, 0x8f, 0x0b, 32, filler);
        eprintln!("{cyc} cycles per access: read {r:.4} (device {rd_dev}), write {w:.4} (device {wr_dev})");
        assert!((r - rd_dev).abs() <= 0.002, "read, {cyc} cycles: {r:.4}, the device {rd_dev}");
        assert!((w - wr_dev).abs() <= 0.002, "write, {cyc} cycles: {w:.4}, the device {wr_dev}");
    }
    // 64 cycles per access (30 NOPs + the op): a read costs two (device 2.0002).
    let r = phi2_per_access(&mut m, LDA_ABS, 0xdc00, 0, 0x8f, 0x0b, 32, 30);
    assert!((r - 2.0).abs() <= 0.002, "a read 64 cycles after the last costs two: {r:.4}");

    let on_free = phi2_per_access(&mut m, LDA_ABS, 0xdc00, 0, 0x8f, 0x1b, 64, 0);
    let on_stalls = phi2_per_access(&mut m, LDA_ABS, 0xdc00, 0, 0x0f, 0x1b, 64, 0);
    eprintln!("display on: CIA1 read {on_free:.4} with bit 7 = 1, {on_stalls:.4} with bit 7 = 0 (device 1.0001 / 1.0611)");
    assert!((on_free - 1.0001).abs() <= 0.002, "bit 7 = 1: badlines do not slow a CIA read");
    assert!((1.05..=1.07).contains(&on_stalls), "bit 7 = 0: the stalls cost a CIA read ~6 %: {on_stalls:.4}");
}

// ── D10. a colour store paints one pixel ─────────────────────────────────────────────────

/// tm2.py `colour_rows`: per raster line, poll `$D012`, `wait` × `LDA $DC0D` (each one PHI2
/// cycle, ending right after an edge), `d` cycles of NOP/BIT, then `LDA # / STA $D020 /
/// NOP…` for each colour, then the base colour. Runs until `$C0FF` is set.
fn colour_rows(lines: &[(u8, u32, usize)], colours: &[u8], spacing_nops: usize, base: u8) -> Vec<u8> {
    let mut a = Asm::new();
    a.emit(&[0xa9, 0x00]);
    a.abs(STA_ABS, 0xc0ff);
    a.emit(&[0xa9, base]);
    a.abs(STA_ABS, 0xd020);
    let frame = a.pc();
    for &(line, d, wait) in lines {
        a.emit(&[0xa9, line]);
        let w = a.pc();
        a.abs(0xcd, 0xd012);
        let rel = (w.wrapping_sub(a.pc() + 2)) as u8;
        a.emit(&[0xd0, rel]);
        for _ in 0..wait {
            a.abs(LDA_ABS, 0xdc0d);
        }
        let (n2, n3) = if d % 2 == 0 { (d / 2, 0) } else { ((d - 3) / 2, 1) };
        a.emit(&vec![NOP; n2 as usize]);
        if n3 == 1 {
            a.emit(&[0x24, 0xf0]);
        }
        for &c in colours {
            a.emit(&[0xa9, c]);
            a.abs(STA_ABS, 0xd020);
            a.emit(&vec![NOP; spacing_nops]);
        }
        a.emit(&[0xa9, base]);
        a.abs(STA_ABS, 0xd020);
    }
    a.abs(LDA_ABS, 0xc0ff);
    a.emit(&[0xd0, 0x03]);
    a.jmp(frame);
    a.emit(&[0x60]);
    a.b
}

/// Run a `colour_rows` routine for a few frames at 63x, display off, and return the runs
/// `(x, width, colour)` of the stored colours 1-8 in every frame-buffer row that has one,
/// top down. The base colour is 14, outside the stored ones (the device used 6 and scanned
/// every run).
fn painted_rows(m: &mut Machine, code: &[u8], base: u8) -> Vec<Vec<(usize, usize, u8)>> {
    use trx64_core::render::FB_W;
    m.poke(0x4000, code);
    m.poke(0xc001, &[0x8f, 1, 0x0b, 20, 1, 0x35, 32]);
    m.poke(CMD, &[5]);
    // A run's instruction cap is scaled by the divider at its start, which is 1 MHz here
    // until the routine switches; so run by frames, not by one budget.
    let until_frame = |m: &mut Machine, n: u64| {
        let f0 = m.vic.frame;
        while m.vic.frame < f0 + n {
            run(m, FRAME / 8);
        }
    };
    until_frame(m, 4);
    let fb = m.vic.displayed.clone();
    m.poke(0xc0ff, &[1]);
    until_frame(m, 2);
    let mut rows = Vec::new();
    for row in fb.chunks(FB_W) {
        let mut runs = Vec::new();
        let mut x = 0;
        while x < FB_W {
            let c = row[x] & 0x0f;
            let start = x;
            while x < FB_W && row[x] & 0x0f == c {
                x += 1;
            }
            runs.push((start, x - start, c));
        }
        let stored: Vec<(usize, usize, u8)> =
            runs.into_iter().filter(|&(_, _, c)| (1..=8).contains(&c) && c != base).collect();
        if !stored.is_empty() {
            rows.push(stored);
        }
    }
    rows
}

/// `data/q13_pixels.txt`. At 63x, after 25 CIA reads: stores 8 cycles apart paint one pixel
/// each; the first store's pixel moves by one between d = 0 and d = 2 and again between
/// d = 8 and d = 9 — the grid's boundaries at ~7.0 and ~14.9 cycles after the edge — and at
/// d = 8 a later store doubles one pixel. 24 / 25 / 26 CIA reads move it by 8 pixels each.
/// Stores 6 cycles apart lose a colour where two fall in one pixel, the later winning: the
/// device's `2 5 7 1 2 5 7 1` painted `2 5 7 2 5 7`.
#[test]
fn d10_a_colour_store_paints_the_pixel_it_falls_in() {
    let Some(mut m) = booted() else { return };
    start_tm(&mut m);
    let base = 14;
    let colours: Vec<u8> = (1..=8).collect();

    // d = 0, 2..9 on lines 100, 104, …
    let delays = [0u32, 2, 3, 4, 5, 6, 7, 8, 9];
    let lines: Vec<(u8, u32, usize)> = delays.iter().enumerate().map(|(i, &d)| (100 + 4 * i as u8, d, 25)).collect();
    let rows = painted_rows(&mut m, &colour_rows(&lines, &colours, 1, base), base);
    assert_eq!(rows.len(), delays.len(), "one painted row per line: {rows:?}");
    let first: Vec<usize> = rows.iter().map(|r| r[0].0).collect();
    eprintln!("first pixel by d {delays:?}: {first:?}");
    let (mut widths_seen, mut mismatches) = (Vec::new(), Vec::new());
    for (r, d) in rows.iter().zip(delays) {
        let cols: Vec<u8> = r.iter().map(|&(_, _, c)| c).collect();
        let widths: Vec<usize> = r.iter().map(|&(_, w, _)| w).collect();
        eprintln!("d = {d}: {r:?}");
        assert_eq!(cols, colours, "d = {d}: every store is its own pixel run, in order");
        // The device: at d = 0 colour 8 holds two pixels (the base store comes a pixel
        // later); at d = 8 colour 7 does; everywhere else one pixel each.
        let want: Vec<usize> = match d {
            0 => vec![1, 1, 1, 1, 1, 1, 1, 2],
            8 => vec![1, 1, 1, 1, 1, 1, 2, 1],
            _ => vec![1; 8],
        };
        widths_seen.push((d, widths.clone()));
        if widths != want {
            mismatches.push(format!("d = {d}: widths {widths:?}, the device {want:?}"));
        }
    }
    eprintln!("widths: {widths_seen:?}");
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    let x0 = first[0];
    assert!(first[1..8].iter().all(|&x| x == x0 + 1), "d = 2..8 paint from the next pixel: {first:?}");
    assert_eq!(first[8], x0 + 2, "d = 9 from the one after");

    // 24 / 25 / 26 CIA reads, and stores 6 cycles apart.
    let lines = [(100u8, 0u32, 24usize), (104, 0, 25), (108, 0, 26)];
    let rows = painted_rows(&mut m, &colour_rows(&lines, &colours, 1, base), base);
    let first: Vec<usize> = rows.iter().map(|r| r[0].0).collect();
    assert_eq!(first, vec![first[0], first[0] + 8, first[0] + 16], "one PHI2 cycle = 8 pixels");
    let rows = painted_rows(&mut m, &colour_rows(&[(100, 0, 25)], &[2, 5, 7, 1, 2, 5, 7, 1], 0, base), base);
    eprintln!("6 cycles apart: {:?}", rows[0]);
    // The device's row: 2 5 7 2 5 7, one pixel each — both 1s fell into a pixel the next
    // store also hit, and the later store won. (The loop polls line 100 again at once, so
    // the same line carries a second pass further right; the device's stream window
    // stops before it.)
    let x = rows[0][0].0;
    let want: Vec<(usize, usize, u8)> = [2u8, 5, 7, 2, 5, 7].iter().enumerate().map(|(i, &c)| (x + i, 1, c)).collect();
    assert_eq!(rows[0][..6], want[..], "eight stores 6 cycles apart: six pixels, the 1s overwritten");
    assert!(rows[0].get(6).is_none_or(|r| r.0 > x + 6), "and then the base colour");
}

// ── D11. the mirrors ─────────────────────────────────────────────────────────────────────

/// tm2.py `poke_read`: store each (addr, value), then read each address into `$C0F0+i`.
fn poke_read(m: &mut Machine, writes: &[(u16, u8)], reads: &[u16]) -> Vec<u8> {
    let mut a = Asm::new();
    for &(addr, v) in writes {
        a.emit(&[0xa9, v]);
        a.abs(STA_ABS, addr);
    }
    for (i, &addr) in reads.iter().enumerate() {
        a.abs(LDA_ABS, addr);
        a.abs(STA_ABS, 0xc0f0 + i as u16);
    }
    a.emit(&[0x60]);
    timed(m, &a.b, None, 0x0b);
    (0..reads.len() as u16).map(|i| rd(m, 0xc0f0 + i)).collect()
}

const TURBO_REGS: [u16; 11] = [0xd030, 0xd031, 0xd070, 0xd071, 0xd0b0, 0xd0b1, 0xd0f0, 0xd0f1, 0xd0bc, 0xd07a, 0xd07b];

fn x_now(m: &mut Machine, k: u8) -> f64 {
    measure(m, Params { d011: 0x0b, tenths: 10, k, ..Params::default() }).x
}

/// `data/leftovers.txt`: `$D030/$D031` read the same at `$D070/71`, `$D0B0/B1`, `$D0F0/F1`,
/// and a store to a mirror acts — `$D071 = $89` gave 16x, `$D0B1 = $8C` 32x.
#[test]
fn d11_the_turbo_registers_are_mirrored_every_40() {
    let Some(mut m) = booted() else { return };
    start_tm(&mut m);
    m.set_u64_turbo(0x01, 0x09);
    let r = poke_read(&mut m, &[(0xd071, 0x89)], &TURBO_REGS);
    assert_eq!(r, vec![0xff, 0x89, 0xff, 0x89, 0xff, 0x89, 0xff, 0x89, 0xff, 0xff, 0xff]);
    assert!(close(x_now(&mut m, 16), 16.0, 0.004), "`$D071 = $89`: 16x");
    let r = poke_read(&mut m, &[(0xd0b1, 0x8c)], &TURBO_REGS);
    assert_eq!(r[1], 0x8c, "`$D0B1 = $8C`: `$D031` reads $8C");
    assert!(close(x_now(&mut m, 32), 32.0, 0.004), "and runs 32x");
}

// ── D12. TurboEnable-bit mode ────────────────────────────────────────────────────────────

/// `data/leftovers.txt`, menu 16 MHz / Badline Timing Disabled, "TurboEnable Bit": after a
/// reset `$D030` reads `$FE`, `$D031` `$00`, 1x; `$D030 = 1` → `$FF`, `$D031` the menu's
/// `$89`, 16x; `$D030 = 0` → back; `$D031` is writable (`$83` → 4.02x); a menu change to
/// 32 → `$8C`, 32x. In registers mode `$D030` reads `$FF` and a store to it does nothing.
#[test]
fn d12_turboenable_bit_mode_as_measured() {
    let Some(mut m) = booted() else { return };
    m.set_u64_turbo(0x05, 0x09);
    reset_and_restart(&mut m);
    let r = poke_read(&mut m, &[], &TURBO_REGS);
    assert_eq!(r, vec![0xfe, 0x00, 0xfe, 0x00, 0xfe, 0x00, 0xfe, 0x00, 0xff, 0xff, 0xff], "after a reset");
    assert!(close(x_now(&mut m, 1), 1.0, 0.005), "1x after a reset");

    let r = poke_read(&mut m, &[(0xd030, 0x01)], &TURBO_REGS);
    assert_eq!(r, vec![0xff, 0x89, 0xff, 0x89, 0xff, 0x89, 0xff, 0x89, 0xff, 0xff, 0xff], "`$D030 = 1`");
    assert!(close(x_now(&mut m, 16), 16.0, 0.004), "the menu's 16x");

    let r = poke_read(&mut m, &[(0xd030, 0x00)], &TURBO_REGS);
    assert_eq!(r[..2], [0xfe, 0x00], "`$D030 = 0`: back");
    assert!(close(x_now(&mut m, 1), 1.0, 0.005), "1x");

    let r = poke_read(&mut m, &[(0xd030, 0x01), (0xd031, 0x83)], &TURBO_REGS);
    assert_eq!(r[..2], [0xff, 0x83], "`$D031` is writable in this mode");
    assert!(close(x_now(&mut m, 4), 4.02, 0.006), "4x");

    m.set_u64_turbo(0x05, 0x0c);
    let r = poke_read(&mut m, &[], &TURBO_REGS);
    assert_eq!(r[..2], [0xff, 0x8c], "a menu change to 32");
    assert!(close(x_now(&mut m, 32), 32.0, 0.004), "32x");

    m.set_u64_turbo(0x01, 0x0c); // U64 Turbo Registers
    let r = poke_read(&mut m, &[(0xd030, 0x00)], &[0xd030, 0xd031]);
    assert_eq!(r, vec![0xff, 0x8c], "registers mode: `$D030` reads $FF and a store does nothing");
    assert!(close(x_now(&mut m, 32), 32.0, 0.004), "still 32x");
}

// ── D13. `$D07A`, `$D07B`, `$D0BC` ───────────────────────────────────────────────────────

/// `data/leftovers.txt`, registers mode, menu 16 / Disabled: `$D031 = $85` → 8x; a store to
/// `$D07A` → `$D031` `$00`, 1x; to `$D07B` → the MENU's `$89`, 16x — not the `$85` the
/// program wrote. Both read `$FF`. `$D0BC` reads `$01` with SuperCPU Detect enabled, `$FF`
/// disabled.
#[test]
fn d13_the_supercpu_switches_and_d0bc() {
    let Some(mut m) = booted() else { return };
    start_tm(&mut m);
    m.set_u64_turbo(0x01, 0x09);
    let r = poke_read(&mut m, &[(0xd031, 0x85)], &TURBO_REGS);
    assert_eq!(r[1], 0x85);
    assert!(close(x_now(&mut m, 8), 8.0, 0.004), "8x");
    let r = poke_read(&mut m, &[(0xd07a, 0x00)], &TURBO_REGS);
    assert_eq!(r, vec![0xff, 0x00, 0xff, 0x00, 0xff, 0x00, 0xff, 0x00, 0xff, 0xff, 0xff], "after `$D07A`");
    assert!(close(x_now(&mut m, 1), 1.0, 0.005), "`$D07A`: 1x");
    let r = poke_read(&mut m, &[(0xd07b, 0x00)], &TURBO_REGS);
    assert_eq!(r[1], 0x89, "`$D07B` loads the menu's $89, not the program's $85");
    assert!(close(x_now(&mut m, 16), 16.0, 0.004), "16x");

    // TurboEnable mode alike: `$D031 = $83`, then `$D07A` 1x, `$D07B` 16x.
    m.set_u64_turbo(0x05, 0x09);
    poke_read(&mut m, &[(0xd030, 0x01), (0xd031, 0x83)], &[]);
    assert!(close(x_now(&mut m, 4), 4.02, 0.006));
    poke_read(&mut m, &[(0xd07a, 0x00)], &[]);
    assert!(close(x_now(&mut m, 1), 1.0, 0.005), "TurboEnable: `$D07A` 1x");
    poke_read(&mut m, &[(0xd07b, 0x00)], &[]);
    assert!(close(x_now(&mut m, 16), 16.0, 0.004), "TurboEnable: `$D07B` 16x");

    m.set_u64_turbo(0x03, 0x09); // registers mode + SuperCPU Detect
    assert_eq!(poke_read(&mut m, &[], &[0xd0bc]), vec![0x01], "SuperCPU Detect Enabled");
    m.set_u64_turbo(0x01, 0x09);
    assert_eq!(poke_read(&mut m, &[], &[0xd0bc]), vec![0xff], "Disabled");
}

// ── D14. any setting re-applies the menu speed ───────────────────────────────────────────

/// `data/leftovers.txt`: with the program's `$8C` in force, toggling SuperCPU Detect —
/// unrelated to speed — turned `$D031` back to the menu's `$89`.
#[test]
fn d14_an_unrelated_setting_reapplies_the_menu_speed() {
    let Some(mut m) = booted() else { return };
    start_tm(&mut m);
    m.set_u64_turbo(0x01, 0x09);
    poke_read(&mut m, &[(0xd031, 0x8c)], &[]);
    assert!(close(x_now(&mut m, 32), 32.0, 0.004), "the program's 32x");
    m.set_u64_turbo(0x03, 0x09); // SuperCPU Detect Enabled, speed untouched
    assert_eq!(poke_read(&mut m, &[], &[0xd031]), vec![0x89], "the menu's $89 again");
    assert!(close(x_now(&mut m, 16), 16.0, 0.004), "16x");
    poke_read(&mut m, &[(0xd031, 0x8c)], &[]);
    m.u64_settings_changed(); // any other item of the category
    assert_eq!(poke_read(&mut m, &[], &[0xd031]), vec![0x89], "any item of the category");
}

// ── D15. the post-reset hold ─────────────────────────────────────────────────────────────

/// `resethold.bin` (CBM80 at `$8000`, so the KERNAL jumps into it straight after a reset)
/// counts PHI2 ticks from its first instruction until its loop gets fast. Device, REST
/// `machine:reset`: Manual 16 → 4,194,236 (three runs), Manual 64 → 4,194,166; registers
/// mode with `$D031 = $89` written at the first instruction → 4,194,244 (the write is kept);
/// registers mode without a write → never fast. 2^22 = 4,194,304 PHI2 cycles in all.
#[test]
fn d15_the_hold_is_2_to_the_22_phi2_cycles() {
    const PRG: &[u8] = include_bytes!("fixtures/turbomeas/resethold.bin");
    let probe = |regs_en: u8, prefer: u8, write: Option<u8>| -> Option<u32> {
        let mut m = booted()?;
        m.set_u64_turbo(regs_en, prefer);
        m.poke(0x8000, PRG);
        m.poke(0x8100, &[u8::from(write.is_some()), write.unwrap_or(0)]);
        m.poke(0x8110, &[0; 16]);
        m.warm_reset();
        run(&mut m, 6 * PHI2_HZ as u64);
        assert!(rd(&m, 0x811b) != 0 || rd(&m, 0x811a) == 0x5a, "the probe ran");
        if rd(&m, 0x811a) != 0x5a {
            return None;
        }
        let left = (0..4).fold(0u32, |v, i| v << 8 | u32::from(rd(&m, 0x8110 + i)));
        Some(0xffff_ffff - left)
    };
    for (what, regs_en, prefer, write, device) in [
        ("Manual 16", 0x00u8, 0x09u8, None, 4_194_236u32),
        ("Manual 64", 0x00, 0x0f, None, 4_194_166),
        ("registers, $D031 = $89 at the first instruction", 0x01, 0x80, Some(0x89u8), 4_194_244),
    ] {
        let Some(t) = probe(regs_en, prefer, write) else {
            if rom_dir().join("kernal-901227-03.bin").exists() {
                panic!("{what}: the loop never got fast");
            }
            return;
        };
        eprintln!("{what}: fast after {t} PHI2 ticks (device {device})");
        assert!(t.abs_diff(device) <= 120, "{what}: {t}, the device {device}");
    }
    if rom_dir().join("kernal-901227-03.bin").exists() {
        assert_eq!(probe(0x01, 0x89, None), None, "registers mode without a write stays at 1 MHz");
    }
}

