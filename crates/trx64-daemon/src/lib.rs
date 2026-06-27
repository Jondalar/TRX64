//! trx64-daemon (library face).
//!
//! The daemon is, and stays, a `[[bin]]` (`src/main.rs`) — the WS JSON-RPC server.
//! This `[lib]` target exposes the SAME compilation unit as a linkable library so
//! the in-process FFI (`trx64-ffi`) can call the daemon's request-dispatch
//! synchronously, socket-free, against the SAME `dispatch()` + `NotifyHub` the
//! socket transport uses. There is NO second runtime path: the embed reuses the
//! exact handlers, so the typed Swift bindings cannot drift from the wire contract.
//!
//! Additive only: this file includes `main.rs` as a module and re-exports the items
//! the embed needs. `main.rs` is unchanged apart from `pub`-widening a handful of
//! types/functions and extracting the (previously inline) `State` initializer into
//! `build_state` so the binary, the FFI, and the tests all build an identical State.
//!
//! The `fn main` carried in `main.rs` is inert here (it is only the binary's entry
//! point); the lib never calls it.

#[path = "main.rs"]
mod daemon;

// The embed surface consumed by `trx64-ffi`.
pub use daemon::{
    build_state, create_embedded_state, dispatch, notify_hub, Request, Response, RpcError,
    SharedState, State,
};

// Live A/V PULL-API (ADR-073 §pull) — the two direct-core pull entry points + their
// plain (Send) data structs, consumed by the FFI's typed `frameBuffer()` /
// `audioDrain()`. Additive; these bypass `dispatch` because A/V is binary.
pub use daemon::{
    pull_audio_drain, pull_frame_buffer, AudioDrainData, FrameBufferData, AUDIO_SAMPLE_RATE,
};

// The event-broadcast hub (subscribe a forwarder channel → typed events).
pub use daemon::streaming;

/// Process-wide reSID construction count (see `trx64_core::resid_ffi`). Re-exported so
/// the FFI audio-path tests can ASSERT the persistent-engine contract — the
/// `audioDrain()` render thread constructs reSID ONCE, not per drain (the per-drain
/// reconstruct was the ~60 Hz hum). Test-facing only.
pub fn resid_construct_count() -> u64 {
    trx64_core::resid_ffi::resid_construct_count()
}

// `main.rs`'s submodules (streaming.rs, project_knowledge.rs) reference a handful of
// items by the CRATE-ROOT path (`crate::stream_*`, `crate::now_iso8601_utc`). In the
// `[[bin]]` those live at the crate root (main.rs IS the root); in this `[lib]` they
// live inside the `daemon` module. Re-export them at the lib root so `crate::X`
// resolves identically in both compilation contexts. Crate-internal only — NOT part
// of the public FFI surface.
pub(crate) use daemon::{
    now_iso8601_utc, stream_debug_gated_advance, stream_maybe_autocapture,
    stream_maybe_autopersist_cart, stream_maybe_autopersist_disk, stream_maybe_feed_recorder,
};
