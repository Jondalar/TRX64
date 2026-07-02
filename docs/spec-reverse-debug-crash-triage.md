# Spec — Reverse-Debug + Crash-Triage (real backward-stepping on the Rust core)

**Status:** PROPOSED (review). **Repo:** TRX64 (core feature, superset over the TS ring-crutch).
**Cross-link:** Spec 753 (mem-access capture w/ old_value), 764 (JAM/BRK auto-break),
766 (recorder shared-mem ring), 705.B (checkpoint ring = the cycle-exact replay fallback).

## Goal (user, 2026-06-26)
Real **backward-stepping at any time**, to find a **derail / stack-crash** and **pin the
exact state that caused it**. It does NOT need to be cycle-exact — it needs to walk the
CPU+memory history backward from a crash to the culprit instruction.

The snapshot ring was the *crutch* (restore-anchor + replay-forward = O(snapshot-interval)
per back-step, chip-cycle-exact). For crash-debugging we don't need cycle-exactness — so we
get **true O(1) reverse-step** from a delta-undo log instead, and keep the ring only as the
cycle-exact replay fallback.

## Two layers — only layer 1 is in scope
1. **Logical reverse (CPU + RAM + IO-register writes)** — the whole job here. Undo per
   instruction from a delta log. O(1)/step, available any time the log covers the window.
2. **Cycle-exact whole-machine reverse (VIC raster / CIA timer / sprite-DMA sequencer)** —
   these evolve every cycle, not just on writes; a write-log can't undo them. OUT OF SCOPE.
   When cycle-exactness is genuinely needed → restore nearest checkpoint anchor + replay
   forward (the existing ring; unchanged).

## The data already exists
The `.c64retrace` (Spec 753) already records, per instruction:
- the **cpu_step**: PC, A, X, Y, SP, P, cycle (the pre-execute register state), and
- every **memory/IO write**: addr, value, **`old_value`** ← the undo info.

So the "complete transaction stream between snapshots" the user described is **already
captured**. We don't add a new log — we add an engine that *reads it backward*.

## Phase 1 — the primitives

### 1a. Reverse-step engine (`crates/trx64-core`)
Reverse one instruction (from trace index `i` to `i-1`):
1. For each write of instruction `i` in REVERSE order: write back `old_value` (undo RAM/IO,
   incl. $D0xx/$DCxx register writes).
2. Restore CPU regs from `cpu_step[i]` (the pre-`i` state = post-`i-1`).
→ machine now sits at the state *before* instruction `i`. O(writes) ≈ O(1–3).

**Hard contract:** reverse-step restores CPU+RAM+IO-registers, NOT chip internal counters.
So it is **inspect-backward only**. To *resume forward* from a past point, restore the
nearest checkpoint anchor (cycle-exact) — reverse-step is for *looking*, not re-running.
The reverse window = how far back the trace/recorder covers (see 1c).

### 1b. API
- `runtime/reverse_step` (+ `reverse_steps n`) — WS + MCP `runtime_reverse_step`. Returns the
  landed `{pc, regs, the writes undone}` so a caller sees what changed.
- `runtime/who_wrote { addr }` — last writer of an address: scan trace backward → `{pc,
  cycle, old→new}`. Backed by the existing `trace_store_bus_find` / taint; surface it in the
  monitor as `whowrote <addr>` (and in the JAM drop-in).

### 1c. Recorder bounded-always-on (the enabler)
A crash is only reverse-debuggable if the run-up was being recorded. So: the recorder (Spec
766) runs as a **bounded always-on ring of the last N instructions' deltas** (evict-oldest),
OR auto-arms when JAM-break is armed (Spec 764). Default-on with a sane cap (e.g. last
~250k instructions of deltas; deltas are tiny — regs + 1–3 writes/instr). Memory bounded.

## Phase 2 — guided crash-triage (thin layer on 1, high value)
On JAM/derail/BRK auto-break (Spec 764), the monitor auto-runs a triage and prints the
causal chain, instead of making the user hand-walk it:
1. **Crash PC** — where it JAMmed / hit the illegal opcode / wild PC.
2. **The wild control transfer** — walk back to the last RTS/JMP(ind)/RTI that landed bad
   (e.g. the RTS that popped a bad return address).
3. **The corruptor** — read the stack slot that RTS popped (`$0100+SP`), then `who_wrote` it
   → the instruction that put the bad byte there.
4. **Report:** `JAM @ $XXXX ← wild RTS @ $YYYY popped $ZZZZ from $01SS ← $01SS written by
   $WWWW @ cyc C (was $old → $bad)`.

That auto-pins the causing state — the user's exact ask.

## Verification (NOT the differential gate — this is a TRX64 superset)
TS has only the ring-crutch; it can't do real reverse-step, so the conformance gate can't
compare. Verify instead by:
- **Rust unit tests** in `trx64-core`: synth a delta stream, reverse-step, assert
  CPU+RAM byte-exact return to the prior state; multi-step round-trips.
- **Crash-repro demo** (a Rust integration test or a scripted scenario): inject a deliberate
  stack-smash (overlong PHA run / unbalanced JSR), run to JAM, then assert (a) `who_wrote`
  pins the smashing instruction and (b) the triage chain names it.
- A no-disk oracle run stays green (this is post-capture, decoupled from the emu core).

## Open scope question (for review)
Phase 1 (primitives) is the foundation and clearly in. **Phase 2 (guided triage) — build it
in this pass, or ship primitives first and add triage after?** Phase 2 is cheap on top of
Phase 1 but is where the "magic" is. (Recommendation: build both — Phase 2 is small and is
the actual UX the user described.)

## Decisions (user, 2026-06-26)
- **The ring is ALWAYS-ON, no pre-arming.** "trace on" must NOT be required to inspect history
  — the instructions+deltas are always in the ring; the `.c64retrace` file is an **on-demand
  DUMP** of the ring (a persisted slice), not a separate capture you arm beforehand. So a
  crash's run-up is ALWAYS reverse-debuggable.
- **Depth = 10 seconds** of always-on full-delta history (≈3M instructions + ~3M writes ≈
  100–150 MB). Tunable via env. Phase 1a shipped the CPU-only ring (~0.85s, 6 MB); this
  extends it to the full deltas at 10s.
- **One ring serves all live reads:** chis (done) + swimlane/map/taint + reverse_step +
  who_wrote read the SAME always-on ring; the finalized-trace/sidecar path stays only as the
  fallback for windows older than the ring. Build order: (1b) full-delta ring + reverse_step +
  who_wrote; (1c) route swimlane/map/taint at the live ring + make "trace on" = dump-on-demand.

## Out of scope
- Cycle-exact whole-machine reverse (use the ring + replay).
- Reverse across a media swap / cold reset (the trace boundary; report it, don't fake it).
- Reverse beyond the recorder window (fall back to nearest anchor + replay).
