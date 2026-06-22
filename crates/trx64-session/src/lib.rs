//! trx64-session — instance lifecycle.
//!
//! Boot-paused, idle-safe, opChain-serialized mutations, media mount, snapshot ring /
//! rewind tree. Phase-2 home of warp + parallel `explore()` over COW machine forks.

use trx64_core::Machine;

/// One session = one machine instance. Long-lived, outlives MCP reconnects.
pub struct Session {
    pub id: String,
    pub machine: Machine,
    /// Sessions boot PAUSED — no autonomous tick loop (idle-safe, Spec 744.3).
    pub running: bool,
    /// Active trace: sibling `.c64retrace` path + accumulated meta. When set,
    /// session/run streams CpuStep/RAM_WRITE/IO_WRITE frames into a FrameSink and
    /// flushes to this path. `None` = no trace.
    pub trace: Option<TraceState>,
}

/// Trace bookkeeping for an active `.c64retrace` capture.
pub struct TraceState {
    /// Absolute `.c64retrace` path (sibling of the `.duckdb` outputPath).
    pub retrace_path: std::path::PathBuf,
    /// JSON meta string embedded in the file header.
    pub meta_json: String,
    /// Cycle at which the trace started (= TS cycleStart).
    pub cycle_start: u64,
    /// Accumulated frame buffer (header + events), flushed at trace/run/stop.
    pub buf: Vec<u8>,
    /// runId for status replies.
    pub run_id: String,
    pub event_count: u64,
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            machine: Machine::new(),
            running: false,
            trace: None,
        }
    }

    /// Boot the session: load ROMs from `rom_dir` and cold-reset the machine.
    pub fn boot(&mut self, rom_dir: &std::path::Path) -> Result<(), trx64_core::RomError> {
        self.machine.boot_from_dir(rom_dir)
    }
}

/// Phase-2 mutation-search primitive (sketch — built after Phase-1 parity is green).
///
/// COW-fork `base` per overlay, warp `run_for(budget)` with probes, stream compact
/// verdicts back. The reason TRX64 exists; Node's single-thread loop can't do this.
pub struct Overlay {
    /// (addr, bytes) patches = coder overlay / crack applied to a forked machine.
    pub patches: Vec<(u16, Vec<u8>)>,
}
