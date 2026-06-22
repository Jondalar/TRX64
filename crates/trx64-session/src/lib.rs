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
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            machine: Machine::new(),
            running: false,
        }
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
