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

use std::collections::{HashMap, HashSet};
use std::path::Path;
use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
const SAMPLES: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples";
const TRACES: &str = "/Users/alex/Development/C64/Tools/TRX64/traces";

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
}

/// Run one game end-to-end and report behavioral state.
fn run_game(file: &str, kind: DiskKind, name: &str) -> Option<GateResult> {
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
    // consecutive samples (matches the proof-canary "sustained" rule).
    let mut c64_hist: HashMap<u16, u64> = HashMap::new();
    let mut first_game_pc: Option<u16> = None;
    let mut game_live = false;
    let mut prev_game = false;
    let chunk = 100_000u64;
    let budget = 40_000_000u64; // ~40s PAL, generous for a slow custom loader
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
        // Once we've confirmed game-live AND the screen is non-trivial, we can
        // stop early to save time.
        if game_live && total >= 12_000_000 {
            let (_w, _h, rgba) = m.render_canvas_rgba();
            if distinct_colors(&rgba) > 3 {
                break;
            }
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

    // Render the final framebuffer to a PNG.
    let (w, h, rgba) = m.render_canvas_rgba();
    let png = encode_png_rgba(w as u32, h as u32, &rgba);
    let png_path = format!("{TRACES}/gate_{name}_trx64.png");
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
        distinct_colors: distinct_colors(&rgba),
        screen_nonblank: nonblank,
        top_c64_pcs: top,
        png_path,
    })
}

fn report(r: &GateResult) {
    let verdict = if r.game_live {
        "PASS (game code live in RAM)"
    } else if r.distinct_colors > 4 || r.screen_nonblank > 20 {
        "PARTIAL (screen renders, game-PC not sustained)"
    } else {
        "FAIL"
    };
    eprintln!("\n========== {} ({:?}) ==========", r.name, r.kind);
    eprintln!("VERDICT: {verdict}");
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
            if let Some(r) = run_game($file, $kind, $name) {
                report(&r);
            }
        }
    };
}

game_test!(g1_scramble, "scramble_infinity.d64", DiskKind::D64, "scramble");
game_test!(g2_polarbear, "POLARBEAR.d64", DiskKind::D64, "polarbear");
game_test!(g3_motm, "motm.g64", DiskKind::G64, "motm");
game_test!(
    g4_california,
    "california_games_s1[epyx_1987](ntsc).g64",
    DiskKind::G64,
    "california"
);
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
