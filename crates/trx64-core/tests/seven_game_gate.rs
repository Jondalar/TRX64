//! seven_game_gate.rs — the 7-game behavioral gate vs c64re.
//!
//! For each real-software disk game: boot the full C64, mount the disk (D64 or
//! G64), inject `LOAD"*",8,1` + RUN via the keyboard buffer, run a fixed cycle
//! budget, then decide PASS/FAIL on the SAME criterion c64re's proof-canary-disk
//! (Spec 715) uses:
//!
//!   PASS = after LOAD + RUN, the C64 PC sustains a GAME-CODE address in RAM
//!          ($0200..$9FFF, outside KERNAL/BASIC ROM and the READY/serial stuck
//!          loops), proving the (fast)loader completed + the game is live —
//!          OR the title/gameplay screen renders (many distinct colors).
//!
//! The BAR is c64re PARITY (not cycle-exact): a game passes if TRX64 reaches the
//! same state class as c64re for the same disk + input.
//!
//! Run a single game:
//!   cargo test -p trx64-core --test seven_game_gate <name> -- --ignored --nocapture
//! Run all:
//!   cargo test -p trx64-core --test seven_game_gate -- --ignored --nocapture
//!
//! What this verdict does NOT check: the picture. PASS needs game code live or
//! >= 8 colours on screen; nothing compares the frame with a reference, so a game
//! that reaches its code with a corrupt or wrong picture passes. `scripts/gate.sh`
//! diffs the PNGs against the previous run's, and only as a note.
//!
//! Spec 871 — `GATE_DRIVE_B=<unit>`: each game runs twice, B off and then drive
//! position B powered at that unit with a blank disk in it, idle, switched on with
//! the C64. The B-off run is the unchanged gate above and writes the usual PNG; the
//! B-on run writes `gate_<name>_trx64_b<unit>.png` and is judged by its PICTURE: the
//! frame must equal the B-off frame of the same build byte for byte, unless the game
//! carries a named expectation in `B_ON_EXPECTED` saying why it differs. A game that
//! is expected to differ and matches fails too — the expectation is stale then.
//! Unset (the default) the machine has one drive and each game runs once, as before.
//!
//! Spec 872 §9.12 — `GATE_DRIVE_B_TYPE=1581` with `GATE_DRIVE_B`: position B is a 1581
//! (the 1581 DOS from `$TRX64_1581_ROM_DIR`, the ROM directory or VICE's
//! `data/DRIVES`) with a blank D81, idle. A characterisation, not a gate: the B-on
//! picture is judged against the B-off frame and printed, and where the two runs part
//! the first divergence is printed too (`B-ON 1581:` lines); nothing fails on it.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const SAMPLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/samples");
const TRACES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../traces");

fn roms_present() -> bool {
    let d = Path::new(ROM_DIR);
    d.join("kernal-901227-03.bin").exists()
        && (d.join("dos1541-325302-01+901229-05.bin").exists() || d.join("1541.bin").exists())
}

fn inject_keys(m: &mut Machine, s: &[u8]) {
    for (i, b) in s.iter().enumerate() {
        m.poke(0x0277 + i as u16, &[*b]);
    }
    m.poke(0x00c6, &[s.len() as u8]);
}

/// The Spec-715 stuck set: READY/BASIC idle, LOAD/SAVE stalls, serial RX stall.
fn is_stuck(pc: u16) -> bool {
    matches!(
        pc,
        0xE5CD..=0xE5D4 // READY/BASIC editor idle loop
            | 0xF6BF | 0xA483 | 0xF6C5 | 0xF6DA // LOAD/SAVE stalls
            | 0xEEA9 | 0xEEAF | 0xEEB2 | 0xED5A | 0xED5D // serial RX stall
    )
}

/// Game code lives in RAM, outside ROM ($A000+), outside the stuck loops.
fn game_running(pc: u16) -> bool {
    (0x0200..0xA000).contains(&pc) && !is_stuck(pc)
}

fn distinct_colors(rgba: &[u8]) -> usize {
    let mut set = HashSet::new();
    for px in rgba.chunks_exact(4) {
        set.insert((px[0], px[1], px[2]));
    }
    set.len()
}

struct GateResult {
    name: String,
    kind: DiskKind,
    /// PC sustained in game space (the earliest 2-sample-sustained hit).
    game_live: bool,
    first_game_pc: Option<u16>,
    final_pc: u16,
    /// Drive read GCR (head advanced + SYNC found + time in DOS read loop).
    drive_read_gcr: bool,
    sync_found: bool,
    head_advanced: bool,
    distinct_colors: usize,
    screen_nonblank: usize,
    /// Top boundary PCs post-RUN (for divergence pinning on FAIL).
    top_c64_pcs: Vec<(u16, u64)>,
    png_path: String,
    /// The frame written to `png_path` (the canvas, RGBA).
    rgba: Vec<u8>,
    width: usize,
    /// The unit drive B sat at, `None` for the one-drive run.
    drive_b: Option<u8>,
}

/// Run one game end-to-end and report behavioral state.
fn run_game(file: &str, kind: DiskKind, name: &str, drive_b: Option<u8>) -> Option<GateResult> {
    if !roms_present() {
        eprintln!("skip {name}: ROMs absent");
        return None;
    }
    let path = format!("{SAMPLES}/{file}");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("skip {name}: sample absent ({path})");
            return None;
        }
    };

    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut sink = NullSink;
    // Spec 871 — optionally a second 1541 on the bus, switched on with the C64 so its
    // DOS has finished its power-on routine and sits idle by the time of the LOAD.
    if let Some(unit) = drive_b {
        power_drive_b(&mut m, unit);
    }

    // Boot to BASIC READY.
    m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});

    // Mount + settle.
    m.drive8.attach_disk(DiskImage {
        kind: kind.clone(),
        bytes: bytes.clone(),
        backing_path: Some(path.clone()),
        read_only: false,
    });
    let head_before = m.drive8.rotation.gcr_head_offset;
    m.run_for_full(800_000, &mut sink, |_, _, _, _, _, _, _| {});

    // LOAD"*",8,1 + RETURN.
    inject_keys(&mut m, b"LOAD\"*\",8,1\r");

    // Drive the LOAD: run until the BASIC editor is idle again (load complete)
    // or a load cap. Track GCR read activity throughout.
    let mut sync_found = false;
    let mut max_head = head_before;
    let mut drive_pc_hist: HashMap<u16, u64> = HashMap::new();
    let mut ready_streak = 0u32;
    for _ in 0..400 {
        m.run_for_full(50_000, &mut sink, |pc, _, _, _, _, _, _| {
            *drive_pc_hist.entry(pc).or_insert(0) += 1;
        });
        if m.drive8.rotation.sync_found() != 0 {
            sync_found = true;
        }
        let h = m.drive8.rotation.gcr_head_offset;
        if h > max_head {
            max_head = h;
        }
        let pc = m.cpu6510.reg_pc;
        if (0xE5C0..=0xE5F0).contains(&pc) && m.read_full(0x00c6) == 0 {
            ready_streak += 1;
            if ready_streak >= 3 {
                break;
            }
        } else {
            ready_streak = 0;
        }
    }

    // Type RUN regardless (custom/protected loaders may not return to BASIC —
    // the LOAD itself can chain into the game; for those, RUN is a no-op but
    // harmless, and game_running is already detected).
    inject_keys(&mut m, b"RUN\r");

    // Run forward, sampling the boundary PC. PASS = game code sustained over two
    // consecutive samples (matches the proof-canary "sustained" rule), OR a
    // coherent title/gameplay frame renders (often the game's IRQ paints the
    // title while the main thread sits in a ROM/wait loop). We give a generous
    // budget because the FIRST file of a multi-disk game can come in over the
    // slow standard-KERNAL serial path (c64re's scramble ref needs ~30M cyc to
    // get the BASIC stub before the fastloader installs).
    let mut c64_hist: HashMap<u16, u64> = HashMap::new();
    let mut first_game_pc: Option<u16> = None;
    let mut game_live = false;
    let mut prev_game = false;
    // Track the BEST (most-colorful) frame seen across the whole run + its PNG —
    // a mid-redraw final frame must not mask a title that rendered earlier.
    let mut best_colors = 0usize;
    let mut best_rgba: Option<Vec<u8>> = None;
    let chunk = 100_000u64;
    // GATE_BUDGET env override (default 100M ~100s PAL) — lets a slow multi-file
    // loader (e.g. maniac) run long enough to reach its menu without changing the gate.
    let budget = std::env::var("GATE_BUDGET").ok().and_then(|v| v.parse().ok()).unwrap_or(100_000_000u64);
    let mut total = 0u64;
    while total < budget {
        m.run_for_full(chunk, &mut sink, |pc, _, _, _, _, _, _| {
            *drive_pc_hist.entry(pc).or_insert(0) += 1;
        });
        total += chunk;
        if m.drive8.rotation.sync_found() != 0 {
            sync_found = true;
        }
        let h = m.drive8.rotation.gcr_head_offset;
        if h > max_head {
            max_head = h;
        }
        let pc = m.cpu6510.reg_pc;
        *c64_hist.entry(pc).or_insert(0) += 1;
        let now_game = game_running(pc);
        if now_game {
            if first_game_pc.is_none() {
                first_game_pc = Some(pc);
            }
            if prev_game {
                game_live = true; // two consecutive samples in game space
            }
        }
        prev_game = now_game;
        // Sample the frame periodically; keep the most-colorful one.
        if total % 1_000_000 == 0 {
            let (_w, _h, rgba) = m.render_canvas_rgba();
            let c = distinct_colors(&rgba);
            if c > best_colors {
                best_colors = c;
                best_rgba = Some(rgba);
            }
        }
        // Early-out once we have BOTH game-live and a coherent title (>4 colors).
        if game_live && best_colors > 4 && total >= 12_000_000 {
            break;
        }
    }

    // GCR read evidence (for G64s / fastloaders).
    let in_gcr_loop: u64 = drive_pc_hist
        .iter()
        .filter(|(pc, _)| (0xF400..=0xF5FF).contains(*pc))
        .map(|(_, n)| *n)
        .sum();
    let head_advanced = max_head > head_before;
    let drive_read_gcr = sync_found && head_advanced && in_gcr_loop > 1000;

    // Render: write the BEST (most-colorful) frame seen during the run — a
    // mid-redraw final frame must not mask a title that rendered earlier.
    let (w, h, final_rgba) = m.render_canvas_rgba();
    let final_colors = distinct_colors(&final_rgba);
    if final_colors > best_colors {
        best_colors = final_colors;
        best_rgba = Some(final_rgba.clone());
    }
    let out_rgba = best_rgba.unwrap_or(final_rgba);
    let png = encode_png_rgba(w as u32, h as u32, &out_rgba);
    let png_path = match drive_b {
        Some(unit) if drive_b_is_1581() => format!("{TRACES}/gate_{name}_trx64_b{unit}_1581.png"),
        Some(unit) => format!("{TRACES}/gate_{name}_trx64_b{unit}.png"),
        None => format!("{TRACES}/gate_{name}_trx64.png"),
    };
    if let Some(unit) = drive_b {
        let b = &m.drive_b;
        eprintln!(
            "  drive B: unit {unit} powered={} clk={} motor={} half_track={} (idle = motor off, head on 18)",
            b.powered(),
            b.drive_clk,
            b.ports().motor_on,
            b.half_track()
        );
    }
    std::fs::write(&png_path, &png).expect("write PNG");

    // Screen text non-blank count (title chars).
    let vic_bank = m.vic_bank_base();
    let vm = ((m.vic.regs[0x18] >> 4) & 0x0f) as u16;
    let screen = vic_bank.wrapping_add(vm * 0x0400);
    let nonblank = (0..1000)
        .filter(|&i| {
            let c = m.read_full(screen.wrapping_add(i));
            c != 0x20 && c != 0x00
        })
        .count();

    let mut top: Vec<_> = c64_hist.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    top.truncate(8);

    Some(GateResult {
        name: name.to_string(),
        kind,
        game_live,
        first_game_pc,
        final_pc: m.cpu6510.reg_pc,
        drive_read_gcr,
        sync_found,
        head_advanced,
        distinct_colors: best_colors,
        screen_nonblank: nonblank,
        top_c64_pcs: top,
        png_path,
        rgba: out_rgba,
        width: w,
        drive_b,
    })
}

/// Spec 872 — `GATE_DRIVE_B_TYPE=1581`: position B is a 1581.
fn drive_b_is_1581() -> bool {
    std::env::var("GATE_DRIVE_B_TYPE").as_deref() == Ok("1581")
}

/// The 1581 DOS (Commodore IP, not bundled).
fn rom_1581() -> Option<Vec<u8>> {
    let vice = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../vice/vice/data/DRIVES");
    let mut dirs: Vec<String> = std::env::var("TRX64_1581_ROM_DIR").ok().into_iter().collect();
    dirs.push(ROM_DIR.into());
    dirs.push(vice.into());
    dirs.iter().find_map(|d| std::fs::read(Path::new(d).join("dos1581-318045-02.bin")).ok())
}

/// Spec 872 §9.12 — where a run with B at `unit` first parts from the run without it:
/// both machines are built alike up to `LOAD"*",8,1`, stepped frame by frame, and the
/// first frame that differs is replayed one instruction at a time.
fn first_divergence(file: &str, kind: DiskKind, unit: u8, max_frames: u32) -> String {
    let bytes = std::fs::read(format!("{SAMPLES}/{file}")).expect("sample");
    let build = |b: bool| {
        let mut m = Machine::new();
        m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
        let mut sink = NullSink;
        if b {
            power_drive_b(&mut m, unit);
        }
        m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});
        m.drive8.attach_disk(DiskImage { kind: kind.clone(), bytes: bytes.clone(), backing_path: None, read_only: false });
        m.run_for_full(800_000, &mut sink, |_, _, _, _, _, _, _| {});
        inject_keys(&mut m, b"LOAD\"*\",8,1\r");
        m
    };
    let (mut a, mut b) = (build(false), build(true));
    let same = |a: &Machine, b: &Machine| a.c64_core.clk == b.c64_core.clk && a.cpu6510.reg_pc == b.cpu6510.reg_pc && a.ram[..] == b.ram[..];
    let mut sink = NullSink;
    for f in 0..max_frames {
        let (pa, pb) = (a.clone(), b.clone());
        a.run_for_full(19_656, &mut sink, |_, _, _, _, _, _, _| {});
        b.run_for_full(19_656, &mut sink, |_, _, _, _, _, _, _| {});
        if same(&a, &b) {
            continue;
        }
        let (mut a, mut b) = (pa, pb);
        for _ in 0..100_000 {
            a.run_for_full(1, &mut sink, |_, _, _, _, _, _, _| {});
            b.run_for_full(1, &mut sink, |_, _, _, _, _, _, _| {});
            if a.c64_core.clk != b.c64_core.clk || a.cpu6510.reg_pc != b.cpu6510.reg_pc {
                return format!(
                    "first divergence in frame {f} after the LOAD, at C64 cycle {}: C64 ${:04X} (B off) vs ${:04X} (B on), drive {unit} at ${:04X}",
                    a.c64_core.clk,
                    a.cpu6510.reg_pc,
                    b.cpu6510.reg_pc,
                    b.drive_b.cpu().reg_pc
                );
            }
        }
        return format!("the runs part in frame {f} after the LOAD (C64 RAM), not at an instruction boundary");
    }
    format!("no divergence within {max_frames} frames after the LOAD")
}

/// Spec 871 — the unit in `GATE_DRIVE_B`, or `None` when the variable is unset.
fn drive_b_from_env() -> Option<u8> {
    Some(std::env::var("GATE_DRIVE_B").ok()?.parse().expect("GATE_DRIVE_B = a unit number"))
}

/// Power drive position B at `unit` with a blank disk in it.
fn power_drive_b(m: &mut Machine, unit: u8) {
    use trx64_core::drive::DrivePosition;
    m.set_drive_unit(DrivePosition::B, unit).expect("drive B unit");
    if drive_b_is_1581() {
        m.set_drive_type(DrivePosition::B, trx64_core::iec::DriveType::Drive1581).expect("B off: a 1581");
        m.drive_b.set_rom_1581(&rom_1581().expect("GATE_DRIVE_B_TYPE=1581 needs the 1581 DOS")).unwrap();
        m.drive_b.attach_disk(DiskImage { kind: DiskKind::D81, bytes: vec![0u8; 819_200], backing_path: None, read_only: false });
        m.set_drive_power(DrivePosition::B, true).expect("drive B on");
        return;
    }
    m.drive_b.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: vec![0u8; 174_848],
        backing_path: None,
        read_only: false,
    });
    m.set_drive_power(DrivePosition::B, true).expect("drive B on");
}

/// What the B-on frame is expected to be, against the same game's B-off frame.
enum BOn {
    /// Byte-identical: an idle second drive leaves the game alone.
    Identical,
    /// Differs anywhere: the game's own loader is broken by the second drive.
    Differs(&'static str),
    /// Differs, but only inside this box (x0, y0, x1, y1 inclusive, canvas pixels):
    /// the game runs the same, shifted in time; anything outside the box is a failure.
    DiffersWithin(&'static str, (usize, usize, usize, usize)),
}

/// Every 1541 on the bus answers ATN — the ATN-acknowledge gate pulls DATA in
/// hardware the moment ATN falls, addressed or not, then its DOS runs its ATN routine
/// (VICE iecbus.c conf3, via1d1541.c store_prb). Nothing isolates an idle drive. So:
fn b_on_expected(name: &str) -> BOn {
    match name {
        // Its loader at $0380 asserts ATN as its request line (~2 000 edges per
        // emulated second); drive 9 pulls DATA on each — the title never draws.
        "greenberet" => BOn::Differs("ATN-toggling fastloader ($0380-$038A); an idle drive 9 answers every ATN and pulls DATA"),
        // Its in-game loader asserts ATN a few times per block (C64 $4270-$4290);
        // drive 9 pulls DATA in its ATN IRQ — the title bitmap arrives corrupt.
        "motm" => BOn::Differs("ATN-toggling in-game loader ($4270-$4290); an idle drive 9 answers every ATN and pulls DATA"),
        // The first file comes in through the KERNAL. Every command byte under ATN is
        // received by drive 9 too, and the C64 waits for the slower listener: first
        // divergence at C64 cycle 24 540 206 (C64 in $ED23 vs $ED33, drive 9 in its
        // ATN code at $E8EF). The game runs the same, later; the frame differs only
        // in the blinking "Loading" label, caught in the other phase.
        "scramble" => BOn::DiffersWithin(
            "KERNAL serial load shifted by drive 9's ATN handshake; only the \"Loading\" label's blink phase differs",
            (298, 239, 351, 248),
        ),
        _ => BOn::Identical,
    }
}

/// The B-on frame against the B-off frame: `Ok(summary)` when the expectation holds.
fn judge_b_on(name: &str, off: &GateResult, on: &GateResult) -> Result<String, String> {
    let w = off.width;
    let mut n = 0usize;
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
    for (i, (p, q)) in off.rgba.chunks_exact(4).zip(on.rgba.chunks_exact(4)).enumerate() {
        if p != q {
            let (x, y) = (i % w, i / w);
            n += 1;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    let same = n == 0 && off.rgba.len() == on.rgba.len();
    let diff = if same {
        "byte-identical to B off".to_string()
    } else {
        format!("differs from B off in {n} px, box x {x0}..={x1} y {y0}..={y1}")
    };
    match b_on_expected(name) {
        BOn::Identical if same => Ok(diff),
        BOn::Identical => Err(format!("{diff} — expected byte-identical")),
        BOn::Differs(why) if !same => Ok(format!("{diff} — expected: {why}")),
        BOn::Differs(why) => Err(format!("{diff} — expected to DIFFER ({why}); the expectation is stale")),
        BOn::DiffersWithin(why, (bx0, by0, bx1, by1)) => {
            if same {
                Err(format!("{diff} — expected to differ inside x {bx0}..={bx1} y {by0}..={by1} ({why}); the expectation is stale"))
            } else if x0 >= bx0 && x1 <= bx1 && y0 >= by0 && y1 <= by1 {
                Ok(format!("{diff} — expected inside x {bx0}..={bx1} y {by0}..={by1}: {why}"))
            } else {
                Err(format!("{diff} — outside the expected box x {bx0}..={bx1} y {by0}..={by1} ({why})"))
            }
        }
    }
}

/// One game through the gate: B off as always; with `GATE_DRIVE_B` set, then B on,
/// judged against the B-off frame.
fn gate_game(file: &str, kind: DiskKind, name: &str) {
    let Some(off) = run_game(file, kind.clone(), name, None) else { return };
    report(&off);
    let Some(unit) = drive_b_from_env() else { return };
    let Some(on) = run_game(file, kind.clone(), name, Some(unit)) else { return };
    report(&on);
    if drive_b_is_1581() {
        // Spec 872 §9.12 — recorded, not judged.
        let picture = match judge_b_on(name, &off, &on) {
            Ok(s) | Err(s) => s,
        };
        eprintln!("B-ON 1581: {name}: {picture}");
        eprintln!("B-ON 1581: {name}: {}", first_divergence(file, kind, unit, 3000));
        return;
    }
    match judge_b_on(name, &off, &on) {
        Ok(s) => eprintln!("B-ON VERDICT: PASS {name}: {s}"),
        Err(s) => {
            eprintln!("B-ON VERDICT: FAIL {name}: {s}");
            panic!("B-on gate, {name}: {s}");
        }
    }
}

fn report(r: &GateResult) {
    // PASS = the Spec-715 game-live criterion (PC sustained in game RAM), OR a
    // coherent title/gameplay frame rendered (>= 8 distinct colors — the game's
    // IRQ painted the screen even if the sampled main-thread PC is in a ROM/wait
    // loop). PARTIAL = something rendered but neither bar fully met. FAIL = stuck
    // with a blank/black screen (load never reached the game).
    let title_rendered = r.distinct_colors >= 8;
    let verdict = if r.game_live && title_rendered {
        "PASS (game live + title rendered)"
    } else if r.game_live {
        "PASS (game code live in RAM)"
    } else if title_rendered {
        "PASS (title rendered via game IRQ)"
    } else if r.distinct_colors > 2 || r.screen_nonblank > 20 {
        "PARTIAL (screen renders, game-PC not sustained)"
    } else {
        "FAIL (stuck, blank screen — load never reached game)"
    };
    // The B-on run's reachability is printed, not counted: its verdict is the
    // picture comparison (`B-ON VERDICT`), and `VERDICT:` lines are what gate.sh counts.
    match r.drive_b {
        None => {
            eprintln!("\n========== {} ({:?}) ==========", r.name, r.kind);
            eprintln!("VERDICT: {verdict}");
        }
        Some(unit) => {
            eprintln!("\n========== {} ({:?}) — drive B on at {unit} ==========", r.name, r.kind);
            eprintln!("  reachability (not the B-on verdict): {verdict}");
        }
    }
    eprintln!(
        "  game_live={} first_game_pc={} final_pc=${:04X}",
        r.game_live,
        r.first_game_pc.map(|p| format!("${p:04X}")).unwrap_or("-".into()),
        r.final_pc
    );
    eprintln!(
        "  drive: read_gcr={} sync_found={} head_advanced={}",
        r.drive_read_gcr, r.sync_found, r.head_advanced
    );
    eprintln!(
        "  screen: distinct_colors={} nonblank_chars={}/1000",
        r.distinct_colors, r.screen_nonblank
    );
    eprintln!("  png: {}", r.png_path);
    eprintln!("  top post-RUN C64 PCs:");
    for (pc, n) in &r.top_c64_pcs {
        eprintln!("    ${pc:04X}: {n}");
    }
}

macro_rules! game_test {
    ($fn:ident, $file:expr, $kind:expr, $name:expr) => {
        #[test]
        #[ignore = "behavioral 7-game gate; run with --ignored --nocapture"]
        fn $fn() {
            gate_game($file, $kind, $name);
        }
    };
}

game_test!(g1_scramble, "scramble_infinity.d64", DiskKind::D64, "scramble");
game_test!(g2_polarbear, "POLARBEAR.d64", DiskKind::D64, "polarbear");
game_test!(g3_motm, "motm.g64", DiskKind::G64, "motm");
// NOTE: california_games_s1 is EXCLUDED from the gate. This .g64 dump does not
// carry the EPYX copy-protection track data, so the protected loader can never
// complete — it CANNOT pass on any accurate emulator (c64re included), so it is
// not a valid parity datapoint. (The fastloader still reaches game code in RAM,
// but the protection check never satisfies.) Left here disabled for the record.
#[allow(dead_code)]
const CALIFORNIA_EXCLUDED: &str = "california_games_s1[epyx_1987](ntsc).g64";
game_test!(
    g5_greenberet,
    "green_beret[ocean_1986](!).g64",
    DiskKind::G64,
    "greenberet"
);
game_test!(
    g6_impossible2,
    "impossible_mission_ii[epyx_1987](!).g64",
    DiskKind::G64,
    "impossible2"
);
game_test!(
    g7_lastninja,
    "last_ninja_remix_s1[system3_1991].g64",
    DiskKind::G64,
    "lastninja"
);
game_test!(
    g8_maniac,
    "maniac_mansion_s1[activision_1987](german)(manual)(!).g64",
    DiskKind::G64,
    "maniac"
);

// ── Minimal self-contained PNG encoder (no deps) ────────────────────────────
fn encode_png_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8);
    ihdr.push(6);
    ihdr.push(0);
    ihdr.push(0);
    ihdr.push(0);
    write_chunk(&mut out, b"IHDR", &ihdr);
    let mut raw = Vec::with_capacity((width as usize * 4 + 1) * height as usize);
    let stride = width as usize * 4;
    for y in 0..height as usize {
        raw.push(0u8);
        raw.extend_from_slice(&rgba[y * stride..y * stride + stride]);
    }
    let mut zlib = Vec::new();
    zlib.push(0x78);
    zlib.push(0x01);
    deflate_stored(&mut zlib, &raw);
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    write_chunk(&mut out, b"IDAT", &zlib);
    write_chunk(&mut out, b"IEND", &[]);
    out
}

fn deflate_stored(out: &mut Vec<u8>, data: &[u8]) {
    let mut off = 0usize;
    while off < data.len() {
        let block = std::cmp::min(0xffff, data.len() - off);
        let last = if off + block >= data.len() { 1u8 } else { 0u8 };
        out.push(last);
        let len = block as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(&data[off..off + block]);
        off += block;
    }
    if data.is_empty() {
        out.push(1);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0xffffu16.to_le_bytes());
    }
}

fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_in = Vec::with_capacity(4 + data.len());
    crc_in.extend_from_slice(kind);
    crc_in.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}
