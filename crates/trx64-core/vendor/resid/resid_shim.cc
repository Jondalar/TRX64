// reSID WASM shim — Spec 703.3, handles added by Spec 855
//
// A thin flat C API over reSID's C++ `reSID::SID` so emscripten can export it
// and the TypeScript `SidWasmEngine` (resid-wasm-engine.ts) can drive it via
// cwrap. This file is OUR code (GPL-3.0-or-later, links GPL reSID); it is NOT
// part of the vendored-unmodified set in third_party/resid/.
//
// reSID reference (third_party/resid/sid.h):
//   void  set_chip_model(chip_model)            chip_model { MOS6581=0, MOS8580=1 }
//   bool  set_sampling_parameters(clock, method, sample_freq, ...)
//                                               sampling_method { FAST=0, INTERPOLATE=1,
//                                                                  RESAMPLE=2, RESAMPLE_FASTMEM=3 }
//   void  reset()
//   void  write(reg8 offset, reg8 value)        offset 0x00..0x1f
//   reg8  read(reg8 offset)
//   int   clock(cycle_count& delta_t, short* buf, int n, int interleave=1)
//         -> advances up to delta_t cycles, writes up to n samples to buf,
//            returns #samples written, sets delta_t to remaining (>0 if buf filled).
//   void  clock(cycle_count delta_t)            advance without sampling
//   int   output()                              current 16-bit AUDIO OUT
//
// ---- INSTANCES (Spec 855) ---------------------------------------------------
//
// This shim used to hold ONE module-global `SID`, because the integrated runtime
// drove exactly one and a module-level instance matched the TS SID lifetime.
// That was never reSID's limit — `reSID::SID` is an ordinary C++ class and VICE
// constructs one per chip (sid/resid.cc:94) — it was ours, and it made a second
// engine impossible rather than merely awkward.
//
// So every entry point now exists twice:
//
//   resid_x_h(void* h, ...)   operates on the instance `h` came from
//   resid_x(...)             the SAME call against the default instance
//
// The legacy names keep their exact previous behaviour, which is what preserves
// byte-identity with c64re's WASM build and keeps `resid_oracle` honest: they
// are the handle form applied to `g_default`, and `g_default` is constructed
// exactly as the old `g_sid` was. A null handle means the default instance, so
// the two forms cannot disagree.
//
// `resid_state_size` takes no handle on purpose: it is `sizeof(SID::State)`, a
// property of the type and not of any instance.

#include "sid.h"
#include <cstddef>
#include <cstring>
#include <new>

using namespace reSID;

namespace {

// One engine plus the cycles the last clock() could not consume. Both are per
// instance: `clock_remaining` belongs to the SID whose buffer filled, and a
// shared one would hand another engine's remainder to the caller.
struct Ctx {
  SID sid;
  int clock_remaining = 0;  // cycles not consumed by the last resid_clock (buf filled)
};

Ctx g_default;

// A null handle is the default instance, so the legacy entry points below are
// the handle form with nothing else changed.
inline Ctx& cx(void* h) { return h ? *static_cast<Ctx*>(h) : g_default; }

}  // namespace

extern "C" {

// ---- lifetime ---------------------------------------------------------------

// A new, pristine engine. The caller owns it and must hand it back to
// resid_delete. Nothing here touches the default instance.
//
// THE STORAGE IS ZEROED FIRST, and that is not belt-and-braces. reSID's SID
// constructor does not initialise everything it owns — the resampler's FIR ring
// `sample[]` is only ever written by clock(), which is exactly why resid_reinit
// exists further down. While this shim had one FILE-SCOPE instance that was
// invisible: a global lives in BSS and starts zeroed, so the ring began at
// silence and every run agreed with every other. Move the same object to the
// heap with a plain `new Ctx()` — which for a class with a user-provided
// constructor runs that constructor and nothing else — and those bytes become
// whatever the allocator last left there. Two engines built the same way then
// produce different audio, and an engine nobody wrote to produces sound. Both
// were observed before this memset, and `sid_multi_gate` now holds them down.
void* resid_new() {
  void* mem = ::operator new(sizeof(Ctx));
  std::memset(mem, 0, sizeof(Ctx));
  return new (mem) Ctx();
}

// Destroy an engine from resid_new. Null is accepted and ignored, so a caller
// unwinding a half-built state need not special-case it; the default instance
// has no handle and therefore cannot be passed here. Paired with the placement
// new above: destructor by hand, then the raw storage back.
void resid_delete(void* h) {
  if (!h) {
    return;
  }
  Ctx* c = static_cast<Ctx*>(h);
  c->~Ctx();
  ::operator delete(static_cast<void*>(c));
}

// ---- configuration ----------------------------------------------------------

// TRX64-only addition (additive — does NOT change any existing function's
// behavior, so byte-identity with c64re's WASM shim is preserved):
// fully RE-CONSTRUCT the SID, exactly as if a fresh WASM module had been
// instantiated. reSID::SID::reset() does NOT clear the resampler's FIR ring
// buffer (sample[], protected, written by clock()); in c64re every ResidWasm
// gets a FRESH module so its global SID is pristine. TRX64 reuses long-lived
// native instances, so we placement-new to reproduce that pristine-module
// state. This makes a TRX64 reset byte-identical to a fresh c64re module.
void resid_reinit_h(void* h) {
  Ctx& c = cx(h);
  c.sid.~SID();
  // Zero the storage before rebuilding, for the same reason resid_new does: the
  // constructor leaves the FIR ring alone, so without this "reinit" would carry
  // the previous run's tail into the new engine and only LOOK pristine.
  //
  // Through a void*: the destructor above ended the object's lifetime, so these
  // are raw bytes and not a SID any more. Saying so also keeps the compiler from
  // warning about memset on a non-trivially-copyable type, which is the right
  // warning to get on live objects and the wrong one here.
  void* storage = static_cast<void*>(&c.sid);
  std::memset(storage, 0, sizeof(SID));
  new (storage) SID();
  c.clock_remaining = 0;
}
void resid_reinit() { resid_reinit_h(nullptr); }

// model: 0 = 6581, 1 = 8580
void resid_set_chip_model_h(void* h, int model) {
  cx(h).sid.set_chip_model(model == 1 ? MOS8580 : MOS6581);
}
void resid_set_chip_model(int model) { resid_set_chip_model_h(nullptr, model); }

// Per-voice enable bitmask. VICE inits this to 0x07 (all three voices) right
// after set_chip_model; the reSID ctor does NOT, so without this call the
// default mask mutes voices. Bit i enables voice i.
void resid_set_voice_mask_h(void* h, int mask) {
  cx(h).sid.set_voice_mask(static_cast<reg4>(mask & 0x0f));
}
void resid_set_voice_mask(int mask) { resid_set_voice_mask_h(nullptr, mask); }

// Enable/disable the SID filter stage (VICE: enable_filter(filters_enabled)).
void resid_enable_filter_h(void* h, int enable) {
  cx(h).sid.enable_filter(enable != 0);
}
void resid_enable_filter(int enable) { resid_enable_filter_h(nullptr, enable); }

// method: 0 FAST, 1 INTERPOLATE, 2 RESAMPLE, 3 RESAMPLE_FASTMEM.
// passband / gain match VICE: passband = sample_freq * SidResidPassband/200,
// gain = SidResidGain/100. Pass passband<=0 to use reSID's own default.
// Returns 1 on success, 0 on failure (e.g. invalid resample params).
int resid_set_sampling_h(void* h, double clock_freq, double sample_freq, int method,
                         double passband, double gain) {
  const double pass = passband > 0.0 ? passband : -1.0;
  return cx(h).sid.set_sampling_parameters(
             clock_freq, static_cast<sampling_method>(method), sample_freq,
             pass, gain)
             ? 1
             : 0;
}
int resid_set_sampling(double clock_freq, double sample_freq, int method,
                       double passband, double gain) {
  return resid_set_sampling_h(nullptr, clock_freq, sample_freq, method, passband, gain);
}

// 6581 filter DC bias (VICE: adjust_filter_bias(SidResidFilterBias/1000)).
// THE 6581 filter-character knob; VICE default 500mV → 0.5.
void resid_adjust_filter_bias_h(void* h, double bias) {
  cx(h).sid.adjust_filter_bias(bias);
}
void resid_adjust_filter_bias(double bias) { resid_adjust_filter_bias_h(nullptr, bias); }

// Output RC stage (VICE enables it with the filter). reSID enables it by
// default; expose it so the engine can match VICE explicitly.
void resid_enable_external_filter_h(void* h, int enable) {
  cx(h).sid.enable_external_filter(enable != 0);
}
void resid_enable_external_filter(int enable) {
  resid_enable_external_filter_h(nullptr, enable);
}

void resid_reset_h(void* h) {
  Ctx& c = cx(h);
  c.sid.reset();
  c.clock_remaining = 0;
}
void resid_reset() { resid_reset_h(nullptr); }

// ---- registers --------------------------------------------------------------

void resid_write_h(void* h, int reg, int value) {
  cx(h).sid.write(static_cast<reg8>(reg & 0x1f), static_cast<reg8>(value & 0xff));
}
void resid_write(int reg, int value) { resid_write_h(nullptr, reg, value); }

int resid_read_h(void* h, int reg) {
  return static_cast<int>(cx(h).sid.read(static_cast<reg8>(reg & 0x1f)));
}
int resid_read(int reg) { return resid_read_h(nullptr, reg); }

// ---- clocking ---------------------------------------------------------------

// Advance up to `delta` C64 cycles, writing up to `max_samples` signed 16-bit
// mono samples into `buf` (a pointer into the WASM heap supplied by the caller).
// Returns the number of samples produced. If the buffer filled before `delta`
// cycles were consumed, the remainder is stored and readable via
// resid_clock_remaining(); the caller loops until that is 0.
int resid_clock_h(void* h, int delta, short* buf, int max_samples) {
  Ctx& c = cx(h);
  cycle_count dt = delta;
  int produced = c.sid.clock(dt, buf, max_samples);
  c.clock_remaining = static_cast<int>(dt);
  return produced;
}
int resid_clock(int delta, short* buf, int max_samples) {
  return resid_clock_h(nullptr, delta, buf, max_samples);
}

int resid_clock_remaining_h(void* h) { return cx(h).clock_remaining; }
int resid_clock_remaining() { return resid_clock_remaining_h(nullptr); }

// Advance `delta` cycles without producing samples (for clockUntil-style use
// when audio output is muted but SID state must still age).
void resid_clock_silent_h(void* h, int delta) {
  cx(h).sid.clock(static_cast<cycle_count>(delta));
}
void resid_clock_silent(int delta) { resid_clock_silent_h(nullptr, delta); }

// Current 16-bit AUDIO OUT (post external filter).
int resid_output_h(void* h) { return cx(h).sid.output(); }
int resid_output() { return resid_output_h(nullptr); }

// ---- Spec 705.A step 4 — reSID synthesis-state snapshot/restore -------------
//
// VICE restores reSID's full SYNTHESIS state on snapshot read (not just SID
// registers): src/sid/sid-snapshot.c (sid_snapshot_write/read_resid_module) +
// src/sid/resid.cc (resid_state_read/write) map reSID::SID::State <->
// sid_snapshot_state_t. reSID::SID::State (third_party/resid/sid.h:65-93) holds
// exactly that synthesis state: sid_register[0x20], bus_value/bus_value_ttl,
// write_pipeline/write_address, voice_mask, accumulator[3], shift_register[3],
// shift_register_reset[3], shift_pipeline[3], pulse_output[3],
// floating_output_ttl[3], rate_counter[3]/rate_counter_period[3],
// exponential_counter[3]/exponential_counter_period[3], envelope_counter[3],
// envelope_state[3], hold_zero[3], envelope_pipeline[3].
//
// We expose reSID's own read_state()/write_state() over a flat byte buffer
// (= the SID::State POD). Self-consistent within this build (same struct layout
// for read+write), so snapshot -> restore round-trips bit-exact. This is the
// VICE-shaped synthesis state, NOT a SID-register reinit.

// No handle: this is sizeof(SID::State), a property of the type. Every instance
// reports the same number, so asking one of them in particular would only
// invite the reader to wonder which.
int resid_state_size() { return static_cast<int>(sizeof(SID::State)); }

void resid_read_state_h(void* h, unsigned char* buf) {
  // The blob must be byte-deterministic: `resid_state_roundtrip` compares two
  // captures of the same state, and a snapshot that differs from itself is a
  // snapshot nobody can diff.
  //
  // This used to memset a local `State` and then ASSIGN the result of
  // read_state() into it, on the reasoning that the implicit copy-assignment
  // writes only the named members and leaves the zeroed padding alone. The
  // standard permits that reading, but it does not require it: for a trivially
  // copyable type the compiler may emit a whole-object copy, which carries the
  // right-hand temporary's padding — whatever was on the stack — straight into
  // the zeroed local. It survived only because both captures came through an
  // identical call path into one file-scope SID, so the residue happened to
  // match. Spec 855 gave each engine its own heap context, the path changed,
  // and the same state captured twice differed by one byte.
  //
  // So the padding is zeroed explicitly, where it is rather than where a copy
  // happened to leave it. reSID's State is 4-byte members throughout
  // (reg4/reg8/reg16/reg24 are `unsigned int`, cycle_count is `int`) with ONE
  // exception: `bool hold_zero[3]` is three bytes and `cycle_count
  // envelope_pipeline[3]` behind it needs four-byte alignment. That single
  // byte is the struct's only hole, and `offsetof` finds it without this code
  // knowing the layout.
  SID::State s = cx(h).sid.read_state();
  std::memcpy(buf, &s, sizeof(s));

  const size_t gap_start = offsetof(SID::State, hold_zero) + sizeof(s.hold_zero);
  const size_t gap_end = offsetof(SID::State, envelope_pipeline);
  if (gap_end > gap_start) {
    std::memset(buf + gap_start, 0, gap_end - gap_start);
  }
}
void resid_read_state(unsigned char* buf) { resid_read_state_h(nullptr, buf); }

void resid_write_state_h(void* h, const unsigned char* buf) {
  SID::State s;  // default-constructed, then overwritten by the captured POD
  std::memcpy(&s, buf, sizeof(s));
  cx(h).sid.write_state(s);
}
void resid_write_state(const unsigned char* buf) { resid_write_state_h(nullptr, buf); }

}  // extern "C"
