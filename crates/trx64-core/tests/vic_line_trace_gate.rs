//! Spec 859 gate — the line as the VIC saw it.
//!
//! The recorder writes what the chip did; these tests hold it to the numbers every VIC
//! document agrees on and to the machine's own clock:
//!
//!   * a frame is 312 × 63 cycles, each line 1..=63 in order, and the recorder's clock
//!     ends where the machine's does;
//!   * a bad line has 40 c-accesses and BA down for 43 cycles (40 + the 3-cycle lead);
//!   * a sprite with DMA on takes 2 Φ2 s-accesses and pulls BA 3 cycles early;
//!   * no CPU read lands in a cycle with BA down (it stalls instead);
//!   * recording changes nothing the machine computes;
//!   * a replay from an earlier state reproduces the picture on screen (`verified`).

use std::path::Path;
use trx64_core::vic_line_trace::{self, FrameWhich, Phi1Kind, Phi2Kind, CYCLES_PER_FRAME};
use trx64_core::{BusKind, Machine, NullSink};

const ROM_DIR: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");

fn booted() -> Option<Machine> {
    if !Path::new(ROM_DIR).join("kernal-901227-03.bin").exists() {
        eprintln!("SKIP: ROMs absent ({ROM_DIR})");
        return None;
    }
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    // Past the KERNAL's screen init: READY, text mode, DEN on, YSCROLL 3.
    m.run_for_full(3_000_000, &mut NullSink, |_, _, _, _, _, _, _| {});
    Some(m)
}

fn next_frame_start(m: &Machine) -> u64 {
    m.c64_core.clk - vic_line_trace::frame_position(m) + CYCLES_PER_FRAME
}

#[test]
fn a_frame_is_312_lines_of_63_cycles_and_the_clock_agrees() {
    let Some(m) = booted() else { return };
    let mut s = m.clone();
    let start = next_frame_start(&m);
    let f = vic_line_trace::record_frame(&mut s, start, FrameWhich::Next, None).expect("record");
    assert_eq!(f.cycles.len(), CYCLES_PER_FRAME as usize);
    for (i, c) in f.cycles.iter().enumerate() {
        assert_eq!(c.line as usize, i / 63, "cycle {i}");
        assert_eq!(c.cycle as usize, i % 63 + 1, "cycle {i}");
        assert_eq!(c.clk, start + i as u64);
    }
    // Cycle 1 of line 0: the counter still reads 311 there (it resets in cycle 2).
    assert_eq!(f.cycles[0].raster, 311);
    assert_eq!(f.cycles[1].raster, 0);
}

#[test]
fn a_bad_line_has_40_c_accesses_and_43_ba_cycles() {
    let Some(m) = booted() else { return };
    let mut s = m.clone();
    let f = vic_line_trace::record_frame(&mut s, next_frame_start(&m), FrameWhich::Next, None).expect("record");
    let mut bad = 0;
    for line in 0..312usize {
        let row = &f.cycles[line * 63..line * 63 + 63];
        if !row.iter().any(|c| c.bad_line) {
            continue;
        }
        bad += 1;
        let c_acc = row.iter().filter(|c| c.phi2 == Phi2Kind::Matrix).count();
        let ba = row.iter().filter(|c| c.ba).count();
        let g = row.iter().filter(|c| c.phi1 == Phi1Kind::Graphics).count();
        assert_eq!(c_acc, 40, "line {line}: c-accesses");
        assert_eq!(ba, 43, "line {line}: BA-low cycles");
        assert_eq!(g, 40, "line {line}: g-accesses");
        // The first three BA cycles are the lead-in: AEC still high, the VIC's c-access
        // blocked, the CPU may still write.
        let first_ba = row.iter().position(|c| c.ba).unwrap();
        assert!(!row[first_ba].aec && !row[first_ba + 2].aec && row[first_ba + 3].aec, "line {line}: AEC lags BA by 3");
    }
    assert_eq!(bad, 25, "25 bad lines in a text screen");
}

#[test]
fn a_sprite_takes_two_s_accesses_and_pulls_ba_three_cycles_early() {
    let Some(mut m) = booted() else { return };
    m.poke(0x07f8, &[0x0d]); // sprite 0 data at $0340
    m.poke_io(0xd000, &[0x80, 0x90]); // X, Y
    m.poke_io(0xd015, &[0x01]);
    let mut s = m.clone();
    let f = vic_line_trace::record_frame(&mut s, next_frame_start(&m), FrameWhich::Next, None).expect("record");
    // A line inside the sprite (Y $90..$A4) that is not a bad line.
    let line = (0x92usize..0xa4).find(|l| !f.cycles[l * 63].bad_line && !f.cycles[l * 63 + 20].bad_line).unwrap();
    let row = &f.cycles[line * 63..line * 63 + 63];
    let s_phi2 = row.iter().filter(|c| c.phi2 == Phi2Kind::SpriteData && c.phi2_sprite == 0).count();
    let s_phi1 = row.iter().filter(|c| c.phi1 == Phi1Kind::SpriteData && c.phi1_sprite == 0).count();
    assert_eq!((s_phi2, s_phi1), (2, 1), "line {line}: sprite 0 fetches 3 bytes, two of them in Φ2");
    // Sprite 0's pointer fetch is in cycle 58; BA falls in cycle 55 (three cycles before).
    let ba: Vec<u8> = row.iter().filter(|c| c.ba).map(|c| c.cycle).collect();
    assert_eq!(ba, vec![55, 56, 57, 58, 59], "line {line}: BA for sprite 0");
    let ptr = row.iter().find(|c| c.phi1 == Phi1Kind::SpritePointer && c.phi1_sprite == 0).unwrap();
    assert_eq!((ptr.cycle, ptr.phi1_addr, ptr.phi1_data), (58, 0x07f8, 0x0d));
}

#[test]
fn no_cpu_read_lands_in_a_ba_low_cycle() {
    let Some(m) = booted() else { return };
    let mut s = m.clone();
    let f = vic_line_trace::record_frame(&mut s, next_frame_start(&m), FrameWhich::Next, None).expect("record");
    let mut stalls = 0;
    for c in &f.cycles {
        let reads: Vec<_> = f
            .accesses
            .iter()
            .filter(|a| a.clk == c.clk && matches!(a.kind, BusKind::Read | BusKind::Fetch))
            .collect();
        if c.ba {
            assert!(reads.is_empty(), "line {} cycle {}: read with BA low: {:?}", c.line, c.cycle, reads);
            if !f.accesses.iter().any(|a| a.clk == c.clk) {
                stalls += 1;
            }
        }
    }
    assert!(stalls > 0, "a bad line must stall the CPU somewhere");
}

#[test]
fn recording_changes_nothing_the_machine_computes() {
    let Some(m) = booted() else { return };
    let start = next_frame_start(&m);
    let mut recorded = m.clone();
    vic_line_trace::record_frame(&mut recorded, start, FrameWhich::Next, None).expect("record");
    let mut plain = m.clone();
    let budget = recorded.c64_core.clk - plain.c64_core.clk;
    plain.run_for_full(budget, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert_eq!(plain.c64_core.clk, recorded.c64_core.clk);
    assert!(plain.ram[..] == recorded.ram[..], "RAM differs");
    assert!(plain.vic.displayed[..] == recorded.vic.displayed[..], "picture differs");
    assert!(plain.vic.dbuf[..] == recorded.vic.dbuf[..], "frame in progress differs");
    assert_eq!((plain.vic.raster_line, plain.vic.raster_cycle), (recorded.vic.raster_line, recorded.vic.raster_cycle));
}

#[test]
fn a_replay_from_earlier_reproduces_the_picture_on_screen() {
    let Some(m) = booted() else { return };
    let anchor = m.clone();
    let mut live = m.clone();
    // Freeze somewhere mid-frame, a few frames on.
    live.run_for_full(3 * CYCLES_PER_FRAME + 7_777, &mut NullSink, |_, _, _, _, _, _, _| {});
    let start = vic_line_trace::displayed_frame_start(&live);
    assert!(start > anchor.c64_core.clk);
    let mut s = anchor.clone();
    let f = vic_line_trace::record_frame(&mut s, start, FrameWhich::Displayed, Some(&live.vic.displayed[..]))
        .expect("record");
    assert_eq!(f.verified, Some(true), "the replay must draw the picture the frozen machine shows");
    // And a picture from a different frame does not verify.
    let mut t = anchor.clone();
    let wrong = vic_line_trace::record_frame(&mut t, start - CYCLES_PER_FRAME, FrameWhich::Displayed, Some(&live.vic.displayed[..]));
    // At READY the cursor blinks, so the frame before may or may not equal; only the
    // right one is required to verify. The call itself must succeed.
    assert!(wrong.is_ok());
}

// ── Spec 860 — the frame map ─────────────────────────────────────────────────────────

/// A raster split in a loop, with interrupts off: at line $80 the charset flips to $1800,
/// the border to red and sprite 0 to Y=160; at the top of the frame all three flip back.
/// "HELLO" sits on screen row 15, below the split.
fn split_machine() -> Option<Machine> {
    let mut m = booted()?;
    #[rustfmt::skip]
    let prog: [u8; 53] = [
        0x78,                   // SEI
        0xA9, 0x7F, 0x8D, 0x0D, 0xDC,   // LDA #$7F / STA $DC0D
        0xA9, 0x80, 0xCD, 0x12, 0xD0, 0xD0, 0xFB, // wait $D012 == $80
        0xA9, 0x17, 0x8D, 0x18, 0xD0,   // charset $1800
        0xA9, 0x02, 0x8D, 0x20, 0xD0,   // border red
        0xA9, 0xA0, 0x8D, 0x01, 0xD0,   // sprite 0 Y = 160
        0xA9, 0x10, 0xCD, 0x12, 0xD0, 0xD0, 0xFB, // wait $D012 == $10
        0xA9, 0x15, 0x8D, 0x18, 0xD0,   // charset $1000
        0xA9, 0x0E, 0x8D, 0x20, 0xD0,   // border light blue
        0xA9, 0x3C, 0x8D, 0x01, 0xD0,   // sprite 0 Y = 60
        0x4C, 0x06, 0xC0,               // JMP $C006
    ];
    m.poke(0xc000, &prog);
    m.poke(0x0340, &[0xff; 63]);
    m.poke(0x07f8, &[0x0d]);
    m.poke(0x0400 + 15 * 40 + 5, &[8, 5, 12, 12, 15]);
    m.poke_io(0xd000, &[0x80, 0x3c]);
    m.poke_io(0xd015, &[0x01]);
    m.c64_core.reg_pc = 0xc000;
    m.run_for_full(3 * CYCLES_PER_FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
    Some(m)
}

fn frame_map(m: &Machine) -> serde_json::Value {
    let mut s = m.clone();
    let f = vic_line_trace::record_frame(&mut s, next_frame_start(m), FrameWhich::Next, None).expect("record");
    f.frame_map_json(true)
}

#[test]
fn the_frame_map_names_the_objects_on_a_split_screen() {
    let Some(m) = split_machine() else { return };
    let fm = frame_map(&m);
    let objs = fm["objects"].as_array().unwrap();
    let display: Vec<_> = objs.iter().filter(|o| o["kind"] == "display").collect();
    // Above the split: the KERNAL banner with the ROM charset at $1000.
    assert!(
        display.iter().any(|o| o["dataBase"] == 0x1000 && o["rom"] == true && o["y"].as_i64().unwrap() < 128 - 16),
        "an object above the split in the ROM charset $1000: {display:#?}"
    );
    // Below it: HELLO, drawn from $1800 — a different object because the source differs.
    let hello = display
        .iter()
        .find(|o| o["dataBase"] == 0x1800)
        .unwrap_or_else(|| panic!("an object from the charset at $1800: {display:#?}"));
    assert_eq!((hello["cols"].as_i64(), hello["cells"].as_i64()), (Some(5), Some(5)), "HELLO is five cells");
    assert_eq!(hello["ranges"]["screen"][0], serde_json::json!({ "addr": 0x0400 + 15 * 40 + 5, "length": 5 }));
    // Every row of the text is one display row of 8 lines.
    assert_eq!(hello["h"], 8);
}

#[test]
fn a_multiplexed_sprite_is_two_frames() {
    let Some(m) = split_machine() else { return };
    let fm = frame_map(&m);
    let s0: Vec<_> = fm["objects"].as_array().unwrap().iter().filter(|o| o["kind"] == "sprite" && o["sprite"] == 0).cloned().collect();
    assert_eq!(s0.len(), 2, "sprite 0 appears twice: {s0:#?}");
    let first = s0[0]["lines"][0].as_i64().unwrap();
    let second = s0[1]["lines"][0].as_i64().unwrap();
    assert!((60..=62).contains(&first) && (160..=162).contains(&second), "starts at {first} and {second}");
    for s in &s0 {
        assert_eq!(s["h"], 21, "21 lines each");
        assert_eq!(s["ranges"]["sprite"][0]["addr"], 0x0340);
    }
}

#[test]
fn a_border_write_mid_line_is_marked_where_it_lands() {
    let Some(m) = split_machine() else { return };
    let fm = frame_map(&m);
    let w = fm["writes"].as_array().unwrap();
    let d020: Vec<_> = w.iter().filter(|x| x["reg"] == 0x20 && x["line"] == 128).collect();
    assert_eq!(d020.len(), 1, "one $D020 store on line 128: {w:#?}");
    assert_eq!(d020[0]["value"], 2);
    assert_eq!(d020[0]["midLine"], true, "it lands inside the visible line: {:?}", d020[0]);
    // The cell it lands in carries the VIC-write bit.
    let cyc = d020[0]["cycle"].as_u64().unwrap() as usize;
    let bits = fm["cells"][128][cyc - 1].as_u64().unwrap() as u16;
    assert!(bits & vic_line_trace::cell::VIC_WRITE != 0);
    // A bad line's cells: 43 with BA.
    let l51 = &fm["cells"][51].as_array().unwrap();
    let ba = l51.iter().filter(|b| b.as_u64().unwrap() as u16 & vic_line_trace::cell::BA != 0).count();
    assert_eq!(ba, 43);
}

fn rules(fm: &serde_json::Value) -> Vec<String> {
    fm["techniques"].as_array().unwrap().iter().map(|t| t["rule"].as_str().unwrap().to_string()).collect()
}

#[test]
fn a_plain_screen_shows_no_technique() {
    let Some(m) = booted() else { return };
    let fm = frame_map(&m);
    assert!(rules(&fm).is_empty(), "READY uses no trick: {:#?}", fm["techniques"]);
}

#[test]
fn the_split_screen_names_its_split_its_multiplexer_and_its_mid_line_store() {
    let Some(m) = split_machine() else { return };
    let fm = frame_map(&m);
    let t = fm["techniques"].as_array().unwrap();
    let split = t.iter().find(|x| x["rule"] == "split").unwrap_or_else(|| panic!("a split: {t:#?}"));
    assert!(split["detail"].as_str().unwrap().contains("$D018 $15→$17"), "{split}");
    let at = split["lines"][0].as_u64().unwrap();
    assert!((128..=129).contains(&at), "the split is at line 128/129, not {at}");
    assert!(t.iter().any(|x| x["rule"] == "multiplexer"), "{t:#?}");
    assert!(t.iter().any(|x| x["rule"] == "mid_line" && x["detail"].as_str().unwrap().contains("$D020")), "{t:#?}");
    for r in ["fli", "fld", "linecrunch", "dma_delay", "side_border", "tb_border", "sprite_height"] {
        assert!(!rules(&fm).iter().any(|x| x == r), "{r} must not fire here: {t:#?}");
    }
}

/// FLD: at line $60 YSCROLL goes to 7, so line 99 — due to be a bad line — is not, and the
/// display idles until line 103. At the top of the frame YSCROLL goes back to 3.
#[test]
fn a_pushed_bad_line_is_fld() {
    let Some(mut m) = booted() else { return };
    #[rustfmt::skip]
    let prog: [u8; 29] = [
        0x78,                                     // SEI
        0xA9, 0x60, 0xCD, 0x12, 0xD0, 0xD0, 0xFB, // wait $D012 == $60
        0xA9, 0x1F, 0x8D, 0x11, 0xD0,             // $D011 = $1F (YSCROLL 7)
        0xA9, 0x10, 0xCD, 0x12, 0xD0, 0xD0, 0xFB, // wait $D012 == $10
        0xA9, 0x1B, 0x8D, 0x11, 0xD0,             // $D011 = $1B (YSCROLL 3)
        0x4C, 0x01, 0xC0,                         // JMP $C001
        0xEA,
    ];
    m.poke(0xc000, &prog);
    m.c64_core.reg_pc = 0xc000;
    m.run_for_full(3 * CYCLES_PER_FRAME, &mut NullSink, |_, _, _, _, _, _, _| {});
    let fm = frame_map(&m);
    let t = fm["techniques"].as_array().unwrap();
    let fld = t.iter().find(|x| x["rule"] == "fld").unwrap_or_else(|| panic!("FLD: {t:#?}"));
    assert_eq!(fld["lines"], serde_json::json!([99, 102]), "{fld}");
    assert!(!rules(&fm).iter().any(|x| x == "linecrunch" || x == "fli"), "{t:#?}");
}
