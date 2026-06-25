# TRX64 ↔ c64re TS Parity Backlog — Phase 2 (100% WS-surface parity)

Source matrix: `docs/ts-parity-reconciliation.md`. Authority = the TS runtime
(`../C64ReverseEngineeringMCP/src/workspace-ui/ws-server.ts` + `runtime-controller.ts`).
**TRX64 adapts to TS, never the reverse. Never touch the c64re UI/MCP.**

Definition-of-done per item = (1) `cargo build` clean, (2) the item's behavior matches
TS (per-item gate below), (3) NO oracle regression (no-disk subset stays GREEN), (4)
1:1 with the TS handler. **No fake-green** — a stub/no-op is `blocked`, never `done`.
Cite the fix in the commit. Status: `todo | wip | done | blocked`.

Model routing: quick/mechanical = `sonnet`/`haiku`; real core ports (cycle/banking/
chip behavior) = `opus`. Driver gates + commits; builders port one item.

## Tier 1 — live-UI bugs (user-reported) + quick wins

- [ ] T1.1 `audio-streaming-flag` [sonnet] — session/state `sid.streaming` reflect the
      live A/V hub (StreamHub::has_subscribers via a State flag), not hardcoded false.
      Gate: oracle boot GREEN; UI SID light ON while running, OFF on pause.
- [ ] T1.2 `debug-control-owner` [sonnet] — read `source` on debug/run|pause|continue|
      step, track control_owner in State, broadcast `debug/control {owner}` on change
      (runtime-controller.ts:338). Gate: notif-diff shows debug/control on llm/human switch.
- [ ] T1.3 `session-set-pacing` [sonnet] — add handler (modes pal|warp|fixed-ratio) →
      returns controller state (ws-server.ts:1378). Gate: method present, returns state.
- [ ] T1.4 `drive-power-shape` [haiku] — omit `mode` key on fallback path to match TS
      (ws-server.ts:1626). Gate: oracle/shape.
- [ ] T1.5 `session-create-attached` [haiku] — reflect real attach state, not hardcoded
      true. Gate: oracle.
- [ ] T1.6 `reset-warm` [opus] — port `resetWarm()` into trx64-core (re-init CPU+chips+
      drive + restore $01 banking so $FFFC→$FCE2 runs clean, recovers from JAM); session/
      reset "soft" uses it (ws-server.ts:1404). Gate: soft-reset recovers a JAMmed machine;
      oracle GREEN.
- [ ] T1.7 `joystick-model` [opus] — port the joystick model into trx64-core (CIA1 port1/2
      read, active-low, overlaps keyboard matrix); wire session/joystick_set, joystick_clear,
      release_keys (joy part), input_status (joystick1/2 live). Gate: joystick_set drives a
      game/test; input_status reflects held bits.

## Tier 2 — UI tabs / observability

- [ ] T2.1 `runtime-call-bridge` [sonnet] — add `runtime/call {op,args}` → AgentQueryApi
      op allowlist (ws-server.ts:1717). UI Snapshots/Scenarios/Trace tabs depend on it.
      Gate: representative ops dispatch + return TS-shaped results.
- [ ] T2.2 `observer-log-drain` [sonnet] — drain pending_log/marks/cmds after run_until_break
      + in debug/step; broadcast `debug/observer_log` (runtime-controller.ts:698). Infra
      already in observers.rs. Gate: an observer `do log` emits observer_log.
- [ ] T2.3 `drive-status-sector` [opus] — GCR sector-under-head decoder (vs hardcoded 0).
      Gate: sector matches VICE/TS under a known head position.
- [ ] T2.4 `cart-write-led` [sonnet] — port writableGeneration write-pulse counter →
      cart_status activity "write" (1.2s hold, BUG-042). Gate: a flash write shows "write".
- [ ] T2.5 `media-cart-persisted-broadcast` [sonnet] — broadcast `media/cart_persisted`
      on auto-persist (runtime-controller.ts:513). Gate: notif on persist.
- [ ] T2.6 `trace-run-start-mark-current` [sonnet] — add trace/run/start (definition_id),
      trace/run/mark (marks vec), trace/current (last store). Gate: methods work, 1:1 shape.
- [ ] T2.7 `memory-access-map` [sonnet] — implement debug/memory_access_map (vs stub).
      Gate: returns liveness map for a cycle budget.

## Tier 3 — deep ports (high-effort)

- [ ] T3.1 `vic-inspect-engine` [opus] — port vic-inspect (at/region/provenance, Spec 710/721).
- [ ] T3.2 `time-travel` [opus] — runtime/snapshot_tree, promote_branch, overlay_run (Spec 769).
- [ ] T3.3 `media-ingress-full` [opus] — checkpoint-before/after + dirty-guard + pause/resume
      + CRT path (currently -32601) (Spec 709.13).
- [ ] T3.4 `run-prg-autostart` [sonnet] — BASIC RUN autostart in runtime/run_prg.
- [ ] T3.5 `trace-read-duckdb` [opus] — trace/read 7 query ops (DuckDB). May stay deferred.
- [ ] T3.6 `scenario-save-file` [haiku] — disk-backed scenario_save (+ filePath).

## Sequencing
Tier 1 first (T1.4/T1.5/T1.1/T1.3/T1.2 quick → T1.6/T1.7 ports), then Tier 2, then Tier 3.
input_status (T1.7) before nothing else depends. Serial: most items touch main.rs →
one builder at a time (no worktree parallelism). Commit per item.
