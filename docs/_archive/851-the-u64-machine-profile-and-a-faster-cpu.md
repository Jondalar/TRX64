# Spec 851 — The U64 machine profile, and a CPU that is actually faster

**Status:** BUILT 2026-09-16 — `u64_turbo_gate` 9/9, full gate green (52 gate tests, daemon 376,
seven games 7/7), core lib 295/0; 1 MHz costs nothing measurable (§8). The UCI block the `u64` profile
carries is Spec 852.
**Repos:** TRX64 (`trx64-core`, `trx64-daemon`, `trx64-cli`). UE2 maps the firmware's speed settings onto it.
**Number:** 851 (registry: `../../../C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** Spec 815 (it extends the profile 815 built and replaces its §6 limit "does not make the
CPU faster"). The `u64` profile contains the UCI block of Spec 852, which sits on Spec 850's port.
**Origin:** the owner, 2026-09-15, on the UCI requirements: "ein Startparameter --mode UE2 oder so wäre
gut … Turbo usw dann auch gleich implementieren, soweit möglich", then: "kein Fake-CRT-Workaround … beim
Start des TRX64 einen Parameter, Default C64, optional U64/UE2/128", and the mode is simply "I emulate an
Ultimate, or I don't".

---

## §1 What exists, and one defect in it

Spec 815 built `SpeedProfile { C64, C128, U64 }` as the session's claim, settable by
`session/turbo`, `trx64cli boot --turbo`, per sandbox item and the monitor verb `turbo`. On `u64`,
`$D031` is a read/write byte and `$D030` stays open bus. Nothing runs faster.

**Defect.** `vic.rs` sets `fastmode = (value != 0)` on a `$D031` write. Bit 7 of that register is
*badline timing*, not speed: the firmware writes the same layout to `C64_SPEED_PREFER` as
`speed_index | badlines << 7` and uses `$80` for "off" (`u64_config.cc:1627-1630`). A release writing
`$80` — 1 MHz with badline timing — is reported as turbo engaged today.

## §2 The start parameter

**D1.** `--machine c64|u64|128` on `trx64-daemon` and on `trx64cli boot`/`sandbox`, default `c64`,
applied BEFORE power-on. 815 §5 recorded why that order matters: a cartridge probes in its boot stub,
inside the power-on warm-up, and a claim set afterwards arrives after the answer was given. `--turbo`
stays as an alias. A library user (UE2) constructs the machine with the same profile through the core
API.

| Value | What the machine is |
|---|---|
| `c64` | a C64. Default; bit-identical to today. |
| `u64` (also `c64u`, `ue2`) | an Ultimate: U64, U64 Elite II and C64 Ultimate are one machine here. It brings the turbo registers and the faster CPU (§3, §4) and the UCI block (Spec 852). `ue2` is not a separate machine — it is this one, with a firmware behind it. |
| `128` | 815's probe profile: the VIC-IIe register masks, nothing else. |

The Ultimates differ in the speed table only, so that is a parameter, not a machine:
`--speed-table u64ii` (U64 Elite II and C64 Ultimate, the default) or `u64` (the first U64).

The profile is recorded in the `.c64re` dump and the checkpoint ring as `machine_model`, which reads
`c64-pal` today, so a restored session knows which machine it was.

## §3 The registers — what is known, and from where

The U64's CPU and turbo logic are in the closed FPGA core. What the open side fixes:

| Fact | Source |
|---|---|
| Enable word `C64_TURBOREGS_EN`: bit 0 U64 turbo registers (`$D031`), bit 1 SuperCPU detect (`$D0BC`), bit 2 TurboEnable bit (`$D030`). Menu "Off" and "Manual" write `$00`, "U64 Turbo Registers" `$01`, "TurboEnable Bit" `$05`; SuperCPU detect adds `$02`. | `u64_config.cc:325-326`, `:1631-1634` |
| Preferred speed `C64_SPEED_PREFER`: bits 0-6 speed index, bit 7 badline timing; "Off" writes `$80`. | `u64_config.cc:1627-1630` |
| Speed index → MHz. U64: 1 2 3 4 5 6 8 10 12 14 16 20 24 32 40 48. U64-II / C64 Ultimate: 1 2 3 4 6 8 10 12 14 16 20 24 32 40 48 64. | `u64_config.cc:321-322` |
| `$D031`: bits 0-6 speed, bit 7 badline timing, active with either register mode. `$D030` bit 0: on/off at the preferred speed, only in "TurboEnable Bit" mode. A release tests `$D031` for not-`$FF` first. | the U64 manual as quoted on Lemon64 and Forum64 — secondary |
| `$D0BC-$D0BF` on a SuperCPU read `dosext << 7 \| ramlink << 6`, i.e. not `$FF`. | VICE `scpu64/scpu64mem.c:671-676` |

Not known from any open source, and modelled minimally until the owner's U64 answers: the read-back of
`$D031`'s bits, which of `$D031` and the menu wins after both were set, and the exact `$D0BC` value on a
U64.

**D2.** The U64 profile carries `regs_en`, `speed_prefer` and a speed table (U64-II by default, U64 as a
parameter). `--machine u64` starts as the menu's "U64 Turbo Registers" at 1 MHz with badline timing on:
`regs_en = $01`, `speed_prefer = $80`.

- `$D031` answers only with `regs_en` bit 0, `$D030` bit 0 only with bit 2, `$D0BC` only with bit 1;
  otherwise they are open bus, as on a C64.
- Effective speed index: the last `$D031` write while bit 0 is set; else `speed_prefer` while `$D030`
  bit 0 is set in TurboEnable mode; else `speed_prefer` when `regs_en` is 0 (menu "Manual"; "Off" is
  `$80`, which is index 0).
- `turbo_engaged()` = effective index ≠ 0. That fixes the §1 defect.

## §4 A faster CPU

**D3 — the divider.** Port pattern: VICE's TurboMaster (`c64/cart/turbomaster.c:327-360`). The
accelerated CPU counts its own cycles and advances `maincpu_clk` only on every Nth one
(`turbomaster_clk_inc`). `clk` therefore stays the PHI2 clock that the VIC, CIAs, SID, drive, checkpoint
ring, rewind and trace store are keyed on. That is the one thing that must not change.

- `C64Core6510` gains `turbo_div: u32` (1 = off) and `turbo_phase: u32`. In `clk_inc`, with
  `turbo_div > 1`: `turbo_phase += 1`; below the divider no `vic_cycle`, no `process_alarms`, no
  `clk += 1`; at the divider, phase back to 0 and the normal cycle.
- Divider = the table's MHz value, taking 1 MHz as one PHI2 cycle. The 1.5 % between 0.985 and 1 MHz is
  ignored, as UE2's S14 §4 ignores its 0.6 ppm.
- **Badline timing** (bit 7). On: the BA steal applies as today, and the CPU waits whole PHI2 cycles.
  Off: `check_ba` is skipped while the divider is above 1.
- **I/O stretch.** TurboMaster stretches every I/O access to a whole system cycle
  (`turbomaster_clk_inc_stretch`), because its I/O chips are real 1 MHz parts. On the U64 everything is
  inside the FPGA and nothing says it stretches. D3 does not stretch; the gate records both counts, so
  one hardware measurement settles it.
- **Interrupt delays** stay stamped in `clk`, as in VICE's TurboMaster. At 48 MHz the 6510's two-cycle
  delay is a few CPU cycles longer than on hardware. Stated, not fixed.

**Cost.** A faster CPU costs proportionally more host time; where real time ends depends on the host
and is not a limit of this design. The one performance rule is that the divider costs nothing at 1 MHz.

## §5 Gates

- **Default profile:** bit-identical. The seven-game gate and 815's gates pass unchanged.
- **The defect:** on `u64`, `$D031 = $80` does not engage turbo; `$D031 = $04` does.
- **Enable word:** with `regs_en = $00`, `$D031`, `$D030` and `$D0BC` read `$FF`. With `$05`, `$D030`
  bit 0 engages the preferred speed. With `$02`, `$D0BC` reads not-`$FF`.
- **Speed, through the CPU:** a `INC $02 / JMP` loop counts iterations over one frame. At index 3
  (4 MHz) the count is 4× the 1 MHz count within the badline share; badline timing off raises it by that
  share.
- **Time is PHI2 time:** CIA1 timer A underflows at the same `clk`, and `$D012` advances identically, at
  every speed.
- **Interrupts:** a raster IRQ is taken at 48 MHz, and RTI returns to the loop.
- **Start parameter:** `trx64-daemon --machine u64` answers the `$D031` probe inside the power-on warm-up
  (the 815 §5 trap, run as the probe).
- **1 MHz costs nothing:** on `c64` and on `u64` at 1 MHz, `docs/perf-compare.md` stays within noise.
- **Profile in a dump:** a `u64` session dumped and undumped comes back as `u64`.

## §6 UE2

UE2 constructs the machine with the `u64` profile; the UCI block comes with it (Spec 852), and nothing is
attached from outside. Its `C64Port` latches the core-config speed registers today (S14 §5.3,
"TURBO/SPEED latched only"); the bridge maps them onto `Machine::set_u64_turbo(regs_en, speed_prefer)`,
and ue2emu runs turbo.

## §7 Not in this spec

- 815 §3, what a set speed bit does to the C128 picture. Unrelated, still waiting on a hardware answer.
- A 65816 or SuperCPU memory map. `$D0BC` answers the detection probe; nothing behind it exists.
- Anything in the closed U64 core beyond the firmware and the manual. Every assumption above names the
  measurement that would replace it.

## §8 As built

**Where it lives.** `vic.rs`: `U64SpeedTable`, the enable word and preferred speed on the VIC
(`u64_regs_en`, `u64_speed_prefer`, `u64_d031_written`), `u64_speed()`, `u64_extra_read()` for
`$D0BC-$D0BF`. `c64_6510core.rs`: `turbo_div`/`turbo_phase`/`turbo_badline` and the divider in
`clk_inc`, the badline skip in `check_ba`. `lib.rs`: `set_machine_profile`, `set_u64_turbo`,
`set_u64_speed_table`, `turbo_divider`, the divider refreshed at each instruction boundary, the settings
carried across `warm_reset`, `run_for_full`'s instruction cap scaled by the divider. Daemon:
`--machine`, `--speed-table`, the dump manifest's `machine_model` (`u64-pal`), undump restoring the
profile. CLI: `--machine` as an alias of `--turbo`. Gate: `crates/trx64-core/tests/u64_turbo_gate.rs`,
in `scripts/gate.sh`.

> **Superseded on one point (Spec 868 §5a, 2026-09-21).** "The divider refreshed at each
> instruction boundary" was a convenience with no source behind it, and it charged UPic's
> per-row resync pair four PHI2 cycles — enough that the program ran out of raster line and
> painted half a picture. The speed is still READ at the instruction boundary; it is now
> ADOPTED at the next PHI2 edge, and the write reloads the divider's counter. Measured
> against real U64 firmware with both models in one binary: row period 126 → 63 PHI2,
> canvas 132 → 256 of 272 rows.

**What the build settled.**
- A run capped by instructions ends early at turbo speed: every `budget / 2 + 1000` cap in the core,
  the daemon's breakpoint segment and the observer registry now scales by `turbo_divider()`.
- `turbo on` writes `$83` (4 MHz, badline timing), not 815's `$0e`: the bit now really speeds the CPU
  up, and 48 MHz behind a monitor verb is a surprise, not a test.
- The dump manifest said `c64-pal` for every machine. It names the profile now, and undump sets the
  claim without touching the restored VIC registers. The enable word and preferred speed are not in the
  dump; a restored `u64` session starts from the menu default.
- Interrupt delays stay counted in PHI2 cycles, as in VICE's TurboMaster.

**Measured.** 4 MHz runs 7138 loops per frame against 1785 at 1 MHz (3.999×). Badline timing off at 4
MHz: 7138 against 6748 with it. At 16 MHz CIA1 timer A and the raster agree with 1 MHz at the same
`clk`; a raster IRQ is taken and returned from at 48 MHz. `perf_bench` pure headless, alternated with the
850 baseline: 11.525 / 11.394 MHz against 11.450 / 11.433 MHz.
