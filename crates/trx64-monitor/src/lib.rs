//! Spec 864 — the monitor, as a library.
//!
//! One implementation of the verbs, two machines to point them at. The daemon is the
//! first host; the second is the UE2 emulator, whose C64 is `trx64-core` running behind
//! an unmodified U64 firmware and which today has no way to look at its own 6510.
//!
//! The rule that shapes every file here: **this crate knows what a verb means, a host
//! knows what its world can answer.** So the crate depends on `trx64-core` and
//! `trx64-static` and nothing else — no transport, no timeline, no media, no trace
//! sinks. When a verb needs one of those it asks the host, and a host that has none
//! answers one sentence instead of failing.
//!
//! What lives here:
//!
//! - [`host`] — the trait, and every type the two hosts disagree about.
//! - `state` — the cursors, the selected device, the assemble mode, the pending prompt.
//! - `breakpoints`, `observers` — the debug policy, and the registry that IS a core
//!   observer. The host installs it for an advance and hands back what it caught.
//! - `assembler` — the one-line 6502 assembler behind `a`.
//! - [`addr_spans`] — the marked address spans (Spec 804) a reply carries, so a
//!   workbench can join symbol names onto them.
//! - `verbs` — the dispatch, the parsing, the output.
//!
//! What does NOT live here: who advances the machine. That is the one thing the two
//! hosts genuinely disagree about, and the monitor never decides it.

pub mod addr_spans;
pub mod assembler;
pub mod host;
pub mod observers;
pub mod session;
pub mod verbs;

pub use session::MonitorSession;

pub use host::{
    CpuView, Device, FlagSpec, Files, MachineEffect, MonitorHost, Reg, ResetKind, Resumption,
    RunUntil, StopInfo, Timeline, Traces,
};

/// The crate's own version of "what happened", returned by [`exec`].
#[derive(Debug, Clone)]
pub struct MonitorReply {
    /// What to show. Carries the marked address spans until a caller strips them.
    pub text: String,
    /// Set when a modal verb is waiting for the next line (`a`, `df -i`).
    pub prompt: Option<String>,
    /// What this command did to the machine — already reported to the host, carried here
    /// so a caller can log or assert on it.
    pub effect: MachineEffect,
}

/// Run one monitor command against a host.
///
/// This is the whole surface. A host owns its `MonitorSession` and calls this with the
/// line the user typed; everything else in the crate is reached through it.
pub fn exec<H: MonitorHost>(_host: &mut H, _command: &str) -> Result<MonitorReply, String> {
    // The verb table moves here from the daemon; until it does, the door exists and says
    // so rather than pretending to be finished.
    Err("the verb table has not moved yet".into())
}
