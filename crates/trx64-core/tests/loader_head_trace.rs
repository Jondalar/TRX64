//! Spec 784 A1b — the armed 1541 head trace fires during a REAL drive read.
//!
//! The unit tests in trx64-trace cover the DRIVE_HEAD encoder + the arm/drain/emit
//! gating. This integration test proves the missing link: that a full-machine disk
//! LOAD actually POPULATES the head trace with plausible (halftrack, sector) samples
//! — i.e. Machine::arm_head_trace + the on_drive_step head sample in
//! run_for_full_capped_dbg see the head moving over real GCR sectors.
//!
//! Boots the full C64, mounts a real .g64, injects LOAD"*",8,1 (the KERNAL directory
//! read engages the GCR path — enough for the head to sweep track 18), arms the head
//! trace, and asserts samples accumulate at the directory track. Skips gracefully if
//! the ROMs / sample disk are absent (CI has neither).

use std::path::Path;
use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
const SAMPLE: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/motm.g64";

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

#[test]
#[ignore = "behavioral: needs local ROMs + sample disk + ~100s full-machine load; run with --ignored"]
fn armed_head_trace_populates_during_real_load() {
    if !roms_present() {
        eprintln!("skip: ROMs absent");
        return;
    }
    let g64 = match std::fs::read(SAMPLE) {
        Ok(b) => b,
        Err(_) => { eprintln!("skip: G64 sample absent ({SAMPLE})"); return; }
    };

    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut sink = NullSink;

    // To BASIC ready, then mount + settle.
    m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::G64,
        bytes: g64,
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    assert!(m.drive8.rotation.image.is_some(), "G64 GCR image attached");
    assert_eq!(m.drive8.rotation.current_half_track, 36, "head parked at T18 on attach");
    m.run_for_full(500_000, &mut sink, |_, _, _, _, _, _, _| {});

    // Arm the loader-lens head trace, then LOAD (the directory read sweeps T18).
    m.arm_head_trace(true);
    inject_keys(&mut m, b"LOAD\"*\",8,1\r");
    let budget = 12_000_000u64;
    let chunk = 200_000u64;
    let mut total = 0u64;
    while total < budget {
        m.run_for_full(chunk, &mut sink, |_, _, _, _, _, _, _| {});
        total += chunk;
    }

    let samples = m.drain_head_trace();
    eprintln!("head-trace samples during LOAD: {}", samples.len());
    assert!(!samples.is_empty(), "the armed head trace must accumulate samples during a real drive read");

    // Every sample is a plausible 1541 head position.
    for (clk, ht, sec) in &samples {
        assert!((2..=84).contains(ht), "halftrack {ht} in 1541 range (clk {clk})");
        assert!(*sec == 0xff || *sec <= 20, "sector {sec} valid (0..20) or 0xff gap");
    }
    // The directory read sweeps track 18 (half-track 36) — at least one sample there,
    // with at least one REAL (non-gap) sector under the head.
    let at_t18 = samples.iter().filter(|(_, ht, _)| *ht == 36).count();
    let real_sectors = samples.iter().filter(|(_, ht, sec)| *ht == 36 && *sec != 0xff).count();
    eprintln!("samples at T18 (ht36): {at_t18}, with a real sector: {real_sectors}");
    assert!(at_t18 > 0, "the head trace must record the head over the directory track T18");
    assert!(real_sectors > 0, "the head trace must see real sectors under the head at T18");

    // Spec 784 Option A — the READ-SET lane must also populate: the drive latched GCR
    // bytes off real sectors during the directory read. This is the truth
    // validate_extraction diffs against, and the fix for the write-time buffering lie.
    let reads = m.drain_block_reads();
    eprintln!("block-read (read-set) records during LOAD: {}", reads.len());
    assert!(!reads.is_empty(), "the read-set lane must record blocks the drive physically read");
    for (clk, ht, sec, bytes) in &reads {
        assert!((2..=84).contains(ht), "read-set halftrack {ht} in range (clk {clk})");
        assert!(*sec <= 20, "read-set sector {sec} valid (0..20) — a gap (0xff) is never a read source");
        assert!(*bytes > 0, "a read-set record always attributes >0 GCR bytes");
    }
    // The directory lives on T18 — at least one block read there.
    let read_t18 = reads.iter().filter(|(_, ht, _, _)| *ht == 36).count();
    eprintln!("read-set records at T18 (ht36): {read_t18}");
    assert!(read_t18 > 0, "the read-set must record a block physically read at the directory track T18");

    // A second run WITHOUT arming must NOT accumulate (armed-on-command contract).
    m.arm_head_trace(false);
    inject_keys(&mut m, b"\r");
    m.run_for_full(1_000_000, &mut sink, |_, _, _, _, _, _, _| {});
    assert!(m.drain_head_trace().is_empty(), "disarmed → no head samples accumulate");
    assert!(m.drain_block_reads().is_empty(), "disarmed → no read-set records accumulate");
}
