//! Spec 876 gate — the POT lines at `$D419`/`$D41A`.
//!
//! Every value here is read by the CPU through the bus (`LDA $D419`), with the read's PHI2
//! cycle taken from the bus record, and by the peek the monitor uses. The spec's §12 numbers
//! the cases; each test names its number.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use trx64_core::c64re_snapshot::{capture_runtime_checkpoint_opts, restore_runtime_checkpoint};
use trx64_core::expansion::Hold;
use trx64_core::model;
use trx64_core::native_snapshot::read_native_snapshot;
use trx64_core::sid::SidMapping;
use trx64_core::vic::SpeedProfile;
use trx64_core::{BusKind, Machine, NullSink, Observer};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pot");
/// PAL: 63 cycles × 312 lines.
const FRAME: u64 = 19_656;

fn roms() -> bool {
    let ok = Path::new(ROM_DIR).join("kernal-901227-03.bin").exists();
    if !ok {
        eprintln!("SKIP: ROMs absent ({ROM_DIR})");
    }
    ok
}

// ── the bus record ────────────────────────────────────────────────────────────────────────

/// Every CPU read of `$D419`/`$D41A` and every write of `$DC00`/`$DC02`, with its cycle.
#[derive(Default)]
struct Rec {
    reads: Vec<(u64, u16, u8)>,
    writes: Vec<(u64, u16, u8)>,
}

impl Observer for Rec {
    fn on_instruction(&mut self, _: u16, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u64) {}
    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, _pc: u16, clk: u64, _old: u8) {
        match kind {
            BusKind::Read | BusKind::DummyRead if addr == 0xd419 || addr == 0xd41a => {
                self.reads.push((clk, addr, value))
            }
            BusKind::Write if addr == 0xdc00 || addr == 0xdc02 => self.writes.push((clk, addr, value)),
            _ => {}
        }
    }
    fn on_interrupt(&mut self, _: u16, _: u64) {}
}

// ── a few bytes of 6502 ───────────────────────────────────────────────────────────────────

const SEI: u8 = 0x78;

/// `LDA #v / STA addr`
fn sta(addr: u16, v: u8) -> [u8; 5] {
    [0xa9, v, 0x8d, addr as u8, (addr >> 8) as u8]
}

/// Exactly `d` cycles (d ≥ 2): NOPs, led by one `BIT $FB` when `d` is odd.
fn delay(d: u64) -> Vec<u8> {
    assert!(d >= 2, "no instruction takes one cycle");
    let mut v = Vec::new();
    let mut left = d;
    if left % 2 == 1 {
        v.extend_from_slice(&[0x24, 0xfb]);
        left -= 3;
    }
    v.extend(std::iter::repeat_n(0xea, (left / 2) as usize));
    v
}

/// `LDY #0 / DEY / BNE` × `outer`: 1 280 CPU cycles each, well past a 512-cycle sample at
/// 1 MHz.
fn settle_loop(outer: u8) -> Vec<u8> {
    vec![0xa2, outer, 0xa0, 0x00, 0x88, 0xd0, 0xfd, 0xca, 0xd0, 0xf8]
}

/// Selection writes, a wait past a sample, `$D419 → $FB`, `$D41A → $FC`, then `JMP *`.
fn read_after(select: &[(u16, u8)]) -> Vec<u8> {
    let mut p = vec![SEI];
    for &(a, v) in select {
        p.extend_from_slice(&sta(a, v));
    }
    p.extend(settle_loop(1));
    p.extend_from_slice(&[0xad, 0x19, 0xd4, 0x85, 0xfb, 0xad, 0x1a, 0xd4, 0x85, 0xfc]);
    let here = 0xc000 + p.len() as u16;
    p.extend_from_slice(&[0x4c, here as u8, (here >> 8) as u8]);
    p
}

fn run_program(m: &mut Machine, code: &[u8], cycles: u64) {
    m.poke(0xc000, code);
    m.c64_core.reg_pc = 0xc000;
    m.run_for_full(cycles, &mut NullSink, |_, _, _, _, _, _, _| {});
}

/// What the CPU reads from `$D419`/`$D41A` under `select`, and what the peek says after.
fn cpu_pot(m: &mut Machine, select: &[(u16, u8)]) -> (u8, u8) {
    run_program(m, &read_after(select), 3_000);
    let got = (m.read_full(0x00fb), m.read_full(0x00fc));
    assert_eq!((m.read_full(0xd419), m.read_full(0xd41a)), got, "the peek shows what the CPU read");
    assert_eq!((m.peek_lens(0xd419, "io"), m.peek_lens(0xd41a, "io")), got, "and so does the io lens");
    got
}

const PORT1: [(u16, u8); 2] = [(0xdc02, 0xc0), (0xdc00, 0x40)];
const PORT2: [(u16, u8); 2] = [(0xdc02, 0xc0), (0xdc00, 0x80)];
const BOTH: [(u16, u8); 2] = [(0xdc02, 0xc0), (0xdc00, 0xc0)];
const NEITHER: [(u16, u8); 2] = [(0xdc02, 0xc0), (0xdc00, 0x00)];

// ── §12.1 — the default ───────────────────────────────────────────────────────────────────

#[test]
fn g01_nothing_set_reads_ff_under_every_selection() {
    for select in [&NEITHER[..], &PORT1[..], &PORT2[..], &BOTH[..], &[(0xdc02, 0x00)][..]] {
        let mut m = Machine::new();
        assert_eq!(cpu_pot(&mut m, select), (0xff, 0xff), "selection {select:02x?}");
    }
}

#[test]
fn g01_an_extra_sid_reads_ff_on_its_pot_registers() {
    let mut m = Machine::new();
    m.set_sid_map(vec![SidMapping::window(0xd400, 0, true), SidMapping::window(0xd420, 1, true)]);
    m.set_pot(1, 0x12, 0x34).unwrap();
    let mut p = vec![SEI];
    for (a, v) in PORT1 {
        p.extend_from_slice(&sta(a, v));
    }
    p.extend(settle_loop(1));
    // chip 0 → $FB/$FC, chip 1 → $FD/$FE
    p.extend_from_slice(&[0xad, 0x19, 0xd4, 0x85, 0xfb, 0xad, 0x1a, 0xd4, 0x85, 0xfc]);
    p.extend_from_slice(&[0xad, 0x39, 0xd4, 0x85, 0xfd, 0xad, 0x3a, 0xd4, 0x85, 0xfe]);
    let here = 0xc000 + p.len() as u16;
    p.extend_from_slice(&[0x4c, here as u8, (here >> 8) as u8]);
    run_program(&mut m, &p, 3_000);
    assert_eq!((m.read_full(0xfb), m.read_full(0xfc)), (0x12, 0x34), "chip 0 has the port's lines");
    assert_eq!((m.read_full(0xfd), m.read_full(0xfe)), (0xff, 0xff), "chip 1 has none");
    assert_eq!((m.read_full(0xd439), m.read_full(0xd43a)), (0xff, 0xff), "and its peek says so");
}

// ── §12.2 — UE2's hardware test, replayed ─────────────────────────────────────────────────

fn booted(name: &str) -> Machine {
    let mut m = Machine::new_with_model(model::resolve(name).expect("model"));
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    m.run_for_full(3_000_000, &mut NullSink, |_, _, _, _, _, _, _| {});
    m
}

/// `LDA $D419 / STA $FB / LDA $D41A / STA $FC / JMP *` at `$C000`, entered with I set so the
/// KERNAL's IRQ cannot move `$DC00` between the hold and the read.
fn lda_after_hold(m: &mut Machine) -> (u8, u8) {
    m.set_hold(None);
    m.poke(0xc000, &[0xad, 0x19, 0xd4, 0x85, 0xfb, 0xad, 0x1a, 0xd4, 0x85, 0xfc, 0x4c, 0x0a, 0xc0]);
    let (pc, p) = (m.c64_core.reg_pc, m.c64_core.status());
    m.c64_core.reg_pc = 0xc000;
    m.c64_core.set_status_composite(p | 0x04);
    m.run_for_full_capped(64, 4, &mut NullSink, |_, _, _, _, _, _, _| {});
    let got = (m.read_full(0x00fb), m.read_full(0x00fc));
    m.c64_core.reg_pc = pc;
    m.c64_core.set_status_composite(p);
    got
}

fn ue2_replay(name: &str, hundred_ms: u64) {
    let mut m = booted(name);
    m.set_pot(1, 0x12, 0x34).unwrap();
    m.set_pot(2, 0x56, 0x78).unwrap();
    for (pa, want) in [(0x40u8, (0x12u8, 0x34u8)), (0x80, (0x56, 0x78))] {
        m.set_hold(Some(Hold::Cpu));
        m.poke_io(0xdc02, &[0xc0]);
        m.poke_io(0xdc00, &[pa]);
        m.run_for_full(hundred_ms, &mut NullSink, |_, _, _, _, _, _, _| {});
        assert_eq!((m.read_full(0xd419), m.read_full(0xd41a)), want, "{name}: peek after 100 ms held, $DC00=${pa:02X}");
        assert_eq!(lda_after_hold(&mut m), want, "{name}: CPU LDA after the hold, $DC00=${pa:02X}");
    }

    // Recorded beside it, not judged: the same writes with the CPU running.
    let mut m = booted(name);
    m.set_pot(1, 0x12, 0x34).unwrap();
    m.set_pot(2, 0x56, 0x78).unwrap();
    for pa in [0x40u8, 0x80] {
        m.poke_io(0xdc02, &[0xc0]);
        m.poke_io(0xdc00, &[pa]);
        m.run_for_full(hundred_ms, &mut NullSink, |_, _, _, _, _, _, _| {});
        eprintln!(
            "{name} unheld, wrote $DC00=${pa:02X}: after 100 ms $DC00=${:02X} $DC02=${:02X}, $D419/$D41A = ${:02X}/${:02X}",
            m.cia1.peek(0xdc00),
            m.cia1.peek(0xdc02),
            m.read_full(0xd419),
            m.read_full(0xd41a)
        );
    }
}

#[test]
fn g02_ue2_input_test_replayed_under_a_held_cpu_pal() {
    if roms() {
        ue2_replay("c64-pal", 98_525);
    }
}

#[test]
fn g02_ue2_input_test_replayed_under_a_held_cpu_ntsc() {
    if roms() {
        ue2_replay("c64-ntsc", 102_273);
    }
}

// ── §12.3 / §12.9 — the latch at its cycle ────────────────────────────────────────────────

const P1: (u8, u8) = (0x11, 0x22);
const P2: (u8, u8) = (0x99, 0xaa);

/// Check every read against the rule: port 2 before the first boundary after `w`, port 1
/// at or after it. Returns how many reads fell between `w` and that boundary.
fn judge_switch(rec: &Rec, w: u64, label: &str) -> usize {
    let mut before = 0;
    for &(r, addr, v) in rec.reads.iter().filter(|x| x.0 > w && x.1 == 0xd419) {
        let want = if r >> 9 > w >> 9 { P1.0 } else { P2.0 };
        assert_eq!(v, want, "{label}: write at {w} (phase {}), read ${addr:04X} at {r} (phase {})", w & 511, r & 511);
        if r >> 9 == w >> 9 {
            before += 1;
        }
    }
    before
}

/// Port 2 selected long enough to stand at W's boundary, then `STA $DC00` (port 1) at W,
/// then 256 reads 9 cycles apart. `pad` moves W by exactly `pad` cycles.
fn switch_program(pad: u64, outer: u8, loops: u8) -> Vec<u8> {
    let mut p = vec![SEI];
    p.extend_from_slice(&sta(0xdc02, 0xc0));
    p.extend_from_slice(&sta(0xdc00, 0x80));
    p.extend(settle_loop(outer));
    p.extend(delay(pad));
    p.extend_from_slice(&sta(0xdc00, 0x40));
    // LDY #loops / L1: LDX #0 / L2: LDA $D419 / INX / BNE L2 / DEY / BNE L1 / JMP *
    p.extend_from_slice(&[0xa0, loops, 0xa2, 0x00, 0xad, 0x19, 0xd4, 0xe8, 0xd0, 0xfa, 0x88, 0xd0, 0xf5]);
    let here = 0xc000 + p.len() as u16;
    p.extend_from_slice(&[0x4c, here as u8, (here >> 8) as u8]);
    p
}

fn the_write(rec: &Rec) -> u64 {
    rec.writes.iter().find(|w| w.1 == 0xdc00 && w.2 == 0x40).expect("the switch was written").0
}

#[test]
fn g03_the_selection_is_latched_at_the_first_boundary_after_the_write() {
    let mut phases = BTreeSet::new();
    let mut on_boundary = 0;
    for pad in 4..4 + 512 {
        let mut m = Machine::new();
        m.set_pot(1, P1.0, P1.1).unwrap();
        m.set_pot(2, P2.0, P2.1).unwrap();
        let code = switch_program(pad, 1, 1);
        m.poke(0xc000, &code);
        m.c64_core.reg_pc = 0xc000;
        let mut rec = Rec::default();
        m.run_for_full(6_000, &mut rec, |_, _, _, _, _, _, _| {});
        let w = the_write(&rec);
        phases.insert(w & 511);
        let before = judge_switch(&rec, w, "1 MHz");
        if w & 511 == 0 {
            on_boundary += 1;
            assert!(before > 0, "a write on the boundary cycle: that boundary kept the old value");
        }
        assert_eq!(rec.reads.len() as u64, m.pot_lines().reads, "every read counted once");
    }
    assert_eq!(phases.len(), 512, "the write fell on every cycle of the window");
    assert_eq!(on_boundary, 1);
}

#[test]
fn g03_set_pot_between_runs_is_latched_at_the_first_boundary_after_it() {
    // SEI / select port 1 / L: LDA $D419 / NOP / JMP L — boundaries 4, 2, 3 cycles apart.
    let mut hit_boundary = 0;
    let mut phases = BTreeSet::new();
    for lead in [0u64, 2, 3] {
        for budget in 2_000..2_000 + 600 {
            let mut m = Machine::new();
            m.set_pot(1, 0x11, 0x22).unwrap();
            let mut p = vec![SEI];
            p.extend_from_slice(&sta(0xdc02, 0xc0));
            p.extend_from_slice(&sta(0xdc00, 0x40));
            if lead > 0 {
                p.extend(delay(lead));
            }
            let l = 0xc000 + p.len() as u16;
            p.extend_from_slice(&[0xad, 0x19, 0xd4, 0xea, 0x4c, l as u8, (l >> 8) as u8]);
            m.poke(0xc000, &p);
            m.c64_core.reg_pc = 0xc000;
            m.run_for_full(budget, &mut NullSink, |_, _, _, _, _, _, _| {});
            let t = m.c64_core.clk;
            phases.insert(t & 511);
            m.set_pot(1, 0x33, 0x44).unwrap();
            let mut rec = Rec::default();
            m.run_for_full(1_200, &mut rec, |_, _, _, _, _, _, _| {});
            for &(r, _, v) in &rec.reads {
                let want = if r >> 9 > t >> 9 { 0x33 } else { 0x11 };
                assert_eq!(v, want, "set at {t} (phase {}), read at {r} (phase {})", t & 511, r & 511);
            }
            if t & 511 == 0 {
                hit_boundary += 1;
                assert!(rec.reads.iter().any(|&(r, _, v)| r >> 9 == t >> 9 && v == 0x11), "a set on the boundary cycle leaves it");
            }
        }
    }
    eprintln!("set_pot sweep: {} distinct phases, {hit_boundary} on the boundary cycle", phases.len());
    assert!(hit_boundary > 0, "the sweep met a set on a boundary cycle");
}

/// §12.9 — the same sweep at 48 MHz on the `u64` profile: the phase is moved by an idle run
/// of `s` PHI2 cycles first, and the rule is the 1 MHz one, in PHI2 cycles.
fn turbo_sweep(fast_path: bool) -> Vec<Vec<(u64, u16, u8)>> {
    let mut logs = Vec::new();
    let mut phases = BTreeSet::new();
    for s in 0..512u64 {
        let mut m = Machine::new();
        m.set_machine_profile(SpeedProfile::U64);
        m.set_u64_turbo(0x00, 0x8e);
        m.turbo_fast_path = fast_path;
        m.set_pot(1, P1.0, P1.1).unwrap();
        m.set_pot(2, P2.0, P2.1).unwrap();
        m.poke(0xc800, &[0x4c, 0x00, 0xc8]);
        m.c64_core.reg_pc = 0xc800;
        if s > 0 {
            m.run_for_full(s, &mut NullSink, |_, _, _, _, _, _, _| {});
        }
        // 20 × 1 280 CPU cycles ≈ 533 PHI2 of port 2, then 12 × 256 reads ≈ 576 PHI2.
        m.poke(0xc000, &switch_program(2, 20, 12));
        m.c64_core.reg_pc = 0xc000;
        let reads0 = m.pot_lines().reads;
        let mut rec = Rec::default();
        m.run_for_full(2_000, &mut rec, |_, _, _, _, _, _, _| {});
        assert_eq!(m.turbo_divider(), 48);
        let w = the_write(&rec);
        phases.insert(w & 511);
        assert!(rec.reads.iter().any(|r| r.0 >> 9 > w >> 9), "the reads ran past the next boundary");
        judge_switch(&rec, w, "48 MHz");
        assert_eq!(m.pot_lines().reads - reads0, rec.reads.len() as u64, "every read counted once (fast path {fast_path})");
        logs.push(rec.reads);
    }
    eprintln!("48 MHz sweep (fast path {fast_path}): the write fell on {} distinct phases", phases.len());
    assert!(phases.len() >= 500, "the idle run moved the write across the window");
    logs
}

#[test]
fn g09_turbo_puts_the_boundaries_on_the_same_phi2_cycles() {
    let fast = turbo_sweep(true);
    let slow = turbo_sweep(false);
    assert_eq!(fast, slow, "the fast path read the same values on the same PHI2 cycles");
}

// ── §12.4 / §12.5 / §12.6 — combination, selection, fire buttons ──────────────────────────

#[test]
fn g04_both_selected_is_the_parallel_combination() {
    let both = |a: Option<u8>, b: Option<u8>| {
        let mut m = Machine::new();
        if let Some(a) = a {
            m.set_pot(1, a, a).unwrap();
        }
        if let Some(b) = b {
            m.set_pot(2, b, b).unwrap();
        }
        cpu_pot(&mut m, &BOTH).0
    };
    assert_eq!(both(Some(100), Some(100)), 50);
    assert_eq!(both(Some(200), Some(56)), 43);
    for n in [1u8, 56, 200, 254] {
        assert_eq!(both(Some(0), Some(n)), 0);
        assert_eq!(both(Some(n), Some(0)), 0);
        assert_eq!(both(Some(0xff), Some(n)), n);
        assert_eq!(both(Some(n), Some(0xff)), n);
        assert_eq!(both(None, Some(n)), n, "port 1 cleared → port 2");
        assert_eq!(both(Some(n), None), n, "port 2 cleared → port 1");
    }
    assert_eq!(both(Some(0xff), Some(0xff)), 0xff);
    assert_eq!(both(None, None), 0xff);

    let mut m = Machine::new();
    m.set_pot(1, 10, 20).unwrap();
    m.set_pot(2, 30, 40).unwrap();
    assert_eq!(cpu_pot(&mut m, &NEITHER), (0xff, 0xff), "neither selected, whatever is set");
}

#[test]
fn g05_an_input_pin_selects() {
    let mut m = Machine::new();
    m.set_pot(1, 100, 30).unwrap();
    m.set_pot(2, 100, 60).unwrap();
    assert_eq!(cpu_pot(&mut m, &[(0xdc02, 0x00), (0xdc00, 0x00)]), (50, 20), "DDRA=$00: both");
    assert_eq!(cpu_pot(&mut m, &[(0xdc02, 0xc0), (0xdc00, 0x00)]), (0xff, 0xff), "DDRA=$C0, PRA=$00: neither");
    assert_eq!(cpu_pot(&mut m, &[(0xdc02, 0x40), (0xdc00, 0x00)]), (100, 60), "PA6 low output, PA7 input: port 2");
}

#[test]
fn g06_fire_two_pressed_and_fire_three_released() {
    let mut m = Machine::new();
    m.set_pot(2, 0x00, 0xff).unwrap();
    let (x, y) = cpu_pot(&mut m, &PORT2);
    assert_eq!(x & 0x80, 0, "POTX bit 7 clear: fire 2 pressed");
    assert_eq!(y & 0x80, 0x80, "POTY bit 7 set: fire 3 released");
}

// ── §12.7 — Last Ninja's gate ─────────────────────────────────────────────────────────────

/// `$0917`'s sequence: `LDA $D419 / CMP #$00 / BMI intro / JMP game`. Intro writes 1 to
/// `$FB`, the game 2.
fn last_ninja(m: &mut Machine, select: &[(u16, u8)]) -> u8 {
    let mut p = vec![SEI];
    for &(a, v) in select {
        p.extend_from_slice(&sta(a, v));
    }
    p.extend(settle_loop(1));
    let base = 0xc000 + p.len() as u16;
    let intro = base + 10;
    let game = intro + 7;
    p.extend_from_slice(&[0xad, 0x19, 0xd4, 0xc9, 0x00, 0x30, 0x03, 0x4c, game as u8, (game >> 8) as u8]);
    p.extend_from_slice(&[0xa9, 0x01, 0x85, 0xfb, 0x4c, (intro + 4) as u8, ((intro + 4) >> 8) as u8]);
    p.extend_from_slice(&[0xa9, 0x02, 0x85, 0xfb, 0x4c, (game + 4) as u8, ((game + 4) >> 8) as u8]);
    run_program(m, &p, 3_000);
    m.read_full(0x00fb)
}

#[test]
fn g07_last_ninja_takes_the_intro_with_nothing_set() {
    assert_eq!(last_ninja(&mut Machine::new(), &[]), 1, "nothing set (power-on DDRA): intro");
    assert_eq!(last_ninja(&mut Machine::new(), &PORT1), 1, "nothing set, port 1: intro");
    let mut m = Machine::new();
    m.set_pot(1, 0x00, 0x00).unwrap();
    assert_eq!(last_ninja(&mut m, &PORT1), 2, "port 1 pulled to zero: the game — the old defect, on purpose");
}

// ── §12.8 — the KERNAL moves the mux (characterisation) ───────────────────────────────────

#[test]
fn g08_the_kernal_scan_moves_the_mux() {
    if !roms() {
        return;
    }
    let mut m = booted("c64-pal");
    m.set_pot(1, 10, 10).unwrap();
    m.set_pot(2, 200, 200).unwrap();
    // CLI / L: LDA $D419 / JMP L, the KERNAL's IRQ on.
    m.poke(0xc000, &[0x58, 0xad, 0x19, 0xd4, 0x4c, 0x01, 0xc0]);
    m.c64_core.reg_pc = 0xc000;
    let mut rec = Rec::default();
    m.run_for_full(100 * FRAME, &mut rec, |_, _, _, _, _, _, _| {});
    let mut dist: BTreeMap<u8, u64> = BTreeMap::new();
    for &(_, _, v) in &rec.reads {
        *dist.entry(v).or_default() += 1;
    }
    eprintln!("KERNAL idle, 100 frames, {} reads of $D419: {dist:?}", rec.reads.len());
    for v in dist.keys() {
        assert!([10u8, 200, 9, 0xff].contains(v), "${v:02X} is none of port 1, port 2, parallel, neither");
    }
    assert!(dist.contains_key(&10), "port 1 between scans");
}

// ── §12.10 — holds ────────────────────────────────────────────────────────────────────────

#[test]
fn g10_a_boundary_inside_a_hold_is_seen_by_the_first_read_after_it() {
    for hold in [Hold::Cpu, Hold::Reset] {
        let mut m = Machine::new();
        m.set_pot(1, 0x11, 0x22).unwrap();
        let mut p = vec![SEI];
        p.extend_from_slice(&sta(0xdc02, 0xc0));
        p.extend_from_slice(&sta(0xdc00, 0x40));
        let l = 0xc000 + p.len() as u16;
        p.extend_from_slice(&[0xad, 0x19, 0xd4, 0x4c, l as u8, (l >> 8) as u8]);
        run_program(&mut m, &p, 3_000);
        m.set_hold(Some(hold));
        let t = m.c64_core.clk;
        m.set_pot(1, 0x33, 0x44).unwrap();
        m.run_for_full(1_500, &mut NullSink, |_, _, _, _, _, _, _| {});
        let end = m.c64_core.clk;
        assert!(end >> 9 > t >> 9, "{hold:?}: a boundary fell inside the hold");
        m.set_hold(None);
        let mut rec = Rec::default();
        m.run_for_full(20, &mut rec, |_, _, _, _, _, _, _| {});
        let first = rec.reads.first().expect("a read after the hold");
        assert_eq!(first.2, 0x33, "{hold:?}: the first read after the hold (at {}, hold {t}..{end})", first.0);
    }
}

// ── §12.11 — checkpoints ──────────────────────────────────────────────────────────────────

fn checkpoint(m: &Machine) -> serde_json::Value {
    capture_runtime_checkpoint_opts(m, "", "", None, None, None, None, true)
}

/// Port 1 selected, `$D419`/`$D41A` read alternately forever.
fn reader(m: &mut Machine) {
    let mut p = vec![SEI];
    p.extend_from_slice(&sta(0xdc02, 0xc0));
    p.extend_from_slice(&sta(0xdc00, 0x40));
    let l = 0xc000 + p.len() as u16;
    p.extend_from_slice(&[0xad, 0x19, 0xd4, 0xad, 0x1a, 0xd4, 0x4c, l as u8, (l >> 8) as u8]);
    m.poke(0xc000, &p);
    m.c64_core.reg_pc = 0xc000;
}

/// The host's input for frame `f`, applied 7 777 cycles into it — mid-window.
fn frame_with_input(m: &mut Machine, f: u64, rec: &mut Rec) {
    m.run_for_full(7_777, rec, |_, _, _, _, _, _, _| {});
    match f % 5 {
        0 => m.clear_pot(1).unwrap(),
        _ => m.set_pot(1, (f * 3) as u8, (f * 7) as u8).unwrap(),
    }
    m.run_for_full(FRAME - 7_777, rec, |_, _, _, _, _, _, _| {});
}

#[test]
fn g11_nothing_set_writes_no_pot_node() {
    let mut m = Machine::new();
    reader(&mut m);
    m.run_for_full(FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert!(checkpoint(&m).get("pot").is_none(), "the default writes nothing");
    // A value set and cleared again, once the latch has sampled the clear, is the default too.
    m.set_pot(1, 1, 2).unwrap();
    m.run_for_full(FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
    m.clear_pot(1).unwrap();
    m.run_for_full(FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert!(checkpoint(&m).get("pot").is_none());
}

#[test]
fn g11_a_restore_mid_window_replays_in_lockstep() {
    let mut a = Machine::new();
    reader(&mut a);
    let mut sink = Rec::default();
    for f in 0..10 {
        frame_with_input(&mut a, f, &mut sink);
    }
    // Mid-window: set, and captured before the boundary that would latch it.
    a.run_for_full(7_777, &mut NullSink, |_, _, _, _, _, _, _| {});
    a.set_pot(1, 0x5a, 0xa5).unwrap();
    a.run_for_full(3, &mut NullSink, |_, _, _, _, _, _, _| {});
    let cp = checkpoint(&a);
    let node = cp.get("pot").expect("a set value writes the node");
    assert_eq!(node["set"][0], serde_json::json!([0x5a, 0xa5]));
    assert_ne!(a.pot_lines().latch(), [0x5a, 0xa5], "captured before the latch took it");

    let mut b = Machine::new();
    restore_runtime_checkpoint(&mut b, &cp).expect("restore");
    assert_eq!(b.pot(1), Some((0x5a, 0xa5)));
    assert_eq!(b.pot_lines().latch(), a.pot_lines().latch());
    assert_eq!(b.pot_lines().sampled(), a.pot_lines().sampled());

    let (mut ra, mut rb) = (Rec::default(), Rec::default());
    for f in 10..510 {
        frame_with_input(&mut a, f, &mut ra);
        frame_with_input(&mut b, f, &mut rb);
    }
    assert!(ra.reads.len() > 100_000, "the reader read");
    assert_eq!(ra.reads.len(), rb.reads.len());
    if let Some(i) = (0..ra.reads.len()).find(|&i| ra.reads[i] != rb.reads[i]) {
        panic!("first divergence at read {i}: straight {:?}, restored {:?}", ra.reads[i], rb.reads[i]);
    }
    let values: BTreeSet<u8> = ra.reads.iter().map(|r| r.2).collect();
    assert!(values.len() > 50, "the reads saw the host's input change");
    assert_eq!(checkpoint(&a), checkpoint(&b), "and the machines end identical");
}

#[test]
fn g11_a_checkpoint_without_the_node_restores_to_nothing_set() {
    let mut m = Machine::new();
    reader(&mut m);
    m.run_for_full(FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
    let mut cp = checkpoint(&m);
    m.set_pot(1, 1, 2).unwrap();
    m.set_pot(2, 3, 4).unwrap();
    m.run_for_full(FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert_eq!(m.read_full(0xd419), 1);
    cp.as_object_mut().unwrap().remove("pot");
    restore_runtime_checkpoint(&mut m, &cp).expect("restore");
    assert_eq!((m.pot(1), m.pot(2)), (None, None));
    assert_eq!(m.pot_lines().latch(), [0xff, 0xff]);
    assert_eq!(m.pot_lines().sampled(), m.c64_core.clk >> 9);
    let mut rec = Rec::default();
    m.run_for_full(FRAME, &mut rec, |_, _, _, _, _, _, _| {});
    assert!(!rec.reads.is_empty() && rec.reads.iter().all(|r| r.2 == 0xff));
}

/// Written by main 1b84320 (the `$80` constant), from a detached worktree: `Machine::new()`,
/// `SEI / L: LDA $D419 / STA $0400 / LDA $D41A / STA $0401 / JMP L` at `$C000` run for
/// 20 000 cycles, `capture_runtime_checkpoint_opts(.., omit_framebuffer)`,
/// `write_native_snapshot`.
#[test]
fn g11_a_c64re_written_by_main_restores() {
    let bytes = std::fs::read(format!("{FIXTURES}/main_1b84320.c64re")).expect("fixture");
    let snap = read_native_snapshot(&bytes).expect("read .c64re");
    assert!(snap.checkpoint.get("pot").is_none(), "main wrote no pot node");
    let mut m = Machine::new();
    m.set_pot(1, 9, 9).unwrap();
    restore_runtime_checkpoint(&mut m, &snap.checkpoint).expect("restore main's .c64re");
    assert_eq!((m.read_full(0x0400), m.read_full(0x0401)), (0x80, 0x80), "main's run read the old constant");
    assert_eq!((m.pot(1), m.pot(2)), (None, None));
    m.run_for_full(FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert_eq!((m.read_full(0x0400), m.read_full(0x0401)), (0xff, 0xff), "its continuation reads open lines (§10)");
}

// ── §12.12 — clone and reset ──────────────────────────────────────────────────────────────

#[test]
fn g12_a_clone_reads_what_the_original_reads() {
    let mut a = Machine::new();
    reader(&mut a);
    a.set_pot(1, 0x21, 0x43).unwrap();
    a.run_for_full(FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
    a.set_pot(1, 0x65, 0x87).unwrap();
    a.run_for_full(100, &mut NullSink, |_, _, _, _, _, _, _| {});
    let mut b = a.clone();
    assert_eq!(b.pot(1), a.pot(1));
    let (mut ra, mut rb) = (Rec::default(), Rec::default());
    a.run_for_full(FRAME, &mut ra, |_, _, _, _, _, _, _| {});
    b.run_for_full(FRAME, &mut rb, |_, _, _, _, _, _, _| {});
    assert_eq!(ra.reads, rb.reads);
    assert!(ra.reads.iter().any(|r| r.2 == 0x21) && ra.reads.iter().any(|r| r.2 == 0x65));
}

#[test]
fn g12_a_warm_reset_keeps_the_set_values_and_selects_both() {
    let mut m = Machine::new();
    reader(&mut m);
    m.set_pot(1, 100, 30).unwrap();
    m.set_pot(2, 100, 60).unwrap();
    m.run_for_full(FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert_eq!(m.read_full(0xd419), 100, "port 1 before the reset");
    m.warm_reset();
    assert_eq!((m.pot(1), m.pot(2)), (Some((100, 30)), Some((100, 60))), "a reset unplugs nothing");
    assert_eq!(m.cia1.pa_output() >> 6, 3, "DDRA=$00: both selected until IOINIT");
    m.set_hold(Some(Hold::Reset));
    m.run_for_full(600, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert_eq!((m.read_full(0xd419), m.read_full(0xd41a)), (50, 20), "the next sample is the parallel reading");
}
