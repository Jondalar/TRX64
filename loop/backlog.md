# TRX64 Build Backlog — Phase 1 (behavior-identical drop-in)

Definition-of-done per item = **Oracle green on its corpus slice** (identical WS
responses + byte-identical `.c64retrace` vs the TS runtime). No fake-green: a
stubbed/skipped component is logged as `blocked`, never `done`.

Status: `todo` | `wip` | `done` | `blocked`

## Model routing (cheap-first, escalate-on-difficulty)

Driver dispatches each builder with the item's `model:` tag via the Agent `model`
override. Opus only where cycle-exact correctness diagnosis is the crux. If an item
goes RED past `max_retries` on its tagged model, the driver re-dispatches it ONCE on
`opus` before marking `blocked`. Driver + verifier run cheap (orchestrate + read the
one-line first-divergence). Driver model: `sonnet`. Verifier model: `haiku`.

## Stage 0 — serial, blocking (nothing parallelizes before CPU is green)

- [x] `oracle-harness` — **done (mechanism)** — differential rig (tools/oracle/):
      replay identical WS command-seq, diff responses + traces, first-divergence.
      - [x] WS JSON-RPC client + sessionId threading + record/compare CLI (validated)
      - [x] byte-exact .c64retrace decoder (validated: 23283 records, v2 mem frames)
      - [x] first-divergence diff engine, responses + traces (validated, fires RED)
      - [x] hermetic daemon lifecycle — TS-vs-TS self-test GREEN, deterministic
      - [~] corpus grows per-subsystem inside each builder item (ongoing, not a blocker)
- [x] `core-substrate` — **done** — `[model: sonnet]` — Machine + 64K RAM + ROM load
      (kernal/basic/chargen), cold reset (pc=$FCE2 from $FFFC). Daemon (tokio +
      tokio-tungstenite) binds --port, WS JSON-RPC 2.0, answers ping + session/create +
      session/run(stub) + session/state. GATE GREEN: compare-vs-trx64 → create matched,
      RED only on $.state-after-boot.c64Cycles (execution = cpu-6510's job). Built sonnet.
- [x] `cpu-6510` — **done** — `[model: opus]` — cycle-exact 6510 in trx64-core (generic
      over `Bus` trait): all legal + illegal opcodes, BCD, RMW dummy-writes, branch/page
      timing, addressing modes, JAM. CPU-ISOLATED gate via monitor/exec inject+run (SEI,
      flat RAM). 8 oracle gates GREEN; confirmation re-run GREEN on iso-trace-broad (1083
      records byte-identical), iso-loads-alu, iso-trace-bcd-illegal. Arch-fit verified
      (core pure/sync, Observer generic). Open: reset P-flag at boot → ADR-011 (integration).
      **Stage 1 unblocked.**

## Stage 1 — parallel (worktree-isolated, on the stable CPU clock)

- [ ] `vic-ii` — todo — `[model: opus]` — literal port. Badlines, BA-low, DMA, sprites.
      Risk sink, expect the most iterations. Diff-clean on VIC corpus slice.
- [ ] `cia` — todo — `[model: opus]` — CIA1/CIA2 timers A/B, TOD, interrupt flags
      (timing edges are subtle).
- [ ] `drive-iec` — todo — `[model: sonnet]` — drive 6510 + VIA x2 + GCR + IEC bus;
      load works. (Escalates to opus if GCR/IEC timing diverges.)

## Stage 2 — serial

- [ ] `protocol-surface` — todo — `[model: sonnet]` — all 50+ WS methods → session
      calls, binary frames, lifecycle (boot-paused, opChain, port-race, crash-log).
      Broad but shallow; diff full WS surface.
- [ ] `snapshot-vsf` — todo — `[model: sonnet]` — saveVsf/loadVsf interop + native
      fast snapshot/restore.
- [ ] `integration` — todo — `[model: opus]` — full PRG corpus, end-to-end response +
      trace parity (hard cross-subsystem divergences).

## Deferred

- [ ] `sid` — todo — `[model: sonnet]` — audio, not in cycle-trace path. Port last /
      Phase 1.5.

---
Phase 2 (warp · parallel · explore/overlay mutation-search) starts only after
Phase 1 integration is green. Tracked separately when we get there.
