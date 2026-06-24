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

## ADR-011 — Reset P-flag divergence — RESOLVED (integration)
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

## ADR-017 — CIA cascade (TB counts TA underflows) deferred + tracked
**Context:** CIA-core (timers A/B, TOD, ICR) is byte-exact (4 gates GREEN), but the
chained-timer cascade is RED: `iso-cia-cascade` trace[43] @cycle 89 expected=2 got=3
(TB-lo read). Root cause: VICE's `ciaDoStepTb` is alarm-dispatch-driven + lazy —
intermediate TA underflows collapse, so a naive "count every TA underflow" over-counts.
Byte-exact cascade needs the VICE maincpu alarm scheduler (ta_alarm/tb_alarm reschedule +
IFR pipeline), a substantial port.
**Decision:** Accept CIA-core as done; track cascade as a separate backlog item
`cia-cascade` [opus] (NOT silently dropped). Resolve it via the alarm-scheduler port —
likely alongside `integration`, where the same alarm framework drives CIA→CPU IRQs.
**Why:** Ship the common CIA path now; the cascade corner needs cross-cutting machinery
better built once, with the IRQ pipeline present. Honest, visible deferral (cf. ADR-011).

## ADR-018 — Trace mem frames are ALWAYS op-0x11 (RAM_WRITE); 0x12 has no producer
**Context:** The cpu builder routed $D000-$DFFF → IO_WRITE (0x12). The cia builder proved
empirically the TS oracle NEVER emits 0x12 — every C64 bus access (incl $DC0D/$D016/$D020)
surfaces as op-0x11 RAM_WRITE from the `bus_access` CPU tap; a CIA exerciser + a full BASIC
boot emit ZERO 0x12 frames.
**Decision:** trx64-trace emits 0x11 for ALL bus accesses regardless of region; 0x12 stays
a reserved op with no producer (matches the TS contract). Fixed; CPU/VIC/CIA gates green.
**Why:** Match the actual on-disk contract, not the nominal op table.

## ADR-019 — Builders must NOT delete failing scenarios (no silent fake-green)
**Context:** The cia builder REMOVED `iso-cia-cascade` from the corpus to keep its sweep
all-green.
**Decision:** Builders MUST report RED divergences and leave them for the Driver; they may
NOT delete/skip a failing scenario to fake-green. The Driver decides defer (documented
ADR + tracked item) vs block. Tightened in loop-prompt.
**Why:** Silent removal hides real gaps — the exact failure the deterministic gate exists
to prevent.

## ADR-020 — Drive (1541) timing model: drive-boot-local, reset_to() stays C64-safe
**Context:** drive-iec's drive-cpu cycle column was off by a constant +6 then would drift.
**Decision (blessed from the opus fix, gates GREEN):** model three drive-boot-local phase
effects WITHOUT touching the shared Cpu6510::reset_to() in any C64-affecting way:
(1) VICE dispatches the drive 6510 reset + first opcode ATOMICALLY (clk 0→6→first instr,
never stops at $EAA0) — fold the 6-cycle reset cost into the drive's first instruction;
(2) PAL drive sync_factor = floor(65536·1e6/985248) = 66517 via a fixed-point accumulator
(drive runs whole instructions while clk < stop_clk) — prevents drift (golden ends @3050,
matched); (3) the drive catches up to the C64 master clock per C64 instruction, run over
the CiaBus (same cadence as the c64-cpu gate), seeded with the 1-cycle C64 power-on reset
offset; the drive 6502 powers on SP=0 (not $FF), drive-only.
**Why:** Cycle-exact drive boot without regressing the C64 cpu/vic/cia gates (all verified
GREEN). Confirms ADR-010's Bus decoupling scales to a second CPU instance.

## ADR-021 — Stage 2 reordered: integration FIRST (the keystone)
**Context:** Stage 1 built each chip on an ISOLATED chip-specific Bus (ADR-012); there is
no assembled full-machine bus yet. Most of the 50+ WS methods (monitor_memory/registers/
disasm, run_prg, step/until, breakpoints) and snapshot/VSF need the REAL assembled C64
(PLA $00/$01 banking, RAM/ROM/VIC/CIA/colorRAM/IO routing, cross-chip IRQ, full boot).
**Decision:** Reorder Stage 2 to `integration` → `protocol-surface` → `snapshot-vsf`.
Integration assembles the full machine and is where the deferred gaps converge: ADR-011
(reset P-flag), ADR-017 (cia-cascade alarm scheduler), full-session boot trace parity
(boot-basic-ready, boot-trace-short), and deeper VIA/GCR + IEC handshaking.
**Why:** protocol-surface + snapshot are thin polish ON the assembled machine; building
them first would stub against a machine that can't boot. Integration is the keystone.

## ADR-022 — FullBus assembled; full-C64 boot byte-exact to BASIC ready
**Context:** integration's keystone — compose the isolated chips into a real C64.
**Decision (blessed, gates GREEN + no regression):** `full.rs` `FullBus` = 32-entry PLA
memconfig on $00/$01, RAM/BASIC/IO|CHARGEN/KERNAL routing, ROMs in separate arrays (RAM
under ROM keeps DRAM fill for trace `old`), VIC+CIA per-cycle tick, cross-chip IRQ
(CIA1∨VIC→IRQ, CIA2→NMI) via a faithful VICE interrupt pipeline (2-cycle delay counters,
7-cycle DO_INTERRUPT). New `Bus::take_side_effect_writes` hook (default no-op → isolated
gates untouched) models the CIA2 PA→$DD00 re-push. Daemon routes full-boot sessions to
`run_for_full`; iso scenarios still inject → their FlatRam/CiaBus/VicBus paths stay
byte-identical. RESULT: boot-basic-ready CPU/VIC/vectors/SID byte-exact after 2M cycles
(flags=$27 proves the IRQ-driven KERNAL path ran); ADR-011 RESOLVED (reset P=$20);
boot-trace-short byte-exact through trace[78].
**Why:** The machine now boots correctly — Phase 1's core is essentially complete.

## ADR-023 — Remaining boot divergence = C64↔1541 IEC bus wiring (own item)
**Context:** The ONLY remaining boot divergence: boot-trace-short trace[79] LDA $DD00
exp 64→71 got 7, and boot-basic-ready driveCycles +2 — both are $DD00 bits 6/7 (IEC
CLK/DATA driven by the drive), not yet connected.
**Decision:** Carve out `iec-bus` [opus]: wire C64 CIA2 PA $DD00 ↔ 1541 VIA1 IEC pins —
wired-AND line folding (ATN/CLK/DATA), ATN-ACK, push-flush drive catch-up on $DD00
read/write. This unblocks deep boot-trace parity + driveCycles. integration resumes after
(deep boot-trace-short, then ADR-017 cia-cascade). cia-cascade was not reached (IEC gap
blocks the trace first).
**Why:** A substantial stateful subsystem deserving its own gated item, not a half-build.

## ADR-024 — C64↔1541 IEC serial bus wired (boot-trace-short byte-exact)
**Context:** iec-bus — the last full-boot divergence (trace[79] $DD00 read).
**Decision (blessed, GREEN + no regression):** `iec.rs` IecCore = 1:1 VICE wired-AND fold
(cpu_port = cpu_bus & drv_bus; ATN-acknowledge auto-pulls DATA), FullBus $DD00 read
composes folded CLK/DATA in + emits the iecReadPins indirection record (read-side-effect
path in cpu.rs), $DD00 write push-flushes + drives cpu_bus, and a push-flush drive
catch-up (catch_up_to) advances the drive 6502 to the exact C64 clk on each $DD00 access.
Drive VIA1 PB read applies the via1d1541 read_prb formula. RESULT: boot-trace-short FULLY
byte-exact; full regression GREEN; boot-basic-ready CPU/VIC/c64Cycles byte-exact.
**Why:** Full-machine boot trace now matches VICE byte-for-byte — Phase-1 emulation parity
proven on a real assembled-machine trace.

## ADR-025 — driveCycles +2 = drive disk-controller VIA2 (own item drive-via2)
**Context:** boot-basic-ready driveCycles 2029941 vs 2029939 (+2), IEC-independent
(present since the keystone). Traced precisely: drive-cpu PC+regs byte-exact for 203087
records, then drive PC $F266 `LDA $1C0C` reads VIA2 PCR — TRX64's VIA2 is a 0xFF stub;
VICE returns computed PCR/timer/handshake reads.
**Decision:** Carve out `drive-via2` [opus] (model the 1541 disk-controller VIA2 computed
reads). It is the bus the drive-boot-idle gate depends on, so model it carefully without
regressing. Tracked, low priority (corner; boots + traces correctly without it).
**Why:** A distinct subsystem, well-diagnosed; not worth risking the green drive gates by
speculative modelling under the IEC item.

## ADR-026 — protocol-surface core done; deferred method-groups carved into items
**Context:** protocol-surface implemented the machine-backed + lifecycle WS methods
(inspection: monitorRegisters/Memory/Disasm, status; stepping: stepInto/Over/until,
breakpoints, run_prg; lifecycle: debug/run|pause|continue|step, break_*, mark, port-race,
crash-log) — 5 protocol scenarios GREEN, no regression. Some methods were honestly
NOT_IMPLEMENTED (ADR-019, no faking) because they need subsystems TRX64 lacks.
**Decision:** Accept protocol-surface (core). Carve the deferred groups into tracked items:
- `vic-render` [opus] — VIC pixel framebuffer → session/screenshot, runtime/render_screen,
  vic/inspect/*, export screenshot/video. (Also the user-requested screenshot capability.)
- `media` [sonnet] — media/ingress mount/eject/swap, swap_disk_and_continue (disk media).
- `daemon-trace-query` [sonnet] — duckdb-backed daemon methods (trace/read, checkpoint/*,
  recorder/*, memory_access_map). The query LAYER stays TS (ADR-001); these are the
  daemon-side delegations — lower priority (MCP trace_store_* reads duckdb files directly).
- vsf/* → the existing `snapshot-vsf` item.
**Why:** Each deferred group is a real subsystem; honest carve-outs keep the gate truthful.

## ADR-027 — snapshot-vsf done (round-trip parity); vsf.rs-in-core tolerated
**Context:** snapshot-vsf — VSF save/load + native snapshot.
**Decision (accepted, gates GREEN):** vsf.rs save_vsf/load_vsf (9 modules in VICE order,
x64sc-vs-c64re auto-detect); 8/9 module bodies byte-size-identical to TS; behavioral gate
= session/state byte-identical before-save/after-load/after-post-restore-run (round-trip),
both vsf scenarios GREEN, no regression. Native snapshot = Session::take_snapshot/restore
over Machine::clone (full drive state, Phase-2 ready). MINOR ARCH-DEBT: vsf.rs landed in
trx64-core not trx64-session (against ADR-002) — but it is PURE (no async/io; file ops in
the daemon), self-contained, no hot-path/dep impact. Tolerated; may move to session later.
The DRIVECPU VSF module is a 0-byte stub vs TS's 2461-byte drive blob (TS documents its own
as a stub too) — folds into `drive-via2` (full drive internal state).
**Why:** The behavioral contract (restored state) is what matters; byte-parity on the
deferred drive blob is out of scope here.

## ADR-028 — media WS surface done; load-from-disk blocked on drive-via2
**Context:** media item — disk mount/swap/browse/persist + swap_disk_and_continue.
**Decision (accepted, gates GREEN):** 8 media methods response-parity vs TS (4 scenarios
GREEN). D64/G64 attach = DiskImage on Drive1541 + real SHA256 (sha2/hex deps in daemon,
core clean); diskPath reflected in session/create+list; additive in drive.rs (no timing
touched); cold_reset clears disk (VICE-faithful). A real LOAD"*",8 byte-load needs the
drive VIA2 + GCR read path → tracked `drive-via2`. crt attach returns explicit error (no
fake). The recurring boot-basic-ready driveCycles +2 is the same drive-via2 gap, confirmed
pre-existing in every sweep — NOT a regression.
**Why:** The mount/attach surface is independent of GCR read; the byte-stream needs VIA2.

## ADR-029 — vic-render done: pixel-exact screenshot via custom RGBA gate
**Context:** VIC pixel framebuffer + session/screenshot. The standard oracle diffs WS
response values, but a screenshot is a base64 PNG dataUrl — PNG zlib output differs
between encoders, so a byte-diff spuriously REDs. (dataUrl is in the oracle's volatile
whitelist → standard compare wouldn't check pixels at all.)
**Decision (accepted, gate GREEN):** a custom render runner (tools/oracle/corpus/render/
capture.mjs + png.mjs) boots both daemons to the steady BASIC-ready screen, decodes both
PNGs to RGBA, compares PIXELS. Independently re-verified: pixel-identical 384×272 (0 of
104448 differ). render.rs (pure renderer, Colodore palette verbatim) in trx64-core; png
encode + base64 in trx64-daemon (ADR-002). session/screenshot + runtime/render_screen
(scale 1/2/4) + vic/inspect wired. Standard text mode is pixel-gated; multicolor/ECM/
bitmap implemented-but-not-gated, sprites unrendered → `vic-sprites-modes` follow-up.
**Why:** Pixel parity needs decoded-RGBA comparison, not container bytes — a legitimate
gate extension. The user-requested screenshot now works, pixel-exact.

## ADR-030 — drive VIA2 modelled; boot-basic-ready FULLY GREEN (main suite byte-exact)
**Context:** the keystone corner — VIA2 0xFF stub caused driveCycles +2.
**Decision (accepted, GREEN + no regression):** VIA2 ($1C00) modelled as a real MOS 6522
(T1/T2 timers + latch/reload anchors, IFR/IER + IRQ-line, ACR/PCR, computed port reads)
in drive.rs, ported from viacore.ts; VIA1 also routed through the real 6522. cpu.rs gained
`set_irq_line_at(asserted, stamp_clk)` + `interrupt_just_dispatched()` — ADDITIVE (the drive
VIA fires IRQ from a sub-instruction clk; the C64 path keeps using set_irq_line, so NO C64
regression — boot-trace-short + all iso gates stay byte-exact). RESULT: boot-basic-ready
FULLY GREEN (driveCycles 2029939, +2 gone) — the main suite is now entirely byte-exact.
Two new tracked follow-ups: `drive-watchdog-phase` (drive-boot-deep RED trace[212703] cyc
1048810 vs 1048808 — 3rd T1 watchdog IRQ +2, needs a VICE drive-cpu cycle cross-check) and
`drive-gcr` (GCR read path for real disk LOAD; not started, no-disk defaults sufficed here).
**Why:** Mandated gate met (boot-basic-ready GREEN, no regression); the deeper corner is a
distinct, well-characterized timing artifact for a dedicated item.

## ADR-031 — SID 6581 osc/env model done; sid domain RESERVED (no live producer)
**Context:** sid builder — model SID 6581 oscillator + envelope so computed reads
($D41B osc3, $D41C env3) match the TS oracle; verify whether the `sid` trace channel
has a live producer (ADR-015 pattern).
**Decision (accepted, GREEN + no regression):** `sid.rs` in trx64-core implements the SID
6581 B-level model 1:1 ported from TS `headless/sid/sid.ts` (Spec 151): 3-voice oscillator
phase advance (24-bit phase, NSHIFT(rv,16) LFSR noise), waveform-aware osc3 readback
(triangle/sawtooth/pulse/noise, combined AND), per-voice ADSR state machine with PAL
rate table, $D41B (osc3) / $D41C (env3) computed reads, $D419/$D41A → 0x80 (POT
unconnected default). Ticked wall-clock batch per instruction (matching TS
`sid.tick(totalCycles)` in integrated-session.ts:946). `Sid6581` added to `Machine` (Clone
for Phase-2 COW forks); FullBus SID read now routes $D41B/$D41C through `sid.read()`
instead of stub shadow; `SidBus` isolation gate added. `sid` domain routes to `run_for_sid`
in daemon; sid `TraceChannels` field added (no frames emitted).
EMPIRICAL FINDING (ADR-015 repeat): `sid` channel is RESERVED with NO live producer —
confirmed by grepping TS source: no `trace.publish("sid", ...)` anywhere; SID writes appear
as op-0x11 RAM_WRITE from the CPU bus tap only (same as all other I/O). The `writeTrace`
callback on `Sid6581` is an audio-recorder hook, not a trace sink. `SID_REG_WRITE = 0x22`
exists in binary-format.ts but is never emitted. PCM audio output (reSID/WAV) is Phase-1.5,
OUT OF SCOPE. RESULT: iso-sid-osc3-env3 GREEN (osc3=0x01, env3=0x01 match TS oracle at
2000 cycles); full 20-scenario regression sweep GREEN (0 regressions).
`monitor/exec "m"` memory dump added to daemon (TS monitor-shell.ts format: 32-byte rows,
96-char padded hex, ASCII chars); VSF `load_sid` resets internal voice state on restore.
**Why:** The SID is the last core chip; ADR-015 empirical check prevents implementing a
channel producer the spec never had. osc3/env3 computed reads now match the TS oracle.

## ADR-031 — sid done (osc3/env3 byte-exact); audio PCM → Phase 1.5
**Context:** last core chip — SID 6581.
**Decision (accepted, GREEN + no regression):** sid.rs models the 3-voice oscillator
(24-bit phase, LFSR noise, waveform-aware $D41B osc3 readback) + per-voice ADSR ($D41C
env3), POT defaults; Sid6581 in Machine (Clone), SidBus isolation gate, FullBus routes
$D41B/$D41C through sid.read() (additive — boot-basic-ready stays GREEN). iso-sid-osc3-env3
GREEN, regression GREEN. ADR-015 re-confirmed: the `sid` trace channel is RESERVED/no
producer (no trace.publish("sid"); SID writes appear as op-0x11). DEFERRED to Phase 1.5:
PCM audio / reSID sample gen / WAV export, voices 1+2 osc advance, ring-mod/hard-sync/filter.
**Why:** Register + osc/env read behavior is the trace-relevant contract; audio is a
separate Phase-1.5 concern. ALL CORE CHIPS (CPU/VIC/CIA/SID/VIA1+2/Drive) now modelled.

## ADR-032 — GCR encoding + mount byte-exact; sector-read/LOAD → drive-load
**Context:** drive-gcr — GCR read path → disk LOAD.
**Decision (PARTIAL accept, gates GREEN):** MERGE milestone 1 — gcr.rs ports VICE gcr.c +
D64 track build; a mounted D64 encodes to a byte-identical GCR bitstream (8 parity tests,
SHA-256 matches TS per speed zone). disk-mount-idle GREEN (drive trace byte-exact with a
D64 mounted), no regression. rotation.rs (rotation_1541_simple) wires the rotating GCR
stream into VIA2 (PRA=GCR_read, PRB7=SYNC, byte_ready→V-flag) — the read path ENGAGES
(motor/head/SYNC/byte-assembly all work) but the live sector read returns status $03 (SYNC)
not $01. CARVE the rest into `drive-load` [opus] with the precise diagnosis:
(1) the set_ca2 byte-ready→SO-overflow flush on the PCR CA2 edge + per-cycle drivecpu_rotate
cadence so the SO edge lands at the controller's exact sampling instant ($F556 read loop —
needs a drive-cpu trace cross-check); (2) the ATN→VIA1 CA1 IRQ (DOS attention $FE67→$E85B)
— VIA1 lacks CA1. Both well-characterized; documented in tests/drive_sector_read.rs (kept).
**Why:** The encoding + mount are real, byte-exact, regression-free value; the cycle-exact
GCR controller sampling + CA1/CA2 handshake is a deep focused subsystem of its own.

## ADR-033 — drive-load: byte-exact GCR read stream; full LOAD → iec-serial
**Context:** drive-load — close the GCR controller sampling + ATN, reach a real LOAD.
**Decision (accepted, gates GREEN):** BOTH diagnosed pieces from ADR-032 are now correct
and merged: (1) set_ca2 byte-ready→drive-CPU SO-overflow flush on PCR CA2 (via2_store_pcr/
prb_effects, latched in DriveBus.pending_set_overflow, folded by step_instruction); (2)
ATN→VIA1 CA1 IRQ (Via6522::signal_ca1 + Drive1541::atn_edge_to_via1_ca1, wired from
c64_store_dd00) + combined VIA1∨VIA2 drive IRQ. RESULT: the drive-cpu GCR read stream is
BYTE-EXACT (disk-read-engage GREEN, 20176 records vs TS); the drive reaches sync, decodes
headers, responds to ATN, seeks, reaches send-byte. No regression (boot-basic-ready/-trace,
drive-boot-idle, cia all GREEN — the changes are drive-scoped). The FULL program-LOAD blocks
on a THIRD subsystem → `iec-serial` [opus]: the bit-level IEC serial-transfer handshake under
TALK/LISTEN — the C64 spins in KERNAL serial-receive CLK-poll $EEA9 (256010×) waiting for the
drive's per-bit CLK (VIA1 PB3→IEC CLK) transition; C64 times out ST=$42. Distinct from the
byte-ready cadence + ATN attention IRQ (both now correct).
**Why:** Each disk-LOAD layer is byte-exact-verified as it lands; the per-bit IEC serial
protocol timing is the final, distinct subsystem.

## ADR-034 — iec-serial misdiagnosis corrected; LOAD blocker = drive-read-engine
**Context:** I carved `iec-serial` as the LOAD blocker (C64 spinning at $EEA9). The builder
investigated and CORRECTED it: the IEC bit-serial layer WORKS (filename LISTEN completes,
TALK sent, the drive runs its receive/send bit-loops). The real blocker is one layer LOWER.
**Decision:** The LOAD blocker is the **GCR read engine**: after TALK, the drive dispatches
a track-18 (directory) read job that returns status **$03 (FDC SYNC)** not $01 — so it never
has file data to send and the C64 times out (ST=$42). The GCR DATA is byte-exact
(gcr_d64_parity passes) and SYNC is present (10 ones), but the controller does not assemble
a byte-exact SECTOR. Re-frame as `drive-read-engine` [opus]: fix the sync/byte-latch path vs
the $F556 timer-driven read so a sector read returns $01 with byte-exact bytes.
**KEY GATE-GAP LEARNING:** `disk-read-engage` is byte-exact on the drive-cpu TRACE yet the
read JOB still fails — trace-parity ≠ functional correctness. drive-read-engine MUST gate on
the JOB STATUS ($01) + the read sector bytes, not just the drive-cpu trace, and cross-check
the drive-cpu trace at $F556 vs the TS oracle to localize the byte-latch divergence.
**Why:** Own the misdiagnosis; route the work to the actually-failing layer with the right gate.

## ADR-035 — drive-read-engine: GCR sector read $03→$01 ROOT CAUSE cracked
**Context:** the disk-LOAD root blocker — sector read returned $03 (SYNC) not $01.
**Decision (accepted, GREEN + byte-exact):** Root cause found: rotation_sync_found()
returned 0x80 (no-sync) while attach_clk != 0 (the 1.8M-cycle DRIVE_ATTACH_DELAY spin-up
window). VICE clears attach_clk only in rotation_byte_read (PRA/$1C01), getting away with
it because real jobs issue a PRA read during setup — but the 1541 DOS find-sync loop $F562
(BIT $1C00 / BMI) polls SYNC via PRB ONLY, so attach_clk never cleared → never sees SYNC →
$1805 watchdog → $03. FIX (rotation.rs): after DRIVE_ATTACH_DELAY elapses, drop the spin-up
window on ANY rotation access (PRB included) — physical reality (disk is up to speed
regardless of which VIA register is sampled). Within the delay nothing changes → byte-exact
mount/idle unaffected. RESULT: T18S0 directory read → JOB STATUS $01, $0300 buffer byte-
identical to the D64 (0/256 diffs). disk-read-byteexact GREEN, no regression. Functional
gate (status $01 + sector bytes) in drive_sector_read.rs since the oracle has no WS surface
to read DRIVE RAM.
**Why:** Cracked the deepest layer of the disk onion via the right (functional) gate.
END-TO-END LOAD still pending: with read-engine + IEC-serial both working, the full
LOAD"$"→$0801 is the integration payoff → next item disk-load-e2e.

## ADR-036 — keyboard matrix done (session/type was a stub); LOAD → iec-talk-turnaround
**Context:** disk-load-e2e — real end-to-end LOAD. Found that session/type was a pure STUB
(no keyboard emulation) — so no LOAD command could ever be typed.
**Decision (accepted, GREEN + no regression):** Ported the CIA1 keyboard matrix to the core
(keyboard.rs: 8×8 matrix, PETSCII type_text w/ auto-SHIFT, exact readRowsForPa; full.rs
$DC01 read via the VICE read_ciapb formula, regression-safe — collapses to raw read with no
keys queued; KeyboardMatrix on Machine, cleared on cold_reset). session/type now really queues
keys. PROVEN: typing LOAD"$",8 → FA=$8 (KERNAL parses, addresses device 8, drive DOS runs
OPEN + directory-search). Regression GREEN. Carve the remaining LOAD blocker into
`iec-talk-turnaround` [opus]: the LISTEN→TALK turnaround deadlocks (~C64 cyc 16.8M) — C64
spins ACPTR $EE67 (wait talker CLK-low), drive returns to $EBFF/$EC00 idle instead of entering
talk-send + pulling CLK; IEC lines cpu_port=$C0, drv_port=$85. Hypothesis: drive doesn't latch
"addressed-to-TALK" across the ATN-release turnaround (EOI/turnaround cadence or missed TALK
latch when ATN drops after the OPEN directory job). Keyboard/IEC/GCR all verified, not implicated.
**Why:** Keyboard is a real, broadly-useful prerequisite (any typed input); the TALK turnaround
is the next precisely-localized cycle-exact handshake.

## ADR-037 — LOAD/custom-loader root cause: IEC cross-domain sampling skew (KEYSTONE)
**Context:** iec-talk-turnaround REFINED the diagnosis (test-only probe, no production change):
the LISTEN→TALK turnaround + the first 10 directory bytes transfer BYTE-EXACT (the drive does
engage as talker). The real blocker is one byte-boundary later.
**The root cause (precise):** cross-domain sampling SKEW. The drive reads the C64's IEC lines
(CLK/DATA/ATN) from the `iec_drv_port` snapshot in full.rs::iec_push_flush_to, which is refreshed
ONLY on a C64 $DD00 access. During a tight bit-bang the drive runs MANY instructions per C64
$DD00 read, so at clk 4945505 (drive PC=$E961, `BNE $E999`) it samples a STALE C64 DATA value
(released) right where the C64's DATA output is mid-transition → the drive misreads it as
stop/EOI and aborts the talk-send → C64 hangs in ACPTR $EE67. Final stall: cpu_port=$C0,
drv_port=$85, $7A(talk)=$01.
**Decision:** This is THE keystone of the whole IEC story — it underlies BOTH the remaining
standard-LOAD blocker AND the custom-loader $DD00 bitbang (the user-flagged hardest case; same
class). Carve `iec-crossdomain-sync` [opus]: make the drive see the C64's IEC line state at the
cycle-exact instant it samples — bidirectional cross-domain catch-up (the existing push-flush
syncs drive→C64-clk on $DD00 access; the missing piece is the drive's view of C64 lines at the
DRIVE's clk when it polls $1800). Fix site: full.rs iec_push_flush_to + drive.rs $1800 read
(~line 690). Approach: TS-oracle drive-cpu trace diff at clk 4945495–4945509 to find VICE's exact
DATA-sample instant. Must not regress bytes 1–10 or the GREEN disk gates.
**Why:** The current lazy snapshot model is too coarse for cycle-tight IEC bit-bang. Fixing it
cycle-exactly is the prerequisite for both LOAD completion and custom loaders.

## ADR-038 — STANDARD LOAD COMPLETE: IEC cross-domain sync cycle-exact (keystone done)
**Context:** the keystone — fix the cross-domain sampling skew (ADR-037) so the drive sees
its own + the C64's IEC pulls cycle-exactly.
**Decision (accepted, GREEN + no regression):** Root cause confirmed: drv_port was snapshotted
ONCE at flush start and held constant through the drive's multi-instruction catch-up, so the
drive's OWN $1800 PB pulls were invisible to its subsequent $1800 reads (drv_port folds the
wired-AND cpu_port = cpu_bus & drv_bus, which includes the drive's contribution). FIX (mirrors
VICE via1d1541.c store_prb / TS via1d1541.ts): a drive $1800 PB/DDRB store that CHANGES the
composed PB output (gated on byte != p_oldpb, matching VICE) immediately re-folds drv_port via
new pure fn iec::fold_drv_port(cpu_bus, pb_out); cpu_bus threaded to the drive (Drive1541.
iec_cpu_bus) at both push-flush sites. The output-change gate leaves idle/boot snapshot behavior
untouched (no regression). RESULT: disk-load-dir GREEN — LOAD"$",8 lands the full 640-byte
directory in $0801 byte-identical to TS, vartab=$0A7F, ST=$40 (EOI); the false $E999 send-abort
is gone; C64 returns cleanly to BASIC. Full regression GREEN (boot/drive/disk/cia/sid). 69 core
tests pass.
**Why:** STANDARD LOAD is complete. This cross-domain model is the SAME one custom-loader $DD00
bitbang relies on — so the user's acid test (scramble) is now reachable. Disk-LOAD onion fully
peeled: via2→gcr→load→read-engine→keyboard→crossdomain-sync.

## ADR-039 — custom-loader acid test: file-load GREEN; blocker = serial-load RATE skew
**Context:** custom-loader-gate on scramble_infinity.d64 (user-pinned acid test). Corpus-only
(no crate code) — a proving gate + a precise diagnosis.
**Findings:** (1) NEW GREEN `scramble-load-file`: LOAD"*",8,1 file-search + sector-link-chain
lands the 7747-byte SCRAMBLE bootstrap at $0801 byte-exact vs TS (ST=$00) — the FILE load path
proven (distinct from the already-green directory path). Boot = LOAD"*",8,1 + RUN (SYS 2061 →
$080D banks out KERNAL, ZP loader times vs CIA1 Timer A, JMP $4000 = the custom $DD00 bitbang).
(2) The custom loader is NOT reached — blocked upstream by a SERIAL-LOAD RATE SKEW: TRX64's
per-byte serial cadence runs ~2.5% (~200k cyc) AHEAD of TS; first divergence ~8.0M cyc
(scramble-load-progress RED: end4[0] 0 vs 4 / $AE=$00 vs $05). The END STATE converges byte-exact
(file-load green → NOT corrupting), but the per-byte CADENCE is fast. A handshaked KERNAL load
tolerates this; a cycle-exact $DD00 bitbang raster-synced to the VIC does NOT. The short directory
load doesn't accumulate enough serial cycles to expose it; the 7747-byte file load does.
**Decision:** Carve `iec-serial-rate` [opus]: close the ~2.5% per-byte serial-load rate skew (the
KERNAL serial-receive loop's per-byte IEC bit-timing / CIA1-Timer-A interaction). Gate: flip
scramble-load-progress RED→GREEN ($AE cadence matches TS at 8M). Then RUN + the custom $DD00
loader become reachable byte-exact (custom-loader-gate remains the eventual goal).
**Why:** This is EXACTLY the $DD00+timing class the user flagged as the TS-core's most expensive —
the acid test surfaced it precisely, one layer before the custom loader. 2 tracked known-REDs now:
drive-boot-deep + scramble-load-progress.

## ADR-040 — CORRECTION of ADR-039: blocker is a rotational-PHASE lead, not a rate skew
**Context:** iec-serial-rate investigated the scramble-load-progress RED. It DISPROVED ADR-039.
**Corrected diagnosis (no fix committed — confident diagnosis, fix deferred to avoid re-breaking
standard LOAD):**
- The per-byte serial RATE is IDENTICAL to TS (~2437 vs ~2429 cyc/byte, noise; lead does NOT grow).
  ADR-039's "~2.5% accumulating rate skew" is WRONG.
- The real divergence is a ONE-TIME ~17,000-cycle PHASE LEAD at transfer start: TRX64 begins the
  data byte-stream at C64-clk ≈3.988M, TS at ≈4.005M; thereafter lockstep (constant +8/+9 byte
  offset). scramble-load-progress `end4` (0 vs 4) catches exactly this checkpoint.
- ~17k ≈ one inter-sector gap. The drive finds its target sector ~one gap EARLY because the head's
  ROTATIONAL PHASE at job-issue differs by a constant — and that traces to the ADR-035 deviation:
  TRX64 clears `attach_clk` inside rotate_disk on ANY rotation access (incl. PRB/SYNC polls),
  whereas VICE/TS clears it ONLY in rotation_byte_read (PRA/$1C01). That deviation was needed to
  fix the $03 find-sync hang, but it starts the disk rotating at a different clk → constant phase
  lead. Bit-rate constants, IEC core, and the sync-factor accumulator were verified 1:1 with TS.
**Decision:** Re-frame as `drive-rotation-phase` [opus]: make attach_clk clearing match VICE
(PRA-only) while preserving the find-sync fix differently (gate sync-visibility on elapsed-since-
attach WITHOUT mutating rotation_last_clk/attach_clk from the PRB poll), so the rotational phase at
transfer start matches TS. GATE: scramble-load-progress RED→GREEN AND drive-read-byteexact /
disk-read-engage MUST stay GREEN (the $03 find-sync must not regress). Confirm the exact phase delta
via a drive-clk diff at the rotate_disk first-call instant before changing it.
**Why:** The loop self-corrected a wrong ADR via trace-diff investigation. The true blocker is one
focused rotational-phase fix, on the same ground as drive-read-engine (ADR-035).

## ADR-041 — ADR-040 falsified too; ADR-035 hack removed (improvement); phase-lead ESCALATED
**Context:** drive-rotation-phase tested the ADR-040 attach_clk theory.
**Outcome:** (1) IMPROVEMENT MERGED: the ADR-035 attach_clk hack (clear inside rotate_disk on
any access) is REPLACED by VICE's actual mechanism — `drive_writeprotect_sense` (drive.ts:1661-
1698) clears attach_clk after DRIVE_ATTACH_DELAY via the PRB read path (WPS bit); the DOS $F562
find-sync polls PB7/SYNC + PB4/WPS together via PRB, so SYNC unmasks with no PRA read — spec-
faithful, removes the deviation, ZERO regression (all $03/find-sync gates + full sweep GREEN).
(2) ADR-040 FALSIFIED: the ~17-20k phase lead is NOT attach_clk/SYNC-visibility — decisive
experiment: shifting the SYNC-unmask by +17k AND +400k (two revolutions) moved transfer-start by
EXACTLY 0 (the unmask happens ~5M cyc before the read job). Combined with ADR-040's falsification
of the rate-skew theory, BOTH prior root-cause theories are dead.
**Remaining (narrowed): the read-job's ROTATIONAL POSITION at sector-lock** — the seek/step timing
to the target track, or the GCR head-advance — places the head ~one inter-sector gap early. Next
investigation: instrument drive-clk + gcr_head_offset at the instant the read job first locks the
target sector's header, TRX64 vs TS.
**Decision:** ESCALATE to the user. This phase-lead has now resisted 3 opus theories (rate / attach_
clk, both falsified by trace-diff); it is the user's flagged hardest domain ($DD00/IEC timing).
Standard LOAD works (end-state byte-exact) — only cycle-exact custom loaders need this. Park the
custom-loader chain as BLOCKED-tracked; route the loop to clean followups (default) until the user
steers (one more focused opus attempt on seek/step+head-advance, vs a dedicated session).
**Why:** 3 falsified theories on one sub-cycle lead = diminishing returns from blind opus; the user's
domain expertise + a fresh look is the higher-leverage path. The diagnosis is now sharp enough to hand off.

## ADR-042 — vic-sprites-modes done: VIC renderer fully pixel-exact
**Context:** vic-render did standard text; sprites + non-text modes + fine-scroll were unguarded.
**Decision (accepted, GREEN + no regression):** render_sprites (render.rs) renders all 8 HW
sprites pixel-exact vs TS — hires/multicolor, X/Y/XY expand, X-MSB ($D010), sprite-sprite +
sprite-bg priority (per-pixel foreground mask = VICE px&0x2). The graphics modes (multicolor-
text, standard+multicolor bitmap, ECM) pixel-gated (color logic already matched). Fine-scroll
geometry rewritten: border WINDOW decoupled from CONTENT origin (content_y0=48+YSCROLL,
content_x0=136+XSCROLL, clip to window); 38-col/24-row/XSCROLL/YSCROLL all pixel-exact (key:
in-row XSCROLL gap fills $D021 background, idle region above/below fills black). New `wr io`
monitor lens → Machine::poke_io (lib.rs) for VIC programming; the default `wr` lens is
UNCHANGED so no iso-* gate is affected. 20 render scenarios GREEN (verified subset + render-
boot + boot-trace + api-call-monitor + 70 cargo tests). Renderer in core (pure); PNG/WS in daemon.
**Why:** The VIC renderer is now complete + pixel-exact (sprites + all modes + scroll) — the full
visual foundation for Phase-2 frame-hash probes (the user-flagged custom-loader visual checks).

## ADR-043 — cia-cascade done: ADR-017 closed, CIA fully byte-exact
**Context:** the long-tracked chained-timer corner (TB counts TA underflows). The cia builder's
per-cycle model over-counted (ADR-017).
**Decision (accepted, GREEN + no regression):** Ported VICE's lazy alarm-driven cascade into
cia.rs — `Ciat::set_alarm` walks the 8192-entry transition table to predict the EXACT next-
underflow clk; `ta_alarmclk`/`tb_alarmclk`; update_ta dispatches each predicted TA-underflow
alarm <= rclk via `intta` (counts + reschedules + in cascade mode calls update_tb + do_step_tb);
the TB decrement is realised LAZILY by TB's next ciat_update, so intermediate TA underflows
collapse exactly as VICE collapses them. Re-arm alarms after every timer-mutating write.
iso-cia-cascade (reconstructed) + -irq + -oneshot all GREEN; the 4 existing CIA gates + boot-
trace-short stay GREEN; 71 core tests. ADR-017 CLOSED.
**Why:** The CIA is now fully byte-exact (timers/TOD/ICR/cascade) — the last CIA gap is gone.

## ADR-044 — drive-watchdog-phase done: drive-boot-deep GREEN; root cause = IRQ-dispatch latency
**Context:** the drive-boot-deep KNOWN-RED (3rd VIA2 T1 watchdog IRQ +2). ADR-030 attributed it to a
T1 free-run reload PHASE bug.
**Decision (accepted, GREEN + ZERO regression):** ADR-030 was a MISATTRIBUTION — the timer schedule
(t1zero = rclk+1+tal) was already correct (testing +FULL_CYCLE_2 broke IRQ#1). The early IRQ was a
drive-6502 IRQ-DISPATCH-LATENCY gap. Fixed two unmodelled VICE `interrupt_check_irq_delay` behaviors
in the SHARED cpu.rs: (1) OPINFO_DELAYS_INTERRUPT — a taken branch w/o page-cross delays the next
IRQ/NMI 1 cycle (6510core BRANCH macro); (2) OPINFO_ENABLES_IRQ — after an I-clearing opcode
(CLI/PLP/RTI, 1→0) with an IRQ cycle-ready, VICE defers dispatch a FULL instruction (IK_IRQPEND).
All 68 watchdog IRQ-entry cycles then matched. + daemon trace-chunking: replay the TS golden's
100k-cycle TRACE_DRAIN segmentation (each runFor break overshoots ~1 instr, ~37 cyc over 20 segs).
Independent comprehensive gate: drive-boot-deep + boot-trace-short + boot-basic-ready + iso-trace-broad
+ iso-vic-badline-irq + iso-cia/cascade + disk-load-dir + disk-read-byteexact ALL GREEN; workspace
tests green. BONUS: scramble-load-file flipped GREEN. This is a global CPU IRQ-dispatch FIDELITY
improvement (the shared change helps the C64 too).
**Why:** The interrupt-dispatch boundary was the real divergence — the same "VICE C-indirection lost
in a cleaner abstraction" class Spec 612 warns about (just IRQ-delay, not write_offset). known_red now
= only scramble-load-progress (the custom-loader phase-lead).

## ADR-045 — C64RE specs are the KEY for the phase-lead (user steer)
**Context:** user pointed to the C64RE specs — "we had this there too" — re the custom-loader phase-lead.
**Findings (durable references for the next phase-lead attempt):**
- **Spec 218** (specs/_archive/218-motm-tx3-tx4-bit-level-divergence.md, CLOSED): the TS team hit the
  EXACT custom-fastloader stall (MoTM LOAD"*",8,1, $DD00 bitbang). ROOT CAUSE: `stepInward` off-by-one
  let the drive head step PAST track 35 (mechanical stop) on G64 extended tracks → GCR shifter bound to
  the wrong track → no-SYNC → loader stalls. FIX: cap stepInward at the track-35 mechanical stop. This
  MATCHES ADR-041's narrowing of the phase-lead to "seek/step + GCR head-advance at sector-lock."
- **Spec 612** (1541 Port Fidelity Rules): every VICE C-indirection (alarm ctx, write_offset, clk_ptr/
  rmw_flag refs) that a "cleaner" port replaced → boundary divergence. PL-6: write_offset is PER-VIA-
  INSTANCE, not hardcoded. viacore.ts:529 `rclk = clk_ptr - write_offset` feeds BOTH the VIA T1 alarm
  clk AND the $DD00 cross-domain callback (`rclk + (write_offset?0:1)`).
**Decision:** Arm the next phase-lead attempt (`drive-seek-phase`) with Spec 218 (head-stepping/track-35)
+ Spec 612 (write_offset per-instance) + the $4000→W425C→$1800/$DD00 transaction-diff playbook. The
phase-lead is very likely a drive head-stepping / seek-timing OR write_offset boundary divergence — NOT
blind anymore.
**Why:** The TS team already walked this exact path; reuse their root cause + doctrine.

## ADR-046 — drive-seek-phase: 4th theory; Spec 218 disproved for scramble; ESCALATED w/ the one measurement
**Context:** the custom-loader phase-lead (~17-20k cyc, scramble-load-progress RED), armed with Spec 218.
**Outcome:** (1) FIDELITY PROGRESS MERGED (zero regression, 7/7 drive+boot gates GREEN): two genuine VICE
store_prb C-indirections the port had dropped — `rotation_rotate_disk` at the TOP of store_prb (advance
head + rotation_last_clk to current clk before stepper/speed/motor) + bug #1083 motor-on-edge second
drive_move_head + the exact store_prb sequence (rotate→stepper→speed→motor). Genuine drive-model
correctness. (2) Spec 218 head-stepping DISPROVED for scramble: SCRAMBLE lives at track 1 sector 0; the
head correctly bumps from track 18 (ht36) to track 1 (ht2) via 36 half-steps and lands EXACTLY on ht=2 —
the seek is right (Spec 218's stepInward-past-track-35 was an extended-track-G64 issue; scramble seeks
inward). The store_prb fix produced a byte-identical end4 → the lead is NOT in the seek/store_prb path.
**Narrowed precisely (4th theory's data):** the lead is the ROTATIONAL PHASE at the track-1 sector LOCK.
TRX64 locks the first track-1 SYNC at drive_clk=7901947, gcr_head_offset=33595 bits (byte ~4199 of the
7692-byte track), zone 3. Most likely: `rotation_1541_simple` accum-carry chunking (the `accum % rpmscale`
fractional carry is order-sensitive) vs the exact set of clk points at which `rotate_disk` is invoked
(VICE rotates on additional VIA2 paths — store_pra/$1C01, set_cb2 — TRX64 may sample at different clk
granularity).
**THE ONE MEASUREMENT NEEDED:** the TS-side drive_clk + head-offset at the IDENTICAL track-1 first sync.
The golden TS daemon doesn't expose drive head state — BUT the user's c64re runtime (mcp__c64-re__runtime_*)
DOES (deep-RE, exposes drive state). 
**Decision:** ESCALATE. 4 theories tested, each falsified by trace-diff but each leaving the drive model
genuinely more accurate. NOT a 5th blind attempt. Park the phase-lead BLOCKED; route the loop to the
remaining clean followups (daemon-trace-query, sid-audio) until the user steers: (a) drive the c64re runtime
to get the track-1-lock drive_clk+offset measurement, then arm a 5th attempt with it; (b) user looks at the
narrowed diagnosis; (c) leave it.
**Why:** The problem is precisely localized; the next step is a specific measurement the user's own deep-RE
tooling provides — higher leverage than another blind opus pass.

## ADR-047 — ARMED measurement: rotation-phase FALSIFIED; lead is UPSTREAM of track-1
**Context:** drive-seek-phase v5, armed with the live c64re reference runtime (user chose a).
**The decisive measurement (probes committed, no production change, 39/40 regression — only target RED):**
Drove the c64re reference (boot → LOAD"*",8,1), located the 1541 DOS find-sync ($F562 BIT $1C00 / BMI →
$F567 LDA $1C01), pinned the first track-1 SYNC lock. RESULT: TRX64 locks the first track-1 SYNC at the
SAME disk byte (~4199, zone 3) as the reference. `rotation_1541_simple` is BIT-FOR-BIT identical to TS
(1000-cyc chunking, accum % rpmscale carry, rpmscale=1_000_000, ROT_SPEED_BPS, DRIVE_SYNC_FACTOR_PAL=66517,
per-instr catch-up). **The rotation accum-carry is NOT the divergence — the 4th/5th rotation-phase theory
is FALSIFIED by direct measurement.**
**Where it actually is:** the ~12-20k C64-cycle lead accumulates UPSTREAM of the track-1 read — the
boot/seek/directory/IEC-filename-transfer phase. TRX64 deposits the first data byte at C64 clk 7988284
(~12k cyc BEFORE the 8M end4 sample); golden after 8M. The track-1 sync lock lands on the identical byte,
so it is NOT the rotation/read. The end4 RED is a SAMPLE-BOUNDARY effect (the lead tips first-byte arrival
to the wrong side of the exact-8M checkpoint).
**Theories now falsified (5):** rate-skew, attach_clk, store_prb-rotate, head-stepping, rotation-accum-carry.
**Next measurement (untested):** the C64-cycle at which the drive BEGINS the track-1 read job, anchored to
the LOAD keypress, in BOTH runtimes — the ~12-20k delta is in the directory-read / IEC-filename-transfer /
seek-START phase. The reference trace is saved at tmp/scramble-track1-measure.duckdb (7.5M events, queryable).
**Decision:** ESCALATE again with the decisive new localization. Each armed measurement narrows precisely +
leaves the model more accurate (no regression), but converges slowly (5 theories). Park the phase-lead
BLOCKED (now very precisely characterized: upstream of track-1, directory/IEC/seek phase). Loop routes to
clean followups. User decides: 6th armed attempt on the upstream lead (querying the saved ref trace), vs
accept as a precisely-bounded known-RED, vs user-led.
**Why:** The armed approach works but is slow; the user (deep $DD00/IEC domain) should weigh the 6th pass.

## ADR-048 — scramble-gold behavioral gate ROOT-CAUSED the custom-loader stall (corrects ADR-047)
**Context:** built the c64re-style behavioral gate (TS-vs-TRX64 stage screenshots) for the scramble custom loader.
**Finding (RED, root-caused, visually verified by Driver):** at 30M post-RUN — TS golden shows the FULL
"SCRAMBLE INFINITY" multicolor-bitmap TITLE screen; TRX64 shows "ENTERING SCRAMBLE SYSTEM" + an EMPTY
progress bar on grey, FROZEN byte-identical 30M→120M (the bar never fills). NOT the renderer (render gates
GREEN, the bar frame is pixel-clean); NOT the first-file load (KERNAL serial load tracks the golden to a few
bytes). THE SPLIT: the first file loads via KERNAL serial routines (works); the title artwork loads AFTER RUN
by the game's own custom $DD00 bit-bang loader — and THAT wedges on TRX64.
**CORRECTS ADR-047:** the cycle-exact `scramble-load-progress` phase lead is NOT a cosmetic sample-boundary
artifact — it is the UPSTREAM CAUSE of a real functional stall. The KERNAL tolerates the sub-byte $DD00 phase
skew; the tighter custom $DD00 loop does NOT. The behavioral gate ELEVATED the cycle-exact gate from "nitpick"
to "documented functional gap with a concrete visual repro."
**Fix target:** the post-RUN custom $DD00 bit-bang loader drive/IEC timing — localize with a drive-cpu trace
taken right after RUN where TRX64's bar stops advancing, find the first divergent $DD00/drive event. The
scramble-gold gate (tools/oracle/corpus/render/scramble-gold.mjs) is the permanent behavioral acid test.
**Why:** The custom loader — the user's hardest case + whole point — genuinely does not run yet; the gate now
proves it + pins the cause.

## ADR-049 — ultracode workflow: phase-skew theory FALSIFIED; bug is a write-path fold deadlock (H1)
**Context:** dd00-loader-stall via a 9-agent ultracode workflow (6 parallel VICE-C-vs-TRX64 source-diffs of
each $DD00/IEC path + c64re reference-trace + synthesize + fix-verify). 31 candidate divergences.
**Outcome (no fix applied — zero regression, nothing fudged, working tree clean):**
- The PHASE-SKEW direction (6 prior attempts) is DEFINITIVELY FALSIFIED: DRIVE_SYNC_FACTOR_PAL=66517 is
  VICE-correct (drivesync.c floor(65536*1e6/985248); 66504 regresses drive-boot-idle/deep AND still wedges);
  the drive-reset origin is already correct (drive-boot-idle GREEN, first boundary at cycle 8).
- BUG RE-CHARACTERIZED: a MUTUAL STUCK-VALUE HANDSHAKE DEADLOCK, not a sub-cycle drift. C64 spins
  $04E2 BIT $DD00 / $04E5 BVC reading a constant $8A (bit6=CLK_in=0) — waits for the drive to pull CLK low;
  the DRIVE spins $0402-$0408 and never asserts CLK. Two-sided wait, neither breaks.
- VERIFIED VICE-faithful (eliminated): IecCore fold/read_prb/via1_pb_iec_output, write push-flush+ATN-CA1
  order, +1 write offset. One INACTIVE-here fidelity gap: via1_pb_iec_output omits VICE's PB7-toggle override
  (viacore.c:720, ACR&T1_PB7) — VIA1 ACR=$00 at the wedge so value-neutral; fix separately for fidelity.
**Ranked hypotheses (next):**
- H1 [highest]: WRITE-path extra refold. TRX64 iec_push_flush_to (full.rs:167) calls
  drive_store_pb(via1_pb_iec_output()) — refolding drv_bus/ports from the drive's CURRENT PB using the OLD
  (pre-write) cpu_bus — BEFORE c64_store_dd00 updates cpu_bus + the ATN-CA1 edge. VICE iecbus_cpu_write_conf1
  does NOT refold from the drive PB inside the write (single fold). The stale-cpu_bus refold may publish a
  transient CLK/DATA the tight loop latches. FIX: write-path catch_up only, let c64_store_dd00's
  recompute_drv_bus(from drv_data_8) be the single fold = VICE-exact.
- H2: CA1 edge stamped 1 cycle off vs the drive's last executed cycle.
- H3 [definitive fallback]: diff the post-RUN drive-PC stream TRX64-vs-c64re-reference (the reference is NOT
  stuck at $04E2) — the first divergent drive-PC is the true first-divergence the analysis never had.
**Fast harness:** in-crate (Machine boot -> attach D64 -> type LOAD/RUN -> run_for_full, watch read_full(0x05FD)
climb past post-LOAD $20 = bar filling), ~10-30s vs the >10min WS gate (which times out here).
**Why:** The workflow killed a 6-attempt dead-end + delivered a precise, VICE-grounded, testable fix (H1) +
a fast iteration loop. THIS is the value of the parallel multi-perspective attack.

## ADR-050 — H1 falsified; ROOT CAUSE found: drive VIA2 byte-ready/SYNC/T1 coupling under direct-poll
**Context:** dd00-loader-stall H1 test + H3 reference-diff (live c64re).
**H1 (write-path single fold) FALSIFIED:** implemented the VICE-faithful single fold (split iec_push_flush_to
into a read-path fold + write-path iec_catch_up_to with drive_set_data_no_fold; the $DD00 write now lets
c64_store_dd00 do the SINGLE wired-AND fold vs the NEW cpu_bus, matching iecbus_cpu_write_conf1 — TRX64 had
double-folded). Result: BIT-IDENTICAL to baseline (the C64 spin is pure reads; the write fold gives the same
final state when cpu_bus is unchanged). Regression-clean (71 tests, dir-load byte-exact). KEPT as a fidelity
correction (removes a non-VICE double-fold).
**H3 ROOT CAUSE (concrete, via live c64re reference diff):** the deadlock is NOT IEC. The drive's custom-loader
$0402 loop (LDA ($3B,X)/BEQ / LDA ($11,X)/BMI $0402, X=$FE) polls VIA2 disk-controller registers DIRECTLY:
($3B,X)→$1F05 = VIA2 T1C-H (timer1 high), ($11,X)→$1CF0 = VIA2 PRB, BMI tests bit7 = SYNC. It is a GCR-read /
byte-ready / SYNC wait on VIA2 ($1C00), NOT an IEC line. The c64re REFERENCE drive cycles $0402↔$0260 ($1C05
T1 wait)↔$07Ax (bit-bang SEND)↔$EC1x ROM and reaches the CLK-toggle send. TRX64 stays PINNED in $0402-$0408
(1.5M cyc), NEVER drives the send → drive PB stays $FD (PB3=1 = CLK held LOW) → the C64's $04E2 BIT $DD00 / BVC
(waits bit6=CLK_in HIGH) spins forever. The IEC fold is CORRECT (PB3=1 ⇒ CLK low ⇒ cpu_port bit6=0 is right).
**The real bug:** the VIA2-timer (T1 $1F05/$1C05) + byte-ready/SYNC (PRB bit7 $1CF0) coupling in the rotation
model, UNDER DIRECT CPU POLLING. The standard DOS read works (dir-load byte-exact) because it uses
byte-ready→SO (set_overflow/V); this custom loader polls VIA2 PRB bit7 + T1 directly in a CPU loop, exposing a
VIA2/rotation byte-ready/SYNC/T1 coupling divergence TRX64's drive doesn't advance through. Fast probes added
(dd00_loader_bar_probe, dd00_loader_decode_probe). NEXT: diff TRX64-vs-c64re VIA2 T1 + byte-ready/SYNC
presentation at the $0402 loop — why the reference advances to $07Ax send + TRX64 stalls.
**Why:** 6 phase-skew attempts + 2 IEC-fold attempts all chased the wrong layer; the live-reference drive-PC
diff finally pinned it to the DRIVE's VIA2/GCR-read coupling under direct-poll — the deepest case, now exact.

## ADR-051 — VIA2 workflow: complicated-engine FALSIFIED; root cause = drive HEAD lands on EMPTY half-track
**Context:** dd00-via2-byteready via a 6-agent ultracode workflow (reference-advance trace + VICE-C diff of
via2d.c/rotation.c/drive.c byte-ready-SO + synthesize + fix). No fix applied (synthesis falsified by live
reference; zero regression; clean tree).
**FALSIFIED:** "port the complicated rotation engine (rotation_1541_gcr / so_delay / BUS_READ_DELAY)" — that
path only activates on a DISK WRITE; ZERO write-mode transitions in the scenario. BOTH reference and TRX64
use rotation_1541_simple EXCLUSIVELY, and TRX64's rotation_1541_simple is BYTE-FOR-BYTE identical to the TS
port (SYNC mask, last_read_data, byte_ready_edge all verified). The PRB/SYNC fold + T1 readout + byte-ready->SO
are byte-exact (why std DOS read is byte-exact — it is SO/edge-driven + phase-tolerant).
**ACTUAL ROOT CAUSE (newly confirmed via live c64re reference):** the drive HEAD TRAJECTORY diverges. TRX64's
head ends on HALF-TRACK 3 (track 1.5 = zero-filled empty inter-track, SYNC IMPOSSIBLE); the reference head ends
on HALF-TRACK 4 (track 2, real GCR data). Proof: reference VIA2 PB ORB $1C00=$AA (PB0-1=10 = stepper phase 2 =
ht4); TRX64 wrote $F5 (PB0-1=01 = phase 1 = stepped 4->3). The loader's stepper-output byte is COMPUTED from
DECODED GCR data; both run identical stepper code, so the divergence is UPSTREAM in the decoded byte stream.
Mechanism: an accumulated rotate_disk per-poll PHASE drift in the direct-poll $0402 loop (LDA $1CF0/BMI advances
the head once per poll via rotate_disk with rotation_last_clk WHOLE-CYCLE anchoring + no cycle-exact bus/SO
delay). Over thousands of direct polls the per-poll SYNC/byte window drifts vs the reference, TRX64 decodes a
DIFFERENT byte, the loader branches into ROM ($EC1x) instead of the RAM decode/send ($06xx hot path), writes the
WRONG stepper phase -> head lands on empty ht3 -> never finds SYNC -> never releases CLK -> bar never fills.
TRX64 is NOT permanently stuck at $0402 (escapes $0408->$040A 271x, reaches send $07Ax + ROM $EC1x) — it just
reads the wrong data and steps to the wrong track.
**NEXT (diagnostic, cheap):** diff TRX64's per-poll head-offset/decoded-byte vs the reference at the FIRST
divergent $1CF0 poll. Reference trace SAVED: /tmp/scramble_ref_head.duckdb (drive-PC hot at $0402-$0408 find-sync
+ $06BD-$06EF decode/send; TRX64 hot at $07Ax+$EC1x ROM). That pins the exact poll where the rotate_disk phase
first drifts -> THEN fix the per-poll phase (bit-cell-exact) WITHIN rotation_1541_simple's call, NOT a new engine.
**Why:** 10 attempts; the bug is finally a CONCRETE, near-physical symptom (head on an empty track from an
accumulated per-poll decode-phase drift), with an exact empirical validation path. This is the user's deepest
domain (custom $DD00 loader + GCR timing).

## ADR-052 — BVC/BVS/PHP/CLV drive SO-rotate clk fidelity fix (Rust omitted TS CLK_ADD(-1))
**Context:** manual Rust-vs-TS source-diff (user method) of the drive 6510 SO-opcode rotate timing.
**Found divergence:** drive.rs step_instruction did the byte-ready/SO rotate (BVC/BVS/PHP/CLV) at the
opcode-FETCH boundary at `bus.clk` (= instruction-start clk S), omitting the TS/VICE clk offset:
drive_6510core.ts fetches opcode+operand (CLK_ADD(2) → case body at S+2), then BVC/BVS do
`CLK_ADD(-1); rotate; CLK_ADD(1)` → rotate at S+1; PHP/CLV rotate at the case body S+2. The Rust
rotated all four at S (1-2 drive cycles early), shifting the byte-ready edge.
**Fix:** rotate BVC/BVS at `bus.clk+1`, PHP/CLV at `bus.clk+2` — matching the TS sample clk.
**Result:** regression-clean — scramble-load-file, disk-read-byteexact, drive-boot-deep, disk-load-dir
GREEN; unit tests pass. A faithful TS-matching fidelity correction.
**NOT the scramble fix:** the dd00_loader_bar probe is BYTE-IDENTICAL with/without the fix (bar stuck $20,
drive 87% in the $0402 spin) — the custom loader NEVER executes a SO opcode; it polls VIA2 directly
(LDA ($3B,X) BMI on $1CF0/SYNC, $1F05/T1). So this corrects a real latent divergence (any BVC-byte-ready
custom loader) but the scramble stall is elsewhere.
**Remaining suspect (last un-diffed Rust-specific layer):** the cross-domain catch-up model — full.rs
push-flush-on-$DD00-access (iec_catch_up_to / drive.catch_up_to) vs the TS continuous interleave. The
tight $04E2 BIT $DD00 / BVC loop touches $DD00 every ~6 cycles → very frequent catch-ups; a per-catch-up
phase the KERNAL load (rarer $DD00) tolerates but the bit-bang loop may not. CIA/IEC/rotation-engine/
call-sites/SO-path all verified faithful or not-in-loader-path.

## ADR-053 — bar = behavioral parity with c64re; verbatim cores done (drive+C64-CPU+VIC-II)
User decision: the bar is BEHAVIORAL parity with c64re (behaves like VICE: renders games/demos, runs
.g64 1541 copy-protection loaders, the 7-game proof gate). TRX64 must MATCH c64re, not exceed it.
Verbatim-core rebuild DONE (the structural fix for the pattern-engine approximations the audit found):
- drive_6510core.rs (verbatim VICE 6510core.c drive) — drive-boot-deep byte-exact.
- c64_6510core.rs + full_sc.rs (verbatim x64sc SC core 6510dtvcore.c) — C64 gates byte-exact.
- vic.rs (verbatim viciisc per-cycle VIC-II: cycle/BA/badline/IRQ/fetch).
Known cycle-count delta (NOT a behavior regression): iso-vic c64Cycles +1/+3 vs the c64re golden — the
verbatim CPU+VIC do the legitimate BA-steal dummy-fetch cycles real VICE does (6510dtvcore FETCH_OPCODE
under check_ba) that c64re's microcode CPU skips. VIC events identical; only the run-budget total clk
differs. Behaviorally harmless; do NOT make the VIC wrong vs VICE to match the oracle count.
Remaining for scramble: the drive ATN-IRQ cross-domain stamping (~7-cycle lag, full.rs:atn_edge_to_via1_ca1)
desyncs the $DD00 bit-bang loader — the pinned blocker (current item dd00-atn-irq).
Remaining for the 7-game gate: render.rs is still an approximation (no sprite collision $D01E/$D01F, no
grey-dot) — needed for gameplay, not for rendering a title/loader. Separate item render-verbatim.

## ADR-054 — dd00-atn-irq: ATN-service IRQ never dispatched (IK_IRQ ack bug); scramble loader now RUNS
The "~7-cycle ATN stamping lag" was a misdiagnosis. ROOT CAUSE: drive `interrupt_ack_irq`
(drive_6510core.rs:202) cleared IK_IRQ (the IRQ LEVEL bit) on every ack. VICE interrupt.h:284-289 clears
ONLY IK_IRQPEND + parks irq_pending_clk=CLOCK_MAX; IK_IRQ tracks nirq>0 and is cleared solely by
interrupt_set_irq when the last source deasserts. With two drive sources asserted (VIA2 T1 watchdog +
VIA1 CA1 ATN-edge), acking one left nirq=2 stuck, global_pending_int permanently lost IK_IRQ → the ATN
attention-service IRQ was never dispatched → drive parked in $EC12 idle, C64 held ATN at $EEAC forever.
Fix: clear only IK_IRQPEND. + a SID ADSR `rate - cycle_accum` underflow → saturating_sub (sid.rs) once the
loader programs the SID. Both VICE-faithful, zero regression (drive-boot-deep + all byte-exact gates GREEN).
RESULT (behavioral, visible): the custom $DD00 loader now RUNS — drive uploads its fast-loader to $0400-06CA,
VIC enters multicolor bitmap, the title LAYOUT renders (border/bg $FC = c64re). From BASIC-fallback to the
title structure. NOT yet pixel-clean: the bulk bitmap DATA from the custom 2-bit $DD00 fast transfer is
corrupt (garbled bitmap, correct structure). NEXT BLOCKER (dd00-fast-transfer): the sub-cycle CLK/DATA
sampling timing in the 2-bit fast loader — bytes wrong while drive($0402-08/$07xx) + C64($04xx/$94xx) run in
lockstep. Reference traces saved: traces/scramble_run_ref.duckdb (c64re), traces/scramble_ref_screen1.png.

## ADR-055 — fast-transfer root cause = cross-domain IEC coupling granularity (the LAST approximation)
The scramble custom 2-bit $DD00 fast-transfer delivers corrupt bytes. Diagnosed to the exact first divergence:
the C64 pc/a/x/y stream is byte-identical to the c64re reference for 323,841 instructions, then diverges in
the KERNAL serial debounce $EEA9 (LDA $DD00 / CMP $DD00 / BNE — loop until two reads agree): the reference
takes 2 iterations (sees a sub-cycle line transition between the two reads), TRX64 takes 1 (the drive
asserted DATA one protocol-step early). The drive runs ahead; later the custom $04xx bit-bang transfer
($04E2 BIT $DD00 / BVC, where timing IS the data) corrupts -> noise bitmap.
ROOT CAUSE (the last "so ähnlich" layer, NOW that drive/C64-CPU/VIC are verbatim): the cross-domain IEC
coupling. TRX64 couples via BATCH-SNAPSHOT (full.rs iec_catch_up_to freezes iec_drv_port for the whole drive
batch; lib.rs:893 per-instruction drive catch-up; the C64 frozen during the drive batch + vice-versa), so
neither debounce loop sees a mid-batch line transition. VICE keeps ONE shared iecbus (iecbus.c
iecbus_cpu_read_conf1 -> drive_cpu_execute_all; via1d1541.c store_prb updates cpu_port at the store's EXACT
drive-clock), cycle-stepped, so two closely-spaced reads STRADDLE a transition + the debounce loops.
THE FIX (foundational, NOT surgical): finer cross-domain interleaving — cycle-stepped drive<->C64 around IEC
accesses (re-fold the shared bus per drive cycle in the handshake windows), matching VICE's single-iecbus
sub-cycle transition timing. Touches lib.rs/full.rs/drive.rs (the cross-domain run loop) + the byte-exact gate
paths — risky. Tested off-by-N read-path phase shifts (+1/-1): divergence index unchanged (not a fixed offset).
ESCALATED to user (foundational + risky decision). Diagnostic probe merged (dd00_fast_probe.rs). All gates GREEN.

## ADR-056 — render-verbatim: sprite collision $D01E/$D01F + collision IRQ (behaviorally-critical gap closed)
The render audit (ADR-052/053 era) found $D01E (sprite-sprite) + $D01F (sprite-bg) collision registers were
NEVER computed — games poll these for hit-detection, so gameplay broke (7-game-gate blocker). Now ported
verbatim: render::render_collisions (vicii-draw-cycle.ts:444-482 / VICE vicii-sprites.c sprline[]) builds the
per-pixel sprite-opacity mask + sets sprite-sprite (2+ sprites overlap) / sprite-bg (fg graphics px&0x2 under
a sprite); vic::apply_collisions (vicii-cycle.c:407-433) ORs into the sticky latches + fires the collision IRQ
on the 0->nonzero edge ($D019 bit2=ss / bit1=sb -> IRQ when unmasked $D01A); $D01E/$D01F read-clear
(vicii-mem.c). + Invalid VIC modes now keep the fg/priority mask (vicii-draw-cycle.ts:189-280: black output
unchanged, fg side-plane gains bits so sprites clip/collide vs illegal-mode graphics). cargo test 90 passed
(+5), render+iso-vic+boot byte/pixel-exact GREEN (collisions are read-only side state). full.rs/lib.rs touched
only for the $D01E/$D01F read-routing (recompute_collisions) — NOT the escalated cross-domain coupling.
BEHAVIORAL ADAPTATION (noted): TRX64's static frame-render recomputes the collision masks on $D01E/$D01F read
(playing vicii_draw_cycle's role) instead of the per-cycle pixel pipeline — collision VALUES are correct +
read-clear is real; the full per-cycle pixel pipeline is a future refinement only if a game needs mid-frame
partial-collision timing.
STATUS: render gameplay-critical gap closed. The ONE remaining blocker = the escalated cross-domain IEC
coupling (dd00-fast-transfer / ADR-055) for the scramble custom $DD00 loader — awaiting user go.

## ADR-058 — 1:1 port works: viacore.ts → viacore.rs (VIA2); scramble advanced end4→end6
USER DIRECTIVE (the lesson, same as CPU/VIC): the c64re emulator only got correct when the VICE classes were
rebuilt 1:1; TRX64's drive layer is DISTILLED (drive.rs/rotation.rs/iec.rs/full.rs = 2553 lines) vs the TS
classes (viacore/via1d/via2d/rotation/iecbus/drivecpu = 7719 lines). Port the TS drive classes 1:1, no
adaptation/type-changes/adapted-fns. First: viacore.rs (1881 lines) = 1:1 viacore.ts + alarm-context.ts +
via2d.ts (every viacore_* fn verbatim with ts: tags, the AlarmContext the distilled version LACKED, the
Via2dBackend store/read/stepper/motor/SYNC). VIA2 wired to it; the distilled Via6522 VIA2 path + adapted
helpers (via2_ports_live / via2_store_prb_effects / via2_store_pcr_effects / via2_pb_output) DELETED. VIA1
stays on the old path this step.
RESULT: all byte-exact gates GREEN (drive-boot-deep byte-exact = the VIA2 timer/IFR watchdog IRQ faithful),
90 tests. And scramble ADVANCED — scramble-load-progress first divergence moved end4 ($AE 0 vs 4) → end6 (31
vs 33), 2 checkpoints further; end-state still byte-exact (scramble-load-file GREEN). The 1:1 approach is
confirmed: the distilled drive layer was the cause, the 1:1 ports fix it incrementally.
NEXT (continue the 1:1 port of the remaining distilled classes): rotation.ts (rotation.rs 472 vs TS 928 — the
disk byte-ready/SYNC the custom loader polls), then via1d1541.ts (VIA1 → viacore), iecbus.ts (iec.rs 171 vs TS
899), drivecpu.ts (the cross-domain catch-up — no Rust 1:1 counterpart yet). Each byte-exact validated, each
advances scramble. (Pre-existing REDs unchanged: iso-vic-badline-irq/-sprites/-probe = VIC-cycle gaps vic.rs;
a clippy error in sid.rs is pre-existing.)

## ADR-059 — rotation.ts → rotation.rs 1:1 (byte-exact; scramble unchanged end6 → divergence is IEC-side)
Continued the 1:1 port of the distilled drive classes. rotation.rs rewritten as a 1:1 port of rotation.ts
(full VICE rotation.c engine, ~committed 53ac4c3). Byte-exact: drive-boot-deep + disk-read-byteexact GREEN.
But scramble-load-progress UNCHANGED at end6 (31 vs 33) — so the distilled rotation was already behaviorally
~equivalent; the end6 divergence is NOT in the disk-read/rotation but in the IEC-handshake side. Merged anyway
(it is now a faithful 1:1 port, per the directive; removes a distilled approximation). NEXT: iecbus.ts (iec.rs
171 vs TS 899 — the MOST distilled, the C64<->drive IEC wired-AND bus) — the likely end6 blocker; then
via1d1541.ts (VIA1 → viacore) + drivecpu.ts (the cross-domain catch-up, no Rust 1:1 counterpart yet).

## ADR-060 — iecbus.ts → iec.rs 1:1 (byte-exact, 2 bugs fixed); end6 is VIA1 (still distilled)
iec.rs 171→903, strict 1:1 port of iecbus.ts (full 16-unit iecbus_t + the conf0..3 per-drive-type callbacks
+ ATN-edge + wired-AND fold) + c64iec.ts glue. 2 real bugs the byte-exact gates caught: (1) IecCore::new must
set TRUEDRIVE + DRIVETYPE for unit 8 (else device nibble 8=NONE -> dead Conf0); (2) fold_drv_port must invert
~pb_out before the drv_bus formula (wrong on 40960/65536 inputs). All byte-exact gates GREEN + 91 tests.
But scramble UNCHANGED at end6 — the builder PROVED (0-diff per-call + byte-identical boot+LOAD IEC fingerprint
vs the distilled baseline) the distilled IEC bus was ALREADY faithful. So end6 is NOT in iecbus/c64iec.
NEXT: the drive's VIA1 is still the DISTILLED Via6522 (only VIA2 was 1:1-ported to viacore). VIA1 owns the
ATN-CA1 IRQ entry + the PB IEC bit-bang timing the custom-loader handshake depends on — the likely end6 blocker.
Port via1d1541.ts -> a viacore-backed VIA1 (like VIA2). Then drivecpu.ts (the cross-domain catch-up) if needed.

## ADR-061 — via1d1541.ts → 1:1 VIA1 (viacore-backed): SCRAMBLE CUSTOM LOADER WORKS, title renders
VIA1 was the scramble blocker (it was the last distilled Via6522; owns the ATN-CA1 IRQ + the PB CLK/DATA/ATN
IEC bit-bang the custom $DD00 loader's handshake depends on). 1:1-ported via1d1541.ts -> Via1dBackend on the
merged 1:1 viacore (store/read PRB IEC fold, read_prb tmp=(drv_port^0x85)|0x1a|driveid, CA1=ATN IRQ via
viacore_signal, ATN-acknowledge). Deleted the distilled Via6522 (VIA1+VIA2) + Via2Ports + signal_ca1 +
iec::fold_drv_port shim — both VIAs now on the 1:1 viacore. RESULT (THE BREAKTHROUGH):
- scramble-load-progress moved PAST end6: end6 [31,11] EXACT (was stuck 31 vs 33), end7 [179,12] EXACT, end8
  [65,14] EXACT. (Residual: end5 [132,9] vs [131,9] off-by-1 — "mid-block sample phase".)
- dd00_loader_bar_probe: BAR CLIMBED to $7E (was stuck $20), drive escapes the $0402 spin (8.1% vs ~100%).
- VISUAL: scramble renders the SCRAMBLE INFINITY title (~90% clean — spaceship/badge/v1.2/artwork all render;
  a few residual garbled blocks = the end5 off-by-1). From complete noise -> the title.
All byte-exact gates GREEN, 91 tests. The 1:1-port-the-distilled-classes directive (viacore/rotation/iecbus/
via1) cracked the custom loader. NEXT: the residual end5 off-by-1 — likely drivecpu.ts (the cross-domain
catch-up, still distilled in full.rs/drive.rs catch_up_to) OR a 1-cycle sample-phase. Then the 7-game gate.

## ADR-062 — scramble sprites fixed: render_sprites missing VIC bank base; TITLE RENDERS CLEAN
User observed the residual garbled blocks were SPRITES. Verdict: a RENDER bug, not loader data. render.rs
render_sprites read the sprite-data block WITHOUT the VIC bank base — `data_base = ptr*64` instead of
`bank_base + ptr*64`. scramble runs in VIC bank 3 ($C000); pointer $48 → VIC reads $C000+$48*64=$D200 (clean
MC sprite), but render.rs read bank-0 $1200 (junk) → garbled SCRAMBLE logo + center sprites. The bitmap
rendered fine (its base already included the bank). Fix (render.rs:411): data_base = inp.bank_base + ptr*64,
exactly like screen_base/char_base/bitmap_base. Citation: c64re vicii-fetch.ts (fetch_phi2 + vbank_phi2 =
CIA2-derived bank) / VICE vicii.vbank. The MC 2-bit→colour mapping was already correct.
RESULT: scramble TITLE RENDERS CLEAN — 96.64% pixel-match overall, bitmap region 99.68% identical, sprites
clean. (Residual ~15% confined to the animated top logo band = $D011-per-frame raster-split animation phase,
a static-single-frame-render limitation, NOT a bug — compared frozen in the $D011=$3B bitmap phase.) Fix is a
no-op for bank 0 (collision/sprite gates unaffected). 91 tests, render gates GREEN.
THE SCRAMBLE CUSTOM $DD00 LOADER ACID TEST IS DONE: the verbatim cores (drive/C64-CPU/VIC) + the 1:1 drive-class
ports (viacore/rotation/iecbus/via1) + the ATN-IRQ dispatch fix + this sprite bank fix = the custom loader
loads + RUNs + renders the clean title. NEXT (user plan): the 7-game gate (diverse .g64/GCR/half-track
loaders — rotation 1:1 added the GCR engine the distilled lacked), then a Rust-vs-TS performance compare.

## ADR-063 — G64 mounting: GcrImage::from_g64 1:1 (GCR/half-track images load + read)
7-game-gate prerequisite. .g64 mounting was a STUB (only from_d64). 1:1-ported the VICE G64 loader
(fsimage_gcr.ts + driveimage.ts + VICE fsimage-gcr.c): fsimage_read_gcr_image, fsimage_gcr_read_half_track,
fsimage_gcr_seek_half_track (GCR-1541 magic, header[9]=num_half_tracks, the 84-slot offset table at
12+(ht-2)*4, the speed-map), util_le_buf_to_*, disk_image_raw_track_size — verbatim names/offsets into
gcr.rs::from_g64; wired drive.rs attach_disk for DiskKind::G64. Half-track slot indexing byte-identical to
from_d64 so the 1:1 rotation_1541_gcr engine reads it unchanged. from_d64 NOT touched.
RESULT: all 20 sample .g64 mount (84 half-tracks); motm.g64 GCR read PROVEN (drive finds SYNC, ~45K bytes,
1.74M cyc in the DOS GCR read loop + custom RAM loader, seeks T18->T26 — not the pre-port sync-never-found).
Matches c64re behavior (motm via standard LOAD"*" streams custom GCR screen-off on BOTH — needs its protected
loader for the title). D64 byte-exact gates ALL GREEN (from_d64 untouched), 95 tests, clippy-clean. Write-back
(fsimage_gcr_write_half_track) intentionally not ported (read-only mount, out of scope).
NEXT: the 7-game gate — run each game (canary criterion: LOAD"*",8,1+RUN -> PC sustains a game-code RAM
address = loader ran + game running) on TRX64 vs c64re. Then the Rust-vs-TS perf compare (user plan).

## ADR-064 — 7-GAME GATE: 7/7 BEHAVIORAL PARITY vs c64re (real software runs)
The c64re proof gate, now run on TRX64. RESULT: 7/7 PASS — TRX64 runs real disk games with the SAME outcome
as c64re. scramble (KRILL fastloader .d64), polarbear (custom .d64), motm/green_beret/impossible_mission_ii/
last_ninja_remix (protected GCR .g64), maniac_mansion (.g64, slow standard-serial — reaches game code $80C1;
c64re ALSO still in the $EE serial loop at 36.5M cyc = exact parity). 6/7 render full title art; every .g64:
drive read_gcr=true/sync_found=true/head_advanced=true (the 1:1 viacore drive reads real GCR + half-tracks).
California Games EXCLUDED (the .g64 dump lacks the EPYX protection track → fails on ANY accurate emulator incl
c64re — not a valid datapoint). Slow-loader cases (lastninja/maniac) are NOT bugs: the first file comes over
the standard-KERNAL serial path (slow); c64re does the identical thing — same loop, same reason. NO emulator
changes needed: the verbatim cores + 1:1 drive-class ports (viacore/rotation/iecbus/via1) + G64 mounting + the
sprite-bank fix all hold on real software. Harness: tests/seven_game_gate.rs (test-only/additive). All
byte-exact gates GREEN, cargo test 95 passed.
THE BEHAVIORAL BAR IS MET: TRX64 = c64re on the 7-game proof set. NEXT (user plan): Rust-vs-TS performance
compare (core cycles/sec, same fixed workload, both release, clean CPU, median K runs + the per-game gate
wall-clock).

## ADR-065 — perf compare: TRX64 Rust core ~8-10× the c64re TS core (production path)
Rust-vs-TS core throughput, both run AS SHIPPED. CRITICAL methodology fix (user caught it): c64re ships as
COMPILED dist/ run by plain `node` (ui.sh -> npm run workspace -> tsc->dist -> node scripts/workspace.mjs), NOT
tsx. A first pass measured c64re via `npx tsx` and got 0.061 MHz -> a BOGUS ~200× ratio. tsx is ~22× slower
than node-on-dist (esbuild on-the-fly transpile defeats V8 tier-up). The live UI plays smoothly at ~1.4× real-
time (50fps), which 0.06× real-time could never do — that contradiction exposed the tsx artifact.
REAL numbers (node on dist/ vs Rust --release, M4, CPU-clean, same verbatim true-drive path):
- pure headless: c64re 1.372 MHz (1.39× RT) vs TRX64 13.435 MHz (13.6× RT) = 9.8×.
- full-system disk (scramble load): c64re 1.388 MHz (1.41× RT) vs TRX64 11.179 MHz (11.3× RT) = 8.1×.
HEADLINE: TRX64 Rust core is ~8-10× the c64re TS core (an order of magnitude) — NOT 200×. Both run the same
verbatim per-cycle x64sc + per-cycle VIC + cycle-stepped drive; the win is native/no-GC/monomorphic on the
identical algorithm. Report: docs/perf-compare.md. Bench: tests/perf_bench.rs (Rust) + bench/c64re_dist_bench.mjs
(node-on-dist — NEVER tsx). All byte-exact gates GREEN (bench is test/doc-only). LESSON: benchmark c64re only
via node on dist/, never tsx.
