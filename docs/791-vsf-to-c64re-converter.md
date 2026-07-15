# Spec 791 — VSF → `.c64re` Converter (one-way import onramp)

**Status:** PROPOSED (2026-07-15). **Repo:** TRX64.
**Shared cross-repo numbering** (registry = C64RE `specs/README.md`).
**Depends on:** existing `crates/trx64-core/src/vsf.rs` partial VICE loader
(`load_vice_vsf`), the `.c64re` native snapshot (Spec 707), the cartridge mappers
(cart.rs), the vice1541 drive, the literal-port VIC.
**Doctrine:** TS + VICE retired as ORACLES (2026-07-15) — but VICE VSF stays a
useful **input** (one-way onramp). This is *ingest VICE state, then do more with
it in `.c64re`* — not "match VICE going forward".

## Motivation

We can already run any `.crt`/`.d64`/`.g64` from cold. What we cannot do is
**ingest an arbitrary VICE machine state** — a `.vsf` a user (e.g. Boris,
mid-EasyFlash-debug) captured in VICE. Today `vsf.rs::load_vice_vsf` restores only
MAINCPU / C64MEM / CIA1 / CIA2 / SID / the VIC-II head, silently **ignores**
`C64CART`, the drive, the VIC pipeline, the keyboard, and reports `errors=[]` —
so a caller mistakes an inspection-only import for a resumable machine. That
"errors=[]" footgun is exactly what turned a partial import into a phantom
"EasyFlash execution divergence" (issue report 2026-07-15; the EF cart itself
cold-boots correctly on current main — verified).

**The design (owner's, 2026-07-15): a standalone converter layer** `VSF → .c64re`.
Parse every VSF module, map it onto our `Machine` state, emit a `.c64re`. The
runtime stays **untouched** (it already reads `.c64re`). One entry:
`trx64cli convert-vsf <in.vsf> <out.c64re>`. Boris does `convert-vsf` + `undump`.

## Direction — import first, export ADDED (owner, 2026-07-15: "Rest und retour")

Import fills a **superset** (VICE's machine-state ⊂ `.c64re`) — clean. Export was
first deferred as a lossy downgrade; the owner then asked for the **return trip**
too (`.c64re` → `.vsf`, so a c64re state can go BACK into VICE — Boris's interop),
accepting the losses. **Export is now IN scope (791.5 below).**

The two honest caveats stand — they shape *what* export emits, not *whether*:
- **Lossy by definition:** `.c64re`'s extras — JAM-freeze, the reverse-step
  delta-ring, rewind cpu-history, the checkpoint ring, embedded PNG frames, trace,
  media-embed, provenance — have **no** representation in VSF (a flat snapshot with
  no notion of history/time-travel/archive). Export simply **drops** all of it; the
  VSF carries the resumable machine core only. That is acceptable for the interop
  use case (hand a running machine to VICE), which is all export is for.
- **The VIC tail:** VICE's `read_module` walks the VIC-IISC fields sequentially and
  rejects a short/mis-sized module, so export must emit the WHOLE module incl. the
  `raster_snapshot` draw-buffer + `draw_cycle` pipeline. But that draw-buffer is
  VICE's OUTPUT framebuffer, which VICE **re-derives** as it runs — so export emits
  it **zeroed at the correct x64sc geometry** (a one-frame redraw glitch on load,
  then exact). Our own viciisc-faithful state supplies every other field verbatim.

Validation is possible: a real **VICE x64sc binary** is on disk
(`.../vice/vice/src/x64sc`), so the export round-trips through actual VICE (load the
emitted `.vsf`, resume, screenshot) — not just through our own loader.

## Key insight — our own fidelity lowers the effort

Because the runtime is itself a **VICE-faithful port** (literal-port VIC,
`vice1541` drive, VICE-shaped CIA/CPU), our state models are structurally **close**
to VICE's snapshot chunks. The converter is mostly **tedious field-mapping, not
deep reconstruction** — cheaper than the `vsf.rs` "cannot reconstruct" comment
implies.

## The coarse-VIC cut

For "continue this game/cart from here" the VIC **micro-pipeline is not needed**. A
coarse VIC restore — 64 registers + raster line + `bad_line` state — resumes the
vast majority of cases. The cycle-exact viciisc pipeline only matters for
mid-raster-effect resume (rare). **Restore the VIC coarse; do NOT reconstruct the
micro-pipeline.** This is the concrete "we do more than VICE": ingest the coarse
state, our runtime (trace/rewind/scrub) takes over.

## Module tiers (effort)

Already restored by `load_vice_vsf` (reuse): MAINCPU / C64MEM / CIA-public / SID /
VIC-head.

| Module | Effort | Note |
|---|---|---|
| MAINCPU full 64-bit clock | trivial | today only low 32 bits — one field |
| C64MEM | done | RAM |
| KEYBOARD | small | matrix, few bytes |
| **C64CART (EasyFlash)** | small-mid | `EasyFlashMapper` already has bank / register02 / jumper / IO2-RAM / lo_flash / hi_flash / flash-FSM — translate VICE's C64CART chunk into those. **The slice that unblocks EF continuation.** |
| CIA1/CIA2 full | mid | timer / alarm deadline / delayed-IRQ pipeline (our `cia.rs` is VICE-shaped) |
| DRIVE8 / DRIVECPU / VIA1 / VIA2 / GCR | mid | `vice1541` IS a VICE port → close state models, field-map |
| SID full (reSID state) | small-mid | or accept regs-only (audio re-derives on resume) |
| VIC micro-pipeline | **skip** | coarse-VIC cut above |

## What is NEW (this spec)

### 791.1 — extend `load_vice_vsf` to a faithful-enough restore
Restore, in addition to today's set: full 64-bit clock; **C64CART (EasyFlash
first)** → the concrete mapper's continuation state; KEYBOARD; the coarse VIC
(regs + raster line + bad_line). CIA-alarms + drive are follow-up slices (below).
Each restored module contributes to a **fidelity result** (791.3).

### 791.2 — `trx64cli convert-vsf <in.vsf> <out.c64re>` (+ `--json`)
Load the `.vsf` into a fresh `Machine` via 791.1, then dump a `.c64re` (707). No
daemon, isolated process. `--json` returns `{ loaded[], coarse[], absent[],
fidelity }`. This is Boris's entry point (then `sandbox --seed` / `undump`).

### 791.3 — fidelity classification (retire the `errors=[]` footgun)
The load returns an explicit fidelity value — **`faithful` | `partial` |
`inspection-only`** — plus per-module `loaded` / `coarse` / `absent` lists.
Ignoring `C64CART` (when a `C64CART` module is present) or restoring the VIC only
coarse ⇒ **not** `faithful`; the result says so. A caller can never again read a
non-resumable import as resumable. (This is the honesty-fix the issue report asked
for, delivered for free by the converter.)

### 791.4 — native-VSF detection by header/module structure
Detect a real VICE VSF from the 58-byte header + module walk, **not** by searching
for the `SIDEXTENDED` marker string. Expose the same detection on the CLI path.

> **Import status (2026-07-15):** 791.1a/b/c largely landed. On top of regs/clock/
> EF/keyboard, the VIC-IISC import now also restores **colour RAM** (module offset
> 761 → `ram[$D800]` AND `io_shadow[$0800]` — the full-machine VIC reads the latter;
> missing it rendered white text/HUD black), **ysmooth** (offset 689 → the 7px
> vertical offset), and **raster_cycle/cycle_flags/raster_line**; the CIA import
> **re-arms the timer alarms** (`Cia::restore_rearm_alarms`) so a running timer keeps
> firing. A mid-game EF Wasteland `.vsf` now renders **structurally 100%** vs the
> VICE screenshot (only the palette differs — ours is colodore, by design).

### 791.5 — `.c64re` → VSF EXPORT (the return trip, owner 2026-07-15)
Emit a **VICE-x64sc-loadable** `.vsf` from a `.c64re` (or the live machine).
`trx64cli convert-c64re <in.c64re> <out.vsf>`. A NEW writer (`save_vice_vsf`) — the
existing `save_vsf` emits the *c64re-own* compact framing, which VICE cannot load;
export emits each module in VICE's **exact** `*_snapshot_read_module` field order:
`MAINCPU`, `C64MEM` (pport + 64K + pport tail), `CIA1`/`CIA2`, `SID`, `VIC-IISC`
(model + regs + raster + vbuf/cbuf/gbuf/dbuf + ysmooth + collisions + vc/rc/vmli +
lightpen + …+ **colour_ram** + sprites + `draw_cycle` pipeline + `raster_snapshot`),
and `C64CART`+`CARTEF`+`FLASH040EF` for an EF cart. The `raster_snapshot` draw-buffer
is emitted **zeroed at the x64sc geometry** (VICE re-derives the picture on the first
frame). **Lossy by design:** the `.c64re` history/ring/trace/provenance extras are
dropped — a VSF is a flat machine core. Validated by loading the emitted `.vsf` in the
real **VICE x64sc** binary and resuming.

## Non-goals

- **No cycle-exact VIC draw-buffer export** — the `raster_snapshot` framebuffer is
  emitted zeroed (correct geometry); VICE re-derives it (one-frame redraw). Every
  other VIC field is exact.
- **No `.c64re` extras in the VSF** — JAM/rewind/ring/PNG/trace/media-embed/
  provenance are dropped on export (VSF has no representation; by design).
- **No cycle-exact VIC micro-pipeline** reconstruction on IMPORT (coarse-VIC cut).
- No TAPE/DATASETTE restore (we don't emulate tape; low value).
- Not a change to the `.c64re` format (707) — the converter emits it as-is.

## Acceptance

1. `trx64cli convert-vsf in.vsf out.c64re` produces a `.c64re` that `undump` /
   `sandbox --seed` loads and runs.
2. Round-trip a disk-game VSF sample (`samples/*.vsf`, gitignored) → `.c64re` →
   boot: CPU regs + RAM match the VSF; the machine continues (does not warm-start).
3. **EF slice:** a VSF carrying a `C64CART` EasyFlash module converts with the
   bank / register02 / jumper / IO2-RAM / flash restored — verified against a
   synthetic EF-cart VSF (a real EF `.vsf`, e.g. Boris's, validates end-to-end when
   available).
4. Fidelity: a VSF whose `C64CART` is dropped, or VIC restored coarse, returns
   `partial`/`inspection-only` with the module lists — never `faithful` / `errors=[]`.
5. `convert-vsf` and the direct Rust API accept the same native VICE VSF files.
6. **Export (791.5):** `trx64cli convert-c64re in.c64re out.vsf` produces a `.vsf`
   the real **VICE x64sc** binary loads and resumes; a round-trip
   `.c64re → .vsf → VICE (screenshot)` matches the `.c64re → our render`, and
   `.c64re → .vsf → our `load_vice_vsf`` returns to the same machine core state.

## Build order (slices)

1. **791.3 fidelity result + 791.2 `convert-vsf` skeleton** over today's partial
   loader — immediate honesty-fix + the CLI onramp, even before more modules land.
2. **791.1a: C64CART (EasyFlash) + full clock + KEYBOARD** — the high-value slice;
   unblocks EF continuation. **DONE.**
3. **791.1b: coarse VIC (colour RAM + ysmooth + raster).** **DONE** (structural 100%).
4. **791.1c: CIA alarms** **DONE**; **DRIVE8** import — follow-up.
5. **791.5 EXPORT** `.c64re → .vsf` (the return trip): the VICE-exact writer +
   `convert-c64re` CLI, validated against the on-disk VICE x64sc. Easy modules first
   (MAINCPU/C64MEM/CIA/SID), then the VIC-IISC module (+ zeroed draw-buffer tail),
   then EF cart; VICE-load-tested at each step.
