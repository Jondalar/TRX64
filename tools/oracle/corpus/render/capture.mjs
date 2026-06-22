// Render gate capture/compare driver.
//
//   node capture.mjs golden            -> spawn TS daemon, boot, screenshot,
//                                         write boot-basic-ready.golden.png
//   node capture.mjs compare           -> spawn TRX64 daemon, boot, screenshot,
//                                         decode both PNGs to RGBA, PIXEL-diff.
//                                         exit 0 GREEN / 1 RED / 2 harness error.
//
// PIXEL parity, not PNG-container bytes: PNG zlib output differs between the
// Rust `png` crate and Node's encoder, so we decode both to raw RGBA and
// compare pixels (see png.mjs). This is the BASIC-ready deterministic screen
// validated pixel-for-pixel vs the TS oracle.

import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import net from "node:net";
import { WebSocket } from "ws";
import { decodePng, diffRgba } from "./png.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const C64RE_ROOT = process.env.C64RE_ROOT ?? "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP";
const TRX64_BIN = process.env.TRX64_DAEMON_BIN ?? "/Users/alex/Development/C64/Tools/TRX64/target/debug/trx64-daemon";
const GOLDEN_PNG = join(HERE, "boot-basic-ready.golden.png");
// 3.0M cycles: the C64 RAM test finishes ~2.4M, the KERNAL prints the banner +
// "READY." and drops to the keyboard wait ($E5CF) by ~2.5M. 3.0M leaves the
// machine sitting on the deterministic BASIC-ready screen (border 14, bg 6,
// screen $0400) with the cursor steady — the static screen the gate validates.
const BOOT_CYCLES = 3_000_000;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

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
  const projectDir = mkdtempSync(join(tmpdir(), `render-${kind}-`));
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

async function bootAndShoot(endpoint) {
  const ws = await wsConnect(endpoint);
  try {
    const created = await rpc(ws, "session/create", { pal: true });
    const sid = created.sessionId;
    await rpc(ws, "session/run", { session_id: sid, cycles: BOOT_CYCLES });
    const shot = await rpc(ws, "session/screenshot", { session_id: sid });
    if (!shot || !shot.dataUrl) throw new Error("screenshot returned no dataUrl");
    const b64 = shot.dataUrl.replace(/^data:image\/png;base64,/, "");
    return { png: Buffer.from(b64, "base64"), width: shot.width, height: shot.height };
  } finally {
    ws.close();
  }
}

async function main() {
  const cmd = process.argv[2];
  if (cmd === "golden") {
    const d = await spawnDaemon("ts");
    try {
      const { png } = await bootAndShoot(d.endpoint);
      writeFileSync(GOLDEN_PNG, png);
      const dec = decodePng(png);
      console.log(`recorded golden: ${GOLDEN_PNG} (${dec.width}x${dec.height}, ${png.length} bytes)`);
    } finally { d.stop(); }
    return 0;
  }
  if (cmd === "compare") {
    if (!existsSync(GOLDEN_PNG)) { console.error("no golden; run `node capture.mjs golden` first"); return 2; }
    const golden = decodePng(readFileSync(GOLDEN_PNG));
    const d = await spawnDaemon("trx64");
    let cand;
    try { cand = await bootAndShoot(d.endpoint); } finally { d.stop(); }
    const candDec = decodePng(cand.png);
    const div = diffRgba(golden.width, golden.height, golden.rgba, candDec.width, candDec.height, candDec.rgba);
    if (!div) {
      console.log(`[render-boot-basic-ready] GREEN — pixel-identical ${golden.width}x${golden.height}`);
      return 0;
    }
    if (div.dim) {
      console.log(`[render-boot-basic-ready] RED — dimension mismatch: expected ${div.expected} got ${div.got}`);
      return 1;
    }
    console.log(`[render-boot-basic-ready] RED — first pixel divergence at (${div.x},${div.y}): expected RGBA ${JSON.stringify(div.expected)} got ${JSON.stringify(div.got)} (${div.totalDiff} px differ of ${golden.width * golden.height})`);
    return 1;
  }
  console.error("usage: node capture.mjs <golden|compare>");
  return 2;
}

main().then((c) => process.exit(c), (e) => { console.error("harness error:", e); process.exit(2); });
