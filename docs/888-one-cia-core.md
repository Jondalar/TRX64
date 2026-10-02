# 888 — One CIA core: the C64's CIAs are VICE's ciacore

**Status:** BUILT (branch `spec-888-one-cia-core`, not merged) — as built in §As built
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

## As built

**The core.** `ciacore.rs` is the one CIA: VICE's `core/ciacore.c` + `core/ciatimer.h`, with the
timer (`Ciat`) moved in from `cia.rs`. It gained what the C64 needs: the BCD TOD on its own mains
alarm (`ciacore_inttod`), the model switch (`CIA_MODEL_6526` / `CIA_MODEL_6526A`), `write_offset`
(0 on x64sc), the `read_ciaicr` / `read_sdr` / `pulse_ciapc` hooks, `ciacore_disable`, and a
`peek` that runs VICE's `ciacore_peek` on a copy so nothing moves. The 1581 keeps the 8520 event
counter as `TodKind::Event8520` (872 D1b). `cia.rs` is deleted.

**The glue.** `c64cia.rs` ports `c64cia1.c` / `c64cia2.c`: the keyboard matrix solver
(`read_ciapa` / `read_ciapb` / `ciapb_forcelow`, ghost keys included — the old pin functions had
none), the joysticks, the POT selection on CIA 1 port A (`store_ciapa`), and CIA 2's port A
(`read_ciapa` with the user-port PA2/PA3 path, `store_ciapa` → the VIC bank and
`iecbus_callback_write`, `undump_ciapa`). Not connected, so VICE's "nothing attached" path: the user
port, the datasette, a parallel cable, the burst modification, joyport output, shift lock, and the
light pen (the VIC has no light-pen input yet).

**The model.** VICE's x64sc resource default is `CIA_MODEL_6526A`, but every C64 model row of
`c64scmodel.c` sets the CIAs explicitly (`CIA_MODEL_DEFAULT_OLD` = 6526 for the C64 rows,
`_NEW` = 6526A for the C64C rows), and a machine is always one of those rows here. So the
`cia` column of `models.toml` decides: 6526 on `c64-pal` / `c64-ntsc` / `c64-paln`. The 6526A is a
block TRX64 has now; the C64C rows are refused for their custom-IC glue alone.

**The interrupt line.** The CIAs record VICE's `cia_set_int_clk` calls; the CPU core replays them
into `IntStatus` (`interrupt_set_irq` / `_nmi` at their own `rclk`) in `clk_inc` and after the
prologue's PROCESS_ALARMS, the machine at its run boundary and after a host access. The per-cycle
level sample and the boundary restamp of the CIA lines are gone. Alarms are dispatched where x64sc
dispatches them: every cycle (`interrupt_delay`), in the prologue, and after a VIC steal
(`maincpu_steal_cycles`).

**TOD.** VICE builds `ciacore_inttod` with `TODRANDOM` (each mains period jittered by
`lib_unsigned_rand(0, 3)`); TRX64 takes the `#else` branch of the same function (`todticks++` /
`--`), which corrects the same drift deterministically, and closes every second on exactly
`ticks_per_sec`. The old fixed period (`985248 / 50` = 19 704) lost 0.96 cycles every PAL tick.

**The alarm switch.** Spec 857's `cia_alarm_check` / `TRX64_CIA_ALARM_CHECK` chose between VICE's
comparison and a catch-up of both timers every cycle. With ciacore the comparison is the chip; the
switch selected a second implementation and is removed (D2). `cia_alarm_check_gate` keeps its
workloads as determinism lockstep, the restore-continues-identically case and frozen digests.

**Snapshots (D3).** `CiaSnapshot` v3 is the whole context, alarm clocks included; a restore is
exact (`cia_alarm_check_gate`'s restore case runs in lockstep with the straight run at 1 and 64
MHz). A v2 node converts — registers, timers, the mask, the flags and the line level as IR, TOD —
and `Machine::restore_notes` says so; `snapshot/undump` returns them as `notes`, the monitor's
`undump` prints them. VSF CIA modules (the c64re-own framing, version 2.5 now, and real VICE files)
go through `ciacore_snapshot_write/read_module`. VICE writes `ACK_IRQFLAGS` / `NEW_IRQFLAGS` with
`SMW_DB` (8-byte doubles) and reads them with `SMR_B`; both halves are ported, which is why the
module is 77 bytes. A save writes from a copy of the chip: VICE's settle moves the live chip, TRX64's
save does not. The old 48-byte record converts with a note. The `.vsf` export's `GLUE` module takes
`old_vbank` from the composed port output (it read the bare PRA).

**Acceptance.**
1. `cia_icr_latch_gate` (booted machine): the issue's program — IRQ_COUNT 24 241 after 100
   frames and MAIN_COUNT frozen from the first IRQ on, `$DC0D` reads `$81`; the CIA 2 analogue
   takes one NMI and never another (the line is never released), `$DD0D` reads `$81`.
2. `iso_cia_gate` replays all seven corpus scenarios record for record against their goldens —
   no value changed.
3. All suites green; the 7-game gate 7/7.
4. `git grep "cia::Cia" -- '*.rs'` finds nothing; `cia.rs` is gone (the string still occurs in
   this spec's own acceptance text).

