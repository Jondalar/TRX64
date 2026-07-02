# Spec — Time-Travel Tooling: ring dump/restore + checkpoint diff

**Status:** PROPOSED (planning). **Repo:** TRX64 (the two new pieces) — App + C64RE consume.
**Motivation:** make the reverse-debug / time-travel buffer usable by **testers** (capture a
bug's full context → ship → a dev restores + scrubs/diffs it) and by the **native app +
C64RE** (live scrub + a typed by-ID state diff). The heavy compute + state already live in
TRX64; this spec pins the cut and the two additive TRX64 pieces.

## Principle (the cut)
- **Compute + state → TRX64** (typed FFI → App; Daemon/WS → C64RE). 
- **Presentation + orchestration → App** (and C64RE's browser UI).
- **What the App calls is TYPED** (uniffi records, A3) — never the raw `call()` escape hatch.

## What goes where

| Piece | Where | Status |
|---|---|---|
| Checkpoint ring · `capture`/`restore` · `thumbnails` | TRX64 | exists |
| `buildTraceFromRing` · trace index/query | TRX64 | exists |
| `snapshot/dump` (one state, `.c64re`) | TRX64 | exists |
| snapshot-diff **compute** (RAM/reg/chip/drive) | TRX64 (`trx64-core/snapshot_diff.rs`) | exists (byte-buffer only) |
| **`diffCheckpoints(idA, idB) -> SnapshotDiff`** (typed, by-ID) | **TRX64 (new)** | NEW |
| **`ringbuffer_dump()` / `ringbuffer_restore(path)`** (whole ring) | **TRX64 (new)** | NEW |
| Ring cadence (pump fills the ring) | App / C64RE | orchestration |
| Scrub filmstrip · drag · buttons · diff-view · drill-in | App / C64RE | UI |
| "Dump / Load ring" buttons | App / C64RE | UI |

The App can build the live scrub + diff UI (Phase A/B) immediately — every primitive but the
two NEW pieces already exists. The two NEW pieces are additive, typed, no parity risk, and
**C64RE benefits too** (a clean by-ID diff API + ring dump/restore over WS).

---

## Piece 1 — `diffCheckpoints(idA, idB) -> SnapshotDiff` (typed, by-ID)

The diff compute already exists in `trx64-core/snapshot_diff.rs` (RAM, CPU/PLA, VIC/CIA/SID,
IEC, DRIVECPU sub-diffs — verified by Batch-8 tests). It is only reachable as a byte-buffer
variant today. Add a typed, **by-ID** wrapper so neither the App nor C64RE shuffles VSF/.c64re
bytes around:

- **`diffCheckpoints(idA: String, idB: String) -> SnapshotDiff`** (trx64-ffi typed method + a
  `runtime/diff_checkpoints` WS method + a monitor verb). Resolves the two checkpoint anchors
  **by ID** from the live (or restored) ring, runs the existing compute, returns a TYPED
  `SnapshotDiff` record (not JSON).
- **`SnapshotDiff` record** (uniffi): `{ cycleA, cycleB, ram: [RamRun{start, old: Data, new: Data}], cpu: [RegChange{name, old, new}], vic/cia/sid: [RegChange], drive: [RegChange] }`. RAM grouped into contiguous runs (a 64 K byte-by-byte list is unusable in a UI).
- C64RE gets the same `runtime/diff_checkpoints` over WS — a by-ID diff instead of only the
  byte-buffer one.

Wraps existing compute + by-ID restore/read. No new diff logic.

---

## Piece 2 — `ringbuffer_dump` / `ringbuffer_restore` (the whole ring)

For the **offline / tester** workflow (and "save/load a debug session"): serialize the WHOLE
reverse-debug buffer to one file and reconstruct it elsewhere.

- **`ringbuffer_dump(path) -> RingDumpInfo`** — serialize, into one `.c64rering` file:
  - every checkpoint-ring anchor (full machine state, each ≈ a `.c64re`),
  - the delta ring (instructions + writes + `old_value`),
  - the cpu-history ring (instructions + opcodes),
  - the thumbnails + metadata (cycle range, the "current" anchor).
  - **gzip the payload** (the raw ring is ~90–160 MB; gz → ~10–30 MB, shippable). Self-contained.
- **`ringbuffer_restore(path) -> RingDumpInfo`** — load → reconstruct the checkpoint ring +
  delta ring + cpu-history into a fresh runtime, restore the machine to the dump's "current".
  After restore the **scrub filmstrip, `reverse_step`, `whoWrote`, `chis`, and
  `diffCheckpoints` all work on the dumped buffer** — the tester's bug is fully explorable.
- Exposed as trx64-ffi typed methods + `ringbuffer/dump` · `ringbuffer/restore` WS + monitor
  verbs.

The two pieces compose: `ringbuffer_restore` loads the ring → `diffCheckpoints` diffs its
anchors.

---

## Tester flow
1. Tester hits a bug → `ringbuffer_dump` (UI button or monitor) → a `.c64rering` file.
2. Ships the file.
3. Dev `ringbuffer_restore` → the UI loads the whole ring → scrub, reverse-step, `whoWrote`,
   `diffCheckpoints` two anchors → debug the tester's exact run.

## Live flow (App / C64RE)
The pump fills the ring (cadence, app-side). The scrub filmstrip + diff-view are UI.
`diffCheckpoints(idA, idB)` powers the diff-view (typed, by-ID, no byte shuffling).

## Acceptance
- `diffCheckpoints(idA, idB)` returns a typed `SnapshotDiff` whose RAM/reg/chip/drive runs
  match the existing byte-buffer compute; by-ID resolution from the ring works; C64RE's
  `runtime/diff_checkpoints` returns the same shape.
- `ringbuffer_dump` → `.c64rering` (gz, self-contained); `ringbuffer_restore` reconstructs a
  ring on which scrub + reverse-step + `whoWrote` + `chis` + `diffCheckpoints` all work; a
  dump→restore round-trip is faithful (the restored ring's anchors diff-equal the originals).
- Both are typed FFI methods (no `call()`) + WS methods (C64RE) + monitor verbs.
- Additive: `--stream`, dispatch, and the conformance gate unaffected (P0 stays green).

## Out of scope (App / C64RE side)
The scrub filmstrip, drag interaction, diff-view, dump/load buttons, and the ring-fill cadence
are the App's (and C64RE's UI) — not built here. TRX64 ships the typed primitives; the
consumers build the UX.
