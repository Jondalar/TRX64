//! Issue #3 (Spec 888 acceptance 1) — writing the ICR mask does not acknowledge.
//!
//! A 6526 keeps an interrupt it has signalled — IR (bit 7 of `$DC0D`) set, the pin low —
//! until the ICR is READ (`ciacore.c`: only `ciacore_read(CIA_ICR)` clears `irqflags` and
//! calls `my_set_int(false)`; the ICR store only changes the mask). The repro from the
//! issue: CIA 1 Timer A interrupts every $4000 cycles, and the handler masks every CIA 1
//! interrupt off (`$DC0D = $7F`) without reading `$DC0D`, then leaves through `$EA81`
//! (past the KERNAL's own `LDA $DC0D`). On the chip the IRQ re-enters for ever and the
//! main loop starves.
//!
//! The CIA 2 analogue is the NMI, which is edge-triggered: a line that is never released
//! never makes a second edge. The handler masks off, then on again, without reading
//! `$DD0D` — the line stays low, so exactly one NMI is taken however often Timer A
//! underflows. A chip that released the line on the mask write would make a fresh edge
//! on every re-enable.
//!
//! Booted machine, ROMs from `TRX64_ROM_DIR` or the sibling C64RE checkout.

use std::path::PathBuf;
use trx64_core::{Machine, NullSink};

const FRAME: u64 = 19_656;
const IRQ_COUNT: u16 = 0xc000;
const MAIN_COUNT: u16 = 0xc002;
const SEEN_ICR: u16 = 0xc004;

fn rom_dir() -> PathBuf {
    std::env::var_os("TRX64_ROM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms")))
}

fn booted() -> Option<Machine> {
    let dir = rom_dir();
    if !dir.join("kernal-901227-03.bin").exists() {
        eprintln!("skip: ROMs absent at {}", dir.display());
        return None;
    }
    let mut m = Machine::new();
    m.boot_from_dir(&dir).expect("boot ROMs");
    m.run_for_full(3_000_000, &mut NullSink, |_, _, _, _, _, _, _| {});
    Some(m)
}

/// A few bytes of 6502 at $C100, labels resolved by hand.
struct Asm {
    code: Vec<u8>,
}
impl Asm {
    const ORG: u16 = 0xc100;
    fn pc(&self) -> u16 {
        Self::ORG + self.code.len() as u16
    }
    fn b(&mut self, bytes: &[u8]) -> &mut Self {
        self.code.extend_from_slice(bytes);
        self
    }
    fn abs(&mut self, op: u8, a: u16) -> &mut Self {
        self.b(&[op, a as u8, (a >> 8) as u8])
    }
}

fn word(m: &Machine, a: u16) -> u16 {
    m.read_full(a) as u16 | (m.read_full(a + 1) as u16) << 8
}

/// The issue's program for `cia` ($DC00 or $DD00), vector `vec` ($0314 IRQ or $0318 NMI),
/// with `handler` emitting everything after the count.
fn program(cia: u16, vec: u16, handler: impl Fn(&mut Asm)) -> Vec<u8> {
    let icr = cia + 0x0d;
    let mut a = Asm { code: Vec::new() };
    a.b(&[0x78]); // SEI
    a.b(&[0xa9, 0x7f]).abs(0x8d, icr); // all off
    a.abs(0xad, icr); // and acknowledged
    let irq_lo_at = a.code.len();
    a.b(&[0xa9, 0x00, 0xa2, 0x00]); // LDA #<irq / LDX #>irq (patched below)
    a.abs(0x8d, vec).abs(0x8e, vec + 1);
    a.b(&[0xa9, 0x00]);
    for c in [IRQ_COUNT, IRQ_COUNT + 1, MAIN_COUNT, MAIN_COUNT + 1, SEEN_ICR] {
        a.abs(0x8d, c);
    }
    a.b(&[0xa9, 0x00, 0xa2, 0x40]).abs(0x8d, cia + 4).abs(0x8e, cia + 5); // Timer A = $4000
    a.b(&[0xa9, 0x81]).abs(0x8d, icr); // Timer A interrupt on
    a.b(&[0xa9, 0x11]).abs(0x8d, cia + 0x0e); // load and start, continuous
    a.b(&[0x58]); // CLI
    let main = a.pc();
    a.abs(0xee, MAIN_COUNT).b(&[0xd0, 0xfb]).abs(0xee, MAIN_COUNT + 1).abs(0x4c, main);
    let irq = a.pc();
    a.abs(0xee, IRQ_COUNT).b(&[0xd0, 0x03]).abs(0xee, IRQ_COUNT + 1);
    handler(&mut a);
    a.code[irq_lo_at + 1] = irq as u8;
    a.code[irq_lo_at + 3] = (irq >> 8) as u8;
    a.code
}

fn run(m: &mut Machine, code: &[u8], frames: u64) {
    m.poke(Asm::ORG, code);
    m.c64_core.reg_pc = Asm::ORG;
    m.run_for_full(frames * FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
}

/// The issue's repro, verbatim: mask off without reading, leave through $EA81.
#[test]
fn a_cia1_mask_write_does_not_release_the_irq() {
    let Some(mut m) = booted() else { return };
    let code = program(0xdc00, 0x0314, |a| {
        a.b(&[0xa9, 0x7f]).abs(0x8d, 0xdc0d); // mask off — $DC0D is NOT read
        a.abs(0x4c, 0xea81); // PLA TAY PLA TAX PLA RTI
    });
    // The main loop runs until Timer A first underflows ($4000 cycles, under a frame).
    run(&mut m, &code, 2);
    let (irqs0, mains0) = (word(&m, IRQ_COUNT), word(&m, MAIN_COUNT));
    m.run_for_full(98 * FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
    let (irqs, mains) = (word(&m, IRQ_COUNT), word(&m, MAIN_COUNT));
    eprintln!(
        "2 frames: IRQ_COUNT={irqs0} MAIN_COUNT={mains0}; 100 frames: IRQ_COUNT={irqs} MAIN_COUNT={mains}, \
         $DC0D peeks ${:02x}",
        m.read_full(0xdc0d)
    );
    assert!(irqs0 > 0, "the first IRQ was taken within two frames");
    assert!(irqs > 10_000, "the IRQ re-enters for ever: {irqs}");
    assert_eq!(mains, mains0, "the main loop starves once the IRQ is up");
    // Nobody read the ICR: TA and IR are still there for the next reader.
    assert_eq!(m.read_full(0xdc0d) & 0x81, 0x81);
}

/// The same handler reads $DC0D after its mask write: IR (bit 7) is still set — the write
/// did not clear it — and the read is what lets the line go.
#[test]
fn a_cia1_icr_read_after_a_mask_write_shows_ir() {
    let Some(mut m) = booted() else { return };
    let code = program(0xdc00, 0x0314, |a| {
        a.b(&[0xa9, 0x7f]).abs(0x8d, 0xdc0d); // mask off
        a.abs(0xad, 0xdc0d).abs(0x8d, SEEN_ICR); // now read it
        a.abs(0x4c, 0xea81);
    });
    run(&mut m, &code, 10);
    assert_eq!(m.read_full(SEEN_ICR) & 0x81, 0x81, "IR and TA after a mask-only write: ${:02x}", m.read_full(SEEN_ICR));
    assert_eq!(word(&m, IRQ_COUNT), 1, "masked and acknowledged: one IRQ");
    assert!(word(&m, MAIN_COUNT) > 1000, "and the main loop runs");
}

/// CIA 2 / NMI: masked off and on again without a read, the line never goes high, so the
/// edge-triggered NMI is taken once — however many underflows follow.
#[test]
fn a_cia2_mask_write_does_not_release_the_nmi() {
    let Some(mut m) = booted() else { return };
    let code = program(0xdd00, 0x0318, |a| {
        a.b(&[0xa9, 0x7f]).abs(0x8d, 0xdd0d); // mask off — $DD0D is NOT read
        a.b(&[0xa9, 0x81]).abs(0x8d, 0xdd0d); // and on again
        a.b(&[0x40]); // RTI
    });
    run(&mut m, &code, 100);
    let (nmis, mains) = (word(&m, IRQ_COUNT), word(&m, MAIN_COUNT));
    eprintln!("100 frames: NMI_COUNT={nmis} MAIN_COUNT={mains}, $DD0D peeks ${:02x}", m.read_full(0xdd0d));
    assert_eq!(nmis, 1, "one edge, one NMI");
    assert!(mains > 1000, "the main loop runs");
    assert_eq!(m.read_full(0xdd0d) & 0x81, 0x81, "TA and IR wait for a read");
}

/// And reading $DD0D after the mask write shows IR set.
#[test]
fn a_cia2_icr_read_after_a_mask_write_shows_ir() {
    let Some(mut m) = booted() else { return };
    let code = program(0xdd00, 0x0318, |a| {
        a.b(&[0xa9, 0x7f]).abs(0x8d, 0xdd0d);
        a.abs(0xad, 0xdd0d).abs(0x8d, SEEN_ICR);
        a.b(&[0x40]);
    });
    run(&mut m, &code, 10);
    assert_eq!(m.read_full(SEEN_ICR) & 0x81, 0x81, "${:02x}", m.read_full(SEEN_ICR));
    assert_eq!(word(&m, IRQ_COUNT), 1);
}
