//! rotation.rs — 1:1 port of the c64re TS oracle's `rotation.ts`
//! (= VICE `src/drive/rotation.c`), the rotating GCR disk engine.
//!
//! PORTED 1:1, NOT DISTILLED. Every `rotation_*` function in `rotation.ts`
//! maps to one method/function here with the SAME name (snake_case), the SAME
//! field names, the SAME branch order, the SAME u32/u64 widths and accum/carry
//! math — line-for-line. The functions the previous DISTILLED version omitted
//! (`rotation_1541_gcr`, `rotation_1541_gcr_cycle`, `rotation_do_wobble`, the
//! simple-engine WRITE path, `rotation_table_get/_set`, the P64 stubs,
//! `read_next_bit`/`write_next_bit`, `RANDOM_next*`) are all present here.
//!
//! Struct bridging (the one unavoidable adaptation, documented): the TS keeps
//! two separate aggregates the engine touches — the module-level
//! `rotation[dnr]: rotation_t` (the bit-cell state) and the per-drive
//! `dptr: drive_t` (the head/disk/byte-ready state). VICE/TS pass `dptr` and
//! index `rotation[dnr]` inside each fn. In this single-drive port (NUM_DISK_UNITS
//! effectively 1) both aggregates live in ONE `Rotation` struct: the `rotation_t`
//! fields and the `drive_t` fields the engine reaches are co-located. Field NAMES
//! are kept verbatim from the TS where the engine reads them (snake_case
//! rotation_t names; `gcr_*` lower-cased `GCR_*` drive_t names). The
//! `GCR_track_start_ptr` pointer is modelled as a resolution of
//! `image.tracks[current_half_track-2]` performed where the TS reads
//! `dptr.GCR_track_start_ptr` (set by `drive_set_half_track`).
//!
//! The public method/field surface (`rotate_disk`, `byte_read`, `sync_found`,
//! `pra_pin`, `writeprotect_sense`, `move_head`, `speed_zone_set`, `begins`,
//! `attach`, `detach`; fields `byte_ready_active`, `byte_ready_edge`,
//! `byte_ready_level`, `read_write_mode`, `current_half_track`, `image`) is the
//! contract `viacore.rs` (the 1:1 VIA core) and `drive.rs` wire to — kept stable.

// The redundant `& 0xff` / `& 0xffff_ffff` masks and the `.max(2)` head clamp are
// the verbatim TS/VICE expressions; clippy's "no effect" / "clamp-like" lints
// would rewrite them away, breaking the line-for-line fidelity. Allowed per the
// viacore.rs precedent (the 1:1 VIA core uses the same module-scoped allows).
#![allow(clippy::identity_op)]
#![allow(clippy::manual_clamp)]
#![allow(clippy::doc_lazy_continuation)]

use crate::gcr::{GcrImage, GcrTrack, WritebackKind, DRIVE_HALFTRACKS_1541};

// =============================================================================
// SECTION 1 — header constants (NL-4)
// =============================================================================

// PORT OF: rotation.ts:77 (rotation.h:35) — `#define BUS_READ_DELAY 14`.
// 875 ns delay (14 × 62.5 ns) for data-bus read access.
pub const BUS_READ_DELAY: i32 = 14;

// ── byte_ready_active bits (drivetypes.ts:161-163 / drive.h) ────────────────
pub const BRA_BYTE_READY: u8 = 0x02;
pub const BRA_MOTOR_ON: u8 = 0x04;

// ── DRIVE_ATTACH_* settle delays (drivetypes.ts:128-130) ────────────────────
/// Settle delay after a fresh attach during which `rotation_byte_read` forces
/// `GCR_read = 0` (DRIVE_ATTACH_DELAY = 3*600000).
pub const DRIVE_ATTACH_DELAY: u64 = 3 * 600_000;
/// Settle delay after a re-attach following a recent detach.
pub const DRIVE_ATTACH_DETACH_DELAY: u64 = 6 * 600_000;

// =============================================================================
// SECTION 2 — file-private constants (rotation.c:43,45)
// =============================================================================

// PORT OF: rotation.ts:84 (rotation.c:43 ACCUM_MAX).
#[allow(dead_code)]
const ACCUM_MAX: u32 = 0x10000;
// PORT OF: rotation.ts:86 (rotation.c:45 ROTATION_TABLE_SIZE).
#[allow(dead_code)]
const ROTATION_TABLE_SIZE: u32 = 0x1000;

// PORT OF: rotation.ts:199-203 (rotation.c:89-90 rot_speed_bps[2][4]).
// Speed (bps) of the disk in the 4 disk areas, indexed by [frequency][zone].
const ROT_SPEED_BPS: [[u32; 4]; 2] = [
    [250_000, 266_667, 285_714, 307_692],
    [125_000, 133_333, 142_857, 153_846],
];

/// 1541 half-track count for the `drive_set_half_track` clamp.
const DRIVE_HALFTRACKS_1541_U32: u32 = DRIVE_HALFTRACKS_1541 as u32;

// =============================================================================
// SECTION 3 — rotation_t + drive_t (the head/disk fields the engine touches)
// =============================================================================

/// PORT OF: rotation.ts:97-142 (rotation.c:48-83 `rotation_s`) + the subset of
/// `drive_t` (drivetypes.ts:503-619) the rotation engine reads/writes. One Rust
/// struct holds both aggregates for this single-drive port (see module doc).
#[derive(Clone)]
pub struct Rotation {
    // ── rotation_t (rotation.ts:97-142) ──────────────────────────────────────
    /// rotation.c:49 — uint32_t accum;
    pub accum: u32,
    /// rotation.c:50 — CLOCK rotation_last_clk;
    pub rotation_last_clk: u64,
    /// rotation.c:52 — unsigned int last_read_data;
    pub last_read_data: u32,
    /// rotation.c:53 — uint8_t last_write_data;
    pub last_write_data: u8,
    /// rotation.c:54 — int bit_counter;
    pub bit_counter: i32,
    /// rotation.c:55 — int zero_count;
    pub zero_count: i32,
    /// rotation.c:57 — int frequency; (1x/2x speed toggle)
    pub frequency: usize,
    /// rotation.c:58 — int speed_zone;
    pub speed_zone: usize,
    /// rotation.c:60 — int ue7_dcba; (UE7 BA counter input)
    pub ue7_dcba: i32,
    /// rotation.c:61 — int ue7_counter;
    pub ue7_counter: i32,
    /// rotation.c:62 — int uf4_counter;
    pub uf4_counter: i32,
    /// rotation.c:63 — uint32_t fr_randcount;
    pub fr_randcount: u32,
    /// rotation.c:65 — int filter_counter;
    pub filter_counter: i32,
    /// rotation.c:66 — int filter_state;
    pub filter_state: i32,
    /// rotation.c:67 — int filter_last_state;
    pub filter_last_state: i32,
    /// rotation.c:69 — int write_flux;
    pub write_flux: i32,
    /// rotation.c:71 — int so_delay;
    pub so_delay: i32,
    /// rotation.c:73 — uint32_t cycle_index;
    pub cycle_index: u32,
    /// rotation.c:75 — CLOCK ref_advance;
    pub ref_advance: u64,
    /// rotation.c:77 — uint32_t PulseHeadPosition;
    pub pulse_head_position: u32,
    /// rotation.c:79 — uint32_t seed;
    pub seed: u32,
    /// rotation.c:81 — uint32_t xorShift32;
    pub xor_shift32: u32,

    // ── drive_t head/disk fields (drivetypes.ts:503-619) ─────────────────────
    /// drivetypes.ts:531 — GCR_head_offset (head bit offset into the track).
    pub gcr_head_offset: u32,
    /// drivetypes.ts:504 — current_half_track (2..=84). Power-on 36 (T18).
    pub current_half_track: u32,
    /// drivetypes.ts:511 — side.
    pub side: i32,
    /// drivetypes.ts:528 — GCR_current_track_size (bytes).
    pub gcr_current_track_size: usize,
    /// drivetypes.ts:585 — GCR_image_loaded.
    pub gcr_image_loaded: i32,
    /// drivetypes.ts:582 — complicated_image_loaded (1 ⇒ GCR engine).
    pub complicated_image_loaded: i32,
    /// drivetypes.ts:588 — P64_image_loaded.
    pub p64_image_loaded: i32,
    /// drivetypes.ts:518 — GCR_dirty_track.
    pub gcr_dirty_track: i32,
    /// drivetypes.ts:521 — GCR_write_value (byte being written; store_pra sink).
    pub gcr_write_value: u8,
    /// drivetypes.ts:534 — read_write_mode (0 = write, !=0 = read). bool: true=read.
    pub read_write_mode: bool,
    /// drivetypes.ts:537 — byte_ready_active (BRA_* bitmask).
    pub byte_ready_active: u8,
    /// drivetypes.ts:547 — GCR_read (assembled byte presented on VIA2 PRA).
    pub gcr_read: u8,
    /// drivetypes.ts:515 — byte_ready_edge (rising-edge → drive 6502 SO/V).
    pub byte_ready_edge: u8,
    /// drivetypes.ts:514 — byte_ready_level.
    pub byte_ready_level: u8,
    /// drivetypes.ts:540 — attach_clk (0 = settled).
    pub attach_clk: u64,
    /// drivetypes.ts:544 — attach_detach_clk.
    pub attach_detach_clk: u64,
    /// drivetypes.ts:574 — req_ref_cycles (IF: requested additional R cycles).
    pub req_ref_cycles: u64,
    /// drivetypes.ts:612 — rpm (300rpm = 30000).
    pub rpm: u64,
    /// drivetypes.ts:615 — wobble_sin_count.
    pub wobble_sin_count: f64,
    /// drivetypes.ts:616 — wobble_factor.
    pub wobble_factor: i32,
    /// drivetypes.ts:617 — wobble_frequency.
    pub wobble_frequency: f64,
    /// drivetypes.ts:618 — wobble_amplitude.
    pub wobble_amplitude: f64,
    /// drivetypes.ts:594 — read_only.
    pub read_only: i32,
    /// drivetypes.ts:603/606 — `image`/`gcr` (= the encoded per-track GCR data).
    /// Models `dptr.gcr` (non-null ⇒ a GCR image attached). `None` = no disk.
    pub image: Option<GcrImage>,

    // ── write-back target (the raw on-disk image bytes) ──────────────────────
    /// The raw `.g64`/`.d64` image bytes the dirty GCR track is serialized back
    /// into (= VICE `fsimage->fd`). Mirrors `DiskImage.bytes`; populated at
    /// [`attach`]. `None` ⇒ no write-back target (the in-memory GCR is the only
    /// copy, e.g. a snapshot-restored disk before its bytes are wired).
    pub writeback_bytes: Option<Vec<u8>>,
    /// Image format for the write-back dispatch (G64 byte serialization vs D64
    /// sector decode). `None` when `writeback_bytes` is `None`.
    pub writeback_kind: Option<WritebackKind>,
    /// The half-track currently marked dirty by [`write_next_bit`] — the head
    /// position at the time of the write. The flush ([`drive_gcr_data_writeback`])
    /// serializes THIS half-track (= VICE `drive_gcr_data_writeback` flushing
    /// `current_half_track` at the moment the head is about to move).
    pub dirty_half_track: u32,

    /// Spec 784 loader-lens — NON-VICE, PASSIVE instrumentation. Monotonic count of
    /// GCR data bytes the drive has latched off the disk surface (bumped once per
    /// [`byte_read`], the via2d `read_pra`/`GCR_read` path). Read ONLY by the
    /// loader-lens block-read lane to attribute a sector as physically consumed
    /// (read-set truth) — never consulted by the emulation logic, so it causes ZERO
    /// behavioural divergence from VICE. Not a `drive_t` field; `#[derive(Clone)]`
    /// carries it (only deltas are used, so a cloned value is harmless) and the
    /// explicit snapshot serializer (`drive_snapshot.rs`) never touches it, so it has
    /// no snapshot-parity impact.
    pub gcr_read_count: u64,
}

impl Default for Rotation {
    fn default() -> Self {
        Self::new()
    }
}

impl Rotation {
    /// Power-on state: rotation_init (rotation.ts:209-224) + the drive_t init
    /// subset (drive.ts:1819-1833) the engine reads. Head parks at track 18
    /// (half-track 36) per drive_set_half_track(36) in drive.ts:555.
    pub fn new() -> Self {
        Self {
            // rotation_t — rotation_init (rotation.ts:209-224).
            accum: 0,
            rotation_last_clk: 0,
            last_read_data: 0,
            last_write_data: 0,
            bit_counter: 0,
            zero_count: 0,
            frequency: 0,
            speed_zone: 0,
            ue7_dcba: 0,
            ue7_counter: 0,
            uf4_counter: 0,
            fr_randcount: 0,
            filter_counter: 0,
            filter_state: 0,
            filter_last_state: 0,
            write_flux: 0,
            so_delay: 0,
            cycle_index: 0,
            ref_advance: 0,
            pulse_head_position: 0,
            seed: 0,
            xor_shift32: 0x1234abcd,
            // drive_t — drive.ts:1819-1833 init subset.
            gcr_head_offset: 0,
            current_half_track: 36,
            side: 0,
            gcr_current_track_size: 0,
            gcr_image_loaded: 0,
            complicated_image_loaded: 0,
            p64_image_loaded: 0,
            gcr_dirty_track: 0,
            gcr_write_value: 0,
            read_write_mode: true,
            byte_ready_active: BRA_BYTE_READY | BRA_MOTOR_ON,
            gcr_read: 0,
            byte_ready_edge: 0,
            byte_ready_level: 0,
            attach_clk: 0,
            attach_detach_clk: 0,
            req_ref_cycles: 0,
            rpm: 30_000,
            wobble_sin_count: 0.0,
            wobble_factor: 0,
            wobble_frequency: 0.0,
            wobble_amplitude: 0.0,
            read_only: 0,
            image: None,
            writeback_bytes: None,
            writeback_kind: None,
            dirty_half_track: 0,
            gcr_read_count: 0,
        }
    }

    // =========================================================================
    // SECTION 5 — public entry points (rotation_*)
    // =========================================================================

    /// PORT OF: rotation.ts:227-250 (rotation.c:111-137 rotation_reset). Clear
    /// shifters/accumulator, re-anchor the last-clock. Keeps the image + track.
    pub fn reset(&mut self, clk: u64) {
        self.last_read_data = 0;
        self.last_write_data = 0;
        self.bit_counter = 0;
        self.accum = 0;
        self.seed = 0;
        self.xor_shift32 = 0x1234abcd;
        self.rotation_last_clk = clk;
        self.ue7_counter = 0;
        self.uf4_counter = 0;
        self.fr_randcount = 0;
        self.filter_counter = 0;
        self.filter_state = 0;
        self.filter_last_state = 0;
        self.write_flux = 0;
        self.pulse_head_position = 0;
        self.so_delay = 0;
        self.cycle_index = 0;
        self.ref_advance = 0;

        self.req_ref_cycles = 0;
    }

    /// PORT OF: rotation.ts:253-256 (rotation.c:139-143 rotation_speed_zone_set).
    pub fn speed_zone_set(&mut self, zone: usize) {
        self.speed_zone = zone;
        self.ue7_dcba = (zone & 3) as i32;
    }

    /// PORT OF: rotation.ts:353-355 (rotation.c:222-225 rotation_overflow_callback).
    pub fn overflow_callback(&mut self, sub: u64) {
        self.rotation_last_clk = self.rotation_last_clk.wrapping_sub(sub);
    }

    /// PORT OF: rotation.ts:361-389 (rotation.c:227-254 write_next_bit).
    /// C `inline static` → method here (the engine's only caller).
    fn write_next_bit(&mut self, value: u8) {
        let mut off = self.gcr_head_offset;
        let byte_offset = (off >> 3) as usize;
        let bit = (!off) & 7;

        // if no image is attached, writes do nothing
        if self.gcr_image_loaded == 0 {
            return;
        }

        off = off.wrapping_add(1);
        if off >= (self.gcr_current_track_size as u32) << 3 {
            off = 0;
        }
        self.gcr_head_offset = off;

        // track does not exist
        let idx = (self.current_half_track as usize).wrapping_sub(2);
        let track_present = self
            .image
            .as_ref()
            .and_then(|img| img.tracks.get(idx))
            .is_some();
        if !track_present {
            return;
        }
        self.gcr_dirty_track = 1;
        // Record WHICH half-track was dirtied so the flush
        // (`drive_gcr_data_writeback`) serializes the right one. VICE keeps this
        // implicit via `current_half_track`; we snapshot it here because a write
        // at the head edge can straddle a step.
        self.dirty_half_track = self.current_half_track;
        if let Some(trk) = self
            .image
            .as_mut()
            .and_then(|img| img.tracks.get_mut(idx))
        {
            if let Some(b) = trk.data.get_mut(byte_offset) {
                if value != 0 {
                    *b = (*b | (1 << bit)) & 0xff;
                } else {
                    *b = (*b & !(1 << bit)) & 0xff;
                }
            }
        }
    }

    /// PORT OF: rotation.ts:394-415 (rotation.c:256-278 read_next_bit).
    fn read_next_bit(&mut self) -> u32 {
        let mut off = self.gcr_head_offset;
        let byte_offset = (off >> 3) as usize;
        let bit = (!off) & 7;

        // if no image is attached, read 0
        if self.gcr_image_loaded == 0 {
            return 0;
        }

        off = off.wrapping_add(1);
        if off >= (self.gcr_current_track_size as u32) << 3 {
            off = 0;
        }
        self.gcr_head_offset = off;

        // track does not exist
        let idx = (self.current_half_track as usize).wrapping_sub(2);
        match self.image.as_ref().and_then(|img| img.tracks.get(idx)) {
            None => 0,
            Some(trk) => {
                ((*trk.data.get(byte_offset).unwrap_or(&0) as u32) >> bit) & 1
            }
        }
    }

    /// PORT OF: rotation.ts:422-427 (rotation.c:280-286 RANDOM_nextInt).
    #[allow(dead_code)]
    fn random_next_int(&mut self) -> i32 {
        let bits = self.seed >> 15;
        self.seed ^= self.accum;
        self.seed = (self.seed << 17) | bits;
        self.seed as i32
    }

    /// PORT OF: rotation.ts:432-437 (rotation.c:288-293 RANDOM_nextUInt).
    fn random_next_uint(&mut self) -> u32 {
        self.xor_shift32 ^= self.xor_shift32 << 13;
        self.xor_shift32 ^= self.xor_shift32 >> 17;
        self.xor_shift32 ^= self.xor_shift32 << 5;
        self.xor_shift32
    }

    /// PORT OF: rotation.ts:440-444 (rotation.c:295-305 rotation_begins).
    pub fn begins(&mut self, clk: u64) {
        self.rotation_last_clk = clk;
        self.cycle_index = 0;
    }

    /// PORT OF: rotation.ts:458-476 (rotation.c:307-333 rotation_do_wobble).
    /// C `static`. Active #else branch only; the disabled `#if 0` lib_rand
    /// wobble path is not ported (it is `#if 0` in VICE too). For PAL D64 the
    /// wobble amplitude/frequency are 0, so `wobble_factor` stays 0.
    fn rotation_do_wobble(&mut self, clk: u64) {
        // cpu cycles since last call — rotation.ts:460-462.
        let cpu_cycles = clk.wrapping_sub(self.rotation_last_clk);

        // rotation.ts:465-470.
        let two_pi = 2.0 * std::f64::consts::PI;
        self.wobble_sin_count +=
            self.wobble_frequency * ((cpu_cycles as f64 * two_pi) / 1_000_000_000.0);
        if self.wobble_sin_count > two_pi {
            self.wobble_sin_count -= two_pi;
        }
        // rotation.ts:472-475 — `sinf()` single-precision; `(int)` truncates.
        let sin_f = (self.wobble_sin_count.sin() as f32) as f64;
        self.wobble_factor =
            (0.5 + (sin_f * (self.wobble_amplitude * 32.0)) / 3.0).trunc() as i32;
    }

    /// Resolve the current track (= `dptr.GCR_track_start_ptr` / its size). The
    /// TS keeps this as a pointer set by `drive_set_half_track`; here it is
    /// resolved on demand at the same point the TS reads it.
    #[inline]
    fn current_track(&self) -> Option<&GcrTrack> {
        let idx = (self.current_half_track as usize).wrapping_sub(2);
        self.image.as_ref().and_then(|img| img.tracks.get(idx))
    }

    /// PORT OF: ws-server.ts:60-79 (`viceSectorUnderHead`). Decode the physical
    /// sector under (or next approaching) the GCR read head. Loader-independent
    /// (works for KERNAL + custom fastloaders) — reads the actual GCR track,
    /// scans from the head bit-position for the next sector header block, returns
    /// its sector number. Mirrors VICE's monitor sector indicator. Returns `-1`
    /// if no header is found (unformatted/empty track).
    ///
    /// The TS reads `d0.gcr.tracks[ht - 2]` (`ht = current_half_track`),
    /// `d0.GCR_head_offset` (a BIT position), and calls `gcr_find_sync` /
    /// `gcr_decode_block` from `gcr.ts`. Here those map to `self.image.tracks[
    /// current_half_track - 2]`, `self.gcr_head_offset`, and the crate-private
    /// `gcr_find_sync` / `gcr_decode_block` — byte-for-byte the same scan.
    pub fn sector_under_head(&self) -> i32 {
        use crate::gcr::{gcr_decode_block, gcr_find_sync};
        // ts:61-62 — ht = current_half_track; raw = gcr.tracks[ht - 2].
        let idx = (self.current_half_track as usize).wrapping_sub(2);
        let raw = match self.image.as_ref().and_then(|img| img.tracks.get(idx)) {
            Some(t) => t,
            None => return -1,
        };
        // ts:63 — if (!raw?.data || !raw.size) return -1.
        if raw.data.is_empty() || raw.size == 0 {
            return -1;
        }
        // ts:64 — bits = raw.size * 8.
        let bits = (raw.size as i64) * 8;
        // ts:66 — p = (((GCR_head_offset % bits) + bits) % bits). GCR_head_offset
        // is a BIT position; normalize into [0, bits).
        let mut p: i64 = (((self.gcr_head_offset as i64) % bits) + bits) % bits;
        // ts:67-68 — header buffer + firstSync tracker.
        let mut header = [0u8; 4];
        let mut first_sync: i64 = -1;
        // ts:69 — for (guard = 0; guard < 64; guard++).
        for _ in 0..64 {
            // ts:70-71 — p = gcr_find_sync(raw, p, bits); if (p < 0) return -1.
            p = gcr_find_sync(raw, p, bits);
            if p < 0 {
                return -1; // no sync = no header
            }
            // ts:72 — if (firstSync === p) return -1 (full revolution, no header).
            if first_sync == p {
                return -1;
            }
            // ts:73 — if (firstSync < 0) firstSync = p.
            if first_sync < 0 {
                first_sync = p;
            }
            // ts:74 — gcr_decode_block(raw, p, header, 1).
            gcr_decode_block(raw, p, &mut header, 1);
            // ts:75 — if (header[0] === 0x08) return header[2] (sector).
            if header[0] == 0x08 {
                return header[2] as i32;
            }
            // ts:76 — not a header (e.g. 0x07 data block); the next find_sync(p)
            // advances to the following sync mark.
        }
        // ts:78 — return -1.
        -1
    }

    /// PORT OF: rotation.ts:482-665 (rotation.c:339-570 rotation_1541_gcr).
    /// C `static`. 1541 circuit simulation for GCR-based images (.g64).
    fn rotation_1541_gcr(&mut self, ref_cycles_in: u64) {
        let mut ref_cycles = ref_cycles_in as i64;

        // rotation.ts:489 — uint64_t tmp = 30000UL;
        let mut tmp: i64 = 30000;
        // rotation.ts:491 — clk_ref_per_rev = 16000000 / (300 / 60);
        let mut clk_ref_per_rev: i64 = 16_000_000 / (300 / 60);
        // rotation.ts:493-495.
        tmp *= clk_ref_per_rev;
        tmp /= self.rpm as i64;
        clk_ref_per_rev = tmp + self.wobble_factor as i64;

        // rotation.ts:498 — cyc_act_frv = 1.
        let cyc_act_frv: i64 = 1;

        // rotation.ts:501.
        let count_new_bitcell: i64 = cyc_act_frv * clk_ref_per_rev;

        // rotation.ts:504-505.
        let mut cyc_sum_frv: i64 = 8 * self.gcr_current_track_size as i64;
        cyc_sum_frv = if cyc_sum_frv != 0 { cyc_sum_frv } else { 1 };

        if self.read_write_mode {
            // rotation.ts:509-601 — READ path.
            while ref_cycles > 0 {
                // rotation.ts:511.
                let mut todo: i64 = 1;
                let delta: i64 = count_new_bitcell - self.accum as i64;
                if (delta > 0) && ((cyc_sum_frv << 1) <= delta) {
                    todo = delta / cyc_sum_frv;
                    if ref_cycles < todo {
                        todo = ref_cycles;
                    }
                    if (self.ue7_counter < 16) && ((16 - self.ue7_counter as i64) < todo) {
                        todo = 16 - self.ue7_counter as i64;
                    }
                    if (self.filter_counter < 40) && ((40 - self.filter_counter as i64) < todo) {
                        todo = 40 - self.filter_counter as i64;
                    }
                    if (self.fr_randcount > 0) && ((self.fr_randcount as i64) < todo) {
                        todo = self.fr_randcount as i64;
                    }
                    if (self.so_delay > 0) && ((self.so_delay as i64) < todo) {
                        todo = self.so_delay as i64;
                    }
                }

                // so signal handling — rotation.ts:531-537.
                if self.so_delay != 0 {
                    self.so_delay -= todo as i32;
                    if self.so_delay == 0 {
                        self.byte_ready_edge = 1;
                        self.byte_ready_level = 1;
                    }
                }

                // 2.5µs flux filter — rotation.ts:540-553.
                self.filter_counter += todo as i32;
                if (self.filter_counter >= 40) && (self.filter_last_state != self.filter_state) {
                    self.filter_last_state = self.filter_state;
                    self.ue7_counter = self.ue7_dcba;
                    self.uf4_counter = 0;
                    self.fr_randcount = ((self.random_next_uint() >> 16) % 31) + 289;
                } else {
                    self.fr_randcount = self.fr_randcount.wrapping_sub(todo as u32);
                    if self.fr_randcount == 0 {
                        self.ue7_counter = self.ue7_dcba;
                        self.uf4_counter = 0;
                        self.fr_randcount = ((self.random_next_uint() >> 16) % 367) + 33;
                    }
                }

                // UE7 divider — rotation.ts:556-585.
                self.ue7_counter += todo as i32;
                if self.ue7_counter == 16 {
                    self.ue7_counter = self.ue7_dcba;
                    self.uf4_counter = (self.uf4_counter + 1) & 0xf;

                    if (self.uf4_counter & 0x3) == 2 {
                        self.last_read_data = ((self.last_read_data << 1) & 0x3fe)
                            | ((((self.uf4_counter + 0x1c) >> 4) & 0x01) as u32);
                        self.last_read_data &= 0x3ff;

                        self.write_flux = (self.last_write_data & 0x80) as i32;
                        self.last_write_data = (self.last_write_data << 1) & 0xff;

                        if self.last_read_data == 0x3ff {
                            self.bit_counter = 0;
                            // FIXME (VICE): latched BYTE READY unmodeled.
                        } else {
                            self.bit_counter += 1;
                            if self.bit_counter == 8 {
                                self.bit_counter = 0;
                                self.gcr_read = (self.last_read_data & 0xff) as u8;
                                self.last_write_data = self.gcr_read;

                                if (self.byte_ready_active & BRA_BYTE_READY) != 0 {
                                    self.so_delay = 16
                                        - ((self.cycle_index as i32 + (todo as i32 - 1)) & 15);
                                    if self.so_delay < 10 {
                                        self.so_delay += 16;
                                    }
                                }
                            }
                        }
                    }
                }

                // advance the count until the next bitcell — rotation.ts:588.
                self.accum = self
                    .accum
                    .wrapping_add((cyc_sum_frv * todo) as u32);

                // read the new bitcell — rotation.ts:591-597.
                if self.accum as i64 >= count_new_bitcell {
                    self.accum = (self.accum as i64 - count_new_bitcell) as u32;
                    if self.read_next_bit() != 0 {
                        self.filter_counter = 39;
                        self.filter_state ^= 1;
                    }
                }

                self.cycle_index = self.cycle_index.wrapping_add(todo as u32);
                ref_cycles -= todo;
            }
        } else {
            // rotation.ts:603-664 — WRITE path.
            while ref_cycles > 0 {
                let mut todo: i64 = 1;
                let delta: i64 = count_new_bitcell - self.accum as i64;
                if (delta > 0) && ((cyc_sum_frv << 1) <= delta) {
                    todo = delta / cyc_sum_frv;
                    if ref_cycles < todo {
                        todo = ref_cycles;
                    }
                    if (self.ue7_counter < 16) && ((16 - self.ue7_counter as i64) < todo) {
                        todo = 16 - self.ue7_counter as i64;
                    }
                    if (self.so_delay > 0) && ((self.so_delay as i64) < todo) {
                        todo = self.so_delay as i64;
                    }
                }

                if self.so_delay != 0 {
                    self.so_delay -= todo as i32;
                    if self.so_delay == 0 {
                        self.byte_ready_edge = 1;
                        self.byte_ready_level = 1;
                    }
                }

                // rotation.ts:627-630.
                self.accum = self
                    .accum
                    .wrapping_add((cyc_sum_frv * todo) as u32);
                if self.accum as i64 >= count_new_bitcell {
                    self.accum = (self.accum as i64 - count_new_bitcell) as u32;
                }

                // rotation.ts:633-659.
                self.ue7_counter += todo as i32;
                if self.ue7_counter == 16 {
                    self.ue7_counter = self.ue7_dcba;
                    self.uf4_counter = (self.uf4_counter + 1) & 0xf;

                    if (self.uf4_counter & 0x3) == 2 {
                        self.last_read_data = ((self.last_read_data << 1) & 0x3fe)
                            | ((((self.uf4_counter + 0x1c) >> 4) & 0x01) as u32);
                        self.last_read_data &= 0x3ff;

                        self.write_next_bit(self.last_write_data & 0x80);

                        self.last_write_data = (self.last_write_data << 1) & 0xff;

                        self.accum = (cyc_sum_frv * 2) as u32;

                        self.bit_counter += 1;
                        if self.bit_counter == 8 {
                            self.bit_counter = 0;
                            self.last_write_data = self.gcr_write_value;

                            if (self.byte_ready_active & BRA_BYTE_READY) != 0 {
                                self.so_delay =
                                    16 - ((self.cycle_index as i32 + (todo as i32 - 1)) & 15);
                                if self.so_delay < 10 {
                                    self.so_delay += 16;
                                }
                            }
                        }
                    }
                }

                self.cycle_index = self.cycle_index.wrapping_add(todo as u32);
                ref_cycles -= todo;
            }
        }
    }

    /// PORT OF: rotation.ts:670-705 (rotation.c:572-610 rotation_1541_gcr_cycle).
    /// C `static`. Top-level GCR dispatcher.
    fn rotation_1541_gcr_cycle(&mut self, clk: u64) {
        // rotation.ts:675.
        let one_rotation: u64 = if self.frequency != 0 { 400_000 } else { 200_000 };

        // rotation.ts:678-680.
        let mut cpu_cycles = clk.wrapping_sub(self.rotation_last_clk);
        self.rotation_last_clk = clk;
        // rotation.ts:682-684.
        while cpu_cycles > one_rotation * 2 {
            cpu_cycles -= one_rotation;
        }

        // rotation.ts:687.
        let mut ref_cycles: u64 = cpu_cycles * (if self.frequency != 0 { 8 } else { 16 });

        // rotation.ts:690-693.
        let mut ref_advance_cycles = self.req_ref_cycles;
        self.req_ref_cycles = 0;
        ref_advance_cycles &= 15;
        ref_cycles += ref_advance_cycles;

        // rotation.ts:696-704.
        if ref_cycles > 0 {
            if ref_cycles > self.ref_advance {
                ref_cycles -= self.ref_advance;
                self.ref_advance = ref_advance_cycles;
                self.rotation_1541_gcr(ref_cycles);
            } else {
                self.ref_advance -= ref_cycles;
            }
        }
    }

    /// PORT OF: rotation.ts:712-716 (rotation.c:618-631 rotation_p64_get_delta).
    /// P64 OoS stub — panics with the spec marker (never silent).
    #[allow(dead_code)]
    fn rotation_p64_get_delta(&mut self) -> u32 {
        panic!("PORT-STUB: P64 not implemented per Spec 612 OoS (§10 PAL first, NTSC/P64 deferred)");
    }

    /// PORT OF: rotation.ts:721-725 (rotation.c:635-942 rotation_1541_p64).
    #[allow(dead_code)]
    fn rotation_1541_p64(&mut self, _ref_cycles: u64) {
        panic!("PORT-STUB: P64 not implemented per Spec 612 OoS (§10 PAL first, NTSC/P64 deferred)");
    }

    /// PORT OF: rotation.ts:730-734 (rotation.c:944-983 rotation_1541_p64_cycle).
    #[allow(dead_code)]
    fn rotation_1541_p64_cycle(&mut self) {
        panic!("PORT-STUB: P64 not implemented per Spec 612 OoS (§10 PAL first, NTSC/P64 deferred)");
    }

    /// PORT OF: rotation.ts:741-864 (rotation.c:989-1100 rotation_1541_simple).
    /// C `static`. "Very simple and fast emulation for perfect images like those
    /// coming from dxx files." Used when complicated_image_loaded == 0.
    fn rotation_1541_simple(&mut self, clk: u64) {
        self.req_ref_cycles = 0;

        // rotation.ts:748-750.
        let mut delta = clk.wrapping_sub(self.rotation_last_clk);
        self.rotation_last_clk = clk;

        // rotation.ts:753-762.
        let mut tmp: i64 = 1_000_000;
        // rotation.ts:757 — `(long)wobble_factor * 1000000 / 3200000` (trunc).
        tmp += (self.wobble_factor as i64 * 1_000_000) / 3_200_000;
        tmp *= 30_000;
        // PL-7: NO `rpm || 30000` guard — divide unconditionally.
        let rpmscale: i64 = tmp / self.rpm as i64;

        let mut bits_moved: i64 = 0;
        while delta > 0 {
            let tdelta = if delta > 1000 { 1000 } else { delta };
            delta -= tdelta;
            self.accum = self.accum.wrapping_add(
                ROT_SPEED_BPS[self.frequency][self.speed_zone].wrapping_mul(tdelta as u32),
            );
            bits_moved += self.accum as i64 / rpmscale;
            self.accum = (self.accum as i64 % rpmscale) as u32;
        }

        if self.read_write_mode {
            // rotation.ts:773-838 — READ path.
            let mut off = self.gcr_head_offset;
            // rotation.ts:783 — `last_read_data = rptr->last_read_data << 7;`
            let mut last_read_data: u32 = self.last_read_data << 7;
            let mut bit_counter = self.bit_counter;

            // rotation.ts:787-793 — initial `byte` window.
            let track = self.current_track().cloned();
            let track_loaded = self.gcr_image_loaded != 0 && track.is_some();
            let mut byte: u32 = if !track_loaded {
                0
            } else {
                let t = track.as_ref().unwrap();
                ((*t.data.get((off >> 3) as usize).unwrap_or(&0) as u32) << (off & 7)) & 0xffff_ffff
            };

            // rotation.ts:795-831 — `while (bits_moved-- != 0)`.
            while bits_moved != 0 {
                bits_moved -= 1;
                // rotation.ts:797 — `byte <<= 1; off++;`
                byte <<= 1;
                off = off.wrapping_add(1);
                if off & 7 == 0 {
                    if (off >> 3) >= self.gcr_current_track_size as u32 {
                        off = 0;
                    }
                    byte = if !track_loaded {
                        0
                    } else {
                        let t = track.as_ref().unwrap();
                        *t.data.get((off >> 3) as usize).unwrap_or(&0) as u32
                    };
                }

                // rotation.ts:811-814.
                last_read_data <<= 1;
                last_read_data |= byte & 0x80;
                self.last_write_data = (self.last_write_data << 1) & 0xff;

                // sync test on bits 7..16 — rotation.ts:817.
                if (!last_read_data) & 0x1ff80 != 0 {
                    bit_counter += 1;
                    if bit_counter == 8 {
                        bit_counter = 0;
                        // rotation.ts:821 — `GCR_read = (uint8_t)(last_read_data >> 7);`
                        self.gcr_read = ((last_read_data >> 7) & 0xff) as u8;
                        self.last_write_data = self.gcr_read;
                        if (self.byte_ready_active & BRA_BYTE_READY) != 0 {
                            self.byte_ready_edge = 1;
                            self.byte_ready_level = 1;
                        }
                    }
                } else {
                    bit_counter = 0;
                }
            }

            // rotation.ts:835-838.
            self.last_read_data = (last_read_data >> 7) & 0x3ff;
            self.bit_counter = bit_counter;
            self.gcr_head_offset = off;
            if self.gcr_read == 0 {
                self.gcr_read = 0x11;
            }
        } else {
            // rotation.ts:840-863 — WRITE path.
            while bits_moved != 0 {
                bits_moved -= 1;
                self.last_read_data = (self.last_read_data << 1) & 0x3fe;
                if (self.last_read_data & 0xf) == 0 {
                    self.last_read_data |= 1;
                }
                // rotation.ts:847 (D47) — emit current bit BEFORE shift.
                self.write_next_bit(self.last_write_data & 0x80);
                self.last_write_data = (self.last_write_data << 1) & 0xff;
                self.bit_counter += 1;
                if self.bit_counter == 8 {
                    self.bit_counter = 0;
                    self.last_write_data = self.gcr_write_value;
                    if (self.byte_ready_active & BRA_BYTE_READY) != 0 {
                        self.byte_ready_edge = 1;
                        self.byte_ready_level = 1;
                    }
                }
            }
            // rotation.ts:862 — force the GCR engine for all future rotations.
            self.complicated_image_loaded = 1;
        }
    }

    /// PORT OF: rotation.ts:871-890 (rotation.c:1106-1125 rotation_rotate_disk).
    pub fn rotate_disk(&mut self, clk: u64) {
        if (self.byte_ready_active & BRA_MOTOR_ON) == 0 {
            self.req_ref_cycles = 0;
            return;
        }

        self.rotation_do_wobble(clk);

        if self.complicated_image_loaded != 0 {
            if self.p64_image_loaded != 0 {
                // P64 OoS stub — Spec 612 §10.
                panic!(
                    "PORT-STUB: P64 not implemented per Spec 612 OoS (§10 PAL first, NTSC/P64 deferred)"
                );
            }
            self.rotation_1541_gcr_cycle(clk);
        } else {
            self.rotation_1541_simple(clk);
        }
    }

    /// PORT OF: rotation.ts:895-901 (rotation.c:1134-1143 rotation_sync_found).
    /// VICE signature `uint8_t rotation_sync_found(drive_t *dptr)`.
    #[inline]
    pub fn sync_found(&self) -> u8 {
        if !self.read_write_mode || self.attach_clk != 0 {
            return 0x80;
        }
        if self.last_read_data == 0x3ff {
            0
        } else {
            0x80
        }
    }

    /// PORT OF: rotation.ts:909-928 (rotation.c:1145-1165 rotation_byte_read).
    /// Writes `dptr->GCR_read` and returns void (the assembled byte is read at
    /// the call site via [`pra_pin`]).
    pub fn byte_read(&mut self, clk: u64) {
        // Spec 784 loader-lens — PASSIVE read-set instrumentation. This is the
        // via2d `read_pra`/`GCR_read` consume point: one GCR data byte latched off
        // the disk surface. Count it so the loader-lens can tell a sector the drive
        // physically READ from one the head merely rotated past (the 78×T35 bug).
        // Never read by the emulation → zero behavioural effect (see field doc).
        self.gcr_read_count = self.gcr_read_count.wrapping_add(1);
        if self.attach_clk != 0 {
            if clk.wrapping_sub(self.attach_clk) < DRIVE_ATTACH_DELAY {
                self.gcr_read = 0;
            } else {
                self.attach_clk = 0;
            }
        } else if self.attach_detach_clk != 0 {
            if clk.wrapping_sub(self.attach_detach_clk) < DRIVE_ATTACH_DETACH_DELAY {
                self.gcr_read = 0;
            } else {
                self.attach_detach_clk = 0;
            }
        } else {
            self.rotate_disk(clk);
        }
        self.req_ref_cycles = 0;
    }

    // =========================================================================
    // SECTION 6 — drive.c head helpers the engine + via2d wire to (drive.ts)
    // =========================================================================

    /// PORT OF: drive.ts:1153-1225 (drive.c:689-733 drive_set_half_track) for a
    /// side-0 1541. Clamp [2,84], select `gcr.tracks[num-2]`, rescale the head
    /// offset by the new track size, update the active track size.
    pub fn set_half_track(&mut self, mut num: u32) {
        // 1541 family clamp (drive.ts:1161-1171).
        if num > DRIVE_HALFTRACKS_1541_U32 {
            num = DRIVE_HALFTRACKS_1541_U32;
        }
        if num < 2 {
            num = 2;
        }

        if self.current_half_track != num || self.side != 0 {
            self.current_half_track = num;
            // P64 pulse-stream reset is OoS (no-op).
        }
        self.side = 0;

        // drive.ts:1200-1224. `tmp` (the side multiplier) is 0 for side 0, so
        // the index is `current_half_track - 2`.
        let idx = (self.current_half_track as usize).wrapping_sub(2);
        match self.image.as_ref().and_then(|img| img.tracks.get(idx)) {
            Some(trk) => {
                let new_size = trk.size;
                if self.gcr_current_track_size != 0 {
                    self.gcr_head_offset = ((self.gcr_head_offset as u64 * new_size as u64)
                        / self.gcr_current_track_size as u64)
                        as u32;
                } else {
                    self.gcr_head_offset = 0;
                }
                self.gcr_current_track_size = new_size;
            }
            None => {
                self.gcr_current_track_size = 0;
                self.gcr_head_offset = 0;
            }
        }
    }

    /// PORT OF: drive.ts:1231-1242 (drive.c:739-747 drive_move_head). Step the
    /// head by ±1/±2 half-tracks. VICE calls `drive_gcr_data_writeback(drive)`
    /// BEFORE stepping (drive.ts:1235) so any dirty track at the current head
    /// position is flushed to the image before the head leaves it — we do the
    /// same here. (`drive_sound_head` is a no-op headless.)
    pub fn move_head(&mut self, step: i32) {
        self.drive_gcr_data_writeback();
        let new = (self.current_half_track as i32 + step).max(2) as u32;
        self.set_half_track(new);
    }

    /// PORT OF: drive.ts:1253-1367 (drive.c:749-847 drive_gcr_data_writeback) for
    /// the single-side 1541 G64/D64 path. If the current track is dirty,
    /// serialize it back into the raw on-disk image bytes via
    /// [`GcrImage::write_half_track`] and clear the dirty flag.
    ///
    /// VICE always writes the requested track for GCR images (no extend needed);
    /// for D64 it writes the in-range track. TRX64 only mounts 35-track D64s and
    /// fully-populated G64s, so the extend/ask-dialog branches (D71/D81 doubled
    /// sides, beyond-image track extension) are never reached and are omitted —
    /// the requested-track write is the live path. Returns whether a flush ran.
    pub fn drive_gcr_data_writeback(&mut self) -> bool {
        // drive.c:759 — `if (drive->image == NULL) return;`
        if self.image.is_none() {
            return false;
        }
        // drive.c:780 — `if (!drive->GCR_dirty_track) return;`
        if self.gcr_dirty_track == 0 {
            return false;
        }

        // The half-track to flush. VICE uses `current_half_track + side*tmp`;
        // side is 0 for the 1541, so it is `current_half_track`. We use the
        // recorded `dirty_half_track` (the head position at write time) which
        // equals `current_half_track` here (the flush precedes the step).
        let half_track = self.dirty_half_track as usize;

        // Always clear the dirty flag (matches every VICE return path).
        self.gcr_dirty_track = 0;

        let (bytes, kind, read_only) = match (
            self.writeback_bytes.as_mut(),
            self.writeback_kind,
        ) {
            (Some(b), Some(k)) => (b, k, self.read_only != 0),
            // No write-back target wired (e.g. snapshot-restored disk). The
            // in-memory GCR already carries the write; nothing to serialize.
            _ => return false,
        };

        let image = self.image.as_ref().expect("image present (checked above)");
        image.write_half_track(kind, bytes, half_track, read_only);
        true
    }

    /// Flush ALL pending dirty tracks (= VICE `drive_gcr_data_writeback_all`,
    /// drive.c:849-870, called at snapshot/detach/exit). For the single 1541
    /// here there is at most one dirty track, so this is `drive_gcr_data_writeback`.
    pub fn drive_gcr_data_writeback_all(&mut self) -> bool {
        self.drive_gcr_data_writeback()
    }

    // =========================================================================
    // SECTION 7 — via2d / drive wiring helpers (kept from the merged port)
    // =========================================================================

    /// Attach a GCR-encoded disk and park the head at track 18 (half-track 36),
    /// selecting that track's GCR buffer. Sets the attach-settle window.
    /// (= driveimage.ts disk_image_attach → drive_set_half_track + attach_clk.)
    ///
    /// `writeback` carries the raw on-disk image bytes + format the dirty GCR
    /// track is serialized back into on a head move / detach / explicit flush
    /// (= VICE `fsimage->fd` + `image->type`). `None` ⇒ no write-back target
    /// (the GCR image is the only copy, e.g. a snapshot-restored disk).
    pub fn attach(&mut self, image: GcrImage, clk: u64) {
        self.attach_with_writeback(image, clk, None);
    }

    /// [`attach`] with an explicit write-back target (`bytes`, `kind`, `read_only`).
    /// [`attach`]: Rotation::attach
    pub fn attach_with_writeback(
        &mut self,
        image: GcrImage,
        clk: u64,
        writeback: Option<(Vec<u8>, WritebackKind, bool)>,
    ) {
        self.image = Some(image);
        self.gcr_image_loaded = 1;
        self.gcr_current_track_size = 0;
        self.gcr_head_offset = 0;
        match writeback {
            Some((bytes, kind, read_only)) => {
                self.writeback_bytes = Some(bytes);
                self.writeback_kind = Some(kind);
                self.read_only = read_only as i32;
            }
            None => {
                self.writeback_bytes = None;
                self.writeback_kind = None;
            }
        }
        self.gcr_dirty_track = 0;
        self.dirty_half_track = 0;
        // Force track (re)selection at the current half-track.
        let ht = self.current_half_track;
        self.current_half_track = 0; // force the != check in set_half_track
        self.set_half_track(ht);
        self.attach_clk = if clk == 0 { 1 } else { clk };
        self.rotation_last_clk = clk;
    }

    /// Detach the disk. VICE's `drive_image_detach` flushes any dirty track
    /// (`drive_gcr_data_writeback`) BEFORE releasing the GCR buffer
    /// (driveimage.ts disk_image_detach), so an eject persists a pending write.
    pub fn detach(&mut self) {
        self.drive_gcr_data_writeback();
        self.image = None;
        self.gcr_image_loaded = 0;
        self.gcr_current_track_size = 0;
        self.gcr_head_offset = 0;
        self.writeback_bytes = None;
        self.writeback_kind = None;
        self.gcr_dirty_track = 0;
        self.dirty_half_track = 0;
    }

    /// Take the (possibly mutated) raw on-disk image bytes out for the daemon to
    /// persist / hash / snapshot. Flushes any pending dirty track first
    /// (= VICE `drive_gcr_data_writeback_all` before reading `fsimage->fd`), then
    /// returns a clone of the current image bytes (the write-back target stays
    /// installed so subsequent writes keep accumulating).
    pub fn writeback_bytes_synced(&mut self) -> Option<Vec<u8>> {
        self.drive_gcr_data_writeback_all();
        self.writeback_bytes.clone()
    }

    /// True if there is a dirty GCR track pending flush (a write occurred since
    /// the last writeback).
    #[inline]
    pub fn has_dirty_track(&self) -> bool {
        self.gcr_dirty_track != 0
    }

    /// Drive one bit-level write at the current head exactly as the rotation
    /// engine's WRITE path does (`write_next_bit`). This is the SAME call the
    /// engine makes internally on a drive write (rotation.rs:670/855); it is
    /// exposed only so a cross-crate integration test (the daemon's disk
    /// auto-persist proof) can produce a REAL dirty GCR track without booting the
    /// drive CPU + a full SAVE. Never called in product code (the engine has its
    /// own internal caller). Marked `#[doc(hidden)]` to keep it off the public API
    /// surface docs.
    #[doc(hidden)]
    pub fn write_one_bit_for_test(&mut self, value: u8) {
        self.write_next_bit(value);
    }

    /// VIA2 PRA pin input = the current `GCR_read` byte (via2d read_pra). The
    /// caller advances the model via [`byte_read`] first, then samples this.
    #[inline]
    pub fn pra_pin(&self) -> u8 {
        self.gcr_read
    }

    /// drive_writeprotect_sense (drive.ts) — returns 0x10 (write-enabled) /
    /// 0x00 (write-protected), AND clears the spin-up `attach_clk` window once
    /// DRIVE_ATTACH_DELAY has elapsed (the via2d read_prb WPS path). Only the
    /// attach branch is modelled (a plain mounted D64 has no detach window).
    pub fn writeprotect_sense(&mut self, clk: u64) -> u8 {
        if self.attach_clk != 0 {
            if clk.wrapping_sub(self.attach_clk) < DRIVE_ATTACH_DELAY {
                return 0x0;
            }
            self.attach_clk = 0;
        }
        if self.gcr_image_loaded == 0 {
            return 0x10;
        }
        if self.read_only != 0 {
            0x0
        } else {
            0x10
        }
    }

    /// VIA2 PRB pin input default (DDRB=0): `sync | wps | 0x6f` (via2d read_prb).
    /// `sync_found` is sampled FIRST (while `attach_clk` may still be set), THEN
    /// `writeprotect_sense` clears the spin-up window.
    #[inline]
    pub fn prb_pin(&mut self, clk: u64) -> u8 {
        let sync = self.sync_found();
        let wps = self.writeprotect_sense(clk);
        sync | wps | 0x6f
    }

    /// `rpmscale` helper kept for the existing unit test (= rotation_1541_simple
    /// inner divisor with wobble_factor = 0).
    #[cfg(test)]
    fn rpmscale(&self) -> i64 {
        let mut tmp: i64 = 1_000_000;
        tmp += (self.wobble_factor as i64 * 1_000_000) / 3_200_000;
        tmp *= 30_000;
        tmp / self.rpm as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank_image() -> GcrImage {
        GcrImage::from_d64(&vec![0u8; 683 * 256])
    }

    #[test]
    fn attach_parks_at_track18() {
        let mut r = Rotation::new();
        r.attach(blank_image(), 0);
        assert_eq!(r.current_half_track, 36);
        // Track 18 raw size = 7142.
        assert_eq!(r.gcr_current_track_size, 7142);
        assert_eq!(r.gcr_image_loaded, 1);
    }

    #[test]
    fn motor_off_does_not_rotate() {
        let mut r = Rotation::new();
        r.attach(blank_image(), 0);
        r.attach_clk = 0; // bypass settle
        r.byte_ready_active = BRA_BYTE_READY; // motor OFF
        let before = r.gcr_head_offset;
        r.rotate_disk(100_000);
        assert_eq!(r.gcr_head_offset, before, "head must not move with motor off");
    }

    #[test]
    fn rotation_advances_head_and_assembles_bytes() {
        let path =
            "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/scramble_infinity.d64";
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => return, // skip without sample
        };
        let mut r = Rotation::new();
        r.attach(GcrImage::from_d64(&bytes), 0);
        r.attach_clk = 0; // bypass settle window for the unit test
        r.read_write_mode = true;
        // Spin ~20000 drive cycles (well over one byte time).
        r.byte_read(20_000);
        assert!(r.gcr_head_offset > 0, "head advanced");
        // A sync should have been seen at some point → byte assembly ran.
        assert!(r.gcr_read != 0, "GCR_read populated");
    }

    #[test]
    fn sync_found_while_writing_is_not_found() {
        let mut r = Rotation::new();
        r.read_write_mode = false;
        assert_eq!(r.sync_found(), 0x80);
    }

    #[test]
    fn prb_pin_layout() {
        let mut r = Rotation::new();
        r.read_write_mode = true;
        r.attach_clk = 0; // settled — SYNC visible
        r.last_read_data = 0; // not sync
        // sync(0x80) | wps(0x10) | 0x6f = 0xff
        assert_eq!(r.prb_pin(0), 0xff);
        // When sync found (last_read_data == 0x3ff), PB7 clears → 0x7f.
        r.last_read_data = 0x3ff;
        assert_eq!(r.prb_pin(0), 0x7f);
    }

    #[test]
    fn rpmscale_default_is_1_000_000() {
        // rotation.ts:753-762: tmp=1_000_000; tmp += 0; tmp *= 30_000; tmp /= rpm.
        // = 30_000_000_000 / 30_000 = 1_000_000 (NOT 33333 — the old distilled
        // module-doc comment was wrong; the helper itself returned 1_000_000).
        let r = Rotation::new();
        assert_eq!(r.rpmscale(), 1_000_000);
    }

    // ── write-back wiring ────────────────────────────────────────────────────

    /// A 35-track D64 with distinct per-sector content (so a write-back can be
    /// byte-checked after re-mount).
    fn synthetic_d64() -> Vec<u8> {
        use crate::gcr::{d64_linear_sector, d64_sectors_per_track, D64_TRACKS};
        let mut d = vec![0u8; 683 * 256];
        for track in 1..=D64_TRACKS {
            for sector in 0..d64_sectors_per_track(track) {
                let off = d64_linear_sector(track, sector).unwrap() * 256;
                for i in 0..256 {
                    d[off + i] = (track ^ sector ^ (i as u8)) & 0xff;
                }
            }
        }
        d
    }

    /// A bit-level write through the rotation engine sets `gcr_dirty_track`, and
    /// `drive_gcr_data_writeback` then serializes the dirty half-track back into
    /// the write-back image bytes — i.e. a drive write persists.
    #[test]
    fn rotation_write_marks_dirty_and_writeback_persists() {
        let d64 = synthetic_d64();
        let img = GcrImage::from_d64(&d64);
        let mut r = Rotation::new();
        r.attach_with_writeback(
            img,
            0,
            Some((d64.clone(), WritebackKind::D64, false)),
        );
        r.attach_clk = 0; // settled

        // Nothing dirty yet.
        assert!(!r.has_dirty_track());
        assert!(!r.drive_gcr_data_writeback(), "no flush when clean");

        // Drive a single bit-level write at the current head (track 18). This is
        // exactly what the WRITE-path of the rotation engine calls.
        r.write_next_bit(1);
        assert!(r.has_dirty_track(), "write_next_bit set GCR_dirty_track");
        assert_eq!(r.dirty_half_track, 36, "dirtied the current half-track (T18)");

        // The flush serializes the dirty track and clears the flag.
        assert!(r.drive_gcr_data_writeback(), "flush ran");
        assert!(!r.has_dirty_track(), "dirty cleared after flush");

        // The write-back bytes are still a valid 174848-byte D64 (the writeback
        // re-encoded T18 from its GCR — a single flipped bit may corrupt one
        // T18 sector's GCR framing, but the image size + all OTHER tracks are
        // intact, which is the persistence contract we assert here).
        let bytes = r.writeback_bytes.as_ref().unwrap();
        assert_eq!(bytes.len(), d64.len(), "image size preserved");
        // Track 1 (untouched) must be byte-identical to the source.
        let t1_off = 0;
        assert_eq!(&bytes[t1_off..t1_off + 256 * 21], &d64[t1_off..t1_off + 256 * 21]);
    }

    /// A full sector write injected into the GCR track persists round-trip
    /// through the rotation write-back into the .d64 image and back out on
    /// re-mount (the behavioral "a sector write reaches the image" proof).
    #[test]
    fn rotation_sector_write_round_trips_through_writeback() {
        use crate::gcr::{d64_linear_sector, gcr_read_sector, gcr_write_sector, CBMDOS_FDC_ERR_OK};
        let d64 = synthetic_d64();
        let mut img = GcrImage::from_d64(&d64);

        // New payload for T18 S3 (T18 is the head-parked track).
        let track: u8 = 18;
        let sector: u8 = 3;
        let slot = (track as usize) * 2 - 2;
        let new_sector: Vec<u8> = (0..256).map(|i| (0x80u16 + i as u16) as u8).collect();
        assert_eq!(gcr_write_sector(&mut img.tracks[slot], &new_sector, sector), CBMDOS_FDC_ERR_OK);

        let mut r = Rotation::new();
        r.attach_with_writeback(img, 0, Some((d64.clone(), WritebackKind::D64, false)));
        r.attach_clk = 0;
        // Mark dirty as the engine would, at the head (T18 = half-track 36).
        r.gcr_dirty_track = 1;
        r.dirty_half_track = 36;

        // Flush + take the synced bytes.
        let bytes = r.writeback_bytes_synced().expect("writeback bytes");
        let off = d64_linear_sector(track, sector).unwrap() * 256;
        assert_eq!(&bytes[off..off + 256], &new_sector[..], "sector write reached the .d64");

        // Re-mount the mutated image → the new sector decodes back.
        let img2 = GcrImage::from_d64(&bytes);
        let mut decoded = [0u8; 256];
        assert_eq!(gcr_read_sector(&img2.tracks[slot], &mut decoded, sector), CBMDOS_FDC_ERR_OK);
        assert_eq!(&decoded[..], &new_sector[..], "round-trips after re-mount");
    }

    /// Detach flushes a pending dirty track before releasing the GCR buffer.
    #[test]
    fn detach_flushes_pending_write() {
        let d64 = synthetic_d64();
        let img = GcrImage::from_d64(&d64);
        let mut r = Rotation::new();
        r.attach_with_writeback(img, 0, Some((d64.clone(), WritebackKind::D64, false)));
        r.attach_clk = 0;
        r.write_next_bit(0);
        assert!(r.has_dirty_track());
        // Detach must flush (clear dirty) before tearing the image down.
        r.detach();
        assert!(!r.has_dirty_track(), "detach flushed the dirty track");
        assert!(r.image.is_none());
        assert!(r.writeback_bytes.is_none());
    }

    /// A read-only mount rejects the write-back (the image bytes are untouched).
    #[test]
    fn read_only_mount_rejects_writeback() {
        let d64 = synthetic_d64();
        let img = GcrImage::from_d64(&d64);
        let mut r = Rotation::new();
        r.attach_with_writeback(img, 0, Some((d64.clone(), WritebackKind::D64, true)));
        r.attach_clk = 0;
        r.write_next_bit(1);
        assert!(r.has_dirty_track());
        r.drive_gcr_data_writeback();
        // Read-only → write_dxx_half_track returned -1, bytes unchanged.
        assert_eq!(r.writeback_bytes.as_ref().unwrap(), &d64, "read-only image untouched");
    }
}
