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

## Why one-way (no `.c64re → VSF` export)

Import fills a **superset** (VICE's machine-state ⊂ `.c64re`) — clean. Export is a
**lossy downgrade** and is also *harder*:
- **Lossy by definition:** `.c64re`'s extras — JAM-freeze, the reverse-step
  delta-ring, rewind cpu-history, the checkpoint ring, embedded PNG frames, trace,
  media-embed, provenance — have **no** representation in VSF (a flat snapshot with
  no notion of history/time-travel/archive). Export drops all of it.
- **Harder on the VIC:** import may take the VIC **coarse** (regs + raster line)
  and let our runtime fill the rest; export must emit VICE's **exact** viciisc
  pipeline encoding or VICE rejects/mis-resumes it. The "off-limits" VIC blob is
  worse for write than read.
- We are **leaving** VICE — no workflow needs VICE to consume our snapshots.

⇒ **Import only.** Export is explicitly out of scope (a strictly-worse format at
higher cost).

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

## Non-goals

- **No `.c64re → VSF` export** (see "Why one-way").
- **No cycle-exact VIC micro-pipeline** reconstruction (coarse-VIC cut).
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

## Build order (slices)

1. **791.3 fidelity result + 791.2 `convert-vsf` skeleton** over today's partial
   loader — immediate honesty-fix + the CLI onramp, even before more modules land.
2. **791.1a: C64CART (EasyFlash) + full clock + KEYBOARD** — the high-value slice;
   unblocks EF continuation.
3. **791.1b: coarse VIC (raster line + bad_line)** — resumable non-warm-start.
4. **791.1c: CIA alarms, then DRIVE8** — faithful CIA/drive continuation.
