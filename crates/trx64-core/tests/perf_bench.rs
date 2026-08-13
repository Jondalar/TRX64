//! perf_bench.rs — TRX64 Rust-core throughput benchmark (vs the c64re TS core).
//!
//! This measures CORE EMULATION THROUGHPUT only: the time to execute a FIXED
//! C64-cycle budget of steady-state emulation, with ROM-load / disk-mount /
//! key-injection / rendering all EXCLUDED from the timed region. It is the Rust
//! half of the cross-core comparison; the TS half lives in
//! `bench/c64re_bench.ts` and runs the IDENTICAL workload on the c64re
//! IntegratedSession. See `docs/perf-compare.md` for the methodology + results.
//!
//! These tests are `#[ignore]` so a normal `cargo test` never runs them. They
//! ONLY produce a defensible number when built `--release` (debug is ~10x slower
//! and meaningless).
//!
//! Run (release, median of K runs printed):
//!   rtk cargo test -p trx64-core --release --test perf_bench -- --ignored --nocapture
//!
//! Single workload:
//!   cargo test -p trx64-core --release --test perf_bench bench_pure_headless -- --ignored --nocapture
//!   cargo test -p trx64-core --release --test perf_bench bench_disk_workload -- --ignored --nocapture

use std::path::Path;
use std::time::Instant;
use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const SAMPLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/samples");

/// C64 PAL master clock (MHz). 985248.444 Hz → 0.985248 MHz.
const PAL_HZ: f64 = 985_248.444;

/// Pure-headless steady-state cycle budget: 100M C64 cycles (~101.5s of PAL
/// wall-clock emulated per run). Large enough to dwarf any fixed overhead and
/// to amortize cache effects. Override with TRX64_PURE_BUDGET to run the SAME
/// reduced budget the TS bench uses (the TS core is ~200x slower, so the
/// cross-core ratio is computed on a smaller identical budget — see
/// docs/perf-compare.md).
const PURE_BUDGET_DEFAULT: u64 = 100_000_000;
const DISK_BUDGET_DEFAULT: u64 = 35_000_000;

/// Number of timed runs per workload; we report min / median / max.
const K_RUNS: usize = 7;

fn env_budget(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(default)
}

fn roms_present() -> bool {
    let d = Path::new(ROM_DIR);
    d.join("kernal-901227-03.bin").exists()
        && d.join("basic-901226-01.bin").exists()
        && d.join("chargen-901225-01.bin").exists()
}

fn inject_keys(m: &mut Machine, s: &[u8]) {
    for (i, b) in s.iter().enumerate() {
        m.poke(0x0277 + i as u16, &[*b]);
    }
    m.poke(0x00c6, &[s.len() as u8]);
}

/// Median of an odd-or-even slice (lower-middle for even N).
fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

struct RunStats {
    budget_cycles: u64,
    secs: Vec<f64>,
}

impl RunStats {
    fn report(&self, label: &str) {
        let mut s = self.secs.clone();
        let min = s.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = s.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let med = median(&mut s);
        let mhz = |secs: f64| (self.budget_cycles as f64) / secs / 1_000_000.0;
        let rt = |secs: f64| (self.budget_cycles as f64) / secs / PAL_HZ;
        eprintln!("\n========== {label} ==========");
        eprintln!(
            "  budget = {} C64 cycles, K = {} timed runs",
            self.budget_cycles,
            self.secs.len()
        );
        eprintln!("  wall-clock  min/median/max : {min:.4} / {med:.4} / {max:.4} s");
        eprintln!(
            "  emulated MHz (median)          : {:.3} MHz  ({:.4} / {:.4} / {:.4} min/med/max)",
            mhz(med),
            mhz(max),
            mhz(med),
            mhz(min)
        );
        eprintln!(
            "  real-time multiple (median)    : {:.1}x  (median {:.3} MHz / {:.6} MHz PAL)",
            rt(med),
            mhz(med),
            PAL_HZ / 1_000_000.0
        );
        eprintln!(
            "  RAW (machine-parseable): {label} budget={} k={} min_s={:.6} med_s={:.6} max_s={:.6} med_mhz={:.4} med_rtx={:.3}",
            self.budget_cycles,
            self.secs.len(),
            min,
            med,
            max,
            mhz(med),
            rt(med)
        );
    }
}

// ── Workload 1: pure headless steady-state main-machine throughput ──────────
//
// Boot to BASIC (UNtimed), then time a FIXED 100M-cycle steady-state run of the
// main machine: CPU + VIC + CIA1/CIA2 + SID, no drive activity. This isolates
// the C64-core throughput (the realistic "sitting at the READY prompt with the
// cursor blinking + the IRQ running" loop, which is the cleanest apples-to-apples
// steady-state path that BOTH cores execute identically).
#[test]
#[ignore = "perf benchmark; run --release with --ignored --nocapture"]
fn bench_pure_headless() {
    if !roms_present() {
        eprintln!("skip bench_pure_headless: ROMs absent at {ROM_DIR}");
        return;
    }
    let pure_budget = env_budget("TRX64_PURE_BUDGET", PURE_BUDGET_DEFAULT);
    let mut secs = Vec::with_capacity(K_RUNS);

    for run in 0..K_RUNS {
        // ── SETUP (UNTIMED): construct + boot to BASIC READY ──────────────
        let mut m = Machine::new();
        m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
        let mut sink = NullSink;
        // Boot well past READY so the timed region is pure steady state.
        m.run_for_full(3_000_000, &mut sink, |_, _, _, _, _, _, _| {});

        // ── TIMED REGION: a FIXED 100M-cycle steady-state run ─────────────
        // Drive the run in fixed chunks with a generous per-chunk instruction
        // cap that can NEVER trip before the cycle budget (so cycles, not
        // instructions, bound the loop — matching the cross-core contract).
        let start_clk = m.c64_core.clk;
        let chunk = 1_000_000u64;
        // worst case ~1 cycle/instr (impossible on 6502, min is 2) → cap = chunk
        // is already unreachable; use chunk*2 for absolute safety.
        let inst_cap = chunk * 2;
        let t0 = Instant::now();
        let mut done = 0u64;
        while done < pure_budget {
            m.run_for_full_capped(chunk, inst_cap, &mut sink, |_, _, _, _, _, _, _| {});
            done += chunk;
        }
        let elapsed = t0.elapsed().as_secs_f64();
        let executed = m.c64_core.clk.wrapping_sub(start_clk);
        // Sanity: we executed at least the budget (chunks are exact multiples).
        assert!(
            executed >= pure_budget,
            "executed {executed} < budget {pure_budget}"
        );
        secs.push(elapsed);
        eprintln!(
            "  [pure run {}/{}] {:.4}s  executed={} cyc  final_pc=${:04X}",
            run + 1,
            K_RUNS,
            elapsed,
            executed,
            m.c64_core.reg_pc
        );
    }

    RunStats {
        budget_cycles: pure_budget,
        secs,
    }
    .report("WORKLOAD 1 — pure headless (CPU+VIC+CIA+SID, no drive)");
}

// ── Workload 1b: cpuhistory-ring overhead (ring ON vs OFF) ──────────────────
//
// reverse-debug Phase 1a perf gate. The always-on CPU-history ring pushes one
// record per retired instruction on the ~1 MHz hot path; this must be negligible.
// Runs the SAME pure-headless steady-state workload TWICE on identical machines —
// once with the ring ENABLED (the shipped default) and once with it DISABLED (the
// `TRX64_CPUHISTORY=0` kill-switch, via `set_enabled(false)`) — and reports the
// cycles/sec delta. K medians each; the delta is the cost of `CpuHistoryRing::push`.
#[test]
#[ignore = "perf benchmark; run --release with --ignored --nocapture"]
fn bench_cpuhistory_ring_overhead() {
    if !roms_present() {
        eprintln!("skip bench_cpuhistory_ring_overhead: ROMs absent at {ROM_DIR}");
        return;
    }
    // Default 2M cycles/run (the task's "free-run ~2M cycles" measure); override with
    // TRX64_PURE_BUDGET to match the larger pure bench.
    let budget = env_budget("TRX64_PURE_BUDGET", 2_000_000);

    let run_once = |ring_on: bool| -> f64 {
        let mut m = Machine::new();
        m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
        // Toggle BOTH always-on rings AFTER construction (the env is read at new());
        // explicit so the bench is deterministic regardless of the ambient
        // TRX64_CPUHISTORY. reverse-debug Phase 1b: this now measures the FULL hot-path
        // cost = the CPU-history ring (Phase 1a) + the full-delta undo ring (Phase 1b,
        // per-instruction begin/commit + per-write record_write). The kill-switch gates
        // both together in production, so the bench gates both together too.
        m.cpu_history.set_enabled(ring_on);
        m.delta_ring.set_enabled(ring_on);
        let mut sink = NullSink;
        m.run_for_full(3_000_000, &mut sink, |_, _, _, _, _, _, _| {});
        let chunk = 500_000u64;
        let t0 = Instant::now();
        let mut done = 0u64;
        while done < budget {
            m.run_for_full_capped(chunk, chunk * 2, &mut sink, |_, _, _, _, _, _, _| {});
            done += chunk;
        }
        let secs = t0.elapsed().as_secs_f64();
        // Sanity: the ON run actually recorded into BOTH rings; OFF recorded nothing.
        if ring_on {
            assert!(m.cpu_history.len() > 0, "cpu-history ring ON but recorded nothing");
            assert!(m.delta_ring.len() > 0, "delta ring ON but recorded nothing");
        } else {
            assert_eq!(m.cpu_history.len(), 0, "cpu-history ring OFF but recorded something");
            assert_eq!(m.delta_ring.len(), 0, "delta ring OFF but recorded something");
        }
        secs
    };

    let mut on = Vec::with_capacity(K_RUNS);
    let mut off = Vec::with_capacity(K_RUNS);
    for _ in 0..K_RUNS {
        // Interleave ON/OFF so thermal/scheduler drift hits both equally.
        on.push(run_once(true));
        off.push(run_once(false));
    }
    let med_on = median(&mut on.clone());
    let med_off = median(&mut off.clone());
    let mhz = |s: f64| (budget as f64) / s / 1_000_000.0;
    let delta_pct = (med_on - med_off) / med_off * 100.0;
    eprintln!("\n========== WORKLOAD 1b — cpuhistory-ring overhead (ON vs OFF) ==========");
    eprintln!("  budget = {budget} C64 cycles/run, K = {K_RUNS} timed runs each");
    eprintln!("  ring OFF (kill-switch)  median : {med_off:.4} s  ({:.3} MHz)", mhz(med_off));
    eprintln!("  ring ON  (shipped)      median : {med_on:.4} s  ({:.3} MHz)", mhz(med_on));
    eprintln!("  DELTA (ON vs OFF)              : {delta_pct:+.2}%  (negative = ON faster = within noise)");
    eprintln!(
        "  RAW: cpuhistory_overhead budget={budget} k={K_RUNS} off_med_s={med_off:.6} on_med_s={med_on:.6} delta_pct={delta_pct:.3}"
    );
}

// ── Workload 2: full-system disk workload (scramble_infinity.d64) ───────────
//
// The realistic "running a game" path: boot + mount the D64 + LOAD"*",8,1 + RUN,
// then run to ~the title. This exercises the cross-domain drive + IEC + GCR +
// the 1:1 viacore — the expensive parts. We time ONLY the post-mount cycle run
// (mount + key-inject are part of setup but cheap; the disk LOAD itself is the
// realistic workload and IS timed). Render is excluded from the timed region.
#[test]
#[ignore = "perf benchmark; run --release with --ignored --nocapture"]
fn bench_disk_workload() {
    if !roms_present() {
        eprintln!("skip bench_disk_workload: ROMs absent at {ROM_DIR}");
        return;
    }
    let disk_path = format!("{SAMPLES}/scramble_infinity.d64");
    let disk_bytes = match std::fs::read(&disk_path) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("skip bench_disk_workload: sample absent ({disk_path})");
            return;
        }
    };

    // Fixed cross-domain cycle budget: 35M C64 cycles after RUN. Per the
    // seven_game_gate notes, scramble's standard-KERNAL serial load needs ~30M
    // cycles to bring in the BASIC stub before the fastloader installs, so 35M
    // lands us in/around the title — a representative drive-heavy slice.
    // Override with TRX64_DISK_BUDGET for the reduced cross-core ratio budget.
    let disk_budget = env_budget("TRX64_DISK_BUDGET", DISK_BUDGET_DEFAULT);
    let mut secs = Vec::with_capacity(K_RUNS);

    for run in 0..K_RUNS {
        // ── SETUP (UNTIMED): construct + boot + mount + settle + inject ───
        let mut m = Machine::new();
        m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
        let mut sink = NullSink;
        m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});
        m.drive8.attach_disk(DiskImage {
            kind: DiskKind::D64,
            bytes: disk_bytes.clone(),
            backing_path: Some(disk_path.clone()),
            read_only: false,
        });
        m.run_for_full(800_000, &mut sink, |_, _, _, _, _, _, _| {});
        inject_keys(&mut m, b"LOAD\"*\",8,1\r");
        // Let the LOAD command be parsed + the load begin (still setup — we want
        // the timed region to be the steady cross-domain load/run, not the
        // editor parsing a line). Small, fixed, and identical on both cores.
        m.run_for_full(500_000, &mut sink, |_, _, _, _, _, _, _| {});
        inject_keys(&mut m, b"RUN\r");

        // ── TIMED REGION: the cross-domain LOAD + RUN cycle run ───────────
        let start_clk = m.c64_core.clk;
        let chunk = 500_000u64;
        let inst_cap = chunk * 2;
        let t0 = Instant::now();
        let mut done = 0u64;
        while done < disk_budget {
            m.run_for_full_capped(chunk, inst_cap, &mut sink, |_, _, _, _, _, _, _| {});
            done += chunk;
        }
        let elapsed = t0.elapsed().as_secs_f64();
        let executed = m.c64_core.clk.wrapping_sub(start_clk);
        assert!(executed >= disk_budget);
        secs.push(elapsed);
        eprintln!(
            "  [disk run {}/{}] {:.4}s  executed={} cyc  sync_found={} head={}  final_pc=${:04X}",
            run + 1,
            K_RUNS,
            elapsed,
            executed,
            m.drive8.rotation.sync_found(),
            m.drive8.rotation.gcr_head_offset,
            m.c64_core.reg_pc
        );
    }

    RunStats {
        budget_cycles: disk_budget,
        secs,
    }
    .report("WORKLOAD 2 — full-system disk (boot+mount+LOAD\"*\",8,1+RUN, scramble_infinity.d64)");
}

// ── Per-game gate throughput: each of the 7 gate games, end-to-end ──────────
//
// For each game: boot + mount (D64 or G64) + LOAD"*",8,1 + RUN (UNtimed setup),
// then TIME a fixed post-RUN cycle run. Reports per-game cycles → MHz so the
// report can quote "game X emulated N cycles in M ms on TRX64". One run each
// (these are throughput datapoints, not a precision median — the gate proper is
// seven_game_gate.rs). Drive-heavy G64 games stress the GCR + viacore the most.
#[test]
#[ignore = "perf benchmark; run --release with --ignored --nocapture"]
fn bench_per_game_gate() {
    if !roms_present() {
        eprintln!("skip bench_per_game_gate: ROMs absent at {ROM_DIR}");
        return;
    }
    // Fixed post-RUN timed budget per game (override with TRX64_GAME_BUDGET).
    let game_budget = env_budget("TRX64_GAME_BUDGET", 30_000_000);

    // (file, kind, name) — the gate roster (california excluded, see gate notes).
    let games: &[(&str, DiskKind, &str)] = &[
        ("scramble_infinity.d64", DiskKind::D64, "scramble"),
        ("POLARBEAR.d64", DiskKind::D64, "polarbear"),
        ("motm.g64", DiskKind::G64, "motm"),
        ("green_beret[ocean_1986](!).g64", DiskKind::G64, "greenberet"),
        ("impossible_mission_ii[epyx_1987](!).g64", DiskKind::G64, "impossible2"),
        ("last_ninja_remix_s1[system3_1991].g64", DiskKind::G64, "lastninja"),
        (
            "maniac_mansion_s1[activision_1987](german)(manual)(!).g64",
            DiskKind::G64,
            "maniac",
        ),
    ];

    eprintln!("\n========== PER-GAME GATE THROUGHPUT (TRX64 release) ==========");
    eprintln!("  post-RUN timed budget = {game_budget} C64 cycles/game");
    eprintln!(
        "  {:<12} {:>6} {:>12} {:>10} {:>8} {:>10}",
        "game", "kind", "cycles", "wall_s", "MHz", "real-time"
    );

    for (file, kind, name) in games {
        let path = format!("{SAMPLES}/{file}");
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => {
                eprintln!("  {name:<12}  (sample absent — skipped)");
                continue;
            }
        };
        let mut m = Machine::new();
        m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
        let mut sink = NullSink;
        m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});
        m.drive8.attach_disk(DiskImage {
            kind: kind.clone(),
            bytes,
            backing_path: Some(path.clone()),
            read_only: false,
        });
        m.run_for_full(800_000, &mut sink, |_, _, _, _, _, _, _| {});
        inject_keys(&mut m, b"LOAD\"*\",8,1\r");
        m.run_for_full(500_000, &mut sink, |_, _, _, _, _, _, _| {});
        inject_keys(&mut m, b"RUN\r");

        let start_clk = m.c64_core.clk;
        let chunk = 500_000u64;
        let t0 = Instant::now();
        let mut done = 0u64;
        while done < game_budget {
            m.run_for_full_capped(chunk, chunk * 2, &mut sink, |_, _, _, _, _, _, _| {});
            done += chunk;
        }
        let secs = t0.elapsed().as_secs_f64();
        let executed = m.c64_core.clk.wrapping_sub(start_clk);
        let mhz = executed as f64 / secs / 1_000_000.0;
        let rt = executed as f64 / secs / PAL_HZ;
        eprintln!(
            "  {:<12} {:>6} {:>12} {:>10.4} {:>8.3} {:>9.1}x",
            name,
            format!("{kind:?}"),
            executed,
            secs,
            mhz,
            rt
        );
    }
}

// ── Workload 4: checkpoint capture cost (Spec 807 baseline) ─────────────────
//
// The stream loop calls `stream_maybe_autocapture` inside its per-frame lock
// window (trx64-daemon streaming.rs:499), and the capture there builds a full
// `serde_json::Value` tree with the 64 KiB RAM base64-encoded into a fresh
// String (c64re_snapshot.rs ram_ta). At the default cadence of 25 frames that
// is ~2 captures/s and invisible. At cadence 1 — which is what a per-frame
// checkpoint (reverse playback) needs — it is 50/s per producer.
//
// This measures ONE capture in isolation, so Spec 807 has a before-number to be
// judged against. It reports µs per capture and the rendered tree size; the
// frame budget it has to fit inside is 20_000 µs (PAL 50 fps), and it shares
// that budget with the entire emulation.
#[test]
#[ignore = "perf benchmark; run --release with --ignored --nocapture"]
fn bench_checkpoint_capture() {
    if !roms_present() {
        eprintln!("skip bench_checkpoint_capture: ROMs absent at {ROM_DIR}");
        return;
    }
    const CAPTURES: usize = 500; // = 10 s of ring at cadence 1

    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut sink = NullSink;
    // Boot past READY so the captured state is a real machine, not a reset one.
    m.run_for_full(3_000_000, &mut sink, |_, _, _, _, _, _, _| {});

    // One capture up front for the size report (untimed).
    let sample = trx64_core::c64re_snapshot::capture_runtime_checkpoint(
        &m, "/tmp/bench.d64", "d64", None, None, None, None,
    );
    let rendered = serde_json::to_string(&sample).expect("render").len();
    let ram_b64 = sample["ram"]["b64"].as_str().map(str::len).unwrap_or(0);

    let mut per_capture_us = Vec::with_capacity(K_RUNS);
    for run in 0..K_RUNS {
        let t0 = Instant::now();
        let mut sink_sum = 0usize; // keep the tree alive so nothing is optimised out
        for _ in 0..CAPTURES {
            let cp = trx64_core::c64re_snapshot::capture_runtime_checkpoint(
                &m, "/tmp/bench.d64", "d64", None, None, None, None,
            );
            sink_sum += cp.as_object().map(|o| o.len()).unwrap_or(0);
        }
        let elapsed = t0.elapsed().as_secs_f64();
        assert!(sink_sum > 0, "captures produced empty trees");
        let us = elapsed * 1_000_000.0 / CAPTURES as f64;
        per_capture_us.push(us);
        eprintln!(
            "  [capture run {}/{}] {CAPTURES} captures in {:.4}s  →  {:.1} µs/capture",
            run + 1,
            K_RUNS,
            elapsed,
            us
        );
    }

    let min = per_capture_us.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = per_capture_us
        .iter()
        .cloned()
        .fold(f64::NEG_INFINITY, f64::max);
    let med = median(&mut per_capture_us.clone());
    // Share of one PAL frame (20 ms) a single capture eats, and what 50/s costs.
    let frame_pct = med / 20_000.0 * 100.0;
    eprintln!("\n========== checkpoint capture (Spec 807 baseline) ==========");
    eprintln!("  captures per timed run         : {CAPTURES}  (= 10 s of ring at cadence 1)");
    eprintln!("  µs/capture  min/median/max     : {min:.1} / {med:.1} / {max:.1}");
    eprintln!("  share of one PAL frame (20 ms) : {frame_pct:.2} %  per capture");
    eprintln!("  at cadence 1, one producer     : {:.2} % of wall-clock", frame_pct);
    eprintln!("  at cadence 1, ring + recorder  : {:.2} % of wall-clock", frame_pct * 2.0);
    eprintln!("  rendered tree (JSON string)    : {rendered} bytes");
    eprintln!("  of which base64 RAM            : {ram_b64} bytes  (raw 65536)");
    eprintln!(
        "  RAW (machine-parseable): checkpoint_capture k={} n={} min_us={:.3} med_us={:.3} max_us={:.3} tree_bytes={} ram_b64_bytes={}",
        K_RUNS, CAPTURES, min, med, max, rendered, ram_b64
    );
}

/// Spec 807 — WHERE the checkpoint tree's bytes are. Reports each top-level field's
/// rendered size, largest first, so the optimisation target is chosen by measurement
/// rather than by assumption. (RAM is the obvious suspect and is NOT the biggest.)
#[test]
#[ignore = "perf benchmark; run --release with --ignored --nocapture"]
fn bench_checkpoint_field_sizes() {
    if !roms_present() {
        eprintln!("skip bench_checkpoint_field_sizes: ROMs absent at {ROM_DIR}");
        return;
    }
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut sink = NullSink;
    m.run_for_full(3_000_000, &mut sink, |_, _, _, _, _, _, _| {});

    let cp = trx64_core::c64re_snapshot::capture_runtime_checkpoint(
        &m, "/tmp/bench.d64", "d64", None, None, None, None,
    );
    let total = serde_json::to_string(&cp).expect("render").len();

    let mut rows: Vec<(String, usize)> = cp
        .as_object()
        .expect("object")
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::to_string(v).map(|s| s.len()).unwrap_or(0)))
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1));

    eprintln!("\n========== checkpoint tree field sizes (Spec 807) ==========");
    eprintln!("  total rendered: {total} bytes");
    for (k, n) in &rows {
        if *n < 64 {
            continue;
        }
        eprintln!("  {:>9} B  {:>5.1} %  {k}", n, *n as f64 / total as f64 * 100.0);
    }
    // Second level for the biggest field, so a fat sub-node is named too.
    if let Some((top_key, _)) = rows.first() {
        if let Some(obj) = cp[top_key].as_object() {
            let mut sub: Vec<(String, usize)> = obj
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::to_string(v).map(|s| s.len()).unwrap_or(0)))
                .collect();
            sub.sort_by_key(|r| std::cmp::Reverse(r.1));
            eprintln!("  ── inside `{top_key}`:");
            for (k, n) in sub.iter().take(8) {
                if *n < 64 {
                    continue;
                }
                eprintln!("     {:>9} B  {k}", n);
            }
        }
    }
}

/// Spec 807 — the framebuffer waste. `capture_recorder_anchor_payload` (the ring's
/// per-frame producer, trx64-daemon main.rs:14930) calls `capture_runtime_checkpoint`
/// and then NULLS `vicPresentation.literalPortFb`/`literalPortFbStable` — after the
/// capture has already base64-encoded both (2 × 162 240 raw → 2 × 216 349 chars).
/// This times that sub-capture on its own, so the discarded work has a number.
#[test]
#[ignore = "perf benchmark; run --release with --ignored --nocapture"]
fn bench_checkpoint_framebuffer_waste() {
    if !roms_present() {
        eprintln!("skip bench_checkpoint_framebuffer_waste: ROMs absent at {ROM_DIR}");
        return;
    }
    const N: usize = 500;
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut sink = NullSink;
    m.run_for_full(3_000_000, &mut sink, |_, _, _, _, _, _, _| {});

    let mut vp_us = Vec::with_capacity(K_RUNS);
    let mut ram_us = Vec::with_capacity(K_RUNS);
    for _ in 0..K_RUNS {
        let t0 = Instant::now();
        let mut acc = 0usize;
        for _ in 0..N {
            let vp = trx64_core::c64re_snapshot::capture_vic_presentation(&m);
            acc += serde_json::to_value(vp).map(|v| v.as_object().map(|o| o.len()).unwrap_or(0)).unwrap_or(0);
        }
        vp_us.push(t0.elapsed().as_secs_f64() * 1_000_000.0 / N as f64);
        assert!(acc > 0);

        let t1 = Instant::now();
        let mut acc2 = 0usize;
        for _ in 0..N {
            acc2 += trx64_core::c64re_snapshot::ram_ta(&m)["b64"].as_str().map(str::len).unwrap_or(0);
        }
        ram_us.push(t1.elapsed().as_secs_f64() * 1_000_000.0 / N as f64);
        assert!(acc2 > 0);
    }
    let vp_med = median(&mut vp_us.clone());
    let ram_med = median(&mut ram_us.clone());
    eprintln!("\n========== checkpoint sub-costs (Spec 807) ==========");
    eprintln!("  capture_vic_presentation (BUILT then NULLED by the ring) : {vp_med:.1} µs");
    eprintln!("  ram_ta (64 KiB → base64)                                 : {ram_med:.1} µs");
    eprintln!(
        "  RAW (machine-parseable): checkpoint_subcosts vp_us={:.3} ram_us={:.3}",
        vp_med, ram_med
    );
}

/// Spec 807 §4.1 — the after-number for slice 1: the same capture with
/// `omit_framebuffer = true`, which is what both per-frame producers now run.
#[test]
#[ignore = "perf benchmark; run --release with --ignored --nocapture"]
fn bench_checkpoint_capture_omit_fb() {
    if !roms_present() {
        eprintln!("skip bench_checkpoint_capture_omit_fb: ROMs absent at {ROM_DIR}");
        return;
    }
    const CAPTURES: usize = 500;
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut sink = NullSink;
    m.run_for_full(3_000_000, &mut sink, |_, _, _, _, _, _, _| {});

    let sample = trx64_core::c64re_snapshot::capture_runtime_checkpoint_opts(
        &m, "/tmp/bench.d64", "d64", None, None, None, None, true,
    );
    assert!(
        sample["vicPresentation"]["literalPortFb"].is_null(),
        "omit_framebuffer=true must not encode literalPortFb"
    );
    let rendered = serde_json::to_string(&sample).expect("render").len();

    let mut us = Vec::with_capacity(K_RUNS);
    for _ in 0..K_RUNS {
        let t0 = Instant::now();
        let mut acc = 0usize;
        for _ in 0..CAPTURES {
            let cp = trx64_core::c64re_snapshot::capture_runtime_checkpoint_opts(
                &m, "/tmp/bench.d64", "d64", None, None, None, None, true,
            );
            acc += cp.as_object().map(|o| o.len()).unwrap_or(0);
        }
        assert!(acc > 0);
        us.push(t0.elapsed().as_secs_f64() * 1_000_000.0 / CAPTURES as f64);
    }
    let min = us.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = us.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let med = median(&mut us.clone());
    eprintln!("\n========== checkpoint capture, omit_framebuffer (Spec 807 §4.1) ==========");
    eprintln!("  µs/capture  min/median/max     : {min:.1} / {med:.1} / {max:.1}");
    eprintln!("  share of one PAL frame (20 ms) : {:.2} % per capture", med / 20_000.0 * 100.0);
    eprintln!("  rendered tree (JSON string)    : {rendered} bytes");
    eprintln!(
        "  RAW (machine-parseable): checkpoint_capture_omitfb k={} n={} min_us={:.3} med_us={:.3} max_us={:.3} tree_bytes={}",
        K_RUNS, CAPTURES, min, med, max, rendered
    );
}

/// Spec 807 §4.3 — does the JSON tree actually cost MEMORY? The rendered string is
/// 97 583 B for a lean entry, but the ring holds a live `serde_json::Value`, where
/// every field is a `String` key into a `Map` and every RAM byte lives inside one big
/// base64 `String`. If the resident cost is close to the rendered size, slices 2-5 buy
/// little and the spec should say so. This fills a ring with 500 lean entries (10 s at
/// cadence 1) and reports process RSS before and after.
#[test]
#[ignore = "perf benchmark; run --release with --ignored --nocapture"]
fn bench_checkpoint_ring_resident_memory() {
    if !roms_present() {
        eprintln!("skip bench_checkpoint_ring_resident_memory: ROMs absent at {ROM_DIR}");
        return;
    }
    fn rss_kb() -> u64 {
        // macOS: `ps -o rss= -p <pid>` in KiB. Linux: /proc/self/statm page count.
        if let Ok(s) = std::fs::read_to_string("/proc/self/statm") {
            let rss_pages: u64 = s.split_whitespace().nth(1).and_then(|t| t.parse().ok()).unwrap_or(0);
            return rss_pages * 4;
        }
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output();
        out.ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }

    const N: usize = 500;
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut sink = NullSink;
    m.run_for_full(3_000_000, &mut sink, |_, _, _, _, _, _, _| {});

    let rendered = serde_json::to_string(
        &trx64_core::c64re_snapshot::capture_runtime_checkpoint_opts(
            &m, "/tmp/b.d64", "d64", None, None, None, None, true,
        ),
    )
    .unwrap()
    .len();

    // A ring big enough to hold all N (byte budget is the secondary bound).
    let mut ring = trx64_core::checkpoint_ring::RuntimeCheckpointRing::with_budget_and_max_entries(
        (N as u64 + 8) * trx64_core::checkpoint_ring::SLOT_BYTES,
        N as u64 + 8,
    );
    let before = rss_kb();
    let t0 = Instant::now();
    for i in 0..N {
        let cp = trx64_core::c64re_snapshot::capture_runtime_checkpoint_opts(
            &m, "/tmp/b.d64", "d64", None, None, None, None, true,
        );
        ring.capture(cp, i as u64, i as u64 * 19_656).expect("capture");
    }
    let capture_us = t0.elapsed().as_secs_f64() * 1_000_000.0 / N as f64;
    let after = rss_kb();
    let held = ring.list().len();
    assert_eq!(held, N, "ring dropped entries: {held} of {N}");

    let delta_kb = after.saturating_sub(before);
    let per_entry_kb = delta_kb as f64 / N as f64;
    let accounted_kb = trx64_core::checkpoint_ring::SLOT_BYTES as f64 / 1024.0;
    eprintln!("\n========== checkpoint ring resident memory (Spec 807 §4.3) ==========");
    eprintln!("  entries held                   : {held}  (= 10 s at cadence 1)");
    eprintln!("  capture+store, END TO END      : {capture_us:.1} µs  ({:.2} % of a PAL frame)", capture_us / 20_000.0 * 100.0);
    eprintln!("  RSS before / after             : {before} / {after} KiB");
    eprintln!("  RSS delta                      : {delta_kb} KiB  ({:.1} MiB)", delta_kb as f64 / 1024.0);
    eprintln!("  per entry, RESIDENT            : {per_entry_kb:.1} KiB");
    eprintln!("  per entry, rendered JSON       : {:.1} KiB", rendered as f64 / 1024.0);
    eprintln!("  per entry, ring ACCOUNTS       : {accounted_kb:.1} KiB  (SLOT_BYTES)");
    eprintln!("  raw state would be (~RAM+chips): ~70 KiB");
    eprintln!(
        "  RAW (machine-parseable): ring_resident n={} rss_delta_kb={} per_entry_kb={:.2} rendered_b={} accounted_kb={:.1}",
        N, delta_kb, per_entry_kb, rendered, accounted_kb
    );
}

/// Spec 807 §4.3 follow-up — WHERE the resident cost of a ring entry actually is.
///
/// Moving the 64 KiB RAM out of the tree only took an entry from 208.9 to 192.4 KiB
/// (8 %), which means the 87 KiB base64 String was NOT the dominant cost and the
/// spec's "3× reduction" projection was wrong. This isolates the two halves: 500
/// copies of the raw RAM alone, then 500 copies of the residual tree alone.
#[test]
#[ignore = "perf benchmark; run --release with --ignored --nocapture"]
fn bench_checkpoint_entry_cost_split() {
    if !roms_present() {
        eprintln!("skip bench_checkpoint_entry_cost_split: ROMs absent at {ROM_DIR}");
        return;
    }
    fn rss_kb() -> u64 {
        if let Ok(s) = std::fs::read_to_string("/proc/self/statm") {
            let p: u64 = s.split_whitespace().nth(1).and_then(|t| t.parse().ok()).unwrap_or(0);
            return p * 4;
        }
        std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }
    const N: usize = 500;

    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut sink = NullSink;
    m.run_for_full(3_000_000, &mut sink, |_, _, _, _, _, _, _| {});

    // Half A: the raw RAM, 500×.
    let a0 = rss_kb();
    let rams: Vec<Vec<u8>> = (0..N).map(|_| m.ram.to_vec()).collect();
    let a1 = rss_kb();
    assert_eq!(rams.len(), N);

    // Half B: the residual tree (a lean checkpoint with `ram` nulled), 500×.
    let mut lean = trx64_core::c64re_snapshot::capture_runtime_checkpoint_opts(
        &m, "/tmp/b.d64", "d64", None, None, None, None, true,
    );
    lean["ram"] = serde_json::Value::Null;
    let residual_rendered = serde_json::to_string(&lean).unwrap().len();
    let b0 = rss_kb();
    let trees: Vec<serde_json::Value> = (0..N).map(|_| lean.clone()).collect();
    let b1 = rss_kb();
    assert_eq!(trees.len(), N);

    let ram_kb = (a1.saturating_sub(a0)) as f64 / N as f64;
    let tree_kb = (b1.saturating_sub(b0)) as f64 / N as f64;
    eprintln!("\n========== ring entry cost, split (Spec 807 §4.3) ==========");
    eprintln!("  raw RAM per entry              : {ram_kb:.1} KiB   (64.0 expected)");
    eprintln!("  residual JSON tree per entry   : {tree_kb:.1} KiB");
    eprintln!("  residual tree, RENDERED        : {:.1} KiB", residual_rendered as f64 / 1024.0);
    eprintln!("  → the tree costs {:.0}× its rendered size", tree_kb / (residual_rendered as f64 / 1024.0));
    eprintln!(
        "  RAW (machine-parseable): entry_split n={} ram_kb={:.2} tree_kb={:.2} residual_rendered_b={}",
        N, ram_kb, tree_kb, residual_rendered
    );
}

/// Rewind-transport question — what does ONE step of backward playback cost if every
/// frame is a REAL restore? If a restore fits in a PAL frame with room to spare, then
/// playback can simply move the machine and every existing viewer (the TUI's `/window`,
/// the C64RE UI, screenshots, register panels) follows for free, with nothing new built.
#[test]
#[ignore = "perf benchmark; run --release with --ignored --nocapture"]
fn bench_checkpoint_restore_step() {
    if !roms_present() {
        eprintln!("skip bench_checkpoint_restore_step: ROMs absent at {ROM_DIR}");
        return;
    }
    const N: usize = 200;
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut sink = NullSink;
    m.run_for_full(3_000_000, &mut sink, |_, _, _, _, _, _, _| {});

    let mut ring = trx64_core::checkpoint_ring::RuntimeCheckpointRing::with_budget_and_max_entries(
        (N as u64 + 8) * trx64_core::checkpoint_ring::SLOT_BYTES,
        N as u64 + 8,
    );
    let mut ids = Vec::with_capacity(N);
    for i in 0..N {
        let cp = trx64_core::c64re_snapshot::capture_runtime_checkpoint_opts(
            &m, "/tmp/b.d64", "d64", None, None, None, None, true,
        );
        ids.push(ring.capture(cp, i as u64, i as u64 * 19_656).expect("capture").id);
    }

    // Walk the ring BACKWARDS, restoring each anchor — this is exactly what one second
    // of backward playback does at cadence 1.
    let mut target = Machine::new();
    target.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut us = Vec::with_capacity(K_RUNS);
    for _ in 0..K_RUNS {
        let t0 = Instant::now();
        for id in ids.iter().rev() {
            let cp = ring.restore_snapshot(id).expect("snapshot");
            trx64_core::c64re_snapshot::restore_runtime_checkpoint(&mut target, &cp)
                .expect("restore");
        }
        us.push(t0.elapsed().as_secs_f64() * 1_000_000.0 / N as f64);
    }
    let med = median(&mut us.clone());
    let min = us.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = us.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    eprintln!("\n========== one backward-playback step (restore) ==========");
    eprintln!("  µs/step  min/median/max        : {min:.1} / {med:.1} / {max:.1}");
    eprintln!("  share of one PAL frame (20 ms) : {:.2} %", med / 20_000.0 * 100.0);
    eprintln!("  at 50 steps/s                  : {:.1} % of wall-clock", med / 20_000.0 * 100.0);
    eprintln!(
        "  RAW (machine-parseable): restore_step k={} n={} min_us={:.3} med_us={:.3} max_us={:.3}",
        K_RUNS, N, min, med, max
    );
}
