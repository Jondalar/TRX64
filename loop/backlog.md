# TRX64 Build Backlog — Phase 1 (behavior-identical drop-in)

Definition-of-done per item = **Oracle green on its corpus slice** (identical WS
responses + byte-identical `.c64retrace` vs the TS runtime). No fake-green: a
stubbed/skipped component is logged as `blocked`, never `done`.

Status: `todo` | `wip` | `done` | `blocked`

## Stage 0 — serial, blocking (nothing parallelizes before CPU is green)

- [ ] `oracle-harness` — **wip** — differential rig (see tools/oracle/README.md):
      replay identical WS command-seq against TS-daemon + TRX64, diff responses +
      traces, emit first-divergence. Curate PRG corpus. THE prerequisite.
- [ ] `core-substrate` — todo — Machine struct, bus, PLA/$00-$01 mapping, ROM load,
      daemon shell binds 4312 and answers `ping` identically.
- [ ] `cpu-6510` — todo — microcode, all legal + illegal opcodes, IRQ/NMI timing.
      Gate: diff-clean on CPU-only corpus. **Unblocks Stage 1.**

## Stage 1 — parallel (worktree-isolated, on the stable CPU clock)

- [ ] `vic-ii` — todo — literal port. Badlines, BA-low, DMA, sprites. Risk sink,
      expect the most iterations. Diff-clean on VIC corpus slice.
- [ ] `cia` — todo — CIA1/CIA2 timers A/B, TOD, interrupt flags.
- [ ] `drive-iec` — todo — drive 6510 + VIA x2 + GCR + IEC bus; load works.

## Stage 2 — serial

- [ ] `protocol-surface` — todo — all 50+ WS methods → session calls, binary frames,
      lifecycle (boot-paused, opChain, port-race, crash-log). Diff full WS surface.
- [ ] `snapshot-vsf` — todo — saveVsf/loadVsf interop + native fast snapshot/restore.
- [ ] `integration` — todo — full PRG corpus, end-to-end response + trace parity.

## Deferred

- [ ] `sid` — todo — audio, not in cycle-trace path. Port last / Phase 1.5.

---
Phase 2 (warp · parallel · explore/overlay mutation-search) starts only after
Phase 1 integration is green. Tracked separately when we get there.
