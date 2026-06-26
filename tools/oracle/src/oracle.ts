#!/usr/bin/env tsx
// TRX64 Oracle CLI.
//
//   record  <scenario.json> [--endpoint ws://127.0.0.1:4312]
//       Drive the golden (TS) daemon, capture responses (+ trace), write <name>.golden.json.
//   compare <scenario.json> --candidate ws://127.0.0.1:43XX
//       Drive the candidate (TRX64) daemon, diff vs golden, print first-divergence.
//       Exit 0 = GREEN, 1 = RED, 2 = harness error.
//
// Response diffing is live now. Trace diffing activates once decodeTrace() has the
// exact binary-format.ts layout (the one remaining fact-dependent piece).

import { readFileSync, writeFileSync } from "node:fs";
import { connect } from "./ws-client.js";
import { diffResponses, diffTraces, formatDivergence, type Divergence } from "./diff.js";
import { decodeTrace } from "./trace-decode.js";
import { spawnDaemon, type DaemonKind } from "./daemon.js";
import type { Scenario, Golden } from "./scenario.js";

/** Deep-substitute the literal "$sessionId" placeholder in step params. */
function substitute<T>(value: T, sessionId: string | undefined): T {
  if (value === "$sessionId") return (sessionId ?? "") as unknown as T;
  if (Array.isArray(value)) return value.map((v) => substitute(v, sessionId)) as unknown as T;
  if (value && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value)) out[k] = substitute(v, sessionId);
    return out as T;
  }
  return value;
}

function pickString(obj: unknown, key: string): string | undefined {
  if (obj && typeof obj === "object" && key in obj) {
    const v = (obj as Record<string, unknown>)[key];
    if (typeof v === "string") return v;
  }
  return undefined;
}

function arg(flag: string): string | undefined {
  const i = process.argv.indexOf(flag);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

function loadScenario(path: string): Scenario {
  return JSON.parse(readFileSync(path, "utf8")) as Scenario;
}

async function replay(endpoint: string, scn: Scenario): Promise<Golden> {
  const client = await connect(endpoint);
  const responses: Golden["responses"] = [];
  let sessionId: string | undefined;
  let retracePath: string | undefined;
  try {
    for (const step of scn.steps) {
      const params = substitute(step.params, sessionId) as Record<string, unknown> | undefined;
      const result = await client.call(step.method, params);
      // Thread session id from session/create; derive the trace file location.
      sessionId ??= pickString(result, "sessionId");
      // trace/start_domains returns outputPath (.duckdb); the binary log is the sibling
      // .c64retrace (retracePathFor() in trace-run.ts). It is the product authority.
      const duckdb = pickString(result, "outputPath") ?? pickString(result, "retracePath");
      if (duckdb) retracePath = duckdb.replace(/\.duckdb$/, ".c64retrace");
      if (step.capture) {
        responses.push({ label: step.label ?? step.method, method: step.method, result });
      }
    }
  } finally {
    client.close();
  }

  let trace: Golden["trace"];
  if (scn.trace) {
    if (!retracePath) throw new Error("scenario.trace set but no retracePath seen (add trace/run/status)");
    trace = decodeTrace(readFileSync(retracePath)).records;
  }
  return { scenario: scn.name, responses, trace };
}

function goldenPath(scnPath: string): string {
  return scnPath.replace(/\.json$/, "") + ".golden.json";
}

async function cmdRecord(scnPath: string): Promise<number> {
  const scn = loadScenario(scnPath);
  // Hermetic: spawn a fresh TS daemon unless an explicit endpoint is given (debug).
  const explicit = arg("--endpoint");
  const daemon = explicit ? null : await spawnDaemon("ts");
  const endpoint = explicit ?? daemon!.endpoint;
  try {
    const golden = await replay(endpoint, scn);
    writeFileSync(goldenPath(scnPath), JSON.stringify(golden, null, 2));
    console.log(`recorded golden: ${goldenPath(scnPath)} (${golden.responses.length} captured)`);
    return 0;
  } finally {
    daemon?.stop();
  }
}

async function cmdCompare(scnPath: string): Promise<number> {
  const scn = loadScenario(scnPath);
  const golden = JSON.parse(readFileSync(goldenPath(scnPath), "utf8")) as Golden;
  // Hermetic: spawn a fresh candidate daemon (default TRX64; --candidate-kind ts for
  // a TS-vs-TS self-test) unless an explicit --candidate endpoint is given (debug).
  const explicit = arg("--candidate");
  const kind = (arg("--candidate-kind") as DaemonKind) ?? "trx64";
  const daemon = explicit ? null : await spawnDaemon(kind);
  const endpoint = explicit ?? daemon!.endpoint;
  let cand: Golden;
  try {
    cand = await replay(endpoint, scn);
  } finally {
    daemon?.stop();
  }

  let firstDiv: Divergence | null = null;
  const n = Math.min(golden.responses.length, cand.responses.length);
  for (let i = 0; i < n; i++) {
    const g = golden.responses[i]!;
    const c = cand.responses[i]!;
    // VICE-arbitrated override (durable, git-tracked in the scenario): where the TS
    // runtime is provably LESS accurate than VICE x64SC (the ground truth) on a
    // cycle-exact value, the golden is TS-RECORDED (and gitignored/regenerable) so it
    // carries TS's value — but the scenario's `viceOverrides[label]` corrects the
    // expected value to VICE-truth before the diff. See the scenario's `_viceNote`.
    const ov = (scn as unknown as { viceOverrides?: Record<string, Record<string, unknown>> })
      .viceOverrides?.[g.label];
    const gResult = ov && g.result && typeof g.result === "object"
      ? { ...(g.result as object), ...ov }
      : g.result;
    const d = diffResponses(gResult, c.result, `$.${g.label}`);
    if (d) {
      firstDiv = d;
      break;
    }
  }
  if (!firstDiv && golden.responses.length !== cand.responses.length) {
    firstDiv = {
      kind: "length",
      path: "responses.length",
      expected: golden.responses.length,
      got: cand.responses.length,
    };
  }
  // Trace parity — the cycle-exact gate.
  if (!firstDiv && golden.trace) {
    firstDiv = diffTraces(golden.trace, cand.trace ?? []);
  }

  console.log(`[${scn.name}] ${formatDivergence(firstDiv)}`);
  return firstDiv ? 1 : 0;
}

async function main(): Promise<number> {
  const cmd = process.argv[2];
  const scnPath = process.argv[3];
  if (!cmd || !scnPath) {
    console.error("usage: oracle <record|compare> <scenario.json> [flags]");
    return 2;
  }
  if (cmd === "record") return cmdRecord(scnPath);
  if (cmd === "compare") return cmdCompare(scnPath);
  console.error(`unknown command: ${cmd}`);
  return 2;
}

main().then(
  (code) => process.exit(code),
  (err) => {
    console.error("harness error:", err);
    process.exit(2);
  },
);
