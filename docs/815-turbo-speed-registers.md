# Spec 815 — Turbo/speed registers: the machines a C64 release detects

**Status:** PARTLY BUILT 2026-08-18 — §2 (the registers and their read-back), §4
(the machine profile, as a parameter AND a monitor verb) are in. §3 — what a set
speed bit DOES to the picture — is **answered for the `u64` profile** (2026-09-19: nothing;
the VIC keeps its slots, see §3.1) and stays open **only for the `128` profile** (VIC-IIe
in 2 MHz mode), where it waits on a hardware answer, see §5.
**Repos:** TRX64 only. C64RE gains nothing: this is a machine fact.
**Number:** 815 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** nothing. Default OFF, so a plain C64 session is bit-identical to
before.
**Origin:** a peer RE session, 2026-08-18: an EasyFlash release offers a turbo mode
on turbo-capable machines and gets it wrong, and the fault is invisible on a plain
C64 because the detection never fires there.

---

## §1 Why a C64 emulator grows a C128 register

A cartridge release does not ask "am I a C64". It probes, and it takes a different
code path when the probe says yes:

```asm
        LDA $D031
        CMP #$FF
        BNE  ...            ; $D031 answers -> a machine with an extended speed reg
        JSR  probe          ; else probe $D02F/$D030 -> the VIC-IIe pair
```

On a plain C64 every one of those reads is `$FF`, the probe fails, and the whole
turbo path — its timing tables, its raster delays, its speed index — is dead code
that no amount of playing will reach. A bug living in there is not hard to find. It
is unreachable.

That is the gap. Not "we should emulate a C128": we should be able to answer the
handful of probes a C64 release actually makes, so its other path can be walked.

## §2 The registers, ported not invented

**VIC-IIe ($D02F/$D030)** — `vice/src/vicii/vicii-mem.c:960-980`, verbatim:

```c
d02f_store: vicii.regs[0x2f] = value | 0xf8;
d030_store: vicii.regs[0x30] = value | 0xfc;  vicii.fastmode = value & 1;
```

The two OR masks ARE the detection. Write `$FE` and both read back `$FE`; write
`$00` and they read `$F8` and `$FC`, which differ by exactly `$04`. A release that
probes for that pair is probing for those masks, and nothing else about the machine
has to be true for the probe to answer correctly.

**Extended speed register ($D031)** — no source tree to port from, so the model is
deliberately thin: on the profile that has it, `$D031` is a plain readable/writable
byte instead of open bus, and `$D030` reads `$FF`. That is exactly what the
published detection distinguishes on, and nothing more is claimed.

On the default profile all of `$D02F`-`$D03F` stay open bus (`$FF`), which is what
a C64 does and what every existing gate expects.

## §3 What a set speed bit DOES — NOT BUILT, and that is deliberate

VICE models the CPU half of 2 MHz mode: `vicii-clock-stretch.c` (203 lines,
half-cycle accounting) and two conditions in `vicii-fetch.c:159,473` that skip the
badline DMA steal while `fastmode` is set. It does **not** model any effect on the
picture — which is consistent with the report that the fault does not reproduce
there either.

The observed fault on real hardware is: **bars where text or bitmap should be, while
colour RAM stays correct.** That last half is the informative one. Colour RAM hangs
off its own bus, not the VIC's main address bus, so a wrong bank or a wrong screen
pointer would have corrupted it too. It did not. Addressing and the colour path are
alive; only the DATA off the main bus is wrong.

The model that follows: while the speed bit is set and the raster is inside the
display window, the VIC's Φ1 accesses return the open-bus value instead of memory,
and the colour fetch is untouched. Open bus is a primitive this emulator already
has.

**It is not built, because one thing about it is still unknown** and it changes the
implementation:

- If the bars are STABLE frame to frame, the fetched value is effectively constant
  and a per-line latch is enough.
- If they MOVE with what the machine is doing, the bus value has to be carried per
  cycle.

Building it on a guess would put behaviour in this emulator that exists nowhere
else, and a later session would "discover" it. That is the same failure as a test
that freezes a bug as its expected value.

### §3.1 The U64 — answered: turbo does not touch the picture (2026-09-19)

Asked through the UE2 session. The authoritative source is Gideon Zweijtzer in
[GideonZ/1541ultimate#665](https://github.com/GideonZ/1541ultimate/issues/665) (2026-03-23):
*"In the U64 design, the VIC will always get priority, regardless of the badline setting. This
allows the VIC to produce correct graphics while turbo is on, unlike the C128 which cannot
display VIC graphics in 2 MHz mode."*

- The internal bus has 48 slots per µs on the U64 (64 on the C64U). The VIC owns its slots
  through a priority encoder — Φ1, plus Φ2 on bad lines and sprite DMA; the turbo CPU gets the
  rest.
- BA is never changed by a setting (the VIC may fetch from the cartridge port).
- "Badline Timing" (`C64_SPEED_PREFER` bit 7, the image of `$D031` bit 7) only decides whether
  the CPU halts while BA is low: on, C64 timing; off, the CPU keeps running on internal
  addresses. External-bus accesses (real SIDs, the cartridge) still go at 1 MHz and only while BA
  is high — the stall on bad lines that was #665's bug, fixed in the FPGA by 2026-05-05.
- The VIC and the arbiter are in the closed FPGA; the open firmware only sets the registers
  (`software/u64/u64_config.cc:387-393`, `:1605-1637`).

So on the `u64` profile the right model is **no effect on the picture**, which is what TRX64
does. UE2 has run UltimateDemo2026 and an 8-SID demo at 64 MHz on TRX64 without bars. The
reported symptom — bars where text or bitmap should be, colour RAM right — is the C128's
2 MHz behaviour, and it is the `128` profile that §3 is still about.

## §4 The profile is a parameter and a monitor verb

One switch sets everything above, and it is off by default:

| Profile | $D02F/$D030 | $D031 | Speed bit |
|---|---|---|---|
| `c64` (default) | open bus | open bus | — |
| `128` | VIC-IIe masks | open bus | stored, no effect yet (§3) |
| `u64` | reads `$FF` | read/write | stored, no effect yet (§3) |

- as a parameter — `session/turbo { mode, on, speed }` on the daemon,
  `trx64cli boot --turbo 128 [--turbo-on]` for a one-shot, and
  `trx64cli sandbox --turbo 128` so a probing ROUTINE can be exercised without a
  full boot (also per batch item, as `"turbo"`)
- as a monitor verb, so a human can flip it mid-session and watch what a release
  does differently

```
turbo                 what this session claims to be, and what is set
turbo mode c64|128|u64  pick the profile
turbo on | off        set/clear the speed bit, as the release would
turbo speed 8         the extended speed value ($D031 on the turbo profile)
```

Bare `turbo` reports rather than toggling: "which machine does this session claim
to be" is the question worth being able to ask, and a verb that silently flips
state when you meant to look is a verb that gets used wrong once and distrusted
after.

## §5 Gates

- The default profile is unchanged: `$D02F`-`$D03F` read `$FF`, writes do nothing.
  Every existing VIC gate covers this by continuing to pass.
- On `128`: write `$FE`, read `$D02F` == `$D030`; write `$00`, read them `$F8` and
  `$FC` (XOR `$04`). That is the probe, run as the probe — and run through the CPU
  BUS, not by poking the chip. The first gate poked `vic.write_reg` directly and
  passed while the real detection failed: testing the wrong door proves the wrong
  thing.
- On `128`: the speed bit is readable back and reported, and setting it changes
  nothing else — the gate asserts the picture is IDENTICAL with the bit set and
  clear, which is what makes §3 a visible hole rather than a forgotten one.
- On `u64`: `$D031` round-trips and `$D030` reads `$FF`, so the type-2 detection
  path resolves.
- The claim is the SESSION's and survives BOTH a warm reset and a power-cycle;
  only a new session starts at `c64`. The first cut kept it on the chip, where a
  power-cycle dropped it — and that is not a detail, because `do_power_on` warms
  the boot by five million cycles and **a cartridge probes in its boot stub, inside
  exactly that warm-up**. The flag was set, the run looked fine, and the detection
  had already answered "plain C64". `trx64cli boot` therefore sets the claim BEFORE
  the mount.
- The speed BIT is a register, so a power-cycle clears it — correctly. `--turbo-on`
  is applied after the mount for that reason.

## §6 What this spec does not do

- **It does not make the CPU faster.** No profile changes the clock. A release that
  sets the speed bit takes its turbo code path, and its timing tables are then wrong
  in a way the real machine would not be. That is a limitation to state, not to hide
  — and it is not fixable by half: a 2 MHz core is its own piece of work.
  **Superseded for `u64` by Spec 851** (2026-09-16): that profile's CPU now runs at the
  speed the firmware's layout selects, and `$D031` reads speed index | badline timing
  << 7 — this spec read any non-zero value as engaged, so `$80` (1 MHz) counted as
  turbo. The `128` profile still stores its bit and nothing more.
- **It does not emulate a C128.** No VDC, no 80 columns, no MMU, no native mode.
  This is a C64 that answers the probes a C64 release makes.
