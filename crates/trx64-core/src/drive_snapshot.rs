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
//!   - DRIVECPU0 is 1.4: the 1.3 fields and the 0x800 RAM, then the drive CPU's
//!     interrupt status — VICE's `interrupt_write_snapshot` fields (irq_clk,
//!     nmi_clk, irq_pending_clk, num_last_stolen_cycles, last_stolen_cycles_clk)
//!     followed by `interrupt_write_new_snapshot`'s (nirq, nnmi, global_pending_int).
//!     VICE writes the first group before the RAM and the second after it
//!     (drivecpu.c:599/629); here both sit at the END of the module, so a reader
//!     from before 1.4 — which finds the module by name and skips to its recorded
//!     size on close — reads the RAM where it always was and never sees them. The
//!     byte layout is therefore NOT VICE's; the fields and their meaning are.
//!     Until 1.4 the block was left out (the c64re facade this was ported from had
//!     the interrupt snapshot hooks as no-ops), and a restored drive left an
//!     uninterrupted one within frames. A 1.3 module restores with the drive's
//!     interrupt status reset.
//!   - GCRIMAGE0 is 3.2: one byte appended, `complicated_image_loaded` (which
//!     rotation engine runs), which VICE neither saves nor restores (it forces 1).
//!   - The capture catches the lazy rotation up to the drive clock first, so the
//!     VIA2 undump on restore has no lag to rotate in the wrong read/write mode.
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

/// drivecpu.c:565-566 — SNAP_MAJOR / SNAP_MINOR (1.3 added cpu_last_data). 1.4 is
/// TRX64's: the interrupt status appended at the end of the module (VICE is at 1.3
/// with the same fields in the middle — see the module doc).
const DRIVECPU_SNAP_MAJOR: u8 = 1;
const DRIVECPU_SNAP_MINOR: u8 = 4;
/// The first DRIVECPU minor that carries the interrupt status.
const DRIVECPU_SNAP_MINOR_INT: u8 = 4;

/// drive-snapshot.c:857-858 — GCRIMAGE_SNAP_MAJOR / _MINOR. 3.2 is TRX64's: one
/// byte appended at the end of the module, `complicated_image_loaded` (see
/// `restore_drive_disk_image`).
const GCRIMAGE_SNAP_MAJOR: u8 = 3;
const GCRIMAGE_SNAP_MINOR: u8 = 2;
/// The first GCRIMAGE minor that carries `complicated_image_loaded`.
const GCRIMAGE_SNAP_MINOR_ENGINE: u8 = 2;

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

/// Spec 863 D6 — the DRIVE module's `MachineVideoStandard` for the machine the drive is in,
/// read off the drive's own catch-up ratio (the model sets both together). PAL keeps the
/// c64re facade's 0; the others are VICE's `MACHINE_SYNC_*` (NTSC 2, old NTSC 3, PAL-N 4).
fn machine_sync_of(drive: &Drive1541) -> u32 {
    use crate::model::VideoStandard;
    crate::model::models()
        .iter()
        .find(|m| m.runs() && m.timing.drive_sync_factor == drive.sync_factor)
        .map(|m| match m.video {
            VideoStandard::Pal => MACHINE_SYNC_PAL,
            v => v.vice_sync(),
        })
        .unwrap_or(MACHINE_SYNC_PAL)
}

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
    if drive.board_1581().is_some() {
        return capture_1581(drive);
    }
    // The rotation runs lazily: it catches up to the drive clock at the next VIA2
    // access. A restore catches it up in the VIA2 undump (undump_pcr → rotate_disk)
    // — but at that point the read/write mode, motor and speed zone are still the
    // restoring machine's, so the lag would be rotated in the wrong mode (a drive
    // writing when captured read its lag back instead: the restored disk and head
    // parted from the straight run). Catching the rotation up here first leaves no
    // lag for the undump to rotate. It is the rotation any VIA2 access would do now;
    // the rotation is the same whether it runs in one stretch or two.
    drive.snapshot_catch_up_rotation();
    let mut s = SnapshotT::create_in_memory();
    write_drive_module(drive, &mut s);
    // VICE drive_snapshot_write_module walks ALL NUM_DISK_UNITS (drives 8..11) and
    // emits a DRIVE<n> chunk for each — full for the live unit 0 (DRIVE8, has_tde=1),
    // a has_tde=0/has_drives=0 STUB for the absent units 1..3 (drive_snapshot.ts
    // :402-419 ≡ drive-snapshot.c:182-210). TRX64 emitted only DRIVE8, so a saved VSF
    // was 72 bytes short of the TS authority (3 × 24-byte stubs) — formats-state-1
    // length divergence. Emit the stubs to restore byte-for-byte module symmetry.
    // (Spec 612 PL-9: write VICE-format module chunks, not a TRX64-shaped subset.)
    write_drive_stub_module(&mut s, 9);
    write_drive_stub_module(&mut s, 10);
    write_drive_stub_module(&mut s, 11);
    write_drivecpu_module(drive, &mut s);
    write_via_modules(drive, &mut s);
    s.to_bytes()
}

/// Write a DRIVE<n> module for a present-but-TDE-OFF disk unit — the symmetry chunk
/// VICE/TS emit for every NUM_DISK_UNITS slot. In the c64re TS authority units 9..11
/// are non-null diskunit objects with `Drive%iTrueEmulation` OFF, so the full branch
/// runs but stops after the two header bytes: has_tde=0, has_drives=1 (single drive,
/// `drive_is_dualdrive_by_devnr ? 2 : 1` ⇒ 1). With has_tde=0 NO further fields are
/// written, so the chunk is exactly the 22-byte header + `00 01` = 24 bytes
/// (drive_snapshot.ts:422-444). `n` is the drive number (9/10/11). (Spec 612 PL-9.)
fn write_drive_stub_module(s: &mut SnapshotT, n: u32) {
    let mut m = s.module_create(&format!("DRIVE{n}"), DRIVE_SNAP_MAJOR, DRIVE_SNAP_MINOR);
    s.smw_b(&mut m, 0); // has_tde   = 0 (Drive<n>TrueEmulation off)
    s.smw_b(&mut m, 1); // has_drives = 1 (single drive present)
    s.module_close(&m);
}

/// Restore the live drive from a `drive1541` blob (= c64re `drive1541.restore()`).
/// Returns Ok on success; Err(reason) on a malformed/incompatible blob.
pub fn restore_drive1541(drive: &mut Drive1541, blob: &[u8]) -> Result<(), String> {
    // Spec 872 — the blob says which board it was taken from (the DRIVE module's `type`);
    // the position holds that board afterwards.
    if blob_drive_type(blob) == Some(DRIVE_TYPE_1581) {
        return restore_1581(drive, blob);
    }
    if drive.board_1581().is_some() {
        drive.force_board_type(crate::iec::DriveType::Drive1541);
    }
    let mut s = SnapshotT::open_in_memory(blob);
    // VICE drive_snapshot_read_module order: the DRIVE module (GCR_head_offset read
    // straight into the drive, drive-snapshot.c:436; the snap_* rotation fields into
    // the rotation by rotation_table_set, :479), then DRIVECPU, then the VIAs. The
    // VIA2 undump (undump_pcr → via2d_update_pcr) calls rotation_rotate_disk, which
    // advances the head from the restored offset up to the restored drive clock —
    // exactly the rotation the uninterrupted drive does lazily at its next access.
    // So the head must be in place BEFORE the VIA read: setting it afterwards (as
    // this did until the DRIVECPU 1.4 change) threw that advance away while keeping
    // the advanced rotation clock, and the restored head ran behind.
    // VICE calls drive_set_half_track only at the very end (:617), so its undump
    // rotate reads from whatever track was current before the restore; here the
    // track is set first too, so that rotate reads the captured track.
    let head = read_drive_module(drive, &mut s)?;
    if let Some((half_track, gcr_head_offset)) = head {
        // drive_set_half_track re-resolves the active track size + GCR_track_start_ptr
        // (and rescales the old offset, which the saved one then replaces).
        drive.rotation.set_half_track(half_track);
        drive.rotation.gcr_head_offset = gcr_head_offset;
    }
    let carried_int = read_drivecpu_module(drive, &mut s)?;
    read_via_modules(drive, &mut s)?;
    if carried_int {
        // interrupt_restore_irq's pending_int half (the VIA reads just set the level).
        // Without the module's status the reset `pending_int` stays clear, so the next
        // instruction boundary takes each asserted source as a fresh edge.
        drive.snapshot_restore_pending_int();
    }
    drive.snapshot_sync_drive_clk();
    drive.snapshot_clear_pending_reset();
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
    // sync_factor (MachineVideoStandard) — the machine's standard.
    s.smw_dw(&mut m, machine_sync_of(drive));

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
/// caller sets via `set_half_track` before the VIA read (see `restore_drive1541`).
/// Returns `Some((half_track, gcr_head_offset))` for that step, or `None` when the
/// dump carried no true-drive-emulation (has_tde=0).
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
    // MachineVideoStandard: the drive's ratio follows the MACHINE's row, which the
    // checkpoint restore already applied — the record here is informational.
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
    // gcr_head_offset), which the caller sets next, before the VIA read. side handling (drive-snapshot.c:607-616) is 1571
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
// DRIVECPU<n> module — drivecpu.c:568-640 / :642-737
// =============================================================================
//
// 1.3 (drivecpu.c:540-562 without its interrupt blocks): CLOCK clk; B a,x,y,sp;
// W pc; B status; DW last_opcode_info; CLOCK last_clk, cycle_accum,
// last_exc_cycles, stop_clk; B cpu_last_data; ARRAY drive RAM (0x800, 1541).
// 1.4 appends the interrupt status (interrupt.c:385-410):
//   CLOCK irq_clk, nmi_clk, irq_pending_clk, num_last_stolen_cycles,
//         last_stolen_cycles_clk                       (interrupt_write_snapshot)
//   DW    nirq, nnmi, global_pending_int               (interrupt_write_new_snapshot)

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

    // 1.4 — the interrupt status, at the end (module doc). The drive CPU has no DMA,
    // so nothing ever steals its cycles: num_last_stolen_cycles and
    // last_stolen_cycles_clk keep interrupt_cpu_status_reset's 0 (only dma.c sets
    // them, for the main CPU) and TRX64's IntStatus has no field for them.
    let int = &drive.int;
    s.smw_clock(&mut m, int.irq_clk);
    s.smw_clock(&mut m, int.nmi_clk);
    s.smw_clock(&mut m, int.irq_pending_clk);
    s.smw_clock(&mut m, 0); // num_last_stolen_cycles
    s.smw_clock(&mut m, 0); // last_stolen_cycles_clk
    s.smw_dw(&mut m, int.nirq);
    s.smw_dw(&mut m, int.nnmi);
    s.smw_dw(&mut m, int.global_pending_int);

    s.module_close(&m);
}

/// Read DRIVECPU0 into the drive. Returns whether the module carried the interrupt
/// status (1.4 on); without it the status is reset.
fn read_drivecpu_module(drive: &mut Drive1541, s: &mut SnapshotT) -> Result<bool, String> {
    let (m, major, minor) = s
        .module_open("DRIVECPU0")
        .ok_or("drive_snapshot: DRIVECPU0 module missing")?;
    // A major we do not know is a layout we cannot read. A newer minor of major 1
    // only appends (the rule 1.4 follows), so its known prefix is read and the rest
    // skipped by `module_close`.
    if major != DRIVECPU_SNAP_MAJOR {
        return Err(format!(
            "drive_snapshot: DRIVECPU0 module version {major}.{minor} — this reader knows major {DRIVECPU_SNAP_MAJOR} only"
        ));
    }

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

    // 1.4 — the interrupt status. interrupt_read_snapshot first clears what it does
    // not carry (pending_int[], global_pending_int, nirq, nnmi — reset and trap are
    // not modelled), then reads; interrupt_read_new_snapshot restores the counts and
    // the pending mask. drivecpu_snapshot_read_module has reset the whole status
    // before (interrupt_cpu_status_reset), which is what an older module leaves.
    let carried_int = minor >= DRIVECPU_SNAP_MINOR_INT;
    let int_block = if carried_int {
        let irq_clk = rclk!();
        let nmi_clk = rclk!();
        let irq_pending_clk = rclk!();
        let _num_last_stolen_cycles = rclk!(); // no IntStatus field — the drive never steals
        let _last_stolen_cycles_clk = rclk!();
        let nirq = rdw!();
        let nnmi = rdw!();
        let global_pending_int = rdw!();
        Some((irq_clk, nmi_clk, irq_pending_clk, nirq, nnmi, global_pending_int))
    } else {
        None
    };

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

    // interrupt_cpu_status_reset (IntStatus::new), keeping the opcode-info mirror
    // in step with the core as VICE keeps `last_opcode_info_ptr` across the reset.
    drive.int = crate::drive_6510core::IntStatus::new();
    drive.int.last_opcode_info_ptr = last_opcode_info;
    if let Some((irq_clk, nmi_clk, irq_pending_clk, nirq, nnmi, global_pending_int)) = int_block {
        drive.int.irq_clk = irq_clk;
        drive.int.nmi_clk = nmi_clk;
        drive.int.irq_pending_clk = irq_pending_clk;
        drive.int.nirq = nirq;
        drive.int.nnmi = nnmi;
        drive.int.global_pending_int = global_pending_int;
    }

    Ok(carried_int)
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
    if drive.board_1581().is_some() {
        return capture_image_1581(drive);
    }
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
    // 3.2 — which rotation engine the drive runs, at the end of the module.
    s.smw_b(&mut m, drive.rotation.complicated_image_loaded as u8);

    s.module_close(&m);
    Some(s.to_bytes())
}

/// Restore the mutable disk content onto the live GCR buffer (= c64re
/// `restoreDiskImage`). Overwrites the per-half-track GCR bytes; a no-op return is
/// Ok when the GCRIMAGE0 module is absent (drive kept at its baseline).
pub fn restore_drive_disk_image(drive: &mut Drive1541, blob: &[u8]) -> Result<(), String> {
    if SnapshotT::open_in_memory(blob).module_open(IMAGE_MODULE).is_some() {
        return restore_image_1581(drive, blob);
    }
    let mut s = SnapshotT::open_in_memory(blob);
    let opened = match s.module_open("GCRIMAGE0") {
        Some(o) => o,
        None => return Ok(()), // module absent → keep the baseline image.
    };
    let (m, major, minor) = opened;
    let _ = m;
    // As DRIVECPU0: an unknown major is refused by name; a newer minor of major 3
    // only appends, so its known prefix is read and `module_close` skips the rest.
    // (TRX64 before 3.2 refuses any higher version, 3.2 included — loudly.)
    if major != GCRIMAGE_SNAP_MAJOR {
        return Err(format!(
            "drive_snapshot: GCRIMAGE0 module version {major}.{minor} — this reader knows major {GCRIMAGE_SNAP_MAJOR} only"
        ));
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
    // Which rotation engine runs: a D64 starts on the simple one and switches to the
    // GCR circuit for good at its first write (rotation.c:1098); a G64 runs the
    // circuit from the attach. VICE's drive_snapshot_read_gcrimage_module sets 1
    // unconditionally ("TODO: verify if it's really like this", :983) and does not
    // save it, so a restored D64 drive that had never written ran the other engine
    // and left the uninterrupted drive within a frame. From 3.2 the module carries
    // it; an older one gets VICE's 1.
    let complicated = if minor >= GCRIMAGE_SNAP_MINOR_ENGINE {
        s.smr_b().ok_or("drive_snapshot: GCRIMAGE0 truncated (complicated_image_loaded)")? as i32
    } else {
        1
    };
    s.module_close(&m);

    drive.rotation.gcr_image_loaded = 1;
    drive.rotation.complicated_image_loaded = complicated;
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

// =============================================================================
// Spec 872 §6 — the 1581 blob: DRIVE8 (type 1581), DRIVECPU0 (0x2000 RAM),
// CIA1581D0, WD17700 + FDD0, in VICE's order for DRIVE_TYPE_1581
// (drive-snapshot.c:162-354, iec.c:271-280). Every position's blob uses unit 0's
// names, as the 1541's does.
// =============================================================================

/// VICE `DRIVE_TYPE_1581`.
const DRIVE_TYPE_1581: u32 = 1581;
/// drive-snapshot.c:643-644 IMAGE_SNAP_MAJOR / _MINOR, and the module name for unit 0.
const IMAGE_SNAP_MAJOR: u8 = 1;
const IMAGE_SNAP_MINOR: u8 = 0;
const IMAGE_MODULE: &str = "IMAGE0";

/// The DRIVE module's `type` (and `None` when the dump carries no true drive).
fn blob_drive_type(blob: &[u8]) -> Option<u32> {
    let mut s = SnapshotT::open_in_memory(blob);
    let (_m, _major, _minor) = s.module_open("DRIVE8")?;
    let has_tde = s.smr_b()?;
    if has_tde == 0 {
        return None;
    }
    let _has_drives = s.smr_b()?;
    let _sync = s.smr_dw()?;
    let _attach_clk = s.smr_clock()?;
    let _brl = s.smr_b()?;
    let _cf = s.smr_b()?;
    let _ht = s.smr_w()?;
    let _detach = s.smr_clock()?;
    let _ext = s.smr_b()?;
    let _gho = s.smr_dw()?;
    let _gr = s.smr_b()?;
    let _gw = s.smr_b()?;
    let _idle = s.smr_b()?;
    let _pc = s.smr_b()?;
    let _ro = s.smr_b()?;
    let _rtp = s.smr_dw()?;
    s.smr_dw()
}

/// The DRIVE8 module for a 1581: the field list VICE writes for every drive
/// (drive-snapshot.c:204-272). A 1581 has no GCR rotation, so those fields are the
/// zeros of a rotation that never ran; `clock_frequency` is 2, the half-track word
/// fdd.c's `(track + 1) * 2`.
fn write_drive_module_1581(drive: &Drive1541, s: &mut SnapshotT) {
    let b = drive.board_1581().expect("a 1581 board");
    let mut m = s.module_create("DRIVE8", DRIVE_SNAP_MAJOR, DRIVE_SNAP_MINOR);
    s.smw_b(&mut m, 1); // has_tde
    s.smw_b(&mut m, 1); // has_drives
    s.smw_dw(&mut m, machine_sync_of(drive));
    s.smw_clock(&mut m, 0); // attach_clk
    s.smw_b(&mut m, 0); // byte_ready_level
    s.smw_b(&mut m, crate::drive1581::CLOCK_FREQUENCY_1581 as u8);
    s.smw_w(&mut m, ((b.head().0 as u16) + 1) * 2);
    s.smw_clock(&mut m, 0); // detach_clk
    s.smw_b(&mut m, 0); // extend_image_policy
    s.smw_dw(&mut m, 0); // GCR_head_offset
    s.smw_b(&mut m, 0); // GCR_read
    s.smw_b(&mut m, 0); // GCR_write_value
    s.smw_b(&mut m, 0); // idling_method
    s.smw_b(&mut m, 0); // parallel_cable
    s.smw_b(&mut m, b.read_only as u8);
    s.smw_dw(&mut m, 0); // rotation_table_ptr
    s.smw_dw(&mut m, DRIVE_TYPE_1581);
    s.smw_dw(&mut m, 0); // accum
    s.smw_clock(&mut m, 0); // rotation_last_clk
    for _ in 0..2 {
        s.smw_dw(&mut m, 0); // bit_counter, zero_count
    }
    s.smw_w(&mut m, 0); // last_read_data
    s.smw_b(&mut m, 0); // last_write_data
    for _ in 0..14 {
        // seed, speed_zone, ue7_dcba, ue7_counter, uf4_counter, fr_randcount,
        // filter_counter, filter_state, filter_last_state, write_flux,
        // PulseHeadPosition, xorShift32, so_delay, cycle_index
        s.smw_dw(&mut m, 0);
    }
    s.smw_clock(&mut m, 0); // ref_advance
    s.smw_dw(&mut m, 0); // req_ref_cycles
    s.smw_clock(&mut m, 0); // attach_detach_clk
    s.smw_b(&mut m, 0); // byte_ready_edge
    s.smw_b(&mut m, 0); // byte_ready_active
    s.module_close(&m);
}

/// DRIVECPU0 for a 1581: the 1541's 1.4 layout with the 1581's `0x2000` RAM
/// (drivecpu.c:616-621) and `cpu_last_data`.
fn write_drivecpu_module_1581(drive: &Drive1541, s: &mut SnapshotT) {
    let b = drive.board_1581().expect("a 1581 board");
    let mut m = s.module_create("DRIVECPU0", DRIVECPU_SNAP_MAJOR, DRIVECPU_SNAP_MINOR);
    let c = &b.core;
    s.smw_clock(&mut m, c.clk);
    s.smw_b(&mut m, c.reg_a);
    s.smw_b(&mut m, c.reg_x);
    s.smw_b(&mut m, c.reg_y);
    s.smw_b(&mut m, c.reg_sp);
    s.smw_w(&mut m, c.reg_pc);
    s.smw_b(&mut m, c.status());
    s.smw_dw(&mut m, c.last_opcode_info);
    s.smw_clock(&mut m, c.clk);
    s.smw_clock(&mut m, b.snapshot_sync_accum() as u64);
    s.smw_clock(&mut m, 0);
    s.smw_clock(&mut m, b.snapshot_stop_clk());
    s.smw_b(&mut m, b.cpu_last_data);
    s.smw_ba(&mut m, b.ram(), 0x2000);
    let int = &b.int;
    s.smw_clock(&mut m, int.irq_clk);
    s.smw_clock(&mut m, int.nmi_clk);
    s.smw_clock(&mut m, int.irq_pending_clk);
    s.smw_clock(&mut m, 0);
    s.smw_clock(&mut m, 0);
    s.smw_dw(&mut m, int.nirq);
    s.smw_dw(&mut m, int.nnmi);
    s.smw_dw(&mut m, int.global_pending_int);
    s.module_close(&m);
}

fn capture_1581(drive: &mut Drive1541) -> Vec<u8> {
    let mut s = SnapshotT::create_in_memory();
    // The CIA module brings its timers and delay line up to the drive clock first,
    // as ciacore_snapshot_write_module does; take that before the CPU is written so
    // the interrupt status the CPU module carries includes what it raised.
    let mut cia = SnapshotT::create_in_memory();
    drive.board_1581_mut().unwrap().snapshot_write_cia(&mut cia);
    write_drive_module_1581(drive, &mut s);
    write_drive_stub_module(&mut s, 9);
    write_drive_stub_module(&mut s, 10);
    write_drive_stub_module(&mut s, 11);
    write_drivecpu_module_1581(drive, &mut s);
    let cia_bytes = cia.to_bytes();
    s.buf.truncate(s.pos);
    s.buf.extend_from_slice(&cia_bytes);
    s.pos = s.buf.len();
    // Spec 875 §8 — with a host's controller fitted the blob ends after the CIA: TRX64's
    // WD and mechanism are not on the bus, and the controller's state is the host's
    // (the checkpoint's `hostFdc` node).
    let b = drive.board_1581().unwrap();
    if b.host_fdc().is_none() {
        b.wd.snapshot_write_module(&mut s);
    }
    s.to_bytes()
}

fn restore_1581(drive: &mut Drive1541, blob: &[u8]) -> Result<(), String> {
    drive.force_board_type(crate::iec::DriveType::Drive1581);
    let dnr = (drive.unit() - 8) as usize;
    let mut s = SnapshotT::open_in_memory(blob);
    // DRIVE8: the fields a 1581 uses are its type and the medium's read-only flag;
    // the medium is the host's (mounted before this), so nothing else is taken.
    {
        let (m, major, minor) = s.module_open("DRIVE8").ok_or("drive_snapshot: DRIVE8 module missing")?;
        if snapshot_version_is_bigger(major, minor, DRIVE_SNAP_MAJOR, DRIVE_SNAP_MINOR) {
            return Err("drive_snapshot: DRIVE8 module higher version".into());
        }
        s.module_close(&m);
    }
    let (m, major, minor) = s.module_open("DRIVECPU0").ok_or("drive_snapshot: DRIVECPU0 module missing")?;
    if major != DRIVECPU_SNAP_MAJOR {
        return Err(format!("drive_snapshot: DRIVECPU0 module version {major}.{minor}"));
    }
    macro_rules! r {
        ($e:expr) => {
            $e.ok_or("drive_snapshot: DRIVECPU0 truncated")?
        };
    }
    let clk = r!(s.smr_clock());
    let a = r!(s.smr_b());
    let x = r!(s.smr_b());
    let y = r!(s.smr_b());
    let sp = r!(s.smr_b());
    let pc = r!(s.smr_w());
    let status = r!(s.smr_b());
    let last_opcode_info = r!(s.smr_dw());
    let _last_clk = r!(s.smr_clock());
    let cycle_accum = r!(s.smr_clock());
    let _last_exc = r!(s.smr_clock());
    let stop_clk = r!(s.smr_clock());
    let cpu_last_data = r!(s.smr_b());
    let mut ram = vec![0u8; 0x2000];
    if !s.smr_ba(&mut ram, 0x2000) {
        return Err("drive_snapshot: DRIVECPU0 truncated (RAM)".into());
    }
    let int_block = if minor >= DRIVECPU_SNAP_MINOR_INT {
        let irq_clk = r!(s.smr_clock());
        let nmi_clk = r!(s.smr_clock());
        let irq_pending_clk = r!(s.smr_clock());
        let _ = r!(s.smr_clock());
        let _ = r!(s.smr_clock());
        let nirq = r!(s.smr_dw());
        let nnmi = r!(s.smr_dw());
        let gpi = r!(s.smr_dw());
        Some((irq_clk, nmi_clk, irq_pending_clk, nirq, nnmi, gpi))
    } else {
        None
    };
    s.module_close(&m);

    let b = drive.board_1581_mut().unwrap();
    b.set_number(dnr);
    b.core.clk = clk;
    b.core.reg_a = a;
    b.core.reg_x = x;
    b.core.reg_y = y;
    b.core.reg_sp = sp;
    b.core.reg_pc = pc;
    b.core.set_status_composite(status);
    b.core.last_opcode_info = last_opcode_info;
    b.snapshot_set_sync_accum((cycle_accum & 0xffff_ffff) as u32);
    b.snapshot_set_stop_clk(stop_clk);
    b.cpu_last_data = cpu_last_data;
    b.ram_mut().copy_from_slice(&ram);
    b.int = crate::drive_6510core::IntStatus::new();
    b.int.last_opcode_info_ptr = last_opcode_info;
    if let Some((irq_clk, nmi_clk, irq_pending_clk, nirq, nnmi, gpi)) = int_block {
        b.int.irq_clk = irq_clk;
        b.int.nmi_clk = nmi_clk;
        b.int.irq_pending_clk = irq_pending_clk;
        b.int.nirq = nirq;
        b.int.nnmi = nnmi;
        b.int.global_pending_int = gpi;
    }
    b.snapshot_read_cia(&mut s)?;
    // Spec 875 §8 — a blob written with a host's controller fitted ends after the CIA;
    // the restore has checked that the live position has the same controller.
    if b.host_fdc().is_none() {
        b.wd.snapshot_read_module(&mut s)?;
    }
    b.resync_iec_output();
    b.drive_clk = b.core.clk;
    b.snapshot_clear_pending_reset();
    let dc = b.drive_clk;
    drive.drive_clk = dc;
    Ok(())
}

/// The `IMAGE0` module (drive-snapshot.c:656-721): the type word, then the D81 as
/// written — the sectors, and the error block when the mounted image has one (a
/// TRX64 extension appended after VICE's layout; a module's size bounds it).
fn capture_image_1581(drive: &Drive1541) -> Option<Vec<u8>> {
    let d = drive.disk_as_written()?;
    let mut s = SnapshotT::create_in_memory();
    let mut m = s.module_create(IMAGE_MODULE, IMAGE_SNAP_MAJOR, IMAGE_SNAP_MINOR);
    s.smw_w(&mut m, DRIVE_TYPE_1581 as u16);
    s.smw_ba(&mut m, &d.bytes, d.bytes.len());
    s.module_close(&m);
    Some(s.to_bytes())
}

/// Put the D81 an `IMAGE0` module carries into the position's medium, without touching
/// the mechanism (the FDD module restored the head, the latch and the resident track).
fn restore_image_1581(drive: &mut Drive1541, blob: &[u8]) -> Result<(), String> {
    let mut s = SnapshotT::open_in_memory(blob);
    let (m, major, minor) = s.module_open(IMAGE_MODULE).ok_or("drive_snapshot: IMAGE0 missing")?;
    if major != IMAGE_SNAP_MAJOR {
        return Err(format!("drive_snapshot: IMAGE0 module version {major}.{minor}"));
    }
    let t = s.smr_w().ok_or("drive_snapshot: IMAGE0 truncated")?;
    if t as u32 != DRIVE_TYPE_1581 {
        return Err(format!("drive_snapshot: IMAGE0 carries a type {t} image; this reader knows 1581"));
    }
    let len = (m.size as usize).saturating_sub(16 + 1 + 1 + 4 + 2);
    if crate::fdd::d81_geometry(len).is_none() {
        return Err(format!("drive_snapshot: IMAGE0 carries {len} bytes, not a D81 size"));
    }
    let mut bytes = vec![0u8; len];
    if !s.smr_ba(&mut bytes, len) {
        return Err("drive_snapshot: IMAGE0 truncated (sectors)".into());
    }
    s.module_close(&m);
    drive.restore_medium_1581(bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const ROM_DIR: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
    const SAMPLE: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/samples/scramble_infinity.d64");

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
        // Read after the capture: it catches the lazy rotation up to the drive clock,
        // so the head the blob describes is the one the drive has now.
        let head = drive.rotation.gcr_head_offset;
        let half_track = drive.rotation.current_half_track;
        let track_size = drive.rotation.gcr_current_track_size;

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
