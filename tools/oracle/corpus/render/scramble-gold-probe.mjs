// Throwaway diagnostic (NOT a gate): drive ONE daemon through boot->load->RUN and
// screenshot at a sweep of post-RUN cycle checkpoints, to locate the loader stage
// each machine is in at a given cycle. Used to characterise the TS-vs-TRX64 loader
// THROUGHPUT delta the scramble-gold loaderbar RED exposed (TS overshoots to the
// title screen at 30M while TRX64 is still on the loader bar).
//
//   node scramble-gold-probe.mjs ts|trx64 <Mcyc..>
//
// Writes scramble-gold-out/probe-<kind>-<cyc>.png per checkpoint.

import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import net from "node:net";
import { WebSocket } from "ws";

const HERE = dirname(fileURLToPath(import.meta.url));
const C64RE_ROOT = process.env.C64RE_ROOT ?? "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP";
const TRX64_BIN = process.env.TRX64_DAEMON_BIN ?? "/Users/alex/Development/C64/Tools/TRX64/target/debug/trx64-daemon";
const SCRAMBLE_D64 = process.env.SCRAMBLE_D64 ?? join(C64RE_ROOT, "samples/scramble_infinity.d64");
const OUT_DIR = join(HERE, "scramble-gold-out");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => { const a = srv.address(); srv.close(() => resolve(a.port)); });
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
      if (msg.error) reject(new Error(`${method}: ${JSON.stringify(msg.error)}`)); else resolve(msg.result);
    };
    ws.on("message", onMsg);
    ws.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
  });
}
async function spawnDaemon(kind) {
  const port = await freePort();
  const projectDir = mkdtempSync(join(tmpdir(), `probe-${kind}-`));
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
  stop(); throw new Error(`daemon ${kind} not ready`);
}
async function shoot(ws, sid, tag, kind) {
  const shot = await rpc(ws, "session/screenshot", { session_id: sid });
  const b64 = shot.dataUrl.replace(/^data:image\/png;base64,/, "");
  mkdirSync(OUT_DIR, { recursive: true });
  const p = join(OUT_DIR, `probe-${kind}-${tag}.png`);
  writeFileSync(p, Buffer.from(b64, "base64"));
  console.log(`  ${kind} @ ${tag}: ${p}`);
}

async function main() {
  const kind = process.argv[2];
  const checkpointsM = process.argv.slice(3).map(Number);
  if (!["ts", "trx64"].includes(kind) || !checkpointsM.length) {
    console.error("usage: node scramble-gold-probe.mjs ts|trx64 <Mcyc..>"); return 2;
  }
  const d = await spawnDaemon(kind);
  try {
    const ws = await wsConnect(d.endpoint);
    const sid = (await rpc(ws, "session/create", { pal: true })).sessionId;
    await rpc(ws, "session/run", { session_id: sid, cycles: 5_000_000 });
    await rpc(ws, "media/ingress", { session_id: sid, kind: "disk", path: SCRAMBLE_D64 });
    await rpc(ws, "session/run", { session_id: sid, cycles: 2_000_000 });
    await rpc(ws, "session/type", { session_id: sid, text: 'LOAD"*",8,1\r' });
    await rpc(ws, "session/run", { session_id: sid, cycles: 60_000_000 });
    await rpc(ws, "session/type", { session_id: sid, text: "RUN\r" });
    let ran = 0;
    for (const m of checkpointsM.sort((a, b) => a - b)) {
      const target = m * 1_000_000;
      const delta = target - ran;
      if (delta > 0) { await rpc(ws, "session/run", { session_id: sid, cycles: delta }); ran = target; }
      await shoot(ws, sid, `${m}M`, kind);
    }
    ws.close();
  } finally { d.stop(); }
  return 0;
}
main().then((c) => process.exit(c), (e) => { console.error("probe error:", e); process.exit(2); });
