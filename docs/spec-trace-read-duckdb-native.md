# Spec — Native (Rust) trace/read DuckDB layer

**Status:** PROPOSED (deferred). Interim = the Node sidecar (see "Interim A" below), which
ships now so the core is testable in the real Wasteland project. This spec is the
**standalone path B**.

**Audit ids covered:** ws-trace-monitor-misc-0 (trace/read NOT_IMPLEMENTED), -1 (no index
built at trace stop), -14 (monitor map/taint/swimlane/chis), and the 5 trace-backed
`runtime/call` AgentQueryApi methods (queryEvents, followPath, swimlaneSlice, traceTaint,
profileLoader).

## Why this matters (not just a c64re feature)
TRX64 must perspectively run inside a **standalone app** (e.g. a Swift host embedding
`trx64-core` via FFI) with **no Node and no c64re TS source present**. `.c64re` (snapshot)
and `.c64retrace` (trace) are the **two interchange formats** used to switch a machine
between instances (daemon ↔ standalone app ↔ oracle). The trace-analysis surface
(`trace/read` + the v2 ops) must therefore be servable by the Rust daemon ALONE — the Node
sidecar (Interim A) is not available in a standalone deployment.

## Architecture (three layers — only layer 1 is runtime-core)
1. **`.c64retrace`** = binary trace, the SOURCE OF TRUTH. TRX64 already writes it correctly
   (oracle byte-diffs it green; Spec 726.B). The live per-frame firehose drain under
   `--stream` is wired (audit background-workers-async-5). **No change needed here.**
2. **`trace.duckdb`** = a DERIVED, rebuildable INDEX over the `.c64retrace`
   (`CREATE TABLE` + `INSERT` of cpu_step / mem_read / mem_write / anchors / marks …).
   Built OFF the hot path (TS uses a worker; Rust uses a `std::thread`/rayon worker), lazily
   on first `trace/read` (TS `ensureIndexBounded`).
3. **Reader ops** = the queries the WS surface exposes (pure functions over the duckdb):
   - store_fn: getInfo, topPcs, findBusEvents, listAnchors, findAnchor, safeQuery (raw SQL)
   - direct: swimlane, query_events, follow_path, taint, profile_loader, sql

## Interim A — Node sidecar (ships now, NOT this spec)
TRX64's `trace/read` handler shells out to a small Node CLI in `tools/` that imports the
EXISTING c64re indexer (`runtime-trace-sink.ts`) + the `v2/` reader ops (they are pure
functions of file paths). TRX64 already writes the `.c64retrace` (the shared contract), so
the sidecar is byte-identical to TS by construction. Requires Node + the c64re TS source on
disk → fine for TRX64-as-c64re-backend, NOT for standalone. This unblocks the audit ids
above for the c64re-MCP use case immediately. This spec (B) replaces it for standalone.

## Scope of the native port (B)
1. **Dependency:** add the `duckdb` Rust crate (bundled DuckDB; vendored C lib — confirm
   build on macOS arm64 + the CI target; gate behind a feature flag if the native lib is
   heavy, so a no-trace build stays lean).
2. **Indexer:** `crates/trx64-trace` (or a new `trx64-traceindex` crate) — read the
   `.c64retrace` (the binary format trx64-trace already writes), `CREATE TABLE` + batched
   `INSERT` into the duckdb. The SCHEMA must match what the c64re `v2/duckdb-backend.ts`
   readers expect (table + column names) so a `.c64retrace` produced by either runtime
   indexes identically. Run on a worker thread; `ensureIndexBounded` semantics (build once,
   bounded size, reuse).
3. **Reader ops:** port the `v2/` query logic to Rust — getInfo/topPcs/findBusEvents/
   listAnchors/findAnchor/safeQuery (mostly SQL strings → run + shape) + swimlane/
   query_events/follow_path/taint/profile_loader (these carry real analysis ALGORITHMS in
   `src/runtime/headless/v2/{swimlane,query-events,follow-path,taint,profile-loader}.ts` —
   port faithfully). `sql`/`safeQuery` = a guarded raw query.
4. **Wire:** `trace/read` (main.rs, currently -32601) + the 5 trace-backed `runtime/call`
   methods + the monitor `map`/`taint`/`swimlane`/`chis` verbs (audit misc-14) all route to
   the native reader. `finalize_trace` keeps writing the `.c64retrace`; the duckdb is built
   lazily by the reader (or eagerly on a worker at stop — match the TS `startBackgroundIndex`
   timing if a consumer relies on `wait_index`).

## Acceptance (same gate, native this time)
- The differential conformance cases for misc-0/1/14 + the 5 runtime/call methods go GREEN
  against the TS authority — with NO Node sidecar present (prove standalone).
- A `.c64retrace` produced by TRX64 and one produced by the TS runtime index to byte/row-
  equal duckdb tables (cross-runtime index parity).
- Reader op outputs match the c64re `v2/` outputs (the differential gate enforces this).
- `npm run conformance` stays green; oracle no-disk subset unaffected (analysis is post-
  capture, decoupled from the emu core).

## Risk / notes
- The `duckdb` native dependency is the main weight (build + binary size). Feature-flag it.
- The v2 analysis algorithms (swimlane/taint/follow_path/profile_loader) are the real port
  effort — port + differential-test each against its TS module.
- Estimated multi-day. Sequence after the smaller deferred items (RewindManager wiring,
  resolvePc/diffSnapshots) unless standalone is needed sooner.
