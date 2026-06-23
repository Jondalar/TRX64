//! GCR read-cadence gate: drives the standalone 1541 controller through a real
//! sector read (track 18 sector 0, the directory BAM) and asserts that SYNC is
//! detected and the read job reaches JOB STATUS $01 (CBMDOS_FDC_ERR_OK).
//!
//! See drive_sector_read.rs for the root-cause writeup (the spin-up `attach_clk`
//! window vs the PRB-only `$F562` find-sync loop) and the disk-ID prime.

use std::path::Path;
use trx64_core::drive::{DiskImage, DiskKind, Drive1541};

const ROM_DIR: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
const SAMPLE: &str =
    "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/scramble_infinity.d64";

fn present() -> bool {
    Path::new(ROM_DIR).join("dos1541-325302-01+901229-05.bin").exists()
        || Path::new(ROM_DIR).join("1541.bin").exists()
}

/// D64 linear byte offset of track 18, sector 0.
fn d64_t18s0_off() -> usize {
    let spt: Vec<usize> = std::iter::repeat(21usize)
        .take(17)
        .chain(std::iter::repeat(19).take(7))
        .chain(std::iter::repeat(18).take(6))
        .chain(std::iter::repeat(17).take(5))
        .collect();
    let mut off = 0;
    for t in 1..18 {
        off += spt[t - 1] * 256;
    }
    off
}

#[test]
fn gcr_sync_cadence_probe() {
    if !present() {
        eprintln!("skip: ROM absent");
        return;
    }
    let d64 = match std::fs::read(SAMPLE) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("skip: sample absent");
            return;
        }
    };
    let off = d64_t18s0_off();
    let (id1, id2) = (d64[off + 0xA2], d64[off + 0xA3]);

    let mut drive = Drive1541::new();
    drive.load_rom(Path::new(ROM_DIR)).unwrap();
    drive.cold_reset();
    drive.run_cycles(1_200_000);
    drive.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: d64,
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    drive.run_cycles(2_000_000);

    // Prime the cached disk ID (post-Initialize state), then request T18S0 read.
    drive.drive_ram_write(0x0012, id1);
    drive.drive_ram_write(0x0013, id2);
    drive.drive_ram_write(0x0006, 18);
    drive.drive_ram_write(0x0007, 0);
    drive.drive_ram_write(0x0000, 0x80);

    // Run in small chunks and sample the rotation state.
    let mut sync_seen = 0u64;
    let mut max_lrd_ones = 0u32;
    let mut status = 0x80u8;
    for _ in 0..2000 {
        drive.run_cycles(2_000);
        let r = &drive.rotation;
        let lrd = r.last_read_data;
        if lrd == 0x3ff {
            sync_seen += 1;
        }
        let ones = (lrd & 0x3ff).count_ones();
        if ones > max_lrd_ones {
            max_lrd_ones = ones;
        }
        status = drive.drive_ram_read(0x0000);
        if status < 0x80 {
            break;
        }
    }
    eprintln!(
        "status={:#04X} sync_seen={} max_lrd_ones={} head_off={} track_size={}",
        status,
        sync_seen,
        max_lrd_ones,
        drive.rotation.gcr_head_offset,
        drive.rotation.gcr_current_track_size
    );
    // SYNC must be physically detectable (10 consecutive 1-bits) and the job
    // must complete OK.
    assert_eq!(max_lrd_ones, 10, "a full 10-bit SYNC must be seen in the GCR stream");
    assert_eq!(status, 0x01, "read job must reach status $01 (OK), got {status:#04x}");
}
