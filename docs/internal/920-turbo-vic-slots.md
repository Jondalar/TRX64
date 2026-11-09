# 920 — Turbo without the stalls: the VIC's own accesses still cost a turbo cycle

**Status:** BUILT, merged to main 2026-11-09. Open: the position of the loss inside the line is checked only by a picture (see Not measured), and one border-colour difference in that picture is not understood.

## Why

With `$D031` bit 7 set the C64 Ultimate no longer waits for the VIC (no badline stalls), and TRX64
modelled that as "the VIC costs a turbo CPU nothing": `check_ba` returned at once. The device
does not behave that way at the two fastest speeds. A fixed amount of CPU work takes longer with the
display on, and with sprites on, than with them off, by one turbo cycle per VIC bus access.
Raster effects timed for the device (one text row of `$D021` writes per 63-cycle line) shear on the
board and did not shear in TRX64.

## Measurement

C64 Ultimate, firmware 3.15, core 1.50, "U64 Turbo Registers". Method: a fixed-work loop timed in
CIA 2 ticks (PHI2), display off (`$D011 = $0B`) against display on (`$1B`), 25 badlines a frame;
sprites: `$D011 = $0B`, 8 sprites on against off, 21 lines each. CPU cycles lost per badline for a
RAM loop / a `STA $D021` loop / a ROM `LDA $E000,X` loop, and per sprite per sprite line:

| `$D031` | turbo cycles per PHI2 | RAM | `$D021` writes | ROM | per sprite per sprite line |
|---|---|---|---|---|---|
| `$8C` | 32 | 0.0 | -0.3 | -0.4 | 0 |
| `$8D` | 40 | -0.5 | -0.4 | -0.9 | 0 |
| `$8E` | 47 | 39.4 | 39.6 | 39.3 | 4.0 |
| `$8F` | 63 | 38.9 | 39.3 | 38.9 | 3.9 |

One turbo cycle is lost per VIC bus access in a PHI2 cycle the VIC takes for itself: the 40
c-accesses of a badline, and per displayed sprite on a sprite line four (1 p + 3 s). Below the two
fastest grids there are free slots in the 64-slot grid (`turbo_short_cycles`), so it costs nothing.
The loss does not depend on where the code sits: it is CPU time, not late writes.

## Not measured

- The position of the loss inside the line. The model puts it in the PHI2 cycle of the access; the
  only check is the picture of a third party's `$D021` demo against the device capture: with the
  model, a 63-cycle and a 62-cycle variant of the demo match the device frame pixel for pixel
  inside the picture area (0.24 % and about 0.5 % of its pixels differ, palette-mapped, 1x); without
  it the 63-cycle variant differs in 50 % of them. That fixes the position to within a few pixels
  per row, not to a cycle. The border colour is not part of that agreement: TRX64 draws it light
  blue where the device draws black (`$D020` reads back 0 in TRX64); not investigated here, and
  not caused by this change.
- A sprite whose DMA is off: the model charges nothing; only displayed sprites were measured.
- The first-generation U64: out of scope (its table is not the one `turbo_short_cycles` produces).
- Firmware 1.1.0: a photo of the same demo from that firmware shows notches without shear, which the
  3.15 captures do not. This model follows 3.15.

## Decision

At `turbo_badline == false` and `turbo_div` 47 or 63, `clk_inc` takes one extra turbo cycle for
each VIC bus access of the PHI2 cycle it has just entered. Every other divider, every machine with
the stalls on, and every non-U64 machine are untouched. No new state: it is read from the VIC after
its tick, so nothing is added to the checkpoint.

## As built

- `vic.rs` `turbo_bus_slots`: after a tick, 1 for the c-access of a badline (the 40 cycles that may
  fetch c), 2 for each cycle of a sprite with DMA on (p + s0, s1 + s2: four per sprite). g, refresh
  and idle accesses cost nothing.
- `C64Core6510Bus::vic_turbo_slots` (default 0), implemented in `full_sc.rs`.
- `c64_6510core.rs` `clk_inc`: after `vic_cycle`, `if !turbo_badline && (div == 47 || div == 63)`,
  that many extra `clk_inc` calls (they only advance the turbo phase). The 1 MHz path pays one
  boolean test, which is false on every machine but a U64 with the stalls off.
- Comments that claimed bit 7 removes everything (`turbo_badline`, `check_ba`, the bit-7 gate) now
  say what is true.
- Gate `without_the_stalls_a_vic_access_costs_a_turbo_cycle_at_the_two_fastest_grids` in
  `turbo_as_measured_gate.rs`: RAM, `$D021` and ROM loops and the sprite loop at `$8C`..`$8F`.
  TRX64 reads 39.9 / 40.2 cycles per badline and 4.2 / 4.0 per sprite line at `$8E` / `$8F`,
  0.0 below; the device reads about one cycle less per badline (39.4 / 38.9). Red without the fix.
- `d21_badline_costs_depend_on_where_in_the_frame_the_loop_ran`: its stall-free base now carries the
  VIC accesses, so every ratio there drops by up to 0.002; the tolerance went from 1e-3 to 3e-3
  (RAM-read maximum x1.1004 before, x1.0994 after, device x1.1011). A tolerance change, not a model
  change.
