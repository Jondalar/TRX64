//! Spec 857 D1 / Spec 888 — the CIA alarm path, frozen.
//!
//! Spec 857 compared a per-cycle catch-up of both timers against VICE's alarm check and
//! required them equal. Since Spec 888 the C64's CIAs are VICE's ciacore, alarm-driven by
//! construction — there is no second path left to compare. What stays:
//!
//!   - **Lockstep determinism.** Same workload, two machines, stopped every 7919 cycles (a
//!     prime, so the stops land between register accesses and not on them) and compared on
//!     the instruction stream, every bus record, every interrupt, the full runtime checkpoint
//!     and a `peek` of all 32 CIA registers.
//!   - **A checkpoint restored mid-count continues identically** to the machine that ran
//!     straight through — the checkpoint carries the whole ciacore context.
//!   - **Frozen digests of the behaviour.** Re-recorded for Spec 888 (the chip changed: the
//!     IFR delay line, the old 6526's late interrupt, VICE's TOD), and they must not move.
//!
//! The workloads are Spec 857 §2's: Timer A continuous and one-shot, Timer B counting Timer A
//! underflows at latches 0, 1 and 2, CIA2 on NMI, the TOD alarm, a checkpoint restored
//! mid-count, and a booted machine — each at 1 and at 64 MHz, because at 64 MHz the prologue
//! runs about eighteen times per PHI2 cycle.
//!
//! `CIA857_PRINT_GOLDEN=1` prints the digests instead of asserting them.

use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, restore_runtime_checkpoint};
use trx64_core::vic::SpeedProfile;
use trx64_core::{BusKind, Machine, NullSink, Observer};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const FRAME: u64 = 19_656;
const STEP: u64 = 7_919;
const MHZ_1: u8 = 0x80;
const MHZ_64: u8 = 0x8f;

// ── a few bytes of 6502, spelled out ─────────────────────────────────────────────────────

struct Asm(Vec<u8>);
impl Asm {
    fn new() -> Self {
        // SEI / IRQ vector -> $C100
        Asm(vec![0x78, 0xa9, 0x00, 0x8d, 0xfe, 0xff, 0xa9, 0xc1, 0x8d, 0xff, 0xff])
    }
    /// LDA #v / STA addr
    fn poke(mut self, addr: u16, v: u8) -> Self {
        self.0.extend_from_slice(&[0xa9, v, 0x8d, addr as u8, (addr >> 8) as u8]);
        self
    }
    /// LDA addr
    fn read(mut self, addr: u16) -> Self {
        self.0.extend_from_slice(&[0xad, addr as u8, (addr >> 8) as u8]);
        self
    }
    /// CLI, then `loop: INC $FB / [LDA probe] / JMP loop`.
    fn cli_loop(mut self, probe: Option<u16>) -> Vec<u8> {
        self.0.push(0x58);
        let l = 0xc000 + self.0.len() as u16;
        self.0.extend_from_slice(&[0xe6, 0xfb]);
        if let Some(a) = probe {
            self.0.extend_from_slice(&[0xad, a as u8, (a >> 8) as u8]);
        }
        self.0.extend_from_slice(&[0x4c, l as u8, (l >> 8) as u8]);
        self.0
    }
}

/// INC $0400 / BNE +3 / INC $0401 / LDA $DC0D / RTI
const ACK_CIA1: [u8; 12] = [0xee, 0x00, 0x04, 0xd0, 0x03, 0xee, 0x01, 0x04, 0xad, 0x0d, 0xdc, 0x40];
/// LDA $DC0D / LDA #$19 / STA $DC0E / INC $0400 / BNE +3 / INC $0401 / RTI — re-arms a one-shot
const REARM_ONESHOT: [u8; 17] =
    [0xad, 0x0d, 0xdc, 0xa9, 0x19, 0x8d, 0x0e, 0xdc, 0xee, 0x00, 0x04, 0xd0, 0x03, 0xee, 0x01, 0x04, 0x40];
/// NMI at $C200: LDA $DD0D / INC $0402 / BNE +3 / INC $0403 / RTI
const ACK_CIA2: [u8; 12] = [0xad, 0x0d, 0xdd, 0xee, 0x02, 0x04, 0xd0, 0x03, 0xee, 0x03, 0x04, 0x40];

fn quiet_cias(a: Asm) -> Asm {
    a.poke(0xdc0d, 0x7f).read(0xdc0d).poke(0xdd0d, 0x7f).read(0xdd0d)
}

type Setup = Box<dyn Fn(&mut Machine)>;

fn program(main: Vec<u8>) -> Setup {
    Box::new(move |m: &mut Machine| {
        m.poke(0xc000, &main);
        m.poke(0xc100, &ACK_CIA1);
        m.poke(0xc200, &ACK_CIA2);
        m.write_full(0x0001, 0x35);
        m.c64_core.reg_pc = 0xc000;
    })
}

fn workloads() -> Vec<(&'static str, Setup, u64)> {
    let ta = quiet_cias(Asm::new()).poke(0xdc04, 0xff).poke(0xdc05, 0x0f).poke(0xdc0d, 0x81).poke(0xdc0e, 0x11);
    let ta_probe = quiet_cias(Asm::new()).poke(0xdc04, 0xff).poke(0xdc05, 0x0f).poke(0xdc0d, 0x81).poke(0xdc0e, 0x11);
    let oneshot = quiet_cias(Asm::new()).poke(0xdc04, 0x00).poke(0xdc05, 0x08).poke(0xdc0d, 0x81).poke(0xdc0e, 0x19);
    let cascade = |l: u8| {
        quiet_cias(Asm::new())
            .poke(0xdc04, l)
            .poke(0xdc05, 0x00)
            .poke(0xdc06, 0x03)
            .poke(0xdc07, 0x00)
            .poke(0xdc0d, 0x82)
            .poke(0xdc0f, 0x51) // TB: start, force load, count Timer A underflows
            .poke(0xdc0e, 0x11)
            .cli_loop(None)
    };
    let nmi = quiet_cias(Asm::new())
        .poke(0xfffa, 0x00)
        .poke(0xfffb, 0xc2)
        .poke(0xdd04, 0x77)
        .poke(0xdd05, 0x07)
        .poke(0xdd0d, 0x81)
        .poke(0xdd0e, 0x11);
    // TOD: 50 Hz, alarm at 00:00:01.0, time 00:00:00.0 (hours stop the clock, tenths start it)
    let tod = quiet_cias(Asm::new())
        .poke(0xdc0e, 0x80)
        .poke(0xdc0f, 0x80)
        .poke(0xdc0b, 0x00)
        .poke(0xdc0a, 0x00)
        .poke(0xdc09, 0x01)
        .poke(0xdc08, 0x00)
        .poke(0xdc0f, 0x00)
        .poke(0xdc0b, 0x00)
        .poke(0xdc0a, 0x00)
        .poke(0xdc09, 0x00)
        .poke(0xdc08, 0x00)
        .poke(0xdc0d, 0x84);

    let oneshot_setup: Setup = {
        let main = oneshot.cli_loop(None);
        Box::new(move |m: &mut Machine| {
            program(main.clone())(m);
            m.poke(0xc100, &REARM_ONESHOT);
        })
    };
    vec![
        ("ta_irq", program(ta.cli_loop(None)), 60),
        ("ta_irq_timer_read", program(ta_probe.cli_loop(Some(0xdc04))), 60),
        ("ta_oneshot", oneshot_setup, 60),
        ("tb_cascade_l0", program(cascade(0)), 60),
        ("tb_cascade_l1", program(cascade(1)), 60),
        ("tb_cascade_l2", program(cascade(2)), 60),
        ("cia2_nmi", program(nmi.cli_loop(None)), 60),
        ("tod_alarm", program(tod.cli_loop(None)), 180),
    ]
}

// ── observation ──────────────────────────────────────────────────────────────────────────

fn fnv(h: &mut u64, v: u64) {
    *h ^= v;
    *h = h.wrapping_mul(0x0000_0100_0000_01b3);
}
fn fnv_bytes(h: &mut u64, b: &[u8]) {
    for &x in b {
        fnv(h, u64::from(x));
    }
}

struct Stream(u64, u64);
impl Observer for Stream {
    #[allow(clippy::too_many_arguments)]
    fn on_instruction(&mut self, pc: u16, op: u8, _: u8, _: u8, a: u8, x: u8, y: u8, _: u8, p: u8, clk: u64) {
        fnv(&mut self.0, u64::from(pc) | (u64::from(op) << 16));
        fnv(&mut self.0, u64::from(a) | (u64::from(x) << 8) | (u64::from(y) << 16) | (u64::from(p) << 24));
        fnv(&mut self.0, clk);
        self.1 += 1;
    }
    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, pc: u16, clk: u64, old: u8) {
        fnv(&mut self.0, kind as u64 | (u64::from(addr) << 8) | (u64::from(value) << 24) | (u64::from(old) << 32));
        fnv(&mut self.0, u64::from(pc));
        fnv(&mut self.0, clk);
    }
    fn on_interrupt(&mut self, vector: u16, clk: u64) {
        fnv(&mut self.0, 0xffff_0000 | u64::from(vector));
        fnv(&mut self.0, clk);
    }
}

fn peeks(m: &Machine) -> Vec<u8> {
    (0..16u16).map(|r| m.cia1.peek(0xdc00 + r)).chain((0..16u16).map(|r| m.cia2.peek(0xdd00 + r))).collect()
}

fn checkpoint(m: &Machine) -> serde_json::Value {
    capture_runtime_checkpoint(m, "", "d64", None, None, None, None)
}

fn machine(prefer: u8) -> Machine {
    let mut m = Machine::new();
    m.set_machine_profile(SpeedProfile::U64);
    m.set_u64_turbo(0x00, prefer);
    m
}

fn first_diff(a: &serde_json::Value, b: &serde_json::Value, path: &str) -> Option<String> {
    use serde_json::Value;
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            keys.into_iter().find_map(|k| match (x.get(k), y.get(k)) {
                (Some(p), Some(q)) => first_diff(p, q, &format!("{path}.{k}")),
                (p, q) => Some(format!("{path}.{k}: {p:?} vs {q:?}")),
            })
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            x.iter().zip(y).enumerate().find_map(|(i, (p, q))| first_diff(p, q, &format!("{path}[{i}]")))
        }
        _ => (a != b).then(|| format!("{path}: {a} vs {b}")),
    }
}

/// Lockstep both machines and return the digest of the first.
fn lockstep(label: &str, off: &mut Machine, on: &mut Machine, steps: u64) -> u64 {
    let mut digest = 0xcbf2_9ce4_8422_2325u64;
    for step in 0..steps {
        let (mut so, mut sn) = (Stream(0xcbf2_9ce4_8422_2325, 0), Stream(0xcbf2_9ce4_8422_2325, 0));
        off.run_for_full(STEP, &mut so, |_, _, _, _, _, _, _| {});
        on.run_for_full(STEP, &mut sn, |_, _, _, _, _, _, _| {});
        assert_eq!((so.0, so.1), (sn.0, sn.1), "{label}: the observable stream diverged in step {step}");
        let (po, pn) = (peeks(off), peeks(on));
        assert_eq!(po, pn, "{label}: a CIA peek diverged after step {step}");
        let (co, cn) = (checkpoint(off), checkpoint(on));
        if co != cn {
            panic!("{label}: the checkpoint diverged after step {step}: {}", first_diff(&co, &cn, "").unwrap_or_default());
        }
        fnv(&mut digest, so.0);
        fnv(&mut digest, so.1);
        fnv_bytes(&mut digest, &po);
        // The `turbo` node (turbo as measured, row 6/D7) is left out of the digest: it is
        // pinned by `turbo_as_measured_gate`, and keeping it out is what lets the `@1`
        // digests below stay the ones recorded before it existed — the proof that 1 MHz
        // did not move. It is still compared between the two machines above.
        let mut co_digest = co.clone();
        if let Some(o) = co_digest.as_object_mut() {
            o.remove("turbo");
        }
        fnv_bytes(&mut digest, serde_json::to_string(&co_digest).unwrap().as_bytes());
    }
    digest
}

// ── frozen digests ──────────────────────────────────────────────────────────────────────

/// Re-recorded for Spec 888 (2026-10-02), every one of them: the C64's CIAs became VICE's
/// ciacore. Each moved for the reasons the spec's D4 names — the old 6526 raises its line a
/// cycle after the underflow (`CIA_IRQ_RAISE1`), the ICR read acknowledges through the IFR
/// delay line, Timer A re-arms its alarm only while an interrupt is wanted and not pending
/// (`ciacore_intta`), and TOD runs VICE's mains alarm (its reset leaves the hour at 1). The
/// history of the previous digests (857, 7ce542a, 843, 868 §9, BUG-061, 870) is in git.
///
/// The ten `@64` digests re-recorded for turbo as measured (2026-10-03), the `@1` ten NOT:
/// row 2 — the menu's 64 MHz runs 63 CPU cycles per PHI2 cycle, not 64; row 4 — the
/// interrupt delay is counted in CPU cycles at the turbo clock, with the lines sampled at
/// every turbo cycle, so a turbo handler is entered earlier. Every `@1` digest is the one
/// recorded for Spec 888, byte for byte.
const GOLDEN: &[(&str, u64)] = &[
    ("ta_irq@1", 0xa246b6efb6558752),
    ("ta_irq@64", 0x83c67dc487307767),
    ("ta_irq_timer_read@1", 0x4b70672fac9eaa9d),
    ("ta_irq_timer_read@64", 0x2d6de302ff73bda5),
    ("ta_oneshot@1", 0x3b54bfe7e21b8689),
    ("ta_oneshot@64", 0x122735eae99918fe),
    ("tb_cascade_l0@1", 0x98a9131c0bae633a),
    ("tb_cascade_l0@64", 0x3f9e14292304a02e),
    ("tb_cascade_l1@1", 0xf7adcbf78ead153a),
    ("tb_cascade_l1@64", 0x7778f24c56018f5d),
    ("tb_cascade_l2@1", 0x5ed3218521a1dc17),
    ("tb_cascade_l2@64", 0xc75d3f932cd05862),
    ("cia2_nmi@1", 0x1a928b753aadd799),
    ("cia2_nmi@64", 0x1026fc1be0534638),
    ("tod_alarm@1", 0xde2b5ecaf5b8347c),
    ("tod_alarm@64", 0x222e61f20fc01cf3),
    ("restore_cascade@1", 0x3da6ee42bde8de72),
    ("restore_cascade@64", 0x32ce5b89ba147a53),
    ("booted@1", 0xfa6905d085cfff85),
    ("booted@64", 0x8e134e547d8e9f30),
];

fn golden(label: &str, digest: u64, printed: &mut Vec<String>) {
    if std::env::var_os("CIA857_PRINT_GOLDEN").is_some() {
        printed.push(format!("    (\"{label}\", 0x{digest:016x}),"));
        return;
    }
    let want = GOLDEN.iter().find(|(l, _)| *l == label).map(|(_, d)| *d);
    assert_eq!(want, Some(digest), "{label}: today's behaviour moved (or no golden recorded)");
}

#[test]
fn the_timer_workloads_are_deterministic_and_frozen() {
    let mut printed = Vec::new();
    for (name, setup, steps) in workloads() {
        for (speed, prefer) in [("1", MHZ_1), ("64", MHZ_64)] {
            let label = format!("{name}@{speed}");
            let (mut a, mut b) = (machine(prefer), machine(prefer));
            setup(&mut a);
            setup(&mut b);
            let d = lockstep(&label, &mut a, &mut b, steps);
            // Something must actually have happened, or equality proves nothing.
            let word = |m: &Machine, at: u16| u64::from(m.read_full(at)) | u64::from(m.read_full(at + 1)) << 8;
            let taken = word(&a, 0x0400) + word(&a, 0x0402);
            assert!(taken > 0, "{label}: no interrupt handler ran — the workload tests nothing");
            golden(&label, d, &mut printed);
        }
    }
    if !printed.is_empty() {
        eprintln!("GOLDEN (timer workloads):\n{}", printed.join("\n"));
    }
}

#[test]
fn a_checkpoint_restored_mid_count_continues_identically() {
    let mut printed = Vec::new();
    let (_, setup, _) = workloads().into_iter().find(|(n, _, _)| *n == "tb_cascade_l1").unwrap();
    for (speed, prefer) in [("1", MHZ_1), ("64", MHZ_64)] {
        let label = format!("restore_cascade@{speed}");
        // One machine runs straight through; the other is captured half-way and rebuilt.
        let mut straight = machine(prefer);
        setup(&mut straight);
        let mut first = machine(prefer);
        setup(&mut first);
        for _ in 0..25 {
            straight.run_for_full(STEP, &mut NullSink, |_, _, _, _, _, _, _| {});
            first.run_for_full(STEP, &mut NullSink, |_, _, _, _, _, _, _| {});
        }
        let cp = checkpoint(&first);
        let mut rebuilt = machine(prefer);
        restore_runtime_checkpoint(&mut rebuilt, &cp).expect("restore");
        assert!(rebuilt.restore_notes.is_empty(), "{label}: {:?}", rebuilt.restore_notes);
        let d = lockstep(&label, &mut straight, &mut rebuilt, 35);
        golden(&label, d, &mut printed);
    }
    if !printed.is_empty() {
        eprintln!("GOLDEN (restore):\n{}", printed.join("\n"));
    }
}

#[test]
fn a_booted_machine_is_deterministic_and_frozen() {
    if !std::path::Path::new(ROM_DIR).join("kernal-901227-03.bin").exists() {
        eprintln!("skip: ROMs absent at {ROM_DIR}");
        return;
    }
    let mut printed = Vec::new();
    for (speed, prefer) in [("1", MHZ_1), ("64", MHZ_64)] {
        let label = format!("booted@{speed}");
        let boot = |m: &mut Machine| {
            m.boot_from_dir(std::path::Path::new(ROM_DIR)).expect("boot ROMs");
            // BUG-061 — after a reset the Ultimate holds its C64 at 1 MHz for 2.06 s; the
            // 120-frame settle below outlasts it, so the `@64` workload is at 64 MHz by
            // the time the lockstep starts.
            m.set_u64_turbo(0x00, prefer);
            m.run_for_full(120 * FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
        };
        let (mut a, mut b) = (machine(prefer), machine(prefer));
        boot(&mut a);
        boot(&mut b);
        let d = lockstep(&label, &mut a, &mut b, 60);
        golden(&label, d, &mut printed);
    }
    if !printed.is_empty() {
        eprintln!("GOLDEN (booted):\n{}", printed.join("\n"));
    }
}
