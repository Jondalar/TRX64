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
    /// True once a CPU-isolated `monitor/exec` inject (`wr` / `r pc=`) has run.
    /// Distinguishes the CPU/chip-ISOLATED gates (which inject a program + set PC,
    /// then run on FlatRam/CiaBus/VicBus) from the FULL-MACHINE boot scenarios
    /// (session/create → session/run straight from the KERNAL reset vector, run
    /// on the assembled FullBus). False at boot ⇒ full-machine run path.
    pub injected: bool,
    /// True once a `wr io` (I/O-lens) inject has run — i.e. a render scenario that
    /// programmed the VIC/colour-RAM via `Machine::poke_io` and then runs a parked
    /// frame to SWEEP the per-cycle renderer. Unlike `injected` (which routes the
    /// run onto the chip-ISOLATED bus for cycle-exact CPU gates), an io-inject
    /// still needs the FULL VIC-ticked machine so the per-cycle draw accumulates
    /// the displayed frame. So: io_injected ⇒ keep the full-machine run path.
    pub io_injected: bool,
    /// Active trace: sibling `.c64retrace` path + accumulated meta. When set,
    /// session/run streams CpuStep/RAM_WRITE/IO_WRITE frames into a FrameSink and
    /// flushes to this path. `None` = no trace.
    pub trace: Option<TraceState>,
    pub disk_path: String,
    /// BUG-023-cart / Spec 742 — the host `.crt` path of the live cartridge (= the
    /// c64re `session.cartPath`), so a writable (EasyFlash) cart can write its
    /// programmed flash back on eject/persist. Empty when no cart or uploaded bytes
    /// with no backing path. Set by the CRT media/ingress path; cleared on eject.
    pub cart_path: String,
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
    /// Domains requested for this trace (= TS trace/start_domains domains). The
    /// daemon maps these → channels to filter which records are emitted, and
    /// whether the VIC must be ticked (vic domain → VIC-isolated run path).
    pub domains: Vec<String>,
    /// T2.6 — manual phase markers: (cpu_clk, label). Matches TS `run.marks[]`
    /// (`trace-run.ts` ActiveRun.run.marks + markCount).
    pub marks: Vec<(u64, String)>,
    /// ws-trace-monitor-misc-23 — the real trace definition id (= TS RuntimeTraceRun
    /// .definitionId = def.id). For a `trace/start_domains` (captureAll) trace this is
    /// "live-capture"; for a `trace/run/start` of a registered definition it is THAT
    /// definition's id. finalize_trace echoes it (NOT a hardcoded "live-capture").
    pub definition_id: String,
    /// The definition's version (= TS RuntimeTraceRun.definitionVersion).
    pub definition_version: i64,
    /// Wall-clock ms at trace start (= TS ActiveRun.startWall = Date.now()); used to
    /// compute `overheadMs` at stop (= TS run.overheadMs = Date.now() - startWall).
    pub start_wall_ms: u128,
    /// The mounted-media identity SHA captured at start (= TS run.media.sha256), or
    /// empty when no disk was attached. Echoed in the stop descriptor's `media`.
    pub media_sha: String,
    /// The mounted-media source name (basename), echoed in the stop descriptor's
    /// `media` (= TS run.media.sourceName). Empty when none.
    pub media_name: String,
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            machine: Machine::new(),
            running: false,
            injected: false,
            io_injected: false,
            trace: None,
            disk_path: String::new(),
            cart_path: String::new(),
        }
    }

    /// Boot the session: load ROMs from `rom_dir` and cold-reset the machine.
    pub fn boot(&mut self, rom_dir: &std::path::Path) -> Result<(), trx64_core::RomError> {
        self.machine.boot_from_dir(rom_dir)
    }

    /// Native fast snapshot: clone the Machine (ADR-002).
    /// Zero-copy for Phase-2 COW forks.
    pub fn take_snapshot(&self) -> Machine {
        self.machine.clone()
    }

    /// Restore from a native snapshot.
    pub fn restore_snapshot(&mut self, snap: Machine) {
        self.machine = snap;
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
