//! Spec 850 gate — the expansion port as a device interface.
//!
//! Every case runs a real program through the CPU bus, never by poking the device or
//! the chip: 815's lesson is that a gate which pokes the chip proves the wrong door.
//! The device here records what reached it through a shared log, because the machine
//! owns the boxed device once it is attached.
//!
//! No ROMs are needed except for the cartridge case, which skips loudly without them.

use std::path::Path;
use std::sync::{Arc, Mutex};
use trx64_core::c64_6510core::{IK_IRQ, IK_NMI, INT_SRC_EXPANSION, INT_SRC_RESTORE};
use trx64_core::{Access, AccessKind, ExpansionDevice, Hold, Machine, NullSink, PortLines, RunStop};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");

#[derive(Default)]
struct Log {
    reads: Vec<(Access, Option<u8>)>,
    writes: Vec<(Access, u8)>,
    snoops: Vec<(Access, u8)>,
    /// What `read` and `peek` answer.
    answer: Option<u8>,
    lines: PortLines,
    /// A write to this address requests a stop.
    stop_on_write: Option<u16>,
    stop_pending: bool,
    /// A read of this address drops the IRQ line.
    drop_irq_on_read: Option<u16>,
    /// A write to this address raises the IRQ line.
    raise_irq_on_write: Option<u16>,
}

struct Recorder {
    log: Arc<Mutex<Log>>,
    snoop: Vec<u16>,
}

impl ExpansionDevice for Recorder {
    fn read(&mut self, a: Access, cart: Option<u8>) -> Option<u8> {
        let mut l = self.log.lock().unwrap();
        l.reads.push((a, cart));
        if l.drop_irq_on_read == Some(a.addr) {
            l.lines.irq = false;
        }
        l.answer
    }
    fn peek(&self, _addr: u16, _cart: Option<u8>) -> Option<u8> {
        self.log.lock().unwrap().answer
    }
    fn write(&mut self, a: Access, value: u8) {
        let mut l = self.log.lock().unwrap();
        l.writes.push((a, value));
        if l.stop_on_write == Some(a.addr) {
            l.stop_pending = true;
        }
        if l.raise_irq_on_write == Some(a.addr) {
            l.lines.irq = true;
        }
    }
    fn snoop_addresses(&self) -> &[u16] {
        &self.snoop
    }
    fn snoop_write(&mut self, a: Access, value: u8) {
        self.log.lock().unwrap().snoops.push((a, value));
    }
    fn lines(&self) -> PortLines {
        self.log.lock().unwrap().lines
    }
    fn take_stop(&mut self) -> bool {
        std::mem::take(&mut self.log.lock().unwrap().stop_pending)
    }
}

fn log_with(f: impl FnOnce(&mut Log)) -> Arc<Mutex<Log>> {
    let mut l = Log::default();
    f(&mut l);
    Arc::new(Mutex::new(l))
}

fn attach(m: &mut Machine, log: &Arc<Mutex<Log>>, snoop: &[u16]) {
    m.attach_expansion(Box::new(Recorder { log: log.clone(), snoop: snoop.to_vec() }));
}

/// Poke `code` at `origin`, bank `$01`, point the SC core at it and run `instrs`
/// instructions on the full machine.
fn run_at(m: &mut Machine, origin: u16, port01: u8, code: &[u8], instrs: u64) -> RunStop {
    m.poke(origin, code);
    m.write_full(0x0001, port01);
    m.c64_core.reg_pc = origin;
    m.run_for_full_capped(instrs * 64, instrs, &mut NullSink, |_, _, _, _, _, _, _| {})
}

fn cpu_reads(log: &Arc<Mutex<Log>>) -> Vec<(u16, AccessKind, Option<u8>)> {
    log.lock().unwrap().reads.iter().map(|(a, c)| (a.addr, a.kind, *c)).collect()
}

// ── R1 ────────────────────────────────────────────────────────────────────────────

/// LDA #$41 / STA $DF1D / LDA $DF1C / STA $0400
const R1_PROG: [u8; 11] = [0xa9, 0x41, 0x8d, 0x1d, 0xdf, 0xad, 0x1c, 0xdf, 0x8d, 0x00, 0x04];

#[test]
fn r1_the_device_sees_the_cpu_without_a_cartridge_and_its_byte_wins() {
    let log = log_with(|l| l.answer = Some(0x5a));
    let mut m = Machine::new();
    attach(&mut m, &log, &[]);
    run_at(&mut m, 0xc000, 0x37, &R1_PROG, 4);

    let l = log.lock().unwrap();
    assert_eq!(l.writes.len(), 1, "one write reached the device");
    assert_eq!((l.writes[0].0.addr, l.writes[0].1, l.writes[0].0.kind), (0xdf1d, 0x41, AccessKind::Cpu));
    assert_eq!(l.reads.len(), 1, "one read reached the device");
    assert_eq!((l.reads[0].0.addr, l.reads[0].0.kind, l.reads[0].1), (0xdf1c, AccessKind::Cpu, None));
    drop(l);
    assert_eq!(m.read_full(0x0400), 0x5a, "A holds the device's byte");
    assert!(m.cartridge.is_none(), "attaching a device plugs in no cartridge");
}

#[test]
fn r1_a_device_that_declines_leaves_the_open_bus_exactly_as_before() {
    let log = log_with(|_| {});
    let mut with = Machine::new();
    attach(&mut with, &log, &[]);
    run_at(&mut with, 0xc000, 0x37, &R1_PROG, 4);

    let mut without = Machine::new();
    run_at(&mut without, 0xc000, 0x37, &R1_PROG, 4);

    assert_eq!(log.lock().unwrap().reads.len(), 1, "the device was asked");
    assert_eq!(with.read_full(0x0400), without.read_full(0x0400), "None keeps the pre-850 byte");
    assert_eq!(with.c64_core.clk, without.c64_core.clk, "and the timing");
}

#[test]
fn r1_all_ram_banking_never_calls_the_device() {
    let log = log_with(|l| l.answer = Some(0x5a));
    let mut m = Machine::new();
    attach(&mut m, &log, &[]);
    run_at(&mut m, 0xc000, 0x34, &R1_PROG, 4);
    let l = log.lock().unwrap();
    assert!(l.reads.is_empty() && l.writes.is_empty(), "$01=$34 maps RAM, not I/O");
}

#[test]
fn r1_a_page_crossing_dummy_read_reaches_the_device_first() {
    let log = log_with(|_| {});
    let mut m = Machine::new();
    attach(&mut m, &log, &[]);
    // LDX #$1F / LDA $DEFF,X
    run_at(&mut m, 0xc000, 0x37, &[0xa2, 0x1f, 0xbd, 0xff, 0xde], 2);
    assert_eq!(
        cpu_reads(&log),
        vec![(0xde1e, AccessKind::Dummy, None), (0xdf1e, AccessKind::Cpu, None)],
        "the uncorrected address first, as a dummy read"
    );
}

/// 850 R1 originally read "no reset EVER calls the device", and that was too broad. The
/// rule it was written for is the UCI block, which must survive a C64 reset — only the
/// FPGA reset clears it. But the connector really does carry /RESET, and VICE resets the
/// REU with the cartridge, so the choice belongs to the device: `ExpansionDevice::reset`
/// defaults to nothing, and a device that stays silent keeps exactly 850's behaviour.
/// This case now pins that default; `reu_gate` pins the other half.
#[test]
fn r1_a_device_that_declines_the_reset_is_not_called() {
    let log = log_with(|_| {});
    let mut m = Machine::new();
    attach(&mut m, &log, &[0xff00]);
    m.cold_reset();
    m.warm_reset();
    let l = log.lock().unwrap();
    assert!(l.reads.is_empty() && l.writes.is_empty() && l.snoops.is_empty(), "no access on reset");
    drop(l);
    assert!(m.cartridge.is_none());
    assert!(m.expansion.is_some(), "and the device survives the reset");
}

#[test]
fn r1_beside_a_cartridge_the_device_gets_its_byte_and_decides() {
    if !Path::new(ROM_DIR).join("kernal-901227-03.bin").exists() {
        eprintln!("SKIP: ROMs absent ({ROM_DIR})");
        return;
    }
    // LDA #$06 / STA $DE02 (EasyFlash 8K game mode) / LDA #$77 / STA $DF1C (EasyFlash RAM)
    // / LDA $DF1C / STA $0400 / JMP *
    let prog: &[u8] = &[
        0xa9, 0x06, 0x8d, 0x02, 0xde, 0xa9, 0x77, 0x8d, 0x1c, 0xdf, 0xad, 0x1c, 0xdf, 0x8d, 0x00, 0x04,
        0x4c, 0x10, 0x02,
    ];
    for (answer, expect) in [(Some(0x5a), 0x5a), (None, 0x77)] {
        let mut m = Machine::new();
        m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
        m.attach_cart_from_bytes(&easyflash_crt(), "synthetic").expect("attach CRT");
        let log = log_with(|l| l.answer = answer);
        attach(&mut m, &log, &[]);
        run_at(&mut m, 0x0200, 0x37, prog, 7);
        let reads = cpu_reads(&log);
        assert!(
            reads.contains(&(0xdf1c, AccessKind::Cpu, Some(0x77))),
            "the device receives the cartridge's byte: {reads:?}"
        );
        assert_eq!(m.read_full(0x0400), expect, "device answer {answer:?} decides");
        assert!(m.cartridge.is_some());
    }
}

/// A one-bank EasyFlash CRT: header + one 8K CHIP at $8000.
fn easyflash_crt() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   ");
    v.extend_from_slice(&0x40u32.to_be_bytes());
    v.extend_from_slice(&0x0100u16.to_be_bytes());
    v.extend_from_slice(&32u16.to_be_bytes()); // EasyFlash
    v.push(1); // EXROM
    v.push(0); // GAME
    v.extend_from_slice(&[0u8; 6]);
    let mut name = [0u8; 32];
    name[..9].copy_from_slice(b"GATE 850 ");
    v.extend_from_slice(&name);
    v.extend_from_slice(b"CHIP");
    v.extend_from_slice(&(0x10u32 + 0x2000).to_be_bytes());
    v.extend_from_slice(&0u16.to_be_bytes()); // ROM
    v.extend_from_slice(&0u16.to_be_bytes()); // bank 0
    v.extend_from_slice(&0x8000u16.to_be_bytes());
    v.extend_from_slice(&0x2000u16.to_be_bytes());
    v.extend(std::iter::repeat(0xeau8).take(0x2000));
    v
}

// ── R2 ────────────────────────────────────────────────────────────────────────────

#[test]
fn r2_peeks_ask_the_device_and_never_read_it() {
    let log = log_with(|l| l.answer = Some(0x5a));
    let mut m = Machine::new();
    attach(&mut m, &log, &[]);
    for _ in 0..100 {
        assert_eq!(m.read_full(0xdf1e), 0x5a);
    }
    assert_eq!(m.peek_lens(0xdf1c, "io"), 0x5a);
    assert_eq!(m.peek_lens(0xdf1c, "cart"), 0x5a);
    assert!(log.lock().unwrap().reads.is_empty(), "a peek is not a read");
}

// ── R3 ────────────────────────────────────────────────────────────────────────────

#[test]
fn r3_a_device_stop_ends_the_run_before_the_next_instruction() {
    let log = log_with(|l| l.stop_on_write = Some(0xdf1c));
    let mut m = Machine::new();
    attach(&mut m, &log, &[]);
    let border = m.read_full(0xd020);
    // STA $DF1C / INC $D020 / INC $D020
    let stop = run_at(&mut m, 0xc000, 0x37, &[0x8d, 0x1c, 0xdf, 0xee, 0x20, 0xd0, 0xee, 0x20, 0xd0], 3);
    assert_eq!(stop, RunStop::Device);
    assert_eq!(m.c64_core.reg_pc, 0xc003, "PC is at the INC");
    assert_eq!(m.read_full(0xd020), border, "$D020 is unchanged");
}

// ── R4 ────────────────────────────────────────────────────────────────────────────

#[test]
fn r4_snooped_writes_are_reported_whatever_the_banking() {
    let log = log_with(|_| {});
    let mut m = Machine::new();
    attach(&mut m, &log, &[0xff00, 0xd036]);
    // LDA #$11 / STA $FF00 / INC $FF00 / STA $D036 / STA $D037
    let prog = [0xa9, 0x11, 0x8d, 0x00, 0xff, 0xee, 0x00, 0xff, 0x8d, 0x36, 0xd0, 0x8d, 0x37, 0xd0];
    run_at(&mut m, 0xc000, 0x37, &prog, 5);

    let snoops: Vec<_> = log.lock().unwrap().snoops.iter().map(|(a, v)| (a.addr, a.kind, *v)).collect();
    let ff00_rom: u8 = 0x00; // no KERNAL loaded: INC reads $00 under the banked-in ROM
    assert_eq!(
        snoops,
        vec![
            (0xff00, AccessKind::Cpu, 0x11),
            (0xff00, AccessKind::Dummy, ff00_rom),
            (0xff00, AccessKind::Cpu, ff00_rom.wrapping_add(1)),
            (0xd036, AccessKind::Cpu, 0x11),
        ],
        "STA once; INC twice, old value first; $D037 is not registered"
    );
    assert_eq!(m.peek_lens(0xff00, "ram"), ff00_rom.wrapping_add(1), "the write still lands in RAM");
    assert_eq!(m.io_shadow[0x036], 0x11, "the VIC write still happens");
    assert!(log.lock().unwrap().writes.is_empty(), "a snoop is not a port write");
}

// ── R5 ────────────────────────────────────────────────────────────────────────────

#[test]
fn r5_an_expansion_irq_is_taken_and_a_handler_read_drops_it_for_good() {
    // The device raises its line on a write to $DF1C (as the UCI does once a command is
    // answered) and drops it when the handler reads $DF1E.
    let log = log_with(|l| {
        l.raise_irq_on_write = Some(0xdf1c);
        l.drop_irq_on_read = Some(0xdf1e);
    });
    let mut m = Machine::new();
    attach(&mut m, &log, &[]);
    // $C000: SEI / LDA #$00 / STA $FFFE / LDA #$C1 / STA $FFFF / CLI / STA $DF1C / JMP $C00F
    let main = [
        0x78, 0xa9, 0x00, 0x8d, 0xfe, 0xff, 0xa9, 0xc1, 0x8d, 0xff, 0xff, 0x58, 0x8d, 0x1c, 0xdf, 0x4c, 0x0f,
        0xc0,
    ];
    // $C100: LDA $DF1E / INC $0400 / RTI
    m.poke(0xc100, &[0xad, 0x1e, 0xdf, 0xee, 0x00, 0x04, 0x40]);
    run_at(&mut m, 0xc000, 0x35, &main, 400); // $35: KERNAL out, vectors in RAM

    assert_eq!(m.read_full(0x0400), 1, "the handler ran exactly once");
    assert!((0xc00f..=0xc011).contains(&m.c64_core.reg_pc), "back in the main loop");
    assert_eq!(m.c64_int.pending_int[INT_SRC_EXPANSION] & IK_IRQ, 0, "the line is released");
}

#[test]
fn r5_the_expansion_nmi_and_restore_are_independent_sources() {
    let mut m = Machine::new();
    m.set_expansion_lines(false, true);
    // NOP
    run_at(&mut m, 0xc000, 0x35, &[0xea], 1);
    assert_ne!(m.c64_int.pending_int[INT_SRC_EXPANSION] & IK_NMI, 0, "expansion NMI pending");
    assert_eq!(m.c64_int.pending_int[INT_SRC_RESTORE], 0, "RESTORE untouched");
}

#[test]
fn r5_the_expansion_source_survives_a_c64re_dump_and_an_old_dump_restores() {
    let mut m = Machine::new();
    m.c64_int.pending_int[INT_SRC_EXPANSION] = IK_IRQ;
    let snap = trx64_core::c64re_snapshot::capture_int_status(&m);

    let mut back = Machine::new();
    trx64_core::c64re_snapshot::restore_int_status(&mut back, &snap);
    assert_eq!(back.c64_int.pending_int[INT_SRC_EXPANSION], IK_IRQ, "round trip");

    let mut old = snap.clone();
    old.int_names.truncate(4);
    old.pending_int.truncate(4);
    let mut from_old = Machine::new();
    from_old.c64_int.pending_int[INT_SRC_EXPANSION] = IK_NMI;
    trx64_core::c64re_snapshot::restore_int_status(&mut from_old, &old);
    assert_eq!(from_old.c64_int.pending_int[INT_SRC_EXPANSION], 0, "a pre-850 dump leaves the slot clear");
}

// ── R6 ────────────────────────────────────────────────────────────────────────────

/// PAL: 63 cycles × 312 lines.
const FRAME: u64 = 19_656;

fn start_cia1_timer_a(m: &mut Machine) {
    m.write_full(0xdc04, 0xff);
    m.write_full(0xdc05, 0xff);
    m.write_full(0xdc0e, 0x01);
}

fn timer_a(m: &Machine) -> u16 {
    u16::from(m.cia1.peek(0xdc04)) | (u16::from(m.cia1.peek(0xdc05)) << 8)
}

/// Run one held frame in 21-cycle slices, returning the raster lines seen. A 63-cycle
/// slice samples every line at one phase and can land on the cycle where line 0 still
/// reads 311.
fn held_frame(m: &mut Machine) -> std::collections::BTreeSet<u16> {
    let mut lines = std::collections::BTreeSet::new();
    for _ in 0..(FRAME / 21) {
        let stop = m.run_for_full_capped(21, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
        assert_eq!(stop, RunStop::CycleBudget);
        lines.insert(m.vic.raster_line as u16);
    }
    lines
}

#[test]
fn r6_a_cpu_hold_keeps_the_chips_running_and_the_cpu_still() {
    let mut m = Machine::new();
    m.poke(0xc000, &[0xea; 16]);
    m.c64_core.reg_pc = 0xc000;
    start_cia1_timer_a(&mut m);
    let (pc, a, x, y, sp) = (m.c64_core.reg_pc, m.c64_core.reg_a, m.c64_core.reg_x, m.c64_core.reg_y, m.c64_core.reg_sp);
    let t0 = timer_a(&m);
    let clk0 = m.c64_core.clk;

    m.set_hold(Some(Hold::Cpu));
    let lines = held_frame(&mut m);
    assert_eq!(lines.len(), 312, "$D012 passes through every line");
    assert_eq!(m.c64_core.clk - clk0, FRAME);
    assert_ne!(timer_a(&m), t0, "CIA1 timer A counts");
    assert_eq!(
        (m.c64_core.reg_pc, m.c64_core.reg_a, m.c64_core.reg_x, m.c64_core.reg_y, m.c64_core.reg_sp),
        (pc, a, x, y, sp),
        "the CPU registers are unchanged"
    );
    m.write_full(0x0400, 0x99);
    assert_eq!(m.read_full(0x0400), 0x99, "write_full lands while held");

    m.set_hold(None);
    m.run_for_full_capped(64, 3, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert_eq!(m.c64_core.reg_pc, pc + 3, "execution continues at the same PC");
}

#[test]
fn r6_a_reset_hold_runs_the_vic_alone() {
    let mut m = Machine::new();
    start_cia1_timer_a(&mut m);
    let t0 = timer_a(&m);
    m.set_hold(Some(Hold::Reset));
    let lines = held_frame(&mut m);
    assert_eq!(lines.len(), 312, "the VIC runs");
    assert_eq!(timer_a(&m), t0, "CIA1 timer A does not count");
}

#[test]
fn r6_a_device_hold_line_holds_the_cpu() {
    let log = log_with(|l| l.lines.hold = true);
    let mut m = Machine::new();
    attach(&mut m, &log, &[]);
    m.poke(0xc000, &[0xea; 16]);
    m.c64_core.reg_pc = 0xc000;
    assert_eq!(m.effective_hold(), Some(Hold::Cpu));
    m.run_for_full_capped(1000, 10, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert_eq!(m.c64_core.reg_pc, 0xc000, "held by the device's line");
    log.lock().unwrap().lines.hold = false;
    m.run_for_full_capped(64, 2, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert_eq!(m.c64_core.reg_pc, 0xc002, "released when the device drops it");
}

// ── Stretched reads ─────────────────────────────────────────────────────────────

#[test]
fn a_read_stalled_by_a_badline_reports_the_stall_and_how_much_of_it_was_on_the_bus() {
    let log = log_with(|_| {});
    let mut m = Machine::new();
    attach(&mut m, &log, &[]);
    m.write_full(0xd011, 0x1b); // display enabled: badlines steal cycles
    // $C000: LDA $DF1E / JMP $C000
    m.poke(0xc000, &[0xad, 0x1e, 0xdf, 0x4c, 0x00, 0xc0]);
    m.write_full(0x0001, 0x37);
    m.c64_core.reg_pc = 0xc000;
    m.run_for_full_capped(2 * FRAME, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});

    let l = log.lock().unwrap();
    let reads: Vec<&Access> = l.reads.iter().map(|(a, _)| a).filter(|a| a.addr == 0xdf1e).collect();
    let stalled: Vec<&&Access> = reads.iter().filter(|a| a.stalled > 0).collect();
    assert!(!stalled.is_empty(), "some reads landed on a badline ({} reads)", reads.len());
    for a in &stalled {
        assert!(a.stalled_on_bus <= 4, "AEC is high at most four stolen cycles: {a:?}");
        assert!(a.stalled_on_bus < a.stalled, "the VIC owned the rest: {a:?}");
    }
    assert!(
        reads.iter().filter(|a| a.stalled == 0).all(|a| a.stalled_on_bus == 0),
        "an unstalled read reports 0 for both"
    );
    eprintln!(
        "{} reads, {} stalled; stalls {:?}",
        reads.len(),
        stalled.len(),
        stalled.iter().map(|a| (a.stalled, a.stalled_on_bus)).collect::<Vec<_>>()
    );
}
