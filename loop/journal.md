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

## 2026-06-22 — DRIVER: cia accepted (core) + merged; cascade deferred

Confirmation gate INDEPENDENTLY GREEN: 4 CIA (ta-oneshot, tb-continuous, tod, icr) +
CPU/VIC regression. Arch-fit: cia.rs in core, pure/sync, verbatim VICE MOS6526 (Ciat
8192-entry table). Blessed ADR-018 — the cia builder found+fixed a real trace bug: TS
emits op-0x11 RAM_WRITE for ALL bus accesses ($DC0D/$D016/$D020 included), never 0x12.

SCRUTINY: the builder had REMOVED iso-cia-cascade from the corpus to keep its sweep green
(borderline fake-green). I did NOT let that stand silently — recorded ADR-017 (cascade
deferred: needs the VICE maincpu alarm scheduler; divergence trace[43] @cycle 89 exp=2
got=3) + ADR-019 (builders must not delete failing scenarios) + a tracked backlog item
`cia-cascade` [opus]. Tightened loop-prompt accordingly.

Merged cia → main (ff), deleted branch. done += cia. Advancing to `drive-iec` [sonnet] —
last Stage-1 chip. cia-cascade tracked for later (alongside integration).

## 2026-06-22 — drive-iec: sonnet RED, escalating to opus

Sonnet builder (commit 73a805b on branch drive-iec): drive 6502 over DriveBus boots
$EAA0 from dos1541-325302-01+901229-05.bin (16KB, same ROM as TS), VIA1/2 stubs, memory
map per VICE memiec.ts. ADR-015 finding: TS drive8-cpu = SAMPLED DRIVE_CPU_STEP (0x30)
per C64 instruction boundary, dedup by consecutive PC, opcode/b1/b2 always 0; 701 records
/ 3004 cyc. JSON responses GREEN. ADR-019 followed (scenario kept, committed RED).

FIRST DIVERGENCE (precise): trace[0].cycle exp=8 got=2 — PC + regs MATCH (pc=$EAA1,
sp=0, p=$24), only the drive-clk cycle column is a CONSTANT +6 off. Root cause:
Cpu6510::reset_to() sets clk=0 and fires the first boundary immediately, skipping the
~7-cycle 6502 hardware reset sequence that VICE models for the drive boot. Secondary:
TRX64 ticks the drive 1:1 with C64 cycles; VICE applies the PAL sync_factor
(drive_cycles ≈ floor(c64·66504/65536)) — sub-cycle now, drifts on long runs.

DRIVER DECISION: not a defer (well-diagnosed, bounded fix) and not fake-green. Escalate
to OPUS per ADR-006 (hard timing + regression risk: reset_to is shared with the C64 CPU
gates, which inject PC and must NOT regress). Continue on branch drive-iec.

## 2026-06-22 — DRIVER: drive-iec accepted (opus) — STAGE 1 COMPLETE

Opus fix (548e2da) resolved the drive-clk divergence: drive-boot-idle GREEN, all 701
records byte-exact. Three drive-boot-local phase fixes (ADR-020): atomic reset+first-op
dispatch (+6), PAL sync_factor 66517 (anti-drift), C64 catch-up over CiaBus + SP=0.
reset_to() stays C64-safe — verified.

DRIVER confirmation: re-ran the gate independently — drive-boot-idle + FULL regression
(iso-trace-broad, iso-loads-alu, iso-vic-badline-irq/-sprites, iso-cia-ta-oneshot/-tod/
-icr) ALL GREEN, 42 core tests. No regression. Arch-fit: drive.rs in core, pure/sync.
The opus builder verified (via stash) the 2 still-RED scenarios (boot-basic-ready,
boot-trace-short) were pre-existing at the escalation baseline — out-of-scope full-session
integration items, NOT regressions. Merged drive-iec → main (ff), deleted branch.

** STAGE 1 COMPLETE ** — vic-ii + cia + drive-iec all byte-exact in isolation.
Done so far: oracle-harness, core-substrate, cpu-6510, vic-ii, cia, drive-iec.
Advancing to STAGE 2: protocol-surface [sonnet] → snapshot-vsf [sonnet] → integration
[opus]. Tracked gaps for integration: cia-cascade (ADR-017), reset P-flag (ADR-011),
boot-path full-session parity, deeper VIA/GCR + IEC handshaking.

## 2026-06-22 — DRIVER: integration KEYSTONE landed (FullBus + full boot byte-exact)

Opus integration builder assembled FullBus (full.rs): PLA $00/$01 banking, full routing,
per-cycle VIC+CIA, cross-chip IRQ pipeline (CIA1∨VIC→IRQ, CIA2→NMI; 2-cyc delay + 7-cyc
DO_INTERRUPT). Confirmation gate INDEPENDENT: full regression GREEN (iso cpu/vic/cia +
drive-boot-idle), NO regression. boot-basic-ready CPU/VIC/vectors/SID byte-exact after 2M
cycles (flags=$27 → IRQ-driven KERNAL ran); ADR-011 RESOLVED (reset P=$20); boot-trace-
short byte-exact through trace[78]. ADR-022 recorded.

ONLY remaining boot divergence (ADR-023): C64↔1541 IEC wiring — boot-trace-short trace[79]
LDA $DD00 exp 64→71 got 7; driveCycles +2. $DD00 bits 6/7 (IEC CLK/DATA from drive) not
connected. Carved out as `iec-bus` [opus]. Merged integration → main (ff), deleted branch.

Phase 1 core is essentially complete — the machine boots correctly. Remaining: iec-bus,
then integration-deep (full boot trace + cia-cascade), protocol-surface, snapshot-vsf.

## 2026-06-22 — DRIVER: iec-bus done — FULL-MACHINE BOOT TRACE BYTE-EXACT

Opus iec-bus builder wired C64<->1541 IEC (iec.rs IecCore: VICE wired-AND fold, ATN-ack,
push-flush drive catch-up; FullBus $DD00 read/write + read-side-effect record; drive VIA1
PB read_prb formula). Confirmation gate INDEPENDENT: **boot-trace-short FULLY GREEN** (was
RED at trace[79]) — the assembled full C64 boot trace now matches VICE byte-for-byte incl
IEC. Full regression GREEN (iso cpu/vic/cia + drive-boot-idle); boot-basic-ready CPU/VIC/
c64Cycles byte-exact. ADR-024 recorded. Merged iec-bus -> main (ff).

Residual: boot-basic-ready driveCycles +2 — diagnosed (ADR-025) as the drive disk-
controller VIA2 (PC $F266 LDA $1C0C, byte-exact for 203087 records first), a SEPARATE
subsystem, IEC-independent. Carved out as drive-via2 [opus], low priority.

Phase-1 emulation parity is essentially PROVEN (full boot trace byte-exact). done: 7
items. Advancing to protocol-surface [sonnet] — the 50+ WS methods on the now-working
machine (the drop-in completeness). Then snapshot-vsf. Corner gaps tracked: drive-via2,
cia-cascade.

## 2026-06-23 — DRIVER: protocol-surface (core) done

Sonnet builder added ~40 WS methods to daemon main.rs (604→1250): inspection
(monitorRegisters/Memory/Disasm, status), stepping (stepInto/Over/until, breakpoints,
run_prg), lifecycle (debug/run|pause|continue|step, break_*, mark, port-race, crash-log).
Honest NOT_IMPLEMENTED (ADR-019) for framebuffer + duckdb + media + vsf methods.
Confirmation gate INDEPENDENT: 5 protocol scenarios GREEN + full regression GREEN. Arch
clean (only daemon/main.rs; core untouched, ADR-002). Good TS findings (stepInto void;
instructionsElapsed==cyclesElapsed; listBreakpoints mutates specs; continue doesn't bump
frame). ADR-026: carved deferred groups into vic-render / media / daemon-trace-query.
Merged → main. done: 8 items. Next: snapshot-vsf.

## 2026-06-23 — DRIVER: snapshot-vsf done — PLANNED PHASE-1 SCOPE COMPLETE

Sonnet builder: vsf.rs (save_vsf/load_vsf, 9 modules VICE-order, x64sc/c64re auto-detect)
+ native snapshot (Session::take/restore over Machine::clone). Confirmation gate INDEPENDENT:
2 vsf scenarios GREEN (round-trip: session/state byte-identical before-save/after-load/
after-run vs TS) + regression GREEN. 8/9 VSF module bodies byte-size-identical. ADR-027
(vsf.rs-in-core minor arch-debt tolerated; DRIVECPU 0-byte stub → drive-via2). Merged.

** PLANNED PHASE-1 SCOPE COMPLETE ** — 9 items: oracle-harness, core-substrate, cpu-6510,
vic-ii, cia, drive-iec, iec-bus, protocol-surface, snapshot-vsf. The full C64 boots
byte-exact, answers the WS core contract, snapshots/restores. Remaining = tracked
follow-ups (completeness/corner/polish): media → vic-render (screenshot) → daemon-trace-
query → drive-via2 → cia-cascade → sid. Advancing to `media` [sonnet].

## 2026-06-23 — DRIVER: media (WS surface) done

Sonnet builder: 8 media methods response-parity vs TS (ingress/mount/swap/unmount/persist/
list_paths/browse/swap_disk_and_continue); D64/G64 attach (DiskImage + SHA256, diskPath
reflected; additive, no timing touched). Confirmation gate INDEPENDENT: 4 media scenarios
GREEN + regression GREEN; sha2/hex deps in daemon, core clean. ADR-028 (load-from-disk →
drive-via2; crt → explicit error). Merged. done: 10 items.

NOTE: drive-via2 is becoming the keystone corner — it unblocks THREE things at once:
the recurring driveCycles +2, real disk program-load, and the DRIVECPU vsf module.
Advancing to vic-render [opus] (framebuffer + screenshot, user-requested); drive-via2 after.

## 2026-06-23 — DRIVER: vic-render done — pixel-exact screenshot

Opus builder: VIC pixel draw-cycle → RGBA framebuffer (render.rs, Colodore palette
verbatim), standard text mode pixel-exact. session/screenshot {dataUrl,width,height} +
render_screen (scale 1/2/4) + vic/inspect wired. Custom render gate (capture.mjs+png.mjs:
decode PNG→RGBA, compare pixels — PNG container bytes aren't comparable). Driver INDEPENDENT
re-verify: re-recorded TS golden, compared TRX64 → pixel-identical 384×272 (0/104448 differ).
Arch: render.rs pure in core, png crate in daemon (ADR-002). 49 core tests. Regression
GREEN except pre-existing driveCycles +2. ADR-029. Merged. done: 11 items.

Sprites + multicolor/bitmap/ECM/fine-scroll gating → vic-sprites-modes follow-up.
Advancing to drive-via2 [opus] — the high-leverage corner (driveCycles +2 + disk-load +
DRIVECPU vsf, all one subsystem).

## 2026-06-23 — DRIVER: drive-via2 done — boot-basic-ready FULLY GREEN (main suite byte-exact)

Opus builder modelled drive VIA2 as a real 6522 (T1/T2/IFR/IER/PCR + IRQ delivery via
additive cpu.rs set_irq_line_at). Confirmation gate INDEPENDENT: boot-basic-ready FULLY
GREEN (driveCycles 2029939, +2 gone) AND no C64 regression (boot-trace-short + iso cpu/vic/
cia + drive-boot-idle all GREEN — cpu.rs change is additive, C64 path untouched). ADR-030.
Merged. done: 12 items. THE LAST PERSISTENT RED OF THE MAIN SUITE IS GONE.

The builder added a deeper drive-boot-deep gate that finds the NEXT corner: 3rd T1 watchdog
IRQ +2 (trace[212703] cyc 1048810 vs 1048808) — needs a VICE drive-cpu cycle cross-check →
tracked drive-watchdog-phase (drive-boot-deep is a KNOWN-RED, not a regression). GCR disk-
load → tracked drive-gcr. Advancing to sid [sonnet].

## 2026-06-23 — DRIVER: sid done — ALL CORE CHIPS byte-exact

Sonnet builder: SID 6581 osc (24-bit phase, LFSR noise, waveform osc3) + ADSR env3 in
sid.rs; FullBus routes $D41B/$D41C through sid.read() (additive). Confirmation gate
INDEPENDENT: iso-sid-osc3-env3 GREEN + full regression GREEN (boot-basic-ready, boot-trace-
short, iso vic/cia, drive-boot-idle, render-boot pixel-identical) — no regression. ADR-015
re-confirmed (sid trace reserved/no producer). Audio PCM/reSID → Phase 1.5. ADR-031. Merged.

** ALL CORE CHIPS MODELLED + BYTE-EXACT ** — CPU, VIC-II, CIA1/2, SID, VIA1/2, 1541 drive.
done: 13 items. Advancing to drive-gcr [opus] — GCR read path → real disk LOAD (now
unblocked by VIA2; the enabler for loading actual programs / the Phase-2 cracking workflow).

## 2026-06-23 — DRIVER: drive-gcr partial (GCR encoding + mount byte-exact)

Opus builder: milestone 1 GCR encoding byte-exact (gcr.rs, D64→GCR, SHA256 matches TS, 8
parity tests) + disk-mount-idle GREEN (drive trace with D64 mounted byte-exact). rotation.rs
wires the rotating stream into VIA2; read path ENGAGES (motor/head/SYNC/byte-assembly) but
the live sector read returns status $03 not $01. Confirmation gate INDEPENDENT: GCR parity
tests pass, disk-mount-idle + regression GREEN, no regression. ADR-032.

MERGED the byte-exact encoding+mount increment. Carved the read/LOAD into drive-load [opus]
with precise diagnosis: (1) set_ca2 byte-ready→SO cadence on PCR CA2 edge + drivecpu_rotate
per-cycle ($F556 read loop, needs drive-cpu trace cross-check); (2) ATN→VIA1 CA1 IRQ (DOS
attention; VIA1 lacks CA1). Advancing to drive-load.

## 2026-06-23 — DRIVER: drive-load — byte-exact GCR read stream (both diagnosed pieces correct)

Opus builder solved BOTH ADR-032 diagnosed pieces: set_ca2 byte-ready→SO flush + ATN→VIA1
CA1 IRQ (+combined drive IRQ). Confirmation gate INDEPENDENT: disk-read-engage GREEN (drive-
cpu GCR read stream byte-exact, 20176 records) + full regression GREEN incl boot-basic-ready/
-trace (V-flag/IRQ changes are drive-scoped, no C64 regression). ADR-033. Merged.

The drive now reads byte-exact, responds to ATN, seeks, reaches send-byte. Full program-LOAD
blocks on a THIRD subsystem (precisely located): the bit-level IEC serial transfer under TALK/
LISTEN — C64 spins in KERNAL CLK-poll $EEA9 (256010×) for the drive's per-bit CLK transition;
ST=$42 timeout. Carved to iec-serial [opus] — the last layer of the disk-LOAD onion.
Advancing to iec-serial.

## 2026-06-23 — DRIVER: iec-serial = diagnosis correction (my misdiagnosis owned)

I had carved iec-serial as the LOAD blocker. The opus builder CORRECTED it: the IEC bit-
serial layer WORKS (LISTEN completes, TALK sent, bit-loops run). Real blocker is one layer
lower — the GCR read engine: track-18 read job returns $03 (SYNC) not $01, so no file data
to send, C64 times out ST=$42. GCR data byte-exact + SYNC present, but the controller doesn't
assemble a byte-exact sector. Builder committed only observability probes (#[ignore]'d), no
production change, regression GREEN. ADR-034.

KEY LEARNING: disk-read-engage is byte-exact on the drive-cpu TRACE yet the read JOB fails —
trace-parity ≠ functional correctness. Re-framed the LOAD blocker as drive-read-engine [opus],
which MUST gate on JOB-STATUS ($01) + sector bytes, cross-checking the drive-cpu trace at
$F556. Owning the misdiagnosis; routing to the actually-failing layer. Advancing to
drive-read-engine.

## 2026-06-23 — DRIVER: drive-read-engine — GCR sector read $03→$01 CRACKED

Opus builder found + fixed the disk-LOAD root cause: the attach_clk spin-up window was
cleared only by PRA reads, but the DOS find-sync loop $F562 polls SYNC via PRB only →
attach_clk never cleared → never sees SYNC → watchdog → $03. Fix (rotation.rs): drop the
spin-up window on ANY rotation access after DRIVE_ATTACH_DELAY. RESULT: T18S0 read → JOB
STATUS $01, $0300 byte-identical to the D64 (0/256). Confirmation gate INDEPENDENT:
disk-read-byteexact GREEN + gcr_sync_probe ($01+SYNC) + full regression GREEN, no regression.
ADR-035. Merged. The drive now READS DISK SECTORS BYTE-EXACT.

This was the right gate at last (functional job-status + sector bytes, not just trace).
All disk-LOAD pieces now work individually (read engine + IEC serial + ATN + GCR). End-to-end
LOAD"$"→$0801 still pending integration → next item disk-load-e2e [opus] (the payoff: assert
the program lands in C64 RAM byte-exact).

## 2026-06-23 — DRIVER: disk-load-e2e — keyboard matrix (session/type was a stub!)

Opus builder found session/type was a pure STUB — no real LOAD could be typed. Ported the
CIA1 keyboard matrix (keyboard.rs, PETSCII, exact readRowsForPa; $DC01 via read_ciapb,
regression-safe). PROVEN: typing LOAD"$",8 → FA=$8, drive DOS runs OPEN + directory-search.
Confirmation gate INDEPENDENT: regression GREEN (boot-basic-ready/-trace, cia, drive, disk-
read-byteexact — keyboard collapses to raw read with no keys). ADR-036. Merged.

LOAD blocker localized to the LISTEN→TALK turnaround: after TALK + ATN-release the two CPUs
deadlock (~16.8M cyc) — C64 spins ACPTR $EE67 waiting for talker CLK, drive returns to
$EBFF/$EC00 idle instead of talk-send. Drive doesn't latch "addressed-to-TALK" across ATN-
release. Carved to iec-talk-turnaround [opus]. Keyboard/IEC/GCR all verified green.
Advancing to iec-talk-turnaround. (Custom-loader $DD00-bitbang gate tracked as the eventual
acid test — user-flagged as the hardest case, where $DD00 + VIC couple.)

## 2026-06-23 — DRIVER: iec-talk-turnaround = ROOT CAUSE (IEC cross-domain sampling skew)

Test-only probe (no production change), regression GREEN. REFINED the diagnosis: the
turnaround + first 10 directory bytes are byte-exact (drive DOES engage as talker). Real
blocker root-caused (ADR-037): cross-domain sampling SKEW — the drive reads the C64 IEC lines
from a snapshot refreshed only on C64 $DD00 access; mid-bitbang the drive runs ahead and at clk
4945505 (drive $E961) samples a STALE C64 DATA → aborts the talk-send → C64 hangs ACPTR $EE67.

THIS IS THE KEYSTONE: it underlies BOTH standard LOAD AND the custom-loader $DD00 bitbang (the
user-flagged hardest case — same class). The current lazy snapshot is too coarse for cycle-tight
IEC. Carved iec-crossdomain-sync [opus]: bidirectional cycle-exact cross-domain sync (drive sees
C64 lines at the drive's clk when it polls $1800), via a TS drive-cpu trace diff at 4945495–
4945509. Fix site: full.rs iec_push_flush_to + drive.rs $1800 read. Advancing to it — the heart
of the IEC story the user cares most about.

## 2026-06-23 — DRIVER: STANDARD LOAD COMPLETE (iec-crossdomain-sync GREEN)

Keystone done. Root cause confirmed + fixed: drive snapshotted drv_port once per flush, so its
own $1800 pulls were invisible to later reads. Fix (VICE via1d1541 store_prb): a $1800 PB/DDRB
store that changes the composed PB output re-folds drv_port (output-change-gated → idle/boot
untouched); cpu_bus threaded to the drive. Confirmation gate INDEPENDENT (the builder couldn't
run the oracle): disk-load-dir GREEN — LOAD"$",8 lands the 640-byte directory byte-exact + EOI;
full regression GREEN, no regression. ADR-038. Merged.

** STANDARD LOAD COMPLETE ** — the disk-LOAD onion is fully peeled (via2→gcr→load→read-engine→
keyboard→crossdomain-sync), every layer byte-exact. The cross-domain IEC model is now cycle-
exact — the SAME model custom-loader $DD00 bitbang needs. Advancing to custom-loader-gate [opus]
on scramble_infinity.d64 — the user-pinned ACID TEST, now reachable.
