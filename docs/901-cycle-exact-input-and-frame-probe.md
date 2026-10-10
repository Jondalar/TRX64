# 901 — Cycle-exact input and a frame probe

**Status:** BUILT on branch `spec-901` (TRX64 half; not merged). Tests: `spec901_*` in the daemon suite, `keyboard::tests` and `full::joystick_gate_tests` in core.
**Repo:** TRX64 (daemon + core). The C64RE half is 899.
**Number:** 901 (board: `C64ReverseEngineeringMCP/specs/README.md`)
**Origin:** C64RE issues #67 and #68 (mrr19121970, Mega Vault C64 port, 2026-10-10); owner split
2026-10-10: capability here, step notation in C64RE 899. Patch: additions only.

## 1. What is wrong

**Input (#67).** A scripted press lands where the script's run call ended, and the C64RE step
engine runs whole frames, so every press lands on a frame boundary. The machine is deterministic,
so a bug that depends on where the beam is when input arrives (read-modify-write of `$D011`, a
raster split, a tick-vs-frame race) either always shows or never does.

Read, not assumed (main at `a9ef8d4`):

- `session/key_down` / `key_up` write the held set (`keyboard.rs` `key_down`), and the CIA reads
  the matrix live on every access (`read_rows_for_pa(now, pa)`). Nothing latches keys to a frame.
  But a held key takes effect at the cycle the RPC arrives — which is wherever the last run call
  stopped. There is no way to say "at cycle X" without stopping the machine there.
- The keyboard already has a cycle-gated queue: `events: Vec<KeyEvent { key, start_cycle,
  end_cycle }>`, folded into `read_rows_for_pa` by `now`. `session/type` fills it. `key_down`
  does not use it.
- `session/joystick_set` writes `machine.joystick1/2`; the full machine copies them into the bus
  **once per run call** (`full.rs` `joy1: self.joystick1` at the three run entries). Within one
  run call the joystick is frozen; it is not cycle-exact even in principle.
- Stopping the machine at cycle X to press there is not a fix: `session/run` checks its budget
  at instruction boundaries (`run_for_capped`), so it overshoots by up to an instruction plus any
  stall, and every `session/run` call does the per-frame background work (checkpoint capture,
  render, cart/disk write-back, recorder) even when it ran a part of a frame.

**Probe (#68).** A `Then` is evaluated where a step stands. A state wrong for a few frames is
missed unless a read happens to land on it, and one RPC per frame per address is the only way to
look today (`session/read_memory` reads the machine where it stands).

## 2. Decision

**D1 — input can be scheduled at an absolute cycle.** `session/key_down`, `session/key_up`,
`session/joystick_set` and `session/joystick_clear` take an optional `at_cycle` (u64, absolute
`c64Cycles`). Absent → today's behaviour, unchanged. Present:

- `at_cycle` in the past (`< clk`) → error -32602, naming both values. Equal to `clk` → applies
  now.
- The event is queued in the machine and takes effect **at that cycle**: a CIA1 port read at a
  cycle `>= at_cycle` sees it, one before does not. Keys go through the keyboard's cycle-gated
  model (extend `KeyEvent` or add a scheduled held-set change — whichever keeps
  `read_rows_for_pa` the one place that answers); the joystick gets the same: the bus reads the
  joystick state at the read's cycle from a schedule, not a copy taken at run entry.
- The reply adds `scheduledAt` (the cycle) and keeps every existing field. `pressed` /
  `input_status` report the state **now**, plus `scheduled: [{cycle, kind, key|port, …}]`.
- Determinism: the same schedule on the same machine gives the same reads, whether the run that
  crosses `at_cycle` is one call or many.
- `release_keys` and a reset / power cycle drop the pending schedule along with the held set.

**D2 — `session/frame_probe`.** One call advances the machine N frames and samples a list of
addresses once per frame at a fixed beam position, inside the daemon.

Params:
- `frames` (1..=50 000) — the window.
- `line` (raster line), `cycle` (raster cycle, default 0) — the sample point in each frame. Out of
  range for the model → -32602.
- `addresses`: `[{addr, len?=1, lens?="cpu"}]` — the same `lens` values as `session/read_memory`
  (`cpu`, `ram`, `rom`, `io`, …), read with `peek_lens` (no side effects). Total bytes per sample
  ≤ 256.
- `mode`: `"series"` (default) or `"assert"`.
  - `series`: returns `rows: [{frame, c64Cycles, line, cycle, bytes}]` — the first sample, then
    only the frames where any sampled byte differs from the previous sample. Plus `samples` (count)
    and `changes`.
  - `assert` with `expect: [{addr, value, mask?=$FF}]` (each addr must be one of `addresses`):
    stops at the **first** sample where an expectation fails and returns `{held: false, frame,
    c64Cycles, line, cycle, actual: [...]}`; the machine stays there. All held →
    `{held: true, samples}`.
- Sample point: the run steps at instruction boundaries (the core cannot stop inside an
  instruction); the sample is taken at the **first instruction boundary at or after** `(line,
  cycle)` in that frame, and the row reports the exact `c64Cycles`, `line` and `cycle` it was taken
  at. Same input, same rows.
- `frame` counts from 0 at the call's start position. The machine ends after the last sample
  (series) or at the failing sample (assert) — not at a frame boundary.
- Refused while `session.running` (pause first), like `session/run`. Works the same on a sandbox
  daemon and on the shared session; it advances the machine like a run.
- The per-frame background work (checkpoint capture, cart/disk write-back, recorder) runs once per
  **whole frame** crossed, exactly as `session/run` would for the same span — not once per sample.
  Rendering is not needed per frame and is not done per sample.
- Scheduled input (D1) inside the window takes effect at its cycle as in any run.
- **Breakpoints and watchpoints.** The probe advances through the same path as `session/run`
  (`run_budget_honouring_breaks`): an armed exec breakpoint or watchpoint stops it, the machine
  stays at the stop and `ctrl_stop` records it. The reply is `{stopped: "breakpoint" | "watchpoint"
  | "device", frame, c64Cycles, line, cycle, pc, breakpoint: {pc, num, registers}, rows, samples,
  changes, layout}` — `breakpoint` is the object `session/run` returns, `rows`/`samples` are what
  was collected before the stop (series mode). With nothing armed, results are unchanged.
- `lens` must be one of `cpu`, `ram`, `rom`, `io`, `cart`; anything else is -32602
  (`session/read_memory` falls back to `cpu` silently; the probe does not).
- "Whole frame crossed" is counted in emulated cycles from the call's start: a 10-frame probe
  sampling at line 250 from line 0 crosses 9 whole frames and does the work 9 times, which is
  what `session/run` does over the same span.

**D3 — method names, for C64RE 899:** `session/key_down|key_up|joystick_set|joystick_clear`
`{…, at_cycle?}`; `session/frame_probe {frames, line, cycle?, addresses, mode?, expect?}`. Both
listed in the daemon's method list / capability surface, wherever `session/read_memory` is.

## 3. Out

- No sub-instruction sampling (the core steps instructions; D2 reports where it actually sampled).
- No seeded jitter or sweep in the daemon — that is step-notation work, C64RE 899 (it computes
  `at_cycle` itself).
- Scheduled input is not part of a checkpoint; a checkpoint restore drops the pending schedule.
- `session/type` unchanged.
- The input journal records `at_cycle` in an entry's detail, but the scenario replay path
  ignores it: a replayed entry is applied at its issue cycle, not at `at_cycle`.
- No release (owner's call).

## 4. Acceptance

Tests in the daemon suite (and core where the model lives), run in the gate:

1. **Key at a cycle.** A PRG polls `$DC01` with column 1 driven, storing `clk`-ordered results;
   `key_down A at_cycle=X`: every read before X sees no key, the first read at/after X sees it.
   Same result whether the window is run as one `session/run` or as ten.
2. **Joystick at a cycle.** Same for `$DC00` / joystick 2 with `joystick_set {port:2, fire, at_cycle}`
   — including a schedule that falls **inside** one `session/run` call (today frozen at entry).
3. **Past cycle refused**, `at_cycle == clk` applies now, absent `at_cycle` byte-identical to today
   (existing key/joystick tests unchanged and green).
4. **#67 reproduced.** A PRG doing `lda $D011 / ora #$10 / sta $D011` after a raster-IRQ setup:
   with the triggering key at a cycle where the beam is above line 256 → IRQ fires; at a cycle
   where it is below → `$D012/$D011` IRQ line = line+256 and no IRQ. Two offsets, two outcomes.
5. **Probe series.** A PRG that changes `$D029` for 3 frames out of 100: `frame_probe
   {frames:100, line:250, addresses:[$D029@io]}` returns the change rows with those frames, and
   `rows` contains only change frames. Two runs from the same checkpoint → identical rows.
6. **Probe assert.** Same PRG, `mode:"assert", expect:[{addr:$D029, value:…}]` stops at the first
   wrong frame with its frame/cycle/actual; all-good window → `held:true`.
7. **Background work per frame.** A 10-frame probe with the checkpoint cadence at 1 adds the same
   number of ring anchors as `session/run` over the same 10 frames.
8. Refused while running; bad `line`/`cycle`/`len` → -32602.
9. Full gate green.
10. **Breakpoints and watchpoints.** An exec breakpoint inside the window stops the probe at that
    pc, in that frame, with the rows collected so far; a watchpoint likewise (`stopped:
    "watchpoint"`); `session/run` from the same checkpoint stops at the same pc and cycle; with
    nothing armed the rows are the unarmed ones.
