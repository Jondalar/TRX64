/**
 * c64re_bench.ts — c64re TypeScript-core throughput benchmark (TS half of the
 * TRX64-vs-c64re cross-core comparison).
 *
 * Mirrors crates/trx64-core/tests/perf_bench.rs EXACTLY: same two workloads,
 * same fixed C64-cycle budgets, same K timed runs, same boot/mount/key-inject
 * sequence, same PAL ROMs (auto-loaded by the c64re core from
 * resources/roms/). It measures CORE EMULATION THROUGHPUT only — the time to
 * execute a fixed C64-cycle budget — with construction / boot / mount /
 * key-injection EXCLUDED from the timed region. No WebSocket, no daemon: the
 * IntegratedSession is driven in-process so there is zero IPC overhead.
 *
 * This imports the live c64re source directly via tsx (NodeNext resolution: the
 * ".js" import specifiers map onto the ".ts" sources). Run it FROM THE c64re
 * REPO ROOT so its relative resources/samples resolve and tsx picks up its
 * tsconfig:
 *
 *   cd /Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP
 *   npx tsx /Users/alex/Development/C64/Tools/TRX64/bench/c64re_bench.ts
 *
 * Or a single workload:
 *   npx tsx .../bench/c64re_bench.ts pure
 *   npx tsx .../bench/c64re_bench.ts disk
 *
 * V8 JIT WARMUP: the c64re core runs under V8. To give the TS core its FAIREST
 * shot (its runtime is "already optimized" via JIT), we run one full untimed
 * WARMUP iteration per workload before the K timed runs, so the hot paths are
 * JIT-compiled by the time we start the clock. This is documented as a
 * methodology choice in docs/perf-compare.md.
 */

import { performance } from "node:perf_hooks";
import { resolve as resolvePath } from "node:path";
import { existsSync } from "node:fs";
// Import the live c64re core directly (relative path to the sibling repo's
// src/). NodeNext: the ".js" specifier maps onto the ".ts" source under tsx.
import { startIntegratedSession } from "../../C64ReverseEngineeringMCP/src/runtime/headless/integrated-session-manager.js";

const C64RE_ROOT = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP";
const SAMPLES = resolvePath(C64RE_ROOT, "samples");

/** C64 PAL master clock (Hz). Matches the Rust bench's PAL_HZ. */
const PAL_HZ = 985_248.444;

/** Same DEFAULT budgets as perf_bench.rs. The c64re TS core runs ~200x slower
 *  than the Rust core, so for a tractable cross-core ratio both benches are run
 *  with a SMALLER identical budget via these env overrides (the MHz rate is
 *  budget-independent in steady state; the smaller budget just keeps the TS run
 *  to minutes instead of hours). See docs/perf-compare.md. */
const PURE_BUDGET = Number(process.env.C64RE_PURE_BUDGET ?? 100_000_000);
const DISK_BUDGET = Number(process.env.C64RE_DISK_BUDGET ?? 35_000_000);
const K_RUNS = Number(process.env.C64RE_K_RUNS ?? 7);

type Session = ReturnType<typeof startIntegratedSession>["session"];

/** Drive a fixed C64-cycle budget on an already-booted session and return the
 *  wall-clock seconds of the cycle-execution ONLY. Mirrors the Rust bench's
 *  chunked run_for_full_capped loop: a per-chunk instruction cap that can never
 *  trip before the cycle budget, so cycles (not instructions) bound the loop. */
function timeCycleRun(session: Session, budget: number, chunkArg: number): { secs: number; executed: number } {
  const chunk = Math.min(chunkArg, budget);
  // Instruction cap per chunk: 2x chunk is unreachable (min 6502 instr = 2 cyc).
  const instCap = chunk * 2;
  const startCycles = session.c64Cpu.cycles;
  const t0 = performance.now();
  let done = 0;
  while (done < budget) {
    session.runFor(instCap, { cycleBudget: chunk });
    done += chunk;
  }
  const secs = (performance.now() - t0) / 1000;
  const executed = session.c64Cpu.cycles - startCycles;
  return { secs, executed };
}

function median(v: number[]): number {
  const s = [...v].sort((a, b) => a - b);
  return s[Math.floor(s.length / 2)];
}

function report(label: string, budget: number, secs: number[]): void {
  const min = Math.min(...secs);
  const max = Math.max(...secs);
  const med = median(secs);
  const mhz = (s: number) => budget / s / 1_000_000;
  const rt = (s: number) => budget / s / PAL_HZ;
  console.log(`\n========== ${label} ==========`);
  console.log(`  budget = ${budget} C64 cycles, K = ${secs.length} timed runs`);
  console.log(`  wall-clock  min/median/max : ${min.toFixed(4)} / ${med.toFixed(4)} / ${max.toFixed(4)} s`);
  console.log(`  emulated MHz (median)          : ${mhz(med).toFixed(3)} MHz  (${mhz(max).toFixed(4)} / ${mhz(med).toFixed(4)} / ${mhz(min).toFixed(4)} min/med/max)`);
  console.log(`  real-time multiple (median)    : ${rt(med).toFixed(1)}x  (median ${mhz(med).toFixed(3)} MHz / ${(PAL_HZ / 1e6).toFixed(6)} MHz PAL)`);
  console.log(`  RAW (machine-parseable): ${label} budget=${budget} k=${secs.length} min_s=${min.toFixed(6)} med_s=${med.toFixed(6)} max_s=${max.toFixed(6)} med_mhz=${mhz(med).toFixed(4)} med_rtx=${rt(med).toFixed(3)}`);
}

// ── Workload 1: pure headless steady-state (CPU+VIC+CIA+SID, no drive) ───────
function benchPureHeadless(): void {
  const build = (): Session => {
    const { session } = startIntegratedSession({ mode: "true-drive", isPal: true });
    session.resetCold("pal-default");
    // Boot well past READY so the timed region is pure steady state (matches
    // the Rust bench's 3M-cycle settle). runFor here is UNTIMED setup.
    const target = session.c64Cpu.cycles + 3_000_000;
    while (session.c64Cpu.cycles < target) session.runFor(2_000_000, { cycleBudget: 1_000_000 });
    return session;
  };

  // V8 warmup: one full untimed iteration so the hot paths are JIT-compiled.
  {
    const s = build();
    timeCycleRun(s, PURE_BUDGET, 1_000_000);
    console.log(`  [pure warmup] done (JIT primed), final_pc=$${s.c64Cpu.pc.toString(16).toUpperCase()}`);
  }

  const secs: number[] = [];
  for (let run = 0; run < K_RUNS; run++) {
    const s = build();
    const { secs: e, executed } = timeCycleRun(s, PURE_BUDGET, 1_000_000);
    secs.push(e);
    console.log(`  [pure run ${run + 1}/${K_RUNS}] ${e.toFixed(4)}s  executed=${executed} cyc  final_pc=$${s.c64Cpu.pc.toString(16).toUpperCase()}`);
  }
  report("WORKLOAD 1 — pure headless (CPU+VIC+CIA+SID, no drive)", PURE_BUDGET, secs);
}

// ── Workload 2: full-system disk (boot+mount+LOAD"*",8,1+RUN) ────────────────
function benchDiskWorkload(): void {
  const diskPath = resolvePath(SAMPLES, "scramble_infinity.d64");
  if (!existsSync(diskPath)) {
    console.log(`skip disk workload: sample absent (${diskPath})`);
    return;
  }

  const build = (): Session => {
    // diskPath at construction = the disk is mounted at build time (outside the
    // timed region), matching the Rust bench's attach_disk-then-settle setup.
    const { session } = startIntegratedSession({ diskPath, mode: "true-drive", isPal: true });
    session.resetCold("pal-default");
    let target = session.c64Cpu.cycles + 2_500_000;
    while (session.c64Cpu.cycles < target) session.runFor(2_000_000, { cycleBudget: 1_000_000 });
    // settle (mirror Rust's 800k post-mount settle)
    target = session.c64Cpu.cycles + 800_000;
    while (session.c64Cpu.cycles < target) session.runFor(2_000_000, { cycleBudget: 500_000 });
    session.typeText('LOAD"*",8,1\r');
    // let the LOAD command be parsed + begin (mirror Rust's 500k pre-RUN run)
    target = session.c64Cpu.cycles + 500_000;
    while (session.c64Cpu.cycles < target) session.runFor(2_000_000, { cycleBudget: 500_000 });
    session.typeText("RUN\r");
    return session;
  };

  {
    const s = build();
    timeCycleRun(s, DISK_BUDGET, 500_000);
    console.log(`  [disk warmup] done (JIT primed), final_pc=$${s.c64Cpu.pc.toString(16).toUpperCase()}`);
  }

  const secs: number[] = [];
  for (let run = 0; run < K_RUNS; run++) {
    const s = build();
    const { secs: e, executed } = timeCycleRun(s, DISK_BUDGET, 500_000);
    secs.push(e);
    console.log(`  [disk run ${run + 1}/${K_RUNS}] ${e.toFixed(4)}s  executed=${executed} cyc  final_pc=$${s.c64Cpu.pc.toString(16).toUpperCase()}`);
  }
  report('WORKLOAD 2 — full-system disk (boot+mount+LOAD"*",8,1+RUN, scramble_infinity.d64)', DISK_BUDGET, secs);
}

const which = process.argv[2];
if (which === "pure") {
  benchPureHeadless();
} else if (which === "disk") {
  benchDiskWorkload();
} else {
  benchPureHeadless();
  benchDiskWorkload();
}
