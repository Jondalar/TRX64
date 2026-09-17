# Spec 857 — The CIA alarm is a comparison, not an update

**Status:** BUILT 2026-09-17 on branch `spec-857-cia-alarm-check`, **not merged** — D0–D4 built, full gate green, 1 MHz 10.53× → 13.33× and 64 MHz 0.97× → 1.22× real time. `main` is untouched until the owner decides. See §8.
**Repo:** TRX64 (`trx64-core`).
**Number:** 857 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** Spec 856 (its profile is what found this).
**Origin:** UE2's profile after 856, 2026-09-17. With the boundary work batched, UltimateDemo2026
at 64 MHz holds real time, and the CIA timer update is the largest single item left at
12–16 % of the emulation thread. 856 barely moved it. The owner asked why, and whether it is
ours to fix.

---

## §1 What VICE does, and what we do in its place

VICE's cycle-exact core dispatches alarms in two places, and in both it is a **comparison**:

- in every instruction's prologue — `6510dtvcore.c`: `while (CLK >= alarm_context_next_pending_clk(ALARM_CONTEXT)) alarm_context_dispatch(…)`;
- in every cycle's `interrupt_delay()` — `mainc64cpu.c:99`, the same loop.

Nothing is due, nothing happens. A CIA timer is caught up to the clock (`cia_update_ta` /
`cia_update_tb`, `ciacore.c:291ff`) only inside the alarm handler that fires at a predicted
underflow, and on a register access — the file says so itself above those functions: *"called
everywhere but in the alarm functions"*.

TRX64 has no alarm context. `FullScBus::process_alarms` (`full_sc.rs:398-407`) stands in for
it, and it calls `Cia::update_to` for both CIAs — which is **VICE's register-access catch-up**,
not its alarm check. It runs in the same two places, plus once more at the run loop's boundary
(`lib.rs:2912`):

| where | how often | per call |
|---|---|---|
| instruction prologue (`c64_6510core.rs:2475`, again after a dispatched interrupt) | every instruction | `update_ta` + `update_tb`, both CIAs |
| `clk_inc` → `interrupt_delay_alarms` | every PHI2 cycle | the same |
| run loop boundary | every instruction, or every 856 batch | the same |

At 1 MHz that is three full catch-ups per PHI2 cycle where VICE compares twice. At 64 MHz a PHI2
cycle holds about eighteen instructions, so about **thirty-six** full catch-ups from the prologue
alone — which is why batching the boundary in 856 left the CIA item where it was.

## §2 Why the catch-up cannot simply become a comparison

Updating every CIA every cycle hides two places where the port does not keep an alarm
current, and one place where a reader relies on the timer being caught up. Each would turn
into a missed or late interrupt, or a wrong readout, the moment the catch-up stops.

**1. Timer B's alarm is predicted and never fired.** VICE's `cia_update_tb` (`ciacore.c:317-344`)
dispatches `ciacore_inttb` for every Timer B alarm up to the clock. Ours predicted
`tb_alarmclk` on every register write and had no dispatch at all: `update_tb` only settled the
counter. Timer B's underflows were latched because `update_to` caught Timer B up every cycle.

**2. Timer B counting Timer A underflows.** VICE's `ciat_single_step` (`ciatimer.h:382-389`)
sets `CIAT_STEP` **and re-predicts the timer's alarm**. Ours (`cia.rs:276-280`) set the step
bit only. The next per-cycle `tb.update` consumed the step anyway.

*This section first named the `.c64re` restore as a third gap. It is not: `restore_cia`
re-predicts both alarms at its end (`c64re_snapshot.rs:481-482`). The first reading stopped
forty lines into the function.*

**3. Readers that bypass the catch-up.** `Cia::peek` (`cia.rs:848-856`) returns `ta.cnt` /
`tb.cnt` directly, and so does the snapshot capture. Both are correct today only because the
counters were caught up a cycle ago. With an alarm check the stored count is valid at the
timer's own `clk`, not at the machine's — the monitor would show a stale timer and two
checkpoints of the same machine would differ in representation.

## §3 Deliverables

**D0 — Baseline.** `perf_bench` at 1 MHz and `bench_turbo_scaling` at 1–64 MHz, both paths in
one binary via D4 and alternated, because a hot-path change inside the machine's ~2 % drift
needs an alternating A/B and never a single run (the TOD lesson).

**D1 — Gates first, green on today's code.** Exact equality with the check off and on, frame by
frame, over the instruction stream, every bus record, every interrupt and the full checkpoint —
the 856 shape. Workloads chosen to stress what §2 names:

- Timer A continuous with an IRQ acknowledged via `$DC0D`, and one-shot;
- **Timer B counting Timer A underflows** with latches 0, 1 and 2, IRQ on Timer B;
- CIA2 → NMI;
- the TOD alarm;
- a `.c64re` capture mid-count, restored, continued;
- a booted machine;
- and `peek` of all four timer bytes at points that are not register accesses — equal on both paths.

Plus `cia_tod_gate`, `snapshot_roundtrip_fidelity` and the seven games, unchanged.

**D2 — Make the alarms carry what the catch-up carried.** Neutral on today's path, so D1 stays
green before D3 exists:

- Timer B gets its alarm handler (`inttb`, after `ciacore_inttb`) and `update_tb` dispatches it,
  as `update_ta` already did for Timer A;
- `Ciat::single_step` re-predicts the alarm, as VICE does;
- `peek` and the snapshot capture report counts caught up on a copy, so neither mutates — to
  `checked_clk`, the clock the machine last checked the alarms at, and **not** to `Cia::clk`,
  which is the TOD tick counter and runs one ahead of the CPU after every `tick()`. The first
  cut used `Cia::clk`; the frozen digests moved at once, which is what they are for.

**D3 — The comparison.** `process_alarms` calls `update_to` for a CIA only when
`ta_alarmclk <= clk || tb_alarmclk <= clk`. The boundary's catch-up goes the same way. Its
interrupt restamp stays exactly as it is: that is the path an acknowledge inside one PHI2
cycle takes (856 §3).

**D4 — A switch,** `TRX64_CIA_ALARM_CHECK=0` and a field, for the equality gate and the A/B.

## §4 Stop rules

- **If D1 diverges on today's semantics, stop and read VICE before choosing a side.** A
  divergence in the cascade cases may mean the per-cycle catch-up was the deviation. That is a
  finding to record and decide, not a difference to accept quietly.
- **Not merged without a gain beyond noise** in the alternating A/B at 1 MHz, and a measurable one
  at 64 MHz. A refactor of the interrupt path that buys nothing is risk without return.

## §5 Not in this spec

- The SDR / serial shift register alarm and the TOD machinery beyond its alarm — both have their
  own paths and were not read for this.
- The VIC tick, the 6510 decode and `clk_inc` itself — the cycle-exact part of the core.
- VICE's "skip alarms nobody needs" optimisation in `ciacore_intta` (it re-arms Timer A only
  when an IRQ, PB6, the shift register or a cascade wants it). Worth reading later; ours always
  re-arms, which is correct and slower.

## §8 As built (2026-09-17, on the branch)

**D0 — measured, alternating in one binary** (`bench_cia_alarm_check`, K = 9, the side that runs
first alternates each pair):

| load | check off | check on | wall | pairs faster |
|---|---|---|---|---|
| booted, READY prompt, KERNAL IRQ, 1 MHz | 10.53× | **13.33×** | −21.0 % | 9 / 9 |
| RAM loop, 64 MHz | 0.97× | **1.22×** | −20.4 % | 9 / 9 |

Well beyond the ~2 % drift, and every pair points the same way. It is the stock machine that
gains as much as the turbo one, because the prologue ran the catch-up at 1 MHz too.

**D1 — `cia_alarm_check_gate`, three cases, in `scripts/gate.sh`.** Lockstep equality every 7919
cycles — instruction stream, bus records, interrupts, full checkpoint, `peek` of all 32
registers — over Timer A continuous, Timer A with a timer read in the loop, one-shot, Timer B
cascade at latches 0/1/2, CIA2 NMI, the TOD alarm, a checkpoint restored mid-count and a booted
machine, each at 1 and 64 MHz. Plus frozen digests of the pre-857 behaviour.

**The frozen digests earned their place twice.** First run: the restore case diverged at 64 MHz
**on the code before 857 touched anything** — checkpoints did not carry `turbo_phase`, so a
restored machine at turbo ran one instruction more in its first 7919 cycles. A defect since 851
(v0.6.0), fixed in its own commit (`7ce542a`) so it can go to `main` without 857. Second: the
first cut of D2 caught readers up to `Cia::clk`, which is the TOD tick counter and runs one
ahead of the CPU after every `tick()`, and the digests moved at once.

**Both failure kinds provoked before the gate was trusted.** Taking Timer B out of the alarm
check turned the cascade case red; letting `peek` read the stored counters turned the timer and
booted cases red. The restore case stays green under both — it compares two machines with the
same setting, which is its job.

**D2** as §3 states, corrected once (§2 no longer claims the restore needs re-arming).
`checked_clk` is set at every alarm check, whether or not anything is due. **D3** in
`FullScBus::process_alarms` and at the run loop boundary; the boundary's interrupt restamp is
unchanged. **D4** is `Machine::cia_alarm_check` / `TRX64_CIA_ALARM_CHECK=0`.

**Gate:** full gate green on the branch — 13 suites / 154 tests, daemon 382, seven games 7/7.
Clippy: no warning on any line this branch added.

**Confirmed by UE2 on the demo** (UltimateDemo2026 at 64 MHz, run D conditions, idle skip off,
one run each). Emulated/wall stays at real time and the cost is what moved — process CPU per
window:

| | window 1 | 2 | 3 | 4 |
|---|---|---|---|---|
| a6e0465 (run D) | 75.9 % | 85.2 % | 69.1 % | 96.5 % |
| **857 branch** | **66.3 %** | **74.2 %** | **59.1 %** | **80.8 %** |
| 857 branch, check off | 86.2 % | 94.3 % | 72.6 % | 99.4 % |

A fresh profile says where it went: `Ciat::update` + `Cia::update_ta` are **0.0 %** and out of
the top thirty, from 15.9 % on a6e0465. Everything else keeps its absolute cost and rises as a
share of a smaller total. Function: 435 workspace tests, cartridge smoke 27/27 with the Action
Replay freeze, upstream `uci-targets`, and mandelbrot-upic with turbo — all identical with the
check off and on, and the demo's audio judged clean by ear on the 857 build. The one apparent divergence, two mandelbrot frames differing, reproduces
between two runs of the SAME build: the picture cycles palettes and the phase follows the start
timing.

**Open, and measured by nobody yet: the check-off path looks slower than before 857.** UE2's
third row sits above run D in every window and dips below real time in the last one. If that is
real it is D2's cost — `update_tb` now dispatches alarms and arms lazily where it only settled a
counter before — and it would mean the kill switch is worse than the code it falls back to. One
run each and `ps` noise cannot tell them apart; it needs the alternating worktree A/B against
`main`.

**Also open:** the lazy-arm clause in `alarm_due` means a Timer B counting Timer A underflows
with nothing pending still catches up every time — correct, and no faster than before, for the
rare program that uses the cascade.
