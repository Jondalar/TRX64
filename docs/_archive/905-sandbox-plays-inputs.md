# Spec 905 (TRX64 half) — A sandbox run plays inputs, and nothing patches the shared machine any more

**Status:** DONE — merged to main 2026-10-11 (TRX64 half; the C64RE half is C64RE 905). Not released yet: removes RPCs, so it ships as a minor (0.13.0). Tests: `spec905_*` in the daemon suite. Decision in `README.md` here.
**Repo:** TRX64 (daemon + core scenario player). The C64RE half — anchors, the candidate as
project memory, the parallel loop, `verify_build`, the workbench — is
`C64ReverseEngineeringMCP/specs/905-from-an-anchor-to-the-build.md`.
**Number:** 905 (registry: `C64ReverseEngineeringMCP/specs/README.md`).
**Origin:** owner, 2026-10-11, via the C64RE session: *"Ich definiere einen Ankerpunkt im
Ring/Stream. Ab hier kann ich verschiedene Pfade gehen — in Kombination mit Code-Overlay schnell
verschiedene Ansätze parallel in Sandboxes iterieren — am Ende die Lösung in den Code überführen
und damit in den Build."* Delegated here the same day ("delegier den Teil dahin. Dann baut bitte").
**Depends on:** 809 (marks, `sandbox/run(Many)`, COW media folder — built), 796 (candidates —
removed here), 901 (input at a cycle — released 0.12.10).

---

## §1 The problem

**Who:** the owner at the workbench and the LLM in a session, on one shared machine (Doctrine
rule 2: the shared machine is never moved for a test).

**What they want:** from a named anchor, run several patched variants **with the same inputs**
for the same time, each on a machine of its own, and compare the ends — while the shared machine
stays where it is.

**What happens today** (read on main `41f11b3`):

- `sandbox/run` / `sandbox/runMany` (`main.rs` ~12154) clone the machine per run from a mark or
  an anchor and take `patches` + `cycles` only. `sandbox_one` (~16426) applies the patches and runs
  the budget. **There is no way to give a run inputs** — a variant that needs a key press or a
  joystick move cannot be tried in a sandbox at all.
- The scenario player's only target is `MachineScenarioTarget { session: &mut Session }`
  (~14894): it can only drive the live session.
- So the one place inputs + patches + anchor come together is the candidate path:
  `runtime/candidate_*` (~8881–9010) → `run_candidate` (~17464). It **restores the anchor into the
  live session**, applies the patches there, plays the scenario there and leaves the shared
  machine in the patched end state. One candidate at a time. Exactly what rule 2 forbids.

Worked example: an owner at frame 4123 wants to know whether `lda #$01` instead of `lda #$00` at
`$C10A` stops the sprite glitch two seconds after pressing fire. Today: `candidate_run` — and the
machine they were watching is now two seconds later, patched. After: one `sandbox/runMany` with
two runs (no patch / patch), both with `inputs: [{atCycle: 1000, kind: "joystick2", payload:
{fire: true}}]`, `cycles: 100000`; the live machine is byte-identical before and after.

## §2 Scenarios — what proves it is solved

```gherkin
Scenario: a sandbox run plays a joystick press at its cycle
  Given an anchor A on a machine whose program reads $DC00 every few cycles and logs it to RAM
  When  sandbox/runMany runs [{from A, inputs: [joystick2 fire at N], cycles: C},
                              {from A, inputs: [], cycles: C}]
  Then  the two runs' logs are identical before cycle A+N and differ from the first read at or after it
  And   the no-input run equals a plain sandbox run from A without `inputs`
  And   the live session's cycles, registers and run state are byte-identical before and after

Scenario: inputs mean the same live and in a sandbox
  Given the same anchor and input list
  When  played once by the scenario player on the live session and once in a sandbox run
  Then  both end in the same RAM digest and registers

Scenario: a keyboard input and a model switch in a sandbox
  Given an anchor at the BASIC prompt
  When  a sandbox run plays inputs [keyboard "PRINT 1\r" at 0]
  Then  the run's screen RAM shows the result; the live machine shows nothing typed

Scenario: the candidate RPCs are gone
  When  any runtime/candidate_* is called
  Then  error -32601 (or the daemon's "method not found" code) naming the method and saying
        "removed — use sandbox/runMany with patches + inputs"
  And   the shared machine did not move
```

| # | Scenario | Test | Where it runs |
|---|---|---|---|
| S1 | joystick press at its cycle, baseline equal, live untouched | `spec905_sandbox_inputs_*` daemon test | `scripts/gate.sh` (daemon suite) |
| S2 | live and sandbox mean the same | `spec905_scenario_target_live_equals_sandbox` | gate |
| S3 | keyboard input in a sandbox (needs ROMs → skips loudly without them, like the other ROM tests) | `spec905_sandbox_keyboard_input` | gate |
| S4 | candidate RPCs refused by name | `spec905_candidate_rpcs_refused` | gate |

## §3 Decision

- **D1 — a scenario target over a bare `Machine`.** New `MachineTarget<'a> { machine: &'a mut
  Machine, full_machine: bool, … }` implementing `ScenarioTarget`: `type_text` → keyboard queue,
  `set_joystick1/2` → the machine's sticks, `run_for` → `run_machine_budget(machine, cycles,
  full_machine)` (the sandbox's own runner). `MachineScenarioTarget` (session) delegates every input
  method to it and keeps only what needs the session: `run_for` through `run_cycle_budget` (traces,
  observers) and `switch_model` (frame-boundary switch). One meaning for an input, two hosts.
- **D2 — `inputs` on each run.** `sandbox/run` and every `runs[i]` of `sandbox/runMany` take
  `inputs` — the existing scenario input list parsed by `scenario_steps_from_inputs`:
  `[{atCycle, kind: "keyboard"|"joystick1"|"joystick2"|"model", payload}]`, `atCycle` relative to
  the run's start (the anchor). `sandbox_one` applies patches, then plays the inputs with the
  scenario player over D1's target **within** the run's `cycles` budget (an input at or past
  `cycles` is not reached and is reported in `inputsSkipped`), then runs out the rest of the budget.
  The summary adds `inputsPlayed` (count). Inputs land at their cycle with 901's semantics (a CIA1
  read at or after the cycle sees them). Absent `inputs` → byte-identical to today.
- **D3 — `model` in a sandbox**: supported if the bare-machine switch is cheap to share (the
  live switch's frame-boundary logic moved to a `Machine`-level helper the session calls);
  otherwise refused per run by name ("a model switch is not playable in a sandbox run") — the
  builder decides by reading `switch_at_frame_boundary`, and records it in §5.
- **D4 — disk.** There is no disk-swap input kind today; what a run writes to its disk already
  goes to the clone's copy-on-write folder (809). Nothing to change; S1–S3 run with the anchor's
  media.
- **D5 — the candidate RPCs go.** Remove `runtime/candidate_create|patch|run|remove_patch|list|
  delete|export`, `run_candidate`, `apply_candidate_patch` if unused afterwards, `State.candidates`
  / `candidate_seq`, and `candidate.rs` if nothing else uses it. A call is refused by name (S4).
  Anything that points at them — monitor verbs and help, `runtime/find_cheat_candidates`' text,
  docs (`FEATURES.md`, `wl-trx64-play-api.md`, README), the method listings — is brought in line.
  Removing RPCs is breaking → the next TRX64 release is a **minor** (0.13.0), cut only when the
  C64RE half is green and the owner says so.

## §4 Out

- Paddle and RESTORE inputs: the scenario player has them as no-ops today; unchanged.
- A disk-swap input kind (none exists).
- Persisting candidates: that is the C64RE half (project memory).
- Sandboxes on the C64 Ultimate bridge (889's question).
- The release itself.

## §5 Decisions taken while building

- **`inputs` form:** `inputs: [{atCycle, kind: "keyboard"|"joystick1"|"joystick2"|"model", payload}]`
  on `sandbox/run` and on each `runs[i]` of `sandbox/runMany`. `atCycle` is relative to the
  run's start (absent = 0). An input with `atCycle >= cycles` is not played and is returned
  in `inputsSkipped`. The summary gains `inputsPlayed` and `inputsSkipped` whenever the run
  carried an `inputs` array (absent key → today's summary, unchanged), and `modelSwitches` +
  `model` when a `model` input switched. Unknown kinds are dropped by the shared parser and so
  are not counted in `inputsPlayed`.
- **One loop:** the replay loop of `run_scenario` became `play_scenario`, generic over the
  target; the live scenario and the sandbox both run through it. `run_scenario`'s `atCycle`
  stays the machine's absolute clock (unchanged); the sandbox shifts the relative cycles by
  the clone's clock before handing them to the player.
- **D3 — a model switch IS supported in a sandbox.** Reading `switch_at_frame_boundary`: only
  the "run one cycle until the raster wraps" loop touched the session (`run_cycle_budget`);
  the transplant itself is `Machine::switch_model` plus a kept-state report. The loop is now
  `advance_to_boundary_by` (shared) and the transplant `transplant_machine`;
  `switch_at_frame_boundary` (session, also sets `session.model`) and
  `switch_machine_at_frame_boundary` (bare machine, one cycle via `run_machine_budget`) are
  thin wrappers. Test: `spec905_sandbox_model_switch`.
- **Joystick input syncs the machine first** (`Machine::sync_input`, as `session/joystick_set`
  does), on the live scenario target too, so a change queued earlier cannot land over a
  scenario input. No effect when nothing is queued.
- **Removed:** `runtime/candidate_*` (all seven), `run_candidate`, `apply_candidate_patch`,
  `State.candidates`/`candidate_seq`, `candidate.rs`, the 796 test. A call answers `-32601`
  `Method not found: <method> — removed — use sandbox/runMany with patches + inputs`.
  `runtime/find_cheat_candidates` never depended on candidates (only its comment did; fixed).
- **S1/S2 need no Commodore ROM:** they boot a full machine on synthetic ROM files (a KERNAL
  that is a $DC00 logging loop) written to a temp dir; S3 and the model-switch test use the real
  ROMs and skip loudly without them.
- Docs brought in line: `FEATURES.md`, `concepts-snapshots-scenarios-overlays.md`,
  `864-the-monitor-as-a-library.md`. README names no candidates.
- A C64RE regression report (899 sweep: glitch at frame 42 vs 43) did not reproduce on the
  finished build (52 pass / 0 fail); `spec905_frame_numbers_are_pinned_around_sandbox_inputs`
  pins probe frames and cycles around sandbox runs with inputs.
