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
