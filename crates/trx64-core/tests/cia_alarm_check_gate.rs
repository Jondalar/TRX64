//! Spec 857 D1 — the CIA alarm check may change the speed and nothing else.
//!
//! Two kinds of evidence, because one cannot do both jobs:
//!
//!   - **Lockstep equality, check off against check on.** Same workload, two machines, stopped
//!     every 7919 cycles (a prime, so the stops land between register accesses and not on
//!     them) and compared on the instruction stream, every bus record, every interrupt, the
//!     full runtime checkpoint and a `peek` of all 32 CIA registers.
//!   - **Frozen digests of today's behaviour.** Equality alone cannot show that D2 left the old
//!     path alone — D2 changes both sides at once. So the check-off digest of every workload
//!     was recorded on the code before 857 touched the CIA, and it must not move.
//!
//! The workloads are the ones §2 of the spec says the per-cycle catch-up was hiding: Timer A
//! continuous and one-shot, Timer B counting Timer A underflows at latches 0, 1 and 2, CIA2 on
//! NMI, the TOD alarm, a checkpoint restored mid-count, and a booted machine — each at 1 and at
//! 64 MHz, because at 64 MHz the prologue runs about eighteen times per PHI2 cycle.
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

fn machine(prefer: u8, check: bool) -> Machine {
    let mut m = Machine::new();
    m.set_machine_profile(SpeedProfile::U64);
    m.set_u64_turbo(0x00, prefer);
    m.cia_alarm_check = check;
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

/// Lockstep both machines and return the digest of the check-off side.
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
        fnv_bytes(&mut digest, serde_json::to_string(&co).unwrap().as_bytes());
    }
    digest
}

// ── frozen digests of the code before 857 touched the CIA ────────────────────────────────

/// Recorded with the D4 switch in place and reading nothing — the CIA code exactly as 857
/// found it. The 64 MHz digests are recorded after `7ce542a`: this gate found that checkpoints
/// did not carry the turbo phase (a defect since 851, the restore case diverged), and the fix
/// adds `turboPhase` to every turbo checkpoint, which the digests hash. The 1 MHz digests are
/// the same on both sides of that fix.
///
/// All re-recorded for Spec 843 D1, which fills `vicProvenance` in every checkpoint (it was
/// a hardcoded `null`). With that one field set back to `null` the digests are exactly the
/// ones recorded before — checked on the 843 branch before re-recording — so nothing the CIA
/// does moved; only the checkpoint says more. The same gate found that a restored machine
/// lost the record (restore did not read it back), fixed in `restore_vic_provenance`.
/// **Every `@64` digest re-recorded for Spec 868 §9 (2026-09-21), and not one `@1` digest
/// moved.** A `$D031` write is adopted at the next PHI2 EDGE instead of from the next
/// instruction, so a turbo machine reaches its speed a cycle earlier and every observable
/// stream at 64 MHz shifts with it; at 1 MHz there is no divider to adopt and the digests
/// are bit-for-bit the ones recorded before. That split is the evidence the change is
/// confined to turbo — it is the same shape as the 7-game screenshot gate, one level down.
///
/// The same change made this gate earn its keep for the third time: `restore_cascade@64`
/// failed on EQUALITY, not on its digest, because a restored machine only learned its
/// speed at the next instruction boundary and spent a PHI2 cycle at the wrong divider.
/// `restore_runtime_checkpoint` puts the divider back in force immediately now.
/// **`booted@64` re-recorded for BUG-061 (2026-09-23), and it alone.** After a reset the
/// Ultimate holds its C64 at 1 MHz for 2.06 s, and this is the only workload that boots —
/// so its machine now spends most of the 120-frame settle at 1 MHz and reaches the
/// lockstep in a different state. Every other digest, including every `@64` one that does
/// not reset, is bit-identical to the line above.
const GOLDEN: &[(&str, u64)] = &[
    ("ta_irq@1", 0xe32345655ea52187),
    ("ta_irq@64", 0x103bd9367deaf0ae),
    ("ta_irq_timer_read@1", 0x82b1452c59e244c7),
    ("ta_irq_timer_read@64", 0x505800baa5bb5916),
    ("ta_oneshot@1", 0xe5b6f8ff84cbca64),
    ("ta_oneshot@64", 0x1842ff65e4a3264a),
    ("tb_cascade_l0@1", 0xa3b68baf80d90fd8),
    ("tb_cascade_l0@64", 0x7715fc9bb4f39a31),
    ("tb_cascade_l1@1", 0x390ad349e24c3a37),
    ("tb_cascade_l1@64", 0x0ef43999b24d6d2b),
    ("tb_cascade_l2@1", 0x42064c33bfaf60f5),
    ("tb_cascade_l2@64", 0xafc4462d480d855e),
    ("cia2_nmi@1", 0x50afb16bdf9d38ae),
    ("cia2_nmi@64", 0x090e1a4cbf8358c3),
    ("tod_alarm@1", 0x0a9b7d74247126c1),
    ("tod_alarm@64", 0xda0e684ceaa882f2),
    ("restore_cascade@1/check=false", 0x734ab0a82991ce2e),
    ("restore_cascade@64/check=false", 0x6381f6030a3b5d9f),
    ("booted@1", 0xc9fc1a34eda0ea27),
    ("booted@64", 0x5296fe658776b17e),
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
fn the_alarm_check_changes_nothing_on_the_timer_workloads() {
    let mut printed = Vec::new();
    for (name, setup, steps) in workloads() {
        for (speed, prefer) in [("1", MHZ_1), ("64", MHZ_64)] {
            let label = format!("{name}@{speed}");
            let (mut off, mut on) = (machine(prefer, false), machine(prefer, true));
            setup(&mut off);
            setup(&mut on);
            let d = lockstep(&label, &mut off, &mut on, steps);
            // Something must actually have happened, or equality proves nothing.
            let taken = u64::from(off.read_full(0x0400)) + u64::from(off.read_full(0x0402));
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
        for check in [false, true] {
            let label = format!("restore_cascade@{speed}/check={check}");
            // One machine runs straight through; the other is captured half-way and rebuilt.
            let mut straight = machine(prefer, check);
            setup(&mut straight);
            let mut first = machine(prefer, check);
            setup(&mut first);
            for _ in 0..25 {
                straight.run_for_full(STEP, &mut NullSink, |_, _, _, _, _, _, _| {});
                first.run_for_full(STEP, &mut NullSink, |_, _, _, _, _, _, _| {});
            }
            let cp = checkpoint(&first);
            let mut rebuilt = machine(prefer, check);
            restore_runtime_checkpoint(&mut rebuilt, &cp).expect("restore");
            let d = lockstep(&label, &mut straight, &mut rebuilt, 35);
            if !check {
                golden(&label, d, &mut printed);
            }
        }
    }
    if !printed.is_empty() {
        eprintln!("GOLDEN (restore):\n{}", printed.join("\n"));
    }
}

#[test]
fn the_alarm_check_changes_nothing_on_a_booted_machine() {
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
        let (mut off, mut on) = (machine(prefer, false), machine(prefer, true));
        boot(&mut off);
        boot(&mut on);
        let d = lockstep(&label, &mut off, &mut on, 60);
        golden(&label, d, &mut printed);
    }
    if !printed.is_empty() {
        eprintln!("GOLDEN (booted):\n{}", printed.join("\n"));
    }
}
