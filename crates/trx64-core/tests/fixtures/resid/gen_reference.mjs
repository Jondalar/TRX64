#!/usr/bin/env node
// Generate the reSID audio ORACLE reference for TRX64.
//
// Drives c64re's vendored GPL reSID — compiled to WASM by
// scripts/build-resid-wasm.mjs (src/runtime/headless/sid/wasm/resid.mjs) — with
// a fixed SID write/boundary sequence and the SAME post-reset configuration the
// TS ResidWasm engine uses (resid-wasm-engine.ts configure()). The emitted Int16
// PCM is written as a little-endian s16 blob + a JSON manifest. The TRX64 Rust
// FFI test (tests/resid_oracle.rs) replays the IDENTICAL sequence through the
// SAME C++ compiled NATIVELY and asserts byte-identical PCM.
//
// This is the byte-identity proof of FFI-ing the same reSID: c64re-WASM vs
// TRX64-native, same source, same shim, same config → same samples.
//
// Run from the c64re repo (it owns the committed WASM):
//   node gen_reference.mjs <c64re_repo_root> <out_dir>

import { writeFileSync } from "node:fs";
import { resolve } from "node:path";

const repoRoot = resolve(process.argv[2] ?? ".");
const outDir = resolve(process.argv[3] ?? ".");

const CLOCK = 985248;     // PAL_CLOCK_FREQ
const SAMPLE_RATE = 44100; // DEFAULT_SAMPLE_RATE
const RESAMPLE = 2;        // sampling_method RESAMPLE
const MODEL_6581 = 0;
const MAX_SAMPLES_PER_CALL = 4096;

const wasmUrl = `file://${repoRoot}/src/runtime/headless/sid/wasm/resid.mjs`;
const { default: createResidModule } = await import(wasmUrl);
const mod = await createResidModule();

const b = {
  setChipModel: mod.cwrap("resid_set_chip_model", null, ["number"]),
  setVoiceMask: mod.cwrap("resid_set_voice_mask", null, ["number"]),
  enableFilter: mod.cwrap("resid_enable_filter", null, ["number"]),
  adjustFilterBias: mod.cwrap("resid_adjust_filter_bias", null, ["number"]),
  enableExternalFilter: mod.cwrap("resid_enable_external_filter", null, ["number"]),
  setSampling: mod.cwrap("resid_set_sampling", "number", ["number", "number", "number", "number", "number"]),
  reset: mod.cwrap("resid_reset", null, []),
  write: mod.cwrap("resid_write", null, ["number", "number"]),
  clock: mod.cwrap("resid_clock", "number", ["number", "number", "number"]),
  clockRemaining: mod.cwrap("resid_clock_remaining", "number", []),
};
const bufPtr = mod._malloc(MAX_SAMPLES_PER_CALL * 2);

// ---- configure() — VICE order, ResidWasm defaults (filter OFF, 6581) --------
b.reset();
b.setChipModel(MODEL_6581);
b.setVoiceMask(0x07);
b.enableFilter(0);              // BUG-049 default OFF (ResidWasm default)
b.adjustFilterBias(0.5);        // 500 mV
b.enableExternalFilter(1);
const passband = (SAMPLE_RATE * 90) / 200;
b.setSampling(CLOCK, SAMPLE_RATE, RESAMPLE, passband, 0.97);

// ---- emit() — verbatim ResidWasm.emit loop -----------------------------------
function emit(cycles) {
  const chunks = [];
  let total = 0;
  let dt = cycles;
  let guard = 0;
  const base = bufPtr >> 1;
  while (dt > 0 && guard++ < (1 << 20)) {
    const n = b.clock(dt, bufPtr, MAX_SAMPLES_PER_CALL);
    if (n > 0) { chunks.push(mod.HEAP16.slice(base, base + n)); total += n; }
    const rem = b.clockRemaining();
    if (n === 0 && rem >= dt) break;
    dt = rem;
  }
  const out = new Int16Array(total);
  let off = 0;
  for (const c of chunks) { out.set(c, off); off += c.length; }
  return out;
}

// ---- the fixed write/boundary script (mirrors smoke test 1, multi-frame) -----
// Voice 1: sawtooth A4 (freq16 7493), ADSR, gate, full volume; then noise; then
// gate off (release). Boundaries at one PAL frame each (19656 cycles) so we
// exercise the cross-call cycleAcc/sample-offset path repeatedly.
const FRAME = 19656;
const writes = [
  [0xD400, 7493 & 0xff],
  [0xD401, (7493 >> 8) & 0xff],
  [0xD405, 0x09],   // attack=0 decay=9
  [0xD406, 0xf0],   // sustain=15 release=0
  [0xD418, 0x0f],   // master volume = 15
  [0xD404, 0x21],   // GATE | SAW
];
const all = [];
for (const [addr, val] of writes) b.write(addr & 0x1f, val & 0xff);
// 40 frames of saw
for (let f = 0; f < 40; f++) { const s = emit(FRAME); all.push(s); }
// switch to noise
b.write(0xD404 & 0x1f, 0x81);
for (let f = 0; f < 20; f++) { const s = emit(FRAME); all.push(s); }
// gate off → release
b.write(0xD404 & 0x1f, 0x80);
for (let f = 0; f < 20; f++) { const s = emit(FRAME); all.push(s); }

let n = 0;
for (const c of all) n += c.length;
const merged = new Int16Array(n);
let off = 0;
for (const c of all) { merged.set(c, off); off += c.length; }

// ---- write s16le blob + manifest --------------------------------------------
const bytes = Buffer.from(merged.buffer, merged.byteOffset, merged.byteLength);
writeFileSync(resolve(outDir, "c64re_wasm.saw_noise_release.pcm.s16le"), bytes);

// fnv1a for a compact identity in the manifest/report
let h = 0x811c9dc5 >>> 0;
for (let i = 0; i < bytes.length; i++) { h ^= bytes[i]; h = Math.imul(h, 0x01000193) >>> 0; }

const manifest = {
  clock: CLOCK, sampleRate: SAMPLE_RATE, method: "RESAMPLE", model: "6581",
  filter: false, filterBias: 0.5, externalFilter: true, voiceMask: 0x07,
  passband, gain: 0.97, frameCycles: FRAME,
  frames: { saw: 40, noise: 20, release: 20 },
  sampleCount: merged.length, byteCount: bytes.length, fnv1a: (h >>> 0).toString(16),
};
writeFileSync(resolve(outDir, "c64re_wasm.saw_noise_release.json"), JSON.stringify(manifest, null, 2) + "\n");
console.log(`reference: ${merged.length} samples, ${bytes.length} bytes, fnv1a=${manifest.fnv1a}`);
