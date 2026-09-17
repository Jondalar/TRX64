//! Spec 856 D1 — the turbo fast path may change the speed and nothing else.
//!
//! Written and green on the loop that synchronises at every instruction BEFORE the fast
//! path existed, so these pin today's behaviour instead of describing the new one.
//!
//! Two things can go wrong when instructions that do not advance `clk` stop paying for the
//! boundary sync:
//!
//!   - an IRQ acknowledge that lands inside one PHI2 cycle never reaches `IntStatus` — below
//!     the divider `clk_inc` returns early, so the boundary restamp is the only path it has
//!     (§3). The handler returns to a line that is still high and takes the same IRQ again;
//!   - something the fast path skipped was not idempotent after all, and state drifts.
//!
//! The first is caught by counting handler entries against the events that caused them, the
//! second by running every workload twice — fast path off and on — and demanding the same
//! machine at every frame boundary. Exact equality, not a bound.

use trx64_core::c64re_snapshot::capture_runtime_checkpoint;
use trx64_core::vic::SpeedProfile;
use trx64_core::{BusKind, Machine, NullSink, Observer};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
/// PAL: 63 cycles × 312 lines.
const FRAME: u64 = 19_656;
/// `$D031` speed bytes on the U64-II table. Bit 7 = badline timing.
const MHZ_1: u8 = 0x80;
const MHZ_16: u8 = 0x89;
const MHZ_64: u8 = 0x8f;
const MHZ_64_NO_BADLINE: u8 = 0x0f;

fn machine(prefer: u8, fast: bool) -> Machine {
    let mut m = Machine::new();
    m.set_machine_profile(SpeedProfile::U64);
    m.set_u64_turbo(0x00, prefer);
    m.turbo_fast_path = fast;
    m
}

fn start(m: &mut Machine, main: &[u8], handler: &[u8]) {
    m.poke(0xc000, main);
    m.poke(0xc100, handler);
    m.write_full(0x0001, 0x35); // BASIC + KERNAL out: the IRQ vector is RAM
    m.c64_core.reg_pc = 0xc000;
}

fn entries(m: &mut Machine) -> u64 {
    u64::from(m.read_full(0x0400)) | (u64::from(m.read_full(0x0401)) << 8)
}

// ── CIA1 timer A, acknowledged by reading $DC0D ──────────────────────────────────────────

/// SEI / vector → $C100 / mask all, clear pending / latch $0FFF / enable TA / start / CLI /
/// `JMP *` — the main loop never touches IO, so at 64 MHz it is exactly what the fast path
/// runs back to back.
const CIA_MAIN: [u8; 43] = [
    0x78, // C000 SEI
    0xa9, 0x00, 0x8d, 0xfe, 0xff, // LDA #$00 / STA $FFFE
    0xa9, 0xc1, 0x8d, 0xff, 0xff, // LDA #$C1 / STA $FFFF
    0xa9, 0x7f, 0x8d, 0x0d, 0xdc, // LDA #$7F / STA $DC0D
    0xad, 0x0d, 0xdc, // LDA $DC0D
    0xa9, 0xff, 0x8d, 0x04, 0xdc, // LDA #$FF / STA $DC04
    0xa9, 0x0f, 0x8d, 0x05, 0xdc, // LDA #$0F / STA $DC05
    0xa9, 0x81, 0x8d, 0x0d, 0xdc, // LDA #$81 / STA $DC0D
    0xa9, 0x11, 0x8d, 0x0e, 0xdc, // LDA #$11 / STA $DC0E
    0x58, // C027 CLI
    0x4c, 0x28, 0xc0, // C028 JMP $C028
];
/// INC $0400 / BNE +3 / INC $0401 / LDA $DC0D / RTI — 23 CPU cycles with the IRQ entry,
/// so at 64 MHz the whole handler, acknowledge included, fits inside one PHI2 cycle.
const CIA_HANDLER: [u8; 12] = [0xee, 0x00, 0x04, 0xd0, 0x03, 0xee, 0x01, 0x04, 0xad, 0x0d, 0xdc, 0x40];
const CIA_PERIOD: u64 = 0x1000;

#[test]
fn a_cia_acknowledge_inside_one_phi2_cycle_is_taken_once_per_underflow() {
    for (prefer, fast) in [(MHZ_1, false), (MHZ_64, false), (MHZ_64, true), (MHZ_64_NO_BADLINE, true)] {
        let mut m = machine(prefer, fast);
        start(&mut m, &CIA_MAIN, &CIA_HANDLER);
        let clk0 = m.c64_core.clk;
        m.run_for_full(50 * FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
        let underflows = (m.c64_core.clk - clk0) / CIA_PERIOD;
        let taken = entries(&mut m);
        assert!(underflows > 200, "the run covered enough underflows to mean something: {underflows}");
        assert!(
            taken.abs_diff(underflows) <= 2,
            "speed ${prefer:02x} fast={fast}: {taken} handler entries for {underflows} underflows"
        );
    }
}

// ── VIC raster, acknowledged by writing $D019 ─────────────────────────────────────────────

const RASTER_MAIN: [u8; 38] = [
    0x78, // C000 SEI
    0xa9, 0x00, 0x8d, 0xfe, 0xff, // LDA #$00 / STA $FFFE
    0xa9, 0xc1, 0x8d, 0xff, 0xff, // LDA #$C1 / STA $FFFF
    0xa9, 0x80, 0x8d, 0x12, 0xd0, // LDA #$80 / STA $D012
    0xad, 0x11, 0xd0, 0x29, 0x7f, 0x8d, 0x11, 0xd0, // LDA $D011 / AND #$7F / STA $D011
    0xa9, 0xff, 0x8d, 0x19, 0xd0, // LDA #$FF / STA $D019
    0xa9, 0x01, 0x8d, 0x1a, 0xd0, // LDA #$01 / STA $D01A
    0x58, // C022 CLI
    0x4c, 0x23, 0xc0, // C023 JMP $C023
];
/// INC $0400 / BNE +3 / INC $0401 / LDA #$FF / STA $D019 / RTI
const RASTER_HANDLER: [u8; 14] =
    [0xee, 0x00, 0x04, 0xd0, 0x03, 0xee, 0x01, 0x04, 0xa9, 0xff, 0x8d, 0x19, 0xd0, 0x40];

#[test]
fn a_raster_acknowledge_inside_one_phi2_cycle_is_taken_once_per_frame() {
    for (prefer, fast) in [(MHZ_1, false), (MHZ_64, false), (MHZ_64, true), (MHZ_64_NO_BADLINE, true)] {
        let mut m = machine(prefer, fast);
        start(&mut m, &RASTER_MAIN, &RASTER_HANDLER);
        let clk0 = m.c64_core.clk;
        m.run_for_full(50 * FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
        let frames = (m.c64_core.clk - clk0) / FRAME;
        let taken = entries(&mut m);
        assert!(
            taken.abs_diff(frames) <= 1,
            "speed ${prefer:02x} fast={fast}: {taken} handler entries in {frames} frames"
        );
    }
}

// ── the fast path changes nothing ─────────────────────────────────────────────────────────

/// Folds every retired instruction, every bus record and every interrupt, so two runs agree
/// on the whole observable stream and not only on where they end up. The bus records matter
/// on their own: an instruction batched without resetting the bus's pre-fetch state would
/// attribute its fetch to the previous opcode's PC — invisible in the instruction stream,
/// visible to every trace.
struct HashSink {
    h: u64,
    n: u64,
}
impl HashSink {
    fn new() -> Self {
        HashSink { h: 0xcbf2_9ce4_8422_2325, n: 0 }
    }
    fn fold(&mut self, v: u64) {
        self.h ^= v;
        self.h = self.h.wrapping_mul(0x0000_0100_0000_01b3);
    }
}
impl Observer for HashSink {
    #[allow(clippy::too_many_arguments)]
    fn on_instruction(&mut self, pc: u16, opcode: u8, _: u8, _: u8, a: u8, x: u8, y: u8, _: u8, p: u8, clk: u64) {
        self.fold(u64::from(pc));
        self.fold(u64::from(opcode));
        self.fold(u64::from(a) | (u64::from(x) << 8) | (u64::from(y) << 16) | (u64::from(p) << 24));
        self.fold(clk);
        self.n += 1;
    }
    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, pc: u16, clk: u64, old: u8) {
        self.fold(kind as u64 | (u64::from(addr) << 8) | (u64::from(value) << 24) | (u64::from(old) << 32));
        self.fold(u64::from(pc));
        self.fold(clk);
    }
    fn on_interrupt(&mut self, vector: u16, clk: u64) {
        self.fold(0xffff_0000 | u64::from(vector));
        self.fold(clk);
    }
}

fn leaf_diffs(prefix: &str, a: &serde_json::Value, b: &serde_json::Value, out: &mut Vec<String>) {
    use serde_json::Value;
    match (a, b) {
        (Value::Object(ma), Value::Object(mb)) => {
            let mut keys: Vec<&String> = ma.keys().chain(mb.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                match (ma.get(k), mb.get(k)) {
                    (Some(va), Some(vb)) => leaf_diffs(&p, va, vb, out),
                    (x, y) => out.push(format!("{p}: {x:?} vs {y:?}")),
                }
            }
        }
        (Value::Array(aa), Value::Array(ba)) if aa.len() == ba.len() => {
            for (i, (va, vb)) in aa.iter().zip(ba.iter()).enumerate() {
                if va != vb {
                    leaf_diffs(&format!("{prefix}[{i}]"), va, vb, out);
                }
            }
        }
        _ => {
            if a != b {
                out.push(format!("{prefix}: {a} vs {b}"));
            }
        }
    }
}

fn checkpoint(m: &Machine) -> serde_json::Value {
    capture_runtime_checkpoint(m, "", "d64", None, None, None, None)
}

/// Run `frames` PAL frames twice from identical machines — fast path off, then on — and
/// demand the same instruction stream and the same machine at every frame boundary.
fn same_with_and_without(label: &str, prefer: u8, frames: u64, setup: &dyn Fn(&mut Machine)) {
    let mut slow = machine(prefer, false);
    let mut fast = machine(prefer, true);
    setup(&mut slow);
    setup(&mut fast);
    for frame in 0..frames {
        let mut hs = HashSink::new();
        let mut hf = HashSink::new();
        slow.run_for_full(FRAME, &mut hs, |_, _, _, _, _, _, _| {});
        fast.run_for_full(FRAME, &mut hf, |_, _, _, _, _, _, _| {});
        assert_eq!(
            (hs.h, hs.n),
            (hf.h, hf.n),
            "{label} speed ${prefer:02x}: the instruction stream diverged in frame {frame}"
        );
        let (a, b) = (checkpoint(&slow), checkpoint(&fast));
        if a != b {
            let mut diffs = Vec::new();
            leaf_diffs("", &a, &b, &mut diffs);
            diffs.truncate(12);
            panic!("{label} speed ${prefer:02x}: machine state diverged after frame {frame}:\n  {}", diffs.join("\n  "));
        }
    }
}

/// Both IRQ sources at once through one handler that acknowledges both, and a main loop that
/// mixes pure RAM work with a VIC write and a CIA read every 256 iterations — so the fast
/// path is entered, left for IO, and interrupted, all in the same run.
const MIXED_MAIN: [u8; 80] = [
    0x78, // C000 SEI
    0xa9, 0x00, 0x8d, 0xfe, 0xff, // vector lo
    0xa9, 0xc1, 0x8d, 0xff, 0xff, // vector hi
    0xa9, 0x7f, 0x8d, 0x0d, 0xdc, // CIA1 mask all
    0xad, 0x0d, 0xdc, // clear pending
    0xa9, 0x37, 0x8d, 0x04, 0xdc, // latch lo
    0xa9, 0x05, 0x8d, 0x05, 0xdc, // latch hi ($0537)
    0xa9, 0x81, 0x8d, 0x0d, 0xdc, // enable TA
    0xa9, 0x11, 0x8d, 0x0e, 0xdc, // start
    0xa9, 0x40, 0x8d, 0x12, 0xd0, // raster $40
    0xad, 0x11, 0xd0, 0x29, 0x7f, 0x8d, 0x11, 0xd0, // raster bit 8 off
    0xa9, 0xff, 0x8d, 0x19, 0xd0, // ack VIC
    0xa9, 0x01, 0x8d, 0x1a, 0xd0, // enable raster IRQ
    0x58, // C03E CLI
    0xe6, 0xfb, // C03F INC $FB
    0xd0, 0x0a, // C041 BNE $C04D
    0xe6, 0xfc, // C043 INC $FC
    0xa5, 0xfc, // C045 LDA $FC
    0x8d, 0x20, 0xd0, // C047 STA $D020
    0xad, 0x01, 0xdc, // C04A LDA $DC01
    0x4c, 0x3f, 0xc0, // C04D JMP $C03F
];
/// LDA $DC0D / LDA #$FF / STA $D019 / INC $0400 / BNE +3 / INC $0401 / RTI
const MIXED_HANDLER: [u8; 17] =
    [0xad, 0x0d, 0xdc, 0xa9, 0xff, 0x8d, 0x19, 0xd0, 0xee, 0x00, 0x04, 0xd0, 0x03, 0xee, 0x01, 0x04, 0x40];

#[test]
fn the_fast_path_changes_nothing_on_a_mixed_irq_and_io_workload() {
    let setup = |m: &mut Machine| start(m, &MIXED_MAIN, &MIXED_HANDLER);
    for prefer in [MHZ_1, MHZ_16, MHZ_64, MHZ_64_NO_BADLINE] {
        same_with_and_without("mixed", prefer, 40, &setup);
    }
}

/// The same with a 16 MB REU on the port — the shape UE2 runs in, where the expansion chain
/// is live, its lines are polled and a DMA transfer can be armed.
#[test]
fn the_fast_path_changes_nothing_with_a_device_on_the_port() {
    let setup = |m: &mut Machine| {
        assert!(m.attach_reu(16384), "16 MB is a real REU size");
        start(m, &MIXED_MAIN, &MIXED_HANDLER);
    };
    for prefer in [MHZ_16, MHZ_64, MHZ_64_NO_BADLINE] {
        same_with_and_without("reu", prefer, 40, &setup);
    }
}

#[test]
fn the_fast_path_changes_nothing_on_a_booted_machine() {
    if !std::path::Path::new(ROM_DIR).join("kernal-901227-03.bin").exists() {
        eprintln!("skip: ROMs absent at {ROM_DIR}");
        return;
    }
    // KERNAL IRQ, cursor blink, keyboard scan and the drive: the ordinary machine, sped up.
    let setup = |m: &mut Machine| {
        m.boot_from_dir(std::path::Path::new(ROM_DIR)).expect("boot ROMs");
        m.run_for_full(150 * FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
    };
    for prefer in [MHZ_16, MHZ_64, MHZ_64_NO_BADLINE] {
        same_with_and_without("booted", prefer, 40, &setup);
    }
}
