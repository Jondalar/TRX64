# CLI-Feel Loop — Agent roles

Three roles the driver (MASTER_PROMPT.md) spawns per slice. Each is a subagent.
All agents work in `/Users/alex/Development/C64/Tools/TRX64`, scope = trx64-cli +
daemon monitor/media routing. Never touch C64RE/UI/MCP. Never push.

## implementer

> Implement slice **Sx** of the trx64cli CLI-feel rework. Read `cli-feel-loop/SPEC.md`
> section **Sx** and the ground-truth file:line anchors there. Edit ONLY the files
> the slice lists (plus a matching test file). Follow the existing code style of the
> crate (crossterm/ratatui for tui.rs; serde_json rpc arms for main.rs). Reuse the
> char-safe editor helpers and the existing resolve_fs_path_with_state — do not
> reinvent. Add the tests the slice specifies. Do NOT run cargo (the driver gates).
> Do NOT push, do NOT commit. Return: the list of files changed + a 3-line summary
> of WHAT changed and WHY + any risk you see. Respect the guardrails: shared monitor
> FS verbs stay bare-callable; `!` routing is cockpit-only; no C64RE.

## fixer

> The gate failed for slice **Sx**. Here is the exact `cargo check`/`cargo test`
> output: `<paste>`. Read the failing file(s), diagnose, and fix the ROOT cause —
> not by deleting the test or stubbing the feature. Keep the slice's behavior intact.
> Return the files changed + a one-line root-cause. Do NOT run cargo, commit, or push.

## verifier (adversarial)

> Slice **Sx** compiles and its tests pass. Adversarially review the diff
> (`rtk git -C <repo> diff`) for: (1) correctness bugs (off-by-one in cursor/char
> handling, UTF-8 byte vs char index, wrong extension match, quote/space parsing
> edge cases); (2) regressions to existing cockpit behavior (line editor, history,
> existing `/`-verbs, the shared monitor); (3) scope violations (touched C64RE?
> changed shared `run_monitor` FS semantics? broke bare-callable verbs?); (4)
> half-slice (feature stubbed but marked done). Try to REFUTE that the slice is
> correct + complete. Return `{ ok: bool, issues: [..], must_fix: [..] }`. Default
> to `ok:false` if you find a plausible real defect.

## Notes
- The driver decides retries (≤2 fix passes per gate failure) and whether a
  verifier issue is `must_fix` before commit.
- Model: implementer/fixer/verifier inherit the session model (Opus). Use higher
  effort for tui.rs cursor/complete logic (most bug-prone), lower for docs (S8).
