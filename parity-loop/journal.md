# TRX64 Parity-Loop Journal (Phase 2)

Newest first. One entry per iteration: item, model, gate result, commit, divergences.

## 2026-06-25 — loop armed
Backlog from `docs/ts-parity-reconciliation.md` (6-class audit). 19 items across 3 tiers.
Already landed before the loop (freeze + pause/power roots):
- 434b3e3 debug/* lifecycle broadcasts + session/state runState
- 62d76dd session/state shape 1:1 (drop controlOwner)
- d58c732 debug/checkpoint_restored
- 8efeefb stream_loop honors run-state (pause/power/reset-stream)
Driver = this session. Start: Tier 1.
