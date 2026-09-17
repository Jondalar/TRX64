# Spec 855 — More than one SID

**Status:** PARTLY BUILT 2026-09-17 — D1 (handles in the shim) is built and gated; D2–D8 open. See §8.
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
**Seven** installing call sites exist outside `sid.rs` — `streaming.rs` ×2, `main.rs` ×3 and the
core's own `scramble_av_record` recording ×1, plus three that clear the hook. The figure said
THREE here until D4 was built, and that was my undercount from one early grep; it is also what UE2
was told when the interface was agreed. Not a large number either way, but a number in a spec is a
claim like any other.

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

## §8 As built — the whole of TRX64's half

**Shipped: the shim takes handles.** `resid_new()`/`resid_delete(h)` plus an `_h` form of every
entry point; the legacy names remain as wrappers on a default instance, so c64re's WASM build and
`resid_oracle`'s byte-identity are untouched. `RESID_GUARD` is gone, `Resid` owns its engine and
frees it on drop, and `resid_state_size` stays handle-free because `sizeof(SID::State)` belongs to
the type. Gate: `sid_multi_gate`, four cases, in `scripts/gate.sh`.

**Two defects found by building it, both mine, both in the spec's own subject matter.**

*The engine was born full of rubbish.* reSID's constructor does not initialise everything it owns —
the resampler's FIR ring is only ever written by `clock()`, which is why `resid_reinit` exists at
all. While the shim held one FILE-SCOPE instance that was invisible: a global lives in BSS and
starts zeroed. Moving the same object to the heap with `new Ctx()` runs the constructor and nothing
else, so those bytes became whatever the allocator last left there. Two engines built identically
then produced different audio, and an engine nobody had written to produced sound. Both were
observed, not theorised. `resid_new` now zeroes its storage and `resid_reinit_h` does the same
before rebuilding, which is what its own comment had been claiming all along.

*The snapshot blob differed from itself.* `resid_read_state` memset a local `State` and then
ASSIGNED the captured one into it, on the reasoning that the implicit copy-assignment writes only
the named members. The standard permits that reading but does not require it: for a trivially
copyable type the compiler may copy the whole object, padding included. It had survived because
both captures came through one identical call path; a heap context changed the path and the same
state captured twice differed by one byte. reSID's `State` has exactly one hole — `bool
hold_zero[3]` is three bytes and the `cycle_count` behind it needs four-byte alignment — and it is
now zeroed explicitly via `offsetof`, so determinism is constructed rather than lucky.

**One gate case of mine was too strict and was corrected, not the code.** The first draft demanded
byte-identity between successive engines. It failed at ±2 LSB: reSID builds its filter and FIR
tables with libm at construction, and rebuilding them in one process does not land on the same last
bit. `resid_oracle` had already met this and bounded it at `INPROC_RECONSTRUCT_BOUND`; the case now
asserts the exact sample COUNT and that bound, which is the honest claim. 855 also makes that
residual routine rather than exotic — several engines per process is the normal case now — and the
oracle's comment saying otherwise was corrected with it.

**Shipped in slice 2: D2, D3 and D6.** `SidChip` and `SidMapping`, resolved by `resolve_sid`;
`Machine::set_sid_map` takes the host's resolved windows and the crate learns nothing about U64
registers. Chip 0 stays `sid_regs` + `sid` and extra chips live in `sid_extra` — the asymmetry is
deliberate, because making chip 0 a list element would force the snapshot, the VSF export, the
monitor and the bus to index for no behavioural gain. `cold_reset` resets every chip and KEEPS the
table, per D2. The routing went everywhere the old `& 0x1f` was and not merely the obvious two: bus
read and write, `poke_io`, both monitor peek paths, and the reverse-debug UNDO path, where putting
a second chip's write back into chip 0's shadow would corrupt both silently and only while
rewinding. `$DE00-$DFFF` honours §7's per-window bool, with the simple reading written into the
code: ahead means the SID answers first, behind means the chain wins outright, because `port_read`
resolves to a byte rather than an `Option`. D6's envelope getter reads the fastsid model.

**A defect this slice UNCOVERED but did not cause.** `snapshot_roundtrip_fidelity` was red on
`main` — `cia1/cia2.todticks` off by ~17 300 cycles on every scenario — and had been since TOD
stopped being a countdown earlier the same day. The restore re-based the target clk by recomputing
a WHOLE period, so a machine captured part-way through a tenth came back with a fresh one. Capture
stores the pair (`todticks`, `todclk`), so the remaining interval is their difference; that is what
is restored now, and a pre-TOD dump still gets the full period. Bounded by bisection rather than
assumed: green at `5f93646`, red at `790c2c9`. It hid because that suite was not in `scripts/gate.sh`
— neither was `resid_oracle`, which went red in slice 1 for a different reason. **Both are in the
gate now**, which is the actual fix for the pattern.

**Shipped in slice 3: D4.** The hook is `SidTrace` on `Machine`, carrying `(chip, reg, value, clk)`,
and `Sid6581` is back to being only what the 6502 can see — `Clone` and `Debug` are derived again.

It lives on the machine rather than the engine for two reasons, and the second is the one that
decided it. Per engine a host would install N hooks and re-install on every firmware remap. And
`Sid6581::write` is reached from the bus, from `poke_io` and from the isolated `SidBus`, so moving
the call to the bus dispatch — which is where the chip and the cycle are — would have silently
stopped tracing host pokes. A monitor write to `$D418` would have gone quiet with nothing to show
for it. One hook at the machine, fired by the bus dispatch and by `poke_io`, is the shape that
keeps every path. `SidBus` alone is not traced: it is an isolated test bus with no audio, and that
is a decision rather than an oversight.

**The gate caught one of mine.** The `poke_io` stamp first read `cpu6510.clk`, copied from the CIA
arms beside it. That field is a MIRROR, correct only after `cpu6510.clk = c64_core.clk` runs; the
bus stamps `c64_core.clk`, so a poke and a bus write would have reported two different clocks for
the same machine. The case that sets the clock to a recognisable number rather than asserting
`0 == 0` is what found it. (The CIA arms still read the mirror. That is older than this spec and
there is no evidence it is unintended, so it is recorded here and not quietly changed.)

**Shipped in slice 4: D5 and D8, and D7 resolved without code.**

**D5** is `SidHostAccess` on the machine: one `read` and one `peek`, each taking `(chip, reg)` and
returning `Option<u8>`, `Some` winning and `None` falling through — 850's precedence rather than a
second idiom. One pair for the machine and not one per chip, for D4's reason: per chip a host
would re-install on every remap.

**The two halves have different bounds, and that is the contract showing through the types.**
`read` is `FnMut` because the bus dispatch holds `&mut self` and a real read may advance the host's
protocol. `peek` is `Fn`, because `read_full` and `peek_lens` take `&self` — and a peek is
side-effect-free by definition, so being unable to mutate is the rule enforced rather than a
limitation. Changing those two to `&mut self` to allow an `FnMut` peek was the alternative and was
rejected: they are widely used public reads, and the restriction is the correct one.

The gate drives the read path with real 6502 code, deliberately. `read_full` is a PEEK and never
reaches the bus read, so a case built on it would claim to test the read override while testing the
peek one. And one case asserts that a peek does NOT run the read hook, which is the whole reason
there are two.

**D7 needs no code in TRX64, and that is the finding rather than a gap.** `WAVES` selects the reSID
chip model, and since D1 that is already per instance: `ResidConfig { model }`, one per engine, set
by whoever builds the engine — which for UE2 is UE2, driving `Resid` directly. The resonance
switch, the digi level and the 1024-entry filter curve have no reSID equivalent and are modelled
nowhere, which §3 D7 states and this repo means. Storing them so a host could set and read them
back unchanged would add dead fields that advertise a capability we do not have; a host that wants
to remember its own settings can remember them. The gap is stated, not furnished.

**D8** extends the `.c64re` snapshot: `SidChipSnapshot` for chips 1.., carried on `SidSnapshot` as
`chips` with `serde(default, skip_serializing_if = "Vec::is_empty")`. A dump written before 855
still deserialises, and a one-SID machine writes no new key at all — so the node stays
byte-identical for every existing c64re session.

**The VSF export deliberately still says one SID.** VICE's SID module puts `num_sids` in front of a
register block, and the layout for several is something I have not read. Writing a count this code
cannot back up would be the plausible-looking lie this repo keeps catching, so `vsf_export` is
unchanged and this sentence is why. UE2 does not use VSF.

**TRX64's half of 855 is complete: D1, D2, D3, D4, D5, D6, and D7 by resolution.** `sid_multi_gate`
has sixteen cases in `scripts/gate.sh`. UE2's half — the UltiSID register face, the resolution into
the table, its ARMSID protocol behind D5, and its mixer — is theirs and unstarted.
