//! trx64-core — pure, deterministic C64 emulation.
//!
//! No I/O, no async, no socket, no trace format. The crown jewel, isolated and
//! testable against the VICE-derived TS port as Phase-1 spec
//! (C64ReverseEngineeringMCP/src/runtime/headless).
//!
//! Hot-path rule: the core takes a generic `O: Observer` (monomorphized, zero-cost
//! when unused). It NEVER calls back into another process per event.

use std::path::Path;

/// Zero-cost observation hook, inlined into the core step loop.
///
/// Three faces, one mechanism:
/// - [`NullSink`] — tracing off; the compiler eliminates the hooks entirely.
/// - `FrameSink` (in `trx64-trace`) — forensic firehose → `.c64retrace`.
/// - `ProbeSet` (Phase 2) — mutation-search verdicts, no firehose.
pub trait Observer {
    fn on_instruction(&mut self, pc: u16, opcode: u8, a: u8, x: u8, y: u8, sp: u8, p: u8, clk: u64);
    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8);
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
    fn on_instruction(&mut self, _: u16, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u64) {}
    #[inline(always)]
    fn on_bus(&mut self, _: BusKind, _: u16, _: u8) {}
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

/// Full mutable machine state (~75 KiB headless).
///
/// `Clone` is intentional and load-bearing: a clone is the cheap COW fork base for
/// Phase-2 parallel mutation search (`explore()`), thousands of branches feasible.
#[derive(Clone)]
pub struct Machine {
    pub ram: Box<[u8; 0x10000]>,
    /// Monotonic cycle counter (CLOCK, never wraps — per Spec 743).
    pub clk: u64,
    /// CPU registers.
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
            cpu: Cpu::default(),
        }
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
        self.cpu.pc = lo | (hi << 8);
        self.cpu.a = 0;
        self.cpu.x = 0;
        self.cpu.y = 0;
        self.cpu.sp = 0xFF;
        // Interrupt disable set on reset (bit 2 = I flag).
        self.cpu.p = 0x04;
        self.cpu.cycles = 0;
        self.clk = 0;
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

    /// Execute one CPU clock cycle. The CPU is the clock master; `tick()`/CLK_INC
    /// advances VIC/CIA per cycle (drive is lazy catch-up at IEC edges).
    pub fn step_cycle<O: Observer>(&mut self, _obs: &mut O) {
        // TODO(loop): port Cpu65xxVice.executeCycle + tick + c64ViciiCycle.
        unimplemented!("step_cycle ported by cpu/vic builders")
    }

    /// Run up to `budget` cycles. Warp = native speed, no throttle.
    pub fn run_for<O: Observer>(&mut self, budget: u64, obs: &mut O) {
        let end = self.clk.saturating_add(budget);
        while self.clk < end {
            self.step_cycle(obs);
        }
    }
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}
