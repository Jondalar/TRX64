//! GCR read-cadence probe: drives the standalone 1541 controller through a real
//! sector read (track 18 sector 0, the directory BAM) and instruments the
//! rotation engine to see whether SYNC is ever detected at the controller's
//! sampling cadence and whether GCR bytes assemble sequentially.

use std::path::Path;
use trx64_core::drive::{DiskImage, DiskKind, Drive1541};

const ROM_DIR: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
const SAMPLE: &str =
    "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/scramble_infinity.d64";

fn present() -> bool {
    Path::new(ROM_DIR).join("dos1541-325302-01+901229-05.bin").exists()
        || Path::new(ROM_DIR).join("1541.bin").exists()
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

    // Request T18S0 read.
    drive.drive_ram_write(0x0006, 18);
    drive.drive_ram_write(0x0007, 0);
    drive.drive_ram_write(0x0000, 0x80);

    // Run in small chunks and sample the rotation state.
    let mut sync_seen = 0u64;
    let mut max_lrd_ones = 0u32;
    let mut byte_edges = 0u64;
    let mut prev_off = drive.rotation.gcr_head_offset;
    let mut max_step = 0u32;
    let mut status = 0x80u8;
    for _ in 0..2000 {
        drive.run_cycles(2_000);
        let r = &drive.rotation;
        // count one-bits run in last_read_data (sync = many consecutive 1s)
        let lrd = r.last_read_data;
        if lrd == 0x3ff {
            sync_seen += 1;
        }
        let ones = (lrd & 0x3ff).count_ones();
        if ones > max_lrd_ones {
            max_lrd_ones = ones;
        }
        if r.byte_ready_edge != 0 {
            byte_edges += 1;
        }
        let off = r.gcr_head_offset;
        let step = off.wrapping_sub(prev_off);
        if step < 100000 && step > max_step {
            max_step = step;
        }
        prev_off = off;
        status = drive.drive_ram_read(0x0000);
        if status < 0x80 {
            break;
        }
    }
    eprintln!(
        "status={:#04X} sync_seen={} max_lrd_ones={} byte_edges={} max_head_step={} head_off={} track_size={}",
        status,
        sync_seen,
        max_lrd_ones,
        byte_edges,
        max_step,
        drive.rotation.gcr_head_offset,
        drive.rotation.gcr_current_track_size
    );
}
