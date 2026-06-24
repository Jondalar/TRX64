// c64re production-path throughput bench — imports the COMPILED dist/ and runs
// under plain `node` (NOT tsx). tsx is ~22x slower (esbuild on-the-fly transpile
// defeats V8 tier-up) and is NOT how c64re ships (ui.sh -> npm run workspace ->
// node on dist/). Run after `npm run build:mcp` from the c64re repo.
//
//   node bench/c64re_dist_bench.mjs [pure|disk]
//
// Pairs with crates/trx64-core/tests/perf_bench.rs (the Rust side). See
// docs/perf-compare.md for the methodology + the tsx-vs-dist caveat.

import { startIntegratedSession } from "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/dist/runtime/headless/integrated-session-manager.js";

const PAL_HZ = 985_248.444;
const which = process.argv[2] ?? "both";
const SAMPLE = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/scramble_infinity.d64";

function settle(s, cyc, chunk) {
  const t = s.c64Cpu.cycles + cyc;
  while (s.c64Cpu.cycles < t) s.runFor(2_000_000, { cycleBudget: chunk });
}
function timed(s, budget, chunk) {
  const c0 = s.c64Cpu.cycles, w0 = performance.now();
  settle(s, budget, chunk);
  const dt = (performance.now() - w0) / 1000, cyc = s.c64Cpu.cycles - c0;
  return { mhz: cyc / dt / 1e6, rt: cyc / dt / PAL_HZ, dt, cyc };
}

if (which === "pure" || which === "both") {
  const { session: s } = startIntegratedSession({ mode: "true-drive", isPal: true });
  s.resetCold("pal-default");
  settle(s, 3_000_000, 1_000_000);          // boot settle
  settle(s, 5_000_000, 1_000_000);          // V8 warmup
  const r = timed(s, 5_000_000, 1_000_000);
  console.log(`c64re dist pure: ${r.cyc} cyc / ${r.dt.toFixed(3)}s = ${r.mhz.toFixed(3)} MHz = ${r.rt.toFixed(2)}x realtime`);
}
if (which === "disk" || which === "both") {
  const { session: s } = startIntegratedSession({ diskPath: SAMPLE, mode: "true-drive", isPal: true });
  s.resetCold("pal-default");
  settle(s, 3_000_000, 500_000);
  s.typeText('LOAD"*",8,1\r');
  settle(s, 3_000_000, 500_000);
  s.typeText("RUN\r");
  settle(s, 3_000_000, 500_000);            // warmup the loading phase
  const r = timed(s, 5_000_000, 500_000);
  console.log(`c64re dist disk: ${r.cyc} cyc / ${r.dt.toFixed(3)}s = ${r.mhz.toFixed(3)} MHz = ${r.rt.toFixed(2)}x realtime`);
}
