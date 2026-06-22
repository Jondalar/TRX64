# TRX64 Architecture Decisions (ADR log)

The Driver OWNS architecture. This file is the durable memory of every standing design
decision — the Driver reads it every tick and appends to it. Builders inherit these as
hard constraints (the Driver passes the relevant ones into each builder prompt).

Format: `## ADR-NNN — <title>` · **Context** · **Decision** · **Why**. Newest at bottom.
Supersede rather than delete: mark an old ADR `SUPERSEDED by ADR-NNN` and add the new one.

---

## ADR-001 — Swap the process behind ws://4312, not "a core"
**Context:** The C64RE runtime is already a separate daemon (WS JSON-RPC 2.0 on 4312);
UI + MCP both talk to it.
**Decision:** TRX64 replaces that process. The WS JSON-RPC contract, the `.c64retrace`
binary format, the UI, and the existing MCP tools are IMMOVABLE — never change them.
**Why:** The value is the validated contract + tooling; we only swap the implementation.

## ADR-002 — Crate boundaries (separation of concerns = the performance)
**Context:** A naive port mixes emulation, trace, async, and protocol.
**Decision:** `trx64-core` = pure, sync, deterministic emulation + a generic zero-cost
`Observer` (monomorphized, never a per-event cross-process callback); `Machine` is
`Clone` for Phase-2 COW forks. `trx64-trace` = TraceOp encoder. `trx64-session` =
lifecycle (+ Phase-2 `explore()`). `trx64-daemon` = the ONLY async/WS/JSON-RPC layer.
**Why:** Keeps the hot path monomorphized + branch-free; isolates the crown jewel (core)
for diff-testing.

## ADR-003 — Trace data-plane: file-only forensic; probes for search
**Context:** Trace capture is the hottest write path (~985k events/s).
**Decision:** Forensic trace is post-hoc file (`.c64retrace`), same model as TS — no
shared memory. Phase-2 mutation-search uses in-core PROBES (compact verdicts), not the
firehose. The Observer hook has three faces (NullSink / FrameSink / ProbeSet), one
mechanism.
**Why:** No per-event boundary crossing; "live" = the LLM, served by incremental
result-push over the existing WS, so no shm is ever needed.

## ADR-004 — Verification = deterministic oracle, not an LLM verifier
**Context:** Both daemons speak the same WS protocol + emit the same trace format.
**Decision:** The gate is `tools/oracle`: hermetic fresh-daemon-per-run, replay
identical WS command-seq, diff WS responses + `.c64retrace`, emit first-divergence. No
LLM judges correctness.
**Why:** Deterministic ground truth is cheaper + more reliable than LLM judgment;
gives every builder a mechanical green/red.

## ADR-005 — The CPU gate must be CPU-isolated
**Context:** A full KERNAL boot couples to CIA/VIC (IRQs change the execution path).
**Decision:** Verify the CPU with interrupts disabled (SEI/I-flag), no VIC/CIA-dependent
reads, deterministic exercisers — NOT by booting to a fixed cycle count.
**Why:** Otherwise CPU correctness can't be isolated from chip timing not yet ported.

## ADR-006 — Model routing: cheap-first, escalate-on-difficulty
**Decision:** Per-item `[model:]` tags; opus only where cycle-exact correctness is the
crux (cpu/vic/cia/integration), sonnet for substrate/protocol/drive/snapshot/sid. RED
past max_retries → re-dispatch the same item once on opus, then escalate.
**Why:** Token discipline for overnight autonomy without sacrificing the hard items.

## ADR-007 — Phases hard-separated
**Decision:** Phase 1 = behavior-identical drop-in (trace-diff green). Phase 2 (warp /
parallel / `explore` / overlay) is ADDITIVE new tools, started only after Phase-1
integration is green. Existing tools stay frozen.
**Why:** Keeps the parity oracle meaningful; avoids building on a moving target.

## ADR-008 — Loop is context-stateless / disk-stateful
**Decision:** All loop state lives on disk (state.json, backlog.md, journal.md, THIS
file) + git. Each tick assumes fresh context. Being killed mid-work is normal.
**Why:** Survives token resets and session restarts; resume = read disk.

## ADR-009 — Builders work on a per-item branch; Driver merges after gate
**Context:** The cpu-6510 builder committed on a `cpu-6510` branch (8 commits) rather
than main.
**Decision:** Adopt this for ALL items (not just Stage-1 worktrees): each builder works
on a per-item branch `<item>`; the Driver runs the confirmation gate + architecture-fit
check, then fast-forward/merges to `main` and deletes the branch. main stays green.
**Why:** Atomic accept; the gate guards what reaches main; clean Stage-1 parallelism.

## ADR-010 — The CPU is generic over a `Bus` trait
**Context:** The cpu-6510 builder made the 6510 generic over a `Bus` trait (it ran on a
flat-64K-RAM bus for the CPU-isolated gate).
**Decision:** Bless it. The 6510 stays decoupled from the memory map via `Bus`. Flat RAM
now; the full PLA/banked/$00-$01 + I/O bus is supplied later (core-substrate growth +
vic/cia integration) by implementing `Bus`, without touching the CPU.
**Why:** Clean separation; lets the CPU be gate-isolated now and composed into the full
machine later; supports Phase-2 COW forks (the bus is swappable per instance).

## ADR-011 — Reset P-flag divergence is deferred to integration (open)
**Context:** cpu-6510 isolated cold-reset sets I (P=$24); the full-KERNAL-boot trace
shows P=$20 at the first traced instruction (`boot-trace-short` trace[0].p 32 vs 36).
**Decision:** NOT a CPU defect (isolated gates are byte-exact). Leave it open; resolve
when the boot path + CIA/VIC are assembled (the `integration` item), where the real
reset-sequence + first-instruction-trace timing is reproduced.
**Why:** Can't reconcile boot-reset timing before the chips that drive it exist.
