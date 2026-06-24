//! resid_audio.rs — the SID AUDIO tier, decoupled from the CPU tick.
//!
//! 1:1 port of the c64re TS `audio/` decoupling plumbing (sid-write-ring.ts,
//! sid-pcm-ring.ts, sid-audio-recorder.ts, wav-writer.ts), reduced to the
//! single-process Rust shape:
//!
//!   * a SID write-stream carrying `(addr, value)` writes in CPU order plus a
//!     per-frame BOUNDARY record carrying that frame's elapsed cycle count
//!     (`dCycles` → `Resid::emit`). This is the same Spec 768 contract the TS
//!     `SidWriteRingProducer` uses (TYPE_WRITE / TYPE_BOUNDARY).
//!   * a per-frame drain: replay writes-then-`emit(dCycles)`, exactly the TS
//!     worker loop (Spec 703 model: writeTrace applies writes, flush emits).
//!   * Int16 PCM accumulation + WAV (RIFF/PCM s16le) export (wav-writer.ts).
//!
//! Decoupling means: the byte-exact fastsid register engine (`sid.rs`) is the
//! in-tick authority; this engine subscribes to the SAME writes via the
//! `Sid6581::write_trace` hook (additive, None on byte-exact paths) and renders
//! audio per-frame off the reSID FFI. No new per-cycle tick hook — it reuses the
//! per-instruction cycle budget that already drives `sid.tick`.

use crate::resid_ffi::{Resid, ResidConfig};

/// Record kinds in the SID write-stream (mirrors TS `SID_REC_TYPE_*`).
pub const SID_REC_TYPE_WRITE: u8 = 1;
pub const SID_REC_TYPE_BOUNDARY: u8 = 2;

/// One record in the SID write-stream. A WRITE carries `(addr & 0x1f, value)`;
/// a BOUNDARY carries `d_cycles` (elapsed Φ2 cycles for that frame).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SidWriteRecord {
    pub kind: u8,
    pub addr: u8,
    pub value: u8,
    pub d_cycles: u32,
}

impl SidWriteRecord {
    #[inline]
    pub fn write(addr: u8, value: u8) -> Self {
        Self { kind: SID_REC_TYPE_WRITE, addr: addr & 0x1f, value, d_cycles: 0 }
    }
    #[inline]
    pub fn boundary(d_cycles: u32) -> Self {
        Self { kind: SID_REC_TYPE_BOUNDARY, addr: 0, value: 0, d_cycles }
    }
}

/// WAV (RIFF/PCM) output format (mirrors `wav-writer.ts` WavOptions).
#[derive(Clone, Copy, Debug)]
pub struct WavFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl Default for WavFormat {
    fn default() -> Self {
        Self { sample_rate: 44100, channels: 2 }
    }
}

/// The decoupled SID audio engine: a no-drop write-stream feeding the reSID FFI
/// per-frame, accumulating Int16 PCM that can be exported to WAV.
///
/// Usage (per-frame, 1:1 with the TS worker):
///   * during the frame, `record_write(addr, value)` for each SID register write
///     (in CPU order) — this is what the `Sid6581::write_trace` hook calls;
///   * at the frame end, `record_boundary(d_cycles)`;
///   * `flush()` drains the pending stream: replay the writes into reSID, then
///     `emit(d_cycles)` for each boundary, appending the produced PCM.
///
/// Mono PCM (single SID). `export_wav` duplicates L=R for stereo consumers.
pub struct SidAudioEngine {
    resid: Resid,
    pending: Vec<SidWriteRecord>,
    pcm: Vec<i16>,
    dropped: u64,
}

impl SidAudioEngine {
    /// Create with the given reSID config (default = `ResidWasm` defaults:
    /// 6581, filter OFF, RESAMPLE @ 44.1k/PAL).
    pub fn new(cfg: ResidConfig) -> Self {
        Self {
            resid: Resid::new(cfg),
            pending: Vec::new(),
            pcm: Vec::new(),
            dropped: 0,
        }
    }

    pub fn new_default() -> Self {
        Self::new(ResidConfig::default())
    }

    /// Reset the engine: re-init reSID + clear the stream and PCM buffer.
    pub fn reset(&mut self) {
        self.resid.reset();
        self.pending.clear();
        self.pcm.clear();
        self.dropped = 0;
    }

    /// Record a SID register write (addr = $D4xx offset, masked to 0x00..0x1f).
    /// This is the `Sid6581::write_trace` subscriber (Spec 768 producer.write).
    #[inline]
    pub fn record_write(&mut self, addr: u8, value: u8) {
        self.pending.push(SidWriteRecord::write(addr, value));
    }

    /// Mark a frame boundary: after `flush`, `emit(d_cycles)` of PCM is produced
    /// from the writes recorded so far (Spec 768 producer.boundary).
    #[inline]
    pub fn record_boundary(&mut self, d_cycles: u32) {
        self.pending.push(SidWriteRecord::boundary(d_cycles));
    }

    /// Drain the pending stream in order: WRITE → reSID register write; BOUNDARY
    /// → `emit(d_cycles)` appended to the PCM buffer. Mirrors the TS reSID worker
    /// loop (drain → replay writes-then-emit per boundary). Returns the number of
    /// PCM samples produced by this flush.
    pub fn flush(&mut self) -> usize {
        let before = self.pcm.len();
        // Take the stream so the borrow on `self.pending` does not collide with
        // the `&mut self.resid` calls below.
        let records = std::mem::take(&mut self.pending);
        for rec in &records {
            match rec.kind {
                SID_REC_TYPE_WRITE => self.resid.write(rec.addr, rec.value),
                SID_REC_TYPE_BOUNDARY => {
                    let samples = self.resid.emit(rec.d_cycles);
                    self.pcm.extend_from_slice(&samples);
                }
                _ => {}
            }
        }
        self.pcm.len() - before
    }

    /// Convenience: feed a whole pre-built write-stream and flush it.
    pub fn run_stream(&mut self, records: &[SidWriteRecord]) -> usize {
        self.pending.extend_from_slice(records);
        self.flush()
    }

    /// The accumulated mono Int16 PCM (verbatim reSID output).
    pub fn pcm(&self) -> &[i16] {
        &self.pcm
    }

    /// Take the accumulated PCM, clearing the internal buffer.
    pub fn take_pcm(&mut self) -> Vec<i16> {
        std::mem::take(&mut self.pcm)
    }

    /// reSID synthesis-state checkpoint (= VICE sid_snapshot_state_t).
    pub fn capture_state(&self) -> Vec<u8> {
        self.resid.capture_state()
    }
    pub fn restore_state(&mut self, bytes: &[u8]) {
        self.resid.restore_state(bytes)
    }

    /// Mutable access to the underlying reSID engine (oracle / advanced use).
    pub fn resid_mut(&mut self) -> &mut Resid {
        &mut self.resid
    }

    /// Build a complete WAV byte buffer from the accumulated PCM. Mono is
    /// duplicated to L=R when `fmt.channels == 2` (single SID → identical
    /// channels, as the TS `monoToStereoLR` does). Verbatim of `wav-writer.ts`
    /// `buildWav` (44-byte RIFF header + interleaved s16le payload).
    pub fn export_wav(&self, fmt: WavFormat) -> Vec<u8> {
        build_wav(&self.pcm, fmt)
    }

    /// Count of records the producer could not enqueue (always 0 here — the
    /// Rust stream is an unbounded Vec; kept for TS API parity / future SAB).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Build a WAV (RIFF/PCM) byte buffer from MONO Int16 samples. For
/// `channels == 2`, each mono sample is duplicated L=R (single SID). 1:1 with
/// `wav-writer.ts` buildWav + monoToStereoLR.
pub fn build_wav(mono: &[i16], fmt: WavFormat) -> Vec<u8> {
    let channels = fmt.channels.max(1);
    let bits_per_sample: u16 = 16;
    let frames = mono.len();
    let total_samples = frames * channels as usize; // interleaved sample count
    let data_bytes = total_samples * 2;
    let byte_rate = fmt.sample_rate * channels as u32 * (bits_per_sample as u32 / 8);
    let block_align = channels * (bits_per_sample / 8);
    let file_size = 36 + data_bytes as u32;

    let mut buf = Vec::with_capacity(44 + data_bytes);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&file_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&fmt.sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&bits_per_sample.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for &s in mono {
        for _ in 0..channels {
            buf.extend_from_slice(&s.to_le_bytes());
        }
    }
    buf
}
