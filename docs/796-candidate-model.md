# Spec 796 — Candidate Model (live scenario-bound overlay branch + integrated eval)

**Status:** PROPOSED (2026-07-16). **Repo:** TRX64 (candidate store + run/eval capability) +
C64RE (assemble⊕overlay orchestration, MCP tools, delta metadata).
**Board:** row 796 on `C64RE:specs/README.md`. **Next free after: 797.**

**Bases:** 769.2 + **795** (overlay, RAM + cart) · **231/268** (deterministic scenario replay) ·
**794** (whitebox component-diff = the eval) · `trx64-core::rewind::SnapshotBranch` (the branch
data model: id / anchor / `patches`).

**Yardstick:** `docs/usecases-runtime-dev-sandbox.md` #3, part 2 — the **candidate model** the
owner deferred behind 795 (banked overlay). It turns the one-shot overlay what-if into an
iterating, scenario-bound loop whose accumulating patch-set becomes the final code delta (#4).

## Planning decisions (owner, 2026-07-16)

1. **A candidate is a LIVE session/branch object** — not a portable file. Session-lifetime,
   held in the daemon; builds on the existing branch mechanism. (Export gives a portable
   patch-set when you want to persist the delta.)
2. **Scenario-bound** — a candidate = `{baseline anchor + accumulating patch-set + scenario}`;
   every run is a deterministic replay (state + overlay + timed inputs + cycle budget).
3. **Integrated auto-eval** — each run auto-diffs (794) against the cached no-patch baseline
   run of the same scenario → the run returns the verdict "what did my code change / is it
   equivalent," so the iterate loop + fan-out ranking are self-contained.

## Grounding that shaped it

The Rust `RewindManager` is a **thin data-model port** (`SnapshotBranch` carries `patches` +
anchor + tree, but only `promote_branch`/`to_json` — no create/add-patch/run), and the daemon
mints a fresh one per call (no persistent store). The full branch lifecycle lives only in the
TS `rewind.ts` (deprecating). So the candidate model is a **persistent store + net-new
lifecycle ops on the TRX64 side**, composing existing internals (795 overlay + 231 scenario +
794 diff) — NOT built on the dying TS runtime.

## Model

### The candidate object (daemon `State.candidates: HashMap<String, Candidate>`)

```
Candidate {
  id: String,
  baseline_anchor: String,        // checkpoint id (the fixed version + state)
  scenario: Value,                // 231 scenario: timed inputs + cycle_budget
  patches: Vec<Patch>,            // the accumulating overlay set
  baseline_result: Value,         // cached end-checkpoint of the NO-PATCH scenario run
  last_verdict: Option<Value>,    // last run's 794 component-diff
}
Patch { space: "ram"|"roml"|"romh", bank: Option<u16>, addr: u16, source: String, bytes: Vec<u8> }
```

- **`source`** (asm, org = addr via its own `.pc`/`*=`) is the delta seed; **`bytes`**
  (assembled) are what a run applies via 795 overlay. Keyed by `(space, bank, addr)` — re-adding
  at the same target **replaces** (iterate a fix, not stack forever).
- Session-lifetime: the candidate dies with the session (owner decision 1). No `.c64re`-style
  file. `export` hands back the patch-set to persist the delta.

### Operations (the lifecycle)

- **create(anchor, scenario)** → run the scenario ONCE from the anchor with NO patches →
  cache `baseline_result` (the equivalence reference) → return `id`.
- **patch(id, space?, bank?, addr, source)** → assemble `source` at `addr` (the existing
  file-based pipeline; inline source is wrapped to a temp `.asm` with the org) → `bytes` →
  add/replace the patch. **assemble ⊕ overlay = one step** (C64RE assembles, daemon stores).
- **run(id)** → restore `baseline_anchor` → apply ALL patches (795 overlay, RAM + cart) → run
  the scenario (231, deterministic) → capture the end checkpoint → **auto-diff (794) vs
  `baseline_result`** → return `{ registers, verdict, ranCycles }`, store `last_verdict`.
  Ephemeral — the baseline anchor is untouched; the next run restores fresh.
- **remove_patch(id, space?, bank?, addr)** · **list(id)** (patches + last verdict) ·
  **delete(id)**.
- **export(id)** → the patch-set `[{space, bank, addr, source}]` = the delta seed. (The full
  "final code delta" shaping is the next spec, #4; here export is the raw accumulated source.)

### Where a run executes

`run` restores + runs on the **live shared session** — exactly as 769.2 `overlay_run` does
(the human at the UI sees the jump; the co-drive model). One candidate at a time. **Isolated
N-parallel fan-out** (N candidates at once) is a later composition over **787-V2 scratch
instances** — out of scope here.

## Surfaces

- **Daemon** (TRX64): `State.candidates`; WS `runtime/candidate_{create,patch,run,remove_patch,
  list,delete,export}`. `run` composes existing internals — checkpoint restore + 795
  `overlay_bank_write`/RAM write + 231 `run_scenario` + 794 `diff_checkpoints` vs the cached
  baseline. No new emulation logic.
- **C64RE MCP** (`runtime_candidate_*`): thin orchestration; `runtime_candidate_patch`
  assembles `source`→`bytes` (the C64RE assembler pipeline) then calls the daemon with
  bytes + source. Registered in DEFAULT_TOOLS. Meaning/provenance (the delta) lands here.

## Acceptance / gate

1. **Deterministic baseline.** create runs the no-patch scenario → cached `baseline_result`;
   two creates on the same anchor+scenario yield a component-identical baseline (794 strict).
2. **Patch accumulate + replace.** patch at two addrs → both present; patch twice at the SAME
   `(space,bank,addr)` → replaced, not stacked.
3. **Run applies all + auto-evals.** a patch that changes RAM → `verdict.differing` names the
   component; a no-op patch (bytes == original) → `verdict.identical == true`.
4. **Ephemeral / deterministic.** two consecutive runs of the same candidate produce the same
   verdict (baseline anchor untouched between runs).
5. **Cart patch in a candidate.** a roml/romh patch (795) applied during a candidate run
   reaches the EF bank (verified via the run's effect / a read-back).

## Scope boundary (stated, not silent)

- **Single candidate** iterate on the live session — NOT N-parallel fan-out (composes later
  over 787-V2).
- **Session-lifetime** — no disk persistence of the candidate object; `export` persists the
  patch-set (the delta seed).
- The **final-code-delta shaping** (provenance, build-ready patch, apply-to-source) = the NEXT
  spec (#4). 796 stops at `export` = the raw source-patch-set.
- Assemble = the existing file-based pipeline; inline-source ergonomics is an impl detail.

## Cross-links

Spec 769.2 + 795 (overlay) · 231/268 (scenario) · 794 (eval) · `rewind.rs` SnapshotBranch ·
787-V2 (future isolated fan-out substrate) · 762 (first end-to-end consumer, later) ·
yardstick `docs/usecases-runtime-dev-sandbox.md` #3/#4 ·
concept map `docs/concepts-snapshots-scenarios-overlays.md`.
