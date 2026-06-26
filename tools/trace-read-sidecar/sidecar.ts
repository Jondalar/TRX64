#!/usr/bin/env tsx
// ─────────────────────────────────────────────────────────────────────────────
// trace/read NODE SIDECAR  (Spec — "Interim A", docs/spec-trace-read-duckdb-native.md)
//
// TRX64 already WRITES the `.c64retrace` (the shared interchange format the oracle
// byte-diffs green). It does NOT yet have a native DuckDB indexer + the v2 reader
// algorithms. This sidecar closes that gap WITHOUT re-implementing anything: it
// IMPORTS the EXISTING c64re indexer (background-indexer `ensureIndexBounded`) and
// the v2 reader ops, then runs them over a `.c64retrace` / sibling `.duckdb`. By
// construction the output is byte-identical to the c64re TS daemon's `trace/read`,
// because it IS the same code path (ws-server.ts:1302-1377).
//
// This is the c64re-as-backend path (Node + the c64re TS source on disk). The
// standalone (no-Node) deployment is served by the native Rust port (spec path B).
//
// CONTRACT (CLI):
//   tsx sidecar.ts <op> --duckdb <path> [--retrace <path>] [--args <json>]
//
//   <op>  =  store_fn | swimlane | query_events | follow_path | taint
//            | profile_loader | sql
//   --duckdb   the `.duckdb` INDEX path. Built lazily from the `.c64retrace`
//              authority (the sibling, or --retrace) when absent. This IS the path
//              ensureIndexBounded()/withDuckDb() take, so the lazy-build (audit
//              misc-1, "no index at trace stop") is covered for free on first read.
//   --retrace  optional explicit `.c64retrace`. Defaults to the `.duckdb` sibling
//              (== c64re retracePathFor). When the index already exists it is unused.
//   --args     op-specific argument object, JSON. Matches the WS `args` field
//              shape ws-server.ts forwards to the reader (e.g. for store_fn:
//              {"fn":"getInfo"} or {"fn":"topPcs","args":{"cpu":"c64","limit":20}}).
//
// Output: a single JSON document on stdout = the reader op's result, BigInt-safe
// (cycle/seq cast to Number, well under 2^53). A failure prints {"error":"..."} to
// stdout and exits non-zero, so the Rust caller can surface a clean WS error
// (never a panic).
//
// Resolution: C64RE_ROOT (env, else the known dev path). Modules load from
// `<root>/dist/**` (built JS) when present, else `<root>/src/**` (tsx transpiles).
// The dist twin is preferred because the indexer spawns a Worker that loads the
// BUILT `binary-log-index-worker.js` (it can't load a `.ts`), and dist avoids
// transpiling the whole c64re src tree on every invocation.
// ─────────────────────────────────────────────────────────────────────────────

import { existsSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { resolve as resolvePath } from "node:path";

const C64RE_ROOT =
  process.env.C64RE_ROOT ?? "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP";

// Prefer the built dist tree (JS); fall back to src (tsx). The index worker is a
// BUILT .js, so when we import from src the c64re background-indexer still resolves
// its worker via its own src→dist path swap — but importing from dist keeps the
// whole chain in one (faster, build-consistent) tree.
const LAYER: "dist" | "src" = existsSync(resolvePath(C64RE_ROOT, "dist", "server-tools", "runtime.js"))
  ? "dist"
  : "src";
const EXT = LAYER === "dist" ? "js" : "ts";

/** Dynamic-import a c64re module by its repo-relative path (no extension). */
async function c64re<T = any>(relNoExt: string): Promise<T> {
  const abs = resolvePath(C64RE_ROOT, LAYER, `${relNoExt}.${EXT}`);
  if (!existsSync(abs)) {
    throw new Error(
      `c64re module not found: ${abs} (C64RE_ROOT=${C64RE_ROOT}, layer=${LAYER}). ` +
        (LAYER === "dist" ? "Run `npm run build:mcp` in the c64re repo." : "Check the c64re source tree."),
    );
  }
  return (await import(pathToFileURL(abs).href)) as T;
}

/** `.duckdb` → its `.c64retrace` authority (== c64re retracePathFor). */
function retracePathFor(duckdbPath: string): string {
  return duckdbPath.endsWith(".duckdb")
    ? duckdbPath.slice(0, -".duckdb".length) + ".c64retrace"
    : duckdbPath + ".c64retrace";
}

/** Recursively cast BigInt → Number (cycles/seq are < 2^53), so the result
 *  JSON-stringifies. Mirrors the WS handler's jsonSafe down-cast. */
function jsonSafe(x: unknown): unknown {
  return JSON.parse(JSON.stringify(x, (_k, v) => (typeof v === "bigint" ? Number(v) : v)));
}

interface Argv {
  op: string;
  duckdb: string;
  retrace?: string;
  args: Record<string, any>;
}

function parseArgv(argv: string[]): Argv {
  const op = argv[0];
  if (!op || op.startsWith("--")) throw new Error("usage: sidecar <op> --duckdb <path> [--retrace <path>] [--args <json>]");
  const flag = (name: string): string | undefined => {
    const i = argv.indexOf(name);
    return i >= 0 ? argv[i + 1] : undefined;
  };
  const duckdb = flag("--duckdb");
  if (!duckdb) throw new Error("--duckdb <path> required");
  const retrace = flag("--retrace");
  const rawArgs = flag("--args");
  let args: Record<string, any> = {};
  if (rawArgs) {
    try {
      args = JSON.parse(rawArgs);
    } catch (e) {
      throw new Error(`--args is not valid JSON: ${e instanceof Error ? e.message : String(e)}`);
    }
  }
  return { op, duckdb, retrace, args };
}

/** Run a trace/read op. 1:1 with ws-server.ts:1302-1377 (the daemon trace/read
 *  handler) + the store_fn dispatch (queries.ts). The only difference is that we
 *  open the store from a separate process (READ_ONLY, no daemon lock contention). */
async function runOp({ op, duckdb, retrace, args }: Argv): Promise<unknown> {
  // LAZY-ON-READ — build the index from the `.c64retrace` authority if it is
  // missing (covers audit misc-1: no index built at trace stop). If an explicit
  // --retrace was given and the default sibling does not exist, ensure the sibling
  // points at it by validating presence; ensureIndexBounded derives the sibling
  // itself, so we only need the .c64retrace to live at the sibling path. The Rust
  // caller passes the matching pair (it writes both at the same stem), so the
  // sibling is correct; --retrace is an override hook for non-standard layouts.
  const wantRetrace = retrace ?? retracePathFor(duckdb);
  if (!existsSync(duckdb) && !existsSync(wantRetrace)) {
    throw new Error(`no trace store and no .c64retrace authority at ${duckdb} (looked for ${wantRetrace})`);
  }

  // store_fn — the trace_store_* reader functions (queries.ts). ensureIndexBounded
  // first (queries.ts withConn does NOT, unlike withDuckDb), then dispatch by fn.
  if (op === "store_fn") {
    const { ensureIndexBounded } = await c64re("runtime/headless/trace/background-indexer");
    await ensureIndexBounded(duckdb);
    const q = await c64re("runtime/trace-store/queries");
    const fa = (args.args ?? {}) as Record<string, any>;
    let out: unknown;
    switch (String(args.fn)) {
      case "getInfo": out = await q.getInfo(duckdb); break;
      case "topPcs": out = await q.topPcs(duckdb, fa.cpu, fa.limit); break;
      case "findBusEvents": out = await q.findBusEvents(duckdb, Number(fa.addr), fa.limit); break;
      case "listAnchors": out = await q.listAnchors(duckdb); break;
      case "findAnchor": out = await q.findAnchor(duckdb, String(fa.name), fa.limit); break;
      case "safeQuery": out = await q.safeQuery(duckdb, String(fa.sql), fa.limit); break;
      default: throw new Error(`trace/read store_fn: unknown fn "${args.fn}"`);
    }
    return jsonSafe(out);
  }

  // The DuckDB-backed v2 reader ops. withDuckDb itself calls ensureIndexBounded,
  // opens READ_ONLY (heals to read-write + compat-view layer on an old store), and
  // hands a (conn, backend) pair to the closure — 1:1 with ws-server.ts.
  const { withDuckDb } = await c64re("server-tools/runtime");
  const a = args;
  return await withDuckDb(duckdb, async (conn: any, backend: any) => {
    switch (op) {
      case "swimlane": {
        const { swimlaneSlice } = await c64re("runtime/headless/v2/swimlane");
        return await swimlaneSlice(backend, {
          runId: a.run_id as string,
          cycleRange: [Number(a.cycle_start), Number(a.cycle_end)],
          compact: a.compact as boolean,
          ...(a.focus ? { focus: a.focus as "main" | "irq" | "nmi" } : {}),
          ...(a.nmi_vector !== undefined ? { nmiVector: Number(a.nmi_vector) } : {}),
        });
      }
      case "query_events": {
        const { queryEvents } = await c64re("runtime/headless/v2/query-events");
        return jsonSafe(await queryEvents(backend, a as never));
      }
      case "follow_path": {
        const { followPath } = await c64re("runtime/headless/v2/follow-path");
        return jsonSafe(await followPath(backend, a as never));
      }
      case "taint": {
        const { traceTaint } = await c64re("runtime/headless/v2/taint");
        return jsonSafe(await traceTaint(backend, a as never));
      }
      case "profile_loader": {
        const { profileLoader } = await c64re("runtime/headless/v2/loader-profile");
        return jsonSafe(
          await profileLoader(backend, a.scenario_id as string, [Number(a.cycle_start), Number(a.cycle_end)]),
        );
      }
      case "map": {
        // Monitor `map` (audit misc-14): the trace-memory-map text renderer over a
        // raw-SQL runner. 1:1 with ws-server.ts:2104-2110 (the `mop === "map"` arm).
        const runQuery = async (sql: string) =>
          (await conn.runAndReadAll(sql)).getRows().map((r: unknown[]) => r.map((c) => (typeof c === "bigint" ? Number(c) : c)));
        const { buildMemoryMapText } = await c64re("server-tools/trace-memory-map");
        const r = await buildMemoryMapText(runQuery, { cpu: String(a.cpu ?? "c64") });
        return { text: r?.text ?? "map: empty (the trace captured no memory accesses — enable the memory domain)" };
      }
      case "swimlane_text": {
        // Monitor `swimlane` over the CURRENT store (audit misc-14). 1:1 with the
        // ws-server.ts:2076-2096 currentStorePath swimlane render: default window
        // anchors to the store's own MAX(cycle) (NOT a live clock), span = lastCycles
        // ?? 2000; output = "# <stem>\n" + renderText(slice, {maxRows:200}). `stem`
        // (the store basename without .duckdb) is passed by the caller since the
        // sidecar can't reproduce the monitor's path-naming.
        const { swimlaneSlice } = await c64re("runtime/headless/v2/swimlane");
        const { renderText } = await c64re("runtime/headless/v2/swimlane-render");
        const rid = (await conn.runAndReadAll("SELECT run_id FROM trace_run LIMIT 1")).getRows()[0]?.[0];
        const runId = rid != null ? String(rid) : undefined;
        let cs = Number(a.cycle_start);
        let ce = Number(a.cycle_end);
        if (!Number.isFinite(cs) || !Number.isFinite(ce)) {
          const span = Number(a.last_cycles ?? 2000);
          const rg = (await conn.runAndReadAll("SELECT MIN(cycle), MAX(cycle) FROM trace_event WHERE cycle IS NOT NULL")).getRows()[0];
          const mn = Number(rg?.[0] ?? 0), mx = Number(rg?.[1] ?? 0);
          if (!Number.isFinite(ce)) ce = mx;
          if (!Number.isFinite(cs)) cs = Math.max(mn, ce - span);
        }
        const slice = await swimlaneSlice(backend, { runId, cycleRange: [cs, ce], compact: true } as never);
        const stem = String(a.stem ?? "trace");
        return { text: `# ${stem}\n` + renderText(slice, { maxRows: 200 }) };
      }
      case "taint_text": {
        // Monitor `taint` over the CURRENT store (audit misc-14). 1:1 with the
        // ws-server.ts:2111-2126 currentStorePath taint render: default start cycle =
        // the trace's own MAX(cycle); custom line format per contributing node.
        const { traceTaint } = await c64re("runtime/headless/v2/taint");
        const hx = (n: number) => (n & 0xffff).toString(16).padStart(4, "0");
        let startCycle = Number(a.start_cycle);
        if (!Number.isFinite(startCycle)) {
          const rg = (await conn.runAndReadAll("SELECT MAX(cycle) FROM trace_event WHERE cycle IS NOT NULL")).getRows()[0];
          startCycle = Number(rg?.[0] ?? 0);
        }
        const startAddr = Number(a.start_addr);
        const g: any = await traceTaint(backend, { runId: a.run_id, startCycle, startAddr } as never);
        const ns: any[] = Object.values(g.nodes ?? {});
        if (!ns.length) {
          return { text: `taint: no contributing write found for $${hx(startAddr)} @cyc ${startCycle} (try an explicit cycle from \`swimlane\`/\`map\`)` };
        }
        const lines = [`taint $${hx(startAddr)} @cyc ${startCycle} — ${ns.length} node(s)${g.truncated ? " (truncated)" : ""}:`];
        for (const n of ns.slice(0, 40)) lines.push(`  cyc ${n.cycle} pc=$${hx(n.pc ?? 0)} ${n.contribution} $${hx(n.addr ?? 0)}=$${(n.value ?? 0).toString(16).padStart(2, "0")}`);
        return { text: lines.join("\n") };
      }
      case "sql": {
        const reader = await conn.runAndReadAll(String(a.sql));
        const rows = reader.getRows().slice(0, Number(a.limit ?? 200));
        return { rows: rows.map((r: unknown[]) => r.map((c) => (typeof c === "bigint" ? c.toString() : c))) };
      }
      default:
        throw new Error(`trace/read: unknown op "${op}"`);
    }
  });
}

async function main(): Promise<void> {
  let parsed: Argv;
  try {
    parsed = parseArgv(process.argv.slice(2));
  } catch (e) {
    process.stdout.write(JSON.stringify({ error: e instanceof Error ? e.message : String(e) }) + "\n");
    process.exit(2);
  }
  try {
    const out = await runOp(parsed);
    process.stdout.write(JSON.stringify(out) + "\n");
    process.exit(0);
  } catch (e) {
    process.stdout.write(JSON.stringify({ error: e instanceof Error ? e.message : String(e) }) + "\n");
    process.exit(1);
  }
}

void main();
