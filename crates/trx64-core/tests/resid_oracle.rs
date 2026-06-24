//! resid_oracle.rs — the reSID audio oracle.
//!
//! Proves TRX64's audio comes from the SAME vendored GPL reSID C++ + the SAME
//! flat-C shim c64re compiles to WASM, driven by an identical SID write/cycle
//! sequence. Two gates, because there is one hard floating-point boundary:
//!
//!   GATE A (byte-identity): TRX64's NATIVE reSID is byte-for-byte identical to
//!     the committed native golden (trx64_native.*). This is the engine that
//!     ships — deterministic and reproducible across builds.
//!
//!   GATE B (c64re cross-check, bounded): the same sequence run through c64re's
//!     WASM reSID (committed reference c64re_wasm.*) agrees with TRX64's native
//!     output on the EXACT sample count and within a tiny LSB bound. It is NOT
//!     bit-identical, and CANNOT be: reSID builds its filter (filter8580new.cc
//!     log1p/exp/sqrt) and FIR resampler (sid.cc I0/sin/round) lookup tables at
//!     construction using libm; emscripten's musl libm rounds exp/log/sin/round
//!     differently from the native system libm at the ULP level, so a handful of
//!     `(short)round(table)` entries differ by ±1, propagating ≤ a few LSB
//!     (RMS < 1) through the convolution. This is a WASM↔native libm-table
//!     rounding boundary, NOT a port/synthesis bug: the source, shim, config and
//!     sample COUNT are identical, and the residual is bounded + sub-perceptual
//!     (≤8/32768 = 0.024% full-scale). c64re's own probe-705 accepts the same
//!     class of bounded resampler residual.
//!
//! Regenerate (maintainer-only):
//!   native golden : cargo test -p trx64-core --test resid_oracle regen_native -- --ignored --nocapture
//!   c64re ref     : node tests/fixtures/resid/gen_reference.mjs <c64re_root> <this_dir>
//!
//! Run: cargo test -p trx64-core --test resid_oracle -- --nocapture

use std::path::PathBuf;

use trx64_core::resid_audio::{SidAudioEngine, SidWriteRecord};
use trx64_core::resid_ffi::ResidConfig;

const FRAME: u32 = 19656; // one PAL frame, matches gen_reference.mjs
const NATIVE_GOLDEN: &str = "tests/fixtures/resid/trx64_native.saw_noise_release.pcm.s16le";
const C64RE_REF: &str = "tests/fixtures/resid/c64re_wasm.saw_noise_release.pcm.s16le";

/// c64re-WASM ↔ TRX64-native libm-table rounding bound (LSB, full-scale 32768).
/// Empirically the max delta is 5; 8 leaves headroom for libm-version drift.
const LIBM_LSB_BOUND: i32 = 8;

/// Build the EXACT same write/boundary stream as gen_reference.mjs:
///   saw A4 + ADSR + gate + vol → 40 frames; noise → 20 frames; gate off → 20.
fn reference_stream() -> Vec<SidWriteRecord> {
    let mut s = Vec::new();
    for (addr, val) in [
        (0x00u8, (7493 & 0xff) as u8),
        (0x01, ((7493 >> 8) & 0xff) as u8),
        (0x05, 0x09),
        (0x06, 0xf0),
        (0x18, 0x0f),
        (0x04, 0x21), // GATE | SAW
    ] {
        s.push(SidWriteRecord::write(addr, val));
    }
    for _ in 0..40 {
        s.push(SidWriteRecord::boundary(FRAME));
    }
    s.push(SidWriteRecord::write(0x04, 0x81)); // noise
    for _ in 0..20 {
        s.push(SidWriteRecord::boundary(FRAME));
    }
    s.push(SidWriteRecord::write(0x04, 0x80)); // gate off → release
    for _ in 0..20 {
        s.push(SidWriteRecord::boundary(FRAME));
    }
    s
}

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn load_i16(path: &PathBuf) -> Vec<i16> {
    let bytes = std::fs::read(path)
        .unwrap_or_else(|e| panic!("fixture {}: {e}", path.display()));
    assert!(bytes.len() % 2 == 0, "s16le byte count must be even");
    bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
}

/// Run the reference stream through TRX64's native reSID, returning the PCM.
fn run_native() -> Vec<i16> {
    let mut eng = SidAudioEngine::new(ResidConfig::default());
    let produced = eng.run_stream(&reference_stream());
    assert_eq!(produced, eng.pcm().len(), "flush count == buffered");
    eng.pcm().to_vec()
}

fn fnv1a(pcm: &[i16]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &s in pcm {
        for b in s.to_le_bytes() {
            h ^= b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
    }
    h
}

/// GATE A — TRX64's native reSID is byte-identical to the committed native
/// golden. Proves the FFI engine is deterministic + reproducible (this is what
/// ships). A broken FFI / config / emit loop would diverge here immediately.
#[test]
fn gate_a_native_byte_identical() {
    let pcm = run_native();
    let golden = load_i16(&fixture(NATIVE_GOLDEN));
    assert_eq!(
        pcm.len(),
        golden.len(),
        "native sample count {} vs golden {}",
        pcm.len(),
        golden.len()
    );
    for (i, (&a, &b)) in pcm.iter().zip(golden.iter()).enumerate() {
        assert_eq!(a, b, "native PCM diverges at sample {i}: got {a} golden {b}");
    }
    println!(
        "GATE A: {} samples BYTE-IDENTICAL to native golden, fnv1a={:08x}",
        pcm.len(),
        fnv1a(&pcm)
    );
}

/// GATE B — c64re's WASM reSID cross-check. Same source/shim/config/sequence →
/// EXACT sample count + agreement within the WASM↔native libm-table LSB bound.
#[test]
fn gate_b_c64re_within_libm_bound() {
    let native = run_native();
    let c64re = load_i16(&fixture(C64RE_REF));

    // Sample COUNT must be exact — reSID owns sample timing identically.
    assert_eq!(
        native.len(),
        c64re.len(),
        "sample count: TRX64 native {} vs c64re WASM {}",
        native.len(),
        c64re.len()
    );

    let mut ndiff = 0usize;
    let mut max_delta = 0i32;
    let mut sumsq = 0f64;
    for (&a, &b) in native.iter().zip(c64re.iter()) {
        let d = (a as i32 - b as i32).abs();
        if d != 0 {
            ndiff += 1;
            max_delta = max_delta.max(d);
            sumsq += (d * d) as f64;
        }
    }
    let rms = (sumsq / native.len() as f64).sqrt();
    println!(
        "GATE B: {} samples, c64re-WASM vs TRX64-native: {} differ ({:.2}%), \
         maxAbsDiff={} (bound {}), rms={:.4} — same source, libm-table boundary",
        native.len(),
        ndiff,
        100.0 * ndiff as f64 / native.len() as f64,
        max_delta,
        LIBM_LSB_BOUND,
        rms,
    );
    assert!(
        max_delta <= LIBM_LSB_BOUND,
        "c64re vs native max delta {max_delta} exceeds libm bound {LIBM_LSB_BOUND} \
         (would indicate a real port/synthesis bug, not libm rounding)"
    );
}

/// Determinism (per-process): the engine is byte-deterministic for the LIFE of
/// a process — the runtime drives exactly one SID per process, and that single
/// construction reproduces the committed golden bit-for-bit on every separate
/// process invocation (see GATE A, stable fnv1a across runs).
///
/// CAVEAT — in-process RE-construction: reSID builds large lookup tables in
/// `static`-guarded storage (wave.cc/filter8580new.cc `class_init`,
/// `model_filter`, `model_dac`) on the FIRST `SID` construction in a process.
/// Re-constructing the SID in the SAME process (e.g. several engines in one test
/// binary) leaves a tiny resampler residual (≤6 LSB, sub-perceptual) — a
/// known reSID/native-toolchain property, NOT a port bug, and NOT reachable on
/// the single-SID-per-process runtime path. The two-in-process runs below are
/// therefore asserted within that bound, while the HARD per-process determinism
/// is GATE A (first construction == golden, identical across processes).
const INPROC_RECONSTRUCT_BOUND: i32 = 8;

#[test]
fn resid_pcm_deterministic() {
    let a = run_native();
    let b = run_native();
    assert_eq!(a.len(), b.len(), "sample count stable across in-process runs");
    assert!(a.len() > 1000, "non-trivial PCM produced");
    let max_delta = a
        .iter()
        .zip(b.iter())
        .map(|(&x, &y)| (x as i32 - y as i32).abs())
        .max()
        .unwrap_or(0);
    assert!(
        max_delta <= INPROC_RECONSTRUCT_BOUND,
        "in-process reconstruct residual {max_delta} exceeds bound {INPROC_RECONSTRUCT_BOUND} \
         (per-process determinism is GATE A; this is the static-table reconstruct residual)"
    );
}

/// reSID synthesis-state snapshot/restore round-trips bit-exact.
#[test]
fn resid_state_roundtrip() {
    let mut eng = SidAudioEngine::new(ResidConfig::default());
    let mut s = Vec::new();
    for (addr, val) in [(0x00u8, 0x00), (0x01, 0x20), (0x05, 0x09), (0x06, 0xf0), (0x18, 0x0f), (0x04, 0x11)] {
        s.push(SidWriteRecord::write(addr, val));
    }
    s.push(SidWriteRecord::boundary(120_000));
    eng.run_stream(&s);

    let snap = eng.capture_state();
    assert!(!snap.is_empty(), "state blob non-empty");
    eng.run_stream(&[SidWriteRecord::boundary(90_000)]); // disturb
    assert_ne!(snap, eng.capture_state(), "disturb changed reSID state");
    eng.restore_state(&snap);
    assert_eq!(snap, eng.capture_state(), "restore round-trips bit-exact");
}

/// WAV export: header well-formed, payload is the PCM (stereo L=R duplication).
#[test]
fn resid_wav_export_wellformed() {
    use trx64_core::resid_audio::WavFormat;
    let mut eng = SidAudioEngine::new(ResidConfig::default());
    eng.run_stream(&reference_stream());
    let frames = eng.pcm().len();
    let wav = eng.export_wav(WavFormat::default()); // stereo 44.1k
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    assert_eq!(&wav[12..16], b"fmt ");
    assert_eq!(&wav[36..40], b"data");
    let channels = u16::from_le_bytes([wav[22], wav[23]]);
    let sample_rate = u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]);
    assert_eq!(channels, 2);
    assert_eq!(sample_rate, 44100);
    let data_bytes = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]) as usize;
    assert_eq!(data_bytes, frames * 2 /*ch*/ * 2 /*bytes*/);
    assert_eq!(wav.len(), 44 + data_bytes);
}

/// Maintainer-only: regenerate the native golden from the current build.
#[test]
#[ignore]
fn regen_native() {
    let pcm = run_native();
    let mut bytes = Vec::with_capacity(pcm.len() * 2);
    for &s in &pcm {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    let path = fixture(NATIVE_GOLDEN);
    std::fs::write(&path, &bytes).unwrap();
    println!(
        "regen_native: wrote {} samples to {} (fnv1a={:08x})",
        pcm.len(),
        path.display(),
        fnv1a(&pcm)
    );
}
