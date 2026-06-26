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

const ROM_DIR: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
const SAMPLES: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples";

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
