# TRX64 Build Journal

Append-only. Each iteration writes what it did, the first-divergence (if red), and
decisions. The loop reads the tail of this for recent context after a reset.

---

## 2026-06-22 — bootstrap

- Repo scaffolded: cargo workspace, 4 crates (core/trace/session/daemon), all compile.
- Architecture locked (see README.md): swap the process behind ws://4312, not "a core".
  - core = pure/deterministic/sync, generic `Observer` (zero-cost), Clone-able for forks.
  - trace = TraceOp encoder → `.c64retrace` (immovable format = oracle).
  - Data-plane: file-only for forensic; Phase-2 search uses in-core probes, not firehose.
    No shared-memory needed (live = LLM, incremental result-push over existing WS).
- Phases hard-separated. Phase 1 = parity drop-in, verified by trace-diff vs TS oracle.
- Loop is context-stateless / disk-stateful: survives token resets, resume from state.json.
- Next: build `oracle-harness` (Stage 0, current item). Without it no builder can verify.

## 2026-06-22 — oracle-harness: mechanism proven end-to-end

Built `tools/oracle/` (TS, type:module): ws-client (JSON-RPC 2.0 text frames; binary
frames ignored — not needed for run+register+trace), diff (first-divergence, volatile-
key whitelist), scenario format with `$sessionId` threading, trace-decode (byte-exact
.c64retrace per binary-format.ts v2), oracle CLI (record/compare). Typechecks.

VALIDATED against a live TS daemon (tsx source mode, port 4399, scratch project):
- WS + sessionId threading: `boot-basic-ready` golden recorded, session/state read
  (pc=$FD7E a=$AB after 2M-cycle boot). ✓
- byte-exact trace decode: `boot-trace-short` → 23283 records, cpu+ram families,
  v2 mem frame (access=$81 write+old-present, old decoded), cycles monotonic. ✓
- compare engine: fires precise RED with first-divergence + exit 1. ✓

Exact protocol/format facts captured (see this commit's code): launch via
`tsx src/runtime/headless/daemon/run.ts --project <dir> --port N`; methods
session/create, session/run{session_id,cycles}, session/state, trace/start_domains
(returns outputPath=.duckdb; the .c64retrace is the sibling, the product authority),
trace/run/stop{wait_index:true}. Magic "C64RETR1", header + per-op fixed frames.

OPEN — determinism/isolation (blocks calling oracle-harness done): the daemon has ONE
long-lived default session; c64Cycles + absolute trace cycles accumulate across runs,
so goldens aren't reproducible on a reused daemon (compare self-test went RED on
$.create.c64Cycles). Fix before the gate is trustworthy: hermetic daemon lifecycle
(spawn fresh TS/TRX64 daemon per scenario on an ephemeral port, teardown after) OR a
clean cold-reset before each scenario. Decision pending.

## 2026-06-22 — oracle-harness: determinism gate GREEN (loop-ready)

Chose (a) hermetic. Added `daemon.ts`: spawn a fresh daemon (ts | trx64) on an
ephemeral free port with a throwaway tmp project dir, readiness-poll via WS connect +
ping, teardown = SIGTERM the process group + rm tmp. record/compare now spawn their own
daemon (explicit --endpoint/--candidate still works for debug; --candidate-kind ts for
a TS-vs-TS self-test). Volatile keys extended: outputPath, evidenceRef, createdAt,
overheadMs.

PROOF: record (fresh TS) then compare vs a SECOND fresh TS -> GREEN, exit 0. Two
independent cold-reset daemons produce byte-identical responses + 50k-cycle trace. No
daemon leaks after teardown.

=> oracle-harness MECHANISM done, gate trustworthy. Corpus is still two boot smoke
tests; it grows per-subsystem inside each builder's item (cpu builder writes cpu
exercisers + records goldens before porting, etc.). Advancing to `core-substrate`:
first point where compare-vs-trx64 actually runs (expect RED until the daemon binds +
answers ping/session/create).

## 2026-06-22 — core-substrate done + loop dry-run GREEN (mechanics proven)

Wired model routing (per-item [model:] tags, cheap-first + escalate-to-opus). Adopted
rtk (project CLAUDE.md, user-authorized) for token discipline.

DRY RUN — dispatched a `core-substrate` builder on SONNET (tests cheap routing). It
built: trx64-core Cpu + ROM load (kernal/basic/chargen) + cold_reset (pc=$FCE2 from
$FFFC); trx64-session boot(); trx64-daemon = tokio + tokio-tungstenite WS JSON-RPC 2.0
answering ping / session/create / session/run(stub) / session/state. rtk cargo build
clean (2 warnings).

GATE ran end-to-end vs the real TRX64 daemon (hermetic spawn):
  [boot-basic-ready] RED $.state-after-boot.c64Cycles: expected=2000000 got=0
=> session/create MATCHED (both cold-reset pc=64738), ping ok, precise first-divergence
on execution (correct — cpu-6510's job), no crash, no leaks. Full loop mechanic proven:
builder -> build -> daemon answers -> gate -> precise RED.

core-substrate meets its DoD. Advancing to cpu-6510 [opus]. Pre-flight complete; arming
the cron (30-min interval) next.

## 2026-06-22 — cpu-6510 DONE (opus builder, ~30 min)

Cycle-exact 6510 in trx64-core, generic over a new `Bus` trait. ALL opcode groups
byte-exact vs TS oracle: loads/stores/transfers, ALU incl BCD carry/borrow edges, RMW
(dummy-write-old), branches (taken/page-cross/not-taken timing), stack/flow, all
addressing modes incl page-cross, illegals (SLO/RLA/SRE/RRA/DCP/ISB/LAX/SAX/ANC/ALR/ARR/
AXS/SBC-imm/NOP-variants/JAM). 18 unit tests + 8 oracle gates green.

Isolation (ADR-005): inject+run both daemons identically over WS — session/create →
monitor/exec "wr <addr> <bytes>" → monitor/exec "r pc=" → session/run {cycles}. Flat 64K
RAM bus, no VIC/CIA/banking, IRQ structure inert. Empirical find: VICE power-on DRAM fill
= 64-byte alternating blocks (addr & 0x40 ? 0xFF : 0x00). CpuStep p = raw reg_p (N/Z in
caches), not composite flags().

DRIVER confirmation: re-ran the gate independently — GREEN on iso-trace-broad (1083
records byte-identical), iso-loads-alu, iso-trace-bcd-illegal. Arch-fit verified: CPU in
trx64-core (1816 LOC), no async/rand/SystemTime, Observer stays generic, daemon only
wires monitor/exec. Recorded ADR-009 (per-item branch + Driver merge), ADR-010 (Bus
trait), ADR-011 (reset P-flag deferred to integration). Merged cpu-6510 → main (ff).

OPEN (ADR-011): boot-trace-short RED trace[0].p 32 vs 36 — reset P-flag $20 vs $24,
full-boot-path only, resolve at integration. NOT a CPU defect.

Advancing to STAGE 1 (parallel): vic-ii [opus] ∥ cia [opus] ∥ drive-iec [sonnet],
worktree-isolated. Next tick dispatches the set.

## 2026-06-22 — vic-ii: cycle-exact VIC-II + badline cycle-stealing, GATE GREEN

Built cycle-exact VIC-II (6569 PAL) in trx64-core (`vic.rs`), ticked once per CPU master
cycle, gated CPU-isolated via a `VicBus` ($D000-$D3FF→VIC, flat RAM elsewhere).

DECISIVE FINDING (ADR-015): the TS oracle's `vic` trace channel has schema+encoder+decoder
+kind-codes but NO LIVE PRODUCER — nothing calls publish("vic",…). Verified empirically: a
vic-domain .c64retrace over a full PAL frame = ZERO records. So VIC parity = (1) empty vic
trace + (2) c64Cycles matching TS, which forced the real work: the badline / sprite-DMA
BA-low CPU read-stall (vicii_steal_cycles), since it shifts CPU instruction timing.

Built incrementally (raster→badline/BA→DMA→sprites):
- raster counter (63cyc/line, 312 lines PAL), reg file $D000-$D02E, $D011/$D012 9-bit raster
  IRQ compare (edge-triggered raster_irq_triggered), $D019 write-1-to-clear, IRQ line level.
- badline detection ((line&7)==ysmooth && allow_bad_lines && 0x30..0xf7), sticky
  allow_bad_lines (DEN on first_dma_line), BA-low BaFetch window raster_cycle 12..54.
- sprite DMA turn-on (cycle 55/56, Y==line, enabled), sprite-fetch BA window.
- VIC↔CPU coupling (ADR-016): Bus trait gained default-no-op tick()+check_ba_before_read();
  Cpu6510::tick→bus.tick (per master cycle), Cpu6510::load→bus.check_ba_before_read (steal).
  FlatRam keeps defaults → every CPU gate stays byte-identical. VicBus does the steal loop.
- tick() reordered to VICE vicii_cycle() exact order (raster_cycle++ FIRST, then line wrap
  at cyc 0, allow_bad_lines, badline, edge raster IRQ, sprite DMA, BA) — this fixed the last
  off-by-1/2 c64Cycles divergences.
- trx64-trace: VIC_REG_WRITE (0x20, 13 bytes) encoder + TraceChannels (domains→channels =
  TS domainsToChannels) record filter; daemon trace/start_domains stores domains, run path
  uses run_for_vic + channel filter (vic-only → empty trace; cpu/memory unchanged).

GATE GREEN: all 4 VIC corpus scenarios (corpus/vic/: iso-vic-probe, -raster, -badline-irq,
-sprites) — byte-identical responses + empty vic trace + exact c64Cycles. NO CPU regression
(7 CPU gates GREEN). 33 core unit tests pass. clippy clean (only pre-existing-pattern warns).

OPEN: none in VIC scope. boot-trace-short (P-flag, ADR-011) + boot-basic-ready (driveCycles)
remain RED — both integration/full-boot items, not VIC. Pixel draw-cycle/framebuffer
intentionally not ported (never reaches the trace; ADR-015).

## 2026-06-22 — DRIVER: vic-ii accepted + merged

Confirmation gate re-run INDEPENDENTLY: 4 VIC scenarios (iso-vic-probe/-raster/-badline-irq/
-sprites) GREEN + CPU regression (iso-trace-broad, iso-loads-alu) GREEN — no regression.
Arch-fit verified: vic.rs in trx64-core, pure/sync (no async/rand/time), Bus trait gained
default-no-op tick()+check_ba_before_read (FlatRam unaffected → CPU gates stay green; VicBus
overrides). Blessed the builder's ADR-015 (TS vic trace channel is RESERVED/no producer →
parity = empty vic trace + c64Cycles via badline BA-low stealing) and ADR-016 (VIC↔CPU
coupling via Bus hooks). Merged vic-ii → main (ff), deleted branch.

PROCESS NOTE: the vic-ii builder edited loop/ files (backlog/journal/decisions) — Driver-owned.
Content was correct so kept; tightened loop-prompt so future builders don't touch loop/.

Advancing to `cia` [opus]. stage1_remaining: [drive-iec].
