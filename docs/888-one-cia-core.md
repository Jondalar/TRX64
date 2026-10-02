# 888 — One CIA core: the C64's CIAs are VICE's ciacore

**Status:** PROPOSED
**Trigger:** TRX64 issue #3 — writing the ICR mask (`$DC0D = $7F`) releases a pending CIA 1
IRQ and clears IR; a real 6526 and VICE keep both until `$DC0D` is READ. A crack intro that
leaves its IRQ through `$EA81` without reading `$DC0D` runs in TRX64 and hangs on hardware.

## Why there are two CIA cores

- `cia.rs` (June 2026) was grown increment by increment until a handful of iso CIA
  exercisers matched the TS oracle byte for byte: VICE's timer table, the alarm cascade and
  the TOD were ported piecemeal. The IRQ line is a level test, `irqflags & mask`
  (`cia.rs:933`) — not VICE's IFR delay pipeline — and no iso case wrote the mask without
  reading the ICR.
- Spec 872 (1581) recorded exactly that gap and planned **one** CIA: `cia.rs` grows FLAG, the
  serial register, port hooks and the IRQ line from `core/ciacore.c` (872 D1b). The build
  instead ported `ciacore.c` 1:1 into a NEW file, `ciacore.rs`, used by the 1581 alone,
  "the C64's CIAs untouched". The deviation from D1b was not recorded at merge.

So the exact VICE port drives only the 1581, and the C64 runs the approximation.

## Decisions

**D1 — CIA 1 and CIA 2 of the C64 run on `ciacore.rs`.** VICE's `core/ciacore.c` with the
C64 glue of `c64/c64cia1.c` and `c64/c64cia2.c` ported 1:1: CIA 1 port A/B = keyboard
matrix + joysticks (+ the POT select lines of Spec 876), IRQ on the CPU's IRQ line; CIA 2
port A = VIC bank + IEC (ATN/CLK/DATA out, CLK/DATA in), port B = user port, IRQ on NMI;
TOD from the mains frequency of the model row (Spec 863); the CIA model (6526 / 6526A) as
VICE chooses it for the machine. FLAG on CIA 1 = cassette read (unconnected), on CIA 2 =
user port pin B; SP/CNT as VICE wires them.

**D2 — `cia.rs` is deleted.** Every user — the full bus, the CPU-isolated exerciser bus,
snapshots, VSF, the monitor's `io` view, checkpoints, the U64 turbo alarm path — moves to
the one core. No compatibility shim, no second implementation to A/B against.

**D3 — snapshots carry the ciacore state.** `.c64re` and the checkpoint ring store the
ciacore context (registers, timers, IFR/IER, `ifr_delay`, SDR state, TOD). VSF CIA modules
load and save through VICE's own `ciacore_snapshot_*` layout. An old `.c64re` with the old
CIA record is converted on load (registers + timers), and says so.

**D4 — what may change, and how it is judged.** Timing changes are expected where `cia.rs`
was not VICE (the IFR pipeline: IRQ raise one cycle late on a 6526, the ICR read/ack
window). Each diverging gate case is examined: a difference toward VICE is accepted and the
golden re-blessed with the reason; a difference away from VICE is a bug in the port.

## Acceptance

1. Issue #3's repro: IRQ_COUNT runs up for ever, MAIN_COUNT stays ~0; `$DC0D` read inside
   the handler shows IR (bit 7) set after a mask-only write. Same for CIA 2 with NMI.
2. The iso CIA corpus (`tools/oracle/corpus/cia/*`) runs on the new core; every changed
   value is explained (D4).
3. The 7-game gate, the 1541/1581 drive gates, `cia_alarm_check_gate`, TOD tests, the
   keyboard/joystick/POT tests and the full workspace tests are green.
4. `cia.rs` no longer exists; `git grep "cia::Cia"` finds nothing.
