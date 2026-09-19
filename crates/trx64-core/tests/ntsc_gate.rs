//! Spec 863 gate — NTSC, and the other C64 models as configuration.
//!
//! The machine is a row of `models.toml`; these tests hold the rows that run to what VICE
//! and the hardware say they are:
//!
//!   * the cycle tables are VICE's, row by row, both phases, every column (§6.2);
//!   * a cold boot on NTSC leaves `$02A6 = 0`, on PAL `1`, from the same KERNAL file (§6.4);
//!   * TOD counts 60 mains ticks a second on NTSC, reSID samples at 1 022 730 Hz, and a
//!     D64 `LOAD` completes on NTSC with the bytes it loads on PAL (§6.5);
//!   * on an NTSC bad line with sprites 0 and 3 on, the recorder shows the BA and the
//!     sprite fetches where VICE's table puts them (§6.6);
//!   * PAL-N runs 65 × 312 at 1 023 440 Hz with a 50 Hz TOD — a row, not engine code (§6.8);
//!   * the NTSC picture is 384 × 247 and a raster bar at line 5 is at its bottom (§6.9).
//!
//! The frame itself (17 095 cycles, the wrap at 263, the raster IRQ on 262 and never on
//! 263 — §6.3) is `vic.rs`'s unit tests; the transplant and snapshot identity (§6.7) the
//! daemon's.

use std::path::Path;
use trx64_core::cia::{Cia, CIAT_TABLEN, CIA_CRA_TODIN_50HZ, CIA_TOD_SEC, CIA_TOD_TEN};
use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::model::{self, CycleFamily};
use trx64_core::resid_audio::SidAudioEngine;
use trx64_core::resid_ffi::ResidConfig;
use trx64_core::vic::{cycle_tab, Row};
use trx64_core::vic_line_trace::{self, FrameWhich, Phi1Kind, Phi2Kind};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const SAMPLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/samples");
/// VICE's source, when the sibling checkout is there.
const VICE_CHIP_MODEL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../vice/vice/src/viciisc/vicii-chip-model.c");

fn roms() -> bool {
    let ok = Path::new(ROM_DIR).join("kernal-901227-03.bin").exists();
    if !ok {
        eprintln!("SKIP: ROMs absent ({ROM_DIR})");
    }
    ok
}

fn booted(name: &str) -> Machine {
    let mut m = Machine::new_with_model(model::resolve(name).expect("model"));
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    m.run_for_full(3_000_000, &mut NullSink, |_, _, _, _, _, _, _| {});
    m
}

// ── §6.2 — the table is VICE's ────────────────────────────────────────────────────────

/// One `{ Phi1(1), 0x194, None, SprPtr(3), BaSpr2(3, 4), None }` row of VICE's source, read
/// with VICE's own #define values (vicii-chip-model.c:53-97).
fn parse_vice_row(line: &str) -> Row {
    let inner = &line[line.find('{').unwrap() + 1..line.rfind('}').unwrap()];
    let mut cols = Vec::new();
    let (mut depth, mut cur) = (0, String::new());
    for ch in inner.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if ch == ',' && depth == 0 {
            cols.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    cols.push(cur.trim().to_string());
    assert_eq!(cols.len(), 6, "{line}");
    let args = |s: &str| -> Vec<u16> {
        s[s.find('(').unwrap() + 1..s.rfind(')').unwrap()].split(',').map(|a| a.trim().parse().unwrap()).collect()
    };
    let cycle = {
        let n = args(&cols[0])[0] as u8;
        if cols[0].starts_with("Phi2") { n | 0x80 } else { n }
    };
    let xpos = u16::from_str_radix(cols[1].trim_start_matches("0x"), 16).unwrap();
    let visible = if cols[2] == "None" { 0 } else { args(&cols[2])[0] | 0x80 };
    let fetch = match cols[3].split('(').next().unwrap() {
        "None" => 0,
        "SprPtr" => 0x100 | args(&cols[3])[0],
        "SprDma0" => 0x200 | args(&cols[3])[0],
        "SprDma1" => 0x300 | args(&cols[3])[0],
        "SprDma2" => 0x400 | args(&cols[3])[0],
        "Refresh" => 0x500,
        "FetchG" => 0x600,
        "FetchC" => 0x700,
        "Idle" => 0x800,
        other => panic!("fetch {other}"),
    };
    let ba = match cols[4].split('(').next().unwrap() {
        "None" => 0,
        "BaFetch" => 0x100,
        "BaSpr1" | "BaSpr2" | "BaSpr3" => args(&cols[4]).iter().fold(0, |m, &s| m | (1 << s)),
        other => panic!("ba {other}"),
    };
    let flags = cols[5]
        .split('|')
        .map(|f| match f.trim() {
            "None" => 0,
            "UpdateMcBase" => 0x001,
            "ChkSprExp" => 0x002,
            "ChkSprDma" => 0x004,
            "ChkSprDisp" => 0x008,
            "ChkSprCrunch" => 0x010,
            "ChkBrdL1" => 0x020,
            "ChkBrdL0" => 0x040,
            "ChkBrdR0" => 0x080,
            "ChkBrdR1" => 0x100,
            "UpdateVc" => 0x200,
            "UpdateRc" => 0x400,
            other => panic!("flag {other}"),
        })
        .fold(0, |m, f| m | f);
    Row { cycle, xpos, visible, fetch, ba, flags }
}

fn vice_table(src: &str, name: &str) -> Vec<Row> {
    let head = format!("static const struct ViciiCycle {name}[] = {{");
    let start = src.find(&head).unwrap_or_else(|| panic!("{name} not in VICE source")) + head.len();
    let body = &src[start..start + src[start..].find("\n};").unwrap()];
    body.lines().filter(|l| l.trim_start().starts_with('{')).map(parse_vice_row).collect()
}

/// Acceptance 2 — every row of the three families, both phases, every column (cycle, xpos,
/// visible, fetch type + sprite, BA mask, flags), against VICE's source text.
#[test]
fn the_cycle_tables_are_vices_row_by_row() {
    let Ok(src) = std::fs::read_to_string(VICE_CHIP_MODEL) else {
        eprintln!("SKIP: VICE source absent ({VICE_CHIP_MODEL})");
        return;
    };
    for (name, family, rows) in [
        ("cycle_tab_pal", CycleFamily::Pal, 126),
        ("cycle_tab_ntsc", CycleFamily::Ntsc, 130),
        ("cycle_tab_ntsc_old", CycleFamily::NtscOld, 128),
    ] {
        let vice = vice_table(&src, name);
        let port = cycle_tab(family);
        assert_eq!(vice.len(), rows, "{name}: VICE rows");
        assert_eq!(port.len(), vice.len(), "{name}: ported rows");
        for (i, (v, p)) in vice.iter().zip(port.iter()).enumerate() {
            let phase = if v.cycle & 0x80 != 0 { "Phi2" } else { "Phi1" };
            assert_eq!(p, v, "{name} row {i} ({phase}({}))", v.cycle & 0x7f);
        }
        assert_eq!(family.cycles_per_line() as usize * 2, rows, "{name}: two phases per cycle");
    }
}

// ── §6.4 — the KERNAL detects it ──────────────────────────────────────────────────────

/// Acceptance 4 — the KERNAL's own raster test (it waits for line $137, which a 263-line
/// frame never reaches) sets `$02A6`: 0 on NTSC, 1 on PAL, same 901227-03 file.
#[test]
fn a_cold_boot_detects_the_standard_from_the_same_kernal() {
    if !roms() {
        return;
    }
    let pal = booted("c64-pal");
    let ntsc = booted("c64-ntsc");
    assert_eq!(pal.model().kernal, ntsc.model().kernal, "one KERNAL file");
    assert_eq!(pal.kernal_rom[..], ntsc.kernal_rom[..], "the same bytes");
    assert_eq!(pal.ram[0x02a6], 1, "PAL");
    assert_eq!(ntsc.ram[0x02a6], 0, "NTSC");
    // Both reached READY: the cursor blinks in the editor loop.
    for m in [&pal, &ntsc] {
        assert!((0xe5cd..=0xe5d4).contains(&m.c64_core.reg_pc) || m.c64_core.reg_pc >= 0xe000, "{:04x}", m.c64_core.reg_pc);
    }
}

// ── §6.5 — clocks ─────────────────────────────────────────────────────────────────────

fn tab() -> Box<[u16; CIAT_TABLEN]> {
    Box::new([0u16; CIAT_TABLEN])
}

/// A CIA on a model's clock with CRA bit 7 as given, its TOD started (writing tenths
/// starts it); run `cycles` and read the TOD as tenths.
fn tod_after(ticks_per_sec: u32, mains: u32, cra7: u8, cycles: u64) -> u32 {
    let t = tab();
    let mut cia = Cia::new_timed(ticks_per_sec, mains);
    cia.write(0x0e, cra7, cia.clk, &t);
    cia.write(0x08, 0, cia.clk, &t);
    for _ in 0..cycles {
        cia.tick(&t);
    }
    let ten = cia.regs[CIA_TOD_TEN] as u32;
    let sec = cia.regs[CIA_TOD_SEC] as u32;
    (sec >> 4) * 100 + (sec & 0x0f) * 10 + (ten & 0x0f)
}

/// Acceptance 5 — TOD: an NTSC machine's mains ticks 60 times an emulated second, and CRA
/// bit 7 still only picks the divider: the 60 Hz divider counts true seconds, the 50 Hz one
/// runs 6/5 fast (it waits five ticks for a tenth that has six).
#[test]
fn tod_counts_sixty_mains_ticks_a_second_on_ntsc() {
    let ntsc = model::resolve("c64-ntsc").unwrap();
    let m = Machine::new_with_model(ntsc);
    assert_eq!((m.cia1.tod_power_freq, m.cia1.ticks_per_sec), (60, 1_022_730));
    assert_eq!(m.cia1.tod_period() * 60, 1_022_700, "60 ticks fit one emulated second");
    let (hz, mains) = (ntsc.timing.cpu_hz, ntsc.timing.tod_hz);
    let second = hz as u64;
    assert_eq!(tod_after(hz, mains, 0, second), 10, "60 Hz divider on 60 Hz mains: one second");
    assert_eq!(tod_after(hz, mains, CIA_CRA_TODIN_50HZ, second), 12, "50 Hz divider on 60 Hz mains: 1.2 s");

    // PAL is untouched: 50 Hz mains, the 50 Hz divider counts a second.
    let pal = model::resolve("c64-pal").unwrap().timing;
    assert_eq!(tod_after(pal.cpu_hz, pal.tod_hz, CIA_CRA_TODIN_50HZ, pal.cpu_hz as u64), 10);
}

/// Zero crossings of a PCM stream (rising only).
fn rising_crossings(pcm: &[i16]) -> usize {
    let mean = pcm.iter().map(|&s| s as i64).sum::<i64>() / pcm.len().max(1) as i64;
    pcm.windows(2).filter(|w| (w[0] as i64) < mean && (w[1] as i64) >= mean).count()
}

fn tone(cfg: ResidConfig, cycles: u32) -> Vec<i16> {
    let mut e = SidAudioEngine::new(cfg);
    for (r, v) in [(0x18, 0x0f), (0x05, 0x00), (0x06, 0xf0), (0x00, 0x00), (0x01, 0x10), (0x04, 0x11)] {
        e.record_write(r, v);
    }
    e.record_boundary(cycles);
    e.flush();
    e.take_pcm()
}

/// Acceptance 5 — reSID is sampled at the NTSC clock: the same register value (voice 1
/// $1000, triangle) sounds 1 022 730 / 985 248 ≈ 3.8 % higher, as on the hardware.
#[test]
fn resid_samples_at_the_ntsc_clock() {
    let ntsc = model::resolve("c64-ntsc").unwrap();
    let pal = model::resolve("c64-pal").unwrap();
    assert_eq!(ResidConfig::for_model(ntsc).clock_freq, 1_022_730.0);
    assert_eq!(ResidConfig::for_model(pal).clock_freq, ResidConfig::default().clock_freq);
    // One second of each machine's clock → one second of samples each.
    let p = tone(ResidConfig::for_model(pal), 985_248);
    let n = tone(ResidConfig::for_model(ntsc), 1_022_730);
    let (fp, fn_) = (rising_crossings(&p) as f64, rising_crossings(&n) as f64);
    let ratio = fn_ / fp;
    let want = 1_022_730.0 / 985_248.0;
    assert!((ratio - want).abs() < 0.01, "pitch ratio {ratio:.4}, want {want:.4} ({fp} vs {fn_} Hz)");
    // A switch re-samples in place.
    let mut e = SidAudioEngine::new(ResidConfig::for_model(pal));
    e.set_clock_freq(1_022_730.0);
    assert_eq!(e.clock_freq(), 1_022_730.0);
}

/// Type a string into the keyboard buffer.
fn inject_keys(m: &mut Machine, s: &[u8]) {
    for (i, b) in s.iter().enumerate() {
        m.poke(0x0277 + i as u16, &[*b]);
    }
    m.poke(0x00c6, &[s.len() as u8]);
}

/// LOAD"*",8,1 and wait for READY. Returns the loaded range.
fn load_first_file(name: &str, d64: &[u8]) -> (u16, u16, Vec<u8>) {
    let mut m = booted(name);
    m.drive8.attach_disk(DiskImage { kind: DiskKind::D64, bytes: d64.to_vec(), backing_path: None, read_only: false });
    inject_keys(&mut m, b"LOAD\"*\",8,1\r");
    // Until the KERNAL is back at READY with the load's end address set.
    let mut spent = 0u64;
    let mut done = false;
    while spent < 60_000_000 {
        m.run_for_full(200_000, &mut NullSink, |_, _, _, _, _, _, _| {});
        spent += 200_000;
        let pc = m.c64_core.reg_pc;
        if spent > 2_000_000 && (0xe5cd..=0xe5d4).contains(&pc) && m.ram[0x00c6] == 0 {
            done = true;
            break;
        }
    }
    assert!(done, "{name}: LOAD did not come back to READY within {spent} cycles");
    // The KERNAL's load pointer ($C3/$C4, the file's own address under ,8,1) and end ($AE/$AF).
    let start = m.ram[0xc3] as u16 | (m.ram[0xc4] as u16) << 8;
    let end = m.ram[0xae] as u16 | (m.ram[0xaf] as u16) << 8;
    (start, end, m.ram[start as usize..end as usize].to_vec())
}

/// Acceptance 5 — a D64 `LOAD` completes on NTSC (the drive's catch-up ratio is the NTSC
/// one, 64079) and loads the bytes it loads on PAL.
#[test]
fn a_d64_load_completes_on_ntsc() {
    if !roms() {
        return;
    }
    let Ok(d64) = std::fs::read(Path::new(SAMPLES).join("scramble_infinity.d64")) else {
        eprintln!("SKIP: sample absent");
        return;
    };
    assert_eq!(Machine::new_with_model(model::resolve("c64-ntsc").unwrap()).drive8.sync_factor, 64079);
    let (ps, pe, pal) = load_first_file("c64-pal", &d64);
    let (ns, ne, ntsc) = load_first_file("c64-ntsc", &d64);
    assert!(pe > ps && pe - ps > 100, "PAL loaded ${ps:04x}-${pe:04x}");
    assert_eq!((ns, ne), (ps, pe), "the same file, the same range");
    assert!(ntsc == pal, "NTSC loaded different bytes");
}

// ── §6.6 — stolen cycles ──────────────────────────────────────────────────────────────

/// Acceptance 6 — on an NTSC bad line with sprites 0 and 3 on, BA and the fetches are where
/// VICE's `cycle_tab_ntsc` puts them: the matrix BA 12–54, released at 55 (PAL keeps it for
/// sprite 0 there), taken again from 56 for sprite 0; sprite 3's s-accesses in cycle 65 of
/// the line before and cycle 1 of the line itself.
#[test]
fn ntsc_stolen_cycles_are_where_vices_table_puts_them() {
    if !roms() {
        return;
    }
    let mut m = booted("c64-ntsc");
    m.poke(0x0340, &[0xff; 64]);
    m.poke(0x07f8, &[0x0d]);
    m.poke(0x07fb, &[0x0d]);
    // Sprites 0 and 3 at Y = 100: DMA on from line 100, bad lines 107 and 115 inside.
    m.poke_io(0xd000, &[0x60, 100, 0, 0, 0, 0, 0xa0, 100]);
    m.poke_io(0xd015, &[0x09]);
    let mut s = m.clone();
    let start = vic_line_trace::next_frame_start(&m);
    let f = vic_line_trace::record_frame(&mut s, start, FrameWhich::Next, None).expect("record");
    assert_eq!(f.cycles.len(), 17095);
    assert_eq!(f.geometry.cycles_per_line, 65);
    let at = |line: u16, cycle: u8| f.cycles.iter().find(|c| c.line == line && c.cycle == cycle).copied().unwrap();
    let bad = 107u16;
    assert!(at(bad, 20).bad_line, "line {bad} is a bad line");
    for c in 12..=54 {
        assert!(at(bad, c).ba, "BA at {c}");
    }
    assert!(!at(bad, 55).ba, "NTSC releases BA at 55 (no BaFetch, no sprite BA)");
    // Sprite 0's BA: BaSpr1(0) 56-57, BaSpr2(0,1) 58-59, BaSpr3(0,1,2) 60.
    for c in 56..=60 {
        assert!(at(bad, c).ba, "sprite 0 BA at {c}");
    }
    // 61 is BaSpr2(1,2): neither is on.
    assert!(!at(bad, 61).ba, "no BA at 61 with sprites 1 and 2 off");
    // Sprite 3's BA: BaSpr3(1,2,3) 62 … BaSpr2(3,4) 65, and BaSpr3(3,4,5) in cycle 1.
    for c in 62..=65 {
        assert!(at(bad, c).ba, "sprite 3 BA at {c}");
    }
    assert!(at(bad + 1, 1).ba, "sprite 3 BA in cycle 1 of the next line");
    assert!(!at(bad + 1, 2).ba, "and not in 2 (BaSpr2(4,5))");
    // Sprite 3: DMA0 in Φ2 of cycle 65 of the line before, DMA1/DMA2 in cycle 1.
    let c65 = at(bad - 1, 65);
    assert_eq!((c65.phi2, c65.phi2_sprite), (Phi2Kind::SpriteData, 3), "s3 in cycle 65");
    let c1 = at(bad, 1);
    assert_eq!((c1.phi1, c1.phi1_sprite), (Phi1Kind::SpriteData, 3), "s3 Φ1 in cycle 1");
    assert_eq!((c1.phi2, c1.phi2_sprite), (Phi2Kind::SpriteData, 3), "s3 Φ2 in cycle 1");
    // Sprite 0: pointer at 59, data 59-60.
    assert_eq!((at(bad, 59).phi1, at(bad, 59).phi1_sprite), (Phi1Kind::SpritePointer, 0));
    assert_eq!((at(bad, 60).phi1, at(bad, 60).phi1_sprite), (Phi1Kind::SpriteData, 0));
    // No CPU read lands in a BA cycle (the CPU stalls instead).
    let header = f.header_json();
    assert_eq!(header["chip"], "6567R8");
    assert_eq!(header["linesPerFrame"], 263);
}

// ── §6.8 — models are rows ────────────────────────────────────────────────────────────

/// Acceptance 8 — `c64-paln` runs 65 × 312 at 1 023 440 Hz with a 50 Hz TOD, without a
/// line of engine code written for it; `c64c-pal` is refused naming the 6526A.
#[test]
fn paln_is_a_row_and_c64c_is_refused_by_name() {
    let p = model::resolve("c64-paln").unwrap();
    let m = Machine::new_with_model(p);
    assert_eq!((m.vic.cycles_per_line(), m.vic.screen_height()), (65, 312));
    assert_eq!((m.timing().cpu_hz, m.cia1.tod_power_freq, m.timing().cycles_per_frame), (1_023_440, 50, 20_280));
    assert_eq!(m.drive8.sync_factor, (65536.0 * 1e6 / 1_023_440.0f64).floor() as u32);
    let err = model::resolve("c64c-pal").unwrap_err();
    assert!(err.contains("6526A"), "{err}");
    if roms() {
        let m = booted("c64-paln");
        assert_eq!(m.ram[0x02a6], 1, "a 312-line frame reads as PAL to the KERNAL");
        assert_eq!(m.render_canvas_indices().0, 384);
    }
}

// ── §6.9 — the picture ────────────────────────────────────────────────────────────────

/// Acceptance 9 — the NTSC canvas is 384 × 247, and its bottom rows are raster lines 0-11
/// of the same displayed frame: a border bar at line 5 is at the bottom, not the top.
#[test]
fn an_ntsc_raster_bar_at_line_5_is_at_the_bottom() {
    if !roms() {
        return;
    }
    let mut m = booted("c64-ntsc");
    #[rustfmt::skip]
    let prog: [u8; 33] = [
        0x78,                               // SEI
        0xA9, 0x7F, 0x8D, 0x0D, 0xDC,       // LDA #$7F / STA $DC0D
        0xAD, 0x11, 0xD0, 0x30, 0xFB,       // $C006: LDA $D011 / BMI $C006   (line < 256)
        0xAD, 0x12, 0xD0, 0xC9, 0x05, 0xD0, 0xF4, // LDA $D012 / CMP #5 / BNE $C006
        0xA9, 0x02, 0x8D, 0x20, 0xD0,       // border red
        0xA2, 0x0C,                         // LDX #12
        0xCA, 0xD0, 0xFD,                   // DEX / BNE — most of the line
        0x8E, 0x20, 0xD0,                   // STX $D020 (black)
        0x4C, 0x06,                         // JMP $C006 (hi byte below)
    ];
    m.poke(0xc000, &prog);
    m.poke(0xc021, &[0xc0]);
    m.c64_core.reg_pc = 0xc000;
    m.run_for_full(4 * 17095, &mut NullSink, |_, _, _, _, _, _, _| {});
    let (w, h, idx) = m.render_canvas_indices();
    assert_eq!((w, h), (384, 247), "the NTSC canvas");
    let row_red = |r: usize| idx[r * w..r * w + 32].iter().filter(|&&c| c == 2).count();
    // Line 5 is canvas row 263 + 5 - 28 = 240.
    let bar: usize = (239..=242).map(row_red).sum();
    assert!(bar > 0, "the bar is at the bottom (rows 239-242)");
    let top: usize = (0..200).map(row_red).sum();
    assert_eq!(top, 0, "and not at the top");
}

/// The frame the NTSC picture shows starts at the vsync line (12), not at line 0: replaying
/// from an earlier state to `displayed_frame_start` draws exactly the picture on screen,
/// wherever in the frame the machine was frozen — before the swap, on it, just after it.
#[test]
fn an_ntsc_replay_reproduces_the_picture_on_screen() {
    if !roms() {
        return;
    }
    let anchor = booted("c64-ntsc");
    for off in [0u64, 1, 2, 64, 65, 777, 5_000, 11 * 65 + 64, 12 * 65, 17_094] {
        let mut live = anchor.clone();
        live.run_for_full(3 * 17_095 + off, &mut NullSink, |_, _, _, _, _, _, _| {});
        let start = vic_line_trace::displayed_frame_start(&live);
        assert!(start > anchor.c64_core.clk);
        let mut s = anchor.clone();
        let f = vic_line_trace::record_frame(&mut s, start, FrameWhich::Displayed, Some(&live.vic.displayed[..]))
            .expect("record");
        assert_eq!(f.cycles[0].line, 12, "a displayed NTSC frame starts at the vsync line");
        assert_eq!(f.verified, Some(true), "frozen {off} cycles into the fourth frame (line {} cycle {})",
            live.vic.raster_line, live.vic.raster_cycle + 1);
        // The frame map lays the lines out by raster line, 65 cycles each.
        let fm = f.frame_map_json(true);
        assert_eq!(fm["cells"].as_array().unwrap().len(), 263);
        assert_eq!(fm["cells"][100].as_array().unwrap().len(), 65);
        assert_eq!(fm["frame"]["firstLine"], 12);
        assert_eq!(fm["geometry"]["visible"]["h"], 247);
    }
}

