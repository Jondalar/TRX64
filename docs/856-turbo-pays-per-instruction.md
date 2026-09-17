# Spec 856 — Turbo pays per instruction

**Status:** PROPOSED 2026-09-17 — nothing built. D0 decides whether anything is.
**Repo:** TRX64 (`trx64-core`). UE2 consumes the result and measures it; it builds nothing here.
**Number:** 856 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** Spec 851 D3 (the turbo divider this spec leaves untouched).
**Origin:** the owner, 2026-09-17, on the UE2 emulator: the core holds up at 1 MHz and
collapses as the MHz setting goes up. Is that the price of a faster CPU, or something we
charge on top of it?

---

## §1 What turbo is here, and what it is not

Turbo is **CPU-only**, as on the Ultimate. `c64_6510core.rs:828` is a port of VICE's
TurboMaster: the CPU counts its own cycles and advances `clk` — the PHI2 clock that VIC,
CIAs, SID, alarms and the 1541 are keyed on — only on every `turbo_div`-th of them. Below
the divider there are no alarms, no line samples and no VIC tick. The picture stays at
50 Hz, the SID stays in tune, the drive stays at 1 MHz. The speed table comes from the
firmware (`vic.rs:708`, top speed on a U64-II is 64 MHz).

It is **not** warp. Nothing in this spec changes that.

## §2 The two costs

**The unavoidable one.** 64 MHz is 64× the 6502 work per emulated second — about 63 million
CPU cycles per second of real time on PAL (985 248 Hz × 64). An FPGA does not pay for that;
a software CPU does. No change in this spec touches it.

**The one we add.** `run_for_full_capped_dbg` (`lib.rs:2840ff`) does its synchronisation at
every *instruction* boundary. That loop was shaped when an instruction was 2–7 PHI2 cycles
long, so the boundary cost was spread over several cycles. At turbo it is the other way
round. With a mean instruction of roughly 3.5 CPU cycles — an **assumption**, not a
measurement — an instruction at 64 MHz crosses a PHI2 edge about one time in eighteen. **About
94 % of boundaries arrive with `clk` exactly where it was**, and each of them still pays:

| work at the boundary | when it actually has something to do |
|---|---|
| CIA1 + CIA2 `update_to(clk)` | `clk` moved |
| IRQ/NMI restamp into `IntStatus` (VIC, CIA1, CIA2, expansion) | `clk` moved **or** the instruction touched IO — see §3 |
| `FullBus` + `FullScBus` built afresh, about fifty fields | never, in itself; it is the price of the borrow shape |
| banking, `$00`/`$01` and `$DD00` output written back to `Machine` | the instruction touched the processor port or IO |
| `sid.tick(instruction_cycles)` | `clk` moved (already early-returns on 0, `sid.rs:285`) |
| drive `catch_up_to(clk)` | `clk` moved (already a no-op on an equal clock, `drive.rs:839`) |
| `iec_drive_write`, `sample_pc_change`, head trace | the drive ran |
| `cpu_history` + `delta_ring` record | always, **if enabled** — their meaning is per instruction |

Of these, only the last is per instruction by nature. The rest is catch-up to a clock that
did not move.

## §3 The one that is not free to skip

An IRQ acknowledge happens **inside** an instruction: reading `$DC0D` clears the CIA's
flags, writing `$D019` clears the VIC's. The line drops at that moment. It reaches
`IntStatus` in exactly two places — the per-PHI2 stamp in `clk_inc`, and the boundary
restamp. Below the divider `clk_inc` returns early, so **within a PHI2 cycle the boundary
restamp is the only path an acknowledge has.**

Skip it whenever `clk` is unchanged and the handler at 64 MHz sees a line that is still
high, returns, and takes the same IRQ again — up to ~20 times before the next PHI2 edge
lands the acknowledge. That is an interrupt storm, and on most handlers a stack overflow.

So the rule cannot be "sync when the clock moved". It has to be **"sync when the clock moved
or the instruction touched anything that is not RAM."**

## §4 Deliverables

**D0 — Measure before deciding.** UE2 is measuring a 64 MHz demo now, first as shipped, then
with the reverse rings off (`TRX64_CPUHISTORY=0`). Add a profile of the same window
(`samply` or Instruments) that splits wall time into `execute_one` and the boundary block.
With the boundary share `s` and the skippable fraction `f` from the same run (boundaries
with `clk` unchanged and no IO), the most this spec can gain is `1 / (1 − s·f)`.

**Build D1–D4 only if that bound is at least 1.5×.** Below it, this spec closes RESOLVED with
the measurement recorded, because the cost is `execute_one` itself and there is nothing at
the boundary worth the risk. The threshold is a proposal; the owner sets it. Spec 807's
premise did not survive its own baseline — that is why this one starts with its baseline.

**D1 — Gates first, green on the current loop.** Written and passing *before* the loop
changes, so they pin today's behaviour rather than describe the new one:

- *an acknowledge inside one PHI2 cycle holds at 64 MHz:* a CIA1 timer IRQ whose handler
  reads `$DC0D` is entered exactly once per underflow;
- the same for a raster IRQ acknowledged via `$D019`;
- *the fast path changes nothing:* the same turbo workload for N frames with the fast path
  off and on gives identical machine state at every frame boundary — RAM, CPU registers,
  `clk`, CIA and VIC state. Exact equality, not a bound: everything the fast path skips is
  idempotent on an unchanged clock, so any difference is a defect.

The last one is why D4 exists.

**D2 — The bus reports what it touched.** `FullBus` sets a flag on any access that is not a
plain RAM or plain ROM read: `$D000-$DFFF` while IO is mapped, IO1/IO2, cartridge ROML/ROMH
(a mapper may bank on a read, and the cart read-set records them for that reason), and the
processor port at `$00`/`$01`. Conservative on purpose. A false "touched" costs one sync; a
missed one breaks §3.

**D3 — The inner loop.** Inside the bus scope, while `turbo_div > 1`:

```
loop {
    execute_one(...)
    executed += 1
    stop if: clk moved, IO touched, halt requested, device stop,
             instruction cap reached, next PC is a breakpoint, exec-watch hit
}
then: drop the bus, run the boundary block once
```

Breakpoint, exec-watch and the instruction cap stay per instruction, so a debugger sees no
difference. A `$D031` write is IO, so a speed change still takes effect with the next
instruction. A DMA arm is an IO write, so the transfer still runs at the boundary Spec 853
put it at.

At `turbo_div == 1` every instruction advances `clk` — `clk_inc` increments on every call —
so the loop never runs a second iteration. **The stock machine's path is the one it has
today.** The seven-game, iso-VIC and cartridge gates are the proof.

**D4 — A switch.** `TRX64_TURBO_FASTPATH=0` and a public field on `Machine`, following
`TRX64_CPUHISTORY`. D1's equality gate needs both paths in one binary. After that it is the
way to rule the fast path out when a turbo title misbehaves, without a rebuild.

## §5 What changes for whom

| | effect |
|---|---|
| a stock C64 / 1 MHz | none — see D3 |
| turbo, RAM-only code (depackers, maths, plasma, 3D) | the boundary block runs about once per PHI2 cycle instead of once per instruction |
| turbo, IO-heavy code | little — every IO access still forces the sync, which is what keeps §3 correct |
| the 1541 and fastloaders | none — the drive follows `clk`, and `$DD00` is IO |
| observers, trace, breakpoints | none — they sit inside `execute_one` or in the inner loop's stop checks |
| reverse rings | unchanged and independent — if enabled they still record every instruction |

**The reverse rings, as they already are.** Their depth is sized in instructions
(`TRX64_REVERSE_SECONDS`, 10 s, assuming a 1 MHz instruction rate). At 64 MHz the same ring
covers about 1/64 of that in emulated time — roughly 0.16 s. That is true today and this spec
does not change it; it is written down so nobody reads the depth as seconds at turbo.

## §6 Not in this spec

- **Hoisting the bus build out of the loop for every speed.** It would help 1 MHz too, but
  it restructures the stock path, and D3 is shaped so the stock path does not move.
- **`check_ba` on every memory access** with badline timing on. That cost is per CPU cycle,
  so it grows with the speed by nature. `$D031` bit 7 already turns it off.
- **Making the rings cheaper at turbo.** Their switch exists; their semantics are per
  instruction.
- **Warp.**
- **UE2's pacing, audio underrun and frame drop behaviour** when the host cannot keep up.
  That is theirs.

## §7 Open

- **Does the hardware agree with §3?** TRX64 follows VICE's TurboMaster: an acknowledge is
  visible to the next instruction, and the interrupt delay counts in PHI2 cycles. D1 pins
  that behaviour; it does not prove the U64's FPGA matches it. No hardware measurement until
  a real title demands one.
- **The 94 %** rests on an assumed mean instruction length. D0 replaces it with the measured
  fraction.
