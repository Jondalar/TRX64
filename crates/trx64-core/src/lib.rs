//! trx64-core — pure, deterministic C64 emulation.
//!
//! No I/O, no async, no socket, no trace format. The crown jewel, isolated and
//! testable against the VICE-derived TS port as Phase-1 spec
//! (C64ReverseEngineeringMCP/src/runtime/headless).
//!
//! Hot-path rule: the core takes a generic `O: Observer` (monomorphized, zero-cost
//! when unused). It NEVER calls back into another process per event.

use std::path::Path;

pub mod cia;
pub mod cpu;
pub mod drive;
pub mod full;
pub mod gcr;
pub mod iec;
pub mod render;
pub mod sid;
pub mod tables;
pub mod vic;
pub mod vsf;

pub use cia::Cia;
pub use cpu::{Bus, Cpu6510};
pub use drive::Drive1541;
pub use full::{FullBus, MemConfig};
pub use iec::IecCore;
pub use sid::Sid6581;
pub use vic::VicII;

/// Zero-cost observation hook, inlined into the core step loop.
///
/// Three faces, one mechanism:
/// - [`NullSink`] — tracing off; the compiler eliminates the hooks entirely.
/// - `FrameSink` (in `trx64-trace`) — forensic firehose → `.c64retrace`.
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusKind {
    Fetch,
    Read,
    Write,
    DummyRead,
    DummyWrite,
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
            self.vic.read_reg(addr as u8)
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
    /// BA-low into the VIC's ba_low_flag for the next read-stall.
    #[inline]
    fn tick(&mut self) {
        self.vic.tick();
    }
    /// VICE check_ba(): stall the CPU read while BA is low (badline / sprite DMA),
    /// stealing cycles + advancing the VIC. Returns the stolen-cycle count.
    #[inline]
    fn check_ba_before_read(&mut self) -> u32 {
        self.vic.steal_cycles()
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
    /// Cycle-stepped 6510 (cpu.rs). The flat RAM above is its bus.
    pub cpu6510: Cpu6510,
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
    /// 1541 floppy drive (isolation gate: ADR-012). Booted from DOS ROM, no IEC
    /// wiring to the C64 in Phase 1. Ticked by `run_for_drive_sampled` when the
    /// `drive8-cpu` trace domain is active.
    pub drive8: Drive1541,

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
    /// Monotonic C64-clock reference the drive has been advanced up to. The
    /// push-flush catch-up advances the drive by `clk - drive_c64_ref` before
    /// sampling/applying the IEC lines on a $DD00 access (= VICE
    /// drive_cpu_execute_one/all at the exact C64 read/write instant).
    pub drive_c64_ref: u64,
}

/// ROM load error.
#[derive(Debug)]
pub enum RomError {
    Io(std::io::Error),
    /// ROM file had unexpected size (got, expected).
    BadSize(usize, usize),
}

impl std::fmt::Display for RomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RomError::Io(e) => write!(f, "ROM I/O error: {e}"),
            RomError::BadSize(got, exp) => write!(f, "ROM size mismatch: got {got}, expected {exp}"),
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
    pub fn new() -> Self {
        Self {
            ram: Box::new([0u8; 0x10000]),
            clk: 0,
            cpu6510: Cpu6510::new(),
            cpu: Cpu::default(),
            vic: VicII::new(),
            cia1: Cia::new(),
            cia2: Cia::new(),
            cia_table: cia::new_table(),
            drive8: Drive1541::new(),
            basic_rom: Box::new([0u8; 0x2000]),
            kernal_rom: Box::new([0u8; 0x2000]),
            char_rom: Box::new([0u8; 0x1000]),
            io_shadow: Box::new([0u8; 0x1000]),
            sid_regs: [0u8; 32],
            sid: Sid6581::new(),
            port_dir: 0x2f,
            port_data: 0x37,
            memconfig: full::build_memconfig_table()[0x1f],
            memconfig_table: full::build_memconfig_table(),
            full_assembled: false,
            cia2_pa_out: 0xff,
            iec: IecCore::new(),
            drive_c64_ref: 0,
        }
    }

    /// Mirror the live Cpu6510 register state into the legacy `cpu` snapshot +
    /// `clk` (the daemon reads from these). Call after any run.
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

    pub fn cold_reset(&mut self) {
        let lo = self.ram[0xFFFC] as u16;
        let hi = self.ram[0xFFFC + 1] as u16;
        let pc = lo | (hi << 8);
        self.cpu6510.reset_to(pc);
        // ADR-011 RESOLVED (integration): the C64/VICE 6510 power-on leaves
        // P = $20 (P_UNUSED only) — the I flag is NOT set by reset. The KERNAL
        // reset routine's own `SEI` at $FCE4 sets I. The full-boot trace[0]
        // (LDX #$FF @ $FCE2) records reg_p = $20; forcing I here produced $24.
        // (The earlier `reg_p |= 0x04` was a CPU-isolated convenience — but the
        // CPU-isolated gates inject PC via `set_pc`, never `cold_reset`, so they
        // are unaffected by dropping it.)
        // CPU-port power-on latches: $00=$2F (DDR), $01=$37 (port) — boot config
        // 31 (BASIC+IO+KERNAL). These drive the FullBus banking; the actual RAM[0]/
        // [1] mirror is written by `prepare_full_boot` (only on the full-machine
        // path) so the CPU/chip-ISOLATED gates keep zero-page $00/$01 at the power-
        // on DRAM fill (their exercisers were recorded against that).
        self.port_dir = 0x2f;
        self.port_data = 0x37;
        let port = (!self.port_dir | self.port_data) & 0x07;
        self.memconfig = self.memconfig_table[(port | 0x18) as usize & 0x1f];
        // IEC bus: power-on released (= installCia2 seeds iecWrite(0xff, 0x3f)).
        self.iec = IecCore::new();
        self.cia2_pa_out = 0xff;
        self.drive_c64_ref = 0;
        // SID: reset register file + voice state to power-on defaults.
        self.sid_regs = [0u8; 32];
        self.sid.reset();
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

    /// Set the program counter (CPU-isolated: no boot, atomic PC write).
    pub fn set_pc(&mut self, pc: u16) {
        self.cpu6510.reg_pc = pc;
        self.cpu.pc = pc;
    }

    /// Refresh the legacy `cpu`/`clk` snapshot after monitor register edits.
    pub fn sync_after_monitor(&mut self) {
        self.sync_snapshot();
    }

    /// Side-effect-free banked read through the current PLA config (for
    /// session/state vectors). RAM / BASIC / KERNAL / CHARGEN / IO per memconfig;
    /// I/O reads use the register PEEK (no IRQ-latch clears), color RAM low
    /// nibble + $F0 open bus. Reads $00/$01 as the latched port.
    pub fn read_full(&self, addr: u16) -> u8 {
        match addr {
            0x0000 => self.port_dir,
            0x0001 => self.port_data,
            0x0002..=0x9fff => self.ram[addr as usize],
            0xa000..=0xbfff => {
                if self.memconfig.basic {
                    self.basic_rom[(addr as usize) - 0xa000]
                } else {
                    self.ram[addr as usize]
                }
            }
            0xc000..=0xcfff => self.ram[addr as usize],
            0xd000..=0xdfff => {
                if self.memconfig.io {
                    match addr {
                        0xd000..=0xd3ff => self.vic.read_reg(addr as u8),
                        0xd400..=0xd7ff => self.sid_regs[(addr as usize - 0xd400) & 0x1f],
                        0xd800..=0xdbff => (self.io_shadow[(addr as usize) - 0xd000] & 0x0f) | 0xf0,
                        0xdc00..=0xdcff => self.cia1.peek(addr),
                        0xdd00..=0xddff => self.cia2.peek(addr),
                        _ => self.io_shadow[(addr as usize) - 0xd000],
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
                } else {
                    self.ram[addr as usize]
                }
            }
        }
    }

    /// Current VIC bank base from CIA2 port-A bits 0-1 (= computeVicBankBase):
    /// bank = 3 - (PA & DDRA & 3); base = bank * $4000. Input pins float high, so
    /// the effective output is (PRA | ~DDRA) — but for bank selection VICE uses
    /// (pra & ddra) with the inverted-bank convention. We mirror session/state.
    pub fn vic_bank_base(&self) -> u16 {
        let pra = self.cia2.peek(0xdd00);
        let ddra = self.cia2.peek(0xdd02);
        let bank = ((pra & ddra & 0x03) ^ 0x03) as u16;
        bank.wrapping_mul(0x4000)
    }

    /// Render the current (frozen) display to the VICE PAL screenshot canvas
    /// (384×272 RGBA, colodore). Pixel-identical to the TS oracle's
    /// `renderLiteralPortRgba` for a static screen. Returns (width, height, rgba).
    pub fn render_canvas_rgba(&self) -> (usize, usize, Vec<u8>) {
        // Colour RAM low nibbles live in the IO shadow at $D800-$DBFF.
        let mut color_ram = [0u8; 0x0400];
        for (i, c) in color_ram.iter_mut().enumerate() {
            *c = self.io_shadow[0x0800 + i] & 0x0f;
        }
        let inp = render::RenderInput {
            regs: &self.vic.regs,
            ram: &self.ram,
            char_rom: &self.char_rom,
            color_ram: &color_ram,
            bank_base: self.vic_bank_base(),
        };
        render::render_canvas_rgba(&inp)
    }

    /// Run a cycle budget against an arbitrary observer (= TS session/run with a
    /// tracing sink). Instruction-stepped, identical budget semantics to
    /// `run_for`. Returns the post-run cycle count.
    pub fn run_for_with<O: Observer>(&mut self, budget: u64, obs: &mut O) -> u64 {
        self.run_for(budget, obs);
        self.clk
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
        self.load_kernal(&rom_dir.join("kernal-901227-03.bin"))?;
        self.load_basic(&rom_dir.join("basic-901226-01.bin"))?;
        self.load_chargen(&rom_dir.join("chargen-901225-01.bin"))?;
        // Full machine assembled: ROMs are also in the separate arrays now, and
        // the FullBus is available via run_for_full*.
        self.full_assembled = true;
        self.cold_reset();
        // Drive ROM: non-fatal — if absent the drive runs with zeroed ROM
        // (bus open; CPU will JAM immediately, which is a valid isolated state).
        let _ = self.drive8.load_rom(rom_dir);
        self.drive8.cold_reset();
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
    pub fn run_for_capped<O: Observer>(&mut self, budget: u64, max_instructions: u64, obs: &mut O) {
        let start = self.cpu6510.clk;
        let mut executed: u64 = 0;
        let mut bus = FlatRam { mem: &mut self.ram };
        loop {
            if self.cpu6510.clk.wrapping_sub(start) >= budget {
                break;
            }
            if executed >= max_instructions {
                break;
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
    pub fn run_for_full<O: Observer, F>(&mut self, budget: u64, obs: &mut O, on_drive_step: F)
    where
        F: FnMut(u16, u8, u8, u8, u8, u8, u64),
    {
        let max_instructions = budget.div_ceil(2) + 1000;
        self.run_for_full_capped(budget, max_instructions, obs, on_drive_step);
    }

    /// FULL-MACHINE run with an explicit instruction cap.
    pub fn run_for_full_capped<O: Observer, F>(
        &mut self,
        budget: u64,
        max_instructions: u64,
        obs: &mut O,
        mut on_drive_step: F,
    ) where
        F: FnMut(u16, u8, u8, u8, u8, u8, u64),
    {
        let start = self.cpu6510.clk;
        let mut executed: u64 = 0;
        let table = self.cia_table.clone();
        // Seed CIA clocks from the live CPU clk so timer state machines run from
        // the right rclk.
        self.cia1.clk = self.cpu6510.clk;
        self.cia2.clk = self.cpu6510.clk;
        loop {
            if self.cpu6510.clk.wrapping_sub(start) >= budget {
                break;
            }
            if executed >= max_instructions {
                break;
            }
            // Drive catches up to the current C64 clock BEFORE the instruction
            // (= integrated-session.ts:898 catchUpDrive). Advances the drive's
            // own clock via the PAL sync_factor.
            let c64_clk_before = self.cpu6510.clk;

            // Refresh cross-chip interrupt lines at the boundary: advance both
            // CIA timers to the current clk so any underflow latches its ICR flag,
            // then OR the level sources onto the CPU lines.
            self.cia1.update_to(self.cpu6510.clk, &table);
            self.cia2.update_to(self.cpu6510.clk, &table);
            let irq = self.cia1.irq_asserted() || self.vic.irq_line;
            self.cpu6510.set_irq_line(irq);
            self.cpu6510.set_nmi_line(self.cia2.irq_asserted());

            // Run a whole instruction over the FullBus (VIC ticked per cycle +
            // CIAs in lockstep + IRQ/NMI sampled at the boundary).
            {
                let mut bus = full::FullBus {
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
                    config: self.memconfig,
                    memconfig_table: &self.memconfig_table,
                    port_dir: self.port_dir,
                    port_data: self.port_data,
                    clk: self.cpu6510.clk,
                    cia2_pa_out: self.cia2_pa_out,
                    side_effects: Vec::new(),
                    read_side_effects: Vec::new(),
                    drive: &mut self.drive8,
                    iec: &mut self.iec,
                    drive_c64_ref: self.drive_c64_ref,
                };
                loop {
                    self.cpu6510.execute_cycle(&mut bus, obs);
                    if self.cpu6510.is_at_boundary() {
                        break;
                    }
                }
                // Persist bus-mutated banking/port state back to the Machine.
                self.memconfig = bus.config;
                self.port_dir = bus.port_dir;
                self.port_data = bus.port_data;
                self.cia2_pa_out = bus.cia2_pa_out;
                // Persist the push-flush reference (the drive may have been advanced
                // mid-instruction by a $DD00 access inside the FullBus).
                self.drive_c64_ref = bus.drive_c64_ref;
            }
            // Tick SID by this instruction's cycle cost — wall-clock batch tick
            // matching TS integrated-session.ts:946 `sid.tick(totalCycles)`.
            let instruction_cycles = self.cpu6510.clk.wrapping_sub(c64_clk_before);
            self.sid.tick(instruction_cycles, &self.sid_regs);

            // Drive catches up to the NEW C64 clock AFTER the instruction (= TS
            // afterCycleSync / catchUpDrive to the post-instruction clk). A $DD00
            // access already pushed it part-way; this finishes the slice. Feed the
            // live bus state in first (so the drive's PB reads see the C64 lines),
            // then re-fold the drive's PB output into the IEC core for the next
            // instruction's $DD00 reads.
            self.drive8.iec_drv_port = self.iec.drv_port;
            self.drive_c64_ref = self.drive8.catch_up_to(self.cpu6510.clk, self.drive_c64_ref);
            self.iec.drive_store_pb(self.drive8.via1_pb_iec_output());
            if let Some((pc, a, x, y, sp, p, drv_clk)) = self.drive8.sample_pc_change() {
                on_drive_step(pc, a, x, y, sp, p, drv_clk);
            }
            executed += 1;
        }
        self.sync_snapshot();
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
            // Drive advances by this C64 instruction's cycle cost, scaled by the PAL
            // sync factor.
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
