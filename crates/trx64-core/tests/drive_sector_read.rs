//! Milestone 2 (COMPLETE): byte-exact GCR sector read through the live DOS controller.
//!
//! Boots the 1541 drive standalone (DOS ROM), mounts a D64, then pokes the DOS
//! job queue directly — $00 = $80 (READ buffer 0), $06/$07 = track/sector — to
//! request a sector read WITHOUT the full IEC command handshake (the standard
//! 1541 job-queue trick; the IRQ-driven controller picks up the job regardless
//! of IEC activity).
//!
//! WHAT THIS GATES (green): the FULL read engine. The DOS controller picks up the
//! read job, spins the motor, finds the data SYNC, reads the GCR header + data,
//! GCR-decodes them, and completes with JOB STATUS $01 (CBMDOS_FDC_ERR_OK). The
//! decoded sector buffer at $0300 is byte-identical to the D64 image. Exercises
//! the whole path: D64→GCR encode, head rotation, VIA2 PRA (GCR_read) / PRB7
//! (SYNC) reads, the stepper/motor/speed-zone store_prb side-effects, and the
//! byte-ready (SO) → drive-CPU V-flag handshake.
//!
//! ROOT-CAUSE FIX (ADR drive-read-engine):
//!   The previous $03 (CBMDOS_FDC_ERR_SYNC) failure was NOT a GCR-data or cadence
//!   bug. `rotation_sync_found` returns 0x80 (no-sync) while `attach_clk != 0`
//!   (the spin-up settle window). VICE clears `attach_clk` ONLY in
//!   `rotation_byte_read` (the PRA / $1C01 read) once `DRIVE_ATTACH_DELAY`
//!   (1.8M cycles) elapses — it gets away with this because a real drive ALWAYS
//!   issues a PRA read during head/job setup before the $F556 find-sync loop. The
//!   1541 DOS find-sync loop ($F562 `BIT $1C00` / `BMI`) polls PB7/SYNC via PRB
//!   ONLY, so a drive that has never read $1C01 keeps `attach_clk` set forever and
//!   never sees SYNC → spins out the $1805 watchdog → $03. The fix (rotation.rs
//!   `rotate_disk`) drops the spin-up window once the delay has expired on ANY
//!   rotation access, matching the physical reality (the disk is up to speed
//!   regardless of which VIA register is sampled). Within the delay nothing
//!   changes, so the byte-exact mount/idle drive-cpu traces are unaffected.
//!
//! DISK-ID PRIME: the job-queue trick bypasses the DOS `Initialize` command that
//! normally caches the disk ID at $12/$13 from the BAM. Without it the header
//! verify ($F3F9 `CMP $16`) compares the freshly-read header ID against an
//! uninitialised $12/$13 → $0B (CBMDOS_FDC_ERR_ID). We prime $12/$13 with the
//! disk ID (the post-Initialize state) so the verify exercises a REAL match.

use trx64_core::drive::{DiskImage, DiskKind, Drive1541};
use std::path::Path;

const ROM_DIR: &str =
    "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
const SAMPLE: &str =
    "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/scramble_infinity.d64";

/// D64 linear byte offset of track 18, sector 0 (zones 21/19/18/17 sectors/track).
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
fn drive_reads_t18s0_byte_exact_status_ok() {
    let rom_dir = Path::new(ROM_DIR);
    if !rom_dir.join("dos1541-325302-01+901229-05.bin").exists()
        && !rom_dir.join("1541.bin").exists()
    {
        eprintln!("skip: DOS ROM absent");
        return;
    }
    let d64 = match std::fs::read(SAMPLE) {
        Ok(b) => b,
        Err(_) => { eprintln!("skip: sample disk absent"); return; }
    };
    let off = d64_t18s0_off();
    let id1 = d64[off + 0xA2];
    let id2 = d64[off + 0xA3];

    let mut drive = Drive1541::new();
    drive.load_rom(rom_dir).expect("load DOS ROM");
    drive.cold_reset();

    // Boot the drive to its idle loop (DOS init + IRQ controller).
    drive.run_cycles(1_200_000);

    // Mount the disk AFTER boot (cold_reset clears any disk). This triggers the
    // D64→GCR encode and parks the head at track 18.
    drive.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: d64.clone(),
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    assert!(
        drive.rotation.image.is_some(),
        "mounting a D64 must populate the rotating GCR image"
    );
    // Let the attach spin-up window pass (DRIVE_ATTACH_DELAY = 1.8M cycles).
    drive.run_cycles(2_000_000);

    // Prime the cached disk ID at $12/$13 (the post-Initialize state) so the
    // header verify compares against a real match instead of a stale $0B.
    drive.drive_ram_write(0x0012, id1);
    drive.drive_ram_write(0x0013, id2);

    // Track 18, sector 0 (the BAM) — always present.
    drive.drive_ram_write(0x0006, 18);
    drive.drive_ram_write(0x0007, 0);
    drive.drive_ram_write(0x0000, 0x80); // READ job

    let head_before = drive.rotation.gcr_head_offset;

    // Run the controller until the job completes (status replaces the job code).
    let mut status = 0x80u8;
    for _ in 0..200 {
        drive.run_cycles(100_000);
        status = drive.drive_ram_read(0x0000);
        if status < 0x80 {
            break;
        }
    }

    // The read PATH ran: motor on, head advanced over the GCR bitstream.
    assert_ne!(
        drive.rotation.byte_ready_active & trx64_core::rotation::BRA_MOTOR_ON,
        0,
        "the DOS must have spun the motor up for the read"
    );
    assert!(
        drive.rotation.gcr_head_offset != head_before || drive.rotation.gcr_head_offset > 0,
        "the head must have advanced over the GCR bitstream"
    );
    assert_eq!(
        drive.rotation.current_half_track, 36,
        "the head should be at track 18 (half-track 36) for this read"
    );

    // FUNCTIONAL GATE: the job completes with $01 (OK)...
    assert_eq!(
        status, 0x01,
        "read job must complete with status $01 (CBMDOS_FDC_ERR_OK), got {status:#04x}"
    );

    // ...and the decoded sector buffer at $0300 is byte-identical to the D64.
    let mut readback = [0u8; 256];
    for (i, b) in readback.iter_mut().enumerate() {
        *b = drive.drive_ram_read(0x0300 + i as u16);
    }
    let expect = &d64[off..off + 256];
    assert_eq!(
        &readback[..],
        expect,
        "decoded sector $0300 must be byte-identical to D64 track-18 sector-0"
    );
    eprintln!("drive sector-read job completed with status {status:#04x} (0x01 = OK), sector byte-exact");
}
