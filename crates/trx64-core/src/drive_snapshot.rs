//! drive_snapshot.rs — the VICE 1541 drive snapshot module-stream (the
//! `cp.drive1541` + `cp.driveDiskImage` blobs of the `.c64re` RuntimeCheckpoint).
//!
//! 1:1 PORT of the c64re TS drive-snapshot surface:
//!   C64ReverseEngineeringMCP/src/runtime/headless/vice1541/drive_snapshot.ts
//!   + drivecpu.ts (drivecpu_snapshot_write/read_module)
//!   + the vice1541-facade.ts wiring (snapshot()/restore() + snapshotDiskImage()/
//!     restoreDiskImage()), which is itself the verbatim port of VICE
//!   vice/src/drive/drive-snapshot.c + drivecpu.c + gcr.c.
//!
//! The bytes this writes are IDENTICAL to what a live c64re daemon's
//! `drive1541.snapshot()` / `.snapshotDiskImage()` writes, so a TRX64 `.c64re`
//! dump with an attached drive resumes cross-runtime in c64re's
//! `drive_snapshot_read_module` (and vice-versa). It layers on
//! [`crate::vice_snapshot_stream::SnapshotT`] (ADR-078) + the VIA1D/VIA2D
//! [`viacore_snapshot_write_module`]/[`_read_module`] (this crate's viacore.rs).
//!
//! Two payloads (matching the c64re facade split, Spec 714.4):
//!   1. `drive1541` blob = `drive_snapshot_write_module(s, save_disks=0,
//!      save_roms=0)` — the drive CORE: DRIVE8 + DRIVECPU0 + 1541VIA1D0 + VIA2D0.
//!      NO disk image, NO ROM.
//!   2. `driveDiskImage` blob = `drive_snapshot_write_gcrimage_module(s, 0)` — the
//!      mutable disk content (the GCRIMAGE0 module).
//!
//! Field parity notes (vs drive-snapshot.c / drivecpu.c / the c64re facade):
//!   - The c64re facade wires `interrupt_write_snapshot`/`_read_snapshot`/
//!     `_write_new_snapshot`/`_read_new_snapshot` ALL to no-ops returning 0, so the
//!     DRIVECPU module carries NO interrupt sub-blocks — just the header regs +
//!     0x800 RAM. We match exactly (drivecpu.ts:1006/1045 hooks are no-ops).
//!   - `vdrive_snapshot_module_write/read`, `machine_drive_rom_setup_image`,
//!     `ieee_drive_snapshot_*` are no-ops in the c64re facade → the `drive1541`
//!     blob is exactly DRIVE8 + DRIVECPU0 + 1541VIA1D0 + VIA2D0 in that order.
//!   - VICE drive_t fields TRX64's DISTILLED rotation lacks (`snap_ue7_dcba` etc.
//!     map to live rotation fields; `last_clk`/`last_exc_cycles` of drivecpu have
//!     no TRX64 mirror) are emitted from the closest live value / a stable
//!     placeholder. The resume needs CLK + regs + RAM + VIA + GCR head, which IS
//!     captured; the byte-exact gates are the guard (ADR-077).

use crate::drive::{DiskImage, DiskKind, Drive1541};
use crate::gcr::GcrImage;
use crate::vice_snapshot_stream::{snapshot_version_is_bigger, SnapshotT};
use crate::viacore;

// ── VICE snapshot module versions (drive-snapshot.c / drivecpu.c) ────────────────

/// drive-snapshot.c:159-160 — DRIVE_SNAP_MAJOR / _MINOR.
const DRIVE_SNAP_MAJOR: u8 = 2;
const DRIVE_SNAP_MINOR: u8 = 0;

/// drivecpu.c:565-566 — SNAP_MAJOR / SNAP_MINOR (1.3 added cpu_last_data).
const DRIVECPU_SNAP_MAJOR: u8 = 1;
const DRIVECPU_SNAP_MINOR: u8 = 3;

/// drive-snapshot.c:857-858 — GCRIMAGE_SNAP_MAJOR / _MINOR.
const GCRIMAGE_SNAP_MAJOR: u8 = 3;
const GCRIMAGE_SNAP_MINOR: u8 = 1;

/// drivetypes.ts:110 — DRIVE_HALFTRACKS_1571 (the half-track multiplier folded into
/// the saved `current_half_track + side*DRIVE_HALFTRACKS_1571` word).
const DRIVE_HALFTRACKS_1571: u32 = 84;

/// drivetypes.ts:196 — MAX_GCR_TRACKS (read-side num_half_tracks bound).
const MAX_GCR_TRACKS: u32 = 168;
/// drivetypes.ts:194 — NUM_MAX_MEM_BYTES_TRACK (per-track size bound).
const NUM_MAX_MEM_BYTES_TRACK: u32 = 65536;
/// drive-snapshot.c:146 — MAX_TRACKS_1571 (gcrimage num_half_tracks = ×2 = 140).
const MAX_TRACKS_1571: u32 = 70;

/// VICE drive type for the 1541 (drivetypes.ts DRIVE_TYPE_1541). Stamped into the
/// DRIVE module `type` dword so a c64re read re-enables the 1541 path.
const DRIVE_TYPE_1541: u32 = 1541;

/// VICE sync_factor for PAL (MachineVideoStandard → sync_factor in the DRIVE
/// module). The c64re facade reports `MachineVideoStandard = MACHINE_SYNC_PAL = 0`.
const MACHINE_SYNC_PAL: u32 = 0;

// GCRIMAGE track indexing: the on-wire entry index IS the 0-based slot index, the
// SAME in TRX64 and c64re (both store `tracks[slot]` = data for half-track slot+2;
// c64re fsimage_gcr.ts:314 read_half_track(half_track+2, tracks[half_track])). The
// c64re drive_snapshot_write_gcrimage_module writes `drive.gcr.tracks[i]` directly
// — so there is NO half-track offset in the wire format (entry i ↔ tracks[i]).

// =============================================================================
// drive1541 blob — drive_snapshot_write_module(s, 0, 0) equivalent
// =============================================================================

/// Build the `drive1541` blob from the live drive (= c64re `drive1541.snapshot()`).
/// Module order: DRIVE8, DRIVECPU0, 1541VIA1D0, VIA2D0. Returns the raw bytes the
/// `.c64re` checkpoint stores as the `cp.drive1541` `$ta` node.
pub fn capture_drive1541(drive: &mut Drive1541) -> Vec<u8> {
    let mut s = SnapshotT::create_in_memory();
    write_drive_module(drive, &mut s);
    write_drivecpu_module(drive, &mut s);
    write_via_modules(drive, &mut s);
    s.to_bytes()
}

/// Restore the live drive from a `drive1541` blob (= c64re `drive1541.restore()`).
/// Returns Ok on success; Err(reason) on a malformed/incompatible blob.
pub fn restore_drive1541(drive: &mut Drive1541, blob: &[u8]) -> Result<(), String> {
    let mut s = SnapshotT::open_in_memory(blob);
    // VICE drive_snapshot_read_module order: read the DRIVE module rotation fields,
    // then drivecpu, then VIA — and re-establish the head position (drive_set_
    // half_track + GCR_head_offset) LAST (drive-snapshot.c:601-620, AFTER the VIA
    // undump). The VIA2 undump_prb/undump_pcr call rotation_rotate_disk, which would
    // move the head if applied before; deferring the head set matches VICE exactly.
    let head = read_drive_module(drive, &mut s)?;
    read_drivecpu_module(drive, &mut s)?;
    read_via_modules(drive, &mut s)?;
    if let Some((half_track, gcr_head_offset)) = head {
        // drive_set_half_track re-resolves the active track size + GCR_track_start_ptr.
        drive.rotation.set_half_track(half_track);
        // VICE restores GCR_head_offset directly (drive-snapshot.c:440 sets it into
        // the drive_t; set_half_track only rescales, then the saved value is the
        // authoritative head position at the snapshot instant).
        drive.rotation.gcr_head_offset = gcr_head_offset;
    }
    drive.snapshot_sync_drive_clk();
    Ok(())
}

// =============================================================================
// DRIVE<n> module — drive-snapshot.c:162-354 / :356-639 (single 1541, unit 0)
// =============================================================================
//
// Field order (drive-snapshot.c:204-272), for has_tde=1, one drive:
//   B  has_tde
//   B  has_drives
//   DW sync_factor                                  (MachineVideoStandard)
//   --- per drive (dnr=0) ---
//   CLOCK attach_clk
//   B  byte_ready_level
//   B  clock_frequency
//   W  current_half_track + side*DRIVE_HALFTRACKS_1571
//   CLOCK detach_clk
//   B  extend_image_policy
//   DW GCR_head_offset
//   B  GCR_read
//   B  GCR_write_value
//   B  idling_method
//   B  parallel_cable
//   B  read_only
//   DW rotation_table_ptr[unr]
//   DW type
//   DW snap_accum
//   CLOCK snap_rotation_last_clk
//   DW snap_bit_counter
//   DW snap_zero_count
//   W  snap_last_read_data
//   B  snap_last_write_data
//   DW snap_seed
//   DW snap_speed_zone
//   DW snap_ue7_dcba
//   DW snap_ue7_counter
//   DW snap_uf4_counter
//   DW snap_fr_randcount
//   DW snap_filter_counter
//   DW snap_filter_state
//   DW snap_filter_last_state
//   DW snap_write_flux
//   DW snap_PulseHeadPosition
//   DW snap_xorShift32
//   DW snap_so_delay
//   DW snap_cycle_index
//   CLOCK snap_ref_advance
//   DW snap_req_ref_cycles
//   CLOCK attach_detach_clk
//   B  byte_ready_edge
//   B  byte_ready_active

fn write_drive_module(drive: &mut Drive1541, s: &mut SnapshotT) {
    let mut m = s.module_create("DRIVE8", DRIVE_SNAP_MAJOR, DRIVE_SNAP_MINOR);

    // has_tde = 1, has_drives = 1 (single 1541).
    s.smw_b(&mut m, 1);
    s.smw_b(&mut m, 1);
    // sync_factor (MachineVideoStandard) — PAL.
    s.smw_dw(&mut m, MACHINE_SYNC_PAL);

    let r = &drive.rotation;
    let half_track_word =
        (r.current_half_track + (r.side as u32) * DRIVE_HALFTRACKS_1571) & 0xffff;

    s.smw_clock(&mut m, r.attach_clk);
    s.smw_b(&mut m, r.byte_ready_level);
    // clock_frequency = 1 for the 1541 (unit.clock_frequency).
    s.smw_b(&mut m, 1);
    s.smw_w(&mut m, half_track_word as u16);
    // detach_clk — TRX64 has no detach window field; emit 0 (settled).
    s.smw_clock(&mut m, 0);
    // extend_image_policy — TRX64 has no extend policy; emit 0.
    s.smw_b(&mut m, 0);
    s.smw_dw(&mut m, r.gcr_head_offset);
    s.smw_b(&mut m, r.gcr_read);
    s.smw_b(&mut m, r.gcr_write_value);
    // idling_method (unit.idling_method) — 0 = DRIVE_IDLE_NO_IDLE (the c64re
    // facade's live value, vice1541-facade.ts:304).
    s.smw_b(&mut m, 0);
    // parallel_cable (unit.parallel_cable) — 0 = DRIVE_PC_NONE.
    s.smw_b(&mut m, 0);
    s.smw_b(&mut m, (r.read_only & 0xff) as u8);
    // rotation_table_ptr[unr] — VICE rotation_table_get writes `speed_zone` here
    // (rotation.ts:273 / rotation.c:153), NOT `frequency`. (`snap_speed_zone` below
    // carries the same value; on restore rotation_table_set sets speed_zone from
    // this field, then snap_speed_zone overwrites it — so this slot MUST be
    // speed_zone for byte-exact wire parity.)
    s.smw_dw(&mut m, r.speed_zone as u32);
    s.smw_dw(&mut m, DRIVE_TYPE_1541);

    // snap_* rotation fields.
    s.smw_dw(&mut m, r.accum);
    s.smw_clock(&mut m, r.rotation_last_clk);
    s.smw_dw(&mut m, r.bit_counter as u32);
    s.smw_dw(&mut m, r.zero_count as u32);
    s.smw_w(&mut m, (r.last_read_data & 0xffff) as u16);
    s.smw_b(&mut m, r.last_write_data);
    s.smw_dw(&mut m, r.seed);
    s.smw_dw(&mut m, r.speed_zone as u32);
    s.smw_dw(&mut m, r.ue7_dcba as u32);
    s.smw_dw(&mut m, r.ue7_counter as u32);
    s.smw_dw(&mut m, r.uf4_counter as u32);
    s.smw_dw(&mut m, r.fr_randcount);
    s.smw_dw(&mut m, r.filter_counter as u32);
    s.smw_dw(&mut m, r.filter_state as u32);
    s.smw_dw(&mut m, r.filter_last_state as u32);
    s.smw_dw(&mut m, r.write_flux as u32);
    s.smw_dw(&mut m, r.pulse_head_position);
    s.smw_dw(&mut m, r.xor_shift32);
    s.smw_dw(&mut m, r.so_delay as u32);
    s.smw_dw(&mut m, r.cycle_index);
    s.smw_clock(&mut m, r.ref_advance);
    s.smw_dw(&mut m, (r.req_ref_cycles & 0xffff_ffff) as u32);
    s.smw_clock(&mut m, r.attach_detach_clk);
    s.smw_b(&mut m, r.byte_ready_edge);
    s.smw_b(&mut m, r.byte_ready_active);

    s.module_close(&m);
}

/// Read the DRIVE8 module into the live rotation. Applies every rotation field
/// EXCEPT the head position (`current_half_track` / `gcr_head_offset`), which the
/// caller re-establishes LAST via `set_half_track` (VICE order — after the VIA
/// undump's rotate). Returns `Some((half_track, gcr_head_offset))` for that final
/// step, or `None` when the dump carried no true-drive-emulation (has_tde=0).
fn read_drive_module(
    drive: &mut Drive1541,
    s: &mut SnapshotT,
) -> Result<Option<(u32, u32)>, String> {
    let (m, major, minor) = s
        .module_open("DRIVE8")
        .ok_or("drive_snapshot: DRIVE8 module missing")?;
    let _ = m;
    if snapshot_version_is_bigger(major, minor, DRIVE_SNAP_MAJOR, DRIVE_SNAP_MINOR) {
        return Err("drive_snapshot: DRIVE8 module higher version".into());
    }

    macro_rules! rb {
        () => {
            s.smr_b().ok_or("drive_snapshot: DRIVE8 truncated (byte)")?
        };
    }
    macro_rules! rw {
        () => {
            s.smr_w().ok_or("drive_snapshot: DRIVE8 truncated (word)")?
        };
    }
    macro_rules! rdw {
        () => {
            s.smr_dw().ok_or("drive_snapshot: DRIVE8 truncated (dword)")?
        };
    }
    macro_rules! rclk {
        () => {
            s.smr_clock().ok_or("drive_snapshot: DRIVE8 truncated (clock)")?
        };
    }

    let has_tde = rb!();
    let _has_drives = rb!();
    if has_tde == 0 {
        // No true-drive-emulation in the dump — leave the live drive as-is.
        s.module_close(&m);
        return Ok(None);
    }
    let _sync_factor = rdw!();

    let attach_clk = rclk!();
    let byte_ready_level = rb!();
    let _clock_frequency = rb!();
    let half_track_word = rw!() as u32;
    let _detach_clk = rclk!();
    let _extend_image_policy = rb!();
    let gcr_head_offset = rdw!();
    let gcr_read = rb!();
    let gcr_write_value = rb!();
    let _idling_method = rb!();
    let _parallel_cable = rb!();
    let read_only = rb!();
    // rotation_table_ptr = speed_zone (rotation.ts:322 sets speed_zone from it,
    // then snap_speed_zone below overwrites — so this leading copy is redundant;
    // consumed for byte-exact wire position, the snap_speed_zone value wins).
    let _rotation_table_ptr = rdw!();
    let _type = rdw!();

    let accum = rdw!();
    let rotation_last_clk = rclk!();
    let bit_counter = rdw!() as i32;
    let zero_count = rdw!() as i32;
    let last_read_data = rw!() as u32;
    let last_write_data = rb!();
    let seed = rdw!();
    let speed_zone = rdw!();
    let ue7_dcba = rdw!() as i32;
    let ue7_counter = rdw!() as i32;
    let uf4_counter = rdw!() as i32;
    let fr_randcount = rdw!();
    let filter_counter = rdw!() as i32;
    let filter_state = rdw!() as i32;
    let filter_last_state = rdw!() as i32;
    let write_flux = rdw!() as i32;
    let pulse_head_position = rdw!();
    let xor_shift32 = rdw!();
    let so_delay = rdw!() as i32;
    let cycle_index = rdw!();
    let ref_advance = rclk!();
    let req_ref_cycles = rdw!() as u64;
    let attach_detach_clk = rclk!();
    let byte_ready_edge = rb!();
    let byte_ready_active = rb!();

    s.module_close(&m);

    // Apply every rotation field EXCEPT the head position (current_half_track +
    // gcr_head_offset), which the caller re-establishes LAST (VICE order: after the
    // VIA undump's rotate_disk). side handling (drive-snapshot.c:607-616) is 1571
    // only; the 1541 keeps side 0, so half_track_word == current_half_track.
    let r = &mut drive.rotation;
    r.attach_clk = attach_clk;
    r.attach_detach_clk = attach_detach_clk;
    r.byte_ready_level = byte_ready_level;
    r.gcr_read = gcr_read;
    r.gcr_write_value = gcr_write_value;
    r.read_only = read_only as i32;
    // `frequency` (1x/2x toggle) is NOT in the VICE DRIVE module — it is derived
    // live from the speed-zone density and left untouched on restore.

    r.accum = accum;
    r.rotation_last_clk = rotation_last_clk;
    r.bit_counter = bit_counter;
    r.zero_count = zero_count;
    r.last_read_data = last_read_data;
    r.last_write_data = last_write_data;
    r.seed = seed;
    r.speed_zone = speed_zone as usize;
    r.ue7_dcba = ue7_dcba;
    r.ue7_counter = ue7_counter;
    r.uf4_counter = uf4_counter;
    r.fr_randcount = fr_randcount;
    r.filter_counter = filter_counter;
    r.filter_state = filter_state;
    r.filter_last_state = filter_last_state;
    r.write_flux = write_flux;
    r.pulse_head_position = pulse_head_position;
    r.xor_shift32 = xor_shift32;
    r.so_delay = so_delay;
    r.cycle_index = cycle_index;
    r.ref_advance = ref_advance;
    r.req_ref_cycles = req_ref_cycles;
    r.byte_ready_edge = byte_ready_edge;
    r.byte_ready_active = byte_ready_active;

    Ok(Some((half_track_word, gcr_head_offset)))
}

// =============================================================================
// DRIVECPU<n> module — drivecpu.c:568-640 / :642-737 (no interrupt sub-blocks)
// =============================================================================
//
// drivecpu.c:934-953 format (1.3): CLOCK clk; B a,x,y,sp; W pc; B status;
// DW last_opcode_info; CLOCK last_clk, cycle_accum, last_exc_cycles, stop_clk;
// B cpu_last_data; ARRAY drive RAM (0x800 for the 1541). The c64re facade wires
// the interrupt snapshot hooks to no-ops, so NO interrupt block follows the RAM.

fn write_drivecpu_module(drive: &mut Drive1541, s: &mut SnapshotT) {
    let mut m = s.module_create("DRIVECPU0", DRIVECPU_SNAP_MAJOR, DRIVECPU_SNAP_MINOR);

    let clk = drive.core.clk;
    let a = drive.core.reg_a;
    let x = drive.core.reg_x;
    let y = drive.core.reg_y;
    let sp = drive.core.reg_sp;
    let pc = drive.core.reg_pc;
    let status = drive.core.status();
    let last_opcode_info = drive.core.last_opcode_info;
    let cycle_accum = drive.snapshot_sync_accum() as u64;
    let stop_clk = drive.snapshot_stop_clk();

    s.smw_clock(&mut m, clk);
    s.smw_b(&mut m, a);
    s.smw_b(&mut m, x);
    s.smw_b(&mut m, y);
    s.smw_b(&mut m, sp);
    s.smw_w(&mut m, pc);
    s.smw_b(&mut m, status);
    s.smw_dw(&mut m, last_opcode_info);
    // last_clk — VICE drivesync field; TRX64 mirrors it on `core.clk`.
    s.smw_clock(&mut m, clk);
    // cycle_accum = the drive-sync fixed-point accumulator.
    s.smw_clock(&mut m, cycle_accum);
    // last_exc_cycles — VICE drivesync field; no TRX64 mirror, emit 0.
    s.smw_clock(&mut m, 0);
    s.smw_clock(&mut m, stop_clk);
    // cpu_last_data — no TRX64 mirror, emit 0.
    s.smw_b(&mut m, 0);

    // ARRAY drive RAM (0x800 for the 1541).
    let ram = *drive.snapshot_ram();
    s.smw_ba(&mut m, &ram, 0x800);

    s.module_close(&m);
}

fn read_drivecpu_module(drive: &mut Drive1541, s: &mut SnapshotT) -> Result<(), String> {
    let (m, _major, _minor) = s
        .module_open("DRIVECPU0")
        .ok_or("drive_snapshot: DRIVECPU0 module missing")?;
    let _ = m;

    macro_rules! rb {
        () => {
            s.smr_b().ok_or("drive_snapshot: DRIVECPU0 truncated (byte)")?
        };
    }
    macro_rules! rw {
        () => {
            s.smr_w().ok_or("drive_snapshot: DRIVECPU0 truncated (word)")?
        };
    }
    macro_rules! rdw {
        () => {
            s.smr_dw().ok_or("drive_snapshot: DRIVECPU0 truncated (dword)")?
        };
    }
    macro_rules! rclk {
        () => {
            s.smr_clock()
                .ok_or("drive_snapshot: DRIVECPU0 truncated (clock)")?
        };
    }

    let clk = rclk!();
    let a = rb!();
    let x = rb!();
    let y = rb!();
    let sp = rb!();
    let pc = rw!();
    let status = rb!();
    let last_opcode_info = rdw!();
    let _last_clk = rclk!();
    let cycle_accum = rclk!();
    let _last_exc_cycles = rclk!();
    let stop_clk = rclk!();
    let _cpu_last_data = rb!();

    // ARRAY drive RAM (0x800).
    let mut ram = [0u8; 0x800];
    if !s.smr_ba(&mut ram, 0x800) {
        return Err("drive_snapshot: DRIVECPU0 truncated (RAM)".into());
    }

    s.module_close(&m);

    drive.core.clk = clk;
    drive.core.reg_a = a;
    drive.core.reg_x = x;
    drive.core.reg_y = y;
    drive.core.reg_sp = sp;
    drive.core.reg_pc = pc;
    drive.core.set_status_composite(status);
    drive.core.last_opcode_info = last_opcode_info;
    drive.snapshot_set_sync_accum((cycle_accum & 0xffff_ffff) as u32);
    drive.snapshot_set_stop_clk(stop_clk);
    *drive.snapshot_ram_mut() = ram;

    Ok(())
}

// =============================================================================
// VIA1D / VIA2D modules — via the viacore snapshot module-stream (this crate)
// =============================================================================
//
// drive_snapshot.c order (machine_drive_snapshot_write → iec_drive_snapshot_write
// then iecieee_drive_snapshot_write): VIA1 (1541VIA1D0) first, then VIA2 (VIA2D0).

fn write_via_modules(drive: &mut Drive1541, s: &mut SnapshotT) {
    drive.snapshot_via1(|ctx, b| {
        viacore::viacore_snapshot_write_module(ctx, b, s);
    });
    drive.snapshot_via2(|ctx, b| {
        viacore::viacore_snapshot_write_module(ctx, b, s);
    });
}

fn read_via_modules(drive: &mut Drive1541, s: &mut SnapshotT) -> Result<(), String> {
    let rc1 = drive.snapshot_via1(|ctx, b| viacore::viacore_snapshot_read_module(ctx, b, s));
    if rc1 < 0 {
        return Err("drive_snapshot: VIA1 (1541VIA1D0) read failed".into());
    }
    let rc2 = drive.snapshot_via2(|ctx, b| viacore::viacore_snapshot_read_module(ctx, b, s));
    if rc2 < 0 {
        return Err("drive_snapshot: VIA2 (VIA2D0) read failed".into());
    }
    Ok(())
}

// =============================================================================
// GCRIMAGE<n> module — drive-snapshot.c:860-903 / :905-987 (the disk content)
// =============================================================================
//
// Format: DW num_half_tracks (= MAX_TRACKS_1571*2 = 140); then per half-track:
//   DW track_size; if track_size: BA track_data[track_size].
// VICE indexes `gcr->tracks[i]` (half-track-indexed, slots 0/1 unused); TRX64's
// `image.tracks[i-2]` (0-based slot). The on-wire index space is VICE's, so we
// map slot ↔ (i - GCR_TRACK_VICE_OFFSET).

/// Build the `driveDiskImage` blob (= c64re `drive1541.snapshotDiskImage()`), or
/// `None` when no GCR image is loaded.
pub fn capture_drive_disk_image(drive: &Drive1541) -> Option<Vec<u8>> {
    let img = drive.rotation.image.as_ref()?;
    if drive.rotation.gcr_image_loaded == 0 {
        return None;
    }
    let mut s = SnapshotT::create_in_memory();
    let mut m = s.module_create("GCRIMAGE0", GCRIMAGE_SNAP_MAJOR, GCRIMAGE_SNAP_MINOR);

    let num_half_tracks = MAX_TRACKS_1571 * 2; // 140

    s.smw_dw(&mut m, num_half_tracks);

    // The on-wire track index IS the 0-based slot index — IDENTICAL in TRX64 and
    // c64re. Both store `tracks[slot]` = the data for actual half-track `slot + 2`
    // (TRX64 from_d64 `half_track = track*2-2`; c64re fsimage_gcr.ts:314
    // `read_half_track(half_track + 2, tracks[half_track])`). The c64re
    // drive_snapshot_write_gcrimage_module writes `drive.gcr.tracks[i]` directly,
    // so snapshot entry `i` ↔ TRX64 `image.tracks[i]` with NO offset.
    for i in 0..num_half_tracks {
        match img
            .tracks
            .get(i as usize)
            .filter(|t| t.size > 0 && !t.data.is_empty())
        {
            Some(t) => {
                let track_size = t.size as u32;
                s.smw_dw(&mut m, track_size);
                s.smw_ba(&mut m, &t.data, track_size as usize);
            }
            None => {
                s.smw_dw(&mut m, 0);
            }
        }
    }

    s.module_close(&m);
    Some(s.to_bytes())
}

/// Restore the mutable disk content onto the live GCR buffer (= c64re
/// `restoreDiskImage`). Overwrites the per-half-track GCR bytes; a no-op return is
/// Ok when the GCRIMAGE0 module is absent (drive kept at its baseline).
pub fn restore_drive_disk_image(drive: &mut Drive1541, blob: &[u8]) -> Result<(), String> {
    let mut s = SnapshotT::open_in_memory(blob);
    let opened = match s.module_open("GCRIMAGE0") {
        Some(o) => o,
        None => return Ok(()), // module absent → keep the baseline image.
    };
    let (m, major, minor) = opened;
    let _ = m;
    if snapshot_version_is_bigger(major, minor, GCRIMAGE_SNAP_MAJOR, GCRIMAGE_SNAP_MINOR) {
        return Err("drive_snapshot: GCRIMAGE0 higher version".into());
    }

    let num_half_tracks = s
        .smr_dw()
        .ok_or("drive_snapshot: GCRIMAGE0 truncated (num_half_tracks)")?;
    if num_half_tracks > MAX_GCR_TRACKS {
        return Err("drive_snapshot: GCRIMAGE0 num_half_tracks too large".into());
    }

    // Ensure the rotation has a GCR image to overlay onto. The on-wire track index
    // IS the 0-based slot index (no offset — see capture_drive_disk_image): entry
    // `i` ↔ `image.tracks[i]`. Grow the slot vector to cover the written range.
    let img = drive
        .rotation
        .image
        .get_or_insert_with(|| GcrImage { tracks: Vec::new() });
    if img.tracks.len() < num_half_tracks as usize {
        img.tracks.resize_with(num_half_tracks as usize, || {
            crate::gcr::GcrTrack { data: Vec::new(), size: 0 }
        });
    }

    for i in 0..num_half_tracks {
        let track_size = s
            .smr_dw()
            .ok_or("drive_snapshot: GCRIMAGE0 truncated (track_size)")?;
        if track_size > NUM_MAX_MEM_BYTES_TRACK {
            return Err("drive_snapshot: GCRIMAGE0 track_size too large".into());
        }
        if track_size > 0 {
            let mut data = vec![0u8; track_size as usize];
            if !s.smr_ba(&mut data, track_size as usize) {
                return Err("drive_snapshot: GCRIMAGE0 truncated (track data)".into());
            }
            if let Some(t) = img.tracks.get_mut(i as usize) {
                t.data = data;
                t.size = track_size as usize;
            }
        } else if let Some(t) = img.tracks.get_mut(i as usize) {
            t.data = Vec::new();
            t.size = 0;
        }
    }
    s.module_close(&m);

    drive.rotation.gcr_image_loaded = 1;
    drive.rotation.complicated_image_loaded = 1;
    // Re-resolve the active track size for the current head WITHOUT rescaling the
    // head offset. The drive1541 blob already restored `current_half_track` +
    // `gcr_head_offset` via its own drive_set_half_track + the explicit head value;
    // `set_half_track` here would re-rescale `gcr_head_offset` by the (now-overlaid)
    // track-size ratio and corrupt it. VICE's drive_snapshot_read_gcrimage_module
    // likewise only swaps the track buffers — the head is set elsewhere. So just
    // point `gcr_current_track_size` at the current track's overlaid size.
    let slot = (drive.rotation.current_half_track as usize).wrapping_sub(2);
    let cur_size = drive
        .rotation
        .image
        .as_ref()
        .and_then(|img| img.tracks.get(slot))
        .map(|t| t.size)
        .unwrap_or(0);
    drive.rotation.gcr_current_track_size = cur_size;

    Ok(())
}

/// Re-derive a `DiskImage` placeholder when undumping into a fresh drive that has
/// no attached disk yet, so `restore_drive_disk_image` has a GCR image to overlay.
/// `kind`/`bytes` come from the embedded media (the daemon attaches the disk
/// before the drive restore); this is a fallback for a media-less undump.
pub fn ensure_disk_attached(drive: &mut Drive1541, bytes: &[u8], kind: DiskKind) {
    if drive.get_attached_disk().is_some() {
        return;
    }
    drive.attach_disk(DiskImage {
        kind,
        bytes: bytes.to_vec(),
        backing_path: None,
        read_only: false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const ROM_DIR: &str =
        "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
    const SAMPLE: &str =
        "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/scramble_infinity.d64";

    fn rom_present() -> bool {
        let p = Path::new(ROM_DIR);
        p.join("dos1541-325302-01+901229-05.bin").exists() || p.join("1541.bin").exists()
    }

    #[test]
    fn drive1541_blob_roundtrip_no_disk() {
        if !rom_present() {
            eprintln!("skip: DOS ROM absent");
            return;
        }
        let mut drive = Drive1541::new();
        drive.load_rom(Path::new(ROM_DIR)).expect("load DOS ROM");
        drive.cold_reset();
        drive.run_cycles(1_200_000);

        // Seed a recognizable RAM pattern.
        drive.drive_ram_write(0x0040, 0xab);
        drive.drive_ram_write(0x07ff, 0xcd);

        let pc = drive.core.reg_pc;
        let a = drive.core.reg_a;
        let sp = drive.core.reg_sp;
        let clk = drive.core.clk;
        let via2_ifr = drive.via2_ifr_test();
        let blob = capture_drive1541(&mut drive);
        assert!(!blob.is_empty());

        // Restore into a fresh drive.
        let mut d2 = Drive1541::new();
        d2.load_rom(Path::new(ROM_DIR)).expect("load DOS ROM");
        d2.cold_reset();
        restore_drive1541(&mut d2, &blob).expect("restore drive1541 blob");

        assert_eq!(d2.core.reg_pc, pc, "drive PC");
        assert_eq!(d2.core.reg_a, a, "drive A");
        assert_eq!(d2.core.reg_sp, sp, "drive SP");
        assert_eq!(d2.core.clk, clk, "drive CLK");
        assert_eq!(d2.drive_ram_read(0x0040), 0xab, "drive RAM $40");
        assert_eq!(d2.drive_ram_read(0x07ff), 0xcd, "drive RAM $7ff");
        assert_eq!(d2.via2_ifr_test(), via2_ifr, "VIA2 IFR");
    }

    #[test]
    fn drive1541_blob_roundtrip_with_disk() {
        if !rom_present() {
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
        drive.load_rom(Path::new(ROM_DIR)).expect("load DOS ROM");
        drive.cold_reset();
        drive.run_cycles(1_200_000);
        drive.attach_disk(DiskImage {
            kind: DiskKind::D64,
            bytes: d64.clone(),
            backing_path: Some(SAMPLE.to_string()),
            read_only: false,
        });
        drive.run_cycles(2_000_000);

        let head = drive.rotation.gcr_head_offset;
        let half_track = drive.rotation.current_half_track;
        let track_size = drive.rotation.gcr_current_track_size;
        let pc = drive.core.reg_pc;
        // VICE's drive_snapshot_read_module order means VIA2 undump_prb re-derives
        // speed_zone from (PRB | ~DDRB) >> 5 & 3 AFTER the DRIVE module — so the
        // post-restore speed_zone is the register-derived value, NOT the live
        // snap_speed_zone. Compute the expectation the same way.
        let (prb, ddrb) = drive.via2_prb_ddrb_test();
        let expected_speed_zone = (((prb | !ddrb) >> 5) & 0x03) as usize;

        let blob = capture_drive1541(&mut drive);
        let disk_blob = capture_drive_disk_image(&drive).expect("disk image blob");
        assert!(!disk_blob.is_empty());

        // Sample a known GCR byte from the current track for content comparison.
        let cur_slot = (half_track as usize) - 2;
        let sample_byte = drive.rotation.image.as_ref().unwrap().tracks[cur_slot]
            .data
            .get(100)
            .copied();

        // Restore into a fresh drive WITH the disk attached (daemon order).
        let mut d2 = Drive1541::new();
        d2.load_rom(Path::new(ROM_DIR)).expect("load DOS ROM");
        d2.cold_reset();
        d2.run_cycles(1_200_000);
        d2.attach_disk(DiskImage {
            kind: DiskKind::D64,
            bytes: d64.clone(),
            backing_path: Some(SAMPLE.to_string()),
            read_only: false,
        });
        restore_drive1541(&mut d2, &blob).expect("restore drive1541");
        restore_drive_disk_image(&mut d2, &disk_blob).expect("restore disk image");

        assert_eq!(d2.rotation.current_half_track, half_track, "half_track");
        assert_eq!(d2.rotation.gcr_head_offset, head, "GCR head offset");
        assert_eq!(
            d2.rotation.speed_zone, expected_speed_zone,
            "speed_zone (VIA-undump re-derived, VICE order)"
        );
        assert_eq!(
            d2.rotation.gcr_current_track_size, track_size,
            "active track size"
        );
        assert_eq!(d2.core.reg_pc, pc, "drive PC");
        let restored_byte = d2.rotation.image.as_ref().unwrap().tracks[cur_slot]
            .data
            .get(100)
            .copied();
        assert_eq!(restored_byte, sample_byte, "GCR track byte 100 survived");

        // Resume: the restored drive runs without jamming and the PC advances
        // (a runnable resume, not a frozen/garbage state).
        d2.run_cycles(500_000);
        assert!(!d2.core.is_jammed, "restored drive must not jam on resume");
    }
}
