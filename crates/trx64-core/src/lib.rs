//! trx64-core — pure, deterministic C64 emulation.
//!
//! No I/O, no async, no socket, no trace format. The crown jewel, isolated and
//! testable against the VICE-derived TS port as Phase-1 spec
//! (C64ReverseEngineeringMCP/src/runtime/headless).
//!
//! Hot-path rule: the core takes a generic `O: Observer` (monomorphized, zero-cost
//! when unused). It NEVER calls back into another process per event.

use std::collections::HashSet;
use std::path::Path;

pub mod c64_6510core;
pub mod c64re_snapshot;
pub mod cart;
pub mod checkpoint_diff;
pub mod checkpoint_ring;
pub mod cia;
pub mod user_dir;
pub mod cpu;
pub mod cpu_history;
pub mod crash_triage;
pub mod delta_ring;
pub mod drive;
pub mod drive_6510core;
pub mod drive_snapshot;
pub mod expansion;
pub mod flash040;
pub mod full;
pub mod full_sc;
pub mod gcr;
pub mod iec;
pub mod keyboard;
pub mod m93c86;
pub mod model;
pub mod native_snapshot;
pub mod recorder;
pub mod rewind;
pub mod ring_dump;
pub mod rotation;
pub mod render;
pub mod resid_audio;
pub mod resid_ffi;
pub mod scenario_player;
pub mod sid;
/// Serial (SPI) flash — the device GMod3/GMod4 bit-bang over CS/CLK/DI/DO. Distinct from
/// `flash040`, which models the parallel-flash families ($AAA/$555 unlock).
pub mod spi_flash;
pub mod tables;
/// Spec 852 — the Ultimate Command Interface, the U64 profile's own device on the port.
pub mod georam;
pub mod reu;
pub mod uci;
pub mod vic;
pub mod vic_draw;
pub mod vic_inspect;
pub mod vic_line_trace;
pub mod viacore;
pub mod vice_snapshot_stream;
pub mod vsf;
pub mod vsf_export;

pub use cia::Cia;
pub use cpu::{Bus, Cpu6510};
pub use cpu_history::{CpuHistEntry, CpuHistoryRing};
pub use crash_triage::{
    Confidence, CrashPoint, StackSlot, TransferKind, TriageChain, WildTransfer,
};
pub use delta_ring::{CallerChain, DeltaEntry, DeltaRing, LoopOnset, WriteRec};
pub use drive::Drive1541;
pub use expansion::{
    Access, AccessKind, ExpansionChain, ExpansionDevice, ExpansionRam, Hold, OwnedRam, PortLines,
    SnoopSet,
};
pub use full::{Bank8, BankA, BankE, FullBus, MemConfig};
pub use iec::IecCore;
pub use resid_audio::{SidAudioEngine, SidWriteRecord, WavFormat};
pub use resid_ffi::{Resid, ResidConfig};
pub use sid::Sid6581;
pub use georam::{GeoRam, GeoRamStatus};
pub use reu::{DmaBus, Reu, ReuStatus};
pub use uci::{Uci, UciEvents, UciStatus};
pub use vic::VicII;

/// Zero-cost observation hook, inlined into the core step loop.
///
/// Three faces, one mechanism:
/// - [`NullSink`] — tracing off; the compiler eliminates the hooks entirely.
/// - `FrameSink` (in `trx64-trace`) — forensic firehose → `.c64retrace`.
/// BUG-042 — the per-access truth, handed to `on_access` at the moment of the access.
///
/// `on_access` used to take only `(kind, addr, value)`, so the observer registry had
/// nowhere to get a cycle, a PC or the registers from — and read them instead from a
/// snapshot refreshed ONCE PER RUN SEGMENT. Every event in a segment therefore carried
/// the same `cyc`/`pc`/`a`: ~130 consecutive hits stamped `cyc=24578787 pc=$093C a=$06`.
/// A 6502 `sta` costs at least three cycles, so that was impossible on its face.
///
/// The registers matter as much as the stamps, and less visibly: the same stale snapshot
/// is what register CONDITIONS (`if a==$06`) were evaluated against, so an observer could
/// fire on the wrong events and miss the right ones while its output looked normal.
///
/// These are the values AT the access: `pc`/`clk` as the bus records them, and the
/// registers as the executing core holds them at that instant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccessCtx {
    pub pc: u16,
    pub clk: u64,
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub sp: u8,
    pub p: u8,
}

/// - `ProbeSet` (Phase 2) — mutation-search verdicts, no firehose.
pub trait Observer {
    /// Fired once per retired instruction (= TS `onInstructionComplete`).
    /// `pc` = address of the instruction; `b1`/`b2` = raw operand bytes;
    /// `a/x/y/sp/p` = post-instruction registers; `clk` = post-instruction cycle.
    #[allow(clippy::too_many_arguments)]
    fn on_instruction(
        &mut self,
        pc: u16,
        opcode: u8,
        b1: u8,
        b2: u8,
        a: u8,
        x: u8,
        y: u8,
        sp: u8,
        p: u8,
        clk: u64,
    );
    /// Fired on every bus access. `pc` = live CPU reg_pc at the access; `clk` =
    /// CPU master clock at the access (= TS `BusEvent.cycle`). `old` = pre-write
    /// byte at `addr` for WRITE events (Spec 753 mutation surface), else 0.
    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, pc: u16, clk: u64, old: u8);
    fn on_interrupt(&mut self, vector: u16, clk: u64);
    /// Watchpoint-access hook. Fired ONLY when a per-address access-watch table is
    /// armed AND the watched address is hit on a real READ/WRITE (= the TS
    /// `this.accessWatch[addr] && this.onObservedAccess(...)` gate,
    /// cpu65xx-vice.ts:468/495). Returns `true` to request a halt; the run loop
    /// honors it at the NEXT instruction boundary (never re-enters the CPU
    /// mid-instruction). The POLICY (which addrs, conditions, actions) lives
    /// OUTSIDE the core; the default returns `false` (observe only, never halt) so
    /// existing observers compile unchanged and NullSink stays zero-cost.
    #[inline]
    fn on_access(&mut self, _kind: BusKind, _addr: u16, _value: u8, _cx: AccessCtx) -> bool {
        false
    }
    /// Fired when the VIC observes a register write that the TS `vic` trace
    /// channel would tag (raster/mode/irq). `clk` = master clock at the write,
    /// `raster_y` = VIC raster line at that cycle, `kind` = VIC_KIND_CODE
    /// (1=raster,2=mode,3=irq,4=badline), `value` = byte written.
    ///
    /// NOTE: the TS oracle's vic channel has NO live producer, so a parity sink
    /// MUST NOT emit these into the gate trace (the golden vic trace is empty).
    /// The hook exists for binary-format completeness + future integration; the
    /// default is a no-op and the daemon's domain filter never enables it.
    #[inline]
    fn on_vic_reg(&mut self, _clk: u64, _raster_y: u16, _kind: u8, _value: u8) {}
    /// Spec 859 — this observer records the VIC cycle by cycle. The full-machine bus then
    /// ticks the VIC through `tick_g::<true>`; every other observer gets the plain `tick`,
    /// with no recorder code in it. Compile-time, so the live path pays nothing.
    const RECORDS_VIC: bool = false;
}

/// A tee: forwards every [`Observer`] callback to BOTH sinks.
///
/// It lived in the daemon, where it let an armed (breakpoint / watchpoint) run also feed
/// the trace firehose — without it a trace captured nothing while any breakpoint was set.
/// Spec 864 moved it here, because it is a pure combinator over this crate's own trait and
/// because the monitor's second host needs exactly this to chain its own observer with the
/// one the monitor arms. `on_access` halts if EITHER sink asks.
pub struct TeeObserver<'a, A: Observer, B: Observer> {
    pub a: &'a mut A,
    pub b: &'a mut B,
}

impl<'a, A: Observer, B: Observer> TeeObserver<'a, A, B> {
    pub fn new(a: &'a mut A, b: &'a mut B) -> Self {
        Self { a, b }
    }
}

impl<A: Observer, B: Observer> Observer for TeeObserver<'_, A, B> {
    // Spec 859 — a tee records the VIC if either side does.
    const RECORDS_VIC: bool = A::RECORDS_VIC || B::RECORDS_VIC;
    #[allow(clippy::too_many_arguments)]
    fn on_instruction(&mut self, pc: u16, opcode: u8, b1: u8, b2: u8, a: u8, x: u8, y: u8, sp: u8, p: u8, clk: u64) {
        self.a.on_instruction(pc, opcode, b1, b2, a, x, y, sp, p, clk);
        self.b.on_instruction(pc, opcode, b1, b2, a, x, y, sp, p, clk);
    }
    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, pc: u16, clk: u64, old: u8) {
        self.a.on_bus(kind, addr, value, pc, clk, old);
        self.b.on_bus(kind, addr, value, pc, clk, old);
    }
    fn on_interrupt(&mut self, vector: u16, clk: u64) {
        self.a.on_interrupt(vector, clk);
        self.b.on_interrupt(vector, clk);
    }
    fn on_access(&mut self, kind: BusKind, addr: u16, value: u8, cx: AccessCtx) -> bool {
        // Evaluate BOTH (no short-circuit) so each sink sees every access; halt if either asks.
        let halt_a = self.a.on_access(kind, addr, value, cx);
        let halt_b = self.b.on_access(kind, addr, value, cx);
        halt_a || halt_b
    }
    fn on_vic_reg(&mut self, clk: u64, raster_y: u16, kind: u8, value: u8) {
        self.a.on_vic_reg(clk, raster_y, kind, value);
        self.b.on_vic_reg(clk, raster_y, kind, value);
    }
}


#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusKind {
    Fetch,
    Read,
    Write,
    DummyRead,
    DummyWrite,
}

/// Why a capped run loop stopped. Mirrors the TS `runFor` return's `aborted`
/// field (integrated-session.ts:962-995): `undefined` → `Completed`,
/// `"breakpoint"` → `Breakpoint(pc)`, `"cycle-budget"` → `CycleBudget`,
/// `"observer"` → `Observer`. `Completed` is the no-debug hot-path result (the
/// pre-existing `()`-returning entry points map to it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStop {
    /// Ran to the cycle budget or instruction cap without tripping a debug gate
    /// (= TS `aborted` absent). The historical end-of-run for all existing callers.
    Completed,
    /// An exec breakpoint at this PC fired at an instruction boundary BEFORE the
    /// instruction ran (= TS `aborted: "breakpoint"`; VICE break-on-exec). The PC
    /// is the breakpoint address; the CPU has NOT executed it.
    Breakpoint(u16),
    /// The cycle budget tripped at an instruction boundary (= TS
    /// `aborted: "cycle-budget"`). Distinguished from `Completed` only on the
    /// debug entry points; the plain entry points fold it into `Completed`.
    CycleBudget,
    /// An access-watch `on_access` returned `true` during the last instruction;
    /// honored at the NEXT boundary (= TS `aborted: "observer"`, post-access
    /// "at the trigger" state). The CPU is at the instruction boundary AFTER the
    /// watched access.
    Observer,
    /// Spec 850 D4 — a device on the expansion port asked, during the last instruction,
    /// for the run to end; honored at the NEXT boundary, like `Observer`.
    Device,
}

/// No-op observer. Hooks compile away to nothing when tracing is off.
pub struct NullSink;

impl Observer for NullSink {
    #[inline(always)]
    fn on_instruction(
        &mut self,
        _: u16,
        _: u8,
        _: u8,
        _: u8,
        _: u8,
        _: u8,
        _: u8,
        _: u8,
        _: u8,
        _: u64,
    ) {
    }
    #[inline(always)]
    fn on_bus(&mut self, _: BusKind, _: u16, _: u8, _: u16, _: u64, _: u8) {}
    #[inline(always)]
    fn on_interrupt(&mut self, _: u16, _: u64) {}
    #[inline(always)]
    fn on_access(&mut self, _: BusKind, _: u16, _: u8, _: AccessCtx) -> bool {
        false
    }
}

/// 6510 CPU registers.
#[derive(Clone, Debug, Default)]
pub struct Cpu {
    pub pc: u16,
    pub a: u8,
    pub x: u8,
    pub y: u8,
    /// Stack pointer (low byte; stack is at $0100-$01FF).
    pub sp: u8,
    /// Processor status flags.
    pub p: u8,
    /// Monotonic cycle counter for this CPU (mirrors Machine::clk).
    pub cycles: u64,
}

/// Flat 64K RAM bus borrowing a `[u8; 0x10000]` — the CPU-isolated bus for the
/// Phase-1 gate. No banking, no I/O port, no VIC/CIA. ROMs are copied into the
/// flat array verbatim, so reads from $A000-$BFFF / $E000-$FFFF return ROM bytes.
/// Deterministic by construction.
pub struct FlatRam<'a> {
    pub mem: &'a mut [u8; 0x10000],
}

impl<'a> Bus for FlatRam<'a> {
    #[inline]
    fn read(&mut self, addr: u16) -> u8 {
        self.mem[addr as usize]
    }
    #[inline]
    fn write(&mut self, addr: u16, value: u8) {
        self.mem[addr as usize] = value;
    }
}

/// VIC-isolated bus (ADR-012): routes $D000-$D3FF to the VIC-II register file
/// (the VIC mirrors every $40 bytes across the 1 KiB I/O block) and flat 64K RAM
/// everywhere else. No PLA banking, no CIA, no $00/$01 port — exactly the
/// chip-isolation gate the CPU-isolated exerciser (SEI; minimal loop + VIC
/// register writes) needs. The VIC itself is CLOCK-DRIVEN and ticked once per
/// CPU master cycle by the Machine run loop, NOT by bus accesses.
pub struct VicBus<'a> {
    pub mem: &'a mut [u8; 0x10000],
    pub vic: &'a mut crate::vic::VicII,
}

impl<'a> Bus for VicBus<'a> {
    #[inline]
    fn read(&mut self, addr: u16) -> u8 {
        if (0xd000..0xd400).contains(&addr) {
            // $D01E/$D01F (mirrored every $40): recompute the collision latches
            // from the frozen state (fire the collision IRQ on the 0→nonzero
            // edge) then read-clear. Other VIC registers: ordinary read.
            match (addr as u8) & 0x3f {
                0x1e | 0x1f => {
                    self.recompute_collisions();
                    self.vic.read_reg_mut(addr as u8)
                }
                _ => self.vic.read_reg(addr as u8),
            }
        } else {
            self.mem[addr as usize]
        }
    }
    #[inline]
    fn write(&mut self, addr: u16, value: u8) {
        if (0xd000..0xd400).contains(&addr) {
            self.vic.write_reg(addr as u8, value);
        } else {
            self.mem[addr as usize] = value;
        }
    }
    /// One VIC master cycle per CPU master cycle (= c64ViciiCycle hook). Latches
    /// BA-low into the VIC's ba_low_flag for the next read-stall. The VIC reads its
    /// per-cycle fetches through a flat-RAM `VicMemView` (no CHARGEN overlay on the
    /// chip-isolated bus; colour RAM = the $D800 slice of flat RAM; bank 0).
    #[inline]
    fn tick(&mut self) {
        let view = crate::vic::VicMemView {
            ram: self.mem,
            char_rom: None,
            color_ram: &self.mem[0xd800..0xdc00],
            vbank: 0,
            // The chip-isolated bus has no expansion port, so no ultimax and no cart.
            romh: None,
        };
        self.vic.tick(&view);
    }
    /// VICE check_ba(): stall the CPU read while BA is low (badline / sprite DMA),
    /// stealing cycles + advancing the VIC. Returns the stolen-cycle count.
    #[inline]
    fn check_ba_before_read(&mut self) -> u32 {
        let view = crate::vic::VicMemView {
            ram: self.mem,
            char_rom: None,
            color_ram: &self.mem[0xd800..0xdc00],
            vbank: 0,
            // The chip-isolated bus has no expansion port, so no ultimax and no cart.
            romh: None,
        };
        self.vic.steal_cycles(&view)
    }
}

impl<'a> VicBus<'a> {
    /// Recompute the $D01E/$D01F collision latches from the frozen state and merge
    /// them into the VIC (firing the collision IRQ on the 0→nonzero edge). On the
    /// chip-isolated bus everything (incl. colour RAM) is flat RAM; colour RAM low
    /// nibbles read from $D800-$DBFF, no CHARGEN shadow (char_rom zeroed), bank 0.
    fn recompute_collisions(&mut self) {
        let mut color_ram = [0u8; 0x0400];
        for (i, c) in color_ram.iter_mut().enumerate() {
            *c = self.mem[0xd800 + i] & 0x0f;
        }
        let char_rom = [0u8; 0x1000];
        let inp = render::RenderInput {
            regs: &self.vic.regs,
            ram: self.mem,
            char_rom: &char_rom,
            color_ram: &color_ram,
            bank_base: 0,
            model: self.vic.model(),
        };
        let (ss, sb) = render::render_collisions(&inp);
        self.vic.apply_collisions(ss, sb);
    }
}

/// CIA-isolated bus (ADR-012): routes $DC00-$DCFF (CIA1) + $DD00-$DDFF (CIA2) to
/// the two 6526 chips, and flat 64K RAM everywhere else. The CIAs are CLOCK-DRIVEN:
/// `clk` is advanced once per CPU master cycle by `Bus::tick` and used as the rclk
/// for every CIA register access (READ_OFFSET = write_offset = 0 on C64SC, so a
/// read/write at CPU cycle N runs the timer state machine forward to N). No PLA
/// banking, no VIC/SID, no $00/$01 port — exactly the chip-isolation gate the
/// CPU-isolated exerciser (SEI; program timers, count down, read $DCxx) needs.
pub struct CiaBus<'a> {
    pub mem: &'a mut [u8; 0x10000],
    pub cia1: &'a mut crate::cia::Cia,
    pub cia2: &'a mut crate::cia::Cia,
    pub table: &'a [u16; crate::cia::CIAT_TABLEN],
    /// Master clock shared with the CPU: equals the CPU's `self.clk` at each access
    /// because both advance one-per-cycle from the same start and `tick()` fires
    /// at the END of each CPU cycle (after the cycle's bus access).
    pub clk: u64,
}

impl<'a> Bus for CiaBus<'a> {
    #[inline]
    fn read(&mut self, addr: u16) -> u8 {
        if (0xdc00..0xdd00).contains(&addr) {
            self.cia1.read(addr, self.clk, self.table)
        } else if (0xdd00..0xde00).contains(&addr) {
            self.cia2.read(addr, self.clk, self.table)
        } else {
            self.mem[addr as usize]
        }
    }
    #[inline]
    fn write(&mut self, addr: u16, value: u8) {
        if (0xdc00..0xdd00).contains(&addr) {
            self.cia1.write(addr, value, self.clk, self.table);
        } else if (0xdd00..0xde00).contains(&addr) {
            self.cia2.write(addr, value, self.clk, self.table);
        } else {
            self.mem[addr as usize] = value;
        }
    }
    /// One CIA master cycle per CPU master cycle. The CIAs' own `clk` is the bus
    /// `clk`; both advance in lockstep with the CPU. We keep the per-chip prescaler
    /// (TOD) advancing but the timer state machines run lazily on access (warp
    /// counting), so this is O(1).
    #[inline]
    fn tick(&mut self) {
        self.clk = self.clk.wrapping_add(1);
        self.cia1.clk = self.clk;
        self.cia2.clk = self.clk;
        self.cia1.tick(self.table);
        self.cia2.tick(self.table);
    }
}

/// SID-isolated bus (chip-isolation gate, ADR-012): routes $D400-$D7FF to the
/// SID 6581 (32-byte register tile repeated every $20 bytes across the 1 KiB
/// block) and flat 64K RAM everywhere else. The SID is CLOCK-DRIVEN via the
/// `Bus::tick` hook: `tick` advances the SID state machine per master cycle.
/// Used by the CPU-isolated SID exerciser (SEI) that programs a voice (freq +
/// waveform + ADSR gate), runs N cycles, and reads $D41B/$D41C.
pub struct SidBus<'a> {
    pub mem: &'a mut [u8; 0x10000],
    pub sid: &'a mut crate::sid::Sid6581,
    pub sid_regs: &'a mut [u8; 32],
    /// Master clock (advanced by `tick`); passed to SID but not consumed here
    /// (SID is stateful enough via tick count).
    pub clk: u64,
}

impl<'a> Bus for SidBus<'a> {
    #[inline]
    fn read(&mut self, addr: u16) -> u8 {
        if (0xd400..0xd800).contains(&addr) {
            let reg = (addr as usize - 0xd400) & 0x1f;
            self.sid.read(reg, self.sid_regs)
        } else {
            self.mem[addr as usize]
        }
    }
    #[inline]
    fn write(&mut self, addr: u16, value: u8) {
        if (0xd400..0xd800).contains(&addr) {
            let reg = (addr as usize - 0xd400) & 0x1f;
            self.sid_regs[reg] = value;
            self.sid.write(reg, value, self.sid_regs);
        } else {
            self.mem[addr as usize] = value;
        }
    }
    /// One SID master cycle per CPU master cycle — batch-tick is done in the
    /// run loops by calling `sid.tick(instruction_cycles, &sid_regs)` at the
    /// instruction boundary (same pattern as the TS integrated-session.ts).
    /// The per-cycle `tick` hook is intentionally a no-op here: the SID model
    /// is advanced instruction-batch (matching the TS wall-clock tick), not
    /// cycle-by-cycle. This avoids O(N) inner-loop overhead in the hot path.
    #[inline]
    fn tick(&mut self) {
        self.clk = self.clk.wrapping_add(1);
    }
}

/// Full mutable machine state (~75 KiB headless).
///
/// `Clone` is intentional and load-bearing: a clone is the cheap COW fork base for
/// Phase-2 parallel mutation search (`explore()`), thousands of branches feasible.
#[derive(Clone)]
pub struct Machine {
    pub ram: Box<[u8; 0x10000]>,
    /// Monotonic cycle counter (CLOCK, never wraps — per Spec 743).
    pub clk: u64,
    /// Cycle-stepped 6510 (cpu.rs). The flat RAM above is its bus. Used by the
    /// CPU/chip-ISOLATED gates (run_for / run_for_cia / run_for_vic / run_for_sid)
    /// and the inject path — NOT the full machine, which runs on `c64_core`.
    pub cpu6510: Cpu6510,
    /// VERBATIM x64sc 6510 SC core (c64_6510core.rs). The PRODUCTION full-machine
    /// C64 CPU: `run_for_full` drives this (NOT `cpu6510`). Threads vic_cycle +
    /// check_ba + the interrupt-delay counters into every bus access — cycle-exact
    /// vs VICE where the pattern engine could not be.
    pub c64_core: c64_6510core::C64Core6510,
    /// Interrupt status the verbatim core dispatches against (per-source IRQ/NMI
    /// model). Sources: CIA1=0/VIC=1 → IRQ, CIA2=2/RESTORE=3 → NMI.
    pub c64_int: c64_6510core::IntStatus,
    /// Legacy register-snapshot view kept in sync for daemon readers.
    pub cpu: Cpu,
    /// Cycle-exact VIC-II. CLOCK-DRIVEN: ticked once per CPU master cycle by the
    /// VIC-isolated run path (`run_for_vic*`). Raster/badline/BA advance off the
    /// CPU clock regardless of CPU execution (ADR-012 isolation gate).
    pub vic: VicII,
    /// Cycle-exact CIA1 ($DC00-$DCFF). CLOCK-DRIVEN via the CIA-isolated run path
    /// (`run_for_cia*`); timers advance lazily to the CPU clk on register access.
    pub cia1: Cia,
    /// Cycle-exact CIA2 ($DD00-$DDFF).
    pub cia2: Cia,
    /// Shared CIA timer transition table (Arc → cheap to clone with the Machine).
    pub cia_table: cia::CiaTable,
    /// Drive position A (Spec 871): the machine's first 1541, on at unit 8 by
    /// default. The name is historical — since Spec 870 A can stand at unit 8-11, and
    /// since 871 it has a neighbour; `drive8` stays so every caller keeps working.
    /// The `drive8-cpu` trace domain and the head trace follow this position.
    pub drive8: Drive1541,
    /// Drive position B (Spec 871): a second complete 1541 on the same IEC bus, off
    /// by default with its jumpers at 9. Off it is neither clocked nor on the bus.
    /// Switch it on through [`Machine::set_drive_power`], which refuses a unit
    /// number the other position already answers to.
    pub drive_b: Drive1541,

    // ── Full-machine (FullBus) state (ADR-021) ──────────────────────────────
    /// BASIC ROM in a SEPARATE array (the RAM under $A000-$BFFF keeps its DRAM
    /// power-on fill, which the trace `old` byte + writes-through-ROM read).
    pub basic_rom: Box<[u8; 0x2000]>,
    /// KERNAL ROM, separate (RAM under $E000-$FFFF keeps its fill).
    pub kernal_rom: Box<[u8; 0x2000]>,
    /// CHARGEN ROM, separate (mapped into $D000-$DFFF when CHAREN low).
    pub char_rom: Box<[u8; 0x1000]>,
    /// I/O register shadow ($D000-$DFFF) — open-bus reads + color RAM low nibble.
    pub io_shadow: Box<[u8; 0x1000]>,
    /// SID register shadow ($D400-$D41F) — write store for parity reads and
    /// as the register file backing the `sid` voice state machine.
    pub sid_regs: [u8; 32],
    /// SID 6581 oscillator + envelope state machine (osc3/env3 computed reads).
    /// Ticked per instruction in `run_for_full` and `run_for_sid*` paths.
    /// The register file lives in `sid_regs`; this struct holds internal state.
    pub sid: Sid6581,
    /// Spec 855 D3 — SID chips 1.., each with its own register file and model.
    /// Chip 0 is `sid_regs` + `sid` above; this is empty on a stock machine.
    pub sid_extra: Vec<crate::sid::SidChip>,
    /// Spec 855 D2 — the host's decode table, set through `set_sid_map`. Empty
    /// means the pre-855 machine. A cold reset does NOT clear it (see
    /// `cold_reset`), because the firmware that owns it rewrites it on every
    /// reset and a self-clearing table would race that.
    pub sid_map: Vec<crate::sid::SidMapping>,
    /// Spec 855 D4 — the audio subscriber, one for the whole machine rather than
    /// one per engine. Dropped on clone, so a COW fork starts audio-silent.
    pub sid_trace: crate::sid::SidTrace,
    /// Spec 855 D5 — a host's read/peek overrides, so it can answer for a chip
    /// the core does not model (UE2's ARMSID configuration mode). Dropped on
    /// clone: a fork answers from the core.
    pub sid_host: crate::sid::SidHostAccess,
    /// CPU-port latches ($00 direction / $01 value). Power-on $2F / $37.
    pub port_dir: u8,
    pub port_data: u8,
    /// Live PLA memconfig (recomputed on $00/$01 writes).
    pub memconfig: MemConfig,
    /// Pre-built 32-entry memconfig table (no-cart C64).
    pub memconfig_table: [MemConfig; 32],
    /// Whether the full machine is using separate ROM arrays (FullBus assembled).
    /// When true, `boot_from_dir` loads ROMs into the separate arrays AND leaves
    /// the DRAM fill under the ROM windows; when false (legacy), ROMs are copied
    /// into `ram` for the isolated FlatRam/CiaBus/VicBus gates.
    pub full_assembled: bool,
    /// Last CIA2 port-A OUTPUT byte pushed to $DD00 (IEC / VIC bank). Persists
    /// across instructions so the FullBus only re-pushes on an actual change.
    /// Power-on: DDRA=0 → output=$FF.
    pub cia2_pa_out: u8,
    /// IEC serial-bus wired-AND core (C64 CIA2 PA ↔ 1541 VIA1 PB). Persists across
    /// instructions; borrowed into the FullBus each instruction (ADR-021 IEC wiring).
    pub iec: IecCore,
    /// Keyboard matrix (CIA1 PA column drive ↔ PB row read). Holds queued timed
    /// key presses from `session/type`; read by the FullBus on a $DC01 access.
    pub keyboard: crate::keyboard::KeyboardMatrix,
    /// Spec 310 / Sprint 93.1 — joystick port 1 (CIA1 PB bits 0-4, active-low) and
    /// port 2 (CIA1 PA bits 0-4, active-low). 1:1 with the c64re `joystick1` /
    /// `joystick2` (integrated-session.ts:441-442). The FullBus reads them on a
    /// $DC00 (port 2) / $DC01 (port 1) access and ANDs the active-low mask into the
    /// post-DDR latch value (= VICE `read_joyport_dig`). Power-on / reset = all
    /// released. `session/joystick_set|clear|release_keys` mutate these.
    pub joystick1: crate::keyboard::JoystickState,
    pub joystick2: crate::keyboard::JoystickState,
    /// Monotonic C64-clock reference the drives have been advanced up to — one for
    /// both positions, which are always caught up together (Spec 871). The
    /// push-flush catch-up advances the drive by `clk - drive_c64_ref` before
    /// sampling/applying the IEC lines on a $DD00 access (= VICE
    /// drive_cpu_execute_one/all at the exact C64 read/write instant).
    pub drive_c64_ref: u64,
    /// Attached cartridge mapper (= memory-bus.ts `cartridge`), or None for the
    /// stock no-cart machine. Borrowed `&mut` into the FullBus each instruction
    /// (a $DE00 IO write mutates the mapper's bank register, so the change
    /// persists in-place without an explicit write-back). Set by `attach_cart*`.
    pub cartridge: Option<Box<dyn crate::cart::CartMapper>>,
    /// The parsed CRT image backing `cartridge` (name / mapper-type / raw bytes
    /// for the attach record), or None.
    pub cartridge_image: Option<crate::cart::ParsedCartridgeImage>,

    /// Spec 850 — a host's device on the expansion port, or None. Beside `cartridge`, not
    /// instead of it: attaching one changes neither `cartridge`, the PLA index, BankInfo,
    /// the VSF export nor reset behaviour, and no reset ever calls it.
    pub expansion: crate::expansion::PortSlot,
    /// Spec 850 — the machine profile's own device on the port (Spec 852's UCI block on
    /// the `u64` profile), or None. Asked before the host device on every read.
    pub port_profile: crate::expansion::PortSlot,
    /// Spec 850 D5 — the union of both devices' snooped addresses; None when empty.
    pub expansion_snoop: Option<Box<crate::expansion::SnoopSet>>,
    /// Spec 850 D6 — the IRQ/NMI the host itself drives onto the port (a cartridge's
    /// lines, say), ORed with the devices' lines on `INT_SRC_EXPANSION`.
    pub expansion_host_lines: crate::expansion::PortLines,
    /// Spec 850 D7 — the host's hold on the 6510, or None. A device's `hold` line holds
    /// the CPU too; see [`Machine::effective_hold`].
    pub hold: Option<crate::expansion::Hold>,

    /// Spec 853 D7 — the last restore could not put the expansion RAM back.
    pub(crate) expansion_ram_uncovered: bool,
    /// Spec 852 D4 — a C64 reset happened that the UCI block has not been told about. No
    /// reset ever calls a device (850), so `cold_reset` only raises this and
    /// [`Machine::uci_mut`] hands it to the block, where `take_events` reports it.
    uci_c64_reset: bool,

    /// Always-on CPU-history ring (reverse-debug Phase 1a). Fed per retired
    /// instruction from `run_for_full_capped_dbg` (the single full-machine choke
    /// point) so the monitor `chis` verb has the last N executed instructions LIVE,
    /// with NO trace/finalize/sidecar dependency (VICE `cpuhistory`). Default-ON,
    /// env kill-switch `TRX64_CPUHISTORY=0`. NOT machine state: `Clone` yields an
    /// empty ring (live-timeline, not snapshot state), and it is never serialized.
    pub cpu_history: crate::cpu_history::CpuHistoryRing,

    /// Always-on FULL-DELTA undo ring (reverse-debug Phase 1b). Per retired
    /// instruction it records the CPU PRE-state (incl. the composite P) AND every
    /// memory/IO write `{addr, old_value, new_value}` — enough to UNDO the
    /// instruction (write `old_value`s back + restore the pre-state regs). Fed from
    /// the SAME `execute_one` retire point as `cpu_history`. Backs `reverse_step` /
    /// `who_wrote`. Always-on (the write capture is NOT gated on a trace), default-ON,
    /// shared kill-switch `TRX64_CPUHISTORY=0`, depth `TRX64_REVERSE_SECONDS` (10 s).
    /// Like `cpu_history`: live-timeline state, NOT machine state — `Clone` is empty,
    /// never serialized.
    pub delta_ring: crate::delta_ring::DeltaRing,
    /// Spec 856 D4 — the turbo fast path. With a turbo divider above 1, instructions that
    /// neither advance `clk` nor touch anything but RAM run back to back inside one bus,
    /// and the boundary sync runs once when one of them does. No effect at 1 MHz: there
    /// every instruction advances `clk`. Env kill-switch `TRX64_TURBO_FASTPATH=0`, read at
    /// `Machine::new`; the field can be flipped at any time.
    pub turbo_fast_path: bool,
    /// Spec 857 D4 — check CIA alarms by comparison, as VICE does, instead of catching both
    /// timers up to the clock in every instruction prologue and every cycle. Env kill-switch
    /// `TRX64_CIA_ALARM_CHECK=0`, read at `Machine::new`; the field can be flipped at any time.
    pub cia_alarm_check: bool,
    /// Spec 784 loader-lens — armed-on-command 1541 disk-mechanism head trace. OFF by
    /// default (does NOT run with the always-on CPU ring). When armed, a `(drv_clk,
    /// halftrack, sector)` sample is pushed whenever the sector under the head changes,
    /// sampled at each drive-PC step; the daemon drains + emits DRIVE_HEAD (0x34).
    pub head_trace_armed: bool,
    pub head_trace: Vec<(u64, u8, u8)>,
    head_trace_last: Option<(u8, u8)>,
    /// Spec 784 loader-lens READ-SET lane. Armed together with `head_trace`. On each
    /// sector-under-head change, if the drive latched GCR bytes off the sector it is
    /// LEAVING (`rotation.gcr_read_count` advanced since we entered it), a
    /// `(drv_clk, halftrack, sector, bytes)` tuple is pushed — the ground truth of
    /// which physical block the loader actually READ (not merely rotated past). The
    /// daemon drains it → BLOCK_READ (0x35). Distinct from `head_trace` (every
    /// transition) — this keeps ONLY the consumed sectors.
    pub block_reads: Vec<(u64, u8, u8, u16)>,
    /// `rotation.gcr_read_count` snapshot at the moment the head entered the CURRENT
    /// sector. `gcr_read_count - sector_entry_read_count` = bytes read off it so far.
    sector_entry_read_count: u64,
    /// Spec 785 C1 — armed-on-command CARTRIDGE read-set (the bank analogue of the
    /// 1541 `block_reads` lane). OFF by default and never fed by the always-on
    /// rings: while armed, the bus hands `cart_read_set` to `FullBus`, which counts
    /// every read the mapper SERVES out of a ROM window and attributes it to the
    /// bank live at that moment. The daemon drains it → CART_READ (0x36).
    pub cart_read_armed: bool,
    /// The accumulator itself. See [`crate::cart::CartReadSet`].
    pub cart_read_set: crate::cart::CartReadSet,

    /// Spec 863 — which C64 this is: a row of `models.toml`. It decides the VIC's cycle
    /// table and frame, the CPU clock the CIAs' TOD and the drive's catch-up are measured
    /// against, and the displayed window. Read it through [`Machine::timing`]; change it
    /// with [`Machine::switch_model`] (a live machine) or by building one with
    /// [`Machine::new_with_model`].
    model: &'static crate::model::C64Model,
}

/// reverse-debug Phase 1b — the CPU state the machine landed on after a reverse-step
/// (the PRE-state of the oldest instruction undone). `p` is the COMPOSITE status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReverseLanded {
    pub pc: u16,
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub sp: u8,
    pub p: u8,
    pub cycle: u64,
}

/// reverse-debug Phase 1b — the result of [`Machine::reverse_step`]: how many
/// instructions were actually undone, the landed CPU state, and every write rolled
/// back (newest instruction first, each instruction's writes in undo order).
#[derive(Clone, Debug, Default)]
pub struct ReverseStepOutcome {
    pub steps_taken: usize,
    pub landed: ReverseLanded,
    pub undone_writes: Vec<crate::delta_ring::WriteRec>,
}

/// reverse-debug Phase 1b — one [`Machine::who_wrote`] hit: the instruction PC + cycle
/// that wrote `addr`, and the `old→new` bytes. TRX64 feature-request #2 adds the
/// `caller_chain` — the top return-stack frames the writing instruction saw, so a write
/// done by a SHARED primitive can be attributed to its call site, not just the leaf PC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WhoWroteHit {
    pub pc: u16,
    pub cycle: u64,
    pub addr: u16,
    pub old_value: u8,
    pub new_value: u8,
    /// TRX64 feature-request #2 — the caller chain (top 1..3 return addresses, innermost
    /// first; `depth == 0` ⇒ none captured for this hit). Decode to symbols only if a
    /// symbol file is loaded (the daemon does that at render time).
    pub caller_chain: crate::delta_ring::CallerChain,
}

/// TRX64 feature-request #3 — ring-exhaustion as a typed, first-class signal. When a
/// reverse query (`triage`/`chis`/`whowrote`) bottoms out at the ring boundary this
/// distinguishes "the window was too short" (raise depth / break earlier) from "no data
/// / wrong address". Built by [`Machine::ring_exhaustion`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RingExhaustion {
    /// True when the delta ring has WRAPPED (evicted older history) AND the query found
    /// nothing in the retained window — so the answer may lie OLDER than the ring. False
    /// when the ring still has headroom (a miss is then genuinely "never happened in this
    /// run / wrong address", not a window-too-short case).
    pub ring_exhausted: bool,
    /// The current reverse-history depth in seconds (the `revdepth`), so the hint can
    /// suggest a concrete larger value.
    pub revdepth_seconds: usize,
    /// A ready-to-print remediation hint (empty when not exhausted).
    pub hint: String,
}

/// reverse-debug depth knob — the rebuilt always-on-ring sizing after
/// [`Machine::set_reverse_depth`] (or the current state via
/// [`Machine::reverse_depth_info`]). Backs `runtime/set_reverse_depth` / `revdepth`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReverseDepthInfo {
    /// Reverse-history depth in seconds (the requested/derived depth).
    pub seconds: usize,
    /// Delta-ring entry-slab capacity (retained instructions).
    pub delta_entry_capacity: usize,
    /// Delta-ring write-slab capacity (retained `{addr,old,new}` writes).
    pub delta_write_capacity: usize,
    /// CPU-history-ring capacity (retained instructions for `chis`).
    pub cpu_history_capacity: usize,
    /// Total RAM cost of BOTH rings at this depth, in bytes.
    pub ram_bytes: u64,
}

/// ROM load error.
#[derive(Debug)]
pub enum RomError {
    Io(std::io::Error),
    /// ROM file had unexpected size (got, expected).
    BadSize(usize, usize),
    /// Spec 870 D3 — a 1541 ROM that is neither 16 KiB nor 32 KiB (got).
    BadDriveRomSize(usize),
}

impl std::fmt::Display for RomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RomError::Io(e) => write!(f, "ROM I/O error: {e}"),
            RomError::BadSize(got, exp) => write!(f, "ROM size mismatch: got {got}, expected {exp}"),
            RomError::BadDriveRomSize(got) => write!(
                f,
                "1541 ROM size {got} bytes refused: give 16384 (at $C000) or 32768 ($8000-$FFFF)"
            ),
        }
    }
}

impl std::error::Error for RomError {}

impl From<std::io::Error> for RomError {
    fn from(e: std::io::Error) -> Self {
        RomError::Io(e)
    }
}

impl Machine {
    /// A powered-off machine of the default model (`c64-pal`).
    pub fn new() -> Self {
        Self::new_with_model(crate::model::default_model())
    }

    /// A powered-off machine of `model` — fresh chips built on the row before any cycle
    /// runs. Pass a row from [`crate::model::resolve`], which refuses one that cannot run.
    pub fn new_with_model(model: &'static crate::model::C64Model) -> Self {
        let t = model.timing;
        let mut drive8 = Drive1541::new();
        drive8.sync_factor = t.drive_sync_factor;
        let mut drive_b = Drive1541::new_position_b();
        drive_b.sync_factor = t.drive_sync_factor;
        Self {
            ram: Box::new([0u8; 0x10000]),
            clk: 0,
            cpu6510: Cpu6510::new(),
            c64_core: c64_6510core::C64Core6510::new(),
            c64_int: c64_6510core::IntStatus::new(),
            cpu: Cpu::default(),
            vic: VicII::new_for(model),
            cia1: Cia::new_timed(t.cpu_hz, t.tod_hz),
            cia2: Cia::new_timed(t.cpu_hz, t.tod_hz),
            cia_table: cia::new_table(),
            drive8,
            drive_b,
            basic_rom: Box::new([0u8; 0x2000]),
            kernal_rom: Box::new([0u8; 0x2000]),
            char_rom: Box::new([0u8; 0x1000]),
            io_shadow: Box::new([0u8; 0x1000]),
            sid_regs: [0u8; 32],
            sid: Sid6581::new(),
            sid_extra: Vec::new(),
            sid_map: Vec::new(),
            sid_trace: crate::sid::SidTrace::default(),
            sid_host: crate::sid::SidHostAccess::default(),
            port_dir: 0x2f,
            port_data: 0x37,
            memconfig: full::build_memconfig_table()[0x1f],
            memconfig_table: full::build_memconfig_table(),
            full_assembled: false,
            cia2_pa_out: 0xff,
            iec: IecCore::new(),
            keyboard: crate::keyboard::KeyboardMatrix::new(),
            joystick1: crate::keyboard::JoystickState::default(),
            joystick2: crate::keyboard::JoystickState::default(),
            drive_c64_ref: 0,
            cartridge: None,
            cartridge_image: None,
            expansion: crate::expansion::PortSlot::default(),
            port_profile: crate::expansion::PortSlot::default(),
            expansion_snoop: None,
            expansion_host_lines: crate::expansion::PortLines::default(),
            expansion_ram_uncovered: false,
            hold: None,
            uci_c64_reset: false,
            cpu_history: crate::cpu_history::CpuHistoryRing::new(),
            delta_ring: crate::delta_ring::DeltaRing::new(),
            turbo_fast_path: !matches!(
                std::env::var("TRX64_TURBO_FASTPATH").map(|v| v.trim().to_ascii_lowercase()).as_deref(),
                Ok("0") | Ok("off") | Ok("false") | Ok("no")
            ),
            cia_alarm_check: !matches!(
                std::env::var("TRX64_CIA_ALARM_CHECK").map(|v| v.trim().to_ascii_lowercase()).as_deref(),
                Ok("0") | Ok("off") | Ok("false") | Ok("no")
            ),
            head_trace_armed: false,
            head_trace: Vec::new(),
            head_trace_last: None,
            block_reads: Vec::new(),
            sector_entry_read_count: 0,
            cart_read_armed: false,
            cart_read_set: crate::cart::CartReadSet::default(),
            model,
        }
    }

    /// Spec 863 — the row this machine is.
    pub fn model(&self) -> &'static crate::model::C64Model {
        self.model
    }

    /// Spec 863 — how long a frame is and how fast the clock runs: the one answer.
    pub fn timing(&self) -> crate::model::Timing {
        self.model.timing
    }

    /// Spec 863 D5 — the transplant: put this RUNNING machine on another row, keeping its
    /// whole state. CPU, RAM, the CIAs' registers and timers, the SID's registers, the
    /// drive and every framebuffer are values and stay exactly what they are; what the row
    /// decides changes: the VIC's cycle table and frame (it carries on at the line it is on
    /// in the new geometry), the clock the TOD's mains tick and the drive's catch-up ratio
    /// are measured against. Nothing is power-cycled — the running program keeps the
    /// standard it detected at boot.
    ///
    /// The caller stands the machine at the frame boundary (line 0, the first cycles), the
    /// one position every geometry has. A position the new row does not have, or a row
    /// that cannot run here, is refused by name and nothing changes.
    pub fn switch_model(&mut self, model: &'static crate::model::C64Model) -> Result<(), String> {
        if let Some(why) = model.refusal() {
            return Err(why);
        }
        let (line, cycle) = (self.vic.raster_line, self.vic.raster_cycle);
        if cycle >= model.timing.cycles_per_line || line >= model.timing.lines_per_frame {
            return Err(format!(
                "the VIC stands at line {line}, cycle {} — {} has no such position (switch at the frame boundary)",
                cycle + 1,
                model.name
            ));
        }
        self.put_on_model(model)
    }

    /// Put the machine on a row without asking where the VIC stands — for a restore, which
    /// checks the SNAPSHOT's position (the one about to be loaded) instead of the live one.
    pub fn put_on_model(&mut self, model: &'static crate::model::C64Model) -> Result<(), String> {
        if let Some(why) = model.refusal() {
            return Err(why);
        }
        if std::ptr::eq(self.model, model) {
            return Ok(());
        }
        let t = model.timing;
        self.vic.set_model(model);
        // VICE `machine_change_timing` → `cia1_set_timing` / `cia2_set_timing` /
        // `drive_set_machine_parameter` (c64.c:1344-1358).
        self.cia1.set_timing(t.cpu_hz, t.tod_hz);
        self.cia2.set_timing(t.cpu_hz, t.tod_hz);
        self.drive8.sync_factor = t.drive_sync_factor;
        self.drive_b.sync_factor = t.drive_sync_factor;
        self.model = model;
        Ok(())
    }

    /// Mirror the live Cpu6510 register state into the legacy `cpu` snapshot +
    /// `clk` (the daemon reads from these). Call after any ISOLATED run.
    fn sync_snapshot(&mut self) {
        self.cpu.pc = self.cpu6510.reg_pc;
        self.cpu.a = self.cpu6510.reg_a;
        self.cpu.x = self.cpu6510.reg_x;
        self.cpu.y = self.cpu6510.reg_y;
        self.cpu.sp = self.cpu6510.reg_sp;
        self.cpu.p = self.cpu6510.flags();
        self.cpu.cycles = self.cpu6510.clk;
        self.clk = self.cpu6510.clk;
    }

    /// Mirror the live VERBATIM SC core (`c64_core`) register state into the legacy
    /// `cpu` snapshot + `clk`. Call after a FULL-machine run (run_for_full*). The
    /// `p` snapshot uses the composite `status()` (= LOCAL_STATUS, flag_n/flag_z
    /// folded in), matching the daemon's `flags()` semantics.
    fn sync_snapshot_sc(&mut self) {
        self.cpu.pc = self.c64_core.reg_pc;
        self.cpu.a = self.c64_core.reg_a;
        self.cpu.x = self.c64_core.reg_x;
        self.cpu.y = self.c64_core.reg_y;
        self.cpu.sp = self.c64_core.reg_sp;
        self.cpu.p = self.c64_core.status();
        self.cpu.cycles = self.c64_core.clk;
        self.clk = self.c64_core.clk;
        // Mirror the live SC-core registers into `cpu6510` too, so the daemon's
        // direct `cpu6510.reg_pc / reg_sp / flags()` reads (session/state, the
        // step/until loops, the monitor `r` dump) reflect the full-machine CPU
        // without the daemon needing to know which core ran. The full-machine path
        // never uses `cpu6510` for execution; this keeps its register view current.
        self.cpu6510.reg_pc = self.c64_core.reg_pc;
        self.cpu6510.reg_a = self.c64_core.reg_a;
        self.cpu6510.reg_x = self.c64_core.reg_x;
        self.cpu6510.reg_y = self.c64_core.reg_y;
        self.cpu6510.reg_sp = self.c64_core.reg_sp;
        // Decompose the composite status into cpu6510's reg_p + flag_n/flag_z
        // shadow so its `flags()` getter recomposes the same byte.
        let p = self.c64_core.status();
        self.cpu6510.reg_p = p & !0xa2; // clear P_SIGN(0x80) | P_ZERO(0x02) shadows
        self.cpu6510.flag_n = p & 0x80;
        self.cpu6510.flag_z = if p & 0x02 != 0 { 0 } else { 1 };
        self.cpu6510.clk = self.c64_core.clk;
    }

    /// Load 8 KiB KERNAL ROM into $E000-$FFFF (flat RAM for iso buses) AND the
    /// separate `kernal_rom` array (for FullBus banked reads).
    pub fn load_kernal(&mut self, path: &Path) -> Result<(), RomError> {
        let data = std::fs::read(path)?;
        if data.len() != 0x2000 {
            return Err(RomError::BadSize(data.len(), 0x2000));
        }
        self.ram[0xE000..=0xFFFF].copy_from_slice(&data);
        self.kernal_rom.copy_from_slice(&data);
        Ok(())
    }

    /// Load 8 KiB BASIC ROM into $A000-$BFFF (flat RAM) AND `basic_rom`.
    pub fn load_basic(&mut self, path: &Path) -> Result<(), RomError> {
        let data = std::fs::read(path)?;
        if data.len() != 0x2000 {
            return Err(RomError::BadSize(data.len(), 0x2000));
        }
        self.ram[0xA000..=0xBFFF].copy_from_slice(&data);
        self.basic_rom.copy_from_slice(&data);
        Ok(())
    }

    /// Load 4 KiB CHARGEN ROM into the separate `char_rom` array (mapped into
    /// $D000-$DFFF by the FullBus when CHAREN is low).
    pub fn load_chargen(&mut self, path: &Path) -> Result<(), RomError> {
        let data = std::fs::read(path)?;
        if data.len() != 0x1000 {
            return Err(RomError::BadSize(data.len(), 0x1000));
        }
        self.char_rom.copy_from_slice(&data);
        Ok(())
    }

    /// Cold reset: read the reset vector from $FFFC/$FFFD (KERNAL must be loaded)
    /// and set PC. All other registers set to power-on defaults.
    /// Apply the VICE power-on DRAM fill pattern (= memory-bus.ts reset +
    /// applyRamFillPattern, `value_invert=64`). Empirically verified against the
    /// live runtime's trace oldValue: 64-byte alternating blocks —
    /// $00xx-$003F = $00, $0040-$007F = $FF, $0080-$00BF = $00, $00C0-$00FF =
    /// $FF, ... i.e. `(addr & 0x40) ? 0xFF : 0x00`. This is the oldValue/read
    /// source for the trace, so it must be byte-exact. ROM regions are
    /// overwritten by the ROM loads afterward.
    pub fn fill_power_on_ram(&mut self) {
        for addr in 0..0x10000usize {
            self.ram[addr] = if addr & 0x40 != 0 { 0xFF } else { 0x00 };
        }
    }

    /// The live PLA memconfig-table index from the CPU-port latches + the attached
    /// cartridge's EXROM/GAME lines (= memory-bus.ts memPlaConfigChanged index,
    /// ts:855-869). No cart ⇒ EXROM=GAME=1 ⇒ (port | 0x18), byte-identical to the
    /// prior hard-coded no-cart index. Used by `cold_reset` (the FullBus has its
    /// own copy of this in `pla_config_changed`) and by the monitor `swapcrt` verb to
    /// re-apply the banking lines after a same-mapper `set_state` carry-over (= the TS
    /// attachCartridge memconfig recompute, monitor-shell.ts:361).
    pub fn pla_index(&self) -> usize {
        let port = ((!self.port_dir | self.port_data) & 0x07) as usize;
        let loram = port & 0x01;
        let hiram = (port >> 1) & 0x01;
        let charen = (port >> 2) & 0x01;
        let (exrom, game) = match self.cartridge.as_ref() {
            Some(c) => {
                let l = c.get_lines();
                ((l.exrom & 1) as usize, (l.game & 1) as usize)
            }
            None => (1, 1),
        };
        (loram | (hiram << 1) | (charen << 2) | (exrom << 3) | (game << 4)) & 0x1f
    }

    /// Read a reset-vector byte ($FFFC/$FFFD) through the live banked map so an
    /// ultimax cart (which maps its ROMH over $E000-$FFFF) re-vectors the boot from
    /// its own ROM. Mirrors the FullBus $E000-$FFFF read window: KERNAL when the
    /// config maps it, else the cart's ultimax ROMH, else RAM. No cart / non-ultimax
    /// ⇒ exactly the KERNAL/RAM byte the prior `self.ram[0xFFFC]` read returned
    /// (the KERNAL ROM is mirrored into `ram` $E000-$FFFF by load_kernal).
    fn banked_reset_vector_byte(&self, addr: u16) -> u8 {
        if self.memconfig.kernal {
            return self.kernal_rom[(addr as usize) - 0xe000];
        }
        if matches!(self.memconfig.bank_e, full::BankE::CartHiUltimax) {
            if let Some(cart) = self.cartridge.as_ref() {
                let bi = cart::BankInfo {
                    cpu_port_direction: self.port_dir,
                    cpu_port_value: self.port_data,
                    basic_visible: self.memconfig.basic,
                    kernal_visible: self.memconfig.kernal,
                    io_visible: self.memconfig.io,
                    char_visible: self.memconfig.char_rom,
                    cartridge_attached: true,
                    cartridge_exrom: Some(cart.get_lines().exrom),
                    cartridge_game: Some(cart.get_lines().game),
                    phi1: 0xff,
                };
                // peek (side-effect-free): a reset-vector fetch must NOT advance
                // the flash command FSM (the flash is in READ state at reset, so
                // peek returns the same raw ROMH byte the bus read would).
                if let Some(v) = cart.peek(addr, &bi) {
                    return v;
                }
            }
            return 0xff; // open bus
        }
        self.ram[addr as usize]
    }

    /// Side-effect-free cartridge peek at `addr` through the live banking lines (=
    /// the FullBus cart_read consulted by a real CPU read, but using the mapper's
    /// `peek` so no flash/EEPROM FSM advances). Returns the CHIP byte the mapper maps
    /// at `addr` for the CURRENT bank/mode, or None when no cart / the mapper does not
    /// claim the address. Used by `read_full` so the monitor `m cpu/cart` lens
    /// reflects cartridge-mapped ROM (e.g. EasyFlash ROML at $8000 for the selected
    /// bank), not stale RAM. (Audit ws-cart-live-mapping — Spec 713 §7.1: mapped bytes
    /// read the live CHIP image, not open bus / RAM.)
    fn cart_peek_byte(&self, addr: u16) -> Option<u8> {
        let cart = self.cartridge.as_ref()?;
        let lines = cart.get_lines();
        let bi = cart::BankInfo {
            cpu_port_direction: self.port_dir,
            cpu_port_value: self.port_data,
            basic_visible: self.memconfig.basic,
            kernal_visible: self.memconfig.kernal,
            io_visible: self.memconfig.io,
            char_visible: self.memconfig.char_rom,
            cartridge_attached: true,
            cartridge_exrom: Some(lines.exrom),
            cartridge_game: Some(lines.game),
            phi1: 0xff,
        };
        cart.peek(addr, &bi)
    }

    /// Attach a cartridge from raw `.crt` bytes (= memory-bus.ts attachCartridge,
    /// ts:258-275 + loadCartridgeMapperFromBytes ts:114-118). Parses the CRT, builds
    /// the read-only mapper, stores it on the Machine, then re-runs the PLA reconfig
    /// so the banking picks up the cart's EXROM/GAME lines. Call BEFORE `cold_reset`
    /// (or call `cold_reset` after) so the reset vector fetches through the cart.
    /// Returns the parsed image's display name + mapper type on success.
    pub fn attach_cart_from_bytes(
        &mut self,
        bytes: &[u8],
        name: &str,
    ) -> Result<(String, cart::MapperType), cart::CrtError> {
        // Spec 790 — thin wrapper over the smart-attach door with `Auto` intent, so
        // existing callers keep their `.crt`-header-driven behaviour unchanged.
        self.attach_cart_typed(bytes, name, cart::CartType::Auto)
    }

    /// Spec 790 §790.3 — smart cartridge attach (one door, VICE
    /// `cartridge_attach_image` model). Dispatch on the bytes:
    /// - `C64 CARTRIDGE   ` signature ⇒ `parse_crt`; `Forced(t)` overrides the
    ///   header hw type, `Auto` is header-driven.
    /// - else a raw `.bin` ⇒ `Forced(t)` splits per that type's geometry
    ///   (`parse_bin`); `Auto` runs the S1 structural-only detect, which may
    ///   return `BinTypeAmbiguous` (caller must then pass an explicit `--cart-type`;
    ///   the ambiguous-case resolver is the Spec 790 S2 runtime harness).
    pub fn attach_cart_typed(
        &mut self,
        bytes: &[u8],
        name: &str,
        ty: cart::CartType,
    ) -> Result<(String, cart::MapperType), cart::CrtError> {
        let (image, mapper) = if cart::is_crt(bytes) {
            let override_ty = match ty {
                cart::CartType::Forced(t) => Some(t),
                cart::CartType::Auto => None,
            };
            cart::load_cartridge_from_bytes(bytes, name, override_ty)?
        } else {
            match ty {
                cart::CartType::Forced(t) => cart::load_cartridge_from_bin(bytes, name, t)?,
                // Spec 790 S2 — `Auto` raw `.bin`: settle the structural cases (eapi
                // / CBM80 / ultimax) directly; otherwise, instead of erroring
                // `BinTypeAmbiguous`, attach the runtime self-configuring harness,
                // which boots the image and locks the concrete flash family in-place
                // on the first type-specific register access it observes.
                cart::CartType::Auto => match cart::detect_bin_type(bytes) {
                    Ok(t) => cart::load_cartridge_from_bin(bytes, name, t)?,
                    Err(cart::CrtError::BinTypeAmbiguous) => {
                        cart::load_self_config_from_bin(bytes, name)?
                    }
                    Err(e) => return Err(e),
                },
            }
        };
        let result = (image.name.clone(), image.mapper_type);
        self.cartridge = Some(mapper);
        self.cartridge_image = Some(image);
        // ts:274 — re-run the PLA reconfig on attach so the table-driven dispatch
        // picks up the new EXROM/GAME lines.
        self.memconfig = self.memconfig_table[self.pla_index()];
        Ok(result)
    }

    /// Detach the cartridge (releases EXROM/GAME → no-cart banking).
    pub fn detach_cart(&mut self) {
        self.cartridge = None;
        self.cartridge_image = None;
        self.memconfig = self.memconfig_table[self.pla_index()];
    }

    pub fn cold_reset(&mut self) {
        self.arm_u64_reset_hold();
        // CPU-port power-on latches must be set BEFORE the memconfig/vector compute
        // so the banking is the boot config (set again below for clarity/order with
        // the rest of the reset, but needed here for the cart-aware memconfig).
        self.port_dir = 0x2f;
        self.port_data = 0x37;
        // Expansion-port RESET line → cartridge reset (= memory-bus.ts reset()
        // ts:150, BEFORE the PLA recompute + the $FFFC fetch): the cart's bank +
        // mode/lines return to boot config so an ultimax cart re-vectors $FFFC from
        // its own ROMH (the machine reboots INTO the cart, like real hardware).
        if let Some(cart) = self.cartridge.as_mut() {
            cart.reset();
        }
        // The port's /RESET line reaches whatever is out there — each device decides what
        // that means for it (`ExpansionDevice::reset`, default nothing). An REU takes it;
        // the UCI block does not, which is why 852 D4 still holds below.
        if let Some(dev) = self.port_profile.0.as_deref_mut() {
            dev.reset();
        }
        if let Some(dev) = self.expansion.0.as_deref_mut() {
            dev.reset();
        }
        // Spec 852 D4 — the UCI block survives a C64 reset (only the FPGA reset clears it,
        // `command_protocol.vhd:292-306`), but the firmware must hear about it. The block
        // itself declines the reset above; `uci_mut` hands it the news.
        self.uci_c64_reset = true;
        // Recompute the live memconfig from the port latches + cart EXROM/GAME
        // lines (= memPlaConfigChanged, ts:854-871). No cart ⇒ idx (port|0x18),
        // byte-identical to the prior hard-coded no-cart index.
        self.memconfig = self.memconfig_table[self.pla_index()];
        // Read the reset vector THROUGH the banked map: an ultimax cart maps its
        // ROMH over $E000-$FFFF, so $FFFC/$FFFD come from the cart, not RAM.
        let lo = self.banked_reset_vector_byte(0xFFFC) as u16;
        let hi = self.banked_reset_vector_byte(0xFFFD) as u16;
        let pc = lo | (hi << 8);
        self.cpu6510.reset_to(pc);
        // Reset the verbatim SC core to the SAME power-on state (full-machine path).
        // The reset-vector read is performed here (not by an IK_RESET dispatch), so
        // it is untraced — matching the boot golden, whose first record is the
        // KERNAL reset entry (LDX #$FF @ $FCE2), not a vector fetch.
        self.c64_core.reset_to(pc);
        self.c64_int = c64_6510core::IntStatus::new();
        // ADR-011 RESOLVED (integration): the C64/VICE 6510 power-on leaves
        // P = $20 (P_UNUSED only) — the I flag is NOT set by reset. The KERNAL
        // reset routine's own `SEI` at $FCE4 sets I. The full-boot trace[0]
        // (LDX #$FF @ $FCE2) records reg_p = $20; forcing I here produced $24.
        // (The earlier `reg_p |= 0x04` was a CPU-isolated convenience — but the
        // CPU-isolated gates inject PC via `set_pc`, never `cold_reset`, so they
        // are unaffected by dropping it.)
        // CPU-port power-on latches ($00=$2F DDR, $01=$37 port — boot config 31
        // BASIC+IO+KERNAL) and the cart-aware memconfig were already set at the top
        // of cold_reset (before the cart reset + the banked $FFFC vector fetch). The
        // RAM[0]/[1] mirror is written by `prepare_full_boot` (full-machine path) so
        // the CPU/chip-ISOLATED gates keep zero-page $00/$01 at the power-on DRAM
        // fill (their exercisers were recorded against that).
        // IEC bus: power-on released (= installCia2 seeds iecWrite(0xff, 0x3f)).
        self.iec = IecCore::new();
        self.keyboard.clear();
        // integrated-session.ts:736-743 resetCold wipes joystick state too.
        self.joystick1 = crate::keyboard::JoystickState::default();
        self.joystick2 = crate::keyboard::JoystickState::default();
        self.cia2_pa_out = 0xff;
        // Spec 870 — the fresh IEC core knows a drive at unit 8; tell it the ones that
        // are there (another unit, a second drive, or none while off / held). A no-op
        // on a stock machine.
        self.sync_drive_slots();
        self.drive_c64_ref = 0;
        // SID: reset register file + voice state to power-on defaults.
        self.sid_regs = [0u8; 32];
        self.sid.reset();
        // Spec 855 D3 — every other chip takes the same reset. The DECODE TABLE
        // deliberately does not: on a U64 the firmware rewrites it in its own
        // reset task, and a table that cleared itself here would race that and
        // leave the C64 addressing chips that had just vanished.
        for chip in self.sid_extra.iter_mut() {
            chip.reset();
        }
        // reverse-debug Phase 1a — a cold reset is a timeline boundary: drop the
        // CPU-history ring so `chis` never presents pre-reset instructions as
        // continuous with the fresh boot (report the boundary, don't fake it). The
        // slab is retained (clear() only resets the head).
        self.cpu_history.clear();
        // reverse-debug Phase 1b — same timeline boundary: drop the full-delta undo
        // ring so a reverse-step / who_wrote never crosses the cold-reset boundary
        // (the spec's "report the boundary, don't fake it"). Slabs retained.
        self.delta_ring.clear();
        self.sync_snapshot();
    }

    /// HW RESET line (Reset button / SuperReset / SYS 64738) = warm reset.
    /// Port of integrated-session.ts `resetWarm` (= `resetCold({ keepRam: true })`,
    /// ts:776-778 → ts:690-764): re-init the CPU + C64 I/O chips + drive, restore
    /// default $00/$01 banking + PLA, and re-enter the KERNAL reset routine via the
    /// $FFFC vector (= $FCE2). KEEPS user RAM — unlike a power-cycle, which fills
    /// the cold-boot DRAM pattern (`fill_power_on_ram`). Recovers from a running or
    /// JAMmed game: `cold_reset`'s CPU `reset_to` clears the jammed flag + pending
    /// IRQ/NMI, and the chip resets below clear an active raster-IRQ / CIA timers
    /// that would otherwise re-hijack execution (ts:701, ts:719-726). The 1541 disk
    /// stays mounted; the drive re-runs its ROM (ts:707-708).
    ///
    /// TRX64's `cold_reset` already preserves RAM (the DRAM fill lives in
    /// `boot_from_dir`/`fill_power_on_ram`, not in `cold_reset`), so the warm path =
    /// `cold_reset`'s banking/CPU/IEC/keyboard/SID/cart re-init PLUS the CIA + VIC +
    /// drive resets the TS `resetCold` performs (ts:707-708, ts:719-726). RAM is
    /// untouched throughout.
    pub fn warm_reset(&mut self) {
        // Banking restore ($00=$2F/$01=$37 + PLA) + cart reset + $FFFC vector fetch
        // + CPU/IEC/keyboard/SID re-init, RAM preserved (cold_reset does NOT fill).
        // = ts:692-694 (resetCpuPortKeepRam) + ts:699/701/730/719 path.
        self.cold_reset();
        // ts:724-726 — cold-reset the C64 I/O chips so a 2nd+ reset does not leave
        // CIA timers / IRQ state or an active VIC raster-IRQ from the previous run
        // (= the "no cursor / re-hijack after reset" recovery). Fresh power-on chips.
        // Spec 863 — fresh chips of the SAME model: the standard is identity, like the
        // profile below, and a reset does not change the crystal.
        let t = self.model.timing;
        self.cia1 = Cia::new_timed(t.cpu_hz, t.tod_hz);
        self.cia2 = Cia::new_timed(t.cpu_hz, t.tod_hz);
        self.cia1.clk = self.clk;
        self.cia2.clk = self.clk;
        // Spec 815 — which machine this claims to be is IDENTITY, not chip state.
        // Pressing RESET on a C128 does not turn it into a C64, and a release that
        // re-probes after its own reset must get the same answer.
        let profile = self.vic.speed_profile;
        // Spec 851 — the Ultimate's turbo settings are the firmware's, not the C64's; the
        // reset clears only the C64-side `$D030`/`$D031`.
        let (regs_en, prefer, table) = (self.vic.u64_regs_en, self.vic.u64_speed_prefer, self.vic.u64_speed_table);
        self.vic = VicII::new_for(self.model);
        self.vic.speed_profile = profile;
        self.vic.u64_regs_en = regs_en;
        self.vic.u64_speed_prefer = prefer;
        self.vic.u64_speed_table = table;
        // The fresh VIC has no hold; the reset this IS has to arm it (BUG-061).
        self.arm_u64_reset_hold();
        // ts:707-708 + ts:773 — the C64's RESET reaches the 1541 over the IEC RESET
        // line (Spec 870 D2): with the line connected — the default — the drive runs
        // its own reset sequence (flush a pending write, reset the electronics, keep
        // the disk); cut, the drive carries on where it was. A drive that is off
        // ignores it.
        self.drive8.reset_from_c64();
        // Spec 871 — the RESET line runs to every device on the bus.
        self.drive_b.reset_from_c64();
        self.sync_drive_slots();
        self.sync_snapshot();
    }

    /// Inject raw bytes into RAM at `addr` (no banking). The CPU-isolated
    /// inject+run primitive: write an exerciser program, set PC, run N cycles.
    pub fn poke(&mut self, addr: u16, bytes: &[u8]) {
        for (i, b) in bytes.iter().enumerate() {
            let a = addr.wrapping_add(i as u16) as usize;
            self.ram[a] = *b;
        }
    }

    /// Poke through the I/O space (= `wr io` lens). Routes each byte the way a
    /// CPU store with I/O mapped would: $D000-$D3FF → VIC registers, $D400-$D7FF
    /// → SID register file, $D800-$DBFF → colour-RAM nibble (the I/O shadow),
    /// $DC00-$DCFF / $DD00-$DDFF → CIA1 / CIA2, everything else in $D000-$DFFF →
    /// the I/O shadow. This lets a render scenario program the VIC + colour RAM on
    /// the CPU-isolated (flat-bus) inject path, where ordinary `STA $D0xx` would
    /// land in RAM instead of the chip. Out-of-range addresses fall back to RAM.
    /// Spec 855 D5 — install (or clear) a host's answer for SID reads.
    ///
    /// `read` sees a real bus read and may advance the host's protocol state;
    /// `peek` must not, and its `Fn` bound is the type system saying so. `Some`
    /// wins, `None` falls through to the core — 850's precedence, unchanged.
    ///
    /// Install both or the monitor will disagree with the C64: without the peek
    /// half a debugger prints the core's register shadow while the program reads
    /// the host's answer.
    pub fn set_sid_host_access(
        &mut self,
        read: Option<Box<dyn FnMut(u8, usize) -> Option<u8> + Send>>,
        peek: Option<Box<dyn Fn(u8, usize) -> Option<u8> + Send + Sync>>,
    ) {
        self.sid_host = crate::sid::SidHostAccess { read, peek };
    }

    /// Spec 855 D4 — install (or clear) the audio subscriber: every SID register
    /// write in CPU order, as `(chip, reg, value, clk)`.
    ///
    /// One hook for the machine, not one per engine. A subscriber needs to know
    /// which chip wrote in order to clock the right instance, and per-engine
    /// hooks would have to be re-installed every time the firmware remaps. The
    /// raw address is not passed: the host built the decode table, so chip plus
    /// register gives it back, and passing both invites them to disagree.
    pub fn set_sid_write_trace(
        &mut self,
        hook: Option<Box<dyn FnMut(u8, u8, u8, u64) + Send>>,
    ) {
        self.sid_trace = crate::sid::SidTrace(hook);
    }

    // ── Spec 855 D2 — the host's SID decode table ───────────────────────────────
    //
    // The host resolves its own hardware and hands the result over; this crate
    // never learns what a U64 register is. Nothing here bakes in VICE's
    // `Sid2..8AddressStart` defaults: an empty table is the pre-855 machine.

    /// Install the decode table, growing the chip list to cover it.
    ///
    /// Callable at any time — on a U64 the firmware rewrites the mapping in its
    /// reset task, so this is not a construction-time decision. Chips are only
    /// ever ADDED here: shrinking on a remap would throw away a chip's state
    /// because the host happened to move an address.
    pub fn set_sid_map(&mut self, map: Vec<crate::sid::SidMapping>) {
        let needed = map.iter().map(|m| m.chip as usize).max().unwrap_or(0);
        while self.sid_extra.len() < needed {
            self.sid_extra.push(crate::sid::SidChip::new());
        }
        self.sid_map = map;
    }

    /// The table as it stands.
    pub fn sid_map(&self) -> &[crate::sid::SidMapping] {
        &self.sid_map
    }

    /// How many SIDs this machine has, chip 0 included. Always at least 1.
    pub fn sid_chip_count(&self) -> usize {
        1 + self.sid_extra.len()
    }

    /// Read a chip's register shadow without side effects — the monitor's view.
    /// `None` for a chip that does not exist.
    pub fn sid_chip_regs(&self, chip: u8) -> Option<&[u8; 32]> {
        if chip == 0 {
            return Some(&self.sid_regs);
        }
        self.sid_extra.get(chip as usize - 1).map(|c| &c.regs)
    }

    /// Spec 855 D6 — a voice's envelope level, 0..255, for a host that drives an
    /// LED strip or any other cosmetic readout. This is the fastsid model's
    /// value, the one that already answers `$D41C`; reSID keeps its own and the
    /// two will not agree to the last bit.
    pub fn sid_envelope(&self, chip: u8, voice: usize) -> Option<u8> {
        if voice >= 3 {
            return None;
        }
        if chip == 0 {
            return Some(self.sid.voices[voice].adsr_value);
        }
        self.sid_extra.get(chip as usize - 1).map(|c| c.engine.voices[voice].adsr_value)
    }

    pub fn poke_io(&mut self, addr: u16, bytes: &[u8]) {
        for (i, b) in bytes.iter().enumerate() {
            let a = addr.wrapping_add(i as u16);
            match a {
                0xd000..=0xd3ff => self.vic.write_reg(a as u8, *b),
                0xd400..=0xd7ff => {
                    let (chip, reg) = match crate::sid::resolve_sid(&self.sid_map, a) {
                        Some(hit) => hit,
                        None => (0, (a as usize - 0xd400) & 0x1f),
                    };
                    // Spec 855 D4 — a host poke reaches a register file, so it is
                    // a SID write and the subscriber hears it. Moving the hook to
                    // the bus alone would have silently stopped tracing these: a
                    // monitor write to $D418 would go quiet with nothing to show
                    // for it.
                    // `c64_core.clk` and NOT `cpu6510.clk`: the latter is a mirror
                    // that is only correct after a sync (`cpu6510.clk =
                    // c64_core.clk`), and the bus stamps `c64_core.clk` too — a
                    // poke and a bus write must not report two different clocks
                    // for the same machine. The first draft copied `cpu6510` from
                    // the CIA arms below and the gate caught it reporting 0.
                    let clk = self.c64_core.clk;
                    self.sid_trace.fire(chip, reg, *b, clk);
                    if chip == 0 {
                        self.sid_regs[reg] = *b;
                        self.sid.write(reg, *b, &self.sid_regs);
                    } else if let Some(c) = self.sid_extra.get_mut(chip as usize - 1) {
                        c.regs[reg] = *b;
                        let regs = c.regs;
                        c.engine.write(reg, *b, &regs);
                    }
                }
                0xd800..=0xdbff => {
                    // Colour RAM: only the low nibble is stored, in the I/O shadow.
                    self.io_shadow[(a as usize) - 0xd000] = *b & 0x0f;
                }
                0xdc00..=0xdcff => {
                    let clk = self.cpu6510.clk;
                    let tab = self.cia_table.clone();
                    self.cia1.write(a, *b, clk, &tab);
                }
                0xdd00..=0xddff => {
                    let clk = self.cpu6510.clk;
                    let tab = self.cia_table.clone();
                    self.cia2.write(a, *b, clk, &tab);
                }
                0xd000..=0xdfff => self.io_shadow[(a as usize) - 0xd000] = *b,
                _ => self.ram[a as usize] = *b,
            }
        }
    }

    /// The BANKED write — the counterpart to [`Machine::read_full`], and what a
    /// monitor write verb must use.
    ///
    /// [`Machine::poke`] is the raw-RAM injector for building exercisers: it ignores
    /// banking entirely. Pointing `wr`/`f`/`t` at it meant `wr d020 00` wrote the RAM
    /// hidden *under* the I/O window and left the border untouched, while still
    /// reporting success — the write silently did nothing the user could see.
    ///
    /// This goes through `FullBus::write`, the very code the CPU executes, rather than
    /// a second copy of the banking rules that would drift from it: $D020 reaches the
    /// VIC when I/O is banked in, a write under ROM lands in the RAM beneath, and
    /// $00/$01 re-configure the PLA. The banking/port fields the write may change are
    /// persisted back exactly as `run_for_full_capped_dbg` does after an instruction.
    pub fn write_full(&mut self, addr: u16, val: u8) {
        let table = self.cia_table.clone();
        let port_active = self.port_active();
        let mut fb = full::FullBus {
            ram: &mut self.ram,
            basic_rom: &self.basic_rom,
            kernal_rom: &self.kernal_rom,
            char_rom: &self.char_rom,
            io: &mut self.io_shadow,
            vic: &mut self.vic,
            cia1: &mut self.cia1,
            cia2: &mut self.cia2,
            cia_table: &table,
            sid_regs: &mut self.sid_regs,
            sid: &mut self.sid,
            sid_extra: &mut self.sid_extra,
            sid_map: &self.sid_map,
            sid_trace: &mut self.sid_trace,
            sid_host: &mut self.sid_host,
            config: self.memconfig,
            memconfig_table: &self.memconfig_table,
            port_dir: self.port_dir,
            port_data: self.port_data,
            // Between instructions the machine's own clock is "now" (it is synced from
            // whichever core last ran — see the `self.clk = …` assignments after a run).
            clk: self.clk,
            cia2_pa_out: self.cia2_pa_out,
            side_effects: Vec::new(),
            read_side_effects: Vec::new(),
            drive: &mut self.drive8,
            drive_b: &mut self.drive_b,
            iec: &mut self.iec,
            keyboard: &self.keyboard,
            joystick1: self.joystick1,
            joystick2: self.joystick2,
            drive_c64_ref: self.drive_c64_ref,
            cartridge: self.cartridge.as_mut(),
            // Spec 785 C1 — a MONITOR access is not the title reading: never let it
            // enter the cart read-set.
            cart_reads: None,
            cart_account_suspend: false,
            port_profile: self.port_profile.as_mut(),
            port_host: self.expansion.as_mut(),
            snoop: self.expansion_snoop.as_deref(),
            access_kind: crate::expansion::AccessKind::Host,
            stalled: 0,
            stalled_on_bus: 0,
            device_stop: false,
            host_lines: self.expansion_host_lines,
            port_active,
            io_touched: false,
            cia_alarm_check: self.cia_alarm_check,
        };
        fb.write(addr, val);
        self.memconfig = fb.config;
        self.port_dir = fb.port_dir;
        self.port_data = fb.port_data;
        self.cia2_pa_out = fb.cia2_pa_out;
    }

    /// The LIVE read — what the CPU would see, side effects and all.
    ///
    /// [`Machine::read_full`] is the peek lane: it answers without touching the machine,
    /// which is what a monitor wants by default. `sidefx on` asks for the opposite, and
    /// the daemon had no path for it — the toggle was stored and never consulted, so the
    /// monitor kept reporting "reads are LIVE" while every read stayed a peek.
    ///
    /// Goes through `FullBus::read`, the same code the CPU executes.
    pub fn read_full_live(&mut self, addr: u16) -> u8 {
        let table = self.cia_table.clone();
        let port_active = self.port_active();
        let mut fb = full::FullBus {
            ram: &mut self.ram,
            basic_rom: &self.basic_rom,
            kernal_rom: &self.kernal_rom,
            char_rom: &self.char_rom,
            io: &mut self.io_shadow,
            vic: &mut self.vic,
            cia1: &mut self.cia1,
            cia2: &mut self.cia2,
            cia_table: &table,
            sid_regs: &mut self.sid_regs,
            sid: &mut self.sid,
            sid_extra: &mut self.sid_extra,
            sid_map: &self.sid_map,
            sid_trace: &mut self.sid_trace,
            sid_host: &mut self.sid_host,
            config: self.memconfig,
            memconfig_table: &self.memconfig_table,
            port_dir: self.port_dir,
            port_data: self.port_data,
            clk: self.clk,
            cia2_pa_out: self.cia2_pa_out,
            side_effects: Vec::new(),
            read_side_effects: Vec::new(),
            drive: &mut self.drive8,
            drive_b: &mut self.drive_b,
            iec: &mut self.iec,
            keyboard: &self.keyboard,
            joystick1: self.joystick1,
            joystick2: self.joystick2,
            drive_c64_ref: self.drive_c64_ref,
            cartridge: self.cartridge.as_mut(),
            // Spec 785 C1 — a MONITOR access is not the title reading: never let it
            // enter the cart read-set.
            cart_reads: None,
            cart_account_suspend: false,
            port_profile: self.port_profile.as_mut(),
            port_host: self.expansion.as_mut(),
            snoop: self.expansion_snoop.as_deref(),
            access_kind: crate::expansion::AccessKind::Host,
            stalled: 0,
            stalled_on_bus: 0,
            device_stop: false,
            host_lines: self.expansion_host_lines,
            port_active,
            io_touched: false,
            cia_alarm_check: self.cia_alarm_check,
        };
        let v = fb.read(addr);
        self.memconfig = fb.config;
        self.port_dir = fb.port_dir;
        self.port_data = fb.port_data;
        self.cia2_pa_out = fb.cia2_pa_out;
        v
    }

    // ── Spec 850 — the expansion port ───────────────────────────────────────────────

    /// Attach a host device to the expansion port and return the one it replaces.
    pub fn attach_expansion(
        &mut self,
        dev: Box<dyn crate::expansion::ExpansionDevice>,
    ) -> Option<Box<dyn crate::expansion::ExpansionDevice>> {
        let old = self.expansion.replace(dev);
        self.refresh_expansion_snoop();
        old
    }

    pub fn detach_expansion(&mut self) -> Option<Box<dyn crate::expansion::ExpansionDevice>> {
        let old = self.expansion.take();
        self.refresh_expansion_snoop();
        old
    }

    /// Spec 853 D1 — remove ONE device, whether it sits bare on the port or inside a
    /// chain, and leave everything else and its state alone. `detach_expansion` above
    /// takes the whole place, which on a chain means everything on it.
    pub fn detach_expansion_device<T: 'static>(
        &mut self,
    ) -> Option<Box<dyn crate::expansion::ExpansionDevice>> {
        let mut slot = self.expansion.take();
        let mut out = None;
        let bare = slot.as_ref().map(|d| d.as_ref().as_any().is::<T>()).unwrap_or(false);
        if bare {
            out = slot.take();
        } else if let Some(dev) = slot.as_mut() {
            if let Some(chain) =
                dev.as_mut().as_any_mut().downcast_mut::<crate::expansion::ExpansionChain>()
            {
                out = chain.remove::<T>();
            }
        }
        self.expansion.0 = slot;
        self.refresh_expansion_snoop();
        out
    }

    /// The REU, off the port, with everything else on it untouched.
    pub fn detach_reu(&mut self) -> Option<Box<dyn crate::expansion::ExpansionDevice>> {
        self.detach_expansion_device::<crate::reu::Reu>()
    }

    /// Install or remove the machine profile's own device (Spec 852's UCI block).
    pub fn set_port_profile_device(
        &mut self,
        dev: Option<Box<dyn crate::expansion::ExpansionDevice>>,
    ) -> Option<Box<dyn crate::expansion::ExpansionDevice>> {
        let old = std::mem::replace(&mut *self.port_profile, dev);
        self.refresh_expansion_snoop();
        old
    }

    // ── Spec 853 — the REU and GeoRAM ─────────────────────────────────────────────────

    /// Spec 853 D7 — did the last restore leave the expansion RAM behind?
    ///
    /// The owner's ruling: 16 MB of REU RAM is out of the checkpoint ring (it would not
    /// shrink a 32 MiB / 64 KiB ring, it would destroy it) and in the `.c64re` dump. So a
    /// rewind puts the C64 back and leaves the expansion RAM where it is, and the machine
    /// is then half restored. Bug 792's lesson was that the SILENCE is the defect, not the
    /// gap: every surface that restores says so, rather than looking clean.
    pub fn expansion_ram_uncovered(&self) -> bool {
        self.expansion_ram_uncovered
    }

    pub(crate) fn set_expansion_ram_uncovered(&mut self, v: bool) {
        self.expansion_ram_uncovered = v;
    }

    /// Spec 853 D11 — look at the expansion RAM.
    ///
    /// Deliberately NOT a `peek_lens`: that door takes a `u16`, and an REU holds up to
    /// 16 MB. A lens keyed on a C64 address cannot address expansion RAM at all, so this
    /// is its own reader rather than a 16-bit hole into a 24-bit space. Reads are always
    /// side-effect-free — the RAM is memory, not a register file.
    pub fn expansion_ram_slice(&self, offset: u32, len: u32) -> Option<Vec<u8>> {
        if let Some(r) = self.reu() {
            return Some(r.ram_slice(offset, len));
        }
        Some(self.georam()?.ram_slice(offset, len))
    }

    /// Spec 853 D9 — load a `.reu` image into the attached device. Never written back:
    /// the owner's ruling is that nothing goes to the filesystem on its own, and VICE's
    /// own `REUImageWrite` is off by default. A short image fills from the start and
    /// leaves the rest; a long one is truncated.
    pub fn load_expansion_image(&mut self, bytes: &[u8]) -> Result<usize, String> {
        if let Some(r) = self.reu_mut() {
            return Ok(r.write_ram(0, bytes) as usize);
        }
        if let Some(g) = self.georam_mut() {
            return Ok(g.write_ram(0, bytes) as usize);
        }
        Err("no expansion device attached".to_string())
    }

    /// Spec 854 D2 — attach a device whose RAM belongs to the caller. `size_kb` is what
    /// the REC believes is fitted; the store decides what is actually there.
    pub fn attach_reu_borrowed(
        &mut self,
        size_kb: u32,
        store: Box<dyn crate::expansion::ExpansionRam>,
    ) -> bool {
        if !self.attach_reu(size_kb) {
            return false;
        }
        self.reu_mut().expect("just attached").set_store(Some(store));
        true
    }

    pub fn attach_georam_borrowed(
        &mut self,
        size_kb: u32,
        store: Box<dyn crate::expansion::ExpansionRam>,
    ) -> bool {
        if !self.attach_georam(size_kb) {
            return false;
        }
        self.georam_mut().expect("just attached").set_store(Some(store));
        true
    }

    /// Lend a store, take it back (`None`), or swap it — without rebuilding the device.
    /// Returns the store that was there.
    pub fn set_expansion_ram(
        &mut self,
        store: Option<Box<dyn crate::expansion::ExpansionRam>>,
    ) -> Option<Box<dyn crate::expansion::ExpansionRam>> {
        if let Some(r) = self.reu_mut() {
            return r.set_store(store);
        }
        self.georam_mut()?.set_store(store)
    }

    /// Is there a device whose RAM a checkpoint would have to carry?
    pub fn expansion_has_ram(&self) -> bool {
        self.reu().is_some() || self.georam().is_some()
    }

    /// Attach a 17xx REU of `size_kb` KiB (128/256/512, or an oversized 1024..16384).
    /// A CORE device: this works on every profile, not only `u64` (owner, 2026-09-16).
    /// Returns false for a size no REU ever had.
    pub fn attach_reu(&mut self, size_kb: u32) -> bool {
        match crate::reu::Reu::new(size_kb) {
            Some(reu) => {
                self.attach_expansion(Box::new(reu));
                true
            }
            None => false,
        }
    }

    /// Attach a GeoRAM of `size_kb` KiB (a whole number of 16 KiB banks).
    pub fn attach_georam(&mut self, size_kb: u32) -> bool {
        match crate::georam::GeoRam::new(size_kb) {
            Some(g) => {
                self.attach_expansion(Box::new(g));
                true
            }
            None => false,
        }
    }

    /// Spec 853 D1 — add a device WITHOUT evicting what is already there.
    ///
    /// `attach_expansion` replaces, which is right when one thing sits on the port and
    /// wrong as soon as two do. This folds the existing device and the new one into an
    /// [`ExpansionChain`], so a host can keep its own device while the machine carries an
    /// REU. With nothing attached it is exactly `attach_expansion`.
    pub fn attach_expansion_also(&mut self, dev: Box<dyn crate::expansion::ExpansionDevice>) {
        match self.expansion.take() {
            None => {
                self.attach_expansion(dev);
            }
            Some(mut existing) => {
                // Already a chain: extend it THROUGH the box. A trait object cannot be
                // upcast to `Any` to unbox it, and nesting chains would work but leaves a
                // shape nobody expects when they look.
                if let Some(chain) =
                    existing.as_mut().as_any_mut().downcast_mut::<crate::expansion::ExpansionChain>()
                {
                    chain.push(dev);
                    self.attach_expansion(existing);
                } else {
                    let chain = crate::expansion::ExpansionChain::new().with(existing).with(dev);
                    self.attach_expansion(Box::new(chain));
                }
            }
        }
    }

    /// Spec 853 D1 — the device on the port, or the one inside the chain that is.
    ///
    /// Once a second device is added the top box is an [`ExpansionChain`], so a bare
    /// downcast finds nothing. Every accessor goes through here for that reason: a
    /// chained REU is still the machine's REU.
    fn port_find<T: 'static>(&self) -> Option<&T> {
        let dev: &dyn crate::expansion::ExpansionDevice = self.expansion.as_deref()?;
        if let Some(t) = dev.as_any().downcast_ref::<T>() {
            return Some(t);
        }
        dev.as_any().downcast_ref::<crate::expansion::ExpansionChain>()?.find::<T>()
    }

    fn port_find_mut<T: 'static>(&mut self) -> Option<&mut T> {
        let dev: &mut dyn crate::expansion::ExpansionDevice = self.expansion.0.as_deref_mut()?;
        if dev.as_any().downcast_ref::<T>().is_some() {
            return dev.as_any_mut().downcast_mut::<T>();
        }
        dev.as_any_mut()
            .downcast_mut::<crate::expansion::ExpansionChain>()?
            .find_mut::<T>()
    }

    /// The REU on the port, if one is attached — directly or inside a chain.
    pub fn reu(&self) -> Option<&crate::reu::Reu> {
        self.port_find::<crate::reu::Reu>()
    }

    pub fn reu_mut(&mut self) -> Option<&mut crate::reu::Reu> {
        self.port_find_mut::<crate::reu::Reu>()
    }

    /// The GeoRAM on the port, if one is attached — directly or inside a chain.
    pub fn georam(&self) -> Option<&crate::georam::GeoRam> {
        self.port_find::<crate::georam::GeoRam>()
    }

    pub fn georam_mut(&mut self) -> Option<&mut crate::georam::GeoRam> {
        self.port_find_mut::<crate::georam::GeoRam>()
    }

    /// Spec 853 D3 — run a transfer a device has armed, at the instruction boundary.
    ///
    /// The device is TAKEN OUT of its place for the duration: the transfer drives the
    /// same `FullBus` the CPU executes through, and a bus master does not answer its own
    /// cycles. That is also what VICE's `reu_dma_active` amounts to — while a transfer
    /// runs, the registers read 0 and ignore writes.
    ///
    /// Cycles land on `c64_core.clk`, so the caller's SID tick and drive catch-up cover
    /// the transfer without knowing it happened.
    pub fn run_pending_dma(&mut self) {
        let mut dev = match self.expansion.take() {
            Some(d) => d,
            None => return,
        };
        {
            let table = self.cia_table.clone();
            let port_active = self.port_active();
            let mut fb = full::FullBus {
                ram: &mut self.ram,
                basic_rom: &self.basic_rom,
                kernal_rom: &self.kernal_rom,
                char_rom: &self.char_rom,
                io: &mut self.io_shadow,
                vic: &mut self.vic,
                cia1: &mut self.cia1,
                cia2: &mut self.cia2,
                cia_table: &table,
                sid_regs: &mut self.sid_regs,
                sid: &mut self.sid,
                sid_extra: &mut self.sid_extra,
                sid_map: &self.sid_map,
                sid_trace: &mut self.sid_trace,
                sid_host: &mut self.sid_host,
                config: self.memconfig,
                memconfig_table: &self.memconfig_table,
                port_dir: self.port_dir,
                port_data: self.port_data,
                clk: self.c64_core.clk,
                cia2_pa_out: self.cia2_pa_out,
                side_effects: Vec::new(),
                read_side_effects: Vec::new(),
                drive: &mut self.drive8,
                drive_b: &mut self.drive_b,
                iec: &mut self.iec,
                keyboard: &self.keyboard,
                joystick1: self.joystick1,
                joystick2: self.joystick2,
                drive_c64_ref: self.drive_c64_ref,
                cartridge: self.cartridge.as_mut(),
                cart_reads: None,
                cart_account_suspend: false,
                port_profile: self.port_profile.as_mut(),
                port_host: None,
                snoop: self.expansion_snoop.as_deref(),
                // A DMA cycle IS a bus cycle, not a host reaching in between instructions.
                access_kind: crate::expansion::AccessKind::Cpu,
                stalled: 0,
                stalled_on_bus: 0,
                device_stop: false,
                host_lines: self.expansion_host_lines,
                port_active,
                io_touched: false,
                cia_alarm_check: self.cia_alarm_check,
            };
            // Same reason as `port_find_mut`: with a second device on the port the box
            // taken here is the chain, and a bare downcast would arm a transfer that then
            // never ran.
            let chained: Option<&mut crate::reu::Reu> =
                if dev.as_ref().as_any().downcast_ref::<crate::reu::Reu>().is_some() {
                    dev.as_mut().as_any_mut().downcast_mut::<crate::reu::Reu>()
                } else {
                    dev.as_mut()
                        .as_any_mut()
                        .downcast_mut::<crate::expansion::ExpansionChain>()
                        .and_then(|c| c.find_mut::<crate::reu::Reu>())
                };
            if let Some(reu) = chained {
                reu.run_dma(&mut fb);
            }
            self.c64_core.clk = fb.clk;
            self.memconfig = fb.config;
            self.port_dir = fb.port_dir;
            self.port_data = fb.port_data;
            self.cia2_pa_out = fb.cia2_pa_out;
            self.drive_c64_ref = fb.drive_c64_ref;
        }
        self.expansion.replace(dev);
    }

    // ── Spec 852 — the UCI block behind the profile place ──────────────────────────────

    /// The Ultimate Command Interface, Some on the `u64` profile.
    pub fn uci(&self) -> Option<&crate::uci::Uci> {
        let dev: &dyn crate::expansion::ExpansionDevice = self.port_profile.as_deref()?;
        dev.as_any().downcast_ref::<crate::uci::Uci>()
    }

    /// The block for the firmware side (`fw_read`/`fw_write`/`take_events`/`set_routed`).
    /// A C64 reset since the last call is handed to it first, so `take_events` reports it.
    pub fn uci_mut(&mut self) -> Option<&mut crate::uci::Uci> {
        let dev: &mut dyn crate::expansion::ExpansionDevice = self.port_profile.0.as_deref_mut()?;
        let uci = dev.as_any_mut().downcast_mut::<crate::uci::Uci>()?;
        if std::mem::take(&mut self.uci_c64_reset) {
            uci.note_c64_reset();
        }
        Some(uci)
    }

    /// The block's state for the monitor and the daemon, events not yet taken included.
    /// Takes nothing.
    pub fn uci_status(&self) -> Option<crate::uci::UciStatus> {
        let mut s = self.uci()?.status();
        s.pending.c64_reset |= self.uci_c64_reset;
        Some(s)
    }

    /// Spec 852 D7 — the block is not in any snapshot, ring or dump; its other half is the
    /// firmware's state in the host. A restore puts it back to power-on (disabled).
    pub fn reset_uci_to_power_on(&mut self) {
        if let Some(uci) = self.uci_mut() {
            uci.reset_to_power_on();
        }
        self.uci_c64_reset = false;
    }

    /// Rebuild the snoop set from both devices. Called on attach; call it again if a
    /// device changes the addresses it snoops.
    pub fn refresh_expansion_snoop(&mut self) {
        let mut set = crate::expansion::SnoopSet::default();
        for dev in [self.port_profile.as_ref(), self.expansion.as_ref()].into_iter().flatten() {
            for &addr in dev.snoop_addresses() {
                set.insert(addr);
            }
        }
        self.expansion_snoop = if set.is_empty() { None } else { Some(Box::new(set)) };
    }

    /// Spec 853 D1 — is this address still snooped by anything on the port?
    ///
    /// The write path pays for a snooped address, so a device that has left must stop
    /// registering one. Being able to ASK is what makes that testable rather than assumed.
    pub fn expansion_snoop_registered(&self, addr: u16) -> bool {
        self.expansion_snoop.as_ref().map(|s| s.contains(addr)).unwrap_or(false)
    }

    /// Spec 850 D6 — the IRQ/NMI the host itself drives onto the port. Takes effect at the
    /// next instruction boundary.
    pub fn set_expansion_lines(&mut self, irq: bool, nmi: bool) {
        self.expansion_host_lines.irq = irq;
        self.expansion_host_lines.nmi = nmi;
    }

    /// The port's lines now: the host's own, ORed with both devices'.
    pub fn expansion_lines(&self) -> crate::expansion::PortLines {
        let mut lines = self.expansion_host_lines;
        if let Some(dev) = self.port_profile.as_ref() {
            lines = lines.or(dev.lines());
        }
        if let Some(dev) = self.expansion.as_ref() {
            lines = lines.or(dev.lines());
        }
        lines
    }

    /// Spec 850 — whether the port has anything to say this run: a device, a host line,
    /// or an expansion interrupt still pending from before. False on a stock machine, and
    /// then the per-cycle and per-instruction port work is skipped.
    #[inline]
    fn port_active(&self) -> bool {
        self.port_profile.is_some()
            || self.expansion.is_some()
            || self.expansion_host_lines != crate::expansion::PortLines::default()
            || self.c64_int.pending_int[c64_6510core::INT_SRC_EXPANSION] != 0
    }

    /// Spec 850 D7 — hold or release the 6510 from the host.
    pub fn set_hold(&mut self, hold: Option<crate::expansion::Hold>) {
        self.hold = hold;
    }

    /// The hold in force: the host's, else `Cpu` while a device holds its line.
    pub fn effective_hold(&self) -> Option<crate::expansion::Hold> {
        self.hold
            .or_else(|| self.expansion_lines().hold.then_some(crate::expansion::Hold::Cpu))
    }

    /// Spec 850 D3 — `$DE00-$DFFF` without side effects: the cartridge's peek, then the
    /// profile's device, then the host's, then the open bus.
    fn port_peek(&self, addr: u16) -> u8 {
        let cart = self.cart_peek_byte(addr);
        let mut answer = None;
        if let Some(dev) = self.port_profile.as_ref() {
            answer = dev.peek(addr, cart);
        }
        if let Some(dev) = self.expansion.as_ref() {
            answer = answer.or(dev.peek(addr, cart));
        }
        answer.or(cart).unwrap_or(self.vic.last_read_phi1)
    }

    /// Spec 850 D7 — advance `cycles` with the 6510 held, per cycle as the SC bus's
    /// `clk_inc` + `vic_cycle` do, the CPU registers untouched. `Cpu`: VIC, CIAs, SID and
    /// drive 8 run. `Reset`: the VIC alone; the drive's reference is moved along so the
    /// next run does not fast-forward it through the hold.
    ///
    /// Port reference: VICE holds the CPU for DMA by stealing cycles while the chips run
    /// (`mainc64cpu.c:122-125`) and services `IK_DMA` at the boundary (`6510core.c:523`).
    fn run_held(&mut self, hold: crate::expansion::Hold, cycles: u64) {
        let table = self.cia_table.clone();
        let start = self.c64_core.clk;
        let end = start.wrapping_add(cycles);
        let chips = hold == crate::expansion::Hold::Cpu;
        while self.c64_core.clk < end {
            self.c64_core.clk = self.c64_core.clk.wrapping_add(1);
            let clk = self.c64_core.clk;
            let vbank = self.vic_bank_base();
            // Resolved BEFORE the view, so the borrow is of `memconfig` + `cartridge`
            // and not of the whole machine — `vic.tick` needs `&mut self.vic`.
            let romh =
                crate::full::vic_romh_window(self.memconfig.ultimax, self.cartridge.as_ref());
            let view = crate::vic::VicMemView {
                ram: &self.ram,
                char_rom: Some(&self.char_rom),
                color_ram: &self.io_shadow[0x0800..0x0c00],
                vbank,
                romh,
            };
            self.vic.tick(&view);
            if chips {
                self.cia1.clk = clk;
                self.cia2.clk = clk;
                self.cia1.tick(&table);
                self.cia2.tick(&table);
            }
        }
        let clk = self.c64_core.clk;
        if chips {
            self.cia1.checked_clk = clk;
            self.cia2.checked_clk = clk;
            self.cia1.update_to(clk, &table);
            self.cia2.update_to(clk, &table);
            self.sid.tick(clk.wrapping_sub(start), &self.sid_regs);
            self.catch_up_drives(clk);
        } else {
            self.drive_c64_ref = clk;
        }
    }

    /// Set the program counter (CPU-isolated: no boot, atomic PC write).
    pub fn set_pc(&mut self, pc: u16) {
        self.cpu6510.reg_pc = pc;
        self.cpu.pc = pc;
    }

    /// Refresh the legacy `cpu`/`clk` snapshot after monitor register edits.
    pub fn sync_after_monitor(&mut self) {
        self.sync_snapshot();
    }

    /// reverse-debug Phase 1b — restore one undone write's `old_value` to the SAME
    /// storage location the forward store wrote, WITHOUT chip side effects. RAM /
    /// zero-page is byte-exact (the crash use-case target: stack / ZP / code). For the
    /// IO window the store goes to the I/O shadow / register file directly (NOT through
    /// the chip's `write`, which would re-run timer/latch side effects) — best-effort,
    /// per the contract that reverse-step does NOT restore chip internal counters.
    fn restore_byte(&mut self, addr: u16, old: u8) {
        match addr {
            // The $00/$01 CPU port: restore the byte AND recompute the PLA memconfig from
            // it, so a subsequent `read_full` peek sees the rolled-back banking (the port
            // drives the bank map; leaving memconfig stale would mis-bank reads after the
            // undo). This is byte-state restoration, not chip-counter restoration.
            0x0000 => {
                self.port_dir = old;
                self.memconfig = self.memconfig_table[self.pla_index()];
            }
            0x0001 => {
                self.port_data = old;
                self.memconfig = self.memconfig_table[self.pla_index()];
            }
            0x0002..=0xcfff => self.ram[addr as usize] = old,
            0xd000..=0xdfff => {
                // I/O window: write the byte back into the shadow/register file
                // directly (no chip side effects). VIC/SID register files + the I/O
                // shadow mirror the storage `poke_io` uses; the colour-RAM nibble keeps
                // its low-nibble convention. Chip internal counters are out of scope.
                match addr {
                    0xd000..=0xd3ff => self.vic.write_reg(addr as u8, old),
                    0xd400..=0xd7ff => {
                        // Spec 855 — undo the byte in the chip that took it. Putting
                        // a second chip's write back into chip 0's shadow would
                        // corrupt both, silently, and only while rewinding.
                        match crate::sid::resolve_sid(&self.sid_map, addr) {
                            Some((chip, reg)) if chip != 0 => {
                                if let Some(c) = self.sid_extra.get_mut(chip as usize - 1) {
                                    c.regs[reg] = old;
                                }
                            }
                            Some((_, reg)) => self.sid_regs[reg] = old,
                            None => self.sid_regs[(addr as usize - 0xd400) & 0x1f] = old,
                        }
                    }
                    0xd800..=0xdbff => {
                        self.io_shadow[(addr as usize) - 0xd000] = old & 0x0f;
                    }
                    _ => self.io_shadow[(addr as usize) - 0xd000] = old,
                }
            }
            // $E000-$FFFF stores land in RAM under the KERNAL (writes go to RAM even
            // with KERNAL mapped — the forward store wrote `ram[addr]`).
            0xe000..=0xffff => self.ram[addr as usize] = old,
        }
    }

    /// reverse-debug Phase 1b — UNDO the last `n` retired instructions, landing the
    /// machine at the state BEFORE the oldest one undone. For each instruction (newest
    /// first): write its writes' `old_value`s back in REVERSE order, then restore the
    /// CPU registers (incl. the composite P) from that entry's PRE-state header. Pops
    /// each undone entry off the delta ring so a subsequent reverse-step targets the
    /// instruction before it.
    ///
    /// HARD CONTRACT: this restores CPU + RAM + IO-register BYTES, NOT chip internal
    /// counters (VIC raster / CIA timers / sprite-DMA). After a reverse-step the machine
    /// is for INSPECTION ONLY — to resume forward, restore a checkpoint anchor.
    ///
    /// Returns `Ok((landed, undone_writes))` where `landed` is the PC/regs the machine
    /// now sits at and `undone_writes` is the full list of writes rolled back (newest
    /// instruction first, each instruction's writes in undo order). Errs if the ring is
    /// disabled/empty or shallower than `n` (reports how many were available).
    pub fn reverse_step(&mut self, n: usize) -> Result<ReverseStepOutcome, String> {
        if !self.delta_ring.enabled() {
            return Err("reverse-step: the delta ring is disabled (TRX64_CPUHISTORY=0)".into());
        }
        if n == 0 {
            return Err("reverse-step: n must be ≥ 1".into());
        }
        let avail = self.delta_ring.len();
        if avail == 0 {
            return Err(
                "reverse-step: no history in the delta ring (run the machine first)".into(),
            );
        }
        let steps = n.min(avail);
        let mut undone_writes: Vec<crate::delta_ring::WriteRec> = Vec::new();
        let mut landed = ReverseLanded::default();
        let mut scratch: Vec<crate::delta_ring::WriteRec> = Vec::new();
        for _ in 0..steps {
            // Pop the newest entry (also rewinds the write cursor past its writes).
            let e = match self.delta_ring.pop_newest() {
                Some(e) => e,
                None => break,
            };
            // The entry's writes were captured BEFORE pop rewound the cursor; re-read
            // them from the just-popped header. (pop_newest only moved the heads; the
            // slab bytes are intact until overwritten by a future forward write.)
            self.delta_ring.writes_for(&e, &mut scratch);
            // Undo writes in REVERSE order (the last write is on top of the prior one
            // at the same address — e.g. RMW dummy-then-real).
            for w in scratch.iter().rev() {
                self.restore_byte(w.addr, w.old_value);
                undone_writes.push(*w);
            }
            // Restore the CPU PRE-state (the state before this instruction).
            self.c64_core.reg_pc = e.pc;
            self.c64_core.reg_a = e.a;
            self.c64_core.reg_x = e.x;
            self.c64_core.reg_y = e.y;
            self.c64_core.reg_sp = e.sp;
            self.c64_core.set_status_composite(e.p);
            self.c64_core.clk = e.cycle;
            landed = ReverseLanded {
                pc: e.pc,
                a: e.a,
                x: e.x,
                y: e.y,
                sp: e.sp,
                p: e.p,
                cycle: e.cycle,
            };
        }
        // Mirror the rewound CPU state into the legacy snapshot the daemon reads.
        self.sync_snapshot_sc();
        Ok(ReverseStepOutcome {
            steps_taken: steps,
            landed,
            undone_writes,
        })
    }

    /// reverse-debug Phase 1b — the stack-crash shortcut. Scan the delta ring's writes
    /// BACKWARD (newest→oldest) for the last `limit` writers of `addr`. Returns each hit
    /// as the writing instruction's PC + cycle + the `old→new` bytes + its caller chain
    /// (TRX64 feature-request #2), newest first. Stops at the ring's readable edge (older
    /// history lives only in the finalized trace). Read-only — does not mutate the machine.
    pub fn who_wrote(&self, addr: u16, limit: usize) -> Vec<WhoWroteHit> {
        self.delta_ring
            .who_wrote_with_callers(addr, limit)
            .into_iter()
            .map(|(e, w, chain)| WhoWroteHit {
                pc: e.pc,
                cycle: e.cycle,
                addr: w.addr,
                old_value: w.old_value,
                new_value: w.new_value,
                caller_chain: chain,
            })
            .collect()
    }

    /// RUNTIME REVERSE-DEPTH KNOB (`runtime/set_reverse_depth` / `revdepth`). Rebuild
    /// BOTH always-on rings (delta + cpu-history) for `seconds` of FUTURE history,
    /// using the same per-second sizing `DeltaRing::new` derives from
    /// `TRX64_REVERSE_SECONDS`. The cpu-history ring (whose BOOT default is a fixed
    /// 256k entries ≈ 0.25 s, NOT seconds-scaled) is also scaled to `seconds` here so
    /// the two rings cover the same window after the knob — a deliberate, documented
    /// effect of the runtime knob (the boot default is unchanged).
    ///
    /// DISCARDS CURRENT HISTORY: both slabs are freshly allocated, so anything already
    /// captured is gone and capture restarts empty. This CANNOT retroactively extend
    /// history — a culprit older than the new depth that already scrolled out is lost;
    /// only capture from now on uses the new depth. Returns the new capacities so the
    /// caller can report the RAM cost. `seconds` is clamped to ≥ 1 here; the daemon
    /// applies the upper clamp + the multi-GB warning.
    pub fn set_reverse_depth(&mut self, seconds: usize) -> ReverseDepthInfo {
        let secs = seconds.max(1);
        let (entry_cap, writes_cap) = crate::delta_ring::DeltaRing::caps_for_seconds(secs);
        let cpu_cap = secs * crate::delta_ring::ring_instr_per_second();
        self.delta_ring.resize(entry_cap, writes_cap);
        self.cpu_history.resize(cpu_cap);
        // RAM bytes: delta entries (24 B) + delta writes (4 B) + cpu-history (24 B).
        let ram_bytes =
            entry_cap * std::mem::size_of::<crate::delta_ring::DeltaEntry>()
                + writes_cap * std::mem::size_of::<crate::delta_ring::WriteRec>()
                + cpu_cap * std::mem::size_of::<crate::cpu_history::CpuHistEntry>();
        ReverseDepthInfo {
            seconds: secs,
            delta_entry_capacity: entry_cap,
            delta_write_capacity: writes_cap,
            cpu_history_capacity: cpu_cap,
            ram_bytes: ram_bytes as u64,
        }
    }

    /// Current reverse-depth as derived from the delta ring's entry capacity (the
    /// authority for `seconds`), plus both rings' live capacities + the RAM cost — so
    /// a no-arg `revdepth` / `runtime/set_reverse_depth` query can report the state.
    pub fn reverse_depth_info(&self) -> ReverseDepthInfo {
        let entry_cap = self.delta_ring.entry_capacity();
        let writes_cap = self.delta_ring.writes_capacity();
        let cpu_cap = self.cpu_history.capacity();
        let seconds = (entry_cap / crate::delta_ring::ring_instr_per_second()).max(1);
        let ram_bytes =
            entry_cap * std::mem::size_of::<crate::delta_ring::DeltaEntry>()
                + writes_cap * std::mem::size_of::<crate::delta_ring::WriteRec>()
                + cpu_cap * std::mem::size_of::<crate::cpu_history::CpuHistEntry>();
        ReverseDepthInfo {
            seconds,
            delta_entry_capacity: entry_cap,
            delta_write_capacity: writes_cap,
            cpu_history_capacity: cpu_cap,
            ram_bytes: ram_bytes as u64,
        }
    }

    /// TRX64 feature-request #3 — the typed ring-exhaustion signal for a reverse query
    /// that came up empty. `found` = whether the query located its target in the
    /// retained window. When `found` is false AND the delta ring has WRAPPED (evicted
    /// older history), the answer may lie older than the ring → `ring_exhausted: true`
    /// with a remediation hint (raise `revdepth` / break earlier). When the ring still
    /// has headroom a miss is a genuine "never happened / wrong address" (not exhausted).
    /// `suggest_seconds` is the next-step depth the hint proposes (caller picks, e.g.
    /// 2× current clamped to 600).
    pub fn ring_exhaustion(&self, found: bool) -> RingExhaustion {
        let seconds = self.reverse_depth_info().seconds;
        if found || !self.delta_ring.entries_wrapped() {
            return RingExhaustion {
                ring_exhausted: false,
                revdepth_seconds: seconds,
                hint: String::new(),
            };
        }
        let suggest = (seconds.saturating_mul(2)).clamp(seconds + 1, 600);
        RingExhaustion {
            ring_exhausted: true,
            revdepth_seconds: seconds,
            hint: format!(
                "the reverse window is full (revdepth={seconds}s, ring wrapped) — the \
                 answer may be OLDER than the ring. Raise depth (`revdepth {suggest}`) \
                 or trigger/break earlier, then re-run."
            ),
        }
    }

    /// TRX64 feature-request — GUARDRAIL #2 (undump vs mounted cart). After an `undump`
    /// restores C64 RAM from a snapshot while a cartridge is MOUNTED, the resident RAM
    /// the snapshot wrote can diverge from the cart's flash/ROM (the field session's
    /// "resident code != mounted flash" pollution). This samples the cart's low ROM
    /// window ($8000..) via the side-effect-free cart peek and compares it to the resident
    /// RAM underneath; if they differ it returns `Some((addr, cart_byte, ram_byte))` for
    /// the FIRST divergent sample — a NON-FATAL nudge. `None` when no cart is mounted, the
    /// cart maps no low ROM, or the resident bytes match the cart at every sample.
    /// Read-only.
    pub fn cart_resident_divergence(&self) -> Option<(u16, u8, u8)> {
        // Only meaningful with a cartridge mounted.
        self.cartridge.as_ref()?;
        // Sample across the 8K low cart window at a coarse stride (the nudge only needs
        // ONE mismatch to flag; a full 8K compare is unnecessary for a workflow warning).
        for off in (0u16..0x2000).step_by(0x40) {
            let addr = 0x8000u16 + off;
            // The cart byte this window would map (None ⇒ cart maps no low ROM here).
            let cart_byte = match self.cart_peek_byte(addr) {
                Some(b) => b,
                None => return None, // mapper exposes no low-ROM peek → can't compare.
            };
            let ram_byte = self.ram[addr as usize];
            if cart_byte != ram_byte {
                return Some((addr, cart_byte, ram_byte));
            }
        }
        None
    }

    /// reverse-debug Phase 2 — guided crash-triage. Reads the two always-on rings
    /// (CPU-history for the wild-transfer walk, delta-ring for the corruptor) and the
    /// current (crashed) PC to reconstruct the causal chain:
    ///   crash point → wild control transfer (RTS/RTI/JMP-ind/JMP/JSR) → stack corruptor.
    ///
    /// PRAGMATIC + HONEST: each step is confidence-tagged; a non-stack-pop transfer is
    /// reported WITHOUT fabricating a stack corruptor (see `crash_triage`). Read-only —
    /// does not mutate the machine, so it is safe on the live JAM drop-in and re-runnable.
    ///
    /// `at_pc` overrides the crash PC (defaults to the live `c64_core.reg_pc`) so a
    /// caller can triage a specific wild PC; the JAM drop-in passes the jammed PC.
    pub fn crash_triage(&self, at_pc: Option<u16>) -> crate::crash_triage::TriageChain {
        // Snapshot the WHOLE CPU-history ring (oldest → newest). A JAMmed CPU re-fetches
        // the same opcode every cycle, so the ring tail is a long storm of identical
        // crash-PC entries (up to a full ~19.6k-cycle PAL frame before the auto-break
        // halts the run) — the wild transfer can be thousands of entries back. The
        // triage's `find_wild_transfer` collapses that storm; we just need to hand it the
        // full window so the transfer is not beyond the snapshot. This is an on-demand
        // query (not the hot path): copying ≤256k × 24 B (≤6 MiB) is fine.
        let mut history: Vec<crate::cpu_history::CpuHistEntry> = Vec::new();
        self.cpu_history.last_n(self.cpu_history.len(), &mut history);
        let crash_pc = at_pc.unwrap_or(self.c64_core.reg_pc);
        crate::crash_triage::triage(crate::crash_triage::TriageInputs {
            history: &history,
            delta: &self.delta_ring,
            crash_pc,
            // Side-effect-free banked read (IO-aware) for the crash opcode + popped
            // stack bytes.
            read: |a| self.read_full(a),
        })
    }

    /// Side-effect-free banked read through the current PLA config (for
    /// session/state vectors). RAM / BASIC / KERNAL / CHARGEN / IO per memconfig;
    /// I/O reads use the register PEEK (no IRQ-latch clears), color RAM low
    /// nibble + $F0 open bus. Reads $00/$01 as the latched port.
    /// $DC00/$DC01 are PINS, not just latches — the keyboard matrix and the two
    /// joysticks pull them low, and `cia1.peek` returns the latch alone. So a
    /// side-effect-free read of $DC00 answered with whatever the KERNAL last wrote
    /// while scanning, and showed neither a held key nor a joystick. Anyone
    /// debugging input through the monitor was measuring nothing and could not tell.
    ///
    /// Neither register has a read side effect — that is $DC0D, the interrupt
    /// latch, which still goes to `peek` untouched. So there is no reason for the
    /// peek to answer differently than the CPU does, and it now calls the same
    /// function the CPU read calls.
    fn cia1_pin_peek(&self, addr: u16) -> u8 {
        let pra = self.cia1.peek(0xdc00);
        let ddra = self.cia1.peek(0xdc02);
        let prb = self.cia1.peek(0xdc01);
        let ddrb = self.cia1.peek(0xdc03);
        match addr & 0xff0f {
            0xdc00 => crate::keyboard::cia1_pa_pins(
                &self.keyboard, self.clk, pra, ddra, prb, ddrb,
                &self.joystick1, &self.joystick2,
            ),
            0xdc01 => crate::keyboard::cia1_pb_pins(
                &self.keyboard, self.clk, pra, ddra, prb, ddrb, &self.joystick1,
            ),
            _ => self.cia1.peek(addr),
        }
    }

    /// $DD00 is PINS too. Bits 6 and 7 are the IEC CLK IN and DATA IN lines, and
    /// `cia2.peek` returns only the latch — so a monitor read of $DD00 showed the
    /// byte the CPU last WROTE and nothing about the bus. Debugging a serial stall
    /// through it means reading a number that cannot answer the question: a KERNAL
    /// loop spinning on DATA looked, from the monitor, like it was spinning on a
    /// value that should have let it out.
    ///
    /// The CPU's own read ([`full::FullBus::io_read`]) also flushes the drive
    /// forward and records an indirection access. A peek does neither — it reads
    /// the bus as it stands. Bits 0-5 come from the latch exactly as the CPU sees
    /// them.
    fn cia2_pin_peek(&self, addr: u16) -> u8 {
        if (addr & 0xff0f) != 0xdd00 {
            return self.cia2.peek(addr);
        }
        let pins = self.iec.iecbus_callback_read(self.clk);
        let pra = self.cia2.peek(0xdd00);
        let ddra = self.cia2.peek(0xdd02);
        (((pra | !ddra) & 0x3f) | pins) & 0xff
    }

    pub fn read_full(&self, addr: u16) -> u8 {
        match addr {
            0x0000 => self.port_dir,
            0x0001 => self.port_data,
            // $8000-$9FFF — ROML when the banking maps the cart low window (CartLo =
            // 8k/16k/ultimax); else RAM. Mirrors FullBus::read so the side-effect-free
            // peek (monitor `m cpu`) reflects cartridge-mapped ROM, not stale RAM.
            // (Audit ws-cart-live-mapping — 713 §7.1.)
            0x8000..=0x9fff => {
                if matches!(self.memconfig.bank8, full::Bank8::CartLo) {
                    if let Some(v) = self.cart_peek_byte(addr) {
                        return v;
                    }
                }
                self.ram[addr as usize]
            }
            0x0002..=0x7fff => self.ram[addr as usize],
            0xa000..=0xbfff => {
                // $A000-$BFFF — ROMH when bank_a==CartHi (16k cart); else BASIC / RAM.
                if matches!(self.memconfig.bank_a, full::BankA::CartHi) {
                    if let Some(v) = self.cart_peek_byte(addr) {
                        return v;
                    }
                    self.ram[addr as usize]
                } else if self.memconfig.basic {
                    self.basic_rom[(addr as usize) - 0xa000]
                } else {
                    self.ram[addr as usize]
                }
            }
            0xc000..=0xcfff => self.ram[addr as usize],
            0xd000..=0xdfff => {
                if self.memconfig.io {
                    match addr {
                        0xd000..=0xd3ff => {
                            self.vic.u64_extra_read(addr).unwrap_or_else(|| self.vic.read_reg(addr as u8))
                        }
                        // Spec 855: a peek shows the register shadow of whichever
                        // chip owns the address — and only the shadow, because a
                        // peek has no side effects and never runs a model. D5's
                        // host peek answers first where one is installed, so the
                        // monitor shows what the C64 would actually read instead
                        // of quietly disagreeing with it.
                        0xd400..=0xd7ff => {
                            let (chip, reg) = match crate::sid::resolve_sid(&self.sid_map, addr) {
                                Some(hit) => hit,
                                None => (0, (addr as usize - 0xd400) & 0x1f),
                            };
                            match self.sid_host.peek(chip, reg) {
                                Some(v) => v,
                                None => self.sid_chip_regs(chip).map(|r| r[reg]).unwrap_or(0xff),
                            }
                        }
                        0xd800..=0xdbff => (self.io_shadow[(addr as usize) - 0xd000] & 0x0f) | 0xf0,
                        0xdc00..=0xdcff => self.cia1_pin_peek(addr),
                        0xdd00..=0xddff => self.cia2_pin_peek(addr),
                        // $DE00-$DFFF with nothing claiming it is the OPEN BUS, and
                        // a monitor must show what the CPU would actually read.
                        // Showing `io_shadow` here was the same defect BUG-049 hit
                        // from the other side: the readback that was supposed to
                        // settle an argument showed the last value written and
                        // never the truth, so it hid the evidence instead.
                        // Spec 850 D3 — the cartridge's side-effect-free peek and the
                        // port's devices before that open bus.
                        _ => self.port_peek(addr),
                    }
                } else if self.memconfig.char_rom {
                    self.char_rom[(addr as usize) - 0xd000]
                } else {
                    self.ram[addr as usize]
                }
            }
            0xe000..=0xffff => {
                if self.memconfig.kernal {
                    self.kernal_rom[(addr as usize) - 0xe000]
                } else if matches!(self.memconfig.bank_e, full::BankE::CartHiUltimax) {
                    // ultimax ROMH at $E000-$FFFF — the cart's high CHIP image (the
                    // boot path an ultimax cart re-vectors through). Mirrors FullBus.
                    self.cart_peek_byte(addr).unwrap_or(0xff)
                } else {
                    self.ram[addr as usize]
                }
            }
        }
    }

    /// T2.8 — side-effect-free banked peek with an explicit bank lens (= the
    /// monitor-shell `c64Bus.peek(addr, lens)` / memory-bus.ts `peek`). Mirrors the
    /// TS lens routing 1:1:
    ///   cpu  → the live PLA-banked side-effect-free read (= [`read_full`]).
    ///   ram  → raw RAM regardless of banking.
    ///   rom  → the underlying ROM byte for ROM windows (BASIC/CHARGEN/KERNAL),
    ///          else raw RAM (memory-bus.ts `peekRom`).
    ///   io   → side-effect-free I/O register peek for $D000-$DFFF, else raw RAM
    ///          (memory-bus.ts `peekIo`).
    ///   cart → cart IO ($DE00-$DFFF) through the mapper's side-effect-free
    ///          `CartMapper::peek` and the expansion port's devices (Spec 850 D3),
    ///          else the open bus; outside it, raw RAM (memory-bus.ts `peekCart`).
    pub fn peek_lens(&self, addr: u16, lens: &str) -> u8 {
        match lens {
            "ram" => self.ram[addr as usize],
            "rom" => match addr {
                0xa000..=0xbfff => self.basic_rom[(addr as usize) - 0xa000],
                0xd000..=0xdfff => self.char_rom[(addr as usize) - 0xd000],
                0xe000..=0xffff => self.kernal_rom[(addr as usize) - 0xe000],
                _ => self.ram[addr as usize],
            },
            "io" => {
                if (0xd000..=0xdfff).contains(&addr) {
                    match addr {
                        0xd000..=0xd3ff => {
                            self.vic.u64_extra_read(addr).unwrap_or_else(|| self.vic.read_reg(addr as u8))
                        }
                        // Spec 855: a peek shows the register shadow of whichever
                        // chip owns the address — and only the shadow, because a
                        // peek has no side effects and never runs a model. D5's
                        // host peek answers first where one is installed, so the
                        // monitor shows what the C64 would actually read instead
                        // of quietly disagreeing with it.
                        0xd400..=0xd7ff => {
                            let (chip, reg) = match crate::sid::resolve_sid(&self.sid_map, addr) {
                                Some(hit) => hit,
                                None => (0, (addr as usize - 0xd400) & 0x1f),
                            };
                            match self.sid_host.peek(chip, reg) {
                                Some(v) => v,
                                None => self.sid_chip_regs(chip).map(|r| r[reg]).unwrap_or(0xff),
                            }
                        }
                        0xd800..=0xdbff => (self.io_shadow[(addr as usize) - 0xd000] & 0x0f) | 0xf0,
                        0xdc00..=0xdcff => self.cia1_pin_peek(addr),
                        0xdd00..=0xddff => self.cia2_pin_peek(addr),
                        // $DE00-$DFFF with nothing claiming it is the OPEN BUS, and
                        // a monitor must show what the CPU would actually read.
                        // Showing `io_shadow` here was the same defect BUG-049 hit
                        // from the other side: the readback that was supposed to
                        // settle an argument showed the last value written and
                        // never the truth, so it hid the evidence instead.
                        _ => self.port_peek(addr),
                    }
                } else {
                    self.ram[addr as usize]
                }
            }
            // cart: the mapper's side-effect-free peek, then the port's devices (Spec
            // 850 D3), then the open bus — never the write-through shadow.
            "cart" => {
                if (0xde00..=0xdfff).contains(&addr) {
                    self.port_peek(addr)
                } else {
                    self.ram[addr as usize]
                }
            }
            // cpu (and any unknown) → the live PLA-banked side-effect-free read.
            _ => self.read_full(addr),
        }
    }

    /// Current VIC bank base from CIA2 port-A bits 0-1 (= computeVicBankBase):
    /// PORT OF: `core/ciacore.c:810` + `c64/c64cia2.c:150-151`. The byte the CIA
    /// puts on port A is `PRA | ~DDRA` — an INPUT pin contributes 1, because the
    /// pin floats high on the pull-up, not 0. `store_ciapa` then takes `~byte & 3`.
    /// Masking with `PRA & DDRA` instead reads an input bank bit as 0 and lands the
    /// VIC 3 banks away: the KERNAL leaves `DDRA = $3F` so both forms agree, but a
    /// fastloader that drives $DD00 itself (Spindle writes `DDRA = $3C`) leaves the
    /// bank bits as inputs and every fetch goes to the wrong 16 KB.
    /// Spec 815 §4 — which machine this session claims to be. Survives a reset;
    /// only `Machine::new` clears it.
    pub fn set_speed_profile(&mut self, profile: crate::vic::SpeedProfile) {
        self.vic.speed_profile = profile;
        // Leaving a stale speed bit behind would make the next probe answer for a
        // machine that is no longer being claimed.
        self.vic.fastmode = 0;
        self.vic.regs[0x2f] = 0;
        self.vic.regs[0x30] = 0;
        self.vic.regs[0x31] = 0;
        self.vic.u64_d031_written = false;
        if profile == crate::vic::SpeedProfile::U64 {
            // Spec 851 D2 — as the menu's "U64 Turbo Registers" at 1 MHz, badline timing on.
            self.vic.u64_regs_en = 0x01;
            self.vic.u64_speed_prefer = 0x80;
        }
        self.c64_core.turbo_div = 1;
        self.c64_core.pending_turbo_div = 1;
        self.c64_core.turbo_phase = 0;
        self.c64_core.turbo_badline = true;
        self.sync_profile_device();
    }

    /// Spec 852 — the `u64` profile owns the UCI block. Entering it installs a fresh block
    /// unless one is already there (a second `set_speed_profile(U64)` keeps its state);
    /// leaving it removes the block. Call after setting `vic.speed_profile` directly, as
    /// an undump does to keep the restored VIC registers.
    pub fn sync_profile_device(&mut self) {
        if self.vic.speed_profile == crate::vic::SpeedProfile::U64 {
            if self.uci().is_none() {
                self.set_port_profile_device(Some(Box::new(crate::uci::Uci::new())));
                // A fresh block has heard of no reset.
                self.uci_c64_reset = false;
            }
        } else if self.uci().is_some() {
            self.set_port_profile_device(None);
        }
    }

    /// Spec 851 D1 — the machine this is: `c64`, `128`, or `u64` (U64, Elite II, C64
    /// Ultimate). Set it before power-on; a cartridge probes in its boot stub.
    pub fn set_machine_profile(&mut self, profile: crate::vic::SpeedProfile) {
        self.set_speed_profile(profile);
    }

    pub fn speed_profile(&self) -> crate::vic::SpeedProfile {
        self.vic.speed_profile
    }

    /// Spec 851 — the firmware's turbo settings, as `setCpuSpeed` writes them
    /// (`u64_config.cc:1634-1636`): the enable word and the preferred speed, applied on
    /// its `C64_SPEED_UPDATE` strobe. Only meaningful on the `u64` profile.
    /// BUG-061 — the Ultimate holds its C64 at 1 MHz for 2.06 s after a reset. Measured
    /// on the owner's device, reset-anchored, the same at 16 and at 64 MHz and stable over
    /// runs; the length is a TIME, so it is taken from this model's clock. Only the U64
    /// profile has it — a stock C64 has no turbo to hold back.
    fn arm_u64_reset_hold(&mut self) {
        const HOLD_SECONDS: f64 = 2.06;
        self.vic.u64_reset_hold = if self.vic.speed_profile == crate::vic::SpeedProfile::U64 {
            (self.model.timing.cpu_hz as f64 * HOLD_SECONDS) as u32
        } else {
            0
        };
    }

    pub fn set_u64_turbo(&mut self, regs_en: u8, speed_prefer: u8) {
        self.vic.u64_regs_en = regs_en;
        self.vic.u64_speed_prefer = speed_prefer;
        // A strobe inside the post-reset hold is not lost: the setting stands and applies
        // when the hold ends (BUG-061 — the firmware strobes 445 cycles after release).
    }

    pub fn set_u64_speed_table(&mut self, table: crate::vic::U64SpeedTable) {
        self.vic.u64_speed_table = table;
    }

    /// Spec 851 — CPU cycles per PHI2 cycle right now (1 on anything but a turbo `u64`).
    /// Callers that cap a run by instructions scale the cap by it.
    pub fn turbo_divider(&self) -> u64 {
        let (index, _) = self.vic.u64_speed();
        u64::from(self.vic.u64_speed_table.mhz(index))
    }

    /// The speed bit as a release would see it: `$D030` bit 0 on the VIC-IIe; on the
    /// Ultimate a speed index above 1 MHz (Spec 851 — bit 7 of `$D031` is badline timing).
    pub fn turbo_engaged(&self) -> bool {
        match self.vic.speed_profile {
            crate::vic::SpeedProfile::U64 => self.vic.u64_speed().0 != 0,
            _ => self.vic.fastmode != 0,
        }
    }

    pub fn vic_bank_base(&self) -> u16 {
        let pra = self.cia2.peek(0xdd00);
        let ddra = self.cia2.peek(0xdd02);
        let bank = (((pra | !ddra) & 0x03) ^ 0x03) as u16;
        bank.wrapping_mul(0x4000)
    }

    /// Render the displayed frame to the model's canvas (PAL 384×272, NTSC 384×247 RGBA,
    /// colodore). The image is the VIC's per-cycle-accumulated `displayed` buffer
    /// (the last COMPLETE frame swept by the raster), cropped + palettized exactly
    /// like the TS oracle's `renderLiteralPortRgba`. This is the VERBATIM per-cycle
    /// output: sprite multiplexing, border sprites, and mid-line raster effects all
    /// render correctly (the prior static single-pass render could not). Returns
    /// (width, height, rgba).
    pub fn render_canvas_rgba(&self) -> (usize, usize, Vec<u8>) {
        render::index_buffer_to_canvas_rgba(&self.vic.displayed[..], &self.model.window)
    }

    /// Render the displayed frame as raw 4-bit COLOUR INDICES (the model's canvas, one
    /// byte per pixel, each `& 0x0f`) — the `fmt 1` palette-indexed live-stream source.
    /// Same per-cycle `displayed` buffer + same crop as [`render_canvas_rgba`],
    /// but un-palettized (the consumer applies [`render::COLODORE`]). Used by the
    /// daemon's live A/V WS push (ADR-073); additive, no byte-exact path touched.
    /// Returns (width, height, indices).
    pub fn render_canvas_indices(&self) -> (usize, usize, Vec<u8>) {
        render::index_buffer_to_canvas_indices(&self.vic.displayed[..], &self.model.window)
    }

    /// Compute the $D01E (sprite-sprite) / $D01F (sprite-background) collision
    /// registers for the current frozen display and merge them into the VIC,
    /// firing the collision IRQ on the 0→nonzero edge (verbatim VICE
    /// vicii-cycle.c:407-433). Returns the freshly-rendered `(ss, sb)` masks. The
    /// merged latches are read-cleared by a subsequent $D01E/$D01F read.
    ///
    /// The full-machine run loop calls this implicitly when the CPU reads
    /// $D01E/$D01F (FullBus::recompute_collisions); this public entry lets the
    /// daemon/session populate the latches for a direct register peek/snapshot.
    pub fn recompute_collisions(&mut self) -> (u8, u8) {
        let mut color_ram = [0u8; 0x0400];
        for (i, c) in color_ram.iter_mut().enumerate() {
            *c = self.io_shadow[0x0800 + i] & 0x0f;
        }
        let bank_base = self.vic_bank_base();
        let inp = render::RenderInput {
            regs: &self.vic.regs,
            ram: &self.ram,
            char_rom: &self.char_rom,
            color_ram: &color_ram,
            bank_base,
            model: self.model,
        };
        let (ss, sb) = render::render_collisions(&inp);
        self.vic.apply_collisions(ss, sb);
        (ss, sb)
    }

    /// Run a cycle budget against an arbitrary observer (= TS session/run with a
    /// tracing sink). Instruction-stepped, identical budget semantics to
    /// `run_for`. Returns the post-run cycle count.
    pub fn run_for_with<O: Observer>(&mut self, budget: u64, obs: &mut O) -> u64 {
        self.run_for(budget, obs);
        self.clk
    }

    // ── Spec 871 — two drive positions on one bus ────────────────────────────────

    /// Put the IEC core's device map in step with both drive positions. A compare
    /// when nothing changed, which on the stock machine is always.
    pub fn sync_drive_slots(&mut self) {
        let (a, b) = crate::drive::pair_bus_slots(&self.drive8, &self.drive_b);
        self.iec.sync_drive_slots(a, b, self.cia2_pa_out);
    }

    /// Catch both drive positions up to the C64 clock `clk`, then fold both ports into
    /// the bus — the sync point at the end of every instruction (Spec 871 D2).
    #[inline]
    pub(crate) fn catch_up_drives(&mut self, clk: u64) {
        self.drive_c64_ref = crate::drive::pair_catch_up(
            &mut self.drive8,
            &mut self.drive_b,
            &mut self.iec,
            clk,
            self.drive_c64_ref,
            self.cia2_pa_out,
        );
        crate::drive::pair_fold_into_iec(&self.drive8, &self.drive_b, &mut self.iec, self.cia2_pa_out);
    }

    /// The drive in position `pos`.
    pub fn drive(&self, pos: crate::drive::DrivePosition) -> &Drive1541 {
        match pos {
            crate::drive::DrivePosition::A => &self.drive8,
            crate::drive::DrivePosition::B => &self.drive_b,
        }
    }

    /// The drive in position `pos`, mutable. Its own setters do not know the other
    /// position: switch power and set the unit through [`Self::set_drive_power`] /
    /// [`Self::set_drive_unit`], which refuse a collision.
    pub fn drive_mut(&mut self, pos: crate::drive::DrivePosition) -> &mut Drive1541 {
        match pos {
            crate::drive::DrivePosition::A => &mut self.drive8,
            crate::drive::DrivePosition::B => &mut self.drive_b,
        }
    }

    /// The position whose powered drive answers to `unit` — what `LOAD"$",9`
    /// reaches. `None`: no powered drive there.
    pub fn position_at_unit(&self, unit: u8) -> Option<crate::drive::DrivePosition> {
        use crate::drive::DrivePosition::{A, B};
        [A, B].into_iter().find(|&p| {
            let d = self.drive(p);
            d.powered() && d.unit() == unit
        })
    }

    /// The position a unit number names for MEDIA: the powered drive answering
    /// there, else a drive that is off but whose jumpers stand at that unit (a disk
    /// can go into a drive that is switched off). `None`: no position is at `unit`.
    pub fn position_for_media(&self, unit: u8) -> Option<crate::drive::DrivePosition> {
        use crate::drive::DrivePosition::{A, B};
        self.position_at_unit(unit).or_else(|| {
            [A, B].into_iter().find(|&p| {
                let d = self.drive(p);
                !d.powered() && d.unit_jumpers() == unit
            })
        })
    }

    /// The other position's claim on `unit`, if it has one: powered, and answering to
    /// `unit` now or from its next reset.
    fn unit_claimed_by_other(&self, pos: crate::drive::DrivePosition, unit: u8) -> Option<String> {
        use crate::drive::DrivePosition::{A, B};
        let other = if pos == A { B } else { A };
        let d = self.drive(other);
        if d.powered() && (d.unit() == unit || d.unit_jumpers() == unit) {
            Some(format!(
                "drive position {} cannot answer to unit {unit}: position {} is powered at unit {unit}",
                pos.name(),
                other.name()
            ))
        } else {
            None
        }
    }

    /// Spec 871 D2 — switch the drive in `pos` on or off (Spec 870 D1 semantics).
    /// Switching on is refused, naming the other position, when that position is
    /// powered at the unit this drive's jumpers would bring it up at: two devices at
    /// one address produce collisions no program relies on. Nothing changes then.
    pub fn set_drive_power(&mut self, pos: crate::drive::DrivePosition, on: bool) -> Result<(), String> {
        if on && !self.drive(pos).powered() {
            let unit = self.drive(pos).unit_jumpers();
            if let Some(e) = self.unit_claimed_by_other(pos, unit) {
                return Err(e);
            }
        }
        self.drive_mut(pos).set_power(on);
        self.sync_drive_slots();
        Ok(())
    }

    /// Spec 871 D2 — set the jumpers of the drive in `pos` (Spec 870 D4: in force at
    /// its next reset). Refused, naming the other position, when this drive is
    /// powered and the other position is powered at `unit`.
    pub fn set_drive_unit(&mut self, pos: crate::drive::DrivePosition, unit: u8) -> Result<(), String> {
        if self.drive(pos).powered() {
            if let Some(e) = self.unit_claimed_by_other(pos, unit) {
                return Err(e);
            }
        }
        self.drive_mut(pos).set_unit(unit)
    }

    /// Load all three standard C64 ROMs from `rom_dir` and perform a cold reset.
    /// Also loads the 1541 DOS ROM for the drive8 emulator (non-fatal if absent).
    ///
    /// Expected filenames (matching the bundled ROMs):
    ///   kernal-901227-03.bin, basic-901226-01.bin, chargen-901225-01.bin
    ///   dos1541-325302-01+901229-05.bin (or 1541.bin alias) — drive ROM
    pub fn boot_from_dir(&mut self, rom_dir: &Path) -> Result<(), RomError> {
        // Power-on DRAM fill FIRST, then ROM loads overwrite their windows.
        self.fill_power_on_ram();
        // Spec 863 — the model's ROM files. The runnable rows all name the ROM set's
        // (C64 NTSC boots the same 901227-03 KERNAL as PAL: the KERNAL's own raster test
        // sets $02A6 from the line count the VIC produces).
        let m = self.model;
        self.load_kernal(&rom_dir.join(&m.kernal))?;
        self.load_basic(&rom_dir.join(&m.basic))?;
        self.load_chargen(&rom_dir.join(&m.chargen))?;
        // Full machine assembled: ROMs are also in the separate arrays now, and
        // the FullBus is available via run_for_full*.
        self.full_assembled = true;
        self.cold_reset();
        // Drive ROM: non-fatal — if absent the drive runs with zeroed ROM
        // (bus open; CPU will JAM immediately, which is a valid isolated state).
        let _ = self.drive8.load_rom(rom_dir);
        // The machine's power-on is the drive's power-on too: the ROM comes into force.
        self.drive8.power_on_reset();
        // Spec 871 — position B gets the same DOS. Its ROM, too, comes into force at
        // B's power-on: here if B is on with the machine, else when it is switched on
        // (`set_power`). Off, its reset is state only; nothing runs.
        let _ = self.drive_b.load_rom(rom_dir);
        if self.drive_b.powered() {
            self.drive_b.power_on_reset();
        } else {
            self.drive_b.cold_reset();
        }
        self.sync_drive_slots();
        Ok(())
    }

    /// Execute one CPU clock cycle. The CPU is the clock master.
    pub fn step_cycle<O: Observer>(&mut self, obs: &mut O) {
        let mut bus = FlatRam { mem: &mut self.ram };
        self.cpu6510.execute_cycle(&mut bus, obs);
        self.clk = self.cpu6510.clk;
    }

    /// Run a CYCLE budget, instruction-stepped (= TS session/run). Convenience
    /// wrapper that applies the TS instruction cap `ceil(budget/2) + 1000`, so a
    /// tight loop stops on the instruction cap exactly as the TS daemon does.
    pub fn run_for<O: Observer>(&mut self, budget: u64, obs: &mut O) {
        let max_instructions = budget.div_ceil(2) + 1000;
        self.run_for_capped(budget, max_instructions, obs);
    }

    /// Run until EITHER `clk - start >= budget` OR `max_instructions` whole
    /// instructions have retired — the FIRST to trip wins (= TS
    /// `runFor(maxInstructions, { cycleBudget })`). The budget check happens at
    /// instruction boundaries, so c64Cycles ends identically to the TS daemon.
    ///
    /// Plain (no-debug) entry point: delegates to [`run_for_capped_dbg`] with no
    /// breakpoints/watch armed. Monomorphization collapses the `None` gates to the
    /// historical hot path; the `RunStop` is discarded (always `Completed` /
    /// `CycleBudget` here). All pre-existing callers stay source-compatible.
    pub fn run_for_capped<O: Observer>(&mut self, budget: u64, max_instructions: u64, obs: &mut O) {
        self.run_for_capped_dbg(budget, max_instructions, None, None, obs);
    }

    /// Debug-capable variant of [`run_for_capped`] (CPU-isolated FlatRam bus). Adds
    /// the exec breakpoint + exec-watch gate at the TOP of the loop body (BEFORE
    /// execute), returning a [`RunStop`] reason (= TS `runFor`,
    /// integrated-session.ts:962-995). With `breakpoints`/`exec_watch` both `None`
    /// the gates compile to nothing and the result is `Completed`/`CycleBudget`,
    /// identical to the plain path. The access-watch `halt_requested` honoring lives
    /// in the full-machine path ([`run_for_full_capped_dbg`]) where the SC bus
    /// carries the per-access watch table.
    pub fn run_for_capped_dbg<O: Observer>(
        &mut self,
        budget: u64,
        max_instructions: u64,
        breakpoints: Option<&HashSet<u16>>,
        exec_watch: Option<&[u8; 0x10000]>,
        obs: &mut O,
    ) -> RunStop {
        let start = self.cpu6510.clk;
        let mut executed: u64 = 0;
        let mut bus = FlatRam { mem: &mut self.ram };
        let mut stop = RunStop::Completed;
        loop {
            // TOP-of-body checks in the TS order (integrated-session.ts:972-984):
            // the for-guard (instruction cap) FIRST, then breakpoint → cycle-budget
            // → exec-watch, all BEFORE execute so a break halts with PC AT the
            // watched instruction (VICE break-on-exec). `None` ⇒ the breakpoint /
            // exec-watch branches are elided by the optimizer; the cap + cycle-budget
            // checks stay exactly where the historical loop had them.
            if executed >= max_instructions {
                break;
            }
            let pc = self.cpu6510.reg_pc;
            if let Some(bp) = breakpoints {
                if bp.contains(&pc) {
                    stop = RunStop::Breakpoint(pc);
                    break;
                }
            }
            if self.cpu6510.clk.wrapping_sub(start) >= budget {
                stop = RunStop::CycleBudget;
                break;
            }
            if let Some(w) = exec_watch {
                if w[pc as usize] != 0 {
                    stop = RunStop::Observer;
                    break;
                }
            }
            // Step a whole instruction (one fetch boundary to the next). A
            // jammed CPU stays at boundary, so this runs exactly one cycle and
            // still counts as one instruction-step — matching the TS `runFor`
            // loop body (stepC64Instruction + i++) on a halted CPU. This is
            // load-bearing: a JAM-terminated exerciser then trips the
            // instruction cap (ceil(budget/2)+1000) at the same cycle the TS does.
            loop {
                self.cpu6510.execute_cycle(&mut bus, obs);
                if self.cpu6510.is_at_boundary() {
                    break;
                }
            }
            executed += 1;
        }
        drop(bus);
        self.sync_snapshot();
        stop
    }

    /// FULL-MACHINE run (= TS integrated-session `runFor` over the assembled
    /// FullBus). Per C64 instruction: catch up the drive to the C64 clock BEFORE,
    /// refresh the cross-chip interrupt lines (CIA1 ∨ VIC → IRQ; CIA2 → NMI),
    /// run a whole instruction with the VIC ticked per cycle + both CIAs in
    /// lockstep + the CPU sampling the IRQ/NMI lines at the boundary, then catch
    /// up the drive AFTER and sample its PC (deduplicated). Budget/cap semantics
    /// identical to [`run_for_capped`].
    ///
    /// `on_drive_step`: deduplicated drive-PC sample (for the drive8-cpu domain).
    /// Spec 784 — arm/disarm the loader-lens head trace. Arming clears the buffer.
    /// OFF by default; only the daemon's `drive-mechanism`-armed run turns it on.
    pub fn arm_head_trace(&mut self, on: bool) {
        self.head_trace_armed = on;
        if on {
            self.head_trace.clear();
            self.head_trace_last = None;
            // Spec 784 read-set lane: clear + re-baseline the per-sector byte counter
            // to the free-running drive count (only deltas are used, so any prior
            // value is a valid baseline).
            self.block_reads.clear();
            self.sector_entry_read_count = self.drive8.rotation.gcr_read_count;
        }
    }

    /// Spec 784 — take the accumulated `(drv_clk, halftrack, sector)` head samples
    /// (sector-change deduplicated). Sector 0xff = head between sectors.
    pub fn drain_head_trace(&mut self) -> Vec<(u64, u8, u8)> {
        std::mem::take(&mut self.head_trace)
    }

    /// Spec 784 — take the accumulated `(drv_clk, halftrack, sector, bytes)` read-set
    /// records (one per sector the drive latched ≥1 GCR byte off). The ORDERED list of
    /// physical blocks the loader actually READ.
    pub fn drain_block_reads(&mut self) -> Vec<(u64, u8, u8, u16)> {
        std::mem::take(&mut self.block_reads)
    }

    /// Spec 785 C1 — arm/disarm the CARTRIDGE read-set. Arming clears it. OFF by
    /// default; only a `cart-read`-armed run turns it on. The disk lane's twin
    /// ([`Machine::arm_head_trace`]) and this one are independent: a title that
    /// loads from disk AND banks a cart can arm both.
    pub fn arm_cart_reads(&mut self, on: bool) {
        self.cart_read_armed = on;
        if on {
            self.cart_read_set.clear();
        }
    }

    /// Spec 785 C1 — take the accumulated cart read-set: one
    /// `{cycle, bank, slot, off_lo, off_hi, bytes}` record per bank residency,
    /// cycle-ordered, live residencies closed so the tail of the run is included.
    pub fn drain_cart_reads(&mut self) -> Vec<crate::cart::CartReadRecord> {
        self.cart_read_set.drain()
    }

    pub fn run_for_full<O: Observer, F>(&mut self, budget: u64, obs: &mut O, on_drive_step: F)
    where
        F: FnMut(u16, u8, u8, u8, u8, u8, u64),
    {
        // Spec 851 — a faster CPU executes more instructions per PHI2 cycle.
        let max_instructions = budget.div_ceil(2) * self.turbo_divider() + 1000;
        self.run_for_full_capped(budget, max_instructions, obs, on_drive_step);
    }

    /// FULL-MACHINE run with an explicit instruction cap.
    ///
    /// Plain (no-debug) entry point: delegates to [`run_for_full_capped_dbg`] with
    /// no breakpoints/watch armed. Returns the `RunStop` (Spec 850: a device on the
    /// expansion port can end a run, and the caller has to be able to tell that from a
    /// spent budget); a caller that ignores it compiles unchanged, and the hot path is
    /// byte-identical (the `None` gates monomorphize away).
    pub fn run_for_full_capped<O: Observer, F>(
        &mut self,
        budget: u64,
        max_instructions: u64,
        obs: &mut O,
        on_drive_step: F,
    ) -> RunStop
    where
        F: FnMut(u16, u8, u8, u8, u8, u8, u64),
    {
        self.run_for_full_capped_dbg(budget, max_instructions, None, None, None, obs, on_drive_step)
    }

    /// Debug-capable variant of [`run_for_full_capped`]. Adds three gates, all
    /// zero-cost when their option is `None` (= TS `runFor`,
    /// integrated-session.ts:962-995):
    ///   * `breakpoints` — exec breakpoint set, checked at the instruction boundary
    ///     BEFORE execute (halts with PC AT the breakpoint; VICE break-on-exec,
    ///     ts:973).
    ///   * `exec_watch` — per-PC exec-watch table, same boundary check (ts:982).
    ///   * `access_watch` — per-address READ/WRITE watch, threaded into the SC bus;
    ///     a hit during the instruction sets `halt_requested`, honored at the NEXT
    ///     boundary (ts:989, "at the trigger" post-access state).
    /// Returns the [`RunStop`] reason. With all three `None` the gates compile to
    /// nothing and the result is `Completed`/`CycleBudget`, byte-identical to the
    /// plain path.
    #[allow(clippy::too_many_arguments)]
    /// Spec 851 D3 / Spec 868 §9 — the Ultimate's speed, read at an instruction boundary
    /// and adopted at the next PHI2 edge.
    ///
    /// 851 charged a `$D031` write to the NEXT INSTRUCTION. That was a convenience with no
    /// source behind it, and UPic is the program that can tell the difference: its row loop
    /// writes index 0 and straight back to max at the top of every picture row — Aleksi's
    /// own comment calls it a resync — and on the instruction model that pair cost four
    /// PHI2 cycles out of a row's sixty-three, so the machine ran out of line and the
    /// picture arrived half-drawn.
    ///
    /// Two things happen here instead, and they were measured together on real U64 firmware
    /// (UE2 session, 2026-09-21, one binary, the model the only variable): the row period
    /// went 126 → 63 PHI2 and the canvas 132 → 256 of 272 rows, while the run-length
    /// signature that says the phase is still being realigned per row held — 62% of colour
    /// runs at three pixels or shorter, against 59% before.
    ///
    ///   1. **The divider takes effect at the next PHI2 edge.** Both stores of the resync
    ///      pair land inside one PHI2 cycle at 64 MHz, so the edge sees `$8F` and the CPU
    ///      never runs slowly — which is what the pair was written to do.
    ///   2. **The write reloads the divider's counter**, so the sub-PHI2 phase restarts at
    ///      the store. That is what the resync BUYS. The phase decides which pixel a store
    ///      paints (868), so without a known starting place a row's 384 stores walk
    ///      relative to the pixel clock and the picture shears. It is also why the reset
    ///      cannot key on "the divider reached 1" as it first did: under this model the
    ///      machine never observes divider 1 at an instruction boundary at all.
    pub fn sync_turbo_from_vic(&mut self) {
        if self.vic.speed_profile != crate::vic::SpeedProfile::U64 {
            return;
        }
        let (index, badline) = self.vic.u64_speed();
        let div = self.vic.u64_speed_table.mhz(index);
        if self.vic.u64_d031_written_this_instruction {
            self.c64_core.turbo_phase = 0;
            self.vic.u64_d031_written_this_instruction = false;
        }
        // Deliberately NOT `turbo_div`: the adoption is at the PHI2 edge in `clk_inc`.
        // Assigning it here would put the speed change back on the instruction boundary,
        // which is the model this replaced.
        self.c64_core.pending_turbo_div = div;
        self.c64_core.turbo_badline = badline;
    }

    pub fn run_for_full_capped_dbg<O: Observer, F>(
        &mut self,
        budget: u64,
        max_instructions: u64,
        breakpoints: Option<&HashSet<u16>>,
        exec_watch: Option<&[u8; 0x10000]>,
        access_watch: Option<&[u8; 0x10000]>,
        obs: &mut O,
        mut on_drive_step: F,
    ) -> RunStop
    where
        F: FnMut(u16, u8, u8, u8, u8, u8, u64),
    {
        let start = self.c64_core.clk;
        let mut executed: u64 = 0;
        let table = self.cia_table.clone();
        let mut stop = RunStop::Completed;
        // Seed CIA clocks from the live CPU clk so timer state machines run from
        // the right rclk.
        self.cia1.clk = self.c64_core.clk;
        self.cia2.clk = self.c64_core.clk;
        // Spec 850 — devices and host lines only change between runs, so this holds for
        // the whole loop; on a stock machine every port step below is skipped.
        let port_active = self.port_active();
        // A host hold, like the devices, only changes between runs.
        let hold_possible = port_active || self.hold.is_some();
        loop {
            // TOP-of-body checks in the TS order (integrated-session.ts:972-984):
            // instruction cap (for-guard) FIRST, then breakpoint → cycle-budget →
            // exec-watch, all BEFORE execute. `None` ⇒ the breakpoint/exec-watch
            // branches are elided; the cap + cycle-budget checks stay where the
            // historical loop had them (only their relative order with each other,
            // which is unobservable — both just break the loop).
            if executed >= max_instructions {
                break;
            }
            // Spec 850 D7 — a held 6510 does not execute. The rest of the machine runs out
            // the cycle budget; a breakpoint or the instruction cap mean nothing while
            // nothing executes.
            if hold_possible {
                if let Some(hold) = self.effective_hold() {
                    let elapsed = self.c64_core.clk.wrapping_sub(start);
                    if elapsed < budget {
                        self.run_held(hold, budget - elapsed);
                    }
                    stop = RunStop::CycleBudget;
                    break;
                }
            }
            let pc = self.c64_core.reg_pc;
            if let Some(bp) = breakpoints {
                if bp.contains(&pc) {
                    stop = RunStop::Breakpoint(pc);
                    break;
                }
            }
            if self.c64_core.clk.wrapping_sub(start) >= budget {
                stop = RunStop::CycleBudget;
                break;
            }
            if let Some(w) = exec_watch {
                if w[pc as usize] != 0 {
                    stop = RunStop::Observer;
                    break;
                }
            }
            // Drive catches up to the current C64 clock BEFORE the instruction
            // (= integrated-session.ts:898 catchUpDrive). Advances the drive's
            // own clock via the model's sync_factor.
            let c64_clk_before = self.c64_core.clk;

            // Refresh cross-chip interrupt lines at the boundary into the verbatim
            // core's IntStatus, per-source (= VICE: the CIA/VIC `set_int` has already
            // stamped int_status by the time DO_INTERRUPT's interrupt_check_*_delay
            // reads it). Advance both CIA timers to the current clk so any underflow
            // latches its ICR flag, then route VIC∨CIA1 → IRQ (sources 0/1) and
            // CIA2 → NMI (source 2), stamped at the boundary clk (= the old
            // set_irq_line semantics, which stamped at self.clk; the SC core's
            // set_irq/set_nmi re-stamp only on the nirq/nnmi 0→1 edge).
            let now = self.c64_core.clk;
            self.cia1.checked_clk = now;
            self.cia2.checked_clk = now;
            // Spec 857 D3 — the same comparison as `process_alarms`. The restamp below stays
            // unconditional: it carries an acknowledge made inside the last PHI2 cycle (856 §3).
            if !self.cia_alarm_check || self.cia1.alarm_due(now) {
                self.cia1.update_to(now, &table);
            }
            if !self.cia_alarm_check || self.cia2.alarm_due(now) {
                self.cia2.update_to(now, &table);
            }
            self.c64_int.set_irq(c64_6510core::INT_SRC_VIC, self.vic.irq_line, now);
            self.c64_int.set_irq(c64_6510core::INT_SRC_CIA1, self.cia1.irq_asserted(), now);
            self.c64_int.set_nmi(c64_6510core::INT_SRC_CIA2, self.cia2.irq_asserted(), now);
            // Spec 850 D6 — the expansion port's lines on their own source; the per-cycle
            // sample inside `clk_inc` catches a change in the middle of an instruction.
            if port_active {
                let port = self.expansion_lines();
                self.c64_int.set_irq(c64_6510core::INT_SRC_EXPANSION, port.irq, now);
                self.c64_int.set_nmi(c64_6510core::INT_SRC_EXPANSION, port.nmi, now);
            }
            self.sync_turbo_from_vic();
            // Spec 856 D3 — decided per boundary, because the speed above is.
            let fast_path = self.turbo_fast_path && self.c64_core.turbo_div > 1;

            // Run a whole instruction over the SC bus (the verbatim core threads the
            // VIC tick + BA steal + interrupt-delay counters into every access).
            {
                let core_pc: *const u16 = &self.c64_core.reg_pc;
                let core_clk: *const u64 = &self.c64_core.clk;
                let core_regs: *const c64_6510core::C64Core6510 = &self.c64_core;
                let fb = full::FullBus {
                    ram: &mut self.ram,
                    basic_rom: &self.basic_rom,
                    kernal_rom: &self.kernal_rom,
                    char_rom: &self.char_rom,
                    io: &mut self.io_shadow,
                    vic: &mut self.vic,
                    cia1: &mut self.cia1,
                    cia2: &mut self.cia2,
                    cia_table: &table,
                    sid_regs: &mut self.sid_regs,
                    sid: &mut self.sid,
                    sid_extra: &mut self.sid_extra,
                    sid_map: &self.sid_map,
                    sid_trace: &mut self.sid_trace,
                    sid_host: &mut self.sid_host,
                    config: self.memconfig,
                    memconfig_table: &self.memconfig_table,
                    port_dir: self.port_dir,
                    port_data: self.port_data,
                    clk: self.c64_core.clk,
                    cia2_pa_out: self.cia2_pa_out,
                    side_effects: Vec::new(),
                    read_side_effects: Vec::new(),
                    drive: &mut self.drive8,
                    drive_b: &mut self.drive_b,
                    iec: &mut self.iec,
                    keyboard: &self.keyboard,
                    joystick1: self.joystick1,
                    joystick2: self.joystick2,
                    drive_c64_ref: self.drive_c64_ref,
                    cartridge: self.cartridge.as_mut(),
                    // Spec 785 C1 — the cart read-set rides with the run ONLY while
                    // armed; None otherwise, so the hot read path is unchanged.
                    cart_reads: if self.cart_read_armed {
                        Some(&mut self.cart_read_set)
                    } else {
                        None
                    },
                    cart_account_suspend: false,
                    port_profile: self.port_profile.as_mut(),
                    port_host: self.expansion.as_mut(),
                    snoop: self.expansion_snoop.as_deref(),
                    access_kind: crate::expansion::AccessKind::Cpu,
                    stalled: 0,
                    stalled_on_bus: 0,
                    device_stop: false,
                    host_lines: self.expansion_host_lines,
                    port_active,
                    io_touched: false,
                    cia_alarm_check: self.cia_alarm_check,
                };
                let mut bus = full_sc::FullScBus {
                    fb,
                    obs,
                    cpu_history: Some(&mut self.cpu_history),
                    delta_ring: Some(&mut self.delta_ring),
                    core_pc,
                    core_clk,
                    core_regs,
                    fetch: None,
                    cur_op: (self.c64_core.reg_pc, 0),
                    fetched: false,
                    access_watch,
                    halt_requested: false,
                };
                full_sc::execute_one(&mut self.c64_core, &mut bus, &mut self.c64_int);
                // Spec 856 D3 — the turbo fast path. Below the divider an instruction usually
                // leaves `clk` where it was, and then everything this loop does around it —
                // CIA catch-up, the interrupt restamp, the drive and SID sync, rebuilding
                // this bus — would find nothing new. So while the clock has not moved and
                // nothing but RAM was touched, run the next instruction inside the same bus.
                // The first PHI2 edge or IO access ends the batch and the boundary block
                // below runs once for all of it. At a divider of 1 every instruction moves
                // `clk`, so this never iterates (`fast_path` is false there anyway).
                //
                // IO has to end the batch, not only a clock edge: an IRQ acknowledge inside
                // one PHI2 cycle reaches `IntStatus` through nothing but the restamp at the
                // top of this loop (Spec 856 §3).
                if fast_path {
                    loop {
                        if self.c64_core.clk != c64_clk_before
                            || bus.fb.io_touched
                            || bus.halt_requested
                            || bus.fb.device_stop
                        {
                            break;
                        }
                        // What the top of the loop would stop on before the next instruction.
                        // Breaking here leaves it to the top, so the order is unchanged.
                        let next = self.c64_core.reg_pc;
                        if executed + 1 >= max_instructions
                            || breakpoints.is_some_and(|bp| bp.contains(&next))
                            || exec_watch.is_some_and(|w| w[next as usize] != 0)
                        {
                            break;
                        }
                        executed += 1;
                        // A fresh bus starts each instruction pre-fetch: the prologue's
                        // interrupt accesses carry the live PC, not the last opcode's.
                        bus.fetched = false;
                        bus.cur_op = (next, 0);
                        full_sc::execute_one(&mut self.c64_core, &mut bus, &mut self.c64_int);
                    }
                }
                // Honor a watchpoint hit from this instruction at the boundary
                // (= TS integrated-session.ts:989 obs.haltRequested). Latch the
                // reason; we still finish the post-instruction drive/SID sync below
                // so the machine state is consistent, then break after the iteration.
                if bus.halt_requested {
                    stop = RunStop::Observer;
                }
                if port_active && bus.fb.device_stop {
                    stop = RunStop::Device;
                }
                // Persist bus-mutated banking/port state back to the Machine.
                self.memconfig = bus.fb.config;
                self.port_dir = bus.fb.port_dir;
                self.port_data = bus.fb.port_data;
                self.cia2_pa_out = bus.fb.cia2_pa_out;
                // Persist the push-flush reference (the drive may have been advanced
                // mid-instruction by a $DD00 access inside the FullBus).
                self.drive_c64_ref = bus.fb.drive_c64_ref;
            }
            // Spec 853 D3 — a bus master that armed a transfer runs it HERE, before the
            // cycle cost is taken: its cycles then fold into the SID tick and the drive
            // catch-up below, which is what makes a transfer invisible to everything that
            // only watches `clk`. On a stock machine `port_active` is false and this is
            // one already-computed bool.
            if port_active && self.expansion.as_ref().is_some_and(|d| d.dma_pending()) {
                self.run_pending_dma();
            }
            // Tick SID by this instruction's cycle cost — wall-clock batch tick
            // matching TS integrated-session.ts:946 `sid.tick(totalCycles)`.
            let instruction_cycles = self.c64_core.clk.wrapping_sub(c64_clk_before);
            self.sid.tick(instruction_cycles, &self.sid_regs);

            // Drive catches up to the NEW C64 clock AFTER the instruction (= TS
            // afterCycleSync / catchUpDrive to the post-instruction clk). A $DD00
            // access already pushed it part-way; this finishes the slice. Feed the
            // live bus state in first (so the drive's PB reads see the C64 lines),
            // then re-fold the drive's PB output into the IEC core for the next
            // instruction's $DD00 reads.
            // = via1d1541.c store_prb / iec_drive_write(~byte): fold the drive's PB
            // output (inverted) into the bus + iec_update_ports for the next $DD00 read.
            // Spec 870: into the drive's own slot, and not at all while it is off or held.
            // Spec 871: both positions caught up first, then both folded.
            self.catch_up_drives(self.c64_core.clk);
            if let Some((pc, a, x, y, sp, p, drv_clk)) = self.drive8.sample_pc_change() {
                on_drive_step(pc, a, x, y, sp, p, drv_clk);
                // Spec 784 — armed-on-command 1541 head-position sample (loader-lens
                // SOURCE ground truth). Pushed only when the sector under the head
                // changes; never runs with the always-on ring.
                if self.head_trace_armed {
                    let ht = self.drive8.rotation.current_half_track as u8;
                    let s = self.drive8.rotation.sector_under_head();
                    let sec = if s < 0 { 0xff } else { s as u8 };
                    if self.head_trace_last != Some((ht, sec)) {
                        // READ-SET lane: the sector we are LEAVING was physically READ
                        // iff the drive latched GCR bytes off it while over it. Attribute
                        // those bytes to the OLD (halftrack, sector), then re-baseline for
                        // the new sector. A rotation gap (0xff) is never a read source.
                        let now = self.drive8.rotation.gcr_read_count;
                        if let Some((old_ht, old_sec)) = self.head_trace_last {
                            let bytes = now.wrapping_sub(self.sector_entry_read_count);
                            if old_sec != 0xff && bytes > 0 {
                                self.block_reads.push((
                                    drv_clk,
                                    old_ht,
                                    old_sec,
                                    bytes.min(0xffff) as u16,
                                ));
                            }
                        }
                        self.sector_entry_read_count = now;
                        self.head_trace.push((drv_clk, ht, sec));
                        self.head_trace_last = Some((ht, sec));
                    }
                }
            }
            executed += 1;
            // A watchpoint that hit during this instruction halts NOW, at the
            // boundary AFTER the post-instruction drive/SID sync (= TS
            // integrated-session.ts:989-992: haltRequested honored after
            // stepC64Instruction, with the machine left in the post-access state).
            if stop == RunStop::Observer || stop == RunStop::Device {
                break;
            }
        }
        self.sync_snapshot_sc();
        stop
    }

    /// VIC-isolated run (= TS session/run with the VIC ticked per CPU cycle).
    /// Identical budget/instruction-cap semantics to [`run_for`], but the bus is
    /// the [`VicBus`] ($D000-$D3FF → VIC) and the VIC is CLOCK-DRIVEN through the
    /// `Bus::tick` / `Bus::check_ba_before_read` hooks the CPU calls per master
    /// cycle: the VIC advances once per CPU cycle and STEALS read cycles when BA
    /// is low (badline c-access / sprite DMA), so c64Cycles ends exactly as the
    /// TS daemon's (whose CPU stalls the same way — vicii_steal_cycles). This is
    /// the cycle-exact VIC↔CPU coupling.
    pub fn run_for_vic<O: Observer>(&mut self, budget: u64, obs: &mut O) {
        let max_instructions = budget.div_ceil(2) + 1000;
        self.run_for_vic_capped(budget, max_instructions, obs);
    }

    /// VIC-isolated run with an explicit instruction cap (see [`run_for_capped`]).
    pub fn run_for_vic_capped<O: Observer>(
        &mut self,
        budget: u64,
        max_instructions: u64,
        obs: &mut O,
    ) {
        let start = self.cpu6510.clk;
        let mut executed: u64 = 0;
        let mut bus = VicBus { mem: &mut self.ram, vic: &mut self.vic };
        loop {
            if self.cpu6510.clk.wrapping_sub(start) >= budget {
                break;
            }
            if executed >= max_instructions {
                break;
            }
            // Step a whole instruction; the VIC ticks per master cycle via the
            // bus hooks (Bus::tick) and steals read cycles via check_ba_before_read.
            loop {
                self.cpu6510.execute_cycle(&mut bus, obs);
                if self.cpu6510.is_at_boundary() {
                    break;
                }
            }
            executed += 1;
        }
        drop(bus);
        self.sync_snapshot();
    }

    /// CIA-isolated run (= TS session/run with both CIAs ticked per CPU cycle).
    /// Same budget / instruction-cap semantics as [`run_for`], but the bus is the
    /// [`CiaBus`] ($DC00-$DCFF → CIA1, $DD00-$DDFF → CIA2). The CIAs are
    /// CLOCK-DRIVEN through the `Bus::tick` hook the CPU calls per master cycle, and
    /// each register access runs the timer state machine forward to the current clk
    /// (rclk = clk, C64SC offsets = 0) — the cycle-exact CIA↔CPU coupling.
    pub fn run_for_cia<O: Observer>(&mut self, budget: u64, obs: &mut O) {
        let max_instructions = budget.div_ceil(2) + 1000;
        self.run_for_cia_capped(budget, max_instructions, obs);
    }

    /// CIA-isolated run with an explicit instruction cap (see [`run_for_capped`]).
    pub fn run_for_cia_capped<O: Observer>(
        &mut self,
        budget: u64,
        max_instructions: u64,
        obs: &mut O,
    ) {
        let start = self.cpu6510.clk;
        let mut executed: u64 = 0;
        let table = self.cia_table.clone();
        // The CIAs share the CPU master clock: seed the bus clk from the live CPU
        // clk so a read/write at CPU cycle N runs the timer to exactly N.
        self.cia1.clk = self.cpu6510.clk;
        self.cia2.clk = self.cpu6510.clk;
        let mut bus = CiaBus {
            mem: &mut self.ram,
            cia1: &mut self.cia1,
            cia2: &mut self.cia2,
            table: &table,
            clk: self.cpu6510.clk,
        };
        loop {
            if self.cpu6510.clk.wrapping_sub(start) >= budget {
                break;
            }
            if executed >= max_instructions {
                break;
            }
            loop {
                self.cpu6510.execute_cycle(&mut bus, obs);
                if self.cpu6510.is_at_boundary() {
                    break;
                }
            }
            executed += 1;
        }
        drop(bus);
        self.sync_snapshot();
    }

    /// SID-isolated run for the `sid` chip-isolation gate (ADR-012).
    ///
    /// Routes $D400-$D7FF to the SID 6581 (register file + osc/env model);
    /// flat RAM everywhere else; interrupts disabled by the exerciser (SEI).
    /// The SID is ticked instruction-batch (same as the TS integrated-session
    /// wall-clock tick): after each whole instruction the SID advances by the
    /// instruction's cycle cost. Budget / instruction-cap semantics identical
    /// to [`run_for_capped`].
    pub fn run_for_sid<O: Observer>(&mut self, budget: u64, obs: &mut O) {
        let max_instructions = budget.div_ceil(2) + 1000;
        self.run_for_sid_capped(budget, max_instructions, obs);
    }

    /// SID-isolated run with an explicit instruction cap.
    pub fn run_for_sid_capped<O: Observer>(
        &mut self,
        budget: u64,
        max_instructions: u64,
        obs: &mut O,
    ) {
        let start = self.cpu6510.clk;
        let mut executed: u64 = 0;
        let mut bus = SidBus {
            mem: &mut self.ram,
            sid: &mut self.sid,
            sid_regs: &mut self.sid_regs,
            clk: self.cpu6510.clk,
        };
        loop {
            if self.cpu6510.clk.wrapping_sub(start) >= budget {
                break;
            }
            if executed >= max_instructions {
                break;
            }
            let clk_before = self.cpu6510.clk;
            loop {
                self.cpu6510.execute_cycle(&mut bus, obs);
                if self.cpu6510.is_at_boundary() {
                    break;
                }
            }
            // Tick SID by this instruction's cycle cost (wall-clock batch tick,
            // matching TS integrated-session.ts:946 `sid.tick(totalCycles)`).
            let instruction_cycles = self.cpu6510.clk.wrapping_sub(clk_before);
            bus.sid.tick(instruction_cycles, bus.sid_regs);
            executed += 1;
        }
        drop(bus);
        self.sync_snapshot();
    }

    /// Drive-sampled run for the `drive8-cpu` trace domain.
    ///
    /// Mirrors the TS `sampleDrivePc()` pattern (integrated-session.ts:845-868 /
    /// ADR-015): the drive 6502 advances proportionally to the C64 CPU, then at
    /// each C64 instruction boundary the drive PC is sampled. Only when the PC
    /// differs from the previous sample is `on_drive_step` called — this is the
    /// sampled/deduplicated stream the TS oracle emits for `drive8-cpu`.
    ///
    /// Drive sync ratio: 1541 PAL clock ≈ C64 PAL clock (both ~985 kHz), so we
    /// run the drive for the same number of cycles as the C64 per C64 instruction
    /// (drive_budget = instruction_cycles_just_elapsed). This is the "sync_factor
    /// ≈ 1" approximation that matches the TS catchUpDrive behaviour.
    ///
    /// `on_drive_step`: called on each deduplicated PC sample with
    ///   (pc, a, x, y, sp, p, drive_clk).
    pub fn run_for_drive_sampled<O: Observer, F>(&mut self, budget: u64, obs: &mut O, mut on_drive_step: F)
    where
        F: FnMut(u16, u8, u8, u8, u8, u8, u64),
    {
        let max_instructions = budget.div_ceil(2) + 1000;
        let start = self.cpu6510.clk;
        let mut executed: u64 = 0;
        // The drive catches up to the C64's main-clock at each C64 instruction
        // boundary. In the TS oracle that C64 is the FULL integrated session, so the
        // per-instruction retire clock the drive catches up to must match it cycle for
        // cycle — otherwise the catch-up targets, and hence the sampled drive_clk
        // values, drift out of phase. We run the C64 over the same CIA-isolated bus
        // the validated `c64-cpu` gate uses (run_for_cia): it reproduces the TS C64
        // cadence exactly. (The VIC bus is NOT used here — its isolated raster phase
        // badlines at lines the boot ROM does not, perturbing the catch-up clock.)
        let table = self.cia_table.clone();
        self.cia1.clk = self.cpu6510.clk;
        self.cia2.clk = self.cpu6510.clk;
        let mut bus = CiaBus {
            mem: &mut self.ram,
            cia1: &mut self.cia1,
            cia2: &mut self.cia2,
            table: &table,
            clk: self.cpu6510.clk,
        };
        loop {
            if self.cpu6510.clk.wrapping_sub(start) >= budget {
                break;
            }
            if executed >= max_instructions {
                break;
            }
            let c64_clk_before = self.cpu6510.clk;
            loop {
                self.cpu6510.execute_cycle(&mut bus, obs);
                if self.cpu6510.is_at_boundary() {
                    break;
                }
            }
            // Drive advances by this C64 instruction's cycle cost, scaled by the
            // model's sync factor.
            let c64_cycles = self.cpu6510.clk.wrapping_sub(c64_clk_before);
            self.drive8.run_cycles(c64_cycles);
            // Sample drive PC (deduplicated).
            if let Some((pc, a, x, y, sp, p, drv_clk)) = self.drive8.sample_pc_change() {
                on_drive_step(pc, a, x, y, sp, p, drv_clk);
            }
            executed += 1;
        }
        drop(bus);
        self.sync_snapshot();
    }
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod ring_exhaustion_tests {
    use super::*;

    /// Record `n` no-write instructions into the machine's delta ring (the always-on
    /// reverse buffer) so a query past the ring depth can be exercised without a full run.
    fn fill_delta(m: &mut Machine, n: u64) {
        m.delta_ring.set_enabled(true);
        for i in 0..n {
            m.delta_ring.begin(0x1000 + (i as u16 & 0xff), 0, 0, 0, 0xff, 0, i);
            m.delta_ring.commit();
        }
    }

    /// Issue #19 / Spec 840 — an EMPTY expansion port is not RAM.
    ///
    /// `$DE00-$DFFF` with no cartridge is the open bus: the VIC's last phi1 fetch.
    /// It used to return the write-through I/O shadow, so writing a byte there and
    /// reading it back returned that byte — for ever. Every program that probes for
    /// a cartridge or an REU by writing a pattern and reading it back was told the
    /// device is present, on a machine with an empty port.
    ///
    /// The probe is written the way real detection code writes it, on purpose: two
    /// different patterns, because a single one can match the bus by luck.
    #[test]
    fn an_empty_expansion_port_reads_the_open_bus_not_what_was_written() {
        let mut m = Machine::new();

        // Give the VIC something recognisable to have fetched, so "the open bus"
        // is a value we can name rather than an incidental zero.
        m.vic.last_read_phi1 = 0x3c;

        for (addr, pattern) in [(0xde00u16, 0x55u8), (0xdf00, 0xaa), (0xdf00, 0x55)] {
            m.write_full(addr, pattern);
            // read_full_live is the CPU's own path (it builds a FullBus and calls
            // io_read); read_full is the side-effect-free peek. Both must agree.
            let read_back = m.read_full_live(addr);
            assert_eq!(read_back, m.read_full(addr), "peek and CPU disagree at ${addr:04x}");
            assert_ne!(
                read_back, pattern,
                "an empty expansion port answered ${addr:04x} with the byte just \
                 written (${pattern:02x}) — that is RAM behaviour, and it tells every \
                 REU/cartridge probe that a device is present"
            );
            assert_eq!(
                read_back, 0x3c,
                "${addr:04x} must read the open bus (the VIC's last phi1 fetch)"
            );
        }

        // And the monitor must not disagree with the CPU about it — the BUG-049
        // lesson: a readback that shows the last value written hides the evidence
        // it was supposed to provide.
        for lens in ["io", "cart"] {
            assert_eq!(
                m.peek_lens(0xdf00, lens), 0x3c,
                "the `{lens}` lens shows a phantom value the CPU never reads"
            );
        }
    }

    #[test]
    fn ring_exhaustion_typed_when_query_past_wrapped_ring() {
        // FEATURE #3: a query that bottoms out against a WRAPPED ring returns
        // ring_exhausted=true with a remediation hint; a miss against a ring with
        // headroom does NOT (genuine "never happened / wrong address").
        let mut m = Machine::new();
        // Shrink the delta ring to a tiny capacity so it wraps quickly (and set a known
        // revdepth so the hint reports it).
        m.delta_ring.resize(4, 8);
        m.delta_ring.set_enabled(true);

        // (a) Not wrapped + a hit → not exhausted.
        let not_exhausted = m.ring_exhaustion(true);
        assert!(!not_exhausted.ring_exhausted);
        assert!(not_exhausted.hint.is_empty());

        // (b) Not yet wrapped + a miss → still NOT exhausted (ring has headroom).
        fill_delta(&mut m, 3); // 3 < cap 4 → no eviction.
        assert!(!m.delta_ring.entries_wrapped());
        let headroom_miss = m.ring_exhaustion(false);
        assert!(!headroom_miss.ring_exhausted, "miss with headroom is not exhaustion");

        // (c) Wrapped + a miss → EXHAUSTED, with a typed hint that names revdepth.
        fill_delta(&mut m, 10); // 10 > cap 4 → wraps.
        assert!(m.delta_ring.entries_wrapped());
        let exhausted = m.ring_exhaustion(false);
        assert!(exhausted.ring_exhausted, "miss past a wrapped ring is exhaustion");
        assert!(exhausted.revdepth_seconds >= 1);
        assert!(exhausted.hint.contains("revdepth"), "hint suggests raising depth");
        assert!(exhausted.hint.contains("earlier"), "hint mentions breaking earlier");

        // (d) Wrapped but a HIT → not exhausted (we found the answer in-window).
        let wrapped_hit = m.ring_exhaustion(true);
        assert!(!wrapped_hit.ring_exhausted, "a hit is never exhaustion, even when wrapped");
    }
}
