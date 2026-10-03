# 890 — Turbo as measured on the C64 Ultimate

**Status:** PROPOSED
**Source:** measurements on the owner's C64 Ultimate (firmware 3.15, FPGA 125, core 1.50, PAL),
2026-10-03, by the 1541Ultimate_FW session: `/Users/alex/Development/TRX64-Ultimate/docs/turbo-measurements-gideon.md`
(program and raw data in `tests/turbo-gideon/`). Where they contradict TRX64, the measurement wins.

## What TRX64 assumed, and where it came from

| # | TRX64 | measured | origin of TRX64's value |
|---|---|---|---|
| 1 | `$D031` bit 7 = 1 → badline timing ON (`vic.rs` `u64_speed`, gate `d031_80_is_one_mhz_with_badline_timing_and_not_turbo`) | bit 7 = 1 **removes** the badline stalls; 0 keeps them — at every index, including 0 (`$80` = 1 MHz without stalls) | 851 read `C64_SPEED_PREFER` bit 7 as "badline timing" and gave `$D031` the same sense |
| 2 | index 14 / 15 = 48 / 64 CPU cycles per PHI2 (`U64SpeedTable::mhz`) | **47 / 63** (repeated with four loop sizes); every other index as labelled | the menu labels |
| 3 | CIA timers and raster on PHI2 | the same | — (code right; `cia_tod_gate.rs`'s header claims the opposite) |
| 4 | interrupt delay counted in **PHI2** cycles (two PHI2 edges) | taken within **less than one PHI2 cycle** at 8/16/64 MHz (0, or +1 at 16), normal lines, badlines, badline timing on | 851: "as in VICE's TurboMaster" — a different machine, never measured on a U64 |
| 5 | a program's `$D031` holds until reset | **last write wins** between the program and a menu change; a menu PUT applies even when the value does not change | — |
| 6 | after reset: the menu speed after the BUG-061 hold | registers mode: `$D031` reads `$00`, 1 MHz **with** stalls until a `$D031` write or a menu change; Manual: hold, then the menu speed; Off/Manual: `$D030`/`$D031` read `$FF`; registers mode: `$D030` reads `$FF` | — |

## Decisions

**D1 — `$D031` bit 7 is "no badline stalls".** The turbo state is (speed index, badline
stalls). A `$D031` write sets index = bits 0-3 and stalls = NOT bit 7; a `$D031` read in
registers mode returns `index | (stalls ? 0 : $80)`. A menu change shows in `$D031` in that same
sense (16 MHz + Badline Timing Enabled reads `$09`, Disabled `$89`). The menu's "Badline Timing
Enabled" = stalls. What the firmware writes into its internal PREFER register is not visible from
the C64; TRX64 keeps (index, stalls) as the state and derives every readback from it.

**D2 — the U64-II/C64U speed table is the measured one:** 1, 2, 3, 4, 6, 8, 10, 12, 14, 16, 20, 24,
32, 40, **47**, **63** CPU cycles per PHI2. The menu labels (48, 64) are what the menu shows; the
machine runs the measured ratio. The first-generation U64 table is unmeasured and stays as it is,
marked so.

**D3 — CIA timers and the raster stay on PHI2.** Only the wrong comment in `cia_tod_gate.rs` is
fixed.

**D4 — the interrupt delay is counted in CPU cycles at the turbo clock.** The natural 6502 rule
(an IRQ/NMI is taken at the first instruction end at least two CPU cycles after it fired),
sampled at turbo cycles, with an I/O read synchronised to PHI2. This supersedes 851's "counted in
PHI2 cycles". At 1 MHz nothing changes. The turbo fast path's (856) treatment of an acknowledge
inside one PHI2 cycle is re-examined against this rule.

**D5 — last write wins.** A program's `$D031` write and a menu change (`session/turbo` /
the UCI or REST speed set — whatever stands for the menu in TRX64) both set the same state; the
later one applies, and a menu set applies even with an unchanged value.

**D6 — reset, by mode.** Registers mode: after a reset `$D031` = `$00` (index 0, stalls on)
until a `$D031` write or a menu change; the menu speed is not a power-on default. Manual mode: the
BUG-061 hold (2.06 s at 1 MHz), then the menu speed. Readbacks: Off and Manual — `$D030` and `$D031`
read `$FF`; registers mode — `$D030` reads `$FF`. TurboEnable-bit mode (`$D030`) is unmeasured and
keeps its current behaviour, marked so.

**D7 — `.c64re` carries the turbo state.** A `turbo` block in the hardware record: enable word,
the menu's (index, stalls), the `$D031` state, `$D030`. Written on dump, read on undump and in
the checkpoint ring; an old snapshot without it loads with the defaults of D6 and says so.

**D8 — per-pixel colour stores under turbo cover `$D020`-`$D02E`.** Today only a border store
(`$D020`) lands at the VIC pixel it was made in during a turbo cycle; background, multicolour and
sprite colours (`$D021`-`$D02E`) get the same placement.

## Acceptance

Each measured fact has a test on the U64 profile:
1. RAM loop at every index with the display off: CPU cycles per PHI2 = the D2 table (47 and 63
   at 14/15).
2. Display on, `$D031` = `$0F` vs `$8F`: the stall loss is ~5.4 % with bit 7 = 0 and 0 with bit 7 =
   1; `$80` runs 1.00, `$00` 0.945.
3. CIA timers count PHI2 at every index; frames per TOD second are real time.
4. Raster-IRQ latency at 8/16/64 MHz, with and without badline stalls: 0 PHI2 (+1 allowed at 16);
   at 1 MHz unchanged.
5. Program `$85` → 8×; a menu set to 32 → 31.97×, `$D031` reads `$8C`; program `$83` → 4×; the
   same menu value again → 31.97×.
6. Reset in registers mode: `$D031` = `$00`, 1 MHz with stalls, until a write or a menu set;
   readbacks per D6.
7. `.c64re` round trip of the turbo state; an old snapshot loads with the D6 defaults and a note.
8. A colour store to each of `$D021`-`$D02E` in a turbo cycle lands at its pixel.
9. The existing turbo gates are revised where they pinned the old assumptions (each change
   named with its row from the table above); the 7-game gate and all suites stay green.

## Open (not measured, not changed)

The first-generation U64 speed table; the per-access I/O stretch in turbo; TurboEnable-bit mode
(`$D030`); `$D07A/B`, `$D0BC`, the `$D070/71` mirrors; why 14/15 give 47/63.
