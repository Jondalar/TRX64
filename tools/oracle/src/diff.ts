// First-divergence diff engine. Protocol-agnostic: works on decoded WS responses
// and on decoded trace records. The single feedback signal the builder fixes against.

/** Keys whose values legitimately differ between two daemon runs — never a divergence. */
export const VOLATILE_KEYS = new Set<string>([
  "sessionId",
  "diskPath",
  "dbPath",
  "duckdb_path",
  "storeId",
  "runId",
  "path",
  "out_path",
  "output_path",
  "outputPath",
  "evidenceRef",
  "createdAt",
  "overheadMs",
  "updated",
  "dataUrl", // PNG base64 — compared structurally elsewhere, not byte-equal
  "tmp",
  "timestamp",
]);

export interface Divergence {
  kind: "response" | "trace" | "length";
  path: string; // json-path-ish for responses, "trace[i].field" for traces
  expected: unknown;
  got: unknown;
  index?: number; // trace record index
  cycle?: number; // cycle of the diverging trace event
  family?: string; // trace event family (cpu/mem/iec/mark)
}

/** Deep-compare two WS responses; return the FIRST divergence or null if equal. */
export function diffResponses(expected: unknown, got: unknown, base = "$"): Divergence | null {
  if (expected === got) return null;

  const te = typeof expected;
  const tg = typeof got;
  if (te !== tg || expected === null || got === null) {
    if (expected === null || got === null || te !== tg) {
      return { kind: "response", path: base, expected, got };
    }
  }

  if (Array.isArray(expected) || Array.isArray(got)) {
    if (!Array.isArray(expected) || !Array.isArray(got)) {
      return { kind: "response", path: base, expected, got };
    }
    if (expected.length !== got.length) {
      return { kind: "length", path: `${base}.length`, expected: expected.length, got: got.length };
    }
    for (let i = 0; i < expected.length; i++) {
      const d = diffResponses(expected[i], got[i], `${base}[${i}]`);
      if (d) return d;
    }
    return null;
  }

  if (te === "object") {
    const eo = expected as Record<string, unknown>;
    const go = got as Record<string, unknown>;
    const keys = [...new Set([...Object.keys(eo), ...Object.keys(go)])].sort();
    for (const k of keys) {
      if (VOLATILE_KEYS.has(k)) continue;
      const d = diffResponses(eo[k], go[k], `${base}.${k}`);
      if (d) return d;
    }
    return null;
  }

  // primitives that aren't ===
  return { kind: "response", path: base, expected, got };
}

export interface TraceRecord {
  family: string; // "cpu" | "mem" | "iec" | "mark" | ...
  cycle: number;
  // normalized comparable fields per family (pc, opcode, a, x, y, sp, p / addr, value, op ...)
  fields: Record<string, number | string>;
}

/** Compare two decoded trace streams; return the FIRST diverging record or null. */
export function diffTraces(expected: TraceRecord[], got: TraceRecord[]): Divergence | null {
  const n = Math.min(expected.length, got.length);
  for (let i = 0; i < n; i++) {
    const e = expected[i]!;
    const g = got[i]!;
    if (e.family !== g.family) {
      return { kind: "trace", path: `trace[${i}].family`, index: i, cycle: e.cycle, expected: e.family, got: g.family };
    }
    if (e.cycle !== g.cycle) {
      return { kind: "trace", path: `trace[${i}].cycle`, index: i, cycle: e.cycle, family: e.family, expected: e.cycle, got: g.cycle };
    }
    for (const k of Object.keys(e.fields)) {
      if (e.fields[k] !== g.fields[k]) {
        return {
          kind: "trace",
          path: `trace[${i}].${k}`,
          index: i,
          cycle: e.cycle,
          family: e.family,
          expected: e.fields[k],
          got: g.fields[k],
        };
      }
    }
  }
  if (expected.length !== got.length) {
    return {
      kind: "length",
      path: "trace.length",
      index: n,
      expected: expected.length,
      got: got.length,
    };
  }
  return null;
}

/** Human-readable one-line report — what the builder reads to fix. */
export function formatDivergence(d: Divergence | null): string {
  if (!d) return "GREEN — no divergence";
  const at = d.cycle !== undefined ? ` @cycle ${d.cycle}` : "";
  const fam = d.family ? ` (${d.family})` : "";
  return `RED [${d.kind}] ${d.path}${at}${fam}: expected=${JSON.stringify(d.expected)} got=${JSON.stringify(d.got)}`;
}
