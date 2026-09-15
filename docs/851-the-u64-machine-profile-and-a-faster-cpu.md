# Spec 851 — The U64 machine profile, and a CPU that is actually faster

**Status:** PROPOSED 2026-09-15
**Repos:** TRX64 (`trx64-core`, `trx64-daemon`, `trx64-cli`). UE2 maps the firmware's speed settings onto it.
**Number:** 851 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** Spec 815 (it extends the profile 815 built and replaces its §6 limit "does not make the
CPU faster"). Independent of 850; both are needed for UE2.
**Origin:** the owner, 2026-09-15, on the UCI requirements: "ein Startparameter --mode UE2 oder so wäre
gut … Turbo usw dann auch gleich implementieren, soweit möglich."

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

**D1.** `--machine c64|128|u64` on `trx64-daemon` and on `trx64cli boot`/`sandbox`, applied BEFORE
power-on. 815 §5 recorded why that order matters: a cartridge probes in its boot stub, inside the
power-on warm-up, and a claim set afterwards arrives after the answer was given. `--turbo` stays as an
alias.

Named after the machine, not the consumer. UE2 does not run the daemon; it links `trx64-core` and calls
`Machine::set_speed_profile(U64)` itself, so a flag called `ue2` would be read by nobody who runs UE2,
and TRX64 would start knowing about one of its users.

*Open — the owner's call:* the name and whether the flag bundles anything beyond the profile (§6).

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

**Performance, stated before it is measured.** Every emulated second costs N× the CPU work. TRX64 runs
at about 13.4 emulated MHz on this Mac (`docs/perf-compare.md`), so up to about 12 MHz stays real-time;
16 MHz and above run slower than real time, 64 MHz at about a fifth. The gate records the numbers.

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
- **Performance:** emulated MHz at 1, 4, 16, 48 and 64 MHz go into `docs/perf-compare.md`.

## §6 UE2

UE2's `C64Port` latches the core-config speed registers today (S14 §5.3, "TURBO/SPEED latched only"). The
bridge maps them onto `Machine::set_u64_turbo(regs_en, speed_prefer)` with the U64-II table, and ue2emu
runs turbo.

Whether `--machine u64` should also switch on the 850 port device by default is not a machine fact: the
device belongs to the host. So it does not. That is the part of the owner's `--mode UE2` idea this spec
leaves to UE2.

## §7 Not in this spec

- 815 §3, what a set speed bit does to the C128 picture. Unrelated, still waiting on a hardware answer.
- A 65816 or SuperCPU memory map. `$D0BC` answers the detection probe; nothing behind it exists.
- Anything in the closed U64 core beyond the firmware and the manual. Every assumption above names the
  measurement that would replace it.
