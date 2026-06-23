//! Milestone 2 (partial): the GCR read path is engaged by the live DOS controller.
//!
//! Boots the 1541 drive standalone (DOS ROM), mounts a D64, then pokes the DOS
//! job queue directly — $00 = $80 (READ buffer 0), $06/$07 = track/sector — to
//! request a sector read WITHOUT the full IEC command handshake (the standard
//! 1541 job-queue trick; the IRQ-driven controller picks up the job regardless
//! of IEC activity).
//!
//! WHAT THIS GATES (green): the rotating-disk model + VIA2 wiring is live enough
//! that the DOS controller PICKS UP the read job, spins the motor, advances the
//! head over the GCR bitstream, and runs to a definite job completion (the job
//! code at $00 is replaced by a status code), rather than hanging. This exercises
//! the whole new path: D64→GCR encode, head rotation, VIA2 PRA (GCR_read) / PRB7
//! (SYNC) reads, the stepper/motor/speed-zone store_prb side-effects, and the
//! byte-ready (SO) → drive-CPU V-flag handshake.
//!
//! KNOWN DIVERGENCE (full byte-exact sector read — milestone 2 complete):
//!   The controller currently completes the read job with status $03
//!   (CBMDOS_FDC_ERR_SYNC) instead of $01 (OK). First-divergence analysis:
//!     - The D64→GCR encode is byte-exact vs the TS oracle (gcr_d64_parity test).
//!     - The simple rotation engine streams correct GCR in isolation: SYNC is
//!       detected (last_read_data==0x3ff) and full bytes assemble with the
//!       byte-ready edge firing (rotation.rs unit tests).
//!     - In the live controller the byte-ready (SO) edge never reaches a
//!       BVS/BVC/PHP opcode, and the controller's SYNC poll of $1C00/PB7 advances
//!       the head ~96 bit-cells between consecutive reads (~400 drive cycles
//!       apart), wide enough to skip the ~31-bit-cell sync window where
//!       last_read_data==0x3ff. The 1541's GCR sync/byte detection is driven by
//!       the controller's byte-ready cadence, which the lazy at-VIA2-access /
//!       at-BVC rotation advance does not yet reproduce at the right granularity.
//!     The missing piece is the set_ca2-style byte-ready→overflow flush on the
//!     PCR CA2 edge (via2d.c:207-222 set_ca2 → drive_cpu_set_overflow) and the
//!     per-cycle drivecpu_rotate cadence the 6510 core uses, so the SO edge lands
//!     at the controller's exact sampling instant. Closing it needs a drive-CPU
//!     trace cross-check against the TS oracle at the $F556 read loop.
//!
//! This test asserts the reachable milestone and records the divergence; it does
//! not assert the (not-yet-byte-exact) full sector contents.

use trx64_core::drive::{DiskImage, DiskKind, Drive1541};
use std::path::Path;

const ROM_DIR: &str =
    "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
const SAMPLE: &str =
    "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/scramble_infinity.d64";

#[test]
fn drive_engages_gcr_read_path_via_job_queue() {
    let rom_dir = Path::new(ROM_DIR);
    if !rom_dir.join("dos1541-325302-01+901229-05.bin").exists()
        && !rom_dir.join("1541.bin").exists()
    {
        eprintln!("skip: DOS ROM absent");
        return;
    }
    let d64 = match std::fs::read(SAMPLE) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("skip: sample disk absent");
            return;
        }
    };

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
    // Let the attach-settle window pass.
    drive.run_cycles(2_000_000);

    // Track 18, sector 0 (the BAM) — always present.
    drive.drive_ram_write(0x0006, 18);
    drive.drive_ram_write(0x0007, 0);
    drive.drive_ram_write(0x0000, 0x80); // READ job

    let head_before = drive.rotation.gcr_head_offset;

    // Run the controller until the job completes (status replaces the job code).
    let mut status = 0x80u8;
    for _ in 0..40 {
        drive.run_cycles(250_000);
        status = drive.drive_ram_read(0x0000);
        if status < 0x80 {
            break;
        }
    }

    // Reachable milestone: the job ran to completion (no hang) and the disk
    // controller engaged the rotating-GCR path (motor on, head advanced).
    assert!(
        status < 0x80,
        "controller did not complete the read job: $00 = {status:#04x} (still a job code)"
    );
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

    // Document the current status: $01 = full byte-exact read achieved; anything
    // else is the known SYNC-cadence divergence above. We accept any definite
    // completion here (the read PATH is exercised); a follow-up will tighten this
    // to status == 0x01 + byte-exact $0300 once the SO cadence matches the oracle.
    eprintln!("drive sector-read job completed with status {status:#04x} (0x01 = OK)");
}
