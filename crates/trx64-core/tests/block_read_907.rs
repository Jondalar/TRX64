//! Spec 907 — the 1541 read-set (BLOCK_READ2 source) tells the truth.
//!
//! A tiny drive program, poked into drive RAM on a booted 1541 with a standard-GCR
//! G64 mounted, waits for the header of ONE chosen sector, reads that header block
//! (10 raw GCR bytes), waits for its data sync and reads the data block (325 raw GCR
//! bytes) — the way a fastloader's read loop does (`BVC *` / `CLV` / `LDA $1C01`).
//! The machine runs with the head trace armed, exactly as the daemon arms it, and the
//! buckets it produces are checked against what the drive program is KNOWN to have
//! read:
//!
//!   S3a  the bytes are credited to the sector whose block the head was IN (never to
//!        the next one), header bytes and data bytes apart;
//!   S3b  the same read split across several `run_for_full` calls, re-armed before
//!        each, is still one bucket with the same totals;
//!   S3c  stopping mid-block (flush + drain) reports the bytes read so far;
//!   S4   after a drive cold reset (drive clock restarts at 0) every record's C64
//!        clock lies inside the C64-clock window of the run it was read in.
//!
//! Needs the C64 + 1541 ROMs (skips when absent, like the other full-machine tests).

use std::path::Path;

use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::gcr::GcrImage;
use trx64_core::{BlockReadRecord, Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const CODE: u16 = 0x0300;
const MARK: u16 = 0x0500;

fn roms_present() -> bool {
    let d = Path::new(ROM_DIR);
    d.join("kernal-901227-03.bin").exists()
        && (d.join("dos1541-325302-01+901229-05.bin").exists() || d.join("1541.bin").exists())
}

fn run(m: &mut Machine, cycles: u64) {
    m.run_for_full(cycles, &mut NullSink, |_, _, _, _, _, _, _| {});
}

/// A 35-track D64 whose sector bytes differ per (track, sector); the GCR image
/// built from it carries standard headers (0x08) and data blocks (0x07).
fn d64() -> Vec<u8> {
    let mut d = vec![0u8; 174_848];
    for (i, b) in d.iter_mut().enumerate() {
        *b = (i / 256) as u8 ^ (i as u8);
    }
    d
}

/// The same disk as a G64 file (84 half-tracks, standard speed zones), so it runs on
/// the same rotation engine as a real G64 title.
fn g64_from_gcr(img: &GcrImage) -> Vec<u8> {
    const N: usize = 84;
    let mut out = Vec::new();
    out.extend_from_slice(b"GCR-1541\0");
    out.push(N as u8);
    out.extend_from_slice(&7928u16.to_le_bytes());
    let mut body = Vec::new();
    let base = 12 + N * 8;
    let mut offs = Vec::new();
    for t in &img.tracks[..N] {
        if t.size == 0 {
            offs.push(0u32);
        } else {
            offs.push((base + body.len()) as u32);
            body.extend_from_slice(&(t.size as u16).to_le_bytes());
            body.extend_from_slice(&t.data[..t.size]);
        }
    }
    for o in &offs {
        out.extend_from_slice(&o.to_le_bytes());
    }
    for i in 0..N {
        let tr = (i / 2) as u32;
        let zone = (tr < 31) as u32 + (tr < 25) as u32 + (tr < 18) as u32;
        out.extend_from_slice(&zone.to_le_bytes());
    }
    out.extend_from_slice(&body);
    out
}

const GCR: [u8; 16] = [
    0x0a, 0x0b, 0x12, 0x13, 0x0e, 0x0f, 0x16, 0x17, 0x09, 0x19, 0x1a, 0x1b, 0x0d, 0x1d, 0x1e, 0x15,
];

/// The drive program: wait for the header of `sector`, read the 10 header GCR
/// bytes, wait for the data sync, read the 325 data GCR bytes, write 2 to `MARK`.
/// `MARK` = 1 while the data block is being waited for / read.
fn program(sector: u8) -> Vec<u8> {
    // Header bytes (decoded) 08 chk sec trk: sector = 5-bit groups 4 and 5 = bits
    // 20..29 of the first 5 GCR bytes -> low nibble of g2 and top 6 bits of g3.
    let v = ((GCR[(sector >> 4) as usize] as u16) << 5) | GCR[(sector & 15) as usize] as u16;
    let e2 = (v >> 6) as u8;
    let e3 = ((v & 0x3f) << 2) as u8;
    let mut p = vec![
        0x78, // 0300 SEI
        0xAD, 0x00, 0x1C, // LDA $1C00
        0x29, 0x03, // AND #$03   keep the stepper phase
        0x09, 0x4C, // ORA #$4C   motor, LED, density zone 2
        0x8D, 0x00, 0x1C, // STA $1C00
        0xA9, 0xEE, 0x8D, 0x0C, 0x1C, // PCR: CA2 high = byte-ready (SO) enabled, read mode
        0xAD, 0x01, 0x05, 0xF0, 0xFB, // wait until the test says the head has settled
        // 030B find:
        0x2C, 0x00, 0x1C, // BIT $1C00
        0x30, 0xFB, // BMI find
        0xB8, // CLV
        0x50, 0xFE, // 0311 BVC *
        0xB8, // CLV
        0xAD, 0x01, 0x1C, // g0
        0xC9, 0x52, // CMP #$52 (header block id)
        0xD0, 0xF0, // BNE find
        0x50, 0xFE, 0xB8, 0xAD, 0x01, 0x1C, // g1
        0x50, 0xFE, 0xB8, 0xAD, 0x01, 0x1C, // g2
        0x29, 0x0F, 0xC9, e2, 0xD0, 0xDE, // mask/compare, BNE find
        0x50, 0xFE, 0xB8, 0xAD, 0x01, 0x1C, // g3
        0x29, 0xFC, 0xC9, e3, 0xD0, 0xD2, // mask/compare, BNE find
        0x50, 0xFE, 0xB8, 0xAD, 0x01, 0x1C, // g4
        0xA2, 0x05, // LDX #5
        0x50, 0xFE, 0xB8, 0xAD, 0x01, 0x1C, 0xCA, 0xD0, 0xF7, // g5..g9
        0xA9, 0x01, 0x8D, 0x00, 0x05, // MARK = 1
        0x2C, 0x00, 0x1C, 0x30, 0xFB, // wait data sync
        0xB8, 0x50, 0xFE, 0xB8, 0xAD, 0x01, 0x1C, // first data byte
        0x8D, 0x00, 0x06, // STA $0600
        0xA0, 0x01, // LDY #1
        0x50, 0xFE, 0xB8, 0xAD, 0x01, 0x1C, 0x99, 0x00, 0x06, 0xC8, 0xD0, 0xF4, // 255 more
        0xA2, 0x45, // LDX #69
        0x50, 0xFE, 0xB8, 0xAD, 0x01, 0x1C, 0x9D, 0xFF, 0x06, 0xCA, 0xD0, 0xF4, // 69 more
        0xA9, 0x02, 0x8D, 0x00, 0x05, // MARK = 2
    ];
    let end = CODE + p.len() as u16;
    p.extend_from_slice(&[0x4C, end as u8, (end >> 8) as u8]); // JMP *
    p
}

/// Boot, mount, let the disk spin up, install the program and point the drive CPU at
/// it with IRQs masked (the DOS controller must not touch the mechanism).
fn machine_with_program(sector: u8, cold_reset_drive_first: bool) -> (Machine, u8) {
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    run(&mut m, 1_500_000);
    if cold_reset_drive_first {
        m.drive8.cold_reset();
        run(&mut m, 1_500_000);
    }
    let g64 = g64_from_gcr(&GcrImage::from_d64(&d64()));
    m.drive8.attach_disk(DiskImage { kind: DiskKind::G64, bytes: g64, backing_path: None, read_only: false });
    assert!(m.drive8.rotation.image.is_some());
    run(&mut m, 2_200_000); // DRIVE_ATTACH_DELAY
    for (i, b) in program(sector).iter().enumerate() {
        m.drive8.drive_ram_write(CODE + i as u16, *b);
    }
    m.drive8.drive_ram_write(MARK, 0);
    m.drive8.drive_ram_write(0x0501, 0);
    m.drive8.core.reg_pc = CODE;
    m.drive8.core.reg_p |= 0x04;
    // The program turns the motor on and then waits for the go flag.
    run(&mut m, 40_000);
    // The program's phase write landed the head on a half-track of its own choosing;
    // park it on track 18, where the disk's blocks are, and only then let it read.
    m.drive8.rotation.set_half_track(36);
    m.drive8.drive_ram_write(0x0501, 1);
    run(&mut m, 10);
    let ht = m.drive8.rotation.current_half_track as u8;
    assert!(ht.is_multiple_of(2) && m.drive8.rotation.image.as_ref().unwrap().tracks[ht as usize - 2].size > 0, "head on a full track with data (ht {ht})");
    assert_eq!(mark(&m), 0, "the program has not reached the data block yet");
    (m, ht)
}

fn mark(m: &Machine) -> u8 {
    m.drive8.drive_ram_read(MARK)
}

/// Run in small armed chunks (the daemon re-arms at every `session/run`) until
/// `stop(m)`; returns when it holds or after a generous budget.
fn run_until(m: &mut Machine, stop: impl Fn(&Machine) -> bool) {
    for _ in 0..4000 {
        m.arm_head_trace(true);
        run(m, 2_000);
        if stop(m) {
            return;
        }
    }
    panic!("drive program never reached the awaited state (mark {})", mark(m));
}

fn assert_conserved(m: &Machine, base: u64, recs: &[BlockReadRecord]) {
    let total: u64 = recs.iter().map(|r| r.header_bytes as u64 + r.data_bytes as u64).sum();
    assert_eq!(total, m.drive8.rotation.gcr_read_count - base, "every latched byte lands in exactly one bucket");
}

#[test]
fn s3a_bytes_are_credited_to_the_sector_the_head_is_in() {
    if !roms_present() {
        eprintln!("skip: ROMs absent");
        return;
    }
    let target = 7u8;
    let (mut m, ht) = machine_with_program(target, false);
    let base = m.drive8.rotation.gcr_read_count;
    run_until(&mut m, |m| mark(m) == 2);
    m.flush_block_read();
    let recs = m.drain_block_reads();
    let mine: Vec<_> = recs.iter().filter(|r| r.sector == target).collect();
    assert_eq!(mine.len(), 1, "one bucket for sector {target}: {recs:?}");
    assert_eq!((mine[0].half_track, mine[0].header_bytes, mine[0].data_bytes), (ht, 10, 325), "{recs:?}");
    // The search read 5 header bytes of each header it skipped (and one byte of a
    // data block it ran into): header-only buckets, never data for a neighbour.
    let next = target + 1;
    for r in recs.iter().filter(|r| r.sector != target) {
        assert!(r.data_bytes <= 1 && r.header_bytes <= 5, "searching reads stay small and on their own sector: {r:?}");
    }
    if let Some(r) = recs.iter().find(|r| r.sector == next) {
        assert_eq!(r.data_bytes, 0, "sector {next} must not be credited with the data of {target}: {r:?}");
    }
    assert_conserved(&m, base, &recs);
    assert_eq!(m.drive8.drive_ram_read(0x0600), 0x55, "the data block id (GCR 0x07) was the first byte read after its sync");
}

#[test]
fn s3b_a_read_split_over_several_runs_is_one_bucket() {
    if !roms_present() {
        eprintln!("skip: ROMs absent");
        return;
    }
    let target = 11u8;
    let (mut m, _ht) = machine_with_program(target, false);
    let base = m.drive8.rotation.gcr_read_count;
    // Chunks of 2000 cycles, re-armed before each: the data read (325 bytes at
    // ~28 cycles) is spread over ~5 runs.
    run_until(&mut m, |m| mark(m) == 2);
    m.flush_block_read();
    let recs = m.drain_block_reads();
    let mine: Vec<_> = recs.iter().filter(|r| r.sector == target).collect();
    assert_eq!(mine.len(), 1, "one record across the runs: {recs:?}");
    assert_eq!((mine[0].header_bytes, mine[0].data_bytes), (10, 325), "{recs:?}");
    assert_conserved(&m, base, &recs);
}

#[test]
fn s3c_stopping_mid_block_reports_the_bytes_so_far() {
    if !roms_present() {
        eprintln!("skip: ROMs absent");
        return;
    }
    let target = 3u8;
    let (mut m, _ht) = machine_with_program(target, false);
    let base = m.drive8.rotation.gcr_read_count;
    run_until(&mut m, |m| mark(m) == 1);
    let at_mark = m.drive8.rotation.gcr_read_count;
    run_until(&mut m, |m| m.drive8.rotation.gcr_read_count - at_mark >= 100);
    assert_eq!(mark(&m), 1, "still inside the data block");
    m.flush_block_read();
    let recs = m.drain_block_reads();
    let mine: Vec<_> = recs.iter().filter(|r| r.sector == target).collect();
    assert_eq!(mine.len(), 1, "{recs:?}");
    assert_eq!(mine[0].header_bytes, 10, "{recs:?}");
    assert!(
        mine[0].data_bytes >= 100 && mine[0].data_bytes < 325,
        "the data bytes read so far, not zero and not all: {recs:?}"
    );
    assert_conserved(&m, base, &recs);
}

#[test]
fn s4_c64_clock_is_comparable_after_a_drive_cold_reset() {
    if !roms_present() {
        eprintln!("skip: ROMs absent");
        return;
    }
    let target = 5u8;
    let (mut m, _ht) = machine_with_program(target, true);
    let t0 = m.clk;
    let mut windows: Vec<(u64, u64)> = Vec::new();
    for _ in 0..4000 {
        m.arm_head_trace(true);
        let a = m.clk;
        run(&mut m, 2_000);
        windows.push((a, m.clk));
        if mark(&m) == 2 {
            break;
        }
    }
    assert_eq!(mark(&m), 2);
    m.flush_block_read();
    let t1 = m.clk;
    let recs = m.drain_block_reads();
    assert!(!recs.is_empty());
    let mut last = 0;
    for r in &recs {
        assert!(r.c64_clk >= t0 && r.c64_clk <= t1, "c64_clk {} outside the run [{t0}, {t1}]: {r:?}", r.c64_clk);
        assert!(windows.iter().any(|(a, b)| r.c64_clk >= *a && r.c64_clk <= *b), "inside one run window: {r:?}");
        assert!(r.c64_clk >= last, "monotonic with the machine clock");
        last = r.c64_clk;
        assert!(r.drv_clk < t0, "the drive clock restarted at its own reset ({} vs {t0}): {r:?}", r.drv_clk);
    }
}
