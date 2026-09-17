//! resid_ffi.rs — safe Rust wrapper over the vendored GPL reSID C++ engine.
//!
//! FFIs the SAME flat-C shim c64re exports to WASM
//! (vendor/resid/resid_shim.cc, built by build.rs). Because both sides compile
//! the identical reSID source through the identical shim ABI, TRX64's PCM is
//! byte-identical to c64re's reSID for an identical register-write / cycle
//! sequence (proved by tests/resid_oracle.rs).
//!
//! This is the AUDIO tier — decoupled from the per-instruction fastsid register
//! engine in `sid.rs` (which stays the byte-exact trace/readback authority).
//! reSID owns sample timing internally (sample_offset), so `emit()` consumes the
//! full cycle delta and returns exactly the samples reSID produces — mirroring
//! the TS `ResidWasm.emit()` loop 1:1.
//!
//! INSTANCES (Spec 855). Each [`Resid`] owns its own C++ engine, created by
//! `resid_new()` and destroyed on drop. Until 855 the shim held one module-
//! global SID behind a process-wide mutex, so a second `Resid::new` did not run
//! slowly — it DEADLOCKED. That was our restriction, never reSID's: `reSID::SID`
//! is an ordinary class and VICE builds one per chip (sid/resid.cc:94).
//!
//! Two instances with the same [`ResidConfig`], fed the same cycle deltas, stay
//! sample-aligned by construction: reSID's cadence comes from
//! `cycles_per_sample`, computed once in `set_sampling_parameters` from
//! clock/sample frequency alone (sid.cc:982). It does not depend on what the
//! chip is playing, so their sample COUNTS cannot drift apart.
//!
//! `Resid` is `!Send` and `!Sync` (it holds a raw pointer to C++ state): an
//! engine belongs to the thread that made it.
//!
//! GPL note: the linked reSID is GPL-2.0-or-later (Dag Lem); the shim is
//! GPL-3.0-or-later. See vendor/resid/PROVENANCE.md.

use std::os::raw::{c_double, c_int, c_void};
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide count of `Resid::new` calls (= reSID engine constructions).
/// Used by the audio-path tests to ASSERT the persistent-engine contract: the
/// FFI `audioDrain()` render thread must construct the engine ONCE, not once per
/// drain (the per-drain reconstruct was the ~60 Hz hum). Monotonic; never reset.
pub static RESID_CONSTRUCT_COUNT: AtomicU64 = AtomicU64::new(0);

/// Read the process-wide reSID construction count (see [`RESID_CONSTRUCT_COUNT`]).
pub fn resid_construct_count() -> u64 {
    RESID_CONSTRUCT_COUNT.load(Ordering::SeqCst)
}

// ---- shim ABI (vendor/resid/resid_shim.cc) ----------------------------------
// Every call names the instance it acts on. The shim also exports the legacy
// handle-free names (they act on its default instance) for c64re's WASM build;
// TRX64 does not use them, so a Rust-side mistake cannot silently land on a
// shared engine.
extern "C" {
    fn resid_new() -> *mut c_void;
    fn resid_delete(handle: *mut c_void);
    fn resid_reinit_h(handle: *mut c_void);
    fn resid_set_chip_model_h(handle: *mut c_void, model: c_int);
    fn resid_set_voice_mask_h(handle: *mut c_void, mask: c_int);
    fn resid_enable_filter_h(handle: *mut c_void, enable: c_int);
    fn resid_adjust_filter_bias_h(handle: *mut c_void, bias: c_double);
    fn resid_enable_external_filter_h(handle: *mut c_void, enable: c_int);
    // resid_reset_h() exists in the shim but TRX64 uses resid_reinit_h() (full
    // re-construct) for a pristine, c64re-module-identical reset.
    fn resid_set_sampling_h(
        handle: *mut c_void,
        clock_freq: c_double,
        sample_freq: c_double,
        method: c_int,
        passband: c_double,
        gain: c_double,
    ) -> c_int;
    fn resid_write_h(handle: *mut c_void, reg: c_int, value: c_int);
    fn resid_read_h(handle: *mut c_void, reg: c_int) -> c_int;
    fn resid_clock_h(handle: *mut c_void, delta: c_int, buf: *mut i16, max_samples: c_int) -> c_int;
    fn resid_clock_remaining_h(handle: *mut c_void) -> c_int;
    fn resid_clock_silent_h(handle: *mut c_void, delta: c_int);
    fn resid_output_h(handle: *mut c_void) -> c_int;
    /// No handle: `sizeof(SID::State)` is a property of the type.
    fn resid_state_size() -> c_int;
    fn resid_read_state_h(handle: *mut c_void, buf: *mut u8);
    fn resid_write_state_h(handle: *mut c_void, buf: *const u8);
}

/// reSID `sampling_method` (siddefs.h): FAST=0, INTERPOLATE=1, RESAMPLE=2,
/// RESAMPLE_FASTMEM=3.
pub const SAMPLE_FAST: i32 = 0;
pub const SAMPLE_INTERPOLATE: i32 = 1;
pub const SAMPLE_RESAMPLE: i32 = 2;
pub const SAMPLE_RESAMPLE_FASTMEM: i32 = 3;

/// reSID `chip_model`: 0 = MOS6581, 1 = MOS8580.
pub const MODEL_6581: i32 = 0;
pub const MODEL_8580: i32 = 1;

/// PAL Φ2 clock (Hz) — matches TS `PAL_CLOCK_FREQ`.
pub const PAL_CLOCK_FREQ: f64 = 985248.0;
/// NTSC Φ2 clock (Hz) — matches TS `NTSC_CLOCK_FREQ`.
pub const NTSC_CLOCK_FREQ: f64 = 1022730.0;
/// Default sample rate — matches TS `DEFAULT_SAMPLE_RATE`.
pub const DEFAULT_SAMPLE_RATE: f64 = 44100.0;

/// Max samples per inner `resid_clock` call (the emit loop re-issues until the
/// cycle delta is consumed). Mirrors TS `MAX_SAMPLES_PER_CALL`.
const MAX_SAMPLES_PER_CALL: usize = 4096;

/// Configuration for the reSID audio engine. Defaults match the TS `ResidWasm`
/// engine (`resid-wasm-engine.ts` configure()): 6581, filter OFF (BUG-049
/// follow-on — the 6581 filter sounded wrong in A/B), RESAMPLE @ 44.1k/PAL.
#[derive(Clone, Copy, Debug)]
pub struct ResidConfig {
    pub model: i32,
    pub clock_freq: f64,
    pub sample_rate: f64,
    pub sampling_method: i32,
    /// SID analog filter on/off. Default OFF (matches `ResidWasm`).
    pub filter: bool,
    /// 6581 filter DC bias in volts (VICE `SidResidFilterBias`/1000). 6581=0.5.
    pub filter_bias: f64,
    /// Output RC stage (VICE enables it with the filter; reSID defaults on).
    pub external_filter: bool,
    /// Per-voice enable bitmask (VICE inits to 0x07 = all three voices).
    pub voice_mask: i32,
    /// Resampler passband Hz; <=0 → reSID default. VICE: sample*90/200.
    pub passband: f64,
    /// Output gain (VICE `SidResidGain`/100 = 0.97).
    pub gain: f64,
}

impl Default for ResidConfig {
    fn default() -> Self {
        let sample_rate = DEFAULT_SAMPLE_RATE;
        Self {
            model: MODEL_6581,
            clock_freq: PAL_CLOCK_FREQ,
            sample_rate,
            sampling_method: SAMPLE_RESAMPLE,
            filter: false,
            filter_bias: 0.5,
            external_filter: true,
            voice_mask: 0x07,
            passband: (sample_rate * 90.0) / 200.0,
            gain: 0.97,
        }
    }
}

/// Safe handle to one reSID audio engine (the FFI'd GPL reSID C++).
///
/// Owns its C++ instance: `resid_new()` on construction, `resid_delete()` on
/// drop. Several may exist at once and they share nothing — which is the whole
/// of Spec 855's core change.
pub struct Resid {
    cfg: ResidConfig,
    /// Fractional sample-cadence remainder (kept for parity with TS; reSID owns
    /// the authoritative `sample_offset` internally).
    cycle_acc: f64,
    /// The C++ engine. Never null between `new` and `drop`.
    handle: *mut c_void,
}

impl Resid {
    /// Create + configure the engine in VICE's exact post-reset order
    /// (sid/resid.cc): set_chip_model → set_voice_mask(0x07) → enable_filter →
    /// adjust_filter_bias → enable_external_filter → set_sampling.
    ///
    /// Never blocks: each call builds its own engine.
    pub fn new(cfg: ResidConfig) -> Self {
        RESID_CONSTRUCT_COUNT.fetch_add(1, Ordering::SeqCst);
        // SAFETY: the shim allocates a fresh context and returns it; we own it
        // until `drop` hands it back to `resid_delete`.
        let handle = unsafe { resid_new() };
        assert!(!handle.is_null(), "resid_new returned null");
        let r = Self { cfg, cycle_acc: 0.0, handle };
        r.configure();
        r
    }

    /// Convenience: default config (6581, filter OFF, RESAMPLE @ 44.1k/PAL).
    pub fn new_default() -> Self {
        Self::new(ResidConfig::default())
    }

    fn configure(&self) {
        // SAFETY: plain FFI against our own instance; values are validated by
        // the shim (masked / range-checked).
        unsafe {
            // FULL re-construct (not just reset()): reSID::reset() leaves the
            // resampler FIR ring dirty across runs, whereas c64re instantiates a
            // FRESH WASM module each time. resid_reinit_h() reproduces that
            // pristine-module state → byte-identical to c64re's reference.
            resid_reinit_h(self.handle);
            resid_set_chip_model_h(self.handle, self.cfg.model);
            resid_set_voice_mask_h(self.handle, self.cfg.voice_mask);
            resid_enable_filter_h(self.handle, self.cfg.filter as c_int);
            resid_adjust_filter_bias_h(self.handle, self.cfg.filter_bias);
            resid_enable_external_filter_h(self.handle, self.cfg.external_filter as c_int);
            resid_set_sampling_h(
                self.handle,
                self.cfg.clock_freq,
                self.cfg.sample_rate,
                self.cfg.sampling_method,
                self.cfg.passband,
                self.cfg.gain,
            );
        }
    }

    /// Reset the engine + re-apply config (mirrors `ResidWasm.reset`).
    pub fn reset(&mut self) {
        self.cycle_acc = 0.0;
        self.configure();
    }

    /// Write a SID register ($D4xx offset, masked to 0x00..0x1f by the shim).
    #[inline]
    pub fn write(&mut self, reg: u8, value: u8) {
        unsafe { resid_write_h(self.handle, reg as c_int, value as c_int) }
    }

    /// Read a SID register through reSID (the register/readback authority in
    /// TRX64 is `sid.rs`; this exists for completeness / oracle parity).
    #[inline]
    pub fn read(&self, reg: u8) -> u8 {
        unsafe { resid_read_h(self.handle, reg as c_int) as u8 }
    }

    /// Current 16-bit AUDIO OUT (post external filter).
    #[inline]
    pub fn output(&self) -> i16 {
        unsafe { resid_output_h(self.handle) as i16 }
    }

    /// Advance `cycles` Φ2 cycles WITHOUT producing samples (SID state still
    /// ages — for muted-but-live paths).
    pub fn clock_silent(&mut self, cycles: u32) {
        unsafe { resid_clock_silent_h(self.handle, cycles as c_int) }
    }

    /// Emit signed 16-bit mono samples for `cycles` Φ2 cycles. Verbatim port of
    /// the TS `ResidWasm.emit()` loop: consume the FULL delta and return exactly
    /// the samples reSID produces (reSID owns fractional sample timing, so we
    /// must NOT pre-estimate / cap, or it lags cumulatively → pitch drift).
    ///
    /// NOTE for multi-chip callers: that caution is about capping ONE engine's
    /// delta. It says nothing about two engines diverging — fed the same deltas
    /// they produce the same counts (see the module docs).
    pub fn emit(&mut self, cycles: u32) -> Vec<i16> {
        if cycles == 0 {
            return Vec::new();
        }
        let mut out: Vec<i16> = Vec::new();
        let mut buf = [0i16; MAX_SAMPLES_PER_CALL];
        let mut dt = cycles as i32;
        let mut guard: u32 = 0;
        while dt > 0 && guard < (1 << 20) {
            guard += 1;
            // SAFETY: buf is a valid MAX_SAMPLES_PER_CALL i16 slab; the shim
            // writes at most max_samples into it and returns the count.
            let n = unsafe {
                resid_clock_h(self.handle, dt, buf.as_mut_ptr(), MAX_SAMPLES_PER_CALL as c_int)
            };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
            }
            let rem = unsafe { resid_clock_remaining_h(self.handle) };
            if n == 0 && rem >= dt {
                break; // no progress — avoid spin
            }
            dt = rem;
        }
        out
    }

    /// Size in bytes of reSID's synthesis-state blob (= `reSID::SID::State`).
    pub fn state_size(&self) -> usize {
        unsafe { resid_state_size() as usize }
    }

    /// Capture reSID's FULL synthesis state (= VICE's `sid_snapshot_state_t`):
    /// accumulators, shift registers, envelope/rate counters, pipelines, regs.
    pub fn capture_state(&self) -> Vec<u8> {
        let n = self.state_size();
        let mut buf = vec![0u8; n];
        unsafe { resid_read_state_h(self.handle, buf.as_mut_ptr()) };
        buf
    }

    /// Restore a synthesis-state blob captured by [`Resid::capture_state`].
    pub fn restore_state(&mut self, bytes: &[u8]) {
        let n = self.state_size();
        assert_eq!(bytes.len(), n, "reSID state size mismatch: {} != {}", bytes.len(), n);
        unsafe { resid_write_state_h(self.handle, bytes.as_ptr()) };
    }

    /// Engine config (read-only).
    pub fn config(&self) -> &ResidConfig {
        &self.cfg
    }

    /// Fractional cycle remainder (parity-only; reSID owns the real offset).
    pub fn cycle_accumulator(&self) -> f64 {
        self.cycle_acc
    }
}

impl Drop for Resid {
    fn drop(&mut self) {
        // SAFETY: `handle` came from `resid_new` and is dropped exactly once.
        unsafe { resid_delete(self.handle) }
    }
}
