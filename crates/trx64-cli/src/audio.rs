//! The emulator window's audio output — `audioDrain()` → a ring → a cpal stream.
//!
//! Mirrors the SwiftUI `AudioOutput` idea: a producer thread pulls the runtime's
//! drained SID PCM (`pull_audio_drain`, mono i16 @ 44100) at ~the frame cadence and
//! pushes it into a lock-light ring; the cpal output callback only POPS from the ring
//! (never touches the State lock). Underrun = silence (no click). A small pre-roll
//! cushion absorbs jitter before playback starts.
//!
//! `pull_audio_drain`'s first call installs the SID capture hook + spawns the
//! runtime's persistent reSID render thread (constructed ONCE — the per-drain
//! reconstruct was the ~60 Hz hum, already fixed upstream). So this module just
//! drains + plays; it does not construct reSID.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};

use trx64_daemon::{pull_audio_drain, SharedState, AUDIO_SAMPLE_RATE};

/// A simple mono i16 ring shared between the producer (drain) thread and the cpal
/// callback. `Mutex<VecDeque>` is plenty: the callback pops a few hundred samples and
/// the producer pushes once per frame — contention is trivial and bounded.
struct Ring {
    buf: VecDeque<i16>,
    /// Samples that must accumulate before the callback starts emitting audio (jitter
    /// cushion). Once primed, stays primed unless fully drained.
    preroll: usize,
    primed: bool,
}

impl Ring {
    fn new(preroll: usize) -> Self {
        Self { buf: VecDeque::with_capacity(preroll * 4), preroll, primed: false }
    }
}

/// Owns the cpal stream + the producer thread; dropping it stops both.
pub struct AudioOutput {
    _stream: Stream,
    stop: Arc<std::sync::atomic::AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl AudioOutput {
    /// Start audio for the given machine. Returns `None` (logging a warning) if no
    /// output device / supported config is available — the window still runs muted.
    pub fn start(state: SharedState) -> Option<Self> {
        let host = cpal::default_host();
        let device = match host.default_output_device() {
            Some(d) => d,
            None => {
                eprintln!("[trx64-cli] audio: no default output device — window runs muted.");
                return None;
            }
        };

        // We want mono 44100 i16 if possible; otherwise fall back to the device default
        // and up-mix mono→N channels in the callback.
        let supported = match device.default_output_config() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[trx64-cli] audio: no default output config ({e}) — muted.");
                return None;
            }
        };
        let sample_format = supported.sample_format();
        let channels = supported.config().channels.max(1) as usize;
        // Force the runtime's sample rate so 1 drained sample == 1 output frame.
        let config = StreamConfig {
            channels: supported.config().channels.max(1),
            sample_rate: cpal::SampleRate(AUDIO_SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        // Pre-roll cushion. The producer fills in BURSTS (~882 samples per 20 ms pump
        // frame, 0 between), and thread::sleep frame pacing jitters, so a small cushion
        // underruns → re-prime → audible gaps. ~150 ms (≈7 frames) absorbs the
        // burstiness + jitter; the 250 ms cap below still bounds latency. (Matches the
        // SwiftUI AudioOutput's ~180 ms target.)
        let ring = Arc::new(Mutex::new(Ring::new((AUDIO_SAMPLE_RATE as usize) * 150 / 1000)));

        let stream = build_stream(&device, &config, sample_format, channels, Arc::clone(&ring))?;
        if let Err(e) = stream.play() {
            eprintln!("[trx64-cli] audio: stream.play failed ({e}) — muted.");
            return None;
        }

        // Producer thread: drain the runtime + feed the ring at ~the frame cadence.
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name("trx64-cli-audio".into())
            .spawn(move || drain_loop(state, ring, stop_t))
            .ok();

        Some(AudioOutput { _stream: stream, stop, join })
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        // _stream drops here → cpal stops the callback.
    }
}

/// Producer: pull drained PCM ~every 5 ms and append to the ring (bounded so a paused
/// machine producing nothing doesn't let the ring grow; a backlog is trimmed).
fn drain_loop(state: SharedState, ring: Arc<Mutex<Ring>>, stop: Arc<std::sync::atomic::AtomicBool>) {
    // Cap the ring at ~250 ms so a warp burst can't balloon latency.
    let cap = (AUDIO_SAMPLE_RATE as usize) / 4;
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        let samples = pull_audio_drain(&state).samples;
        if !samples.is_empty() {
            let mut r = ring.lock().unwrap();
            r.buf.extend(samples);
            if r.buf.len() > cap {
                let drop = r.buf.len() - cap;
                r.buf.drain(0..drop);
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    fmt: SampleFormat,
    channels: usize,
    ring: Arc<Mutex<Ring>>,
) -> Option<Stream> {
    let err_fn = |e| eprintln!("[trx64-cli] audio stream error: {e}");
    // The mono i16 ring up-mixes into whatever sample format the device wants via
    // cpal's `FromSample<i16>` (covers i16/f32/u16/… uniformly).
    let res = match fmt {
        SampleFormat::I16 => device.build_output_stream(
            config,
            move |out: &mut [i16], _| fill(out, channels, &ring),
            err_fn,
            None,
        ),
        SampleFormat::F32 => device.build_output_stream(
            config,
            move |out: &mut [f32], _| fill(out, channels, &ring),
            err_fn,
            None,
        ),
        SampleFormat::U16 => device.build_output_stream(
            config,
            move |out: &mut [u16], _| fill(out, channels, &ring),
            err_fn,
            None,
        ),
        other => {
            eprintln!("[trx64-cli] audio: unsupported sample format {other:?} — muted.");
            return None;
        }
    };
    match res {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("[trx64-cli] audio: build_output_stream failed ({e}) — muted.");
            None
        }
    }
}

/// Fill an interleaved output buffer from the mono ring, up-mixing one mono sample to
/// all channels. Underrun → silence (the rest of the buffer is zeroed). Pre-roll: emit
/// silence until the cushion is met, then stay primed until the ring empties.
fn fill<T: cpal::Sample + cpal::FromSample<i16>>(
    out: &mut [T],
    channels: usize,
    ring: &Arc<Mutex<Ring>>,
) {
    let silence = T::from_sample(0i16);
    let mut r = ring.lock().unwrap();
    if !r.primed {
        if r.buf.len() >= r.preroll {
            r.primed = true;
        } else {
            out.fill(silence);
            return;
        }
    }
    let channels = channels.max(1);
    let frames = out.len() / channels;
    for f in 0..frames {
        let v = match r.buf.pop_front() {
            Some(x) => T::from_sample(x),
            None => {
                r.primed = false; // underran — re-prime before emitting again
                silence
            }
        };
        for c in 0..channels {
            out[f * channels + c] = v;
        }
    }
}
