// Render-gate scenario runner — pixel-parity for sprites + graphics modes.
//
//   node compare.mjs                 -> run ALL scenarios, report GREEN/RED each.
//   node compare.mjs <name> [name..] -> run the named scenarios only.
//
// Each scenario boots both daemons (TS oracle + TRX64), injects its CPU program,
// screenshots, decodes both PNGs to RGBA and PIXEL-diffs. Exit 0 iff every
// requested scenario is GREEN; 1 if any RED; 2 on harness error.

import { captureScene, spawnDaemon } from "./scene.mjs";
import { decodePng, diffRgba } from "./png.mjs";
import { SCENARIOS } from "./scenarios.mjs";

// Run a scene on both daemons (kind-aware capture) and pixel-diff.
async function compareScene(scene) {
  const ts = await spawnDaemon("ts");
  let goldenPng;
  try { goldenPng = (await captureScene(ts.endpoint, scene, "ts")).png; } finally { ts.stop(); }
  const trx = await spawnDaemon("trx64");
  let candPng;
  try { candPng = (await captureScene(trx.endpoint, scene, "trx64")).png; } finally { trx.stop(); }
  const golden = decodePng(goldenPng);
  const cand = decodePng(candPng);
  const div = diffRgba(golden.width, golden.height, golden.rgba, cand.width, cand.height, cand.rgba);
  return { ok: !div, div, golden, cand };
}

async function main() {
  const args = process.argv.slice(2);
  const names = args.length ? args : Object.keys(SCENARIOS);
  let anyRed = false;
  for (const name of names) {
    const make = SCENARIOS[name];
    if (!make) { console.error(`unknown scenario '${name}'`); return 2; }
    const scene = make();
    const { ok, div, golden } = await compareScene(scene);
    if (ok) {
      console.log(`[render-${name}] GREEN — pixel-identical ${golden.width}x${golden.height}`);
    } else {
      anyRed = true;
      if (div.dim) {
        console.log(`[render-${name}] RED — dimension mismatch: expected ${div.expected} got ${div.got}`);
      } else {
        console.log(`[render-${name}] RED — first divergence at (${div.x},${div.y}): expected RGBA ${JSON.stringify(div.expected)} got ${JSON.stringify(div.got)} (${div.totalDiff} px differ)`);
      }
    }
  }
  return anyRed ? 1 : 0;
}

main().then((c) => process.exit(c), (e) => { console.error("harness error:", e); process.exit(2); });
