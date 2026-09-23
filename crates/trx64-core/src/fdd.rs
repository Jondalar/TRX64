//! fdd.rs — the 1581's floppy mechanism and its MFM surface (Spec 872 D3): a 1:1 port
//! of VICE `drive/iec/fdd.c`, D81 only.
//!
//! The unit is a byte, not a flux bit. One track is `25 × 250` = 6 250 bytes (one
//! revolution at 250 kbit/s, 300 rpm), each carrying a sync flag, so `0x1A1` is an `A1`
//! with the missing clock. Only the track under the head is resident
//! (`raw.track_head = track·2 + head`): it is built from the image on demand
//! (`update_raw`) and decoded back into the image's sectors when it is dirty and the
//! head leaves it (`flush_raw`).
//!
//! Where this differs from VICE, on purpose (Spec 872 §4):
//! - **A sector is always built from its own bytes.** VICE's `fdd_update_raw` checks
//!   only `res < 0`; a D81 error byte makes `disk_image_read_sector` return a positive
//!   code without filling the buffer, so VICE lays the previous sector's bytes down
//!   under a valid CRC. The error block is kept in the image and written back
//!   unchanged, and it does not shape the surface (owner, 2026-09-23).
//! - **No image, motor on:** VICE would build the resident track into a NULL buffer.
//!   Here the surface is a blank track (all gap bytes, no sync), so the WD sees no
//!   address mark and gives up after its index count.
//! - The image is the D81's bytes in memory (`image`); a flush writes sectors into it
//!   and bumps `image_gen`, which is how the drive knows the medium has news.

/// fdd.c:52-53 — the head moves to track 83 (`FDD_MAX_TRACK`); a D81 carries up to 83.
pub const FDD_MAX_TRACK: i32 = 80 + 3;
/// diskconstants.h:121 MAX_TRACKS_1581 — `drv->tracks` for a D81.
pub const MAX_TRACKS_1581: i32 = 83;
/// fdd.c:57 `fdd_data_rates` (kbit/s).
const FDD_DATA_RATES: [i32; 4] = [500, 300, 250, 1000];
/// fdd.c:58 INDEXLEN — the index pulse is the first 16 bytes of the revolution.
pub const INDEXLEN: i32 = 16;

/// The sizes a D81 comes in (diskimage.h:39-46): 80-83 tracks, each with or without the
/// error-info block (one byte per sector).
pub const D81_SIZES: [usize; 8] = [819_200, 822_400, 829_440, 832_680, 839_680, 842_960, 849_920, 853_240];

/// The logical track count of a D81 of `len` bytes, and whether it carries error bytes.
/// `None`: not a D81 size.
pub fn d81_geometry(len: usize) -> Option<(u32, bool)> {
    match len {
        819_200 => Some((80, false)),
        822_400 => Some((80, true)),
        829_440 => Some((81, false)),
        832_680 => Some((81, true)),
        839_680 => Some((82, false)),
        842_960 => Some((82, true)),
        849_920 => Some((83, false)),
        853_240 => Some((83, true)),
        _ => None,
    }
}

/// fdd.c:115-129 — the CCITT CRC table (`crc1021`).
fn crc1021() -> &'static [u16; 256] {
    static T: std::sync::OnceLock<[u16; 256]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u16; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut w = (i as u32) << 8;
            for _ in 0..8 {
                if w & 0x8000 != 0 {
                    w <<= 1;
                    w ^= 0x1021;
                } else {
                    w <<= 1;
                }
            }
            *e = w as u16;
        }
        t
    })
}

/// fdd.c:237-243 `fdd_crc`.
#[inline]
pub fn fdd_crc(crc: u16, b: u8) -> u16 {
    crc1021()[((crc >> 8) as u8 ^ b) as usize] ^ (crc << 8)
}

/// The resident track (`drv->raw`).
#[derive(Clone, Debug)]
pub struct RawTrack {
    pub head: i32,
    pub size: i32,
    pub track_head: i32,
    pub dirty: bool,
    pub data: Vec<u8>,
    pub sync: Vec<u8>,
}

/// `fd_drive_t` — the mechanism.
#[derive(Clone, Debug)]
pub struct Fdd {
    pub myname: String,
    pub number: i32,
    pub disk_change: bool,
    pub write_protect: bool,
    pub track: i32,
    pub tracks: i32,
    pub head: i32,
    pub sectors: i32,
    pub motor: bool,
    pub rate: i32,
    pub sector_size: i32,
    pub iso: bool,
    pub gap2: i32,
    pub gap3: i32,
    pub head_invert: i32,
    pub disk_rate: i32,
    pub image_sectors: i32,
    pub index_count: u32,
    /// The mounted D81's bytes (`drv->image`), `None` without a disk.
    pub image: Option<Vec<u8>>,
    /// The image's logical track count (80-83), from its size.
    pub image_tracks: u32,
    /// Bumped every time a flush writes sectors into `image`.
    pub image_gen: u64,
    pub raw: RawTrack,
}

impl Fdd {
    /// fdd.c:92-112 `fdd_init(num, drive)`.
    pub fn new(num: i32) -> Self {
        Self {
            myname: format!("FDD{num}"),
            number: num & 3,
            disk_change: true,
            write_protect: true,
            track: 0,
            tracks: FDD_MAX_TRACK,
            head: 0,
            sectors: 10,
            sector_size: 2,
            motor: false,
            rate: 2,
            iso: false,
            gap2: 0,
            gap3: 0,
            head_invert: 1,
            disk_rate: 0,
            image_sectors: 40,
            index_count: 0,
            image: None,
            image_tracks: 0,
            image_gen: 0,
            raw: RawTrack { head: 0, size: 0, track_head: -1, dirty: false, data: Vec::new(), sync: Vec::new() },
        }
    }

    /// fdd.c:145-205 `fdd_image_attach` (the D81 branch).
    pub fn image_attach(&mut self, bytes: Vec<u8>, read_only: bool) {
        let (tracks, _) = d81_geometry(bytes.len()).unwrap_or((80, false));
        self.image_tracks = tracks;
        self.image = Some(bytes);
        self.tracks = MAX_TRACKS_1581;
        self.sectors = 10;
        self.sector_size = 2;
        self.head_invert = 1;
        self.disk_rate = 2;
        self.iso = true;
        self.gap2 = 22;
        self.gap3 = 35;
        self.image_sectors = 40;
        self.raw.size = 25 * FDD_DATA_RATES[self.disk_rate as usize];
        self.raw.data = vec![0u8; self.raw.size as usize];
        self.raw.sync = vec![0u8; ((self.raw.size + 7) >> 3) as usize];
        self.raw.track_head = -1;
        self.raw.dirty = false;
        self.raw.head = 0;
        self.disk_change = true;
        self.write_protect = read_only;
    }

    /// fdd.c:207-220 `fdd_image_detach`. Returns the image, written back.
    pub fn image_detach(&mut self) -> Option<Vec<u8>> {
        self.flush_raw();
        let img = self.image.take();
        self.raw.data = Vec::new();
        self.raw.sync = Vec::new();
        self.disk_change = true;
        img
    }

    #[inline]
    fn has_surface(&self) -> bool {
        self.raw.size > 0
    }

    // ── the D81 sector store (disk_image_read/write_sector for a D81) ─────────

    /// The byte offset of logical track `t` (1-based), sector `s` (0-39), or `None`
    /// outside the image.
    fn sector_offset(&self, t: u32, s: u32) -> Option<usize> {
        if t < 1 || t > self.image_tracks || s >= 40 {
            return None;
        }
        Some((((t - 1) * 40 + s) * 256) as usize)
    }

    fn read_sector(&self, t: u32, s: u32, buf: &mut [u8; 256]) -> bool {
        match (self.image.as_ref(), self.sector_offset(t, s)) {
            (Some(img), Some(o)) => {
                buf.copy_from_slice(&img[o..o + 256]);
                true
            }
            _ => false,
        }
    }

    /// Decode the resident track into `img` (the body of `fdd_flush_raw`). Returns
    /// whether a sector was written.
    fn decode_raw_into(&self, img: &mut [u8]) -> bool {
        let mut wrote = false;
        let size = self.raw.size as usize;
        let ss = 128usize << self.sector_size;
        let mut data = vec![0u8; ss];
        let mut p = 0usize;
        for s in 0..self.sectors {
            let mut step = 0;
            let mut d = 0usize;
            let mut i = 0usize;
            while i < size * 2 {
                let mut w = self.raw.data[p] as u16;
                if self.raw.sync[p >> 3] & (0x80 >> (p & 7)) != 0 {
                    w |= 0x100;
                }
                p += 1;
                if p >= size {
                    p = 0;
                }
                i += 1;
                let mut reset_step = true;
                match step {
                    0 => {
                        if w == 0x00 {
                            step += 1;
                        }
                        reset_step = false;
                    }
                    1 => {
                        if w == 0x00 {
                            reset_step = false;
                        } else if w == 0x1a1 {
                            step += 1;
                            reset_step = false;
                        }
                    }
                    2 => {
                        if w == 0x1a1 {
                            reset_step = false;
                        } else if w == 0xfe {
                            step += 1;
                            reset_step = false;
                        }
                    }
                    3 => {
                        if w as i32 == self.raw.track_head / 2 {
                            step += 1;
                            reset_step = false;
                        }
                    }
                    4 => {
                        if w as i32 == ((self.raw.track_head & 1) ^ self.head_invert) {
                            step += 1;
                            reset_step = false;
                        }
                    }
                    5 => {
                        if w as i32 == s + 1 {
                            step += 1;
                            reset_step = false;
                        }
                    }
                    6 => {
                        if w as i32 == self.sector_size {
                            step += 1;
                            reset_step = false;
                        }
                    }
                    7 | 8 => {
                        step += 1;
                        reset_step = false;
                    }
                    9 => {
                        if w == 0x00 {
                            step += 1;
                        }
                        reset_step = false;
                    }
                    10 => {
                        if w == 0x00 {
                        } else if w == 0x1a1 {
                            step += 1;
                        } else {
                            step = 9;
                        }
                        reset_step = false;
                    }
                    11 => {
                        if w == 0x1a1 {
                            reset_step = false;
                        } else if w == 0xfb {
                            step += 1;
                            reset_step = false;
                        }
                    }
                    12 => {
                        data[d] = w as u8;
                        d += 1;
                        if d >= ss {
                            step += 1;
                        }
                        reset_step = false;
                    }
                    13 => {
                        step += 1;
                        reset_step = false;
                    }
                    14 => {
                        let mut sec = ((self.raw.track_head ^ self.head_invert) * self.sectors + s) as u32;
                        sec <<= self.sector_size - 1;
                        let mut t = sec / self.image_sectors as u32 + 1;
                        let mut sc = sec % self.image_sectors as u32;
                        let mut j = 0;
                        while j < (1 << self.sector_size) {
                            if let Some(o) = self.sector_offset(t, sc) {
                                let src = &data[(j as usize) * 128..(j as usize) * 128 + 256];
                                img[o..o + 256].copy_from_slice(src);
                                wrote = true;
                            }
                            sc = (sc + 1) % self.image_sectors as u32;
                            if sc == 0 {
                                t += 1;
                            }
                            j += 2;
                        }
                        i = size * 2;
                        // falls into `default` (NOP) and then `step = 0`
                    }
                    _ => {}
                }
                if reset_step {
                    step = 0;
                }
            }
        }
        wrote
    }

    /// fdd.c:245-433 `fdd_flush_raw`.
    pub fn flush_raw(&mut self) {
        if !self.raw.dirty {
            return;
        }
        self.raw.dirty = false;
        if self.raw.track_head / 2 < self.tracks && self.image.is_some() {
            let mut img = self.image.take().unwrap();
            if self.decode_raw_into(&mut img) {
                self.image_gen += 1;
            }
            self.image = Some(img);
        }
    }

    /// The image with the resident track decoded into it, built on a copy — the D81 a
    /// flush would leave, without flushing (the dirty track stays dirty).
    pub fn image_as_written(&self) -> Option<Vec<u8>> {
        let mut img = self.image.clone()?;
        if self.raw.dirty && self.raw.track_head / 2 < self.tracks {
            self.decode_raw_into(&mut img);
        }
        Some(img)
    }

    fn raw_write(&mut self, p: &mut usize, b: u8, sync: bool) {
        let size = self.raw.size as usize;
        self.raw.data[*p] = b;
        if sync {
            self.raw.sync[*p >> 3] |= 0x80u8 >> (*p & 7);
        } else {
            self.raw.sync[*p >> 3] &= (0xff7fu16 >> (*p & 7)) as u8;
        }
        *p += 1;
        if *p >= size {
            *p = 0;
        }
    }

    /// fdd.c:435-557 `fdd_update_raw` — build the resident track for the head. A sector
    /// comes from its own bytes, always (module doc).
    fn update_raw(&mut self) {
        if !self.has_surface() {
            return;
        }
        if self.track * 2 + self.head == self.raw.track_head {
            return;
        }
        if self.raw.dirty {
            self.flush_raw();
        }
        self.raw.track_head = self.track * 2 + self.head;

        self.raw.data.fill(0x4e);
        self.raw.sync.fill(0);

        if self.track < self.tracks && self.image.is_some() {
            let mut i = (self.track * 2 + (self.head ^ self.head_invert)) * self.sectors;
            i <<= self.sector_size - 1;
            let mut dt = (i / self.image_sectors + 1) as u32;
            let mut ds = (i % self.image_sectors) as u32;

            // D81: ISO, GAP 4a with no index mark.
            let mut p = 32usize;
            let mut buffer = [0u8; 256];
            for s in 0..self.sectors {
                for _ in 0..12 {
                    self.raw_write(&mut p, 0x00, false);
                }
                for _ in 0..3 {
                    self.raw_write(&mut p, 0xa1, true);
                }
                self.raw_write(&mut p, 0xfe, false);
                let tb = self.track as u8;
                let hb = (self.head ^ self.head_invert) as u8;
                let sb = (s + 1) as u8;
                let zb = self.sector_size as u8;
                self.raw_write(&mut p, tb, false);
                let mut crc = fdd_crc(0xb230, tb);
                self.raw_write(&mut p, hb, false);
                crc = fdd_crc(crc, hb);
                self.raw_write(&mut p, sb, false);
                crc = fdd_crc(crc, sb);
                self.raw_write(&mut p, zb, false);
                crc = fdd_crc(crc, zb);
                self.raw_write(&mut p, (crc >> 8) as u8, false);
                self.raw_write(&mut p, crc as u8, false);
                for _ in 0..self.gap2 {
                    self.raw_write(&mut p, 0x4e, false);
                }
                crc = 0xe295;
                let mut j = 0;
                while j < (1 << self.sector_size) {
                    if !self.read_sector(dt, ds, &mut buffer) {
                        // `res < 0` — outside the image: the track ends here, as in VICE.
                        return;
                    }
                    if j == 0 {
                        for _ in 0..12 {
                            self.raw_write(&mut p, 0x00, false);
                        }
                        for _ in 0..3 {
                            self.raw_write(&mut p, 0xa1, true);
                        }
                        self.raw_write(&mut p, 0xfb, false);
                    }
                    for &b in buffer.iter() {
                        self.raw_write(&mut p, b, false);
                        crc = fdd_crc(crc, b);
                    }
                    ds = (ds + 1) % self.image_sectors as u32;
                    if ds == 0 {
                        dt += 1;
                    }
                    j += 2;
                }
                self.raw_write(&mut p, (crc >> 8) as u8, false);
                self.raw_write(&mut p, (crc & 0xff) as u8, false);
                for _ in 0..self.gap3 {
                    self.raw_write(&mut p, 0x4e, false);
                }
            }
        }
    }

    /// fdd.c:559-577 `fdd_rotate`.
    pub fn rotate(&mut self, bytes_in: u64) -> u64 {
        if !self.motor || self.image.is_none() || !self.has_surface() {
            return bytes_in;
        }
        let size = self.raw.size as u64;
        let head = self.raw.head as u64 + bytes_in;
        self.index_count = self.index_count.wrapping_add((head / size) as u32);
        self.raw.head = (head % size) as i32;
        bytes_in
    }

    /// fdd.c:579-585 `fdd_index`.
    pub fn index(&self) -> bool {
        self.raw.head < INDEXLEN
    }
    pub fn index_count_reset(&mut self) {
        self.index_count = 0;
    }
    pub fn index_count(&self) -> u32 {
        self.index_count
    }
    pub fn track0(&self) -> bool {
        self.track == 0
    }
    pub fn write_protect(&self) -> bool {
        self.write_protect
    }
    pub fn disk_change(&self) -> bool {
        self.disk_change
    }

    /// fdd.c:626-652 `fdd_read`. Without a disk the surface is blank (module doc).
    pub fn read(&mut self) -> u16 {
        if !self.motor || !self.has_surface() {
            return 0;
        }
        let mut p = self.raw.head as usize;
        let data = if self.disk_rate == self.rate {
            self.update_raw();
            let mut d = self.raw.data[p] as u16;
            if self.raw.sync[p >> 3] & (0x80 >> (p & 7)) != 0 {
                d |= 0x100;
            }
            d
        } else {
            0
        };
        p += 1;
        if p >= self.raw.size as usize {
            p = 0;
            self.index_count = self.index_count.wrapping_add(1);
        }
        self.raw.head = p as i32;
        data
    }

    /// fdd.c:654-679 `fdd_write`.
    pub fn write(&mut self, data: u16) -> i32 {
        if !self.motor || !self.has_surface() {
            return -1;
        }
        self.update_raw();
        let mut p = self.raw.head as usize;
        if self.disk_rate == self.rate {
            self.raw.data[p] = data as u8;
            if data & 0x100 != 0 {
                self.raw.sync[p >> 3] |= 0x80u8 >> (p & 7);
            } else {
                self.raw.sync[p >> 3] &= (0xff7fu16 >> (p & 7)) as u8;
            }
            self.raw.dirty = true;
        }
        p += 1;
        if p >= self.raw.size as usize {
            p = 0;
            self.index_count = self.index_count.wrapping_add(1);
        }
        self.raw.head = p as i32;
        0
    }

    /// fdd.c:689-723 `fdd_seek_pulse`. (The UI's `current_half_track` is `head()`.)
    pub fn seek_pulse(&mut self, dir: bool) {
        if self.motor {
            self.track += if dir { 1 } else { -1 };
        }
        if self.image.is_some() {
            self.disk_change = false;
        }
        self.track = self.track.clamp(0, FDD_MAX_TRACK);
    }

    /// fdd.c:725-731 `fdd_select_head`.
    pub fn select_head(&mut self, head: i32) {
        self.head = head & 1;
    }
    /// fdd.c:733-739 `fdd_set_motor`.
    pub fn set_motor(&mut self, motor: bool) {
        self.motor = motor;
    }

    // ── FDD 1.0 snapshot module (fdd.c:747-902) ──────────────────────────────

    /// Write the `FDD<4n>` module. `raw.head` is written whole: VICE's
    /// `SMW_DW(m, (uint8_t)drv->raw.head)` truncates it to 8 bits (Spec 872 §6).
    pub fn snapshot_write_module(&self, s: &mut crate::vice_snapshot_stream::SnapshotT) {
        let mut m = s.module_create(&self.myname, 1, 0);
        s.smw_b(&mut m, self.number as u8);
        s.smw_b(&mut m, self.disk_change as u8);
        s.smw_b(&mut m, self.write_protect as u8);
        s.smw_b(&mut m, self.track as u8);
        s.smw_b(&mut m, self.tracks as u8);
        s.smw_b(&mut m, self.head as u8);
        s.smw_b(&mut m, self.sectors as u8);
        s.smw_b(&mut m, self.motor as u8);
        s.smw_b(&mut m, self.rate as u8);
        s.smw_b(&mut m, self.sector_size as u8);
        s.smw_b(&mut m, self.iso as u8);
        s.smw_b(&mut m, self.gap2 as u8);
        s.smw_b(&mut m, self.gap3 as u8);
        s.smw_b(&mut m, self.head_invert as u8);
        s.smw_b(&mut m, self.disk_rate as u8);
        s.smw_dw(&mut m, self.image_sectors as u32);
        s.smw_dw(&mut m, self.index_count);
        s.smw_dw(&mut m, self.raw.head as u32);
        s.smw_b(&mut m, self.raw.track_head as u8);
        s.smw_b(&mut m, self.raw.dirty as u8);
        s.smw_ba(&mut m, &self.raw.data, self.raw.data.len());
        s.smw_ba(&mut m, &self.raw.sync, self.raw.sync.len());
        s.module_close(&m);
    }

    /// Read the `FDD<4n>` module: the mechanism and the resident track, byte for byte.
    /// The image itself is the medium's and is mounted before this runs.
    pub fn snapshot_read_module(&mut self, s: &mut crate::vice_snapshot_stream::SnapshotT) -> Result<(), String> {
        let name = self.myname.clone();
        let (m, major, minor) = s.module_open(&name).ok_or_else(|| format!("{name}: module missing"))?;
        if crate::vice_snapshot_stream::snapshot_version_is_bigger(major, minor, 1, 0) {
            return Err(format!("{name}: module version {major}.{minor} is newer than 1.0"));
        }
        macro_rules! rb {
            () => {
                s.smr_b().ok_or_else(|| format!("{name}: truncated"))? as i32
            };
        }
        macro_rules! rdw {
            () => {
                s.smr_dw().ok_or_else(|| format!("{name}: truncated"))?
            };
        }
        self.number = rb!();
        self.disk_change = rb!() != 0;
        self.write_protect = rb!() != 0;
        self.track = rb!();
        self.tracks = rb!();
        self.head = rb!();
        self.sectors = rb!();
        self.motor = rb!() != 0;
        self.rate = rb!();
        self.sector_size = rb!();
        self.iso = rb!() != 0;
        self.gap2 = rb!();
        self.gap3 = rb!();
        self.head_invert = rb!();
        self.disk_rate = rb!();
        self.image_sectors = rdw!() as i32;
        self.index_count = rdw!();
        self.raw.head = rdw!() as i32;
        let th = rb!();
        // track_head rides as a BYTE (VICE); -1 ("nothing resident") comes back as 255.
        self.raw.track_head = if th == 0xff { -1 } else { th };
        self.raw.dirty = rb!() != 0;

        self.track = self.track.clamp(0, FDD_MAX_TRACK);
        self.tracks = self.tracks.clamp(0, MAX_TRACKS_1581);
        self.head &= 1;
        self.rate &= 3;
        self.sector_size &= 3;
        self.disk_rate &= 3;
        self.raw.size = 25 * FDD_DATA_RATES[self.disk_rate as usize];
        self.raw.head %= self.raw.size;
        self.raw.data = vec![0u8; self.raw.size as usize];
        self.raw.sync = vec![0u8; ((self.raw.size + 7) >> 3) as usize];
        let n = self.raw.data.len();
        if !s.smr_ba(&mut self.raw.data, n) {
            return Err(format!("{name}: truncated (raw.data)"));
        }
        let n = self.raw.sync.len();
        if !s.smr_ba(&mut self.raw.sync, n) {
            return Err(format!("{name}: truncated (raw.sync)"));
        }
        s.module_close(&m);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d81_with(fill: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let mut v = vec![0u8; 819_200];
        for t in 1..=80u32 {
            for s in 0..40u32 {
                let o = (((t - 1) * 40 + s) * 256) as usize;
                v[o..o + 256].fill(fill(t, s));
            }
        }
        v
    }

    /// The layout of fdd.c: gap 4a of 32 × 4E, then per sector 12 × 00, 3 × A1*, FE,
    /// track, side, sector, 02, CRC; the ID's side byte is `head ^ 1`; physical head 1
    /// carries logical sectors 0-19; each ID and data CRC checks to zero.
    #[test]
    fn a_resident_track_is_the_iso_layout_with_inverted_sides() {
        let mut f = Fdd::new(0);
        f.image_attach(d81_with(|t, s| (t * 40 + s) as u8), false);
        f.set_motor(true);
        f.track = 5;
        f.select_head(1);
        let _ = f.read();
        let raw: Vec<u16> = (0..f.raw.size as usize)
            .map(|p| f.raw.data[p] as u16 | if f.raw.sync[p >> 3] & (0x80 >> (p & 7)) != 0 { 0x100 } else { 0 })
            .collect();
        assert!(raw[..32].iter().all(|&w| w == 0x4e), "gap 4a");
        assert!(raw[32..44].iter().all(|&w| w == 0), "12 × 00");
        assert_eq!(&raw[44..48], &[0x1a1, 0x1a1, 0x1a1, 0xfe]);
        assert_eq!(&raw[48..52], &[5, 0, 1, 2], "track 5, side byte 0 on head 1, sector 1");
        let mut crc = 0xb230;
        for &b in &raw[48..54] {
            crc = fdd_crc(crc, b as u8);
        }
        assert_eq!(crc, 0, "the ID CRC checks");
        // Data field of sector 1: logical track 6, sector 0 → fill byte 6*40+0.
        let data_start = 54 + 22 + 12 + 3 + 1;
        assert_eq!(raw[data_start - 1], 0xfb);
        assert_eq!(raw[data_start], ((6 * 40) & 0xff) as u16);
        let mut crc = 0xe295;
        for &b in &raw[data_start..data_start + 514] {
            crc = fdd_crc(crc, b as u8);
        }
        assert_eq!(crc, 0, "the data CRC checks");
    }

    /// Spec 872 §4 / §10.6 — an image with the error block: every sector is built from its
    /// own bytes whatever its error byte says, and a write-back leaves the block as it was.
    #[test]
    fn d81_error_bytes_are_kept_and_ignored() {
        let mut img = d81_with(|t, s| (t + s) as u8);
        img.resize(822_400, 0);
        for (i, e) in img[819_200..].iter_mut().enumerate() {
            *e = if i % 3 == 0 { 0x05 } else { 0x01 }; // 5 = a hard (data CRC) error in VICE
        }
        let errors = img[819_200..].to_vec();
        let mut f = Fdd::new(0);
        f.image_attach(img, false);
        assert_eq!(f.image_tracks, 80);
        f.set_motor(true);
        f.track = 2;
        f.select_head(1); // logical track 3, sectors 0-19
        let _ = f.read();
        // Physical sector 1 = logical 3/0 and 3/1: its own fill bytes, CRC good.
        let data = 32 + 12 + 3 + 1 + 4 + 2 + 22 + 12 + 3 + 1;
        assert_eq!(f.raw.data[data], 3, "3/0 from its own bytes");
        assert_eq!(f.raw.data[data + 256], 4, "3/1 from its own bytes");
        let mut crc = 0xe295;
        for &b in &f.raw.data[data..data + 514] {
            crc = fdd_crc(crc, b);
        }
        assert_eq!(crc, 0, "a valid data CRC whatever the error byte");
        // A written track goes back without touching the error block.
        f.raw.data[data] = 0x99;
        f.raw.dirty = true;
        f.flush_raw();
        let out = f.image.as_ref().unwrap();
        assert_eq!(out.len(), 822_400);
        assert_eq!(out[(2 * 40) * 256], 0x99, "3/0 written");
        assert_eq!(&out[819_200..], &errors[..], "the error block as it was");
    }

    /// A written track decodes back into the image — the sectors change, nothing else.
    #[test]
    fn a_dirty_track_flushes_into_its_own_sectors() {
        let orig = d81_with(|_, _| 0x11);
        let mut f = Fdd::new(0);
        f.image_attach(orig.clone(), false);
        f.set_motor(true);
        f.track = 39;
        f.select_head(1); // logical side 0 of physical track 39 = logical track 40, 0-19
        let _ = f.read();
        // Overwrite the data bytes of physical sector 3 in the resident buffer.
        let per = 12 + 3 + 1 + 4 + 2 + 22 + 12 + 3 + 1 + 512 + 2 + 35;
        let data = 32 + 2 * per + 12 + 3 + 1 + 4 + 2 + 22 + 12 + 3 + 1;
        for k in 0..512 {
            f.raw.data[data + k] = 0xa5;
        }
        f.raw.dirty = true;
        f.flush_raw();
        let img = f.image.as_ref().unwrap();
        let diff: Vec<usize> = (0..img.len()).filter(|&i| img[i] != orig[i]).map(|i| i / 256).collect();
        let first = ((40 - 1) * 40 + 4) as usize;
        assert!(diff.iter().all(|&s| s == first || s == first + 1), "only sectors 40/4-5 changed");
        assert_eq!(diff.len(), 512);
    }
}
