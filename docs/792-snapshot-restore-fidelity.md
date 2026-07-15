# Spec 792 — Snapshot Restore Fidelity (`.c64re` + ring must resume bit-faithful)

**Status:** PROPOSED (2026-07-15). **Repo:** TRX64.
**Shared cross-repo numbering** (registry = C64RE `specs/README.md`).
**Depends on / touches:** Spec 707 (`.c64re` native snapshot,
`c64re_snapshot.rs`), Spec 705.B/765 (checkpoint ring, `checkpoint_ring.rs`), the
cartridge mappers (`cart.rs` `get_state`/`set_state`/`CartState`).

## Problem — restore is NOT bit-faithful

A `.c64re` (and a ring checkpoint) is supposed to be a *resumable* machine — load
it and continue exactly where it was. It is not.

**Confirmed gap #1 — cartridge continuation state is dropped.** On undump,
`c64re_snapshot.rs:1048` re-creates the cart mapper from `cartBytes` + overlays
`cartFlash` — but **never** captures or restores the mapper's live continuation
state: `current_bank`, `control_register` (EF register_02 / C64MegaCart mode),
jumper, IO2 RAM, the flash command-FSM. The mapper already exposes
`get_state()`/`set_state()` (`CartState`), used by the VSF import + cart-insert
paths — the checkpoint just doesn't call them. **So a banked-cart snapshot resumes
at bank 0 / register 0.** (Surfaced twice: the VSF→`.c64re` EF resume renders
black; and a real-world `undump` of a Wasteland `.c64re` in the field landed in the
intro instead of the saved state.)

**There may be more.** The Wasteland case (and any "resumes to the wrong place")
means we cannot assume cart-state is the *only* dropped field. We need to find
**every** capture/restore gap systematically, not patch one symptom.

## The approach — a round-trip fidelity GATE that enumerates gaps

Build a test that is the ground truth for "a snapshot restores exactly":

1. Take a machine in a **non-trivial running state** (booted + run into a game /
   a banked cart at a non-zero bank / drive mid-track).
2. **Capture** → serialize (`.c64re` via `write_native_snapshot`; and separately a
   ring checkpoint).
3. **Restore** into a FRESH machine (`read_native_snapshot` + restore; and the ring
   restore path).
4. **Assert byte-identical machine state**: RAM (64K) + CPU regs + clk + CIA1/CIA2
   full state + VIC state + **cart `get_state()`** + drive `current_half_track` +
   the writable flash + keyboard. Any field that differs is a gap.
5. **Assert N-cycle-identical continuation**: run BOTH (original-continued and
   restored) the same N cycles under a hashing observer; the instruction/bus
   streams must match. This catches state that isn't in the struct compare but
   still changes behaviour.

Every mismatch the gate reports is a capture/restore field to add. Close them until
the gate is green for: a plain machine, a **banked-cart** machine (EF at bank ≠ 0),
and a **drive-active** machine. Run the SAME gate over the **ring** (765) — the
user's requirement: `.c64re` *and* the ring.

## What is NEW (this spec)

### 792.1 — cart continuation state in the checkpoint (the confirmed gap)
Capture `cart.get_state()` (`CartState`: `current_bank`, `control_register`, the
flash-FSM snapshot) into the RuntimeCheckpoint as a `cartState` node, and on
restore call `cart.set_state()` AFTER re-creating the mapper from
`cartBytes`/`cartFlash`. Fixes banked-cart (EF / C64MegaCart / Megabyter / GMOD2)
resume for both `.c64re` and the ring. Unblocks the Spec 791 EF resume too.

### 792.2 — the round-trip fidelity gate (the enumerator)
A `cargo test` (`snapshot_roundtrip_fidelity`) implementing the 5-step audit above
for `.c64re` and the ring, across the plain / banked-cart / drive-active scenarios.
It is the acceptance bar: green ⇒ restore is faithful.

### 792.3 — close every remaining gap the gate finds
Whatever else the gate flags (the Wasteland-intro cause, if not cart) — add its
capture/restore. Iterate until 792.2 is green. Each closed gap gets a line here.

## Non-goals
- Not a `.c64re` format redesign — additive fields only (707 stays readable).
- Not VSF-side (791 consumes this; it is the same checkpoint path).
- Not the coarse-VIC-from-VSF question (791) — that is *import* fidelity from a
  lossy source; THIS is our OWN round-trip, which must be exact.

## Acceptance
1. `snapshot_roundtrip_fidelity` green: capture→`.c64re`→restore is byte-identical
   machine state + N-cycle-identical continuation, for plain / banked-cart /
   drive-active.
2. Same, green, for the **ring** checkpoint path.
3. A banked EF cart snapshot (bank ≠ 0) restores at the SAME bank/register/IO2-RAM/
   flash — `undump` continues in-place (no bank-0 reset, no intro).
4. The Spec 791 EF VSF→`.c64re`→render no longer blanks (rides 792.1).

## Build order
1. **792.1 cart-state capture/restore** (the confirmed gap) + **792.2 the gate**
   asserting it — one slice (fix + its proof).
2. Run 792.2 broadly → **792.3** close whatever else it enumerates (drive, VIC
   seam, CIA alarms-in-our-own-snapshot, …).
3. Ring parity (765) under the same gate.
