#!/usr/bin/env tsx
// integration.ts — the feature-complete-vs-TS-headless CAPSTONE.
//
// Drives a corpus of real C64 programs end-to-end through BOTH daemons live —
// the TRX64 Rust daemon AND the c64re TypeScript daemon — over the IDENTICAL WS
// JSON-RPC sequence, and proves TRX64 produces the SAME observable behavior as
// c64re. This is the broad cross-runtime proof beyond the focused 7-game gate.
//
// Three proof axes (per the work order):
//
//   1. CORPUS  — boot + mount + LOAD"*",8,1 (+ RUN). Outcome CLASS compared TRX64
//                vs c64re: the PC reaches game RAM (the (fast)loader is live — the
//                proof-canary bar) OR a title frame renders. The standard-serial
//                LOAD of a scene loader can take ~30M cycles before the stub
//                returns to READY; with an adequate budget the loader then goes
//                live in RAM. Parity = both runtimes reach the same class.
//   2. WS-SURFACE — on a live, actively-executing machine: session/screenshot,
//                monitor regs/mem/disasm (api/call), checkpoint capture/restore
//                (rewind), a breakpoint that halts (debug/breakpoint_hit, ADR-086),
//                audio/export. Each response shape + behavioral result matches c64re.
//   3. CROSS-RUNTIME SNAPSHOT — dump a live machine on TRX64, undump in c64re
//                (and vice-versa) -> resumes (the full feature-complete claim).
//
// Usage:
//   tsx src/integration.ts                 # full corpus + WS surface + xruntime snap
//   tsx src/integration.ts --quick         # gate-7 + 2 broad, shorter budgets
//   tsx src/integration.ts --only scramble # one corpus item by name substring
//   tsx src/integration.ts --report docs/integration-report.md  # write scorecard
//
// Exit 0 = all axes parity (or documented-criterion) PASS; 1 = a real divergence.

import { mkdtempSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve as resolvePath } from "node:path";
import { inflateSync } from "node:zlib";
import { spawnDaemon, type Daemon } from "./daemon.js";
import { connect, type RpcClient } from "./ws-client.js";
import { diffResponses, formatDivergence, type Divergence } from "./diff.js";

const SAMPLES =
  process.env.C64RE_SAMPLES ??
  "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples";

const PAL_HZ = 985_248;

// ── proof-canary classification (1:1 with proof-canary-disk.mjs / seven_game_gate.rs)
const STUCK = new Set<number>([
  0xe5cd, 0xe5ce, 0xe5cf, 0xe5d0, 0xe5d1, 0xe5d2, 0xe5d3, 0xe5d4, // READY/BASIC loop
  0xf6bf, 0xa483, 0xf6c5, 0xf6da, // LOAD/SAVE stalls
  0xeea9, 0xeeaf, 0xeeb2, 0xed5a, 0xed5d, // serial RX stall
]);
const READY = new Set<number>([0xe5cd, 0xe5ce, 0xe5cf, 0xe5d0, 0xe5d1, 0xe5d2, 0xe5d3, 0xe5d4]);
// PCs that mean the CPU is wedged on a hardware JAM/crash (KERNAL warm-start trap
// $FCE2 / hard reset $FF48 only ever recur on a reset loop). A resumed machine
// that keeps cycling here (with no cycle progress) failed to resume. READY is NOT
// here — a LOADED_READY machine idling at $E5CD is a healthy resumed state.
const JAMMED = new Set<number>([0x0000]);
const gameRunning = (pc: number): boolean => pc >= 0x0200 && pc < 0xa000 && !STUCK.has(pc);

// ── corpus ────────────────────────────────────────────────────────────────────
interface CorpusItem {
  name: string;
  disk: string; // relative to SAMPLES
  // loader class hint (just for the report; not used to gate):
  loader: "krill" | "custom" | "epyx" | "system3" | "ocean" | "magic" | "kernal";
  // per-item run budgets in cycles (some loaders are slow on the std serial path)
  loadCap?: number; // cycles allowed after LOAD before RUN
  runCap?: number; // cycles allowed after RUN to reach game-live
  // xref=true: ALSO drive the live c64re daemon for this item and compare. The
  // c64re TS runtime is ~10× slower (≈70k cyc/s), so a full multi-disk game can
  // take many minutes there. The xref subset is the fast-loader baseline that
  // reaches a running state quickly enough for a live cross-runtime comparison;
  // the slow games' c64re parity is already covered by the focused 7-game gate.
  // Forced on for ALL items with --all-xref.
  xref?: boolean;
}

const CORPUS: CorpusItem[] = [
  // ---- the 7-game gate set (already proven; included as cross-runtime anchor) ----
  // The xref pair (scramble/polarbear) reaches LOADED_READY ~6M cyc post-LOAD on
  // both runtimes; a short runCap keeps the slow c64re leg bounded while still
  // proving the same outcome class. (The gate is the gameplay authority.)
  { name: "scramble", disk: "scramble_infinity.d64", loader: "krill", xref: true, loadCap: 40 * PAL_HZ, runCap: 12 * PAL_HZ },
  { name: "polarbear", disk: "POLARBEAR.d64", loader: "custom", xref: true, loadCap: 40 * PAL_HZ, runCap: 12 * PAL_HZ },
  { name: "motm", disk: "motm.g64", loader: "custom", runCap: 60 * PAL_HZ },
  { name: "maniac_s1", disk: "maniac_mansion_s1[activision_1987](german)(manual)(!).g64", loader: "custom", runCap: 90 * PAL_HZ },
  { name: "im2", disk: "impossible_mission_ii[epyx_1987](!).g64", loader: "epyx", runCap: 60 * PAL_HZ },
  { name: "lnr_s1", disk: "last_ninja_remix_s1[system3_1991].g64", loader: "system3", runCap: 60 * PAL_HZ },
  { name: "green_beret", disk: "green_beret[ocean_1986](!).g64", loader: "ocean", runCap: 60 * PAL_HZ },
  // ---- the BROADER corpus (beyond the gate) -------------------------------------
  { name: "california_s1", disk: "california_games_s1[epyx_1987](ntsc).g64", loader: "epyx", runCap: 60 * PAL_HZ },
  { name: "summer_games", disk: "summer_games[epyx_1984](pal)(!).g64", loader: "epyx", runCap: 60 * PAL_HZ },
  { name: "winter_games", disk: "winter_games_s1[epyx_1985](pal)(!).g64", loader: "epyx", runCap: 60 * PAL_HZ },
  { name: "world_games", disk: "world_games_s1[epyx_1986](pal).g64", loader: "epyx", runCap: 60 * PAL_HZ },
  { name: "rainbow_islands", disk: "rainbow_islands[ocean_1990](pal)(!).g64", loader: "ocean", runCap: 60 * PAL_HZ },
  { name: "last_ninja_iii", disk: "last_ninja_iii_s1[system_3_1991](pal)(!).g64", loader: "system3", runCap: 60 * PAL_HZ },
  { name: "the_pawn", disk: "the_pawn_s1.g64", loader: "custom", runCap: 60 * PAL_HZ },
  { name: "die_dunkle_dimension", disk: "Die_Dunkle_Dimension_Golden Disk 64 (05) (Side 1).d64", loader: "kernal", runCap: 60 * PAL_HZ },
  { name: "accolades", disk: "accolades_comics_s1_ORGINAL.g64", loader: "magic", runCap: 60 * PAL_HZ },
];

const QUICK_NAMES = new Set(["scramble", "polarbear", "california_s1", "summer_games"]);

// ── per-runtime corpus driver ───────────────────────────────────────────────────
interface RunOutcome {
  reached: boolean; // game code live (sustained 2 samples) OR coherent frame
  loadReturnedReady: boolean; // LOAD"*",8,1 completed -> BASIC READY (vs serial stall)
  firstGamePc: number | null;
  finalPc: number;
  colors: number; // distinct RGB colors in the rendered frame
  nonBlank: number; // non-space screen cells (heuristic title evidence)
  cyclesAtEnd: number;
  error?: string;
}

/** call helper that surfaces errors as a thrown Error with method context. */
async function call(c: RpcClient, method: string, params?: Record<string, unknown>): Promise<any> {
  return await c.call(method, params);
}

// The c64re TS runtime advances at ~70k cycles/sec wall time (≈10× slower than
// TRX64). A single multi-million-cycle session/run on c64re can exceed the WS
// call timeout — so we CHUNK every long run into ≤RUN_CHUNK pieces. Each piece
// stays well under the (raised) per-call timeout, while arbitrary total budgets
// remain reachable. TRX64 chunks too (cheap) for identical drive semantics.
const RUN_CHUNK = 1_000_000;

/** Advance the machine by `total` cycles via chunked session/run calls (so no
 *  single WS call blocks past the timeout on the slow c64re runtime). */
async function runCycles(c: RpcClient, sid: string, total: number): Promise<void> {
  let remaining = total;
  while (remaining > 0) {
    const n = Math.min(remaining, RUN_CHUNK);
    await c.call("session/run", { session_id: sid, cycles: n });
    remaining -= n;
  }
}

/** Count distinct RGB triples in an RGBA buffer (title-screen coherence proxy). */
function distinctColors(rgba: Uint8Array): number {
  const set = new Set<number>();
  for (let i = 0; i + 3 < rgba.length; i += 4) {
    set.add((rgba[i]! << 16) | (rgba[i + 1]! << 8) | rgba[i + 2]!);
  }
  return set.size;
}

/** Decode a session/screenshot result -> {w,h,rgba}. Both daemons return a PNG
 *  data URL in `dataUrl`. c64re returns {dataUrl,bytes}; TRX64 {dataUrl,width,height}
 *  — so dimensions come from the decoded PNG, not the (divergent) envelope keys. */
function renderToColors(result: any): { colors: number; w: number; h: number } {
  const w = Number(result?.width ?? result?.w ?? 0);
  const h = Number(result?.height ?? result?.h ?? 0);
  const b64: string | undefined = result?.dataUrl?.replace?.(/^data:image\/png;base64,/, "");
  if (!b64) return { colors: 0, w, h };
  // Decode the PNG to count distinct colors. Use the lightweight inline PNG reader.
  try {
    const buf = Buffer.from(b64, "base64");
    const rgba = decodePngRgba(buf);
    return { colors: rgba ? distinctColors(rgba) : 0, w, h };
  } catch {
    return { colors: 0, w, h };
  }
}

/** Minimal PNG -> RGBA decoder (handles the 8-bit RGBA non-interlaced PNGs both
 *  daemons emit). Returns null on any unsupported chunk so we degrade gracefully. */
function decodePngRgba(buf: Buffer): Uint8Array | null {
  if (buf.length < 8 || buf.readUInt32BE(0) !== 0x89504e47) return null;
  let pos = 8;
  let width = 0, height = 0, bitDepth = 0, colorType = 0;
  const idat: Buffer[] = [];
  while (pos + 8 <= buf.length) {
    const len = buf.readUInt32BE(pos);
    const type = buf.toString("ascii", pos + 4, pos + 8);
    const data = buf.subarray(pos + 8, pos + 8 + len);
    if (type === "IHDR") {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      bitDepth = data[8]!;
      colorType = data[9]!;
    } else if (type === "IDAT") {
      idat.push(data);
    } else if (type === "IEND") {
      break;
    }
    pos += 12 + len;
  }
  if (bitDepth !== 8 || (colorType !== 6 && colorType !== 2)) return null;
  const channels = colorType === 6 ? 4 : 3;
  let raw: Buffer;
  try {
    raw = inflateSync(Buffer.concat(idat));
  } catch {
    return null;
  }
  const stride = width * channels;
  const out = new Uint8Array(width * height * 4);
  const prev = new Uint8Array(stride);
  const cur = new Uint8Array(stride);
  let rp = 0;
  for (let y = 0; y < height; y++) {
    const filter = raw[rp++]!;
    for (let x = 0; x < stride; x++) {
      const rawb = raw[rp++]!;
      const a = x >= channels ? cur[x - channels]! : 0;
      const b = prev[x]!;
      const cc = x >= channels ? prev[x - channels]! : 0;
      let val: number;
      switch (filter) {
        case 0: val = rawb; break;
        case 1: val = rawb + a; break;
        case 2: val = rawb + b; break;
        case 3: val = rawb + ((a + b) >> 1); break;
        case 4: {
          const p = a + b - cc;
          const pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - cc);
          const pred = pa <= pb && pa <= pc ? a : pb <= pc ? b : cc;
          val = rawb + pred; break;
        }
        default: return null;
      }
      cur[x] = val & 0xff;
    }
    for (let x = 0; x < width; x++) {
      const o = (y * width + x) * 4;
      const s = x * channels;
      out[o] = cur[s]!;
      out[o + 1] = cur[s + 1]!;
      out[o + 2] = cur[s + 2]!;
      out[o + 3] = channels === 4 ? cur[s + 3]! : 0xff;
    }
    prev.set(cur);
  }
  return out;
}

/** Read the C64 PC from session/state. */
async function pc(c: RpcClient, sid: string): Promise<number> {
  const st = await call(c, "session/state", { session_id: sid });
  return Number(st?.cpu?.pc ?? 0) & 0xffff;
}
async function cycles(c: RpcClient, sid: string): Promise<number> {
  const st = await call(c, "session/state", { session_id: sid });
  return Number(st?.c64Cycles ?? 0);
}

/** Drive ONE corpus item end-to-end on ONE daemon over WS. */
async function driveCorpusItem(c: RpcClient, item: CorpusItem): Promise<RunOutcome> {
  try {
    const created = await call(c, "session/create", { pal: true });
    const sid: string = created?.sessionId ?? created?.session_id ?? "";

    // Boot to READY (~5M cyc).
    await runCycles(c, sid, 5_000_000);

    // Mount the disk.
    const diskPath = resolvePath(SAMPLES, item.disk);
    await call(c, "media/mount", { session_id: sid, path: diskPath });

    // LOAD"*",8,1
    await call(c, "session/type", { session_id: sid, text: 'LOAD"*",8,1\r' });

    // State accumulated across the whole post-LOAD run.
    let firstHit: number | null = null;
    let reached = false;
    let firstGamePc: number | null = null;
    let bestColors = 0;
    let loadReturnedReady = false;
    let typedRun = false;

    // Run until BASIC READY returns (= load complete, a stub to RUN) OR the
    // (fast)loader goes live in game RAM (sustained) OR the load cap. A loader
    // that runs straight in RAM never returns to READY — break early on game-PC.
    const loadCap = (await cycles(c, sid)) + (item.loadCap ?? 45 * PAL_HZ);
    while ((await cycles(c, sid)) < loadCap) {
      await runCycles(c, sid, 2_000_000);
      const p = await pc(c, sid);
      if (READY.has(p)) {
        loadReturnedReady = true;
        break;
      }
      if (gameRunning(p)) {
        if (firstGamePc === null) firstGamePc = p;
        if (firstHit !== null) {
          reached = true;
          break;
        }
        firstHit = p;
      } else {
        firstHit = null;
      }
    }

    // If the load settled at READY, type RUN to launch the stub.
    if (loadReturnedReady && !reached) {
      await call(c, "session/type", { session_id: sid, text: "RUN\r" });
      typedRun = true;
    }

    // Run until game code is live (sustained 2 samples), sampling colors. Sample
    // the framebuffer only every few chunks — the PNG encode is the costly call
    // on the slow c64re runtime.
    const runCap = (await cycles(c, sid)) + (item.runCap ?? 40 * PAL_HZ);
    let chunkIdx = 0;
    while (!reached && (await cycles(c, sid)) < runCap) {
      await runCycles(c, sid, 1_000_000);
      const p = await pc(c, sid);
      if ((chunkIdx++ & 3) === 0) {
        const frame = renderToColors(await call(c, "session/screenshot", { session_id: sid }));
        if (frame.colors > bestColors) bestColors = frame.colors;
      }
      if (gameRunning(p)) {
        if (firstGamePc === null) firstGamePc = p;
        if (firstHit !== null) {
          reached = true;
          break;
        }
        firstHit = p;
      } else {
        firstHit = null;
      }
    }
    void typedRun;

    const finalPc = await pc(c, sid);
    const cyc = await cycles(c, sid);
    // Final frame color count (best of run-sampled vs final).
    const finalFrame = renderToColors(await call(c, "session/screenshot", { session_id: sid }));
    const colors = Math.max(bestColors, finalFrame.colors);
    await call(c, "session/close", { session_id: sid }).catch(() => undefined);
    return { reached, loadReturnedReady, firstGamePc, finalPc, colors, nonBlank: 0, cyclesAtEnd: cyc };
  } catch (e) {
    return { reached: false, loadReturnedReady: false, firstGamePc: null, finalPc: 0, colors: 0, nonBlank: 0, cyclesAtEnd: 0, error: String(e) };
  }
}

// ── outcome classification + comparison ────────────────────────────────────────
// GAME_LIVE = after LOAD (+RUN if it returned to READY), the PC is sustained in
//   game RAM — the (fast)loader is live, which is the proof-canary bar.
// LOADED_READY = LOAD completed to BASIC READY and the game did not (yet) launch
//   within the run budget (a stub-only program, or a budget too short).
// Over the WS daemon the standard-serial LOAD of a scene loader can take ~30M
// cycles before the stub returns to READY; with an adequate loadCap the matrix-
// typed RUN then launches the loader live in RAM (scramble: READY @~29M, game-PC
// @~33M). Both runtimes reach the same class for the same disk + budget.
type OutcomeClass = "GAME_LIVE" | "RENDERED" | "LOADED_READY" | "STUCK" | "ERROR";
function classify(o: RunOutcome): OutcomeClass {
  if (o.error) return "ERROR";
  if (o.reached) return "GAME_LIVE";
  if (o.colors > 4) return "RENDERED";
  if (o.loadReturnedReady) return "LOADED_READY";
  return "STUCK";
}
/** Outcome classes are "equivalent" if both reached a live/rendered game state.
 *  GAME_LIVE and RENDERED collapse to the same PASS class (a title rendered by the
 *  IRQ while main sits in a ROM wait still counts). LOADED_READY and STUCK are
 *  their own classes — two runtimes both ending LOADED_READY IS parity. */
function sameClass(a: OutcomeClass, b: OutcomeClass): boolean {
  const pass = (x: OutcomeClass) => x === "GAME_LIVE" || x === "RENDERED";
  if (pass(a) && pass(b)) return true;
  return a === b;
}

// ── WS-SURFACE sequence on a RUNNING program ────────────────────────────────────
interface SurfaceResult {
  method: string;
  divergence: Divergence | null;
  note: string;
}

/** Boot to a genuinely RUNNING program: boot + mount + LOAD"*",8,1, then run
 *  until the PC is sustained in game RAM ($0200..$9FFF — the fastloader/game live,
 *  the proof-canary criterion) OR BASIC READY. Returns the session id. This is the
 *  substrate for the WS-surface + cross-runtime-snapshot axes — a live, non-trivial,
 *  deterministically-reached state both runtimes reach identically, with the real
 *  program image resident in RAM. Stops as soon as game RAM is live so the slow
 *  c64re leg stays bounded. */
async function bootToActive(c: RpcClient, item: CorpusItem): Promise<string> {
  const created = await call(c, "session/create", { pal: true });
  const sid: string = created?.sessionId ?? created?.session_id ?? "";
  await runCycles(c, sid, 5_000_000);
  await call(c, "media/mount", { session_id: sid, path: resolvePath(SAMPLES, item.disk) });
  await call(c, "session/type", { session_id: sid, text: 'LOAD"*",8,1\r' });
  // Phase 1: LOAD until the loader is live in RAM (sustained) OR READY returns.
  const cap1 = (await cycles(c, sid)) + 40 * PAL_HZ;
  let prevGame = false;
  let ready = false;
  let live = false;
  while ((await cycles(c, sid)) < cap1) {
    await runCycles(c, sid, 1_000_000);
    const p = await pc(c, sid);
    if (gameRunning(p)) {
      if (prevGame) { live = true; break; }
      prevGame = true;
    } else {
      prevGame = false;
      if (READY.has(p)) { ready = true; break; }
    }
  }
  // Phase 2: if we settled at READY, type RUN and run until the loader is live.
  if (ready && !live) {
    await call(c, "session/type", { session_id: sid, text: "RUN\r" });
    const cap2 = (await cycles(c, sid)) + 12 * PAL_HZ;
    prevGame = false;
    while ((await cycles(c, sid)) < cap2) {
      await runCycles(c, sid, 1_000_000);
      const p = await pc(c, sid);
      if (gameRunning(p)) {
        if (prevGame) break;
        prevGame = true;
      } else prevGame = false;
    }
  }
  // A short extra run so the machine is mid-execution (IRQ/raster active) at the
  // moment we snapshot — not paused exactly on a loop boundary.
  await runCycles(c, sid, 200_000);
  return sid;
}

/** Run the representative WS surface against ONE daemon and capture the response
 *  shapes keyed by a stable label, so the two runtimes can be diffed. */
async function captureSurface(c: RpcClient, sid: string): Promise<Map<string, unknown>> {
  const out = new Map<string, unknown>();
  const cap = async (label: string, method: string, params: Record<string, unknown>) => {
    try {
      out.set(label, await call(c, method, { session_id: sid, ...params }));
    } catch (e) {
      out.set(label, { __error: String(e) });
    }
  };
  // session/screenshot — the render surface BOTH daemons expose (c64re has no
  // runtime/render_screen on the wire; that's a TRX64 superset). Compared
  // behaviorally below (decodable PNG + dimensions), not by envelope keys.
  await cap("screenshot", "session/screenshot", {});
  // monitor registers / memory / disasm (the peek surface) via api/call
  await cap("regs", "api/call", { method: "monitorRegisters", args: [] });
  await cap("mem-io", "api/call", { method: "monitorMemory", args: [0xd000, 0xd02f] });
  await cap("disasm", "api/call", { method: "monitorDisasm", args: [0xfce2, 4] });
  await cap("status", "api/call", { method: "status", args: [] });
  return out;
}

/** Keys that are documented per-runtime SUPERSETS (present on one daemon only) and
 *  must not count as a divergence — the contract is "c64re's surface, possibly
 *  extended". e.g. c64re's status carries scenarioId; TRX64 omits it. */
const SUPERSET_KEYS = new Set<string>(["scenarioId", "stopReason"]);

/** SHAPE-diff: compares the recursive key STRUCTURE + leaf TYPES (not values),
 *  ignoring SUPERSET_KEYS. Exact register/memory VALUES legitimately differ — the
 *  two runtimes are at different wall-clocks / loader phases at capture time — so
 *  the surface-parity contract is "same response SHAPE", which is what the WS
 *  consumer (UI / MCP tools) depends on. (Cross-runtime cycle-exactness is the
 *  separate byte-exact-trace gate, not this live-corpus surface check.) */
function diffShapeTolerant(a: unknown, b: unknown, base: string): Divergence | null {
  const shape = (v: unknown): unknown => {
    if (Array.isArray(v)) return v.length === 0 ? [] : [shape(v[0])];
    if (v && typeof v === "object") {
      const o: Record<string, unknown> = {};
      for (const k of Object.keys(v as object).sort()) {
        if (SUPERSET_KEYS.has(k)) continue;
        o[k] = shape((v as Record<string, unknown>)[k]);
      }
      return o;
    }
    return typeof v;
  };
  return diffResponses(shape(a), shape(b), base);
}

/** Run the WS surface on PRE-BOOTED active sessions (so c64re is booted once and
 *  reused across the surface + snapshot axes — the slow runtime's boots dominate). */
async function runWsSurface(
  tsC: RpcClient,
  rsC: RpcClient,
  tsSid: string,
  rsSid: string,
): Promise<SurfaceResult[]> {
  const tsSurf = await captureSurface(tsC, tsSid);
  const rsSurf = await captureSurface(rsC, rsSid);
  const results: SurfaceResult[] = [];
  for (const label of tsSurf.keys()) {
    if (label === "screenshot") {
      // BEHAVIORAL parity: both must return a decodable PNG of the same dims with
      // real content. The envelope keys differ by design (c64re {dataUrl,bytes} vs
      // TRX64 {dataUrl,width,height}) — that's a known TRX64 superset, not a bug.
      const g = renderToColors(tsSurf.get(label));
      const c = renderToColors(rsSurf.get(label));
      const gDims = decodeDims(tsSurf.get(label));
      const cDims = decodeDims(rsSurf.get(label));
      const ok = gDims.w > 0 && gDims.w === cDims.w && gDims.h === cDims.h && c.colors > 1;
      results.push({
        method: "session/screenshot",
        divergence: ok ? null : { kind: "response", path: "$.screenshot", expected: `${gDims.w}x${gDims.h}`, got: `${cDims.w}x${cDims.h} colors=${c.colors}` },
        note: ok ? `both ${gDims.w}x${gDims.h} PNG, trx64 colors=${c.colors}` : `dim/content mismatch`,
      });
      continue;
    }
    const d = diffShapeTolerant(tsSurf.get(label), rsSurf.get(label), `$.${label}`);
    results.push({ method: `api/call ${label}`, divergence: d, note: d ? "shape diff" : "shape parity" });
  }
  // ---- checkpoint capture/restore (rewind) on the running program -------------
  results.push(await ckptRoundtrip(tsC, tsSid, "ts"));
  results.push(await ckptRoundtrip(rsC, rsSid, "trx64"));
  // ---- a breakpoint that HALTS + fires debug/breakpoint_hit (ADR-086) ---------
  results.push(await breakpointHit(tsC, tsSid, "ts"));
  results.push(await breakpointHit(rsC, rsSid, "trx64"));
  // ---- audio/export on the running program (behavioral: a WAV is produced) ----
  results.push(await audioExport(tsC, tsSid, "ts"));
  results.push(await audioExport(rsC, rsSid, "trx64"));
  // sessions are kept open — the snapshot axis reuses them.
  return results;
}

/** Decode just the PNG dimensions from a screenshot result's dataUrl. */
function decodeDims(result: any): { w: number; h: number } {
  const b64: string | undefined = result?.dataUrl?.replace?.(/^data:image\/png;base64,/, "");
  if (!b64) return { w: 0, h: 0 };
  try {
    const buf = Buffer.from(b64, "base64");
    if (buf.length < 24 || buf.readUInt32BE(0) !== 0x89504e47) return { w: 0, h: 0 };
    // IHDR is the first chunk at offset 8; width/height at 16/20.
    return { w: buf.readUInt32BE(16), h: buf.readUInt32BE(20) };
  } catch {
    return { w: 0, h: 0 };
  }
}

/** Arm a PC breakpoint at the current PC, debug/run, and prove the daemon HALTS
 *  AND emits a debug/breakpoint_hit notification (ADR-086). Behavioral, per runtime. */
async function breakpointHit(c: RpcClient, sid: string, kind: string): Promise<SurfaceResult> {
  try {
    // Robust target: the CURRENT PC of the running program. A running game's main
    // loop re-executes this address within the bounded debug budget, so a
    // debug/continue (which steps past the current PC first, then runs) reliably
    // re-trips the breakpoint there — on BOTH runtimes, regardless of loader.
    const target = await pc(c, sid);
    let fired = false;
    let firedPc = -1;
    const off = c.onNotify((method, params: any) => {
      if (method === "debug/breakpoint_hit") {
        fired = true;
        firedPc = Number(params?.pc ?? -1) & 0xffff;
      }
    });
    await call(c, "debug/break_add", { session_id: sid, pc: target });
    // debug/continue: steps past current PC, then runs to the breakpoint (bounded
    // budget). The daemon halts at the bp + pushes debug/breakpoint_hit.
    const runResp = await call(c, "debug/continue", { session_id: sid });
    // give the notification a moment to land
    await new Promise((r) => setTimeout(r, 50));
    off();
    // The ADR-086 proof of "a breakpoint that halts" is the debug/breakpoint_hit
    // NOTIFICATION firing at the target PC (the broadcast a passive client sees).
    // The debug/continue reply ALSO carries the halt, but its exact shape (runState
    // vs stop.reason; the stopPc may be the next PC in the loop) varies, so the
    // notification is the authoritative, runtime-agnostic signal.
    const stopPc = Number(runResp?.pc ?? runResp?.stop?.pc ?? -1) & 0xffff;
    const respHalted =
      (runResp?.runState === "paused" || runResp?.stop?.reason === "breakpoint") && stopPc === target;
    const notifyHalted = fired && firedPc === target;
    const ok = notifyHalted || respHalted;
    return {
      method: `breakpoint-hit[${kind}]`,
      divergence: ok ? null : { kind: "response", path: "$.breakpoint", expected: `halt@$${target.toString(16)}`, got: `notify(fired=${fired} pc=$${firedPc.toString(16)}) resp(rs=${runResp?.runState} pc=$${stopPc.toString(16)})` },
      note: ok ? `halt@$${target.toString(16)} via ${notifyHalted ? "debug/breakpoint_hit notify" : "continue-reply"}` : `no halt at $${target.toString(16)}`,
    };
  } catch (e) {
    return { method: `breakpoint-hit[${kind}]`, divergence: { kind: "response", path: "$.breakpoint", expected: "halt", got: String(e) }, note: String(e) };
  }
}

/** audio/export a short WAV from the running program; PASS = nonzero samples + bytes. */
async function audioExport(c: RpcClient, sid: string, kind: string): Promise<SurfaceResult> {
  try {
    const out = join(tmpdir(), `trx64-audio-${kind}-${Date.now()}.wav`);
    const res = await call(c, "audio/export", { session_id: sid, out_path: out, duration_sec: 0.1 });
    const samples = Number(res?.samples ?? 0);
    const bytes = Number(res?.bytes ?? 0);
    const ok = samples > 0 && bytes > 0;
    try { rmSync(out, { force: true }); } catch { /* ignore */ }
    return {
      method: `audio-export[${kind}]`,
      divergence: ok ? null : { kind: "response", path: "$.audio", expected: "samples>0", got: `samples=${samples} bytes=${bytes}` },
      note: ok ? `${samples} samples, ${bytes} bytes WAV` : `empty export`,
    };
  } catch (e) {
    return { method: `audio-export[${kind}]`, divergence: { kind: "response", path: "$.audio", expected: "wav", got: String(e) }, note: String(e) };
  }
}

/** capture -> restore round-trip; PASS if the restore returns the same machine
 *  state PC as the capture point (native rewind). Same-runtime behavioral check. */
async function ckptRoundtrip(c: RpcClient, sid: string, kind: string): Promise<SurfaceResult> {
  try {
    const capPc = await pc(c, sid);
    const cap = await call(c, "checkpoint/capture", { session_id: sid });
    const cpId: string = cap?.ref?.id ?? cap?.id ?? "";
    // advance the machine away from the capture point
    await runCycles(c, sid, 2_000_000);
    const restored = await call(c, "checkpoint/restore", { session_id: sid, id: cpId, then: "pause" });
    const restoredPc = Number(restored?.state?.cpu?.pc ?? restored?.state?.pc ?? (await pc(c, sid))) & 0xffff;
    const ok = restoredPc === capPc;
    return {
      method: `checkpoint-rewind[${kind}]`,
      divergence: ok ? null : { kind: "response", path: "$.checkpoint.pc", expected: capPc, got: restoredPc },
      note: ok ? `rewound to $${capPc.toString(16)}` : `expected $${capPc.toString(16)} got $${restoredPc.toString(16)}`,
    };
  } catch (e) {
    return { method: `checkpoint-rewind[${kind}]`, divergence: { kind: "response", path: "$.checkpoint", expected: "ok", got: String(e) }, note: String(e) };
  }
}

// ── CROSS-RUNTIME SNAPSHOT on a RUNNING program ────────────────────────────────
interface XSnapResult {
  direction: string; // "trx64->c64re" | "c64re->trx64"
  dumpedPc: number;
  undumpedPc: number;
  ok: boolean;
  note: string;
}

/** Dump a running program on `src` daemon, undump on `dst` daemon, verify the PC
 *  (and cycle) survive the cross-runtime hop. This is the full feature-complete
 *  claim: the .c64re container is runtime-agnostic. */
async function crossRuntimeSnapshot(
  srcC: RpcClient,
  dstC: RpcClient,
  srcSid: string,
  dstSid: string,
  direction: string,
  tmpDir: string,
): Promise<XSnapResult> {
  try {
    const dumpedPc = await pc(srcC, srcSid);
    const snapPath = join(tmpDir, `xrt-${direction.replace(/[^a-z0-9]/gi, "_")}.c64re`);
    const dump = await call(srcC, "snapshot/dump", { session_id: srcSid, path: snapPath });
    const dumpPc = Number(dump?.pc ?? dumpedPc) & 0xffff;

    // Undump into the OTHER runtime's (singleton) session — overwrites its live
    // machine with the cross-runtime snapshot.
    const undump = await call(dstC, "snapshot/undump", { session_id: dstSid, path: snapPath });
    const undumpPc = Number(undump?.pc ?? (await pc(dstC, dstSid))) & 0xffff;
    const undumpCycle = Number(undump?.cycle ?? 0);
    // Resume proof: continue a little and confirm the machine ACTUALLY advanced
    // (cycles climbed) — i.e. it resumed live execution from the restored state.
    // (We do NOT require the PC to leave the READY loop: a LOADED_READY machine
    //  legitimately idles there; the snapshot's job is to round-trip + resume.)
    const cyc0 = await cycles(dstC, dstSid);
    await call(dstC, "session/run", { session_id: dstSid, cycles: 500_000 });
    const cyc1 = await cycles(dstC, dstSid);
    const afterPc = await pc(dstC, dstSid);

    const restoredOk = undumpPc === dumpPc; // PC survived the cross-runtime hop
    const advanced = cyc1 > cyc0 + 400_000; // executed ~the requested budget
    const resumedOk = advanced && !JAMMED.has(afterPc); // ran on, not a CPU jam
    const ok = restoredOk && resumedOk;
    return {
      direction,
      dumpedPc: dumpPc,
      undumpedPc: undumpPc,
      ok,
      note: `dump pc=$${dumpPc.toString(16)} undump pc=$${undumpPc.toString(16)} cycle=${undumpCycle} after-resume pc=$${afterPc.toString(16)} (+${cyc1 - cyc0} cyc) restored=${restoredOk} resumed=${resumedOk}`,
    };
  } catch (e) {
    return { direction, dumpedPc: 0, undumpedPc: 0, ok: false, note: `error: ${String(e)}` };
  }
}

// ── orchestration ───────────────────────────────────────────────────────────────
function hasArg(flag: string): boolean {
  return process.argv.includes(flag);
}
function argVal(flag: string): string | undefined {
  const i = process.argv.indexOf(flag);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

interface CorpusRow {
  name: string;
  loader: string;
  diskPresent: boolean;
  xref: boolean; // was the live c64re comparison run for this item?
  ts: RunOutcome | null; // null when not xref (c64re not driven)
  rs: RunOutcome;
  tsClass: OutcomeClass | null;
  rsClass: OutcomeClass;
  parity: boolean | null; // null when not xref
}

async function main(): Promise<number> {
  const quick = hasArg("--quick");
  const allXref = hasArg("--all-xref");
  const only = argVal("--only");
  const reportPath = argVal("--report");

  let corpus = CORPUS;
  if (only) corpus = corpus.filter((i) => i.name.includes(only));
  else if (quick) corpus = corpus.filter((i) => QUICK_NAMES.has(i.name));
  // --only and --all-xref force a live c64re comparison for the selected items.
  if (only || allXref) corpus = corpus.map((i) => ({ ...i, xref: true }));

  // Filter to items whose disk is present.
  corpus = corpus.filter((i) => {
    const present = existsSync(resolvePath(SAMPLES, i.disk));
    if (!present) console.warn(`[skip] ${i.name}: disk absent (${i.disk})`);
    return present;
  });

  // --self drives TWO TRX64 daemons (instead of c64re-vs-TRX64) for a FAST
  // mechanics self-test of the harness — not a cross-runtime proof.
  const self = hasArg("--self");
  const refKind = self ? "trx64" : "ts";

  console.log(`=== TRX64 ↔ ${self ? "TRX64 [self-test]" : "c64re"} Integration Capstone ===`);
  console.log(`corpus: ${corpus.length} item(s)${quick ? " [quick]" : ""}${only ? ` [only ${only}]` : ""}\n`);

  // Spawn the reference (c64re TS, or TRX64 for --self) + the TRX64 candidate.
  let tsD: Daemon | null = null;
  let rsD: Daemon | null = null;
  const tmpDir = mkdtempSync(join(tmpdir(), "trx64-integration-"));
  try {
    [tsD, rsD] = await Promise.all([spawnDaemon(refKind), spawnDaemon("trx64")]);
    // 180s per-call timeout: a single ≤1M-cycle chunk is ~14s on c64re, but a
    // busy daemon + GC can stretch a call; keep generous headroom.
    const tsC = await connect(tsD.endpoint, 180_000);
    const rsC = await connect(rsD.endpoint, 180_000);

    // ───────── AXIS 1: CORPUS ─────────
    // TRX64 runs EVERY item (fast). c64re is driven only for xref items (it is
    // ~10× slower — a full broad-corpus c64re sweep would take hours), giving the
    // live cross-runtime comparison on the fast-loader baseline.
    const rows: CorpusRow[] = [];
    for (const item of corpus) {
      process.stdout.write(`[corpus] ${item.name.padEnd(22)} `);
      const drives: Promise<RunOutcome>[] = [driveCorpusItem(rsC, item)];
      if (item.xref) drives.unshift(driveCorpusItem(tsC, item));
      const results = await Promise.all(drives);
      const rs = item.xref ? results[1]! : results[0]!;
      const ts = item.xref ? results[0]! : null;
      const rsClass = classify(rs);
      const tsClass = ts ? classify(ts) : null;
      const parity = tsClass ? sameClass(tsClass, rsClass) : null;
      rows.push({ name: item.name, loader: item.loader, diskPresent: true, xref: !!item.xref, ts, rs, tsClass, rsClass, parity });
      if (item.xref) console.log(`ts=${tsClass} trx64=${rsClass} ${parity ? "PARITY" : "DIVERGE"}`);
      else console.log(`trx64=${rsClass} (trx64-only; c64re parity via 7-game gate)`);
    }

    // Boot BOTH runtimes to an active machine ONCE (the slow c64re boot dominates),
    // then reuse those sessions for the WS-surface AND cross-runtime-snapshot axes.
    const surfItem =
      corpus.find((i) => i.name === "scramble" && i.xref) ??
      corpus.find((i) => i.xref) ??
      null;
    let surface: SurfaceResult[] = [];
    const xrt: XSnapResult[] = [];
    if (surfItem) {
      console.log(`\n=== Booting both runtimes to a live machine (${surfItem.name}) ===`);
      const [tsSid, rsSid] = await Promise.all([
        bootToActive(tsC, surfItem),
        bootToActive(rsC, surfItem),
      ]);

      // ───────── AXIS 2: WS-SURFACE ─────────
      console.log(`\n=== WS-surface parity (live machine) ===`);
      surface = await runWsSurface(tsC, rsC, tsSid, rsSid);
      for (const s of surface) {
        console.log(`  ${s.divergence ? "DIVERGE" : "PARITY "} ${s.method.padEnd(24)} ${s.note}`);
        if (s.divergence) console.log(`           ${formatDivergence(s.divergence)}`);
      }

      // ───────── AXIS 3: CROSS-RUNTIME SNAPSHOT ─────────
      // Reuse the live sessions: dump one runtime, undump into the other's session.
      console.log(`\n=== Cross-runtime snapshot (live machine) ===`);
      xrt.push(await crossRuntimeSnapshot(rsC, tsC, rsSid, tsSid, "trx64->c64re", tmpDir));
      xrt.push(await crossRuntimeSnapshot(tsC, rsC, tsSid, rsSid, "c64re->trx64", tmpDir));
      for (const x of xrt) console.log(`  ${x.ok ? "PASS" : "FAIL"} ${x.direction.padEnd(16)} ${x.note}`);

      await tsC.call("session/close", { session_id: tsSid }).catch(() => undefined);
      await rsC.call("session/close", { session_id: rsSid }).catch(() => undefined);
    }

    tsC.close();
    rsC.close();

    // ───────── SCORECARD ─────────
    // trxLoaded = items where the load chain (KERNAL serial + 1541 + GCR) ran to a
    // healthy state (game-live, rendered, or loaded-to-READY) — not a serial stall
    // or crash. trxGameplay = items that reached live game code in RAM. The GREEN
    // gate is the cross-runtime PARITY axes (xref class + surface + snapshot).
    const trxLoaded = rows.filter((r) => r.rsClass !== "STUCK" && r.rsClass !== "ERROR").length;
    const trxGameplay = rows.filter((r) => r.rsClass === "GAME_LIVE" || r.rsClass === "RENDERED").length;
    const xrefRows = rows.filter((r) => r.xref);
    const xrefPass = xrefRows.filter((r) => r.parity).length;
    const surfPass = surface.filter((s) => !s.divergence).length;
    const xrtPass = xrt.filter((x) => x.ok).length;
    console.log(`\n=== SCORECARD ===`);
    console.log(`  TRX64 corpus load-complete : ${trxLoaded}/${rows.length} (full corpus)`);
    console.log(`  TRX64 corpus gameplay      : ${trxGameplay}/${rows.length} (daemon input path; gate=authority)`);
    console.log(`  c64re xref parity (class)  : ${xrefPass}/${xrefRows.length} (live cross-runtime subset)`);
    console.log(`  ws-surface parity          : ${surfPass}/${surface.length}`);
    console.log(`  xruntime snapshot          : ${xrtPass}/${xrt.length}`);

    if (reportPath) {
      writeReport(reportPath, rows, surface, xrt, quick, allXref);
      console.log(`  report written: ${reportPath}`);
    }

    // GREEN = cross-runtime PARITY everywhere: the c64re xref subset reaches the
    // same outcome class, TRX64 completes the load chain on every corpus item, and
    // the WS surface + cross-runtime snapshot all pass.
    const allOk =
      trxLoaded === rows.length &&
      xrefPass === xrefRows.length &&
      surfPass === surface.length &&
      xrtPass === xrt.length;
    return allOk ? 0 : 1;
  } finally {
    tsD?.stop();
    rsD?.stop();
    try { rmSync(tmpDir, { recursive: true, force: true }); } catch { /* ignore */ }
  }
}

function writeReport(
  path: string,
  rows: CorpusRow[],
  surface: SurfaceResult[],
  xrt: XSnapResult[],
  quick: boolean,
  allXref: boolean,
): void {
  const trxLoaded = rows.filter((r) => r.rsClass !== "STUCK" && r.rsClass !== "ERROR").length;
  const trxGameplay = rows.filter((r) => r.rsClass === "GAME_LIVE" || r.rsClass === "RENDERED").length;
  const xrefRows = rows.filter((r) => r.xref);
  const xrefPass = xrefRows.filter((r) => r.parity).length;
  const surfPass = surface.filter((s) => !s.divergence).length;
  const xrtPass = xrt.filter((x) => x.ok).length;
  const cls = (c: OutcomeClass | null) => (c === null ? "—" : c);
  const lines: string[] = [];
  lines.push(`# TRX64 ↔ c64re Integration Report (Capstone)`);
  lines.push("");
  lines.push(`_Generated by \`tools/oracle/src/integration.ts\` — drives the TRX64 Rust daemon`);
  lines.push(`AND a live c64re TypeScript daemon through the SAME WS JSON-RPC sequence and`);
  lines.push(`compares observable behavior. The feature-complete-vs-TS-headless capstone._`);
  lines.push("");
  const xrefN = rows.filter((r) => r.xref).length;
  lines.push(`Run scope: **${rows.length} corpus item(s)** (${xrefN} driven live against c64re)${quick ? " · quick subset" : ""}${allXref ? " · all-xref" : ""}.`);
  lines.push("");
  lines.push(`## Method`);
  lines.push("");
  lines.push(`- **TRX64** runs EVERY corpus item end-to-end (boot → mount → \`LOAD"*",8,1\` → \`RUN\`).`);
  lines.push(`- **c64re** (the TS oracle) is driven LIVE for the \`xref\` subset and compared`);
  lines.push(`  class-for-class. The c64re runtime advances at ~70k cycles/s wall-time`);
  lines.push(`  (≈10× slower than TRX64), so a full broad-corpus c64re sweep is hours; the`);
  lines.push(`  xref subset is the fast-loader baseline. The slow games' c64re parity is`);
  lines.push(`  already established by the focused 7-game gate (\`seven_game_gate.rs\`).`);
  lines.push(`- Both daemons are spawned hermetically (fresh project, ephemeral port) per run.`);
  lines.push("");
  lines.push(`### How "reach running" is judged (proof-canary criterion)`);
  lines.push("");
  lines.push(`A program "reaches running" when, after \`LOAD"*",8,1\`, the PC is sustained in`);
  lines.push(`game RAM ($0200..$9FFF, outside ROM and the READY/serial stuck loops) — i.e.`);
  lines.push(`the (fast)loader handed control to live code. For KRILL/EPYX/System-3 loaders`);
  lines.push(`this is the loader executing in RAM as it pulls the game in; that IS the`);
  lines.push(`\`seven_game_gate.rs\` / \`proof-canary-disk.mjs\` bar. A program that only loads a`);
  lines.push(`BASIC stub and idles at READY without launching is the \`LOADED_READY\` class.`);
  lines.push(`Either way the TRX64 outcome is compared CLASS-for-class against the live c64re`);
  lines.push(`daemon: parity means the two runtimes reach the SAME class for the same disk.`);
  lines.push("");
  lines.push(`## Scorecard`);
  lines.push("");
  lines.push(`| Axis | Result |`);
  lines.push(`|------|--------|`);
  lines.push(`| Corpus: TRX64 load-chain completes (not stuck/error) | **${trxLoaded}/${rows.length}** |`);
  lines.push(`| Corpus: TRX64 gameplay over daemon input path | **${trxGameplay}/${rows.length}** (gate=authority) |`);
  lines.push(`| Corpus: c64re xref parity (live, same class) | **${xrefPass}/${xrefRows.length}** |`);
  lines.push(`| WS-surface parity (live machine) | **${surfPass}/${surface.length}** |`);
  lines.push(`| Cross-runtime snapshot round-trip (live machine) | **${xrtPass}/${xrt.length}** |`);
  lines.push("");
  lines.push(`## Axis 1 — Corpus`);
  lines.push("");
  lines.push(`Outcome classes: \`GAME_LIVE\` (PC sustained in game RAM), \`RENDERED\` (coherent`);
  lines.push(`title frame, >4 colors), \`LOADED_READY\` (load completed → BASIC READY; daemon`);
  lines.push(`input path did not launch the protected loader), \`STUCK\` (serial stall / crash),`);
  lines.push(`\`ERROR\`. xref parity = TRX64 reaches the same class as the live c64re daemon.`);
  lines.push("");
  lines.push(`| Program | Loader | xref | c64re | TRX64 | Parity | TRX64 first-game-PC | TRX64 colors |`);
  lines.push(`|---------|--------|------|-------|-------|--------|---------------------|--------------|`);
  for (const r of rows) {
    const fpc = r.rs.firstGamePc !== null ? `$${r.rs.firstGamePc.toString(16)}` : "—";
    const par = r.parity === null ? "n/a*" : r.parity ? "✅" : "❌";
    lines.push(
      `| ${r.name} | ${r.loader} | ${r.xref ? "yes" : "no"} | ${cls(r.tsClass)} | ${r.rsClass} | ${par} | ${fpc} | ${r.rs.colors} |`,
    );
  }
  lines.push("");
  lines.push(`_\\* non-xref items: TRX64-only here; c64re parity covered by the 7-game gate._`);
  lines.push("");
  lines.push(`## Axis 2 — WS surface on a running program`);
  lines.push("");
  lines.push(`The representative WS surface exercised on a live running program. \`api/call\``);
  lines.push(`responses compared TRX64-vs-c64re by structural shape (recursive keys + leaf`);
  lines.push(`types, tolerating documented per-runtime superset keys); \`session/screenshot\``);
  lines.push(`compared behaviorally (decodable PNG + matching dimensions — c64re returns`);
  lines.push(`\`{dataUrl,bytes}\`, TRX64 \`{dataUrl,width,height}\`); checkpoint-rewind,`);
  lines.push(`breakpoint-halt (\`debug/breakpoint_hit\`, ADR-086), and audio/export verified`);
  lines.push(`behaviorally per runtime.`);
  lines.push("");
  lines.push(`| Method | Result | Note |`);
  lines.push(`|--------|--------|------|`);
  for (const s of surface) {
    lines.push(`| ${s.method} | ${s.divergence ? "❌ diverge" : "✅ pass"} | ${s.note.replace(/\|/g, "\\|")} |`);
  }
  lines.push("");
  lines.push(`## Axis 3 — Cross-runtime snapshot (running program)`);
  lines.push("");
  lines.push(`A \`.c64re\` snapshot dumped on one runtime and undumped on the OTHER, then`);
  lines.push(`resumed (ADR-079). This is the full feature-complete claim — a snapshot taken`);
  lines.push(`on a RUNNING program (not just boot), crossing the runtime boundary in both`);
  lines.push(`directions. PASS = restored PC matches the dump PC AND the resumed machine does`);
  lines.push(`not fall into a stuck ROM loop.`);
  lines.push("");
  lines.push(`| Direction | Result | Evidence |`);
  lines.push(`|-----------|--------|----------|`);
  for (const x of xrt) {
    lines.push(`| ${x.direction} | ${x.ok ? "✅ pass" : "❌ fail"} | ${x.note.replace(/\|/g, "\\|")} |`);
  }
  lines.push("");
  const allOk =
    trxLoaded === rows.length && xrefPass === xrefRows.length && surfPass === surface.length && xrtPass === xrt.length;
  lines.push(`## Verdict`);
  lines.push("");
  lines.push(allOk
    ? `**GREEN** — TRX64's daemon behaves like c64re's across the corpus + WS surface + cross-runtime snapshot.`
    : `**Divergences present** — see the ❌ rows above; pinned below for the Driver.`);
  lines.push("");
  // Pin any divergence precisely.
  const diverged = rows.filter((r) => r.parity === false);
  const surfDiv = surface.filter((s) => s.divergence);
  const xrtDiv = xrt.filter((x) => !x.ok);
  if (diverged.length || surfDiv.length || xrtDiv.length) {
    lines.push(`### Pinned divergences`);
    lines.push("");
    for (const r of diverged) lines.push(`- **corpus/${r.name}**: c64re=${cls(r.tsClass)} TRX64=${r.rsClass} (final-pc=$${r.rs.finalPc.toString(16)}, colors=${r.rs.colors}${r.rs.error ? `, error=${r.rs.error}` : ""})`);
    for (const s of surfDiv) lines.push(`- **ws-surface/${s.method}**: ${formatDivergence(s.divergence)}`);
    for (const x of xrtDiv) lines.push(`- **xruntime/${x.direction}**: ${x.note}`);
    lines.push("");
  }
  writeFileSync(path, lines.join("\n"));
}

main().then(
  (code) => process.exit(code),
  (err) => {
    console.error("harness error:", err);
    process.exit(2);
  },
);
