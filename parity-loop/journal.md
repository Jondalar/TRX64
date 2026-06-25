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

## 2026-06-25 — Tier-1+2 workflow (wj3ssxqec) COMPLETE: 13/13 done
T1.4 drive_power-shape, T1.5 create.attached(+integrate-fix→true, 2 stale goldens re-recorded),
T1.3 set_pacing, T1.2 debug/control-owner, T2.1 runtime/call-bridge, T2.5 media/cart_persisted,
T2.2 observer_log-drain, T2.6 trace/run/start|mark+current, T2.7 memory_access_map,
T2.4 cart write-LED, T1.6 warm_reset(soft-reset), T2.3 gcr sector-under-head, T1.7 joystick-model.
Integrate-gate caught T1.5 regression (attached false-on-first) → fixed to shared-attach true,
re-recorded boot-basic-ready + iso-cia-icr goldens. Full no-disk oracle GREEN 24/0. core 216/216.
Remaining: T2.8 monitor-command-parity + Tier-3 deep ports.
