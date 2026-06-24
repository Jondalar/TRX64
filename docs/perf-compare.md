# TRX64 (Rust) vs c64re (TypeScript) — core throughput comparison

**Question.** How many more C64 cycles/second does the TRX64 Rust emulation core
execute than the c64re TypeScript core, on identical workloads, both run the way
they ship, CPU-clean, median of K runs?

> **Headline:** the TRX64 Rust core runs **~8–10× faster** than the c64re
> TypeScript core on its **production path** (`node` on the compiled `dist/`):
> ~9.8× on the pure-headless workload, ~8.1× on the full-system disk workload.
> Both cores execute the SAME verbatim `true-drive` instruction stream (TRX64 is
> a trace-diff-verified 1:1 reimplementation of the c64re headless runtime) — only
> the language/runtime differs.

This is a **measurement-only** comparison. No emulation-core source was changed on
either side; the harness is bench/test/doc only.

---

## Results

| Workload | c64re MHz (TS, prod) | TRX64 MHz (Rust) | **Ratio (Rust = N× TS)** | c64re real-time | TRX64 real-time |
|---|---:|---:|---:|---:|---:|
| 1 — pure headless (CPU+VIC+CIA+SID, no disk) | **1.372** | **13.435** | **9.8×** | 1.39× | 13.64× |
| 2 — full-system disk (boot+mount+`LOAD"*",8,1`+RUN, scramble) | **1.388** | **11.179** | **8.1×** | 1.41× | 11.35× |

MHz = emulated-C64 MHz = cycles ÷ wall-seconds ÷ 1e6. Real-time multiple =
emulated MHz ÷ 0.985248 MHz (PAL master clock).

**Per-side detail (median of timed runs):**

| side | workload | budget (cyc) | wall (s) | MHz | notes |
|---|---|---:|---:|---:|---|
| TRX64 (Rust `--release`) | pure | 30,000,000 | 2.211 / 2.233 / 2.267 (min/med/max) | 13.435 | K=7, settles at PC $E5CD |
| TRX64 (Rust `--release`) | disk | 20,000,000 | 1.781 / 1.789 / 1.814 | 11.179 | K=7, drive sync_found=128, head=47976, in $EEB2 serial-RX |
| c64re (`node` on `dist/`) | pure | 5,000,000 | 3.645 | 1.372 | 5M warmup before timed, true-drive |
| c64re (`node` on `dist/`) | disk | 5,000,000 | 3.602 | 1.388 | post-RUN scramble load, true-drive |

The two cores boot the same PAL ROMs to the same READY state (TRX64 PC ≈ $E5CD,
c64re ≈ $E5CD/$E5D1 — the same KERNAL editor idle loop) and run the identical
`true-drive` path (real KERNAL serial bit-bang against the cycle-stepped 1541 —
the product path TRX64 mirrors 1:1).

---

## CRITICAL: measure c64re the way it ships — `node` on `dist/`, NOT `tsx`

The single biggest methodology trap here. c64re ships + runs (the `ui.sh` → `npm
run workspace` path) as **compiled JavaScript executed by plain `node`**
(`build:mcp` = `tsc` → `dist/`, then `node scripts/workspace.mjs`). It does NOT
run via `tsx` in production.

A first measurement pass drove the c64re core through `npx tsx` on the `.ts`
source and got **0.061 MHz** — which is **~22× slower** than the shipped path and
produced a bogus "~200×" ratio. `tsx` (esbuild on-the-fly transpile + a Node
loader hook) defeats V8's tier-up on the hot per-cycle loop. The live UI runs at
~1.4× real-time (smooth 50 fps interactive play); 0.061 MHz (0.06× real-time)
would be 3 fps and could not play live — the contradiction is what exposed the
`tsx` artifact.

**Always benchmark c64re via `node` against `dist/` (after `npm run build:mcp`),
never via `tsx`.** The numbers above use the `dist/` path on both the pure and
disk workloads.

---

## Methodology

**Machine.** Apple M4 (10 cores), 24 GB, macOS 27.0. Runs done back-to-back, not
interleaved with any build, on an otherwise-idle machine (leftover hermetic
daemons killed first). Each emulation core is single-threaded; the other M4 cores
were idle.

**Toolchains.**
- Rust: `--release` (opt-level 3 — debug is ~10× slower and meaningless).
- TS: Node `v22.21.1`, **`node` on the compiled `dist/`** (`tsc` output, the
  production path), NOT `tsx`. One untimed multi-million-cycle warmup precedes the
  timed region so V8 has tiered up the hot loop.
- ROMs: the SAME PAL set on both sides (`kernal-901227-03` / `basic-901226-01` /
  `chargen-901225-01`, 1541 DOS `dos1541-325302-01+901229-05`).

**What is timed.** ONLY the cycle-execution loop. Excluded from every timed
region: process spawn, V8/JIT warmup, machine construction, ROM load, disk mount,
`LOAD"*",8,1`/`RUN` key injection, and rendering. On both cores the timed region
is a chunked run loop bounded by the C64-cycle budget.

**Same workload, same mode.** Both cores run in `true-drive` (real KERNAL serial
against a cycle-stepped 1541 — the product path). The disk workload mounts the
same `scramble_infinity.d64`, issues the same `LOAD"*",8,1` + `RUN`.

**Reproduce.**
```bash
# Rust (TRX64 repo root):
rtk cargo build --release
TRX64_PURE_BUDGET=30000000 cargo test -p trx64-core --release --test perf_bench \
  bench_pure_headless -- --ignored --nocapture --test-threads=1
TRX64_DISK_BUDGET=20000000 cargo test -p trx64-core --release --test perf_bench \
  bench_disk_workload -- --ignored --nocapture --test-threads=1

# c64re — PRODUCTION path (compiled dist + node), from the c64re repo:
cd /Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP
npm run build:mcp                  # tsc -> dist/   (only if dist is stale)
node /path/to/bench/c64re_dist_bench.mjs   # imports from dist/, NOT tsx
```

Harness:
- Rust: `crates/trx64-core/tests/perf_bench.rs` (`bench_pure_headless`,
  `bench_disk_workload`, `bench_per_game_gate`).
- TS: `bench/c64re_dist_bench.mjs` (imports the compiled
  `dist/runtime/headless/integrated-session-manager.js`, run with `node`).

**Validation.** `cargo test --workspace` stays green — additions are
bench/test/doc only, touching no emulation-core source.

---

## Honest caveats

- **The c64re number must use `dist/` + `node`, not `tsx`.** `tsx` is ~22× slower
  and is NOT how c64re ships (see the CRITICAL section). Quoting the `tsx` number
  (the bogus ~200×) would be dishonest; the real ratio is ~8–10×.
- **MHz is a rate; ratio uses each side's median.** The Rust budgets (30M/20M) are
  larger than the TS budgets (5M) only to keep total wall-time sane; MHz is
  budget-independent in steady state (the Rust full-budget and reduced-budget
  rates agree to <2%).
- **V8 JIT warmup (favors TS).** A full untimed warmup precedes the TS timed runs
  so the JIT is hot — the most generous setup for the TS core.
- **No WebSocket overhead (favors TS).** Both cores are driven in-process; the
  daemon's WS round-trip is excluded so we measure pure core throughput.
- **Release vs debug (Rust).** All Rust numbers are `--release`.
- **Single-core, same machine.** Numbers are not portable to other hardware; only
  the *ratio* is the durable result.
- **Why ~8–10× and not more.** Both cores run the same verbatim per-cycle x64sc +
  per-cycle VIC + cycle-stepped drive. The Rust win is the language/runtime
  (native, no GC, monomorphic dispatch, no boxing) on the identical algorithm —
  an order of magnitude, which is the expected headroom for a tight
  interpreter-style loop ported JS→Rust. The disk workload is slightly tighter
  (~8×) because the cross-domain drive catch-up + GCR + viacore add real per-cycle
  work on both sides.
