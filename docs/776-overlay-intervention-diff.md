# Spec 776 — Overlay-Intervention Branches + Outcome-Diff (autonomous-debug loop)

> Shared cross-repo numbering (C64RE + TRX64 draw from ONE range; see the C64RE
> `specs/README.md` board). 776 = first number after C64RE's 775.

**Status:** PROPOSED (planning). **Repo:** TRX64 (the engine) — App + C64RE consume.
**Merges + retires:** C64RE Spec 711 (Code/Data Overlay + Controlled Intervention
Branches) + Spec 712 (Rewind, Replay, Branch-Diff). Those two are the same loop
seen from two ends (711 = intervene/branch, 712 = replay/compare); with the ring +
`diffCheckpoints` + reverse-debug already in TRX64, the only genuinely-new work
across both is ONE capability, specified here.
**Builds on (already exists in TRX64):** `spec-time-travel-tooling.md`
(checkpoint-ring capture/restore, `diffCheckpoints`, ring dump/restore),
`spec-reverse-debug-crash-triage.md` (`reverse_step` / `whoWrote` / `chis`),
`trx64-core/snapshot_diff.rs`, the recorder, mutable-media checkpoints, monitor
poke (mem/reg write).

## Motivation

The passive time-travel loop (`run → rewind → diff two EXISTING checkpoints`) is
done. What is missing is the **active experiment loop** — the core of autonomous
debugging / cheat / crack / bug-fix work:

```text
pin checkpoint → apply code/data overlay-intervention → run forward → diff the
OUTCOME against the un-intervened baseline → retain the branch as evidence
```

i.e. *"apply this patch from here, run, and show me exactly what it changed
downstream"* — without ever mutating the original disk/cartridge/program bytes.
TRX64 already has every primitive **except** (a) an intervention that is a
recorded, replayable **branch object** (not a throwaway live poke) and (b) an
**outcome**-diff (baseline-result vs branch-result, not two arbitrary anchors).

## The cut (Leitregel)

- **Engine → TRX64 (this spec):** overlay-apply from a pinned checkpoint, the
  branch object, run-forward, outcome-diff, branch manifest export. Pure
  runtime-forensics capability.
- **Meaning → C64RE (its EXISTING layer, no new spec):** a validated intervention
  becomes a `FindingRecord` / `PatchRecipe` / provenance edge in the knowledge
  graph. C64RE consumes the branch manifest; it does not re-implement the engine.

## What already exists (do NOT rebuild)

| Piece | Where | Status |
|---|---|---|
| Checkpoint ring · capture/restore · thumbnails | TRX64 | exists |
| `diffCheckpoints(idA, idB) -> SnapshotDiff` (typed, by-ID) | TRX64 | spec-time-travel-tooling (NEW-there, small) |
| `snapshot_diff.rs` compute (RAM/CPU/PLA/VIC/CIA/SID/IEC/drive) | TRX64 | exists |
| recorder (ordered input/media/event replay) | TRX64 | exists |
| reverse-debug (`reverse_step`/`whoWrote`/`chis`) | TRX64 | exists |
| mutable-media checkpoints (disk + every writable cart family) | TRX64 | exists (714 landed on TS; parity in TRX64 ring) |
| monitor mem/reg write (the raw poke) | TRX64 | exists |

## What is NEW here

### N1 — Intervention branch object (recorded, not a throwaway poke)

Binding rule (from retired 711 §2.1): **no unrecorded live poke is an accepted
experiment result.** An intervention roots at a pinned checkpoint and is captured
as an ordered, replayable branch with provenance.

```
InterventionBranch {
  id, parent_checkpoint_id, parent_branch_id?,
  media_identity,                       // content hash of mounted media (must stay unchanged)
  interventions: [Intervention],        // ORDERED
  result_checkpoint_ids: [String],
  evidence_refs: [String],
}
Intervention =
  | MemoryWrite   { space: c64|drive8, address, bytes, applied_cycle, before, author, source }
  | RegisterWrite { space: c64|drive8, register, value, applied_cycle, before, author, source }
  | Overlay       { role, region, bytes }        // code/data overlay (RAM shadow, original bytes retained)
  | BreakpointCmd { command }
  | InputEvent    { input }
  | MediaOverlay  { role, operation }            // virtualized media override; original bytes immutable
```

Immutable-media policy (retired 711 §2.2): overlays may change runtime behaviour,
but **original mounted media bytes + their content hash never change**. Exporting
a patched PRG/disk/CRT is an explicit later C64RE build op, never a side effect of
testing. First version edits at **instruction boundaries** (cycle-exact
mid-instruction edits deferred until a concrete experiment demands them).

### N2 — Run branch + capture outcome

Run the branch forward to a stop condition (breakpoint / cycle / frame / marker);
capture `result_checkpoint_ids`. The recorder logs the branch's ordered events so
the branch is deterministically replayable.

### N3 — Outcome-diff (baseline-result vs branch-result)

The new diff is not "two arbitrary anchors" — it is **the same run point, with vs
without the intervention**:

- `diff_branch_outcome(baseline_branch_id, intervention_branch_id) -> SnapshotDiff`
  — resolve each branch's result checkpoint at the matching stop condition, reuse
  the existing `diffCheckpoints` compute. Returns the typed `SnapshotDiff` (RAM
  runs + CPU/chip/drive reg changes) already defined in spec-time-travel-tooling,
  optionally plus visual-diff (VIC frame) and trace-diff (marker/query summary).
- Two intervention branches from one checkpoint are compared the same way (the
  711/712 "apply two different patches, compare" scenario).

### N4 — Branch manifest export (hand-off to C64RE knowledge)

`export_branch_manifest(branch_id) -> path` — serialize the branch (root
checkpoint ref + ordered interventions + result refs + evidence refs), composing
with `ringbuffer_dump` for the full explorable buffer. C64RE ingests the manifest
into a `PatchRecipe` / `FindingRecord` with provenance; a manifest reopens and
fails clearly if its media/checkpoint deps are unavailable or mismatched.

### Surfaces

Typed FFI (uniffi, no `call()` escape hatch) + WS methods + monitor verbs:

```
intervention/branch/create     intervention/apply     intervention/branch/run
intervention/branch/diff       intervention/branch/export
```

## Acceptance

1. From a pinned checkpoint, apply a C64 RAM/code overlay, run forward, and
   `diff_branch_outcome` returns a typed `SnapshotDiff` vs the un-intervened
   baseline result — **and the mounted-media content hash is unchanged**.
2. Apply two different interventions from one checkpoint; run each; compare the
   two outcomes (frame/state/evidence) without touching original media.
3. A branch is deterministically replayable (recorder events reproduce the same
   result checkpoint); an exported manifest reopens with its deps or fails clearly.
4. Unrecorded destructive edits are NOT silently treated as reproducible
   experiments (they are not branches).
5. Additive + typed: `--stream`, dispatch, conformance/P0 gates unaffected; no
   VICE-parity risk (overlays are RAM shadows / recorded events, the core cores
   stay verbatim-VICE).

## Out of scope (App / C64RE side)

- Timeline / scrub filmstrip / side-by-side diff-view UI — App + C64RE browser UI.
- Permanent cracked-image export — an explicit C64RE **build** operation.
- Arbitrary live JS injection, automatic patch synthesis.
- Turning a validated branch into a finding/patch-recipe — C64RE's existing
  knowledge layer (this spec ships the manifest it consumes).

## Cross-links

- `docs/spec-time-travel-tooling.md` (the diff + ring primitives this composes).
- `docs/spec-reverse-debug-crash-triage.md` (reverse-step / whoWrote / chis).
- Retires C64RE `specs/711-code-overlay-intervention-branches.md` +
  `specs/712-rewind-replay-branch-diff.md` (closed on the C64RE board, folded here).
- Consumed by C64RE: `PatchRecipe` / `FindingRecord` / provenance (existing).
