//! Tick-hook foundation probe (Phase-0, ADR-066) — validates the three
//! observability hook points added to the verbatim full SC path are wired and
//! ZERO-COST-WHEN-OFF correct:
//!
//!   (a) run-loop EXEC BREAKPOINT — `run_for_full_capped_dbg` with a `breakpoints`
//!       set halts with `RunStop::Breakpoint(pc)` at that PC, BEFORE executing it
//!       (= TS integrated-session.ts:973, VICE break-on-exec).
//!   (b) ACCESS-WATCH — an armed per-address watch + an `Observer::on_access`
//!       returning `true` on a store sets `halt_requested`, and the run stops at
//!       the NEXT instruction boundary with `RunStop::Observer` (= ts:495/989).
//!   (c) on_interrupt — fires for a HARDWARE IRQ on the full SC path during boot
//!       (= cpu65xx-vice.ts:666 onInterruptServiced(0xfffe, clk)). DEAD before
//!       this phase (only the CPU-isolated cpu.rs path fired it).
//!
//! These hooks are ADDITIVE and must not change CPU timing; the byte-exact gates
//! prove that separately. This probe only proves the hooks FIRE.

use std::collections::HashSet;
use std::path::Path;
use trx64_core::{BusKind, Machine, Observer, RunStop};

const ROM_DIR: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";

fn roms_present() -> bool {
    Path::new(ROM_DIR)
        .join("kernal-901227-03.bin")
        .exists()
}

/// Observer that records on_interrupt fires and, when an access-watch is armed,
/// requests a halt the first time a watched WRITE is observed.
#[derive(Default)]
struct ProbeSink {
    /// (vector, clk) for every on_interrupt fired.
    interrupts: Vec<(u16, u64)>,
    /// (kind, addr, value) for every on_access hit (only when a watch is armed).
    accesses: Vec<(BusKind, u16, u8)>,
    /// When true, on_access returns true (request halt) on the first WRITE hit.
    halt_on_write: bool,
    /// Latched so we only request the halt once.
    halt_fired: bool,
}

impl Observer for ProbeSink {
    #[allow(clippy::too_many_arguments)]
    fn on_instruction(
        &mut self,
        _pc: u16,
        _op: u8,
        _b1: u8,
        _b2: u8,
        _a: u8,
        _x: u8,
        _y: u8,
        _sp: u8,
        _p: u8,
        _clk: u64,
    ) {
    }
    fn on_bus(&mut self, _kind: BusKind, _addr: u16, _value: u8, _pc: u16, _clk: u64, _old: u8) {}
    fn on_interrupt(&mut self, vector: u16, clk: u64) {
        self.interrupts.push((vector, clk));
    }
    fn on_access(&mut self, kind: BusKind, addr: u16, value: u8) -> bool {
        self.accesses.push((kind, addr, value));
        if self.halt_on_write && !self.halt_fired && kind == BusKind::Write {
            self.halt_fired = true;
            return true; // request halt — honored at the next boundary
        }
        false
    }
}

/// (c) on_interrupt fires for a HW IRQ on the full SC path during boot.
#[test]
fn on_interrupt_fires_in_full_sc_path() {
    if !roms_present() {
        eprintln!("ROMs absent; skipping on_interrupt probe");
        return;
    }
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut sink = ProbeSink::default();
    // Far enough for the CIA1 Timer-A cursor IRQ to fire (same window the
    // irq_push_pc_probe uses to reach $EA31).
    m.run_for_full(2_300_000, &mut sink, |_, _, _, _, _, _, _| {});

    assert!(
        !sink.interrupts.is_empty(),
        "on_interrupt never fired in the full SC path over a 2.3M-cycle boot"
    );
    // At least one IRQ-vector ($FFFE) interrupt must have been serviced.
    let irqs = sink
        .interrupts
        .iter()
        .filter(|(v, _)| *v == 0xfffe)
        .count();
    assert!(
        irqs > 0,
        "no IRQ-vector ($FFFE) on_interrupt fired; got vectors: {:?}",
        sink.interrupts.iter().map(|(v, _)| *v).collect::<Vec<_>>()
    );
    eprintln!(
        "on_interrupt fired {} time(s) ({} IRQ@$FFFE) — first @ vector=${:04X} clk={}",
        sink.interrupts.len(),
        irqs,
        sink.interrupts[0].0,
        sink.interrupts[0].1
    );
}

/// (a) An exec breakpoint halts the full SC run with RunStop::Breakpoint(pc) AT
/// that PC, before executing it.
#[test]
fn breakpoint_halts_at_pc() {
    if !roms_present() {
        eprintln!("ROMs absent; skipping breakpoint probe");
        return;
    }
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");

    // After boot, the verbatim core sits at the KERNAL reset entry ($FCE2,
    // LDX #$FF). A breakpoint there must halt IMMEDIATELY (0 instructions run),
    // proving the BEFORE-execute boundary check.
    let start_pc = m.c64_core.reg_pc;
    let mut bps = HashSet::new();
    bps.insert(start_pc);
    let mut sink = ProbeSink::default();
    let stop = m.run_for_full_capped_dbg(
        5_000_000,
        9_999_999,
        Some(&bps),
        None,
        None,
        &mut sink,
        |_, _, _, _, _, _, _| {},
    );
    assert_eq!(
        stop,
        RunStop::Breakpoint(start_pc),
        "breakpoint at the start PC ${start_pc:04X} did not halt immediately (got {stop:?})"
    );
    assert_eq!(
        m.c64_core.reg_pc, start_pc,
        "CPU advanced past the breakpoint PC"
    );

    // Now a DEEPER breakpoint: $EA31 (the KERNAL IRQ handler) is reached only
    // after the boot runs thousands of instructions and the first HW IRQ fires.
    // The run must execute, then halt AT $EA31.
    let mut m2 = Machine::new();
    m2.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut bps2 = HashSet::new();
    bps2.insert(0xEA31u16);
    let mut sink2 = ProbeSink::default();
    let stop2 = m2.run_for_full_capped_dbg(
        5_000_000,
        9_999_999,
        Some(&bps2),
        None,
        None,
        &mut sink2,
        |_, _, _, _, _, _, _| {},
    );
    assert_eq!(
        stop2,
        RunStop::Breakpoint(0xEA31),
        "breakpoint at $EA31 (IRQ handler) was not hit during boot (got {stop2:?})"
    );
    assert_eq!(m2.c64_core.reg_pc, 0xEA31, "halted PC is not $EA31");
    eprintln!("breakpoint halted at $FCE2 (immediate) and $EA31 (after boot) — OK");
}

/// (b) An armed access-watch + an on_access that requests halt on a store stops
/// the run with RunStop::Observer at the next boundary.
#[test]
fn access_watch_halts_at_next_boundary() {
    if !roms_present() {
        eprintln!("ROMs absent; skipping access-watch probe");
        return;
    }
    // First, a recording run to find a RAM address the boot actually writes to
    // (so the watch is guaranteed to hit). Watch the whole low RAM and capture
    // the first store address.
    let target = {
        let mut m = Machine::new();
        m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
        // Arm a watch over $0002..$0400 (zero page + stack + low RAM); the boot
        // writes there within a handful of instructions. on_access records the
        // hit but does NOT halt (halt_on_write = false here).
        let mut watch = Box::new([0u8; 0x10000]);
        for w in watch.iter_mut().take(0x0400).skip(0x0002) {
            *w = 1;
        }
        let mut sink = ProbeSink::default();
        // A short window is enough to record the first write.
        m.run_for_full_capped_dbg(
            50_000,
            99_999,
            None,
            None,
            Some(&*watch),
            &mut sink,
            |_, _, _, _, _, _, _| {},
        );
        let first_write = sink
            .accesses
            .iter()
            .find(|(k, _, _)| *k == BusKind::Write)
            .map(|(_, a, _)| *a);
        first_write.expect("boot performed no RAM write in $0002..$0400 within 50k cycles")
    };

    // Now arm a watch on ONLY that address and request a halt on the first write
    // to it. The run must stop at the NEXT boundary with RunStop::Observer.
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut watch = Box::new([0u8; 0x10000]);
    watch[target as usize] = 1;
    let mut sink = ProbeSink {
        halt_on_write: true,
        ..Default::default()
    };
    let stop = m.run_for_full_capped_dbg(
        2_000_000,
        9_999_999,
        None,
        None,
        Some(&*watch),
        &mut sink,
        |_, _, _, _, _, _, _| {},
    );
    assert_eq!(
        stop,
        RunStop::Observer,
        "watched store to ${target:04X} did not halt the run (got {stop:?})"
    );
    assert!(
        sink.halt_fired,
        "on_access never requested the halt for ${target:04X}"
    );
    // The recorded access that fired the halt must be a WRITE to the target.
    let hit = sink
        .accesses
        .iter()
        .find(|(k, a, _)| *k == BusKind::Write && *a == target)
        .expect("no WRITE access recorded for the watched address");
    eprintln!(
        "access-watch on ${:04X} fired RunStop::Observer (write value=${:02X}) — OK",
        hit.1, hit.2
    );
}
