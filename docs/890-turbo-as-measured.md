# 890 — Turbo as measured on the C64 Ultimate

**Status:** BUILT (branch `spec-890-turbo-as-measured`, round 1: D1–D8, round 2: D9–D15) — see As built.
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

## Round 2 — the second measurement (2026-10-03)

**Source:** `/Users/alex/Development/TRX64-Ultimate/docs/turbo-measurements-gideon.md`, section
"Round 2" (raw data `tests/turbo-gideon/data/io_cost.txt`, `io_cost_filler.txt`, `q13_pixels.txt`,
`leftovers.txt`; tools `turbomeas.prg` command 5, `tm2.py`, `resethold.asm`), and T29 §10b. Same
device, core and firmware. The owner's rule stands: where Gideon's core is measured, TRX64 follows.

**D9 — what an access costs at turbo, in PHI2 cycles** (Q12, `io_cost.txt`, `io_cost_filler.txt`).
At 63×: RAM/ROM 0.065 per `LDA abs` (the turbo clock); CIA 1, CIA 2, UCI `$DF1C` and IO1 `$DE00`
1.000 — a bus cycle; VIC (every register, `$D011`/`$D012`/`$D019` reads included), colour RAM and
`$D031` 0.0818 read / 0.0653 write; SID 0.0818 read **and write**. At 16×: everything but the bus
cycles 0.2570 (no extra cycle); CIA 1.0002; UCI/IO1 1.0157. Overlap at 63× (32 accesses per
block, 7 cycles of loop on one): reads stay at 1.000 up to 54 cycles per access, 1.0314 from 56,
2.000 at 64; writes 1.000 up to 44, 1.0314 at 46–50, 2.000 from 52. Badlines: bit 7 = 1 —
CIA/UCI not slowed; bit 7 = 0 — CIA read 1.061. TRX64: a bus-cycle access completes in the CPU
cycle right after a PHI2 edge and must be issued a lead before it — 1 cycle for a read, 12 for a
write at 63× (scaled as a time, 3 at 16×); fast reads, and SID writes, cost one extra CPU cycle at
63×. Unmeasured and taken from the nearest measurement: the extra cycle at 20–47× (none, as at
16×); IO1/IO2 writes (as CIA writes); cartridge ROM (memory). Not pinned: UCI/IO1's longer lead at
16× (1.0157); where inside the edge's cycle a write resumes (only its lead is fixed).

**D10 — a colour store paints the pixel it falls in** (Q13, `q13_pixels.txt`). The VIC samples
`$D020` once per pixel, a later store in the same pixel wins, 7.875 CPU cycles per pixel at 63×,
and the grid's boundaries sit at ~−0.9, 7.0 and 14.9 cycles after the PHI2 edge (a store 6 cycles
after a CIA read paints pixel 0, 8–14 pixel 1, 15 pixel 2). Refines D8 for all colour registers.

**D11 — `$D030`/`$D031` are mirrored every `$40`** (`leftovers.txt`): `$D070/71`, `$D0B0/B1`,
`$D0F0/F1` read the same, and stores to them act (`$D071 = $89` → 16×, `$D0B1 = $8C` → 32×).

**D12 — TurboEnable-bit mode, measured** (`leftovers.txt`, menu 16 / Disabled): after a reset
`$D030` reads `$FE`, `$D031` `$00`, 1×; `$D030 = 1` → `$FF`, `$D031` the menu's `$89`, 16×;
`$D030 = 0` → back to `$FE`/`$00`/1×; `$D031` is writable in this mode (`$83` → 4.02×); a menu
change to 32 → `$8C`, 32×. Registers mode: `$D030` reads `$FF`, a store to it does nothing.
Replaces D6's "TurboEnable unmeasured".

**D13 — `$D07A`, `$D07B`, `$D0BC`** (`leftovers.txt`, both register modes): a store to `$D07A`
sets `$D031` to `$00` (1×); one to `$D07B` loads the MENU value, not the program's earlier one;
both read `$FF`. `$D0BC` reads `$01` with SuperCPU Detect enabled, `$FF` disabled (it read `$00`
here).

**D14 — any "U64 Specific Settings" change re-applies the menu speed** (`leftovers.txt`): toggling
SuperCPU Detect turned `$D031` from a program's `$8C` back to the menu's `$89`.

**D15 — the post-reset hold is 2^22 = 4,194,304 PHI2 cycles (4.257 s)** (`resethold.asm`, REST
`machine:reset`): Manual 16 → 4,194,236 ticks from the first instruction to fast (three runs),
Manual 64 → 4,194,166; registers mode with `$D031 = $89` at the first instruction → 4,194,244 —
the write is kept and applies at the hold's end; registers mode without a write stays at 1 MHz.
Replaces BUG-061's 2.06 s. The firmware's `run_prg` ends the hold about 3.2 s after its call — a
different path, the firmware's own, not modelled.

## As built — round 1 (D1–D8), 2026-10-03

**The gate runs the measurement program.** `turbomeas.prg` and its source are in
`crates/trx64-core/tests/fixtures/turbomeas/`; `turbo_as_measured_gate` boots the U64 profile
with ROMs, starts the program at `$0810` past the post-reset hold and drives the same mailbox
`tm.py` drove, with the same arithmetic. A menu change is `Machine::set_u64_turbo`.

- **D1.** `$D031` stores the turbo state in its read-back form (`index | (stalls ? 0 : $80)`,
  bits 4-6 dropped); `u64_speed()` derives (index, stalls) from it; `u64_d031_written` is gone.
  `check_ba` skips the BA stall whenever the stalls are off, at index 0 too. Measured here:
  `$0F` 59.57 / `$8F` 63.00 with the display on (device 59.54 / 62.93), `$80` 1.00, `$00` 0.962
  at k = 16 and 0.945 at k = 1. `u64_menu_as_d031` turns the menu byte (bit 7 = Badline Timing
  Enabled) into `$D031`'s form.
- **D2.** `U64SpeedTable::cycles_per_phi2` (U64-II: … 40, 47, 63) replaces `mhz`;
  `menu_mhz` keeps the labels and the monitor's `turbo` report shows both. The first-generation
  table is marked unmeasured.
- **D3.** `cia_tod_gate`'s header corrected; nothing else.
- **D4.** Below the divider `clk_inc` now runs `turbo_interrupt_cycle`: the CIA events, the
  VIC line and the port lines are sampled and both delay counters advance per CPU cycle, so an
  IRQ/NMI is taken at the first instruction end two CPU cycles after it fired. The NMI
  hijack in `do_irqbrk` counts `nmi_delay_cycles` at turbo. I/O reads see the chip state of
  the PHI2 cycle they fall in; no per-access stretch (open). The IRQPEND tail stays in PHI2
  cycles: it is cleared at the next instruction prologue in either unit. 1 MHz never enters the
  new path. **856 re-examined:** an acknowledge inside one PHI2 cycle no longer depends on the
  boundary restamp; the fast path still ends a batch on I/O (the `$D031` read-back, DMA arm,
  banking and port lines live in the boundary block), and `turbo_fastpath_gate` passes
  unchanged.
- **D5.** `set_u64_turbo` is the menu: with `$D031` enabled it sets the turbo state every time,
  same value or not. Measured sequence reproduced: 8.03 `$85`, 32.00 `$8C`, 4.02 `$83`, 32.00
  `$8C`, 30.29 `$0C` (device 8.0, 31.97, 4.0, 31.97, 30.2). TurboEnable mode follows the same
  rule (unmeasured). `session/turbo` `speed`/`on`/`off` and the monitor verb are `$D031` writes,
  not the menu; `on`/`off` now write `$03`/`$00` (4 / 1 MHz with stalls) so they keep meaning
  what they meant.
- **D6.** `reset_u64_turbo_state` after every reset: registers mode `$00`, TurboEnable mode the
  menu speed (as built). Readbacks were already `$FF` where measured.
- **D7.** A `turbo` node (`enable`, `menuIndex`, `menuStalls`, `d031`, `d030`, `resetHold`) on
  the `u64` profile only; the hold rides too, or a rewind into a boot would lose it. Old `u64`
  checkpoints restore with D6's registers-mode defaults and a `turbo:` restore note. The daemon's
  undump now sets the profile claim BEFORE the restore — it used to set it after, which skipped
  the restore's turbo divider for a `u64` dump undumped into a `c64` session.
- **D8.** `SubCycleColours`: a slot set per colour register, `$D020`-`$D02E`, with a mask; the
  resolve checks the mask (one compare when empty, as the old `Option` did).

**Gates revised (item 9):** `u64_turbo_gate` — `d031_80_is_one_mhz_with_badline_timing_and_not_turbo`
→ `d031_bit_7_removes_the_badline_stalls_and_is_not_speed` (row 1); speed tables and the
index-14 raster IRQ (row 2); the reset test's written-flag (row 6). `pot_gate` g09 divider
48 → 47 (row 2). `subpixel_colour_gate` `only_the_border_colour_is_sub_cycle_today` →
`only_the_colour_registers_are_sub_cycle` (D8). `cia_alarm_check_gate`: the ten `@64` digests
re-recorded (rows 2, 4); the `turbo` node is kept out of the digest so the ten `@1` digests are
the Spec 888 ones byte for byte. `turbo_fastpath_gate`, `perf_bench` (divider 47/63 in the CPU
MHz column), `u64_boot_speed_gate` (menu labels): comments/helpers only. `monitor_golden`
unaffected.

**Item 4, as it came out.** 64 MHz: 0 in every sample, stalls on or off (device: 0). 8 MHz:
0 or −1, in one alignment all −1 on line `$FA` (device: 0, two −1 in 64). 16 MHz: 0 or −1
(device: 0 or +1). 1 MHz `$00`: identical to the build before. The gate asserts "within one
PHI2 cycle" at 8/16 and exactly 0 at 64; which path crosses the PHI2 edge first is what the
per-access I/O cost decides, and that was unmeasured in this round.

**Perf** (`bench_turbo_scaling`, rings off, median of 5, both builds alternated twice, rt-x):
RAM loop 1 MHz 13.8–14.0 → 14.3–14.8 (noise: the 1 MHz path is unchanged); 64 MHz fast path 0.98 → 1.11 (part of it is 63 instead of 64
CPU cycles per PHI2), no fast path 0.74 → 0.77; `LDA $D012` loop at 64 MHz fast path 0.95 →
0.90, no fast path 0.78 → 0.72 — the per-CPU-cycle interrupt sample costs the I/O-heavy load
about 6 %.

## As built — round 2 (D9–D15), 2026-10-03

The round-2 tests are in `turbo_as_measured_gate` too (`d9_*` … `d15_*`); they rebuild `tm2.py`'s
routines byte for byte and run them under the new `turbomeas.prg` (command 5) and
`resethold.bin`, both in `fixtures/turbomeas/` with `tm2.py`.

- **D9.** `C64Core6510Bus::turbo_access_kind` classifies an address (memory / bus cycle / fast /
  SID — the full machine's bus answers from the I/O mapping); the core's load/store helpers call
  `turbo_access_wait` while the divider is above one: a bus-cycle access idles to the next PHI2
  edge (two if its lead does not fit) and runs in the cycle after it; a fast read, and a SID
  access, costs one cycle at 63×. Measured here, every row of `io_cost.txt` at 63× within 0.0005
  (fast reads 0.0813 against 0.0818), the whole overlap table to ±0.0003, display-on CIA read
  1.0002 / 1.0527 (device 1.0001 / 1.0611 — the stalls cost ~5.5 % here, as round 1's RAM loop;
  the device's 6.1 % for this loop is not reproduced, nor its RAM +10 %, `$D012` +9.6 %, `$D020`
  write +5 % with stalls on). UCI/IO1 at 16× read 1.0003 against 1.0157 (lead not pinned).
  **Item 4 tightened:** with the CIA read completing at the edge, the raster IRQ latency is 0 at
  16× and 64× in every sample, 0 at 8× with one −1 in 32 — the device's distribution (16×: 0/+1
  allowed). Round 1's −1s at 8/16× are gone.
- **D10.** `SubCycleColour::pixel_for` = `(64·phase + div) / (8·div)`: the 7.875-cycle step,
  boundaries at 6.9 / 14.8 cycles. The Q13 routine reproduces the device pixel for pixel: the
  first store's pixel by delay (x0, x0+1 for d = 2..8, x0+2 for d = 9), the doubled pixel at d = 0
  (colour 8) and d = 8 (colour 7), 8 pixels per CIA read, and `2 5 7 2 5 7` for eight stores 6
  cycles apart.
- **D11.** Already true: the VIC masks to `$3F`, so every `$40` mirror in `$D000-$D3FF` decodes
  (the device was measured in `$D000-$D0FF`; the mirrors above are unmeasured).
- **D12.** `$D030` stores bit 0 and reads `$FE | bit 0`; a store loads the menu (`1`) or `$00`
  (`0`) into the turbo state; `u64_speed` no longer gates on `$D030`. Reset leaves `$00` in every
  mode. A menu change with `$D030 = 0` leaves the state at `$00` (unmeasured).
- **D13.** `VicII::u64_extra_write` (`$D07A`/`$D07B`, with `$D031` enabled) before the VIC;
  `u64_extra_read` gives `$D0BC = $01` with SuperCPU Detect, `$D0BD-$D0BF` stay `$00`
  (unmeasured).
- **D14.** `Machine::u64_settings_changed` — `set_u64_turbo` calls it, and so does a model switch
  on the `u64` profile ("System Mode"); a host modelling more of the category calls it itself.
- **D15.** `arm_u64_reset_hold` arms 2^22 PHI2 cycles on every model (NTSC unmeasured). The
  probe here: Manual 16 → 4,194,243 (device 4,194,236), Manual 64 → 4,194,172 (4,194,166),
  registers + `$D031 = $89` → 4,194,248 (4,194,244), registers without a write → never fast.
  Stale "2.06 s" comments fixed in `vic.rs`, `lib.rs`, `u64_boot_speed_gate`, `cia_alarm_check_gate`
  and FEATURES. C64RE's `bugs/BUG-061-*` still states 2.06 s — that file is in the other repo and
  was not edited.

**Gates revised in round 2:** `subpixel_colour_gate` `a_phase_names_the_pixel_it_belongs_to`
(D10: phase 7 at divider 64 is pixel 1); `u64_turbo_gate` reset test (D12: `$FE`/`$00` after a
reset); `u64_boot_speed_gate` hold length (D15); `cia_alarm_check_gate` the ten `@64` digests again
(D9, and D15 — `booted@64` settles 220 frames), the ten `@1` unchanged; `turbo_fastpath_gate` booted
settle 150 → 230 frames (D15); `turbo_as_measured_gate` item-1/6 boot settle past the new hold, item
4 tightened (D9), the 1 MHz latency pin re-captured from the pre-890 build with the new settle
(identical histograms there and here).

**Perf after round 2** (`bench_turbo_scaling`, rings off, fast path, median of 5, alternated twice
with the pre-890 build; this host was noisier than in round 1): RAM loop 1 MHz 13.7–14.1 → 12.5–14.4
(noise); 64 MHz 0.89–0.93 → 0.95–1.06; `LDA $D012` loop at 64 MHz 0.89–0.95 → 0.81–0.90 — the
per-cycle interrupt sample plus, now, an access-kind lookup on every I/O access.
