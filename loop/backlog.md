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

- [x] `vic-ii` — **done** — `[model: opus]` — cycle-exact VIC-II (6569 PAL) in
      trx64-core (vic.rs), ticked per CPU master cycle via VicBus ($D000-$D3FF→VIC).
      Raster timing, badlines, sticky allow_bad_lines, BA-low cycle-stealing
      (vicii_steal_cycles), sprite DMA, 9-bit raster IRQ. KEY: the TS `vic` trace
      channel is RESERVED (no producer → empty trace, verified) — so the gate proves
      (1) byte-identical empty vic trace + (2) c64Cycles matching TS via the badline
      BA-low CPU read-stall (ADR-015/016). Bus trait gained default-no-op tick()+
      check_ba_before_read (FlatRam unaffected → all CPU gates stay GREEN). 4 VIC
      corpus gates GREEN (iso-vic-probe/-raster/-badline-irq/-sprites), 33 core tests,
      no CPU regression. Pixel draw-cycle out of scope (never reaches trace).
- [x] `cia` — **done (core)** — `[model: opus]` — cycle-exact CIA1/CIA2 (verbatim VICE
      MOS6526 port: Ciat 8192-entry transition table) in trx64-core (cia.rs). 4 gates
      GREEN: timer A one-shot, timer B + continuous TA, TOD (BCD/AM-PM/latch), ICR
      mask+summary+read-clear. 38 core tests, no CPU/VIC regression. Found+fixed ADR-018
      (trace always 0x11). Cascade deferred → cia-cascade (ADR-017).
- [x] `drive-iec` — **done** — `[model: sonnet→opus]` — 1541 drive 6502 (reuses Cpu6510
      over DriveBus) boots $EAA0 from dos1541 ROM; drive8-cpu trace byte-exact (701
      records). Sonnet got PC/regs exact; escalated to opus (ADR-006) for cycle alignment
      → ADR-020 (atomic reset+first-opcode dispatch, PAL sync_factor 66517, drive-boot-
      local). drive-boot-idle GREEN + full CPU/VIC/CIA regression GREEN, 42 core tests.
      VIA/GCR are stubs (idle boot only); deeper VIA/GCR + IEC handshaking → integration.
- [x] `cia-cascade` (DONE: cia.rs lazy alarm cascade, port of ciacore.c) — todo — `[model: opus]` — chained timers (TB counts TA underflows)
      byte-exact. Needs the VICE maincpu alarm scheduler (ta_alarm/tb_alarm + IFR
      pipeline). Divergence: iso-cia-cascade trace[43] @cycle 89 exp=2 got=3 (ADR-017).
      Best done alongside `integration` (shared alarm framework). Rebuild the scenario.

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

## Stage 2 follow-ups (tracked corner gaps)

- [x] `iec-bus` — **done** — `[model: opus]` — C64<->1541 IEC wired (iec.rs wired-AND fold + ATN-ack + push-flush catch-up). boot-trace-short FULLY byte-exact; full regression GREEN (ADR-024).
- [x] `drive-via2` (DONE: viacore.rs 1:1 VIA2 port, ADR-058) — todo — `[model: opus]` — 1541 disk-controller VIA2 computed reads (PCR/timer/handshake). driveCycles +2; diverges at drive PC $F266 LDA $1C0C after 203087 byte-exact records (ADR-025). Low priority.
- [x] `cia-cascade` (DONE: cia.rs lazy alarm cascade, port of ciacore.c) — todo — `[model: opus]` — chained timers via VICE alarm scheduler (ADR-017), now IEC-unblocked.
