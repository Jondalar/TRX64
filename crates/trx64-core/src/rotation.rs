//! rotation.rs — the rotating GCR disk model (1541 "simple" engine).
//!
//! Byte-exact port of the TS oracle's `rotation.ts` `rotation_1541_simple`
//! (VICE rotation.c:989-1100) plus the supporting harness (`rotation_byte_read`,
//! `rotation_sync_found`, `rotation_speed_zone_set`, `rotation_begins`, init/
//! reset) and the drive-side `drive_set_half_track` track selection
//! (drive.c:689-733).
//!
//! Scope (ADR-012 milestone 2): a mounted D64 uses the SIMPLE engine
//! (`complicated_image_loaded == 0`), which advances the head bit-by-bit per
//! drive-clock delta, assembles GCR bytes into `gcr_read` (VIA2 PRA), detects
//! SYNC, and asserts `byte_ready` (the 6502 SO / V-flag handshake) on each byte.
//! The cycle-accurate UE7/UF4/flux GCR engine (.g64, weak bits) and the P64 and
//! WRITE paths are out of scope here.
//!
//! Wobble is a no-op: `wobble_factor = wobble_frequency = 0` (drive.ts:1831-1832),
//! so `rotation_do_wobble` never perturbs `rotation_last_clk` and `rpmscale`
//! reduces to `floor(1_000_000 * 30_000 / rpm)` = 1_000_000_000 / 30000 = 33333.

use crate::gcr::{GcrImage, DRIVE_HALFTRACKS_1541};

// ── byte_ready_active bits (drivetypes.ts:161-163) ──────────────────────────
pub const BRA_BYTE_READY: u8 = 0x02;
pub const BRA_MOTOR_ON: u8 = 0x04;

/// Settle delay after a fresh attach during which `rotation_byte_read` forces
/// `GCR_read = 0` (drivetypes.ts:128 — DRIVE_ATTACH_DELAY = 3*600000).
pub const DRIVE_ATTACH_DELAY: u64 = 3 * 600_000;

/// Drive rpm in VICE units (300 rpm = 30000; drive.ts:1829).
const RPM: u64 = 30_000;

/// `rot_speed_bps[frequency][zone]` (rotation.ts:199-203). Only the 1x row
/// (frequency 0) is exercised by the PAL 1541.
const ROT_SPEED_BPS: [[u32; 4]; 2] = [
    [250_000, 266_667, 285_714, 307_692],
    [125_000, 133_333, 142_857, 153_846],
];

/// The rotating-disk state for one drive (= VICE `rotation_t` + the head-side
/// `drive_t` fields the simple engine touches). Owned by `Drive1541`.
#[derive(Clone)]
pub struct Rotation {
    // ── rotation_t ──────────────────────────────────────────────────────────
    /// 10-bit GCR read shift register (`last_read_data`, mask 0x3ff).
    pub last_read_data: u32,
    /// Write shifter byte (`last_write_data`, u8).
    pub last_write_data: u8,
    /// Bits assembled within the current GCR byte (0..8).
    pub bit_counter: i32,
    /// Bit-cell phase accumulator (`accum`, u32 wrapping).
    pub accum: u32,
    /// 1x/2x speed toggle (0 = normal PAL). Indexes ROT_SPEED_BPS.
    pub frequency: usize,
    /// Disk speed zone 0..3 (set by VIA2 PB5-6).
    pub speed_zone: usize,
    /// Last drive clock the model was advanced to (`rotation_last_clk`).
    pub rotation_last_clk: u64,

    // ── drive_t head/track fields ────────────────────────────────────────────
    /// Head bit offset into the current track (`GCR_head_offset`).
    pub gcr_head_offset: u32,
    /// Current half-track (2..=84). Track N → half-track N*2. Power-on 36 (T18).
    pub current_half_track: u32,
    /// Active track size in bytes (`GCR_current_track_size`).
    pub gcr_current_track_size: usize,
    /// Whether a GCR image is loaded (`GCR_image_loaded`).
    pub gcr_image_loaded: bool,
    /// Read (1) vs write (0) mode (`read_write_mode`). Power-on read.
    pub read_write_mode: bool,
    /// Motor-on | byte-ready-enable bits (`byte_ready_active`).
    pub byte_ready_active: u8,
    /// The assembled GCR byte presented on VIA2 PRA (`GCR_read`).
    pub gcr_read: u8,
    /// Byte-ready rising-edge flag → drive 6502 SO/overflow (`byte_ready_edge`).
    pub byte_ready_edge: u8,
    /// Byte-ready level (`byte_ready_level`).
    pub byte_ready_level: u8,
    /// Clock the disk was attached (spin-up settle window). 0 = settled.
    ///
    /// Byte-exact with VICE: cleared once `DRIVE_ATTACH_DELAY` has elapsed by
    /// either the PRA path ([`byte_read`] → `rotation_byte_read`) or the PRB/WPS
    /// path ([`writeprotect_sense`] → `drive_writeprotect_sense`). `rotate_disk`
    /// (the bare SYNC poll) never touches it. Gates SYNC visibility ([`sync_found`])
    /// and `byte_read`'s `GCR_read = 0` forcing.
    pub attach_clk: u64,
    /// Encoded GCR image (per half-track). `None` = no disk.
    pub image: Option<GcrImage>,
}

impl Default for Rotation {
    fn default() -> Self {
        Self::new()
    }
}

impl Rotation {
    /// Power-on state (drive.ts:1780-1832 relevant subset + rotation_init).
    pub fn new() -> Self {
        Self {
            last_read_data: 0,
            last_write_data: 0,
            bit_counter: 0,
            accum: 0,
            frequency: 0,
            speed_zone: 0,
            rotation_last_clk: 0,
            gcr_head_offset: 0,
            current_half_track: 36, // track 18, where the head parks on attach
            gcr_current_track_size: 0,
            gcr_image_loaded: false,
            read_write_mode: true,
            byte_ready_active: BRA_BYTE_READY | BRA_MOTOR_ON,
            gcr_read: 0,
            byte_ready_edge: 0,
            byte_ready_level: 0,
            attach_clk: 0,
            image: None,
        }
    }

    /// rotation_reset (rotation.c:111-137): clear shifters/accumulator, re-anchor
    /// the last-clock. Does NOT clear the image or track selection.
    pub fn reset(&mut self, clk: u64) {
        self.last_read_data = 0;
        self.last_write_data = 0;
        self.bit_counter = 0;
        self.accum = 0;
        self.rotation_last_clk = clk;
    }

    /// rotation_speed_zone_set (rotation.c:139-143): set the active speed zone
    /// (density). The simple engine reads `speed_zone` to pick the bit rate.
    pub fn speed_zone_set(&mut self, zone: usize) {
        self.speed_zone = zone & 3;
    }

    /// rotation_begins (rotation.c:...): re-anchor the rotation clock on motor-on.
    pub fn begins(&mut self, clk: u64) {
        self.rotation_last_clk = clk;
    }

    /// Attach a GCR-encoded disk and park the head at track 18 (half-track 36),
    /// selecting that track's GCR buffer. Sets the attach-settle window.
    pub fn attach(&mut self, image: GcrImage, clk: u64) {
        self.image = Some(image);
        self.gcr_image_loaded = true;
        self.gcr_current_track_size = 0;
        self.gcr_head_offset = 0;
        // Force track (re)selection at the current half-track.
        let ht = self.current_half_track;
        self.current_half_track = 0; // force the != check in set_half_track
        self.set_half_track(ht);
        self.attach_clk = if clk == 0 { 1 } else { clk };
        self.rotation_last_clk = clk;
    }

    /// Detach the disk.
    pub fn detach(&mut self) {
        self.image = None;
        self.gcr_image_loaded = false;
        self.gcr_current_track_size = 0;
        self.gcr_head_offset = 0;
    }

    /// drive_set_half_track (drive.c:689-733) for a side-0 1541: clamp [2,84],
    /// select `tracks[half_track-2]`, rescale the head offset by the new track
    /// size, update the active track size.
    pub fn set_half_track(&mut self, mut num: u32) {
        if num > DRIVE_HALFTRACKS_1541 as u32 {
            num = DRIVE_HALFTRACKS_1541 as u32;
        }
        if num < 2 {
            num = 2;
        }
        self.current_half_track = num;

        let idx = (num as usize).wrapping_sub(2);
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

    /// drive_move_head (drive.c:739-747): step the head by ±1/±2 half-tracks.
    pub fn move_head(&mut self, step: i32) {
        let new = (self.current_half_track as i32 + step).max(2) as u32;
        self.set_half_track(new);
    }

    /// `rpmscale` = floor((1_000_000 + wobble) * 30_000 / rpm) with wobble=0.
    #[inline]
    fn rpmscale(&self) -> u64 {
        (1_000_000u64 * 30_000) / RPM
    }

    /// rotation_byte_read (rotation.c:1145-1165): the VIA2 PRA-read entry. During
    /// the attach-settle window force `GCR_read = 0`; otherwise rotate the disk.
    /// Byte-exact with VICE: in the expired-window branch it clears `attach_clk`
    /// and returns WITHOUT rotating (the rotate happens on the *next* access).
    pub fn byte_read(&mut self, clk: u64) {
        if self.attach_clk != 0 {
            if clk.wrapping_sub(self.attach_clk) < DRIVE_ATTACH_DELAY {
                self.gcr_read = 0;
            } else {
                self.attach_clk = 0;
            }
        } else {
            self.rotate_disk(clk);
        }
    }

    /// rotation_sync_found (rotation.c:1134-1143): PB7 SYNC sense. 0x80 = not
    /// found (also forced while writing or during attach settle); 0 = sync.
    ///
    /// VICE-exact: SYNC is masked while the spin-up window is still open
    /// (`attach_clk != 0`). The ADR-035 find-sync fix is achieved NOT by changing
    /// this gate but by clearing `attach_clk` on the PRB path the way VICE does —
    /// through `drive_writeprotect_sense`, which `read_prb` calls for the WPS bit
    /// and which clears `attach_clk` once `DRIVE_ATTACH_DELAY` has elapsed. See
    /// [`writeprotect_sense`] and [`prb_pin`].
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

    /// rotation_rotate_disk (rotation.c:1106-1125): motor gate, then the simple
    /// engine (D64). Wobble is a no-op (factor/frequency 0).
    ///
    /// VICE-EXACT: this path only checks the motor and runs the engine — it NEVER
    /// touches `attach_clk`. The spin-up window is cleared on the PRA path
    /// ([`byte_read`]) and the PRB/WPS path ([`writeprotect_sense`]) exactly as
    /// VICE's `rotation_byte_read` / `drive_writeprotect_sense` do, so the
    /// rotational phase matches VICE bit-for-bit.
    pub fn rotate_disk(&mut self, clk: u64) {
        if self.byte_ready_active & BRA_MOTOR_ON == 0 {
            return;
        }
        self.rotation_1541_simple(clk);
    }

    /// rotation_1541_simple (rotation.c:989-1100) READ path. Advances the head by
    /// the bit-cells elapsed since the last call, shifting GCR bits into
    /// `last_read_data`, assembling bytes into `gcr_read`, and asserting
    /// `byte_ready` per completed byte. WRITE path is out of scope.
    fn rotation_1541_simple(&mut self, clk: u64) {
        let mut delta = clk.wrapping_sub(self.rotation_last_clk);
        self.rotation_last_clk = clk;

        let rpmscale = self.rpmscale();

        // Bit-cells to advance the head, accumulated in ≤1000-cycle chunks.
        let mut bits_moved: u64 = 0;
        while delta > 0 {
            let tdelta = if delta > 1000 { 1000 } else { delta };
            delta -= tdelta;
            self.accum = self
                .accum
                .wrapping_add(ROT_SPEED_BPS[self.frequency][self.speed_zone].wrapping_mul(tdelta as u32));
            bits_moved += (self.accum as u64) / rpmscale;
            self.accum = ((self.accum as u64) % rpmscale) as u32;
        }

        if !self.read_write_mode {
            // WRITE path out of scope for milestone 2.
            return;
        }

        let track = self.image.as_ref().and_then(|img| {
            let idx = (self.current_half_track as usize).wrapping_sub(2);
            img.tracks.get(idx)
        });

        let mut off = self.gcr_head_offset;
        let mut last_read_data: u32 = self.last_read_data << 7;
        let mut bit_counter = self.bit_counter;

        // Initial `byte` window (rotation.c:1027-1031).
        let track_loaded = self.gcr_image_loaded && track.is_some();
        let mut byte: u32 = if !track_loaded {
            0
        } else {
            let t = track.unwrap();
            ((*t.data.get((off >> 3) as usize).unwrap_or(&0) as u32) << (off & 7)) & 0xffff_ffff
        };

        let mut bm = bits_moved;
        while bm != 0 {
            bm -= 1;
            byte = (byte << 1) & 0xffff_ffff;
            off += 1;
            if off & 7 == 0 {
                if (off >> 3) >= self.gcr_current_track_size as u32 {
                    off = 0;
                }
                byte = if !track_loaded {
                    0
                } else {
                    let t = track.unwrap();
                    *t.data.get((off >> 3) as usize).unwrap_or(&0) as u32
                };
            }

            last_read_data = (last_read_data << 1) & 0xffff_ffff;
            last_read_data |= (byte & 0x80) as u32;
            self.last_write_data = (self.last_write_data << 1) & 0xff;

            // SYNC test on bits 7..16 (rotation.c:1052).
            if (!last_read_data) & 0x1ff80 != 0 {
                bit_counter += 1;
                if bit_counter == 8 {
                    bit_counter = 0;
                    self.gcr_read = ((last_read_data >> 7) & 0xff) as u8;
                    self.last_write_data = self.gcr_read;
                    if self.byte_ready_active & BRA_BYTE_READY != 0 {
                        self.byte_ready_edge = 1;
                        self.byte_ready_level = 1;
                    }
                }
            } else {
                bit_counter = 0;
            }
        }

        self.last_read_data = (last_read_data >> 7) & 0x3ff;
        self.bit_counter = bit_counter;
        self.gcr_head_offset = off;
        if self.gcr_read == 0 {
            self.gcr_read = 0x11;
        }
    }

    /// VIA2 PRA pin input = the current `GCR_read` byte (via2d read_pra). The
    /// caller advances the model via [`byte_read`] first, then samples this.
    #[inline]
    pub fn pra_pin(&self) -> u8 {
        self.gcr_read
    }

    /// drive_writeprotect_sense (drive-writeprotect.c:34-75): the WPS bit, AND the
    /// spin-up-window `attach_clk` clear. Returns 0x10 = write-enabled / 0x00 =
    /// write-protected.
    ///
    /// This is the VICE-exact home of the ADR-035 find-sync fix: `read_prb` calls
    /// this for the WPS bit on EVERY PB read, and it clears `attach_clk` once
    /// `DRIVE_ATTACH_DELAY` has elapsed. So the DOS $F562 find-sync loop (which
    /// polls PB7/SYNC and PB4/WPS together via PRB) clears `attach_clk` here —
    /// unmasking SYNC — without any PRA read. The clear runs AFTER `sync_found` in
    /// the `read_prb` sequence (rotate → sync_found → writeprotect_sense), so the
    /// unmask takes effect on the NEXT poll. This replaces the ADR-035 deviation
    /// (clearing `attach_clk` inside `rotate_disk`) with VICE's actual mechanism;
    /// note the unmask happens at ~1.8M drive-clk, long before any read job, so it
    /// is decoupled from the load's rotational phase (verified by probe). Only the
    /// attach branch is modelled — a plain mounted D64 has no detach/attach_detach
    /// window.
    pub fn writeprotect_sense(&mut self, clk: u64) -> u8 {
        if self.attach_clk != 0 {
            if clk.wrapping_sub(self.attach_clk) < DRIVE_ATTACH_DELAY {
                return 0x0;
            }
            self.attach_clk = 0;
        }
        if !self.gcr_image_loaded {
            return 0x10;
        }
        0x10 // mounted D64 is writeable (read_only out of scope here)
    }

    /// VIA2 PRB pin input default (DDRB=0): `sync | wps | 0x6f` (via2d read_prb).
    ///
    /// VICE-EXACT ORDER (via2d.c read_prb): `sync_found` is sampled FIRST (while
    /// `attach_clk` may still be set → 0x80), THEN `writeprotect_sense` clears the
    /// spin-up window. The clear therefore unmasks SYNC only on the following poll.
    #[inline]
    pub fn prb_pin(&mut self, clk: u64) -> u8 {
        let sync = self.sync_found();
        let wps = self.writeprotect_sense(clk);
        sync | wps | 0x6f
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
        assert!(r.gcr_image_loaded);
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
        // Use the real sample disk so the track has actual GCR bytes (sync runs),
        // park at track 18, spin a while, and confirm the head advanced and a
        // byte got assembled.
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
}
