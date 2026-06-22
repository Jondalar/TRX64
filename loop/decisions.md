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

## ADR-012 — Stage-1 chips are gated in ISOLATION, not via the full machine bus
**Context:** vic-ii / cia / drive-iec all need to sit on a memory bus + be ticked per CPU
cycle. If each required the full PLA/banked machine bus + cross-chip IRQ wiring, the three
builders couldn't run in parallel and would collide on shared substrate.
**Decision:** Each Stage-1 chip is built in `trx64-core` and gated in ISOLATION (like the
CPU, ADR-005): a chip-specific `Bus` impl that routes ONLY that chip's I/O range (VIC
$D000-$D3FF, CIA1/2 $DC00-$DDFF) + flat RAM elsewhere; the chip ticked per CPU cycle; a
CPU-isolated exerciser (SEI) that programs/reads it; verified by trace-diff on the chip's
own domain (vic / cia / drive-cpu). VIC/CIA are clock-driven so a minimal CPU loop
suffices. The drive is gated on its own drive-cpu domain. Do NOT build the full machine
bus, PLA banking, or cross-chip IRQ in Stage 1 — that is the `integration` item (Stage 2).
**Why:** Keeps the three builders parallel + independent; composition via `Bus` (ADR-010)
happens once, later, with all chips present.

## ADR-013 — Worktree builders point the oracle at their OWN binary
**Context:** The oracle's `daemon.ts` spawns `TRX64_DAEMON_BIN` (defaults to the MAIN
repo's `target/debug/trx64-daemon`). A builder in a git worktree that runs the oracle
would otherwise test the MAIN binary, not its own changes.
**Decision:** Every worktree-isolated builder MUST export
`TRX64_DAEMON_BIN=<its-worktree>/target/debug/trx64-daemon` before running the oracle.
**Why:** The gate must verify the builder's own work, not stale main.

## ADR-014 — Stage 1 runs SERIALLY (one chip per iteration), not parallel worktrees
**Context:** The Agent tool's worktree isolation is unavailable here — the session was
"not a git repository" at startup (git init happened after), and no WorktreeCreate hooks
are configured. Also the three chips share thin plumbing (trx64-trace frame writers,
daemon trace-domain wiring, core Machine/Bus glue).
**Decision:** Build Stage 1 SERIALLY — one chip per iteration (vic-ii, then cia, then
drive-iec), each on a per-chip branch in the main working dir (ADR-009 preserved),
gated + merged to main before the next starts. This SUPERSEDES the parallel-worktree
execution of ADR-012/ADR-013 (ADR-013's per-worktree TRX64_DAEMON_BIN is now moot — the
oracle uses the main binary, which IS the builder's). The isolation-GATE principle of
ADR-012 (each chip verified against its own domain via a chip-specific Bus) STILL HOLDS.
**Why:** No worktree support; serial avoids merge conflicts on shared plumbing and is
more robust for unattended overnight operation; fits one-iteration-per-tick. Revisit
parallel if WorktreeCreate hooks get configured.

## ADR-015 — The VIC trace channel is RESERVED: the gate proves the empty trace + cycle coupling
**Context:** The vic-ii builder was tasked to verify a cycle-exact VIC-II "byte-for-byte
on the VIC trace domain." Investigation of the TS oracle (the immovable spec) found the
`vic` trace channel has SCHEMA + encoder + decoder + kind-codes (VIC_REG_WRITE 0x20,
{raster:1,mode:2,irq:3,badline:4}) but NO LIVE PRODUCER: nothing calls `publish("vic",…)`
(tickLitVic advances raster/framebuffer but emits no trace event). Verified empirically —
a vic-domain `.c64retrace` over a full PAL frame with VIC-register writes yields ZERO
records (binary-format.ts §"RESERVED … never emitted").
**Decision:** The VIC parity gate is therefore TWO facts, both now GREEN: (1) the vic-domain
trace is byte-identically EMPTY, and (2) `session/run` `c64Cycles` matches the TS daemon —
which requires the cycle-exact VIC↔CPU coupling (badline + sprite-DMA BA-low STEALS read
cycles via `vicii_steal_cycles`), since that shifts CPU instruction timing. The VIC core
(raster/badline/BA/sprite-DMA) is built cycle-exact in trx64-core and ticked once per CPU
master cycle; the daemon honors the domain→channel filter (= TS `domainsToChannels`) so a
vic-only domain enables only the producer-less `vic` channel (empty), while cpu/memory
domains are unaffected. A VIC_REG_WRITE encoder + `Observer::on_vic_reg` hook exist for
binary-format completeness + future integration, but are NEVER emitted into a parity trace.
Pixel draw-cycle / framebuffer is OUT of scope (it never reaches the trace).
**Why:** The oracle is ground truth (ADR-004). Matching it byte-for-byte means reproducing
its reserved-channel reality, not inventing VIC trace records the spec never emits. The
real, verifiable VIC correctness surface is the cycle coupling (badline cycle-stealing),
which the gate DOES exercise via `c64Cycles`.

## ADR-016 — The `Bus` trait gains per-cycle `tick()` + `check_ba_before_read()` (VIC coupling)
**Context:** Cycle-exact VIC↔CPU coupling needs the VIC ticked once per CPU master cycle
and the CPU read-stalled while VIC BA is low (badline/sprite DMA) — VICE
cpu65xx-vice.ts `tick()`→c64ViciiCycle and `load()`→checkBaBeforeRead/vicii_steal_cycles.
**Decision:** Extend the `Bus` trait (ADR-010) with two DEFAULT-NO-OP hooks: `tick(&mut)`
(called from `Cpu6510::tick` for every master cycle) and `check_ba_before_read(&mut)->u32`
(called from `Cpu6510::load` before every read, returns stolen-cycle count). FlatRam keeps
the defaults, so the CPU-isolated gate is byte-identical (all CPU gates stay GREEN). The
`VicBus` ($D000-$D3FF→VIC, flat RAM elsewhere) overrides both: tick advances the VIC +
latches BA-low; check_ba runs the `do{clk++; ba=vicii_cycle()}while(ba)` steal. The 6510
microcode/correctness is untouched — only the clock plumbing is threaded.
**Why:** Keeps the CPU generic + isolated (ADR-005/010) while enabling exact chip coupling
through the same `Bus` seam composition will use later. Same pattern will serve CIA.
