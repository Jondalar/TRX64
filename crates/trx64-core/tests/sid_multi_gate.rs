//! Spec 855 gate — more than one SID.
//!
//! Slice 1 (D1): the shim takes handles, so engines exist independently. Until
//! 855 this file could not have been written at all: the shim held one global
//! `reSID::SID` behind a process-wide mutex, and a second `Resid::new` did not
//! run slowly, it DEADLOCKED. Every case here would have hung.
//!
//! The second case is the one that matters, because it is the claim I got wrong
//! twice — in this repo's notes and to the UE2 session. I said two reSID
//! instances were not sample-synchronous and that mixing was therefore the hard
//! part. It is false: reSID's cadence comes from `cycles_per_sample`, computed
//! once in `set_sampling_parameters` from clock and sample frequency alone
//! (`sid.cc:982`). It does not depend on what the chip plays. 855 §2 says so,
//! and this is that sentence under test rather than in a comment.

use trx64_core::resid_ffi::{Resid, ResidConfig, MODEL_8580};

/// PAL: 63 cycles × 312 lines.
const FRAME: u32 = 19_656;

/// A4-ish saw with a slow envelope, so the engine has something to synthesise.
fn saw(e: &mut Resid) {
    for (reg, val) in [(0x00u8, 0x45), (0x01, 0x1d), (0x05, 0x09), (0x06, 0xf0), (0x18, 0x0f), (0x04, 0x21)] {
        e.write(reg, val);
    }
}

/// Noise at a different frequency — a different waveform AND a different rate,
/// so the two engines cannot accidentally produce the same stream.
fn noise(e: &mut Resid) {
    for (reg, val) in [(0x00u8, 0xe0), (0x01, 0x08), (0x05, 0x00), (0x06, 0xf0), (0x18, 0x0f), (0x04, 0x81)] {
        e.write(reg, val);
    }
}

#[test]
fn two_engines_exist_at_once_and_neither_blocks() {
    let mut a = Resid::new(ResidConfig::default());
    let mut b = Resid::new(ResidConfig::default());
    saw(&mut a);
    noise(&mut b);
    let pa = a.emit(FRAME);
    let pb = b.emit(FRAME);
    assert!(!pa.is_empty() && !pb.is_empty(), "both engines produced samples");
}

#[test]
fn an_engine_does_not_hear_its_neighbour() {
    let mut a = Resid::new(ResidConfig::default());
    let mut b = Resid::new(ResidConfig::default());

    // Only A is ever written to. If the two shared one C++ SID — which is
    // exactly what the shim used to hand out — B would sing along.
    saw(&mut a);
    let pa = a.emit(FRAME);
    let pb = b.emit(FRAME);

    // The comparison is against an engine that never HAD a neighbour, not
    // against silence. A pristine SID need not emit all zeros — it settles — and
    // asserting silence would test reSID's DC behaviour while claiming to test
    // isolation. What isolation means is that B is indistinguishable from an
    // engine built alone.
    let mut lonely = Resid::new(ResidConfig::default());
    let solo = lonely.emit(FRAME);

    assert!(pa.iter().any(|&s| s != 0), "the engine that was written to makes sound");
    assert_eq!(pb, solo, "an untouched engine sounds exactly like one built with no neighbour");
    assert_ne!(pa, pb, "and the one that was written to does not");
}

#[test]
fn the_same_deltas_give_the_same_sample_count_whatever_the_chips_play() {
    let mut a = Resid::new(ResidConfig::default());
    // Deliberately a different chip model as well as different content: the
    // claim is about the CADENCE, which neither should touch.
    let mut b = Resid::new(ResidConfig { model: MODEL_8580, ..ResidConfig::default() });
    saw(&mut a);
    noise(&mut b);

    let (mut total_a, mut total_b) = (0usize, 0usize);
    let (mut pcm_a, mut pcm_b) = (Vec::new(), Vec::new());
    for frame in 0..40 {
        let ca = a.emit(FRAME);
        let cb = b.emit(FRAME);
        assert_eq!(
            ca.len(),
            cb.len(),
            "frame {frame}: two engines given the same delta returned {} and {} samples",
            ca.len(),
            cb.len()
        );
        total_a += ca.len();
        total_b += cb.len();
        pcm_a.extend_from_slice(&ca);
        pcm_b.extend_from_slice(&cb);
    }
    assert_eq!(total_a, total_b, "and they agree cumulatively, where a drift of one would show");

    // The case must actually have met the condition it is about: if both engines
    // produced the SAME audio, equal counts would prove nothing about
    // content-independence.
    assert_ne!(pcm_a, pcm_b, "the two engines were playing different things");
}

/// The same number `resid_oracle` calls `INPROC_RECONSTRUCT_BOUND`, for the same
/// reason: reSID builds its filter and FIR tables with libm at construction, and
/// rebuilding them in one process does not land on the same last bit every time.
/// It is reSID's property and not the port's, and 855 makes it ROUTINE — several
/// engines per process is now the normal case rather than the exception.
const INPROC_RECONSTRUCT_BOUND: i32 = 8;

#[test]
fn every_engine_agrees_with_the_first_within_resids_own_residual() {
    // The old shim reset by placement-newing its one global, so only the FIRST
    // use in a process was truly pristine. With one engine each, every engine is
    // built the same way — but "the same way" is not "bit for bit", and this
    // case asserts the honest thing rather than the flattering one.
    //
    // The sample COUNT must be exact: that is the cadence, and a leak or a
    // half-built engine moves it. The samples must agree within reSID's own
    // documented residual. A first draft demanded byte-identity, which is
    // stricter than this repo's own bound and failed at ±2 LSB with nothing
    // wrong — the same overclaim the oracle's GATE A had already been through.
    let mut first = Resid::new(ResidConfig::default());
    saw(&mut first);
    let reference = first.emit(FRAME * 4);
    assert!(reference.len() > 1000, "non-trivial PCM produced");

    for n in 0..10 {
        let mut e = Resid::new(ResidConfig::default());
        saw(&mut e);
        let pcm = e.emit(FRAME * 4);
        assert_eq!(pcm.len(), reference.len(), "engine {n} produced a different sample count");
        let max_delta = pcm
            .iter()
            .zip(reference.iter())
            .map(|(&x, &y)| (x as i32 - y as i32).abs())
            .max()
            .unwrap_or(0);
        assert!(
            max_delta <= INPROC_RECONSTRUCT_BOUND,
            "engine {n} deviates {max_delta} LSB from the first one built, over the \
             {INPROC_RECONSTRUCT_BOUND} LSB reconstruct bound — that is a leak or a broken \
             engine, not table rounding"
        );
    }
}

// ── Slice 2 (D2 + D3) — the decode table and per-chip state ───────────────────
//
// These drive the machine, not the engine: the question here is what the 6502
// sees, which is `Sid6581` and the register files, not reSID.
//
// Reads are checked through `sid_chip_regs` rather than `read_full`, on purpose.
// `read_full` is a PEEK: it is side-effect-free and answers from the register
// shadow, so it cannot show a computed `$D41B`/`$D41C`. Proving OSC3/ENV3 per
// chip needs a running 6502, and that case belongs with the slice that gives
// the extra chips a tick.

use trx64_core::sid::SidMapping;
use trx64_core::Machine;

#[test]
fn a_machine_with_no_table_is_the_machine_we_had() {
    let mut m = Machine::new();
    assert_eq!(m.sid_chip_count(), 1, "one SID until a host says otherwise");
    assert!(m.sid_map().is_empty());

    m.write_full(0xd405, 0x11);
    assert_eq!(m.sid_chip_regs(0).unwrap()[5], 0x11, "still $D400 onto chip 0");
    // And the mirror every $20 that $D400-$D7FF has always had.
    m.write_full(0xd425, 0x22);
    assert_eq!(m.sid_chip_regs(0).unwrap()[5], 0x22, "mirrored, as before");
}

#[test]
fn mapping_a_chip_grows_the_machine() {
    let mut m = Machine::new();
    m.set_sid_map(vec![SidMapping::window(0xde00, 3, false)]);
    assert_eq!(m.sid_chip_count(), 4, "chip 3 named, so chips 0..3 exist");
}

#[test]
fn a_mapped_window_routes_writes_to_its_own_chip() {
    let mut m = Machine::new();
    m.set_sid_map(vec![
        SidMapping::window(0xd400, 0, true),
        SidMapping::window(0xde00, 1, false),
    ]);

    m.write_full(0xde05, 0xaa);
    assert_eq!(m.sid_chip_regs(1).unwrap()[5], 0xaa, "the second chip took it");
    assert_eq!(m.sid_chip_regs(0).unwrap()[5], 0x00, "and the first one did not hear it");

    m.write_full(0xd405, 0x55);
    assert_eq!(m.sid_chip_regs(0).unwrap()[5], 0x55);
    assert_eq!(m.sid_chip_regs(1).unwrap()[5], 0xaa, "unchanged by a write to the other");
}

#[test]
fn a_cold_reset_clears_the_chips_and_keeps_the_table() {
    let mut m = Machine::new();
    let map = vec![SidMapping::window(0xd400, 0, true), SidMapping::window(0xde00, 1, false)];
    m.set_sid_map(map.clone());
    m.write_full(0xde05, 0xaa);
    m.write_full(0xd405, 0x55);

    m.cold_reset();

    assert_eq!(m.sid_chip_regs(0).unwrap()[5], 0x00, "chip 0 went to power-on");
    assert_eq!(m.sid_chip_regs(1).unwrap()[5], 0x00, "and so did chip 1");
    // D2: the firmware rewrites the mapping in its own reset task. A table that
    // cleared itself here would race that and leave the C64 addressing chips
    // that had just vanished.
    assert_eq!(m.sid_map(), map.as_slice(), "the table survived the reset");
    assert_eq!(m.sid_chip_count(), 2, "and so did the chips themselves");
}

#[test]
fn an_envelope_can_be_read_per_chip() {
    let mut m = Machine::new();
    m.set_sid_map(vec![SidMapping::window(0xde00, 1, false)]);
    // Cosmetic readout only (a firmware LED strip), so the assertion is that it
    // exists per chip and refuses what does not — not what the value is.
    assert_eq!(m.sid_envelope(0, 0), Some(0));
    assert_eq!(m.sid_envelope(1, 2), Some(0));
    assert_eq!(m.sid_envelope(2, 0), None, "no such chip");
    assert_eq!(m.sid_envelope(0, 3), None, "no such voice");
}

// ── Slice 3 (D4) — the write trace ────────────────────────────────────────────

use std::sync::{Arc, Mutex};

type Traced = Arc<Mutex<Vec<(u8, u8, u8, u64)>>>;

fn trace_into(m: &mut Machine) -> Traced {
    let seen: Traced = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    m.set_sid_write_trace(Some(Box::new(move |chip, reg, value, clk| {
        sink.lock().unwrap().push((chip, reg, value, clk));
    })));
    seen
}

#[test]
fn the_trace_says_which_chip_took_the_write() {
    let mut m = Machine::new();
    m.set_sid_map(vec![
        SidMapping::window(0xd400, 0, true),
        SidMapping::window(0xde00, 1, false),
    ]);
    let seen = trace_into(&mut m);

    m.write_full(0xd405, 0x11);
    m.write_full(0xde05, 0xaa);

    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 2, "one record per write: {got:?}");
    assert_eq!((got[0].0, got[0].1, got[0].2), (0, 0x05, 0x11));
    assert_eq!((got[1].0, got[1].1, got[1].2), (1, 0x05, 0xaa), "the second chip, not the first");
}

#[test]
fn the_trace_carries_the_cycle() {
    let mut m = Machine::new();
    // Set the clock to something recognisable rather than asserting 0 == 0, which
    // would pass whether or not the cycle were plumbed at all.
    m.c64_core.clk = 12_345;
    let seen = trace_into(&mut m);

    m.poke_io(0xd418, &[0x0f]);

    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].3, 12_345, "the cycle the write happened on");
}

#[test]
fn a_host_poke_is_a_sid_write_and_is_heard() {
    // The reason D4 keeps the hook at the machine instead of moving it to the bus
    // dispatch: `poke_io` does not go through the bus, so a bus-only hook would
    // have stopped reporting monitor writes. A $D418 poke changing the volume
    // with nothing in the audio stream to show for it is the kind of silence
    // nobody notices until a recording is wrong.
    let mut m = Machine::new();
    let seen = trace_into(&mut m);

    m.poke_io(0xd418, &[0x0f]);
    m.write_full(0xd404, 0x21);

    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 2, "the poke AND the bus write, in order: {got:?}");
    assert_eq!((got[0].0, got[0].1, got[0].2), (0, 0x18, 0x0f), "the poke");
    assert_eq!((got[1].0, got[1].1, got[1].2), (0, 0x04, 0x21), "the bus write");
}

#[test]
fn clearing_the_trace_stops_it_and_a_clone_never_had_it() {
    let mut m = Machine::new();
    let seen = trace_into(&mut m);
    m.write_full(0xd405, 0x11);

    // A fork starts audio-silent: the subscriber is transport plumbing, not
    // register state, so `Clone` drops it — the same rule `Sid6581` used to hold.
    let mut forked = m.clone();
    forked.write_full(0xd405, 0x22);

    m.set_sid_write_trace(None);
    m.write_full(0xd405, 0x33);

    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "only the write made while installed: {got:?}");
    assert_eq!(got[0].2, 0x11);
}

// ── Slice 4 (D5) — a host may answer for a chip ───────────────────────────────
//
// The bus read path is driven with real 6502 code on purpose: `read_full` is a
// PEEK and never reaches it, so a test that used `read_full` would claim to
// check the read override while checking the peek override.

use trx64_core::NullSink;

/// `LDA $D41B / STA $0400 / JMP *` at $C000.
const READ_OSC3: [u8; 9] = [0xad, 0x1b, 0xd4, 0x8d, 0x00, 0x04, 0x4c, 0x06, 0xc0];

fn run_instrs(m: &mut Machine, n: u64) {
    m.run_for_full_capped(n * 64, n, &mut NullSink, |_, _, _, _, _, _, _| {});
}

#[test]
fn a_host_answers_a_bus_read() {
    let mut m = Machine::new();
    m.set_sid_host_access(
        Some(Box::new(|chip, reg| if (chip, reg) == (0, 0x1b) { Some(0x5a) } else { None })),
        None,
    );
    m.poke(0xc000, &READ_OSC3);
    m.write_full(0x0001, 0x37);
    m.c64_core.reg_pc = 0xc000;
    run_instrs(&mut m, 3);

    assert_eq!(m.read_full(0x0400), 0x5a, "the host's byte reached the CPU, not the fastsid's");
}

#[test]
fn a_peek_asks_the_peek_hook_and_never_the_read_hook() {
    let reads = Arc::new(Mutex::new(0usize));
    let counted = Arc::clone(&reads);

    let mut m = Machine::new();
    m.set_sid_host_access(
        Some(Box::new(move |_chip, _reg| {
            *counted.lock().unwrap() += 1;
            Some(0x11)
        })),
        Some(Box::new(|chip, reg| if (chip, reg) == (0, 0x1b) { Some(0x22) } else { None })),
    );

    assert_eq!(m.read_full(0xd41b), 0x22, "the monitor sees the host's answer");
    assert_eq!(
        *reads.lock().unwrap(),
        0,
        "a peek must not run the READ hook — that is the whole point of having two"
    );
}

#[test]
fn without_a_host_the_core_still_answers() {
    let mut m = Machine::new();
    m.poke(0xc000, &READ_OSC3);
    m.write_full(0x0001, 0x37);
    m.c64_core.reg_pc = 0xc000;
    run_instrs(&mut m, 3);
    let from_core = m.read_full(0x0400);

    // The same machine with a host that declines everything must be identical:
    // `None` falls through, exactly as `ExpansionDevice::read` does under 850.
    let mut n = Machine::new();
    n.set_sid_host_access(Some(Box::new(|_c, _r| None)), Some(Box::new(|_c, _r| None)));
    n.poke(0xc000, &READ_OSC3);
    n.write_full(0x0001, 0x37);
    n.c64_core.reg_pc = 0xc000;
    run_instrs(&mut n, 3);

    assert_eq!(n.read_full(0x0400), from_core, "declining is the same as not being there");
}
