# Spec 794 — Whitebox Component-Diff (equivalence verdict + exclusion mask)

**Status:** PROPOSED (2026-07-16). **Repo:** TRX64 (capability) + C64RE (meaning/MCP).
**Board:** row 794 on `C64RE:specs/README.md`. **Next free after: 795.**

**Base it extends:** Spec **246** (save-state semantic diff — `snapshot_diff.rs`
`diff_snapshots(a,b) -> Value`, mirrored TS `snapshot-diff.ts`). 246 already emits
component/register **deltas** (RAM ranges, CPU named regs, CIA/CIA2/VIC/SID per-register,
PLA banking, drive cpu/via1/via2/head, IEC bus). 794 does **not** rebuild that — it turns
the delta list into a masked **equivalence verdict** and completes coverage.

**Yardstick:** `docs/usecases-runtime-dev-sandbox.md` priority #2 — the *whitebox
component diff* that makes the fan-out's N candidates **evaluable** (not a byte-hash /
VICE-oracle outcome compare). Serves UC4 (refactor-equivalence: "components identical →
behaviour-preserving, **proven**"), UC1 (cheat: which RAM/chip the patch actually moved),
UC3/UC5, and the autonomous rank step.

---

## Substrate — checkpoint-level, NOT the 246 VSF-module diff (grounded 2026-07-16)

**Corrected after reading the code.** The Spec 246 `diff_snapshots` reads the *c64re-own
VSF buffer* (`vsf::save_vsf`), which emits ONLY `MAINCPU, C64MEM, CIA1, CIA2, SID,
DRIVECPU, IECBUS, VIC-II, KEYBOARD`. It carries **no Color RAM, no Floppy RAM (DRIVERAM),
no drive VIA1/VIA2, no internal chip state** (the drive VIA/GCR diffs already come back
empty for exactly this reason). The owner requirement "**inkl. Floppy RAM**" is therefore
**structurally impossible** on the VSF-module substrate, regardless of masking.

The **`RuntimeCheckpoint`** (Spec 705.A — the one full-state object `.c64re` persists) DOES
carry everything: `color_ram[0x400]`, drive RAM, and every internal chip field (VIC
raster/sequencer `vc`/`vcbase`/`rc`/`vmli`/`bad_line`/sprites/`draw_cycle`, CIA
timer/latch/cnt/clk + TOD + SDR, SID envelope/LFSR). So the whitebox diff operates on the
**checkpoint**, not on VSF modules.

**This flips the earlier "extend 246" plan.** 246's `diff_snapshots` stays untouched for the
legacy monitor/VSF path; 794 is a **new `checkpoint_diff` module**. Not a real fork — it is
the *only* substrate that satisfies "inkl. Floppy RAM" (the module route would mean
re-inflating the deliberately-lossy VSF serialization, Spec 791).

**Mechanism — classify a structural JSON diff (full coverage by construction):**
1. Serialize checkpoint A and B each to `serde_json::Value` (the struct is already serde,
   TS-mirrored). Every field is present → nothing can be silently uncovered.
2. Walk both trees; collect differing **leaf paths** with `{path, before, after}`; collapse
   contiguous array runs (RAM/colorram/driveram) into ranges like 246's `diff_ram`.
3. **Classify** each path by prefix → `component` (`vic.*`→`vic`, `cia1.*`→`cia1`,
   `drive.ram[..]`→`drive.ram`, `colorRam[..]`→`colorram`, `ram[..]`→`ram`, …) + a `lane`
   tag for volatile fields. An unclassified path lands in `other` (visible, never dropped).
4. Apply the `exclude` mask over paths/components/lanes → the surviving set drives the
   verdict.

Draw buffers / framebuffers (`dbuf`, `literalPortFb*`) are non-state (re-derived) → tagged
`lane:framebuffer`, in the default `equivalence` mask, listed under scope boundary below.

---

## Why 246 is not enough

246 answers "what are the raw deltas between snapshot A and B". The sandbox use cases
need a different question: **"is B equivalent to A, modulo what I chose to ignore?"** Five
concrete gaps:

1. **No verdict.** No `identical` boolean, top-level or per-component. UC4 needs a *ruling*
   ("behaviour preserved"), not a delta table a human eyeballs. This is the core gap.
2. **No mask.** Two scratch runs (787 v1) always differ in volatile lanes — the cycle
   counter always advances, the raster line moved, the SID noise LFSR churned. Naive diff =
   forever "differs". Equivalence must ignore caller-chosen lanes/regions.
3. **Numbers, not names.** CIA/VIC/SID deltas print reg `$0D`, not `ICR` / `$D011`. CPU is
   named; chips are not.
4. **Silent register window.** Only the first 16 (CIA) / 80 (VIC) / 32 (SID) module bytes
   are compared. Internal state past the window (CIA timer latches + alarms + TOD, VIC
   raster/sequencer/sprite/badline, SID envelope + LFSR counters) is **not** diffed and the
   omission is not stated.
5. **Missing whole components.** **Drive RAM (`DRIVERAM`, the 1541's 2 KB $0000–$07FF) and
   C64 Color RAM (1 KB) are diffed nowhere.** Confirmed absent from `snapshot_diff.rs`.

## The owner requirement (2026-07-16)

> "Schon A. Ich muss halt sagen können **was beim Diff ausgenommen sein soll**. **Inkl.
> Floppy RAM!**"

The **exclusion mask is a first-class, caller-supplied input** — by component, by address
range (Floppy RAM = `driveram $0000–$07FF`, fully excludable), and by named volatile lane.
Everything excluded is **echoed back** in the verdict; nothing is silently dropped.

---

## Model

### 1. Verdict (the new output node)

Added to the existing 246 diff `Value`, non-breaking (old fields stay):

```jsonc
"verdict": {
  "identical": false,                       // all IN-SCOPE components equal after the mask
  "differing": ["ram", "cia1"],             // in-scope components that differ
  "excluded":  [                            // exactly what the mask removed (never silent)
    "component:sid",
    "range:driveram $0000-$07FF",
    "lane:cycles", "lane:raster"
  ],
  "scope": ["cpu","ram","colorram","cia1","cia2","vic","sid","pla",
            "drive.cpu","drive.ram","drive.via1","drive.via2","drive.head","iec"]
}
```

- **Machine-consumable** — the fan-out ranker reads `verdict.identical` +
  `verdict.differing` per candidate; no text parsing.
- Per-component the diff nodes gain `"identical": bool` + a one-line `"summary"`.

### 2. The exclusion mask (`exclude` input)

```jsonc
"exclude": {
  "components": ["sid", "drive.ram", "iec"],           // whole components off
  "ranges": [                                           // address windows off
    { "space": "c64ram",   "from": "0x0100", "to": "0x01FF" },   // stack
    { "space": "driveram", "from": "0x0000", "to": "0x07FF" },   // ENTIRE floppy RAM
    { "space": "colorram", "from": "0x0000", "to": "0x03FF" }
  ],
  "lanes": ["cycles", "raster", "sid_noise", "open_bus"],   // named volatile presets
  "presets": ["equivalence"]                                // bundled lane/range set
}
```

- `space` ∈ `c64ram | colorram | driveram | drivezp`. Floppy RAM masked via
  `space:"driveram"` over any/all of its 2 KB.
- `lanes` — named non-behavioural state: `cycles` (both CPU cycle counters), `raster`
  (VIC raster line/cycle), `sid_noise` (noise LFSR), `open_bus` (last-bus-value shadows).
- `presets.equivalence` = `{ lanes: [cycles, raster, sid_noise, open_bus] }` — the default
  set for "same-behaviour" scratch-vs-scratch compares. Omit `exclude` entirely → **strict**
  mode = every component in scope = the Spec 792 byte-exact bar.
- **Echo law:** every applied component/range/lane appears under `verdict.excluded`. A mask
  that matches nothing is reported (`excluded` entry with a `"(matched 0)"` note) — no
  silent no-op.

### 3. Full curated coverage (choice A)

Each component compared via a **curated field-map** (named field → module offset/accessor),
not a raw byte window. New/extended coverage:

| Component | Adds over 246 |
|---|---|
| `cpu` | already full (regs+flags+sc) — keep |
| `ram` | 64 KB (unchanged) |
| **`colorram`** | **NEW — 1 KB nybbles ($D800 shadow), not the RAM-under-IO bytes** |
| `cia1`/`cia2` | full: timer A/B value+latch, TOD, SDR, ICR mask+data, CRA/CRB, alarm/armed |
| `vic` | full: 47 regs **+** internal raster line/cycle, sprite MC/DMA, badline, light-pen latch |
| `sid` | full: 25 regs **+** envelope state/rate, noise LFSR, gate/sustain flags |
| `pla` | banking config string (unchanged) |
| `drive.cpu` | full 1541 6502 (unchanged) |
| **`drive.ram`** | **NEW — 2 KB DRIVERAM $0000–$07FF (drivecode + GCR buffers)** |
| `drive.via1`/`via2` | full VIA registers + timers + latches |
| `drive.head` | halftrack + bit-offset (unchanged) |
| `iec` | atn/clk/data + edge count (unchanged) |

Volatile fields (raster, cycle counters, LFSR) are **covered** (so strict mode sees them)
but carry a `lane` tag so the `equivalence` preset can mask them by name.

### 4. Semantic names (Leitregel split)

- **TRX64** emits each changed field with a **stable field-key** and the canonical hardware
  name where it is chip-canonical: `{ "key": "vic.d011", "name": "control1", "before": …,
  "after": … }`. Hardware register names are faithful-model reference, not project meaning,
  so they live with the field-map in the core.
- **C64RE** `enrich` maps the field-key → a richer human/agent label + doc link
  (`c64ref_lookup`), and attaches any project-semantic label. Meaning lands in C64RE, last.

### 5. Scratch-vs-scratch wiring (feeds the fan-out)

- 787-v1 `trx64cli sandbox` / `boot` already dump a `.c64re`. 794 diffs **two `.c64re`
  files** (or two checkpoint anchors) with an `exclude` mask → `verdict`.
- The autonomous ranker (yardstick endgame) calls: for each candidate overlay, run in a
  scratch instance → dump → `diff baseline candidate --preset equivalence
  --exclude <goal-specific>` → collect `verdict` → rank by
  `(goal component changed as intended) ∧ (fewest unintended differing components)`.

---

## Surfaces (complete — no `absent`)

- **Core** (`trx64-core`): NEW `checkpoint_diff` module — `diff_checkpoints(a, b, exclude)
  -> Value` over two `RuntimeCheckpoint`s (serialize → classify structural JSON diff →
  verdict), with the path→component/lane classifier + `exclude` mask. Full coverage incl.
  `colorram` + `drive.ram` + internal chip state by construction. **246 `snapshot_diff.rs`
  stays untouched** (legacy VSF/monitor path).
- **Daemon** (`trx64-daemon`): monitor `diff <idA> <idB> [--exclude …] [--preset …]`; WS
  `runtime/diff_checkpoints` + `snapshot/diff` gain `exclude`/`preset`, return `verdict`.
  `diff_checkpoints_by_id` threads the mask.
- **CLI** (`trx64cli`): `diff A.c64re B.c64re [--exclude space:from-to] [--component X]
  [--lane X] [--preset equivalence] [--json]` — for headless fan-out.
- **C64RE MCP** (`runtime_diff_snapshots`): add `exclude` (object) + `preset` params; return
  `verdict`; route to the TRX64 compute (retire the TS `snapshot-diff.ts` path per single-
  path once parity holds, or keep as oracle — decide at impl per 788 pattern). `enrich`
  gains field-key → label mapping.
- **TS parity note:** `snapshot-diff.ts` stays the parity oracle until the Rust `verdict`
  path is proven byte-parity on the 246 corpus; then MCP points at TRX64.

## Acceptance / gate

1. **Strict = 792.** No `exclude`: any single-byte / single-field change anywhere in scope
   (incl. colorram + drive.ram + internal lanes) → `verdict.identical=false` naming the
   component. Byte-exact bar preserved.
2. **Equivalence preset.** Two identical scratch runs of the same seed+overlay →
   `identical=true` (volatile lanes masked). A behaviourally-different overlay → `false`
   naming the differing component.
3. **Floppy RAM in/out.** A run that only touches DRIVERAM: default → `identical=false`
   (`drive.ram` differs); with `exclude.ranges=[driveram 0x0000-0x07FF]` → `identical=true`
   **and** `verdict.excluded` lists `range:driveram $0000-$07FF`.
4. **Echo law.** Every applied mask entry appears in `verdict.excluded`; a zero-match mask
   entry is reported, not dropped.
5. **Coverage audit test.** A fixture that mutates one field in *each* covered component
   (incl. the newly-added colorram, drive.ram, CIA/VIC/SID internal fields) → each is
   individually detected in strict mode. Proves no silent window.

## Scope boundary (stated, not silent)

Out of scope for the verdict (un-emulated or intentionally non-behavioural): tape/REU/
userport (not emulated), the VIC draw-buffer pixels (re-derived, not state), reSID
resampler internals (audio re-derives — Spec 791 note). Listed here so the boundary is
explicit per the "reach EVERY component or name the edge" rule.

## Cross-links

Spec 246 (base diff) · 792 (byte-exact fidelity gate = strict mode) · 787/788 (scratch
instances the fan-out diffs) · 762 (cheat-finder, first consumer) · yardstick
`docs/usecases-runtime-dev-sandbox.md` · concept map
`docs/concepts-snapshots-scenarios-overlays.md`.
