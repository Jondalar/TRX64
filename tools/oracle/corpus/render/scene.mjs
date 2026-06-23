// Render-gate scene harness — shared helpers for VIC pixel-parity scenarios
// beyond the BASIC-ready boot screen (sprites + graphics modes + border edges).
//
// A "scene" boots a daemon to the deterministic BASIC-ready screen, then injects
// a small CPU program via monitor/exec ("wr" to poke bytes into RAM, "r pc=" to
// point the PC at it), runs a short fixed cycle budget so the program executes
// and leaves a STATIC frame, then screenshots. Identical program on the TS oracle
// daemon and the TRX64 daemon ⇒ identical displayed frame iff the renderers agree.
//
// PIXEL parity (not PNG bytes): decode both PNGs to RGBA and diff (see png.mjs).
//
// This file only adds helpers + a generic compare runner. The render gate's
// existing capture.mjs (boot-basic-ready) is untouched.

import { spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import net from "node:net";
import { WebSocket } from "ws";
import { decodePng, diffRgba } from "./png.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
export const C64RE_ROOT = process.env.C64RE_ROOT ?? "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP";
export const TRX64_BIN = process.env.TRX64_DAEMON_BIN ?? "/Users/alex/Development/C64/Tools/TRX64/target/debug/trx64-daemon";

// Boot to the steady BASIC-ready screen (same constant as capture.mjs).
export const BOOT_CYCLES = 3_000_000;

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

export function rpc(ws, method, params) {
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

export async function spawnDaemon(kind) {
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

// Apply a scene's setup. A scene programs the VIC + screen/colour/sprite RAM via
// monitor `wr io` writes (the I/O lens routes $D0xx → VIC chip and $D800 → colour
// RAM, RAM addresses → RAM — identical effect on the TS oracle, whose `wr` runs
// the banked CPU write path with real I/O effects when I/O is mapped). This needs
// NO running CPU program: the registers + memory the (state-)renderer reads are
// set directly, so the post-setup frame is fully deterministic on both daemons.
//
// `scene.setup` is an array of monitor command strings (e.g. "wr io d015 01").
// Optionally `scene.code`/`scene.org`/`scene.runCycles` inject + run a CPU
// program too (legacy path; kept for scenarios that need live execution).
// One PAL frame = 312 lines × 63 cycles = 19656 cycles. After programming the
// VIC via `wr io`, we run ≥1 full frame so the TS oracle's per-cycle literal port
// re-renders the framebuffer WITH the new register state (its screenshot reflects
// the last rendered frame, not the live registers). TRX64's renderer is a
// state-render that reads the registers directly, so the extra cycles are inert
// there — but running them on both keeps the harness symmetric.
const FRAME_CYCLES = 19656;

// Apply a scene's setup. `kind` = "ts" | "trx64".
//
// The TS oracle screenshot reflects the LAST per-cycle-rendered frame, so after
// programming the VIC via `wr io` we must run ≥1 full frame for its literal port
// to re-render with the new register state — but a free CPU run after boot would
// also blink the cursor + let the KERNAL touch state, desyncing from TRX64. To
// keep the frame deterministic we run the VIC ON A HALTED CPU: point the PC at a
// JMP-self ($60 RTS would unwind; we inject `4C xx xx`) so cycles elapse, the VIC
// renders, but no KERNAL code runs. TRX64's renderer reads the registers directly
// and needs no run — and its flat injected bus has no ROM — so for TRX64 we skip
// the run entirely and screenshot the state-rendered frame.
export async function applyScene(ws, sid, scene, kind) {
  for (const cmd of scene.setup ?? []) {
    await rpc(ws, "monitor/exec", { session_id: sid, command: cmd });
  }
  if (scene.code && scene.code.length) {
    const hex = scene.code.map((b) => b.toString(16).padStart(2, "0")).join(" ");
    await rpc(ws, "monitor/exec", { session_id: sid, command: `wr ${scene.org.toString(16)} ${hex}` });
    await rpc(ws, "monitor/exec", { session_id: sid, command: `r pc=${scene.org.toString(16)}` });
    await rpc(ws, "session/run", { session_id: sid, cycles: scene.runCycles ?? FRAME_CYCLES });
    return;
  }
  if (scene.setup && scene.setup.length && kind === "ts") {
    // Park the CPU on a JMP-self in low RAM so frames elapse without KERNAL code
    // mutating the screen / cursor. $033C cassette buffer is free here.
    await rpc(ws, "monitor/exec", { session_id: sid, command: "wr 033c 4c 3c 03" });
    await rpc(ws, "monitor/exec", { session_id: sid, command: "r pc=033c" });
    await rpc(ws, "session/run", { session_id: sid, cycles: scene.runCycles ?? FRAME_CYCLES * 2 });
  }
}

// Boot + apply scene + screenshot. `scene` = { setup?, code?, org?, runCycles?, bootCycles? }.
export async function captureScene(endpoint, scene, kind) {
  const ws = await wsConnect(endpoint);
  try {
    const created = await rpc(ws, "session/create", { pal: true });
    const sid = created.sessionId;
    await rpc(ws, "session/run", { session_id: sid, cycles: scene.bootCycles ?? BOOT_CYCLES });
    await applyScene(ws, sid, scene, kind);
    const shot = await rpc(ws, "session/screenshot", { session_id: sid });
    if (!shot || !shot.dataUrl) throw new Error("screenshot returned no dataUrl");
    const b64 = shot.dataUrl.replace(/^data:image\/png;base64,/, "");
    return { png: Buffer.from(b64, "base64"), width: shot.width, height: shot.height };
  } finally {
    ws.close();
  }
}

export { decodePng, diffRgba, wsConnect };
