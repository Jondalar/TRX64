#!/usr/bin/env node
// av_file_recorder.mjs — VALIDATION recorder for the TRX64 daemon's live A/V push.
//
// Decodes the daemon's binary WS stream with the EXACT same logic as the user's
// ws-av-tap.mjs (BIN_VIC=0x01 fmt-1 palette-indexed → RGBA, BIN_AUDIO=0x02 raw
// s16le stereo), but writes the decoded RGBA + PCM to PLAIN FILES (not fifos), so
// finalization is a clean fs close with no ffmpeg-fifo-EOF dependency. After
// DURATION_MS it closes the socket + files and (if ffmpeg is present) muxes an
// H264+AAC .mp4 offline. This isolates "does the daemon's push decode + record to
// a correct-speed mp4 with reSID audio" from the tap's own fifo-finalize quirk.
//
//   WS=ws://127.0.0.1:4399 OUT=/path/out.mp4 DURATION_MS=22000 node av_file_recorder.mjs

const WS_MODULE =
  process.env.WS_MODULE ||
  "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/node_modules/ws/index.js";
const { default: WebSocket } = await import(WS_MODULE);
import { createWriteStream } from "node:fs";
import { spawn } from "node:child_process";

const URL = process.env.WS || "ws://127.0.0.1:4312";
const OUT = process.env.OUT || "/Users/alex/Development/C64/Tools/TRX64/traces/trx64_av_tap.mp4";
const DURATION_MS = Number(process.env.DURATION_MS || 22000);
const V_RAW = "/tmp/trx64rec_v.rgba";
const A_RAW = "/tmp/trx64rec_a.pcm";

const BIN_VIC = 0x01, BIN_AUDIO = 0x02;
const SR = 44100, VW = 384, VH = 272, FPS = 50;

// Identical decode to ws-av-tap.mjs's decodeVic (fmt 1 palette-indexed → RGBA).
function decodeVic(payload) {
  const dv = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
  const w = dv.getUint16(0, true), h = dv.getUint16(2, true), fmt = payload[4];
  if (fmt !== 1 || !w || !h) return null;
  const palOff = 10, idxOff = 58, n = w * h;
  if (payload.length < idxOff + n) return null;
  const rgba = Buffer.allocUnsafe(n * 4);
  for (let p = 0; p < n; p++) {
    const idx = payload[idxOff + p] & 0x0f;
    const pe = palOff + idx * 3, o = p * 4;
    rgba[o] = payload[pe]; rgba[o + 1] = payload[pe + 1]; rgba[o + 2] = payload[pe + 2]; rgba[o + 3] = 0xff;
  }
  return { w, h, rgba };
}

const vs = createWriteStream(V_RAW);
const as = createWriteStream(A_RAW);
let aFrames = 0, aBytes = 0, vFrames = 0, aPeak = 0;

const stats = setInterval(() => {
  console.log(`[rec] 2s: audio=${aFrames} (~${(aFrames / 2).toFixed(0)}/s) video=${vFrames} (~${(vFrames / 2).toFixed(0)}/s) peak=${aPeak}`);
  aFrames = 0; vFrames = 0;
}, 2000);

console.log(`[rec] connecting ${URL} (record ${DURATION_MS}ms → ${OUT}) …`);
const ws = new WebSocket(URL);
ws.binaryType = "nodebuffer";
ws.on("open", () => console.log("[rec] connected (passive, read-only)."));
ws.on("error", (e) => { console.error("[rec] ws error:", e.message); process.exit(1); });
ws.on("message", (data, isBinary) => {
  if (!isBinary || data.length < 5) return;
  const type = data[0];
  const payload = data.subarray(5); // [type:u8][seq:u32]
  if (type === BIN_AUDIO) {
    aFrames++; aBytes += payload.length; vs;
    // peak |amplitude| over the s16le payload (prove not silent).
    for (let i = 0; i + 1 < payload.length; i += 2) {
      let s = payload[i] | (payload[i + 1] << 8);
      if (s & 0x8000) s -= 0x10000;
      const a = Math.abs(s);
      if (a > aPeak) aPeak = a;
    }
    as.write(Buffer.from(payload));
  } else if (type === BIN_VIC) {
    const f = decodeVic(payload);
    if (f) { vFrames++; vs.write(f.rgba); }
  }
});

setTimeout(() => {
  console.log("[rec] duration reached — closing socket + files, muxing.");
  clearInterval(stats);
  try { ws.close(); } catch {}
  vs.end(); as.end();
  let pending = 2;
  const onClose = () => { if (--pending === 0) mux(); };
  vs.on("finish", onClose); as.on("finish", onClose);
}, DURATION_MS);

function mux() {
  console.log(`[rec] audioPeak=${aPeak} (i16 full-scale=32767) — ${aPeak > 0 ? "NOT silent" : "SILENT!"}`);
  const ff = spawn("/opt/homebrew/bin/ffmpeg", [
    "-y",
    "-f", "rawvideo", "-pixel_format", "rgba", "-video_size", `${VW}x${VH}`, "-framerate", String(FPS), "-i", V_RAW,
    "-f", "s16le", "-ar", String(SR), "-ac", "2", "-i", A_RAW,
    "-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", "fast", "-crf", "20",
    "-c:a", "aac", "-b:a", "192k", "-shortest", "-movflags", "+faststart",
    OUT,
  ], { stdio: ["ignore", "inherit", "inherit"] });
  ff.on("close", (code) => { console.log(`[rec] ffmpeg exit ${code} → ${OUT}`); process.exit(code === 0 ? 0 : 1); });
  ff.on("error", (e) => { console.error("[rec] ffmpeg failed:", e.message); process.exit(1); });
}

process.on("SIGINT", () => { try { ws.close(); } catch {} process.exit(0); });
