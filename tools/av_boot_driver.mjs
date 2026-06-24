#!/usr/bin/env node
// av_boot_driver.mjs — drive the TRX64 daemon to mount + LOAD"*",8,1 + RUN +
// warm up to the SCRAMBLE INFINITY title, then HOLD the connection open so the
// daemon's auto-started streaming loop keeps the title running live (the tap
// taps the same singleton machine state). Mirrors the scramble_av_record.rs
// harness boot flow, but over the WS JSON-RPC surface.
//
//   node av_boot_driver.mjs            # boot + hold (Ctrl-C to release)
//   HOLD_MS=40000 node av_boot_driver.mjs
//
// The daemon advances the machine in real-time via the streaming loop once any
// client connects, so the explicit run/* below is the FAST boot bootstrap; after
// it returns BASIC-ready + RUN, we stop issuing runs and let the loop carry it.

// `ws` lives in the C64ReverseEngineeringMCP node_modules (alongside ws-av-tap.mjs);
// ESM bare-specifier resolution is relative to THIS file, so import by absolute path
// (override with WS_MODULE if your checkout differs).
const WS_MODULE =
  process.env.WS_MODULE ||
  "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/node_modules/ws/index.js";
const { default: WebSocket } = await import(WS_MODULE);

const URL = process.env.WS || "ws://127.0.0.1:4312";
const DISK =
  process.env.DISK ||
  "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/scramble_infinity.d64";
const HOLD_MS = Number(process.env.HOLD_MS || 0); // 0 = hold until killed

let id = 1;
const ws = new WebSocket(URL);
ws.binaryType = "nodebuffer";

function call(method, params = {}) {
  return new Promise((resolve, reject) => {
    const myId = id++;
    const onMsg = (data, isBinary) => {
      if (isBinary) return; // ignore the A/V push the daemon sends us
      let m;
      try { m = JSON.parse(data.toString()); } catch { return; }
      if (m.id !== myId) return;
      ws.off("message", onMsg);
      if (m.error) reject(new Error(`${method}: ${m.error.message}`));
      else resolve(m.result);
    };
    ws.on("message", onMsg);
    ws.send(JSON.stringify({ jsonrpc: "2.0", id: myId, method, params }));
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

ws.on("open", async () => {
  console.log("[boot] connected", URL);
  await call("ping");
  console.log("[boot] mounting", DISK);
  const mnt = await call("media/mount", { path: DISK });
  console.log("[boot] mounted:", mnt.type, mnt.sha256?.slice(0, 12));

  // Boot ROMs to BASIC ready (the daemon boots paused at reset).
  await call("session/run", { cycles: 2_500_000 });
  await call("session/run", { cycles: 500_000 });

  // Type LOAD"*",8,1 then run it to completion (drive serial load is slow).
  console.log('[boot] typing LOAD"*",8,1');
  await call("session/type", { text: 'LOAD"*",8,1\r' });
  for (let i = 0; i < 60; i++) {
    await call("session/run", { cycles: 1_000_000 });
  }

  console.log("[boot] typing RUN");
  await call("session/type", { text: "RUN\r" });
  // Warm up to the title + tune (the loader + title come up several M cyc later).
  for (let i = 0; i < 25; i++) {
    await call("session/run", { cycles: 1_000_000 });
  }

  const st = await call("session/state");
  console.log(
    `[boot] warmed up: PC=$${(st.cpu.pc >>> 0).toString(16)} border=${st.vic.border} bg=${st.vic.background}`
  );
  console.log("[boot] HOLDING connection — daemon streaming loop now drives the title live.");
  if (HOLD_MS > 0) {
    await sleep(HOLD_MS);
    console.log("[boot] hold elapsed, closing.");
    ws.close();
    process.exit(0);
  }
  // else: hold forever (Ctrl-C).
});

ws.on("error", (e) => { console.error("[boot] ws error:", e.message); process.exit(1); });
process.on("SIGINT", () => { try { ws.close(); } catch {} process.exit(0); });
