//! Issue #6 — the CLK release of the 1541's "file not found" turnaround, and the KERNAL's
//! TKSA wait that can miss it.
//!
//! A LOAD of a file that is not on the disk ends the drive's TALK + secondary address with
//! one short CLK pulse: ROM `$EA4E: LDA #0 / $EA50: STA $1800` (the store that RELEASES CLK)
//! after the pull at `$E9AE`. The C64 waits for that pulse in TKSA, `$EDD6: JSR $EEA9 /
//! BMI $EDD6`; `$EEA9` is `LDA $DD00 / CMP $DD00 / BNE` — two reads, 4 cycles apart, that
//! must agree. A badline stall that lands between the two reads while the release falls
//! between them makes them disagree; the pulse is then gone and the loop has no timeout.
//!
//! What is pinned here is WHEN the C64 sees the release, derived from the source of the
//! reference and not from this emulator's own drive/IEC code:
//!
//!   * `iecbus_cpu_read_conf1` runs `drive_cpu_execute_all(maincpu_clk)` before it returns
//!     `cpu_port`; a `$DD00` read happens at the clock of the read cycle, before that
//!     cycle's `CLK_INC` (`mainc64cpu.c` `LOAD` → `mem_read_check_ba`).
//!   * `drivecpu_execute` raises `stop_clk` by `sync_factor * (clk - last_clk) >> 16`
//!     (`drivecpu.c:383-390`, `sync_factor = floor(65536 * 1e6 / 985248)`, `drivesync.c:57`)
//!     and runs whole instructions while `*clk_ptr < stop_clk` (`drivecpu.c:393`).
//!   * the store reaches the bus inside the instruction that executes it (`via1d1541.c:
//!     store_prb` writes `iecbus->drv_bus`/`cpu_port` at once, with no timestamp of its own),
//!     so the C64 sees the release at the first `$DD00` read whose `stop_clk` is above the
//!     drive clock at which the `STA` BEGINS.
//!   * both clocks start together at reset (`mainc64cpu.c:643` sets `maincpu_clk = 6` and
//!     `machine_reset` → `drive_reset` sets `last_clk = maincpu_clk`), so
//!     `stop_clk(T) = floor(sync_factor * T / 65536)` for T counted from the reset.
//!
//! Hence the release is visible to a read at C64 clock T exactly when
//! `floor(sf * T / 65536) > D`, D being the drive clock at which `STA $1800` starts: the
//! first such T is `ceil((D + 1) * 65536 / sf)`.
//!
//! The test runs a LOAD of a missing file through the real KERNAL on a generated D64, with
//! the display on, at every start phase over one badline period (504 cycles = 8 lines), and
//! checks that
//!   * every sample of `$DD00` in the TKSA wait agrees with that rule, cycle for cycle;
//!   * the phases that hang are exactly the ones whose `LDA`/`CMP` pair straddles the release;
//!   * every phase that does not hang ends in `?FILE NOT FOUND` (carry set, A = 4).
//!
//! It is a race in the KERNAL, and a faithful one: the set of hanging phases is not empty
//! (11 of the 505 phases swept, ~2 %). Before the drive's catch-up clock lost a one-cycle
//! seed that the reference does not have, every read made exactly one cycle before the
//! first visible clock saw the release already, and the hanging set did not match.
//!
//! The C64 clock of this machine starts at 0 at the first instruction, the reference's at 6,
//! and the drive's anchor moves with it; only the distance between the two matters, and in
//! the reference it is zero.
//!
//!   TRX64_ROM_DIR=<dir with kernal + 1541 ROMs> \
//!     cargo test --release -p trx64-core --test issue6_clk_release_race -- --nocapture

use std::path::{Path, PathBuf};
use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::{BusKind, Machine, NullSink, Observer};

const DOS_ROM: &str = "dos1541-325302-01+901229-05.bin";
const FRAME: u64 = 19_656;
/// 8 lines x 63 cycles: one badline period.
const PERIOD: u32 = 504;
/// The LOAD of a missing file takes ~1.54 M cycles; past this it is stuck.
const HANG_AFTER: u64 = 3_000_000;
const LOAD_NAME_AT: u16 = 0xc800;
const RESULT_AT: u16 = 0xc810;

fn rom_dir() -> PathBuf {
    match std::env::var_os("TRX64_ROM_DIR") {
        Some(d) => PathBuf::from(d),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../C64ReverseEngineeringMCP/resources/roms"),
    }
}

fn roms_present(d: &Path) -> bool {
    d.join("kernal-901227-03.bin").exists() && d.join(DOS_ROM).exists()
}

// ── a tiny formatted D64, generated here ────────────────────────────────────────

fn sectors_per_track(t: u8) -> usize {
    match t {
        1..=17 => 21,
        18..=24 => 19,
        25..=30 => 18,
        _ => 17,
    }
}

/// A blank, formatted 35-track D64: BAM on 18/0, an empty directory on 18/1.
fn blank_d64() -> Vec<u8> {
    let mut d = vec![0u8; 174_848];
    let bam: usize = (1..18).map(sectors_per_track).sum::<usize>() * 256;
    d[bam] = 18;
    d[bam + 1] = 1;
    d[bam + 2] = 0x41;
    for t in 1..=35u8 {
        let n = sectors_per_track(t);
        let e = bam + 4 + (t as usize - 1) * 4;
        let used: u32 = if t == 18 { 0b11 } else { 0 };
        let free: u32 = ((1u32 << n) - 1) & !used;
        d[e] = free.count_ones() as u8;
        d[e + 1] = free as u8;
        d[e + 2] = (free >> 8) as u8;
        d[e + 3] = (free >> 16) as u8;
    }
    for i in 0..16 {
        d[bam + 0x90 + i] = if i < 4 { b"RACE"[i] } else { 0xa0 };
    }
    d[bam + 0xa0] = 0xa0;
    d[bam + 0xa1] = 0xa0;
    d[bam + 0xa2] = b'R';
    d[bam + 0xa3] = b'C';
    d[bam + 0xa4] = 0xa0;
    d[bam + 0xa5] = b'2';
    d[bam + 0xa6] = b'A';
    for i in 0xa7..0xab {
        d[bam + i] = 0xa0;
    }
    d[bam + 256 + 1] = 0xff;
    d
}

// ── the C64 side: the stub and what it samples ──────────────────────────────────

/// SETLFS 1,8,0 / SETNAM "ZZ" / LOAD (A=0) through the KERNAL jump table, then the result
/// (A on return, P, ST) into `RESULT_AT`..+2 and the done flag `$AA` at +3; `delay` cycles
/// of NOP / BIT $FF come first so the LOAD starts `delay` cycles later.
fn stub(delay: u32) -> Vec<u8> {
    let mut s = vec![];
    let mut d = delay;
    if d % 2 == 1 {
        assert!(d >= 3, "a one-cycle delay cannot be built");
        s.extend_from_slice(&[0x24, 0xff]); // BIT $FF, 3 cycles
        d -= 3;
    }
    s.extend(std::iter::repeat(0xea).take((d / 2) as usize)); // NOP, 2 cycles each
    s.extend_from_slice(&[0xa9, 0x01, 0xa2, 0x08, 0xa0, 0x00, 0x20, 0xba, 0xff]); // SETLFS
    s.extend_from_slice(&[0xa9, 0x02, 0xa2, LOAD_NAME_AT as u8, 0xa0, (LOAD_NAME_AT >> 8) as u8, 0x20, 0xbd, 0xff]); // SETNAM
    s.extend_from_slice(&[0xa9, 0x00, 0x20, 0xd5, 0xff]); // LOAD
    let r = RESULT_AT;
    s.extend_from_slice(&[0x08, 0x8d, r as u8, (r >> 8) as u8]); // PHP / STA r
    s.extend_from_slice(&[0x68, 0x8d, (r + 1) as u8, ((r + 1) >> 8) as u8]); // PLA / STA r+1
    s.extend_from_slice(&[0xa5, 0x90, 0x8d, (r + 2) as u8, ((r + 2) >> 8) as u8]); // LDA $90 / STA r+2
    s.extend_from_slice(&[0xa9, 0xaa, 0x8d, (r + 3) as u8, ((r + 3) >> 8) as u8]); // done
    let here = 0xc000 + s.len() as u16;
    s.extend_from_slice(&[0x4c, here as u8, (here >> 8) as u8]); // JMP *
    s
}

/// The `$DD00` samples of the TKSA wait loop (`$EDD6 .. $EDDB`): the clock of the read cycle
/// and the byte read. The loop is entered by the retiring `JSR $EEA9` at `$EDD6` and left at `CLI`
/// (`$EDDB`).
#[derive(Default)]
struct TksaReads {
    inside: bool,
    reads: Vec<(u64, u8)>,
}

impl Observer for TksaReads {
    fn on_instruction(&mut self, pc: u16, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u64) {
        if pc == 0xedd6 {
            self.inside = true;
        } else if pc == 0xeddb {
            self.inside = false;
        }
    }
    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, pc: u16, clk: u64, _old: u8) {
        // The CIA read is reported twice (the IEC pins, then the load itself); keep one.
        if self.inside
            && kind == BusKind::Read
            && addr == 0xdd00
            && (0xeea9..=0xeeb2).contains(&pc)
            && self.reads.last().map(|r| r.0) != Some(clk)
            && self.reads.len() < 20_000
        {
            self.reads.push((clk, value));
        }
    }
    fn on_interrupt(&mut self, _: u16, _: u64) {}
}

fn run(m: &mut Machine, cycles: u64, obs: &mut impl Observer) {
    m.run_for_full(cycles, obs, |_, _, _, _, _, _, _| {});
}

/// The machine at READY with the disk in drive 8, display on.
fn booted(rom: &Path) -> Machine {
    let mut m = Machine::new();
    m.boot_from_dir(rom).expect("boot ROMs");
    let mut sink = NullSink;
    for _ in 0..130 {
        run(&mut m, FRAME, &mut sink);
    }
    m.drive8.attach_disk(DiskImage { kind: DiskKind::D64, bytes: blank_d64(), backing_path: None, read_only: false });
    for _ in 0..40 {
        run(&mut m, FRAME, &mut sink);
    }
    assert!(m.read_full(0xd011) & 0x10 != 0, "display is on");
    m.poke(LOAD_NAME_AT, b"ZZ");
    m
}

/// `base` with the stub for `delay` entered at its next instruction boundary.
fn started(base: &Machine, delay: u32) -> Machine {
    let mut m = base.clone();
    m.poke(0xc000, &stub(delay));
    m.poke(RESULT_AT + 3, &[0]);
    m.c64_core.reg_pc = 0xc000;
    m
}

struct Phase {
    done: bool,
    a: u8,
    p: u8,
    reads: Vec<(u64, u8)>,
}

fn run_phase(base: &Machine, delay: u32) -> Phase {
    let mut m = started(base, delay);
    let mut obs = TksaReads::default();
    let start = m.c64_core.clk;
    let mut done = false;
    while m.c64_core.clk - start < HANG_AFTER {
        run(&mut m, 10_000, &mut obs);
        if m.read_full(RESULT_AT + 3) == 0xaa {
            done = true;
            break;
        }
    }
    Phase { done, a: m.read_full(RESULT_AT), p: m.read_full(RESULT_AT + 1), reads: obs.reads }
}

// ── the drive side: where the releasing STA begins ──────────────────────────────

/// The drive clock at which `STA $1800` at `$EA50` begins.
///
/// The phase is replayed to just before the release and stepped one C64 instruction at a
/// time until the drive's CLK pull (VIA1 PB bit 3) lets go. A C64 instruction is not always
/// a few cycles: across a badline stall it is ~46, and the drive runs ~46 cycles inside it, so
/// the instruction BEFORE the release is kept, and its drive replayed one C64 cycle at a time
/// (`run_cycles(1)`: whole drive instructions while the clock is below the target, the one
/// rule everything here rests on) to see at which drive clock the `STA` starts.
fn release_start(base: &Machine, delay: u32, from_clk: u64) -> u64 {
    let mut m = started(base, delay);
    let mut sink = NullSink;
    while m.c64_core.clk + 10_000 < from_clk {
        run(&mut m, 10_000, &mut sink);
    }
    let pull = |m: &Machine| m.drive8.via1_pb_iec_output() & 0x08 != 0;
    // Step to the instruction that ends with the release, counting, then replay up to the one
    // before it (a machine clone is not cheap, so there is exactly one).
    let snap = m.clone();
    let mut steps = 0;
    let mut was = pull(&m);
    loop {
        run(&mut m, 1, &mut sink);
        steps += 1;
        let now = pull(&m);
        if was && !now {
            break;
        }
        was = now;
        assert!(steps < 50_000, "the drive never released CLK after C64 clock {from_clk}");
    }
    let mut c = snap;
    for _ in 0..steps - 1 {
        run(&mut c, 1, &mut sink);
    }
    assert!(pull(&c), "replay: CLK is pulled until the release instruction");
    c.drive8.feed_iec(&c.iec);
    for _ in 0..400 {
        let (pc, clk, was) = (c.drive8.core.reg_pc, c.drive8.core.clk, pull(&c));
        c.drive8.run_cycles(1);
        let released = was && !pull(&c);
        match pc {
            // STA abs begins here.
            0xea50 if c.drive8.core.clk != clk => {
                assert!(released, "the STA at $EA50 did not release CLK");
                return clk;
            }
            // LDA #0 (2 cycles), then the STA in the same step.
            0xea4e if released => return clk + 2,
            _ => {}
        }
    }
    panic!("the drive's STA $1800 at $EA50 was not found in the slice before the release");
}

/// The first C64 clock at which the release is visible: `floor(sf * T / 65536) > d`.
fn first_visible(sync_factor: u32, drive_start: u64) -> u64 {
    let sf = sync_factor as u64;
    (drive_start + 1).saturating_mul(65_536).div_ceil(sf)
}

// ── the sweep ───────────────────────────────────────────────────────────────────

struct Row {
    delay: u32,
    done: bool,
    a: u8,
    p: u8,
    /// Where the pulse starts: the raster line of its first low sample.
    line: u64,
    /// Every `$DD00` sample from the first low one on: (clock - first visible clock, read low).
    samples: Vec<(i64, bool)>,
    straddles: bool,
    t_star: u64,
    drive_start: u64,
}

fn measure(base: &Machine, sf: u32, delay: u32) -> Row {
    let ph = run_phase(base, delay);
    let first_low = ph.reads.iter().position(|r| r.1 & 0x40 == 0).unwrap_or_else(|| panic!("delay {delay}: the C64 never saw the CLK pulse"));
    let last_low_clk = ph.reads.iter().filter(|r| r.1 & 0x40 == 0).map(|r| r.0).max().unwrap();
    let drive_start = release_start(base, delay, last_low_clk.saturating_sub(300));
    let t_star = first_visible(sf, drive_start);
    let seen = &ph.reads[first_low..];
    // A pair is LDA, CMP. It hangs when the LDA is before the release and the CMP after.
    let straddles = seen.chunks_exact(2).any(|p| p[0].0 < t_star && p[1].0 >= t_star);
    Row {
        delay,
        done: ph.done,
        a: ph.a,
        p: ph.p,
        line: (seen[0].0 % FRAME) / 63,
        samples: seen.iter().map(|&(c, v)| (c as i64 - t_star as i64, v & 0x40 == 0)).collect(),
        straddles,
        t_star,
        drive_start,
    }
}

#[test]
fn clk_release_is_seen_at_the_reference_cycle_and_the_kernal_race_is_exactly_that_window() {
    let rom = rom_dir();
    if !roms_present(&rom) {
        eprintln!("[SKIP] issue6_clk_release_race: ROMs absent at {} (set TRX64_ROM_DIR)", rom.display());
        return;
    }
    let mut base = booted(&rom);
    let sf = base.drive8.sync_factor;
    assert_eq!(sf, (65536.0f64 * (1_000_000.0 / 985_248.0)).floor() as u32, "PAL sync factor");

    // The pulse comes ~1.53 M cycles after the LOAD starts, at the clock the drive's search of
    // the disk ends, and it moves with the start: to put it where a badline stall can reach it,
    // the LOAD has to start at a point of the frame that puts the pulse among the display
    // lines. (At the READY prompt's own phase it lands on lines 257-261, under the display,
    // where there is no badline and nothing to test.) Each phase then asserts that.
    run(&mut base, 30_000, &mut NullSink);

    // Delay 1 cannot be built; 505 stands in for it (the same phase one period on).
    let jobs: Vec<u32> = std::iter::once(0).chain(2..=PERIOD + 1).collect();
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(10);
    let copies: Vec<Machine> = (0..workers).map(|_| base.clone()).collect();
    let mut rows: Vec<Row> = std::thread::scope(|s| {
        let hs: Vec<_> = copies
            .into_iter()
            .enumerate()
            .map(|(w, m)| {
                let jobs = &jobs;
                s.spawn(move || jobs.iter().skip(w).step_by(workers).map(|&d| measure(&m, sf, d)).collect::<Vec<_>>())
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    rows.sort_by_key(|r| r.delay);

    let hang: Vec<u32> = rows.iter().filter(|r| !r.done).map(|r| r.delay).collect();
    let predicted: Vec<u32> = rows.iter().filter(|r| r.straddles).map(|r| r.delay).collect();
    let mut margins = std::collections::BTreeMap::<i64, usize>::new();
    let mut bad: Vec<String> = vec![];
    for r in &rows {
        // Badlines run from line $30 to $F7; the stall is cycles 12-54 of the line.
        assert!((50..=244).contains(&r.line), "delay {}: the pulse starts on raster line {}, outside the badline lines", r.delay, r.line);
        for &(off, low) in &r.samples {
            *margins.entry(off).or_default() += 1;
            // Low exactly while the read is before the first visible clock.
            if low != (off < 0) {
                bad.push(format!(
                    "delay {}: a $DD00 read {off:+} cycles from the first visible clock {} (drive STA begins at {}) saw CLK {}",
                    r.delay, r.t_star, r.drive_start, if low { "low" } else { "high" }
                ));
            }
        }
        if r.done {
            assert!(
                r.p & 1 != 0 && r.a == 4,
                "delay {}: LOAD of a missing file must end carry set, A=4 (FILE NOT FOUND); got A={:02x} P={:02x}",
                r.delay, r.a, r.p
            );
        }
    }
    eprintln!(
        "{} phases: {} FILE NOT FOUND, {} hang; hanging start delays = {hang:?}",
        rows.len(),
        rows.iter().filter(|r| r.done).count(),
        hang.len()
    );
    eprintln!("samples nearest the release, read clock - first visible clock -> count: {:?}", margins.range(-80..=10).collect::<Vec<_>>());
    assert!(bad.is_empty(), "{} samples disagree with the reference rule; first: {:#?}", bad.len(), &bad[..bad.len().min(8)]);
    assert_eq!(hang, predicted, "the hanging phases are not the phases whose LDA/CMP pair straddles the release");
    assert!(!hang.is_empty(), "the race has a window: some phase must hang");
}
