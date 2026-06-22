//! trx64-core — pure, deterministic C64 emulation.
//!
//! No I/O, no async, no socket, no trace format. The crown jewel, isolated and
//! testable against the VICE-derived TS port as Phase-1 spec
//! (C64ReverseEngineeringMCP/src/runtime/headless).
//!
//! Hot-path rule: the core takes a generic `O: Observer` (monomorphized, zero-cost
//! when unused). It NEVER calls back into another process per event.

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

/// Full mutable machine state (~75 KiB headless).
///
/// `Clone` is intentional and load-bearing: a clone is the cheap COW fork base for
/// Phase-2 parallel mutation search (`explore()`), thousands of branches feasible.
#[derive(Clone)]
pub struct Machine {
    pub ram: Box<[u8; 0x10000]>,
    /// Monotonic cycle counter (CLOCK, never wraps — per Spec 743).
    pub clk: u64,
    // TODO(loop): cpu (6510), vic-ii (literal port), cia1, cia2, sid, via x2, iec, drive1541.
    // Ported per loop/backlog.md, each verified cycle-exact via trace-diff.
}

impl Machine {
    pub fn new() -> Self {
        Self {
            ram: Box::new([0u8; 0x10000]),
            clk: 0,
        }
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
