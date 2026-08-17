# Concept — Snapshots · Scenarios · Code Overlays (the runtime time-machine model)

**Status:** REFERENCE / umbrella (2026-07-16). Not a numbered spec — it ties the
scattered specs into one conceptual map and names the GAPS we are closing peu à peu.
Ownership: runtime = TRX64; *meaning* (findings/recipes) = C64RE.

These three are ONE system, not three features. A **snapshot** is a point, a
**scenario** is a point plus a reproducible timed delta, a **code overlay** is an
intervention on a point whose effect you measure by diff — and all of it runs over one
timeline, on one state object, guaranteed bit-faithful on resume.

## The stack, bottom-up + the pipeline (owner, 2026-07-16 — corrects the layering)

The **ring is the FOUNDATION — it lies UNDER the `.c64re` snapshots**, not beside them.
Because it is always-on, you can **time-travel back and DUMP a `.c64re` at any past
moment — WITHOUT a replay.** That is the load-bearing capability: in testing you rewind to
the instant a bug/life-loss happens and mint the exact snapshot that would otherwise cost
a full recorded replay. Gold for debugging — and the **human tester handoff**: testers
(who do NOT use an LLM) rewind via the ring, dump a `.c64re` at the bug, and hand that file
to the dev/LLM. The ring is the man↔machine interchange.

```
RING (always-on capture) = the FOUNDATION
   ├─→ traces
   ├─→ reverse-debugging
   └─→ point-in-time snapshots: time-travel back + DUMP = a .c64re at any past
         moment, WITHOUT a replay        ← the tester's gold / the man↔machine handoff
        │ dump
        ▼
   .c64re  (portable point-in-time state = the currency)
        │ undump into a SANDBOXED TRX64 (scoped / scratch instance)
        ▼
   state  ──+ overlay (your code, inline-compiled)──▶  SCENARIO = state + overlay
        │ validate (WHITEBOX component diff)
        ▼
   derive the FINAL CODE (the delta for the real build)

   N scenarios in PARALLEL  =  N scratch instances (Spec 787: 1 live + N scratch)
```

The rest of this doc details each stage; **read the dependency as above** (ring →
`.c64re` → sandbox → scenario+overlay → validate → final code), not as the flat layer list
below.

---

## 0. The foundation — ONE state object

Everything stands on a single machine-state model, the `RuntimeCheckpoint` (Spec 705.A;
`runtime-checkpoint.ts` / `checkpoint_ring.rs`). Full-system, atomic at a CPU
instruction boundary: RAM + banking, CPU + `$00/$01`, CIA1/2, **SID synthesis state**
(not PCM), IEC core, IRQ/NMI, kbd/joy/paddles, the literal-port VIC + framebuffer, the
**maincpu alarm schedule**, the VICE-drive module + the mutable disk image, cart flash.

**Discipline: one object, many serializations — never a second state model.** Two
fidelity rules fall out and recur everywhere:
- **Framebuffer = a derivable shadow** → never stored per anchor; re-simmed on restore.
- **reSID synthesis = state (snapshotted); buffered PCM = transport (flushed on restore).**

---

## 1. Snapshots — point vs. timeline vs. navigation (three layers, often conflated)

| Layer | What it is | Specs |
|---|---|---|
| **`.c64re`** | The durable **currency**: `MAGIC C64RESNP` + sha256 + gzipped JSON `{manifest, checkpoint, mediaPayloads}`, media **embedded** (self-contained, portable). Deliberately too slow for any hot path — disk-persistence only. `dump`/`undump`; undump leaves the machine **paused**. NOT VSF internally. | 707 |
| **Ring** | Always-on bounded in-memory window, auto-captured ~every 0.5 s (25 PAL frames) at an instruction boundary. **Zero-alloc slab** (one flat `ArrayBuffer` → V8 never scans it → killed the BUG-049 audio "kratzen"). Sized for the **UI scrub filmstrip** (~10 s), NOT deep history. | 705.B / 765 / 772 |
| **Recorder + firehose** | The **timeline** (a stream, not points): off-thread SAB anchors (~3–4 min) + the CPU_STEP firehose `.c64retrace` = the **authority**, DuckDB = a rebuildable **index**. Fire-and-forget memcpy on the tick loop; all heavy work in a worker; slow worker drops oldest (benign gap, never backpressure). No pin — durability is an **explicit dump-from-anchor to `.c64re`**. | 766 / 726.B |
| **Scrub / rewind** | The *use*, not new storage: pause → scrub back over anchors → run on (floppy-atomic); filmstrip for the human, API-seek for the LLM. | 761 / 769 |

**Anchor + firehose data model (charter 746) — the load-bearing idea:** heavy ANCHOR
every 0.5 s + a light deterministic CPU_STEP stream between them. Invariant: **only the
CPU writes RAM**, so anchor-RAM + the instruction stream reconstruct every in-between
state byte-exact by re-sim; autonomous chip state (CIA/SID/VIC/drive-head) is anchored,
not streamed. The recorded PC stream doubles as a **checksum** — a re-sim mismatch is a
found port bug.

**Fidelity contract (792):** a restored snapshot MUST resume bit-faithful — enforced as a
self-enumerating gate: capture → restore → assert **byte-identical** (RAM + CPU + clk +
CIA + VIC + `cart.get_state()` + drive half-track + flash + keyboard) **and**
**N-cycle-identical continuation** (both run N cycles under a hashing observer). The
confirmed gap it caught: cartridge continuation state (bank/register/IO2-RAM/flash-FSM)
was dropped → a banked cart resumed at bank 0 (the field Wasteland `.c64re` landing in
the intro). Related lifecycle: 714 (mutable media in the ring), 793 (undump materializes
embedded media as real picker mounts + purge), 786 (power-cycle undump — fresh VIC/CIA
only from `Machine::new()`).

**VSF = the interop bridge, never the internal format (791):** VSF → `.c64re` is a
one-way **superset import** (VICE's state ⊂ `.c64re`); `.c64re` → VSF is added but
**lossy by design** (JAM-freeze, reverse-step ring, rewind history, checkpoint ring,
PNG frames, trace, provenance — no VSF representation). Two conceptual contributions:
the fidelity classification `faithful | partial | inspection-only` (retires the
`errors=[]` footgun) and the **coarse-VIC cut** (regs + raster + colour RAM resume the
vast majority; skip the micro-pipeline). Import + export now round-trip everything TRX64
emulates (C64 core + EF cart + 1541 drive + keyboard); un-emulated hardware
(tape/REU/userport/FS-drive) + `SIDEXTENDED` (audio re-derives from regs) are out of
scope by definition.

---

## 2. Scenarios — snapshot + timed inputs + cycle budget

A **scenario** is the atomic unit of *deterministic replay* (Spec 231):

```
Scenario = { startSnapshot, inputs[@cycle], cycleBudget, diskPath?, mode }
  → ReplayResult { ramHash, screenshotHash, traceHash, cyclesRan }
```

- **`ScenarioPlayer`** (`scenario_player.rs`) = a pure scheduler: stable-sort steps by
  absolute cycle, `tick(target, clk)` fires everything now due. Deterministic regardless
  of wall time. **`run_scenario`** (daemon): restore `startSnapshot` → build player →
  run `cycleBudget` in segments bounded by the next due input → `sha256(RAM)`.
- **Determinism guarantee:** *same build + same scenario → byte-identical* (RAM,
  screenshot, trace, end-snapshot). Why: **the emulator is a deterministic function of
  static inputs** (image bits + known ROMs + known CPUs). Enforced by: **cycle budgets,
  never wall time**; RNG seeded from **snapshot state, not the clock**; a random-source
  audit script.
- **Scenario ↔ snapshot:** a scenario *begins from* a snapshot ⇒ `scenario = snapshot +
  timed delta`. Reverse direction: the **snapshot tree (268)** — root = start snapshot,
  edges = applyPatch/runForward, any branch **promotable to a scenario**.
- **Payoff:** regression (250, self- + VICE-baseline, drift classes), batch / patch-sweep
  (271: "100 patch variants off the rewind tree, run each 100k cycles → which fixes it").

---

## 3. Code overlays — intervention on a restored anchor

A **code overlay** is a *runtime intervention*: patched bytes (a fix / cheat / crack)
applied **into RAM over a restored state**, so modified code executes **without ever
mutating the original disk/cart/PRG bytes** (immutable-original policy).

**The load-bearing mechanic (769 §3):** restore rolls RAM back to the anchor, so the
patch is undone on every iteration → the overlay must **live in the tool and be
re-applied after each restore**. That is what makes it a repeatable "what-if" rather than
a one-shot poke. Purpose: the fast runtime what-if **without rebuild → reboot → replay**
(Lykia bug, Wasteland Save/Restore).

Two forms, conceptually distinct:
- **Ephemeral run — `runtime_overlay_run` (769 L7, BUILT):** restore anchor → apply
  overlay (`raw {addr,bytes}` or `asm` assembled via the bundled assembler) → run bounded
  (opt. `until_pc`) → observe registers/reads. RAM-only. The proof verifies the mechanic:
  a same-anchor re-run with a *different* overlay yields different behaviour (restore
  undid the prior patch).
- **Recorded intervention branch (711 + 712 → merged into 776, DESIGNED):** the overlay
  as *one member* of a recorded, replayable branch object rooted at a pinned checkpoint,
  carrying ordered interventions with provenance (before/after, author, cycle). Rule:
  **no unrecorded live poke is an accepted experiment result.** Intervention union:
  `MemoryWrite | RegisterWrite | Overlay(code/data) | Breakpoint | InputEvent |
  MediaOverlay`. The genuinely-new capability: `diff_branch_outcome(baseline,
  intervention)` — the same run-point *with vs. without* the patch (reuses
  `diffCheckpoints` / `snapshot_diff`).

---

## 4. How it all composes — the through-line

```
.c64re (currency: one machine, portable, sha256+gz)          ← 707
   └ the SAME RuntimeCheckpoint object (705.A) also fills →
RING (in-mem, ~0.5s, zero-alloc slab, ~10s UI scrub)         ← 705.B / 765 / 772
RECORDER (off-thread SAB anchors) + .c64retrace → DuckDB      ← 766 / 726.B / 746  ┐ TIMELINE
SCENARIO = snapshot + timed inputs + cycle budget            ← 231 / 268
OVERLAY  = intervention on a restored anchor (re-applied)     ← 769 / 711 / 712 / 776
   navigated by SCRUB / REWIND / BRANCH / CHEAT              ← 761 / 769 / 712 / 762
guaranteed bit-faithful on resume                            ← 792
VSF ⇄ .c64re import/export (VICE interop, lossy out)         ← 791
```

The **snapshot tree (268)** is the visual bracket: root = snapshot, edges =
overlays/runForward, leaves → promotable to scenarios, "Compare" = hash-diff of two
branches. So: a **snapshot** is a point; a **scenario** is a point + a reproducible timed
delta; an **overlay** is an intervention on a point measured by outcome-diff. All three
live over ONE timeline, stand on ONE state object, and are made trustworthy by two
guarantees — **determinism** (scenario) and **fidelity** (792).

---

## 5. Status — built vs. designed (the GAPS to close)

**BUILT:** `.c64re` (707) · ring (705.B/765) · recorder (766) · binary timeline (726.B)
· scrub/resume (761) · **769 rewind + `runtime_overlay_run` + the human UI** — the
C64RE workbench ships the wired snapshot/time-travel UI (`ui/src/workbench/`:
`Snapshots.tsx`, `SnapshotTreeView.tsx`, `Filmstrip.tsx`, `ScrubTimeline.tsx`,
`SnapshotDetail.tsx`, `ScenarioInputTimeline.tsx`, ~840 LOC calling
`checkpoint/list|pin|restore|thumbnails|unpin` + `snapshot/dump` + the
`checkpoint_restored` event) — so the "769.5 filmstrip UI, LAST" note in the 769 spec
header is **STALE** (the UI landed via the UI-redesign; reconcile the spec status) ·
scenarios (231 / `scenario_player` / `run_scenario`) · restore-fidelity gate (792) ·
VSF ⇄ `.c64re` import+export incl. drive+keyboard (791).

**DESIGNED, NOT BUILT — the gap-closing backlog:**

1. ~~**776 — recorded intervention branch + outcome-diff**~~ — **SUPERSEDED 2026-08-12.**
   Its subject, the active experiment loop, is the CANDIDATE (796): baseline anchor +
   accumulating patch-set + bound replay, with the no-patch run cached as the reference
   and a 794 diff after every run. Do not build a second branch object; see §6.
2. ~~**762 — snapshot-diff cheat finder**~~ — subsumed by **798**.
3. **766 §10 firehose convergence** — fold the trace onto the *same* shared-memory
   firehose as the ring (one producer, many consumers), reconciling lossy-recorder vs.
   no-drop-trace.
4. **`.c64rering`** — whole-ring gz dump so a tester can ship an entire reverse-debug
   buffer, extending the `.c64re` currency to the full timeline.
5. **Spec-status hygiene** — 769 header still says "769.5 UI LAST / not built"; reconcile
   to reflect the shipped workbench UI (a doc/board fix, not engine work).

**Meaning layer (C64RE, always the last step, never a side effect):** turning a validated
branch into a `FindingRecord` / `PatchRecipe` / provenance edge; "export a cracked image"
is an explicit C64RE build op, not an overlay-test byproduct.

---

## 6. The vocabulary — one word per thing

Added 2026-08-17. Fifteen months of specs grew several names for the same object and
one name for several objects, and that is not a tidiness problem: 810 and 796 were
about to build a second "branch" because neither could see that the other already
had one. This section fixes ONE word per concept and names the code behind it. A
spec that needs a different word has to change this table first.

### The timeline

| Word | Is | Lives in |
|---|---|---|
| **checkpoint** | one full machine state captured by the always-on ring, at the ring's cadence | `checkpoint_ring.rs` (805.B/765/807) |
| **anchor** | the ID of a checkpoint, as used to name a point to return to | `anchor_id` / `cp_<n>_<n>` throughout the transport |
| **mark** | an anchor a human NAMED and pinned, so it survives PLAY cutting the future | `mark/set\|list\|drop\|goto` (809) |
| **snapshot** | a `.c64re` FILE — a checkpoint written out, media embedded, portable | `native_snapshot.rs` (707) |

Not synonyms. A checkpoint is in memory and expires; a mark is a checkpoint with a
name and a reservation; a snapshot is a file you can send someone. "Anchor" is the
identifier, not a fourth kind of thing.

### The intervention

| Word | Is | Lives in |
|---|---|---|
| **patch** | bytes for ONE target `(space, bank, addr)`, plus the source they were assembled from | `Patch` (796) |
| **overlay** | applying a patch-set into a restored state, so modified code runs without touching the original medium | 795, `runtime/overlay_run` (769.2) |
| **delta** | a patch-set shaped for delivery — the thing that becomes a crack or a fix | 797 |

"Intervention" was 776's word for the same idea and 776 is SUPERSEDED (2026-08-12).
Do not reintroduce it.

### The experiment — and the collision that mattered

| Word | Is | Lives in |
|---|---|---|
| **candidate** | a baseline anchor + an accumulating patch-set + a bound replay, with the no-patch run cached as its reference | `State.candidates` (796), 7 MCP tools |
| **branch** | ~~a second name for the same object~~ | — |

**Decided here: a candidate is the branch.** 810 planned to own "the branch — its
name, its patch-set, which scenario it serves, whether it won", and three of those
four already exist as a candidate. Building a second object beside it would give the
repo two things with one job, which is the trap 810 §4b was itself written about.
What 810 adds is the two fields 796 has no use for — a NAME and WON — not a new
model.

`SnapshotBranch` in `rewind.rs` keeps the word for the snapshot TREE's edges (768,
`promote_branch`). That is a graph edge, not an experiment; it is the one surviving
other use and it does not travel.

### The script

**"Scenario" carried three meanings and now carries one.**

| Meaning | Word now | Lives in |
|---|---|---|
| a `.feature` file: Given/When/Then, driven steps, criteria | **scenario** | `scenario-gherkin.ts` (810 + 812) |
| timed inputs at absolute cycles + a cycle budget, replayed deterministically | **replay** | `scenario_player.rs`, `run_scenario` (231) |
| `RuntimeScenarioSchema` — define-once-run-many with breakpoints, no goal | *dead* | Spec 030, whose spec file no longer exists |

The middle one is what a candidate binds and what makes its runs comparable; calling
it a scenario is why 810 §4b had to explain which scenario it meant. The third is the
leftover 810 §4b names, still in `types.ts` with a store and a tool, awaiting a
deliberate removal.

### The judgement

| Word | Is | Lives in |
|---|---|---|
| **criterion** | one `Then` line: what must hold. Byte-exact or verbal | 810 |
| **mask** | what may legitimately differ — cycles, raster, TOD. Belongs to the CRITERION, never the run | 794 provides it, 810 §3 places it |
| **acceptance** | a human said "yes, that is right" once, and the resulting state was frozen as the reference | 810 (types only; no store yet) |
| **verdict** | the result of comparing a run against a reference | 796 returns one; 810 wants one |

**The two references are different questions, and this is the sharpest line in the
whole area.** A candidate's cached baseline is *the same replay without my patch* —
"did my code change anything". An acceptance's baseline is *the state a human
approved* — "does the machine still reach what we agreed was right". Same diff
engine, different reference, and using 796's for an acceptance would silently answer
the wrong question.

### The output

| Word | Is | Lives in |
|---|---|---|
| **capture** | one frame taken out of a machine, on a frame boundary, with the cycle it came from | 812 |
| **reel** | captures assembled into one GIF89a for a release | 812 |

A capture is not a screenshot: a screenshot is a PNG for a human to look at now, a
capture carries its palette indices and its cycle so a reel can be rebuilt from it.
