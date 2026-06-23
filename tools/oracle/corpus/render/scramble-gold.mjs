// scramble-gold — the custom-$DD00-loader BEHAVIORAL acid test (TS-vs-TRX64).
//
//   node scramble-gold.mjs            -> run all stages, report GREEN/RED each.
//   node scramble-gold.mjs <stage..>  -> run the named stage(s) only.
//   node scramble-gold.mjs --dump     -> on RED, write both framebuffers (PNG)
//                                        + an RGBA diff mask to ./scramble-gold-out/.
//
// WHY THIS GATE EXISTS
// --------------------
// scramble_infinity.d64 ships a custom $DD00 (CIA2) serial loader. The only
// remaining RED on this title, `scramble-load-progress`, is a CYCLE-EXACT
// nitpick — a ~12-20k-cycle sample-boundary phase artifact upstream of the
// track-1 read (ADR-047 proved the rotation engine + sync-lock are bit-identical
// to the c64re reference). That gate never answers the real question: does the
// loader LOAD, RUN, and RENDER on TRX64?
//
// This is the c64re team's BEHAVIORAL proof recycled as a TS-vs-TRX64
// differential (cf. scripts/diff-scramble-vs-vice.mjs + probe-scramble-stages.mjs
// in the C64RE repo, which diff stage SCREENSHOTS vs VICE). We drive the SAME
// boot -> LOAD"*",8,1 -> RUN -> settle-to-stage sequence on BOTH hermetic daemons
// (TS golden + TRX64) over the identical WS protocol, screenshot at each stage,
// and PIXEL-diff the 384x272 framebuffers (decoded RGBA, never PNG bytes — zlib
// output differs between the Rust `png` crate and Node, see png.mjs).
//
// GREEN  = the loader-bar (and later) stages are PIXEL-IDENTICAL TS-vs-TRX64 ⇒
//          the custom $DD00 loader runs pixel-exact on TRX64. The cycle-exact
//          `scramble-load-progress` then stands as a documented sample-boundary
//          known-RED, NOT a functional gap.
// RED    = a stage diverges ⇒ the REAL custom-loader bug (behavioral). The gate
//          reports the first divergent stage, WHERE on screen it differs (the
//          divergent bounding box + a raster-region label), and dumps both
//          framebuffers (with --dump) so the visual fault is the fix target.

import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import net from "node:net";
import { WebSocket } from "ws";
import { decodePng, diffRgba } from "./png.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const C64RE_ROOT = process.env.C64RE_ROOT ?? "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP";
const TRX64_BIN = process.env.TRX64_DAEMON_BIN ?? "/Users/alex/Development/C64/Tools/TRX64/target/debug/trx64-daemon";
const SCRAMBLE_D64 = process.env.SCRAMBLE_D64 ?? join(C64RE_ROOT, "samples/scramble_infinity.d64");
const OUT_DIR = join(HERE, "scramble-gold-out");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ── stage script (mirrors probe-scramble-stages.mjs captureStage budgets) ──────
//
// boot:   KERNAL cold boot to BASIC ready (~2.4M RAM test + banner). The probe
//         uses 5M before mount; we mount the disk image (no drive spin needed to
//         INGRESS) then give it that headroom.
// load:   LOAD"*",8,1 then a long run for the directory + first-file load. The
//         probe waits 60M after typing LOAD; the custom loader takes over $DD00.
// run:    RUN\r hands control to the loaded code.
// Then each stage runs a SETTLE budget and screenshots a STATIC frame.
//
// Cycle budgets are deliberately the probe's: stage "loaderbar" settles 30M after
// RUN (the custom loader paints its raster bar by then). Both daemons run the
// EXACT same fixed budgets ⇒ same machine state ⇒ the only difference that can
// remain is a renderer / CPU / drive divergence.
const BOOT_CYCLES = 5_000_000;
const POST_MOUNT_CYCLES = 2_000_000;
const LOAD_WAIT_CYCLES = 60_000_000;

// Each stage = a label + the EXTRA settle cycles to run after the previous stage
// before screenshotting. The first stage's settle runs right after RUN\r.
// Keep the loader-bar stage first (the cheapest, most decisive); deeper stages
// are gated on it being clean.
const STAGES = [
  // stage 0 — the custom loader's raster bar ("loaderbar"). 30M settle after RUN.
  { name: "loaderbar", settle: 30_000_000 },
  // stage 1 — deeper into the loader / credits screen. probe waits 150M here.
  // Only meaningful if stage 0 is GREEN (added per the "1-2 later stages" brief).
  { name: "credits", settle: 150_000_000 },
  // stage 2 — first frame after pressing FIRE/SPACE to leave credits. Runs an
  // extra 60M. Kept last; the loader-bar stage is the headline result.
  { name: "post-space", settle: 60_000_000, pressSpace: true },
];

// ── WS plumbing (same shape as scene.mjs / capture.mjs) ────────────────────────
function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address();
      srv.close(() => resolve(addr.port));
    });
  });
}

function wsConnect(endpoint, timeoutMs = 4000) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(endpoint);
    const t = setTimeout(() => { ws.terminate(); reject(new Error("ws timeout")); }, timeoutMs);
    ws.on("open", () => { clearTimeout(t); resolve(ws); });
    ws.on("error", (e) => { clearTimeout(t); reject(e); });
  });
}

// rpc has NO response timeout on purpose: a single session/run can be tens of
// millions of cycles (seconds of wall time, esp. on the tsx TS daemon). The WS
// connection stays open; we just await the JSON-RPC reply.
function rpc(ws, method, params) {
  return new Promise((resolve, reject) => {
    const id = Math.floor(Math.random() * 1e9);
    const onMsg = (data) => {
      const msg = JSON.parse(data.toString());
      if (msg.id !== id) return;
      ws.off("message", onMsg);
      if (msg.error) reject(new Error(`${method}: ${JSON.stringify(msg.error)}`));
      else resolve(msg.result);
    };
    ws.on("message", onMsg);
    ws.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
  });
}

async function spawnDaemon(kind) {
  const port = await freePort();
  const projectDir = mkdtempSync(join(tmpdir(), `scramble-${kind}-`));
  const endpoint = `ws://127.0.0.1:${port}`;
  let child;
  if (kind === "ts") {
    child = spawn("node_modules/.bin/tsx",
      ["src/runtime/headless/daemon/run.ts", "--project", projectDir, "--port", String(port)],
      { cwd: C64RE_ROOT, stdio: "ignore", detached: true, env: { ...process.env, C64RE_RUNTIME_AUTOSTART: "0" } });
  } else {
    child = spawn(TRX64_BIN, ["--project", projectDir, "--port", String(port)], { stdio: "ignore", detached: true });
  }
  const stop = () => {
    try { if (child.pid) process.kill(-child.pid, "SIGTERM"); } catch { try { child.kill("SIGTERM"); } catch {} }
    try { rmSync(projectDir, { recursive: true, force: true }); } catch {}
  };
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline) {
    try { const c = await wsConnect(endpoint, 2000); await rpc(c, "ping", {}).catch(() => {}); c.close(); return { endpoint, stop }; }
    catch { await sleep(250); }
  }
  stop();
  throw new Error(`daemon ${kind} not ready`);
}

async function screenshot(ws, sid) {
  const shot = await rpc(ws, "session/screenshot", { session_id: sid });
  if (!shot || !shot.dataUrl) throw new Error("screenshot returned no dataUrl");
  const b64 = shot.dataUrl.replace(/^data:image\/png;base64,/, "");
  return { png: Buffer.from(b64, "base64"), width: shot.width, height: shot.height };
}

// Drive ONE daemon through boot -> mount -> LOAD -> RUN -> each requested stage,
// screenshotting at every stage. Returns { [stageName]: {png,width,height} }.
// `stages` is the filtered, IN-ORDER list (cumulative settle — later stages run
// ON TOP of earlier ones, exactly like the probe's sequential captureStage).
async function runStages(endpoint, stages) {
  const ws = await wsConnect(endpoint);
  const shots = {};
  try {
    const created = await rpc(ws, "session/create", { pal: true });
    const sid = created.sessionId;
    // 1. boot to BASIC ready.
    await rpc(ws, "session/run", { session_id: sid, cycles: BOOT_CYCLES });
    // 2. mount the scramble disk via the single ingress authority (kind:disk),
    //    then settle so the READY. prompt is stable before typing.
    await rpc(ws, "media/ingress", { session_id: sid, kind: "disk", path: SCRAMBLE_D64 });
    await rpc(ws, "session/run", { session_id: sid, cycles: POST_MOUNT_CYCLES });
    // 3. LOAD"*",8,1 — the custom loader. Long wait for the directory + first file.
    await rpc(ws, "session/type", { session_id: sid, text: 'LOAD"*",8,1\r' });
    await rpc(ws, "session/run", { session_id: sid, cycles: LOAD_WAIT_CYCLES });
    // 4. RUN\r — hand control to the loaded code.
    await rpc(ws, "session/type", { session_id: sid, text: "RUN\r" });
    // 5. each stage: settle the budget (+ optional SPACE press) then screenshot.
    for (const st of stages) {
      if (st.pressSpace) {
        await rpc(ws, "session/type", { session_id: sid, text: " " });
      }
      await rpc(ws, "session/run", { session_id: sid, cycles: st.settle });
      shots[st.name] = await screenshot(ws, sid);
    }
    return shots;
  } finally {
    ws.close();
  }
}

// ── divergence localisation ────────────────────────────────────────────────────
// The 384x272 canvas: the 320x200 display window lands at (32,35) (see README).
// Map a Y coord to a coarse on-screen region so a RED report says WHERE.
function regionLabel(x, y) {
  const DISP_X0 = 32, DISP_Y0 = 35, DISP_W = 320, DISP_H = 200;
  const inWin = x >= DISP_X0 && x < DISP_X0 + DISP_W && y >= DISP_Y0 && y < DISP_Y0 + DISP_H;
  if (!inWin) return "BORDER";
  const ry = y - DISP_Y0;
  if (ry < DISP_H / 3) return "display(top-third)";
  if (ry < (2 * DISP_H) / 3) return "display(middle-third)";
  return "display(bottom-third)";
}

// Full bounding box + per-region histogram of all differing pixels.
function analyzeDiff(w, h, a, b) {
  let minX = w, minY = h, maxX = -1, maxY = -1, total = 0;
  const regions = new Map();
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      const i = (y * w + x) * 4;
      if (a[i] !== b[i] || a[i + 1] !== b[i + 1] || a[i + 2] !== b[i + 2] || a[i + 3] !== b[i + 3]) {
        total++;
        if (x < minX) minX = x; if (x > maxX) maxX = x;
        if (y < minY) minY = y; if (y > maxY) maxY = y;
        const r = regionLabel(x, y);
        regions.set(r, (regions.get(r) ?? 0) + 1);
      }
    }
  }
  return { total, box: { minX, minY, maxX, maxY }, regions };
}

function dumpStage(stage, golden, cand, diff) {
  mkdirSync(OUT_DIR, { recursive: true });
  // Both daemons hand us real PNG bytes; write them verbatim (the visual fault is
  // for human eyes, so container bytes are fine — pixel-diff already happened).
  writeFileSync(join(OUT_DIR, `scramble-${stage}-ts-golden.png`), golden.pngBytes);
  writeFileSync(join(OUT_DIR, `scramble-${stage}-trx64.png`), cand.pngBytes);
  // A diff mask: differing pixels white, equal pixels black (raw PNG via the
  // golden's decoded geometry; encode as a P8-style PPM-in-PNG is overkill, so
  // we emit a simple raw .rgba sidecar the README documents how to view).
  const { width: w, height: h } = golden;
  const mask = Buffer.alloc(w * h * 4);
  for (let i = 0; i < w * h; i++) {
    const o = i * 4;
    const d = golden.rgba[o] !== cand.rgba[o] || golden.rgba[o + 1] !== cand.rgba[o + 1]
      || golden.rgba[o + 2] !== cand.rgba[o + 2] || golden.rgba[o + 3] !== cand.rgba[o + 3];
    mask[o] = mask[o + 1] = mask[o + 2] = d ? 0xff : 0x00;
    mask[o + 3] = 0xff;
  }
  writeFileSync(join(OUT_DIR, `scramble-${stage}-diffmask-${w}x${h}.rgba`), mask);
  return OUT_DIR;
}

// ── runner ─────────────────────────────────────────────────────────────────────
async function main() {
  const args = process.argv.slice(2);
  const dump = args.includes("--dump");
  const wanted = args.filter((a) => !a.startsWith("--"));
  // Preserve STAGE order (cumulative settle); filter by requested names. If a
  // later stage is requested we must run the earlier ones to reach it, so we
  // always run the prefix UP TO the last requested stage, but only REPORT the
  // requested set.
  let stages = STAGES;
  if (wanted.length) {
    for (const w of wanted) {
      if (!STAGES.some((s) => s.name === w)) { console.error(`unknown stage '${w}'`); return 2; }
    }
    const lastIdx = Math.max(...wanted.map((w) => STAGES.findIndex((s) => s.name === w)));
    stages = STAGES.slice(0, lastIdx + 1);
  }
  const reportSet = wanted.length ? new Set(wanted) : new Set(STAGES.map((s) => s.name));

  console.log(`[scramble-gold] disk ${SCRAMBLE_D64}`);
  console.log(`[scramble-gold] stages: ${stages.map((s) => s.name).join(" -> ")} (reporting: ${[...reportSet].join(", ")})`);

  // Run BOTH daemons through the full sequence (TS golden first, then TRX64).
  const ts = await spawnDaemon("ts");
  let tsShots;
  try { tsShots = await runStages(ts.endpoint, stages); } finally { ts.stop(); }

  const trx = await spawnDaemon("trx64");
  let trxShots;
  try { trxShots = await runStages(trx.endpoint, stages); } finally { trx.stop(); }

  let anyRed = false;
  for (const st of stages) {
    if (!reportSet.has(st.name)) continue;
    const g = decodePng(tsShots[st.name].png);
    const c = decodePng(trxShots[st.name].png);
    const div = diffRgba(g.width, g.height, g.rgba, c.width, c.height, c.rgba);
    if (!div) {
      console.log(`[scramble-${st.name}] GREEN — pixel-identical ${g.width}x${g.height}`);
      continue;
    }
    anyRed = true;
    if (div.dim) {
      console.log(`[scramble-${st.name}] RED — dimension mismatch: TS ${div.expected} vs TRX64 ${div.got}`);
      continue;
    }
    const a = analyzeDiff(g.width, g.height, g.rgba, c.rgba);
    const regs = [...a.regions.entries()].sort((x, y) => y[1] - x[1])
      .map(([r, n]) => `${r}:${n}`).join(" ");
    console.log(`[scramble-${st.name}] RED — ${a.total} px differ of ${g.width * g.height}`);
    console.log(`    first divergence (${div.x},${div.y}) [${regionLabel(div.x, div.y)}]: TS RGBA ${JSON.stringify(div.expected)} vs TRX64 ${JSON.stringify(div.got)}`);
    console.log(`    bounding box: x[${a.box.minX}..${a.box.maxX}] y[${a.box.minY}..${a.box.maxY}]`);
    console.log(`    by region: ${regs}`);
    if (dump) {
      const out = dumpStage(st.name,
        { ...g, pngBytes: tsShots[st.name].png },
        { ...c, pngBytes: trxShots[st.name].png }, a);
      console.log(`    dumped TS + TRX64 PNGs + diff mask -> ${out}`);
    } else {
      console.log(`    (re-run with --dump to write both framebuffers + a diff mask)`);
    }
  }
  return anyRed ? 1 : 0;
}

main().then((c) => process.exit(c), (e) => { console.error("harness error:", e); process.exit(2); });
