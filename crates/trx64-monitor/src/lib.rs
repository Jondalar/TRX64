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

/// The crate's own version of "what happened".
///
/// Not yet returned by anything: while the move is half done, [`verbs::try_exec`]
/// answers with a plain `Option<Result<String, String>>` so a host can tell "not my
/// verb" from "here is your text", and the prompt is read off the session. This is the
/// shape the single door takes once the last verb has crossed.
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

/// Run one monitor command against a host: [`verbs::try_exec`].
///
/// A host owns its [`MonitorSession`] and calls that with the line the user typed. It
/// answers `None` for a verb this crate does not own yet, and the host's own dispatch
/// takes it — the honest shape while the extraction is in progress, and the shape §6
/// keeps afterwards for a host's own verbs.
pub use verbs::try_exec as exec;
