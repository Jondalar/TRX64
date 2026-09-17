# Spec 856 — Turbo pays per instruction

**Status:** BUILT 2026-09-17 — D0 cleared (bound ≈ 1.6×), D1–D4 built; UltimateDemo2026 at 64 MHz now holds real time in UE2 (was 0.72–0.98×), 1 MHz unchanged. See §8.
**Repo:** TRX64 (`trx64-core`). UE2 consumes the result and measures it; it builds nothing here.
**Number:** 856 (registry: `../../../C64ReverseEngineeringMCP/specs/README.md`).
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
| turbo, IO-heavy code | less — every IO access still ends the batch, which is what keeps §3 correct; §8 has the measured number |
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

## §8 As built (2026-09-17)

**D0 — measured in two places, and it cleared.** UE2 ran UltimateDemo2026 at 64 MHz on an
Apple M4 — REU 16 MB, UCI and Ultimate Audio on the port, rings already off (UE2 turns them off
before it builds the `Machine`) — at **0.72–0.98× real time**, the emulation thread at 99–100 %
of one core. Its `sample` profile of that thread:

| where | share |
|---|---|
| run loop self (the inlined boundary work, bus build included) | ~16 % |
| `Ciat::update` + `Cia::update_ta` | 16–18 % |
| expansion chain lines + `dma_pending` | 7–8 % |
| `full_sc::execute_one` self | ~12 % |
| 6510 decode + `clk_inc` + operand access | ~15 % |
| `FullBus::read` | ~9 % |
| VIC tick incl. draw | 4–5 % |
| SID tick | 1–2 % |
| `DeltaRing::commit` (disabled, still an out-of-line call) | ~1 % |

The boundary share is about 40 % of the thread; with §2's 94 % the bound is ≈ 1.6×, above the
1.5× threshold. **The deviation from §4:** D0 asked for the split before building. The split
came from UE2's profile, the build followed, and the before/after bench below is the check
that the bound was real rather than an artefact of inlining.

**Measured, `bench_turbo_scaling`** (bare machine, no IRQs, `run_for_full` per PAL frame,
median of 5, both runs on the same host, rt-x = emulated / wall):

| load | MHz | rings | before | after |
|---|---|---|---|---|
| RAM loop | 1 | off | 11.80× | 11.82× |
| RAM loop | 16 | off | 2.83× | 3.77× |
| RAM loop | 48 | off | 1.07× | 1.72× |
| RAM loop | 64 | off | **0.86×** | **1.40×** |
| RAM loop | 64 | on | 0.77× | 1.09× |
| `LDA $D012` every third instruction | 64 | off | 0.79× | 1.07× |
| `LDA $D012` every third instruction | 64 | on | 0.71× | 0.89× |

At 1 MHz nothing moved beyond noise, which is what D3 promised. The IO loop gains less and
still gains: two instructions in three stay inside the batch.

**D1 — five cases in `turbo_fastpath_gate`, green on the old loop before the new one existed:**
acknowledge-once for CIA1 (`$DC0D`) and the raster (`$D019`) at 1 and 64 MHz, badline timing on
and off; and exact equality with the fast path off and on, frame by frame, for a mixed IRQ+IO
workload, the same with a 16 MB REU on the port, and a booted machine (KERNAL IRQ, keyboard
scan, drive). The equality hashes every instruction, every bus record and every interrupt and
compares the full runtime checkpoint.

**Both kinds of failure were provoked, not assumed.** Dropping the IO break turned all five
red — the handlers storm. Dropping the pre-fetch reset (next item) turned the three equality
cases red and left the two acknowledge cases green, which is why the equality gate hashes bus
records and not only instructions.

**D3 needed one thing §4 did not name.** `FullScBus` carries per-instruction state that a fresh
bus starts clear: `fetched`, which decides whether a bus record carries the live PC (the
interrupt prologue) or the synthetic post-fetch one. An instruction batched into the same bus
without resetting it attributes its fetch to the previous opcode — invisible in the machine
state, wrong in every trace. The inner loop resets `fetched` and `cur_op` before each
instruction. A stop condition found inside the batch (instruction cap, breakpoint, exec-watch)
breaks out and leaves the stop to the top of the loop, so the order of checks is the old one.

**D2** sets the flag in `io_read`, `io_write`, `cart_read`, `cart_write`, `port_snoop` and on
both processor-port addresses, read and write. **D4** is `Machine::turbo_fast_path` plus
`TRX64_TURBO_FASTPATH=0`.

**Gate:** `turbo_fastpath_gate` is in `scripts/gate.sh`. Full gate green after the change —
12 suites / 151 tests, daemon 382, seven games 7/7.

**Named limits:**
- **Code running from cartridge ROM gets no fast path.** Every fetch from ROML/ROMH goes
  through `cart_read`, which sets the flag. Conservative on purpose — some mappers bank on a
  read — and liftable with a per-mapper "reads have no side effects" answer. Not needed for
  anything measured here.
- **The reverse rings stay per instruction.** With them on, 64 MHz is 1.09× instead of 1.40×.
- **`DeltaRing::commit` is still an out-of-line call per instruction when disabled**, ~1 % in
  UE2's profile. Outside this spec.
- **The CIA timer update is now the largest single item and barely moved** — see below. The
  repeats at unchanged `clk` were not where its cost was.

**Confirmed on the demo (UE2, same host, same conditions as the D0 run).** UE2 0.3.0 with the
core at `a6e0465`, UltimateDemo2026 v1.0.1 at 64 MHz, headless, real time, audio on, four
15 s emulated windows. Emulated / wall, and the process's CPU:

| window | before (`1ce84b0`) | after (`a6e0465`) |
|---|---|---|
| 1 | 0.846 @ 100 % | **1.000** @ 76 % |
| 2 | 0.772 @ 99 % | **0.999** @ 85 % |
| 3 | 0.718 @ 100 % | **0.999** @ 69 % |
| 4 | 0.982 @ 99 % | **0.997** @ 97 % |

Real time in every window, and the owner heard the audio underruns stop. Two caveats from UE2:
the windows are placed by wall time, so a faster run is further into the demo in the same
window; and window 4 at 97 % leaves little headroom.

Profile of the emulation thread, share of all samples (after includes idle time where it runs
ahead, so its shares read lower):

| where | before | after |
|---|---|---|
| run loop self | 15–17 % | 5–11 % |
| expansion chain lines + `dma_pending` | 8–9 % | 2–5 % |
| `Ciat::update` + `Cia::update_ta` | 16–18 % | **12–16 %** |
| `full_sc::execute_one` self | 11–13 % | 11–14 % |
| 6510 decode / `clk_inc` / operand access | 14–15 % | 14–16 % |
| `FullBus::read` | 8–9 % | 8–13 % |
| VIC tick incl. draw | 4–5 % | 5–6 % |

The two items this spec aimed at dropped as intended. The CIA update did not, which says its
cost was never the no-op repeats at an unchanged clock but the real per-PHI2 advance. It is now
the largest single item; after it, everything left is per instruction.
