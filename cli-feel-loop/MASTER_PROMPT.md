# CLI-Feel Loop — Master Prompt

You are the **driver** of the trx64cli "bash for the emulator" build. This prompt
fires on every 15-minute loop wake. Your job: advance the slice list to DONE,
autonomously, correctly, committing each finished slice — until everything is green,
then stop and notify.

## On each wake, do exactly this

1. **Orient.** Read `cli-feel-loop/state.json` + `cli-feel-loop/SPEC.md`. Run
   `rtk git -C /Users/alex/Development/C64/Tools/TRX64 log --oneline -8`. Read the
   tail of `cli-feel-loop/journal.md`.

2. **Detect in-flight work.** Call TaskList. If the `cli-feel-implement` workflow
   (or a fix workflow you spawned) is still `running` AND git commits advanced within
   the last ~15 min → work is progressing on its own. Do NOT double-drive: append a
   one-line journal note and reschedule (ScheduleWakeup 900s). Return.

3. **If no work is in flight and tasks remain** (`status` ∈ {todo, in_progress,
   failed}): pick the next task whose `deps` are all `done`, in `S1..S9` order.
   Run the **implement→verify→commit** cycle for it (see AGENTS.md):
   - Spawn an **implementer** agent (or a small Workflow) with the slice's SPEC
     section + files + the ground-truth anchors. It edits code only in-scope.
   - Gate: `rtk cargo check -p <crate>` then `rtk cargo test -p <crate> <scoped>`.
     Red → spawn a **fixer** agent with the exact compiler/test output; retry up to
     2×. Still red → mark task `failed` with the error in `notes`, journal it, move
     to the next unblocked task (do NOT block the whole loop on one slice).
   - Green → spawn a **verifier** agent: adversarially review the diff for
     correctness + regressions + scope violations (touched C64RE? broke shared
     monitor? half-slice?). Real issue → one fix pass → re-gate. Clean → commit
     `feat(cli): <title> [CLI-FEEL Sx]` (NEVER push), mark task `done`.
   - Update `state.json` (status + notes) and append to `journal.md`
     (`Sx: <what changed> — <commit sha> — <green|failed>`).

4. **Advance as far as one wake allows.** Keep taking unblocked tasks in the same
   wake while time/tokens permit; you do not have to stop after one slice.

5. **When all S1..S8 are `done`**, run **S9** (final gate): `rtk cargo build
   --release` (updates the symlinked binary), `rtk cargo test -p trx64-cli -p
   trx64-daemon`, write `TEST-CHECKLIST.md`, final commit. Then:
   - set `loop_status: "DONE"` in state.json,
   - `PushNotification` a one-line summary (slices done, any failed/blocked, "ready
     to test"),
   - **do NOT reschedule** (end the loop).

6. **If blocked / stuck** (same task failed 2 wakes, or a slice needs a human
   decision): journal it, `PushNotification` the blocker, set that task `blocked`,
   continue with other unblocked tasks. If NOTHING is left unblocked, set
   `loop_status: "BLOCKED"`, notify, and stop.

## Rescheduling
At the end of every wake that is NOT terminal (DONE/BLOCKED), call ScheduleWakeup
with `delaySeconds: 900`, `prompt: "/loop cli-feel — drive cli-feel-loop/MASTER_PROMPT.md"`,
reason = the next task id + one-liner.

## Hard rules (repeat of SPEC guardrails)
- **Never push.** Commit only. Alex pushes.
- **Never touch** `../C64ReverseEngineeringMCP`, any UI, or MCP server code.
- Shared `run_monitor` FS verbs must stay bare-callable (C64RE uses them). `!` is a
  **cockpit routing** layer in `engine.rs`/`tui.rs`, not a monitor change.
- Each committed slice must **compile + pass its tests + pass adversarial review**.
  No half-slices, no stubbed-out "TODO later" in a slice marked done.
- Caveman voice in chat/journal is fine; **commit messages + code + docs = normal
  English**.
