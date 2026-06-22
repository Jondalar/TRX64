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

pub use cpu::{Bus, Cpu6510};

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

    /// Run a CYCLE budget, instruction-stepped (= TS `runFor` with a cycle
    /// budget): execute whole instructions until `clk - start >= budget`. This
    /// matches the TS session/run semantics, so c64Cycles ends identically.
    pub fn run_for<O: Observer>(&mut self, budget: u64, obs: &mut O) {
        let start = self.cpu6510.clk;
        let mut bus = FlatRam { mem: &mut self.ram };
        loop {
            if self.cpu6510.clk.wrapping_sub(start) >= budget {
                break;
            }
            if self.cpu6510.is_jammed() {
                // JAM still burns cycles; run to budget so c64Cycles matches.
                self.cpu6510.execute_cycle(&mut bus, obs);
                continue;
            }
            // Step a whole instruction (one fetch boundary to the next).
            loop {
                self.cpu6510.execute_cycle(&mut bus, obs);
                if self.cpu6510.is_at_boundary() {
                    break;
                }
            }
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
