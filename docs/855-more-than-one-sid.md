# Spec 855 — More than one SID

**Status:** PROPOSED 2026-09-17.
**Repos:** TRX64 (`trx64-core`) builds the core half. UE2 (`u64-emulator/crates/c64-bridge`)
builds its own: the UltiSID register face, the address decode it already owns, and its mixer.
**Number:** 855 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** nothing structural. Additive to Spec 703's two-tier SID — the register/readback
engine on the CPU tick, reSID in an audio tier the 6502 cannot observe.
**Origin:** the owner's question, 2026-09-17, followed by an API negotiated with the UE2 session
the same day. **Both sides closed the contract before either wrote a line of code**, which is the
point: the interface here was agreed, not discovered by one side and absorbed by the other.

---

## §1 Why one SID is a single static

A U64-II decodes **four** SID targets — socket 1, socket 2, UltiSID 1, UltiSID 2 — and the
firmware assigns their addresses at runtime, rewriting them on every C64 reset. TRX64 has one
SID, at `$D400`, mirrored across `$D400-$D7FF` by a `& 0x1f`.

The blocker is **ours, not reSID's**. `vendor/resid/sid.cc` is stock reSID and `SID` is an
ordinary C++ class; VICE constructs one per chip (`resid.cc:94`). What prevents it here is our
own flat-C shim: `resid_shim.cc:34` holds a single `SID g_sid;`, the twenty `extern "C"`
functions take no handle, and `resid_ffi.rs` enforces the arrangement with a process-wide
`RESID_GUARD` that a `Resid` holds for its whole lifetime. A second `Resid::new` does not run
slowly. It **deadlocks**.

So this spec is mostly the removal of a restriction we imposed, plus the bookkeeping that N of
anything needs.

## §2 What is *not* the problem, because I got this wrong twice

I claimed — in this repo's notes and to UE2 — that two reSID instances are not
sample-synchronous, and that mixing was therefore the expensive open question. **That is false,
and it is recorded here so nobody rebuilds the design around it.**

`sid.cc:982`: `next_sample_offset = sample_offset + cycles_per_sample`, and `cycles_per_sample`
is computed once in `set_sampling_parameters` from `clock_freq`/`sample_freq` alone. The sample
cadence is **content-independent**. Two instances with identical configuration, fed the identical
boundary sequence, return identical sample counts and cannot drift apart on what they are
playing. VICE belts-and-braces it anyway by passing chip 0's produced count as the bound for
chips 1..n (`sound.c:467`).

The comment I reasoned from (`Resid::emit`: "reSID owns fractional sample timing… or it lags
cumulatively → pitch drift") is about capping a **single** engine's delta. It says nothing about
two engines diverging.

Mixing is therefore elementwise per boundary. Where TRX64 mixes at all (standalone and the
daemon, not UE2), the rule is VICE's `sound_audio_mix` (`sound.h:225`) — the **int16** branch,
not the `SOUND_SYSTEM_FLOAT` one that carries its own "FIXME: fix mono stream to stereo mixing
next". It is not associative, so chips fold in index order.

## §3 Design

**D1 — the shim takes handles.** `resid_new()` / `resid_delete(h)`, and each of the twenty
functions takes one. The current names stay as wrappers on handle 0, so `resid_oracle` is
untouched and byte-identity with c64re's WASM build survives unchanged. `RESID_GUARD` goes. **No
vendored GPL source is touched** — the shim is ours; reSID is not modified.

**D2 — the decode table: the host resolves, the core consumes.** `Machine` takes
`[(start, end, chip, precedence)]` through a setter, and learns nothing about U64 registers. UE2
keeps all of that on its side: the `>>4` encoding, the A11..A4 masks, `EMUSID_SPLIT`/`ADDRSEL`,
the enable bits and `0x01 = unmapped`. **No VICE defaults are baked in** — not
`Sid2..8AddressStart`, not `SOUND_SIDS_MAX`. An empty table means exactly today's behaviour:
everything in `$D400-$D7FF` masked onto chip 0.

Two consequences, both deliberate: the setter is callable at any time, because the firmware
rewrites the mapping on every C64 reset; and **`cold_reset` does not clear the table**. A table
that cleared itself would race `run_reset_task`.

**D3 — one `Sid6581` per chip.** This is the **6502's** view, not audio. Each chip gets its own
32-byte register file and its own oscillator/envelope model, so `$D41B`/`$D41C` (OSC3/ENV3) read
per chip. Chip 0 is bit-identical to today.

**D4 — the write trace carries `(chip, reg, value, clk)`.** Today `Sid6581::set_write_trace`
takes `FnMut(u8, u8)` — register and value, with neither address nor cycle. With N chips anywhere
in `$D400-$D7FF` and `$DE00-$DFFF`, a host cannot tell which instance to clock. The raw address
is deliberately **not** passed: the host built the table, so chip plus register gives it back.
Three installing call sites exist outside `sid.rs`, all in our own daemon
(`streaming.rs:333`, `:395`, `main.rs:15929`).

**D5 — a host may answer reads for a chip, and peek beside it.** An emulated ARMSID in its
configuration mode must answer `$1B`/`$1C` from the host's protocol, not from our `Sid6581`.
So a chip may carry a host `read` **and** a side-effect-free `peek`, both returning `Option<u8>`,
`Some` winning — the same shape `ExpansionDevice` has under 850, so the codebase keeps one idiom
rather than growing a second.

The peek is not optional garnish. Without it the monitor would show our register shadow while the
C64 receives the host's answer, and a quiet disagreement between the two is exactly what costs
someone an afternoon. It is cheap to honour: there are precisely **two** real bus read sites
(`full.rs:406`, `lib.rs:423`), both on `&mut` bus structs, and the peek paths (`lib.rs:2180`,
`:2241`) do not route through `sid.read` at all — they read the shadow directly. A host callback
therefore **cannot** fire on a peek, structurally rather than by discipline.

**D6 — a read-only envelope getter, from the fastsid model.** `Sid6581` already keeps
`adsr_value` per voice; it is what answers `$D41C`. reSID separately keeps `envelope_counter[3]`
in its own state, and the two will not agree exactly. The getter takes the fastsid value: its
only consumer is the firmware's LED strip (`C64_VOICE_ADSR`), which is cosmetic, and using the
model that already answers `$D41C` keeps the two consistent at no cost.

**D7 — UltiSID is not a plain 6581/8580, and the gap is stated, not faked.** Per core the
firmware writes `WAVES` (6581 vs 8580 combined waveforms), a resonance switch `RES`, a digi level
`DIGI` (0..3), and a **1024 × uint16 filter curve** into filter RAM, generated from
`sid_coeff.c`. `WAVES` maps onto reSID's chip model and is honoured. **The resonance switch, the
digi level and the filter curve have no reSID equivalent, so TRX64 models none of them**, and
this sentence is the whole of the position. No silent approximation: an invented filter curve
would be this week's UCI defect with better manners — a plausible-looking value standing in for a
fact nobody checked.

**D8 — snapshots and the VSF export count chips.** `capture_sid`/`restore_sid` become a list,
and `vsf_export.rs:172` stops writing `num_sids` as a hardcoded 1.

## §4 Gates

- **One SID is bit-identical.** `resid_oracle` passes unchanged, and a machine with an empty
  decode table produces the same PCM and the same `$D41B`/`$D41C` as before.
- **Two instances stay aligned.** Fed the same boundary sequence, two `Resid` with identical
  config return the **same sample count**, over a run long enough for a drift of one sample to
  show. This is §2's claim under test rather than in a comment.
- **The table routes.** A write to a mapped window reaches that chip's register file and no
  other; `$D41B`/`$D41C` read per chip; an unmapped address is unchanged behaviour.
- **`cold_reset` leaves the table alone** — the race against `run_reset_task`, asserted rather
  than assumed.
- **A host read answers and a peek does not fire it.** The override returns its byte on the bus
  path, and the monitor's peek reaches the host's `peek`, never its `read`.
- **The precedence bool actually flips the answer** in `$DE00-$DFFF`, against a device on the
  expansion chain at the same address.

## §5 UE2

UE2 builds: the UltiSID register face (`0x10180008..0x12`, `0x29`, `WAVES`/`RES`/`DIGI`, the
filter RAM), the resolution of those registers into the table it hands us, its ARMSID protocol
behind D5's override, and its own mixer (`0x10100500`, ten channels with a pan law). It drives
`Resid` directly — `Resid::new(ResidConfig)`, `.emit(n)`, `clock_silent` — plus
`Sid6581::set_write_trace`, and uses neither `trx64-ffi` nor `SidAudioEngine`. Per-chip
`SidAudioEngine` and `take_pcm_chip` are therefore **ours alone**, for the daemon and standalone,
and form no part of the agreed contract.

## §6 Not in this spec

Stereo panning and the U64's mixer (UE2's, and mono on their side today). The ARMSID
configuration protocol (UE2's). The filter curve, the resonance switch and the digi level (D7 —
not modelled anywhere). Any change to `trx64-ffi`'s mono `audio_drain`, which nobody in this
contract consumes.

## §7 Open

**Where a SID mapped into `$DE00-$DFFF` sits against the expansion chain.** Today that range
goes through `port_read`, which takes the cartridge byte and then the chain — profile device,
host device, cartridge, open bus (850 D2) — and a SID there meets UE2's sampler at `$DF20+`, the
UCI and the REU's mirror.

Both sides believe the FPGA decodes SID ahead of the cart slot, and **neither can show it**: UE2
checked, and the U64-II top level that would place the decode is not in the open firmware tree,
while the open U2+ top has no UltiSID. So D2 carries the precedence **per table entry with no
default**, and the host states it per mapping.

That is a deliberate application of what this repo learned on 2026-09-17: the UCI pointer defect
was an unverified assumption, and its gate confirmed it because the test was written from the
same assumption. A configurable precedence cannot repeat that. A hardcoded one can.
