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
