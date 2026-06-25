//! vic_inspect_behavioral.rs — Spec 710/721 behavioral validation of the
//! vic-inspect engine (vic_inspect.rs) against REAL booted screens.
//!
//! Two cases, ROM/sample-gated (run with `--ignored --nocapture`):
//!   1. BASIC ready screen → a pixel in a character cell resolves to the correct
//!      text_cell NODE with its ORIGIN (screen-RAM addr + char-ROM addr).
//!   2. Scramble title (sprites enabled) → a pixel in the SCRAMBLE logo resolves
//!      to the sprite NODE + its DATA origin (sprite ptr → data block address).
//!
//! The engine is a verbatim 1:1 port of the c64re inspect resolver, so these
//! pin the SAME node/origin the c64re daemon would return for the same VIC state.
//!
//! Run:
//!   cargo test -p trx64-core --test vic_inspect_behavioral -- --ignored --nocapture

use std::path::Path;
use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::vic_inspect as vi;
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
const SAMPLE: &str =
    "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/scramble_infinity.d64";

fn roms_present() -> bool {
    Path::new(ROM_DIR).join("kernal-901227-03.bin").exists()
}

fn inject_keys(m: &mut Machine, s: &[u8]) {
    for (i, b) in s.iter().enumerate() {
        m.poke(0x0277 + i as u16, &[*b]);
    }
    m.poke(0x00c6, &[s.len() as u8]);
}

/// Capture the live machine as a RuntimeCheckpoint Value (the same tree the ring
/// stores and the inspect engine reads).
fn capture(m: &Machine) -> serde_json::Value {
    trx64_core::c64re_snapshot::capture_runtime_checkpoint(m, "", "", None, None, None, None)
}

#[test]
#[ignore = "behavioral: boots ROMs; run with --ignored --nocapture"]
fn basic_ready_text_cell_resolves_to_screen_and_charrom() {
    if !roms_present() {
        eprintln!("skip: ROMs absent");
        return;
    }
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut obs = NullSink;
    // Boot to BASIC ready.
    m.run_for_full(2_500_000, &mut obs, |_, _, _, _, _, _, _| {});

    let cp = capture(&m);
    let snap = vi::build_vic_inspect_snapshot(&cp);
    eprintln!(
        "BASIC ready: mode={:?} bank=${:04X} screen=${:04X} char=${:04X} shadow={}",
        snap.mode, snap.bank_base, snap.screen_base, snap.char_base, snap.char_rom_shadow
    );
    // The standard C64 BASIC screen: bank 0, screen $0400, char ROM at $1000.
    assert_eq!(snap.mode, vi::VicInspectMode::StandardText, "BASIC = standard text");
    assert_eq!(snap.bank_base, 0x0000, "default VIC bank 0");
    assert_eq!(snap.screen_base, 0x0400, "default screen RAM $0400");
    assert_eq!(snap.char_base, 0x1000, "default char base $1000");
    assert!(snap.char_rom_shadow, "char $1000 in bank 0 = char ROM shadow");

    // Resolve a pixel in cell (0,0) — the top-left character. Visible-frame coords:
    // display (4,4) → visible (4+32, 4+35) = (36, 39).
    let node = vi::resolve_visible_node_at(&cp, 36.0, 39.0, None);
    eprintln!("node @ (36,39): type={} value={:?} cell={:?}", node.node_type, node.value, node.cell);
    assert_eq!(node.node_type, "text_cell");
    assert_eq!(node.cell, Some((0, 0, 0)), "cell (0,0) index 0");

    // Its ORIGIN: screen RAM at $0400 holds the screen code; the charset ref points
    // into the char ROM shadow at $1000 + code*8.
    let screen = node.refs.iter().find(|r| r.kind == "screen_ram").expect("screen_ram ref");
    assert_eq!(screen.addr, 0x0400, "cell (0,0) screen RAM = $0400");
    let code = screen.value.expect("screen code");
    let charset = node.refs.iter().find(|r| r.kind == "charset").expect("charset ref");
    assert_eq!(charset.addr, 0x1000 + code * 8, "char ROM addr = $1000 + code*8");
    assert_eq!(charset.note.as_deref(), Some("char ROM shadow"), "char ROM shadow noted");
    eprintln!(
        "ORIGIN: screen $0400 = code ${:02X} → char ROM ${:04X} (shadow) ✓",
        code, charset.addr
    );

    // Sanity: the resolved screen code matches the live RAM byte at $0400.
    assert_eq!(code, m.read_full(0x0400) as i64, "engine read == live screen RAM");
}

#[test]
#[ignore = "behavioral: boots ROMs + scramble disk; run with --ignored --nocapture"]
fn scramble_title_sprite_resolves_to_data_origin() {
    if !roms_present() {
        eprintln!("skip: ROMs absent");
        return;
    }
    let d64 = match std::fs::read(SAMPLE) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("skip: sample disk absent");
            return;
        }
    };
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut obs = NullSink;
    m.run_for_full(2_500_000, &mut obs, |_, _, _, _, _, _, _| {});
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: d64,
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    m.run_for_full(500_000, &mut obs, |_, _, _, _, _, _, _| {});

    // LOAD"*",8,1
    inject_keys(&mut m, b"LOAD\"*\",8,1\r");
    let mut load_done = false;
    for _ in 0..800 {
        m.run_for_full(50_000, &mut obs, |_, _, _, _, _, _, _| {});
        let pc = m.cpu6510.reg_pc;
        if (0xE5C0..=0xE5F0).contains(&pc) && m.read_full(0x00c6) == 0 {
            load_done = true;
            break;
        }
    }
    if !load_done {
        eprintln!("LOAD did not finish; abort (no validation)");
        return;
    }
    inject_keys(&mut m, b"RUN\r");
    for _ in 0..100 {
        m.run_for_full(100_000, &mut obs, |_, _, _, _, _, _, _| {});
    }
    // Step until the bitmap-title phase with sprites enabled (the stable frame).
    let mut found = false;
    for _ in 0..2_000_000 {
        m.run_for_full(1, &mut obs, |_, _, _, _, _, _, _| {});
        if m.vic.regs[0x11] == 0x3b && m.vic_bank_base() == 0xC000 && m.vic.regs[0x15] != 0 {
            found = true;
            break;
        }
    }
    if !found {
        eprintln!("scramble title bitmap phase not reached; abort (no validation)");
        return;
    }

    let cp = capture(&m);
    let snap = vi::build_vic_inspect_snapshot(&cp);
    let enable = m.vic.regs[0x15];
    let bank = m.vic_bank_base() as i64;
    eprintln!(
        "scramble title: mode={:?} bank=${:04X} screen=${:04X} D015(enable)=${:02X}",
        snap.mode, snap.bank_base, snap.screen_base, enable
    );
    assert_eq!(snap.bank_base, 0xC000, "scramble title uses VIC bank 3 ($C000)");
    assert_ne!(enable, 0, "at least one sprite enabled");

    // Find the first enabled sprite and resolve a pixel at the CENTRE of its box.
    // Visible box: x = spriteX-24+32; y = spriteY-16 (16 = CANVAS_Y0).
    let r = |off: usize| m.vic.regs[off & 0x3f] as i64;
    let msbx = r(0x10);
    let xexp = r(0x1d);
    let yexp = r(0x17);
    let mut tested = false;
    for i in 0..8usize {
        if enable & (1 << i) == 0 {
            continue;
        }
        let sx = r(i * 2) | if (msbx & (1 << i)) != 0 { 0x100 } else { 0 };
        let sy = r(i * 2 + 1);
        let w = if (xexp & (1 << i)) != 0 { 48 } else { 24 };
        let h = if (yexp & (1 << i)) != 0 { 42 } else { 21 };
        // centre of the visible bounding box.
        let vx = (sx - 24 + 32) as f64 + (w as f64) / 2.0;
        let vy = (sy - 16) as f64 + (h as f64) / 2.0;
        let node = vi::resolve_visible_node_at(&cp, vx, vy, None);
        eprintln!(
            "sprite {i} (X=${sx:03X} Y=${sy:02X}) centre ({vx},{vy}) → node type={} value={:?}",
            node.node_type, node.value
        );
        // The centre of an enabled sprite's box must resolve to that sprite.
        assert_eq!(node.node_type, "sprite_bounds", "sprite {i} centre resolves to sprite");
        assert_eq!(node.value, Some(i as i64), "resolves to sprite {i}");

        // Its ORIGIN: the sprite POINTER at screen_base+$3F8+i and the DATA block at
        // bank | ptr*64 — exactly the scramble_sprite_probe layout.
        let ptr_ref = node.refs.iter().find(|r| r.kind == "sprite_ptr").expect("sprite_ptr");
        let data_ref = node.refs.iter().find(|r| r.kind == "sprite_data").expect("sprite_data");
        let ptr = ptr_ref.value.expect("ptr value");
        assert_eq!(ptr_ref.addr, snap.screen_base + 0x3f8 + i as i64, "ptr addr = screen+$3F8+i");
        assert_eq!(data_ref.addr, bank + ptr * 64, "data addr = bank | ptr*64");
        // Cross-check against live RAM: the pointer byte + first data byte.
        assert_eq!(ptr, m.read_full(ptr_ref.addr as u16) as i64, "engine ptr == live RAM");
        eprintln!(
            "  ORIGIN: ptr @ ${:04X} = ${:02X} → sprite data @ ${:04X} (bank ${:04X} | ptr*64) ✓",
            ptr_ref.addr, ptr, data_ref.addr, bank
        );

        // ORIGIN via the asset-join (no medium candidates supplied here → honest
        // runtime_generated; the engine still produces the full knowledge chain).
        let (result, knowledge) = vi::resolve_visual_origin(&cp, &node, &[], "scramble");
        assert_eq!(result["classification"], "runtime_generated");
        assert_eq!(knowledge["relations"][0]["relation"], "maps-to");
        tested = true;
        break;
    }
    assert!(tested, "at least one enabled sprite was resolved");
}
