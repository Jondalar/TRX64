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
