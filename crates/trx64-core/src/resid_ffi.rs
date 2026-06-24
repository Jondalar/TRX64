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
//! GPL note: the linked reSID is GPL-2.0-or-later (Dag Lem); the shim is
//! GPL-3.0-or-later. See vendor/resid/PROVENANCE.md.

use std::os::raw::{c_double, c_int};
use std::sync::Mutex;

/// The shim is a SINGLE module-global `reSID::SID`. This process-wide guard
/// serializes the (cheap) construct/configure window so concurrent `Resid::new`
/// calls — e.g. parallel test threads — cannot interleave the global's
/// reinit/configure with another engine's emit. The runtime drives exactly one
/// SID, so this is a safety net, not a hot-path cost.
static RESID_GUARD: Mutex<()> = Mutex::new(());

// ---- shim ABI (vendor/resid/resid_shim.cc) ----------------------------------
// All operate on the shim's single module-global `reSID::SID` instance (the
// runtime drives exactly one SID), matching the WASM build's single-instance
// model. NOT re-entrant / NOT thread-safe across instances — fine: one SID.
extern "C" {
    fn resid_reinit();
    fn resid_set_chip_model(model: c_int);
    fn resid_set_voice_mask(mask: c_int);
    fn resid_enable_filter(enable: c_int);
    fn resid_adjust_filter_bias(bias: c_double);
    fn resid_enable_external_filter(enable: c_int);
    // resid_reset() exists in the shim but TRX64 uses resid_reinit() (full
    // re-construct) for a pristine, c64re-module-identical reset.
    fn resid_set_sampling(
        clock_freq: c_double,
        sample_freq: c_double,
        method: c_int,
        passband: c_double,
        gain: c_double,
    ) -> c_int;
    fn resid_write(reg: c_int, value: c_int);
    fn resid_read(reg: c_int) -> c_int;
    fn resid_clock(delta: c_int, buf: *mut i16, max_samples: c_int) -> c_int;
    fn resid_clock_remaining() -> c_int;
    fn resid_clock_silent(delta: c_int);
    fn resid_output() -> c_int;
    fn resid_state_size() -> c_int;
    fn resid_read_state(buf: *mut u8);
    fn resid_write_state(buf: *const u8);
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

/// Safe handle to the reSID audio engine (the FFI'd GPL reSID C++).
///
/// NOTE: the shim is a SINGLE module-global SID. `Resid` is therefore an
/// EXCLUSIVE handle: it holds `RESID_GUARD` for its whole lifetime, so at most
/// one `Resid` touches the global C++ SID at a time. A second `Resid::new`
/// blocks until the first is dropped. The runtime drives exactly one SID, so
/// this matches the design; the guard also makes parallel tests safe.
pub struct Resid {
    cfg: ResidConfig,
    /// Fractional sample-cadence remainder (kept for parity with TS; reSID owns
    /// the authoritative `sample_offset` internally).
    cycle_acc: f64,
    /// Exclusive ownership of the module-global SID (held for the lifetime).
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Resid {
    /// Create + configure the engine in VICE's exact post-reset order
    /// (sid/resid.cc): set_chip_model → set_voice_mask(0x07) → enable_filter →
    /// adjust_filter_bias → enable_external_filter → set_sampling.
    ///
    /// Acquires `RESID_GUARD` (blocks if another `Resid` is alive — the shim has
    /// one global SID).
    pub fn new(cfg: ResidConfig) -> Self {
        let guard = RESID_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let r = Self { cfg, cycle_acc: 0.0, _guard: guard };
        r.configure();
        r
    }

    /// Convenience: default config (6581, filter OFF, RESAMPLE @ 44.1k/PAL).
    pub fn new_default() -> Self {
        Self::new(ResidConfig::default())
    }

    fn configure(&self) {
        // SAFETY: plain FFI to the shim's global SID; values are validated by
        // the shim (masked / range-checked). We hold RESID_GUARD (exclusive).
        unsafe {
            // FULL re-construct (not just reset()): reSID::reset() leaves the
            // resampler FIR ring dirty across runs, whereas c64re instantiates a
            // FRESH WASM module each time. resid_reinit() reproduces that
            // pristine-module state → byte-identical to c64re's reference.
            resid_reinit();
            resid_set_chip_model(self.cfg.model);
            resid_set_voice_mask(self.cfg.voice_mask);
            resid_enable_filter(self.cfg.filter as c_int);
            resid_adjust_filter_bias(self.cfg.filter_bias);
            resid_enable_external_filter(self.cfg.external_filter as c_int);
            resid_set_sampling(
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
        unsafe { resid_write(reg as c_int, value as c_int) }
    }

    /// Read a SID register through reSID (the register/readback authority in
    /// TRX64 is `sid.rs`; this exists for completeness / oracle parity).
    #[inline]
    pub fn read(&self, reg: u8) -> u8 {
        unsafe { resid_read(reg as c_int) as u8 }
    }

    /// Current 16-bit AUDIO OUT (post external filter).
    #[inline]
    pub fn output(&self) -> i16 {
        unsafe { resid_output() as i16 }
    }

    /// Advance `cycles` Φ2 cycles WITHOUT producing samples (SID state still
    /// ages — for muted-but-live paths).
    pub fn clock_silent(&mut self, cycles: u32) {
        unsafe { resid_clock_silent(cycles as c_int) }
    }

    /// Emit signed 16-bit mono samples for `cycles` Φ2 cycles. Verbatim port of
    /// the TS `ResidWasm.emit()` loop: consume the FULL delta and return exactly
    /// the samples reSID produces (reSID owns fractional sample timing, so we
    /// must NOT pre-estimate / cap, or it lags cumulatively → pitch drift).
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
            let n = unsafe { resid_clock(dt, buf.as_mut_ptr(), MAX_SAMPLES_PER_CALL as c_int) };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
            }
            let rem = unsafe { resid_clock_remaining() };
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
        unsafe { resid_read_state(buf.as_mut_ptr()) };
        buf
    }

    /// Restore a synthesis-state blob captured by [`Resid::capture_state`].
    pub fn restore_state(&mut self, bytes: &[u8]) {
        let n = self.state_size();
        assert_eq!(bytes.len(), n, "reSID state size mismatch: {} != {}", bytes.len(), n);
        unsafe { resid_write_state(bytes.as_ptr()) };
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
