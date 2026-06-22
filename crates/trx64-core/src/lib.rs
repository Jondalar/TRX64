//! trx64-core — pure, deterministic C64 emulation.
//!
//! No I/O, no async, no socket, no trace format. The crown jewel, isolated and
//! testable against the VICE-derived TS port as Phase-1 spec
//! (C64ReverseEngineeringMCP/src/runtime/headless).
//!
//! Hot-path rule: the core takes a generic `O: Observer` (monomorphized, zero-cost
//! when unused). It NEVER calls back into another process per event.

use std::path::Path;

pub mod cpu;
pub mod tables;
pub mod vic;

pub use cpu::{Bus, Cpu6510};
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

    /// Load 8 KiB KERNAL ROM into $E000-$FFFF.
    pub fn load_kernal(&mut self, path: &Path) -> Result<(), RomError> {
        let data = std::fs::read(path)?;
        if data.len() != 0x2000 {
            return Err(RomError::BadSize(data.len(), 0x2000));
        }
        self.ram[0xE000..=0xFFFF].copy_from_slice(&data);
        Ok(())
    }

    /// Load 8 KiB BASIC ROM into $A000-$BFFF.
    pub fn load_basic(&mut self, path: &Path) -> Result<(), RomError> {
        let data = std::fs::read(path)?;
        if data.len() != 0x2000 {
            return Err(RomError::BadSize(data.len(), 0x2000));
        }
        self.ram[0xA000..=0xBFFF].copy_from_slice(&data);
        Ok(())
    }

    /// Load 4 KiB CHARGEN ROM. Stored separately; not mapped into the flat RAM
    /// array for now (character ROM is banked out of CPU space). We keep it for
    /// future VIC-II reads.
    pub fn load_chargen(&mut self, path: &Path) -> Result<(), RomError> {
        let data = std::fs::read(path)?;
        if data.len() != 0x1000 {
            return Err(RomError::BadSize(data.len(), 0x1000));
        }
        // Chargen is not in the CPU flat map; nothing to copy for now.
        let _ = data;
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
        // VICE power-on default P also sets I (bit 2). reset_to leaves P=$20;
        // set I to match a cold reset boundary (the isolated gate disables IRQs).
        self.cpu6510.reg_p |= 0x04;
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

    /// Run a cycle budget against an arbitrary observer (= TS session/run with a
    /// tracing sink). Instruction-stepped, identical budget semantics to
    /// `run_for`. Returns the post-run cycle count.
    pub fn run_for_with<O: Observer>(&mut self, budget: u64, obs: &mut O) -> u64 {
        self.run_for(budget, obs);
        self.clk
    }

    /// Load all three standard C64 ROMs from `rom_dir` and perform a cold reset.
    ///
    /// Expected filenames (matching the bundled ROMs):
    ///   kernal-901227-03.bin, basic-901226-01.bin, chargen-901225-01.bin
    pub fn boot_from_dir(&mut self, rom_dir: &Path) -> Result<(), RomError> {
        // Power-on DRAM fill FIRST, then ROM loads overwrite their windows.
        self.fill_power_on_ram();
        self.load_kernal(&rom_dir.join("kernal-901227-03.bin"))?;
        self.load_basic(&rom_dir.join("basic-901226-01.bin"))?;
        self.load_chargen(&rom_dir.join("chargen-901225-01.bin"))?;
        self.cold_reset();
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
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}
