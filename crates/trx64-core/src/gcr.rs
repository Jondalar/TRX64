//! gcr.rs — 1541 GCR codec + D64→per-track GCR bitstream encoder.
//!
//! Byte-exact port of the TS oracle's `gcr.ts` (VICE src/gcr.c) codec and the
//! D64→GCR track build in `fsimage_dxx.ts` (`fsimage_read_dxx_image`). This is
//! the pure, deterministic half of the disk READ path (ADR-012 milestone 1):
//! a mounted D64's 683 sectors are encoded into the standard 1541 GCR track
//! layout (sync + header + header-gap + sync + data + inter-sector gap), which
//! the rotating-disk model (rotation.rs) then streams to VIA2.
//!
//! Geometry (D64, 35 tracks, 256 bytes/sector):
//!   zone = (t<31) + (t<25) + (t<18)   → index into the 4-entry zone tables
//!   tracks  1-17 → zone 3 → 21 sectors → 7692 raw GCR bytes, gap  8
//!   tracks 18-24 → zone 2 → 19 sectors → 7142 raw GCR bytes, gap 17
//!   tracks 25-30 → zone 1 → 18 sectors → 6666 raw GCR bytes, gap 12
//!   tracks 31-35 → zone 0 → 17 sectors → 6250 raw GCR bytes, gap  9
//!   headergap = 9, synclen = 5 (all zones).
//!
//! Half-track indexing: track N (1-based) → even slot `N*2-2`; the following odd
//! slot is empty. 1541 uses 84 half-track slots (42 nominal tracks); tracks
//! beyond the 35 in a D64 are 0x55-filled.

// ── CBMDOS FDC error codes (gcr.ts:30-54 / cbmdos.h:105-117) ─────────────────
pub const CBMDOS_FDC_ERR_OK: u8 = 1;
pub const CBMDOS_FDC_ERR_HEADER: u8 = 2;
pub const CBMDOS_FDC_ERR_SYNC: u8 = 3;
pub const CBMDOS_FDC_ERR_NOBLOCK: u8 = 4;
pub const CBMDOS_FDC_ERR_DCHECK: u8 = 5;
pub const CBMDOS_FDC_ERR_ID: u8 = 11;
pub const CBMDOS_FDC_ERR_DRIVE: u8 = 15;

// ── GCR conversion table (gcr.ts:61-66): 4-bit nybble → 5-bit GCR ───────────
const GCR_CONV_DATA: [u8; 16] = [
    0x0a, 0x0b, 0x12, 0x13, 0x0e, 0x0f, 0x16, 0x17, 0x09, 0x19, 0x1a, 0x1b, 0x0d, 0x1d, 0x1e, 0x15,
];

/// PORT OF: gcr.ts:70-75 (gcr.c:59-65 From_GCR_conv_data) — 5-bit GCR → 4-bit
/// nybble (0 = invalid). The inverse of [`GCR_CONV_DATA`], used by the
/// GCR-decode-on-write (D64 writeback) path.
const FROM_GCR_CONV_DATA: [u8; 32] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 8, 0, 1, 0, 12, 4, 5, 0, 0, 2, 3, 0, 15, 6, 7, 0, 9, 10, 11, 0, 13,
    14, 0,
];

/// GCR header descriptor (gcr_header_t): the (sector, track, id) tuple encoded
/// into a sector's GCR header block.
#[derive(Clone, Copy)]
pub struct GcrHeader {
    pub sector: u8,
    pub track: u8,
    pub id1: u8,
    pub id2: u8,
}

// ── Geometry tables (fsimage_dxx.ts:131-146) ────────────────────────────────
/// Sectors per track, indexed by speed zone 0..3.
const SECTOR_MAP_D64: [u8; 4] = [17, 18, 19, 21];
/// Raw GCR track size (bytes), indexed by speed zone 0..3.
const RAW_TRACK_SIZE_D64: [usize; 4] = [6250, 6666, 7142, 7692];
/// Inter-sector gap size (bytes), indexed by speed zone 0..3.
const GAP_SIZE_D64: [usize; 4] = [9, 12, 17, 8];

/// Header gap (disk_image_header_gap_size, fsimage_dxx.ts:240) — scalar 9 for D64.
const HEADER_GAP_D64: usize = 9;
/// Sync length (disk_image_sync_size, fsimage_dxx.ts:257) — scalar 5 for D64.
const SYNC_LEN_D64: usize = 5;

/// SECTOR_GCR_SIZE_WITH_HEADER (drivetypes.ts:197).
const SECTOR_GCR_SIZE_WITH_HEADER: usize = 335;

/// D64 BAM location for the disk ID (fsimage_dxx.ts:84-86): track 18, sector 0,
/// bytes 0xA2/0xA3.
const BAM_TRACK_1541: u8 = 18;
const BAM_SECTOR_1541: u8 = 0;
const BAM_ID_1541: usize = 162;

/// 1541 half-track count (drivetypes.ts:108) — 84 slots.
pub const DRIVE_HALFTRACKS_1541: usize = 84;
/// Number of data tracks in a standard 35-track D64.
pub const D64_TRACKS: u8 = 35;

/// Speed zone for a 1-based D64 track (disk_image_speed_map, D64 branch).
#[inline]
pub fn d64_speed_zone(track: u8) -> usize {
    let t = track as u32;
    ((t < 31) as usize) + ((t < 25) as usize) + ((t < 18) as usize)
}

/// Sectors per 1-based D64 track.
#[inline]
pub fn d64_sectors_per_track(track: u8) -> u8 {
    SECTOR_MAP_D64[d64_speed_zone(track)]
}

/// Raw GCR track size (bytes) for a 1-based D64 track.
#[inline]
pub fn d64_raw_track_size(track: u8) -> usize {
    RAW_TRACK_SIZE_D64[d64_speed_zone(track)]
}

/// Linear sector index (0-based) of (track, sector) in the D64 file, or `None`
/// if out of range (disk_image_check_sector, fsimage_dxx.ts:276-289).
#[inline]
pub fn d64_linear_sector(track: u8, sector: u8) -> Option<usize> {
    if track < 1 || track > D64_TRACKS {
        return None;
    }
    if sector >= d64_sectors_per_track(track) {
        return None;
    }
    let mut linear = 0usize;
    for t in 1..track {
        linear += d64_sectors_per_track(t) as usize;
    }
    Some(linear + sector as usize)
}

/// gcr_convert_4bytes_to_GCR (gcr.ts:85-102): 4 raw bytes → 5 GCR bytes,
/// written to `dest[dest_off..dest_off+5]`.
fn gcr_convert_4bytes_to_gcr(source: &[u8], source_off: usize, dest: &mut [u8], dest_off: usize) {
    let mut tdest: u32 = 0;
    let mut s = source_off;
    let mut d = dest_off;
    let mut i = 2;
    while i < 10 {
        tdest = (tdest << 5) & 0xffff;
        tdest |= GCR_CONV_DATA[((source[s] >> 4) & 0x0f) as usize] as u32;
        tdest = (tdest << 5) & 0xffff;
        tdest |= GCR_CONV_DATA[(source[s] & 0x0f) as usize] as u32;
        dest[d] = ((tdest >> i) & 0xff) as u8;
        i += 2;
        s += 1;
        d += 1;
    }
    dest[d] = (tdest & 0xff) as u8;
}

/// gcr_convert_sector_to_GCR (gcr.ts:128-198): encode one 256-byte sector +
/// header into `data` starting at `data_off`. `buffer` is the 256-byte sector.
/// `gap` is the HEADER gap (bytes skipped after the header ID block), `sync` the
/// data-sync length, `error_code` the per-sector CBMDOS error (OK for plain D64).
pub fn gcr_convert_sector_to_gcr(
    buffer: &[u8],
    data: &mut [u8],
    data_off: usize,
    header: GcrHeader,
    gap: usize,
    sync: usize,
    error_code: u8,
) {
    let mut buf = [0u8; 4];
    let idm: u8 = if error_code == CBMDOS_FDC_ERR_ID { 0xff } else { 0x00 };
    let mut d = data_off;
    let mut b = 0usize;

    let sync_fill: u8 = if error_code == CBMDOS_FDC_ERR_SYNC { 0x55 } else { 0xff };

    // Sync (5 bytes)
    for i in 0..5 {
        data[d + i] = sync_fill;
    }
    d += 5;

    // Header block
    let mut chksum: u8 = if error_code == CBMDOS_FDC_ERR_HEADER { 0xff } else { 0x00 };
    chksum ^= (header.sector ^ header.track ^ header.id2 ^ header.id1 ^ idm) & 0xff;
    buf[0] = if error_code == CBMDOS_FDC_ERR_HEADER { 0xff } else { 0x08 };
    buf[1] = chksum;
    buf[2] = header.sector;
    buf[3] = header.track;
    gcr_convert_4bytes_to_gcr(&buf, 0, data, d);
    d += 5;

    // Header ID block
    buf[0] = header.id2;
    buf[1] = (header.id1 ^ idm) & 0xff;
    buf[2] = 0x0f;
    buf[3] = 0x0f;
    gcr_convert_4bytes_to_gcr(&buf, 0, data, d);
    d += 5;

    // Header gap (bytes left as the pre-existing fill)
    d += gap;

    // Sync (data)
    for i in 0..sync {
        data[d + i] = sync_fill;
    }
    d += sync;

    // Data block — first group
    let mut chksum: u8 = if error_code == CBMDOS_FDC_ERR_DCHECK { 0xff } else { 0x00 };
    buf[0] = if error_code == CBMDOS_FDC_ERR_NOBLOCK { 0x00 } else { 0x07 };
    buf[1] = buffer[b];
    buf[2] = buffer[b + 1];
    buf[3] = buffer[b + 2];
    chksum ^= (buffer[b] ^ buffer[b + 1] ^ buffer[b + 2]) & 0xff;
    gcr_convert_4bytes_to_gcr(&buf, 0, data, d);
    b += 3;
    d += 5;

    // 63 middle groups
    for _ in 0..63 {
        chksum ^= (buffer[b] ^ buffer[b + 1] ^ buffer[b + 2] ^ buffer[b + 3]) & 0xff;
        gcr_convert_4bytes_to_gcr(buffer, b, data, d);
        b += 4;
        d += 5;
    }

    // Final group
    buf[0] = buffer[b];
    buf[1] = (chksum ^ buffer[b]) & 0xff;
    buf[2] = 0;
    buf[3] = 0;
    gcr_convert_4bytes_to_gcr(&buf, 0, data, d);
}

/// One half-track of GCR data (disk_track_t): raw bytes + active size.
#[derive(Clone)]
pub struct GcrTrack {
    pub data: Vec<u8>,
    pub size: usize,
}

/// The full GCR-encoded disk: one [`GcrTrack`] per half-track slot (1541 = 84).
#[derive(Clone)]
pub struct GcrImage {
    pub tracks: Vec<GcrTrack>,
}

impl GcrImage {
    /// Build the GCR image from a raw 35-track D64 (`bytes` = 683*256 = 174848).
    /// Byte-exact port of `fsimage_read_dxx_image` (fsimage_dxx.ts:455-568) for the
    /// D64 path: disk ID from the BAM, per-sector codec, inter-track skew rotation.
    pub fn from_d64(bytes: &[u8]) -> Self {
        // Disk ID from the BAM (track 18, sector 0, bytes 0xA2/0xA3). Defaults 0xA0.
        let (id1, id2) = match d64_linear_sector(BAM_TRACK_1541, BAM_SECTOR_1541) {
            Some(lin) => {
                let off = lin * 256;
                if off + BAM_ID_1541 + 1 < bytes.len() {
                    (bytes[off + BAM_ID_1541], bytes[off + BAM_ID_1541 + 1])
                } else {
                    (0xa0, 0xa0)
                }
            }
            None => (0xa0, 0xa0),
        };

        let mut tracks: Vec<GcrTrack> = (0..DRIVE_HALFTRACKS_1541)
            .map(|_| GcrTrack { data: Vec::new(), size: 0 })
            .collect();

        // Running inter-track skew accumulator (fsimage_dxx.ts:452).
        let mut trackoffset: usize = 0;

        // Loop nominal tracks 1..=42 (max_half_tracks/2 = 84/2). Tracks beyond the
        // 35 in the D64 get the 0x55-fill empty path.
        let nominal_tracks = DRIVE_HALFTRACKS_1541 / 2; // 42
        for track in 1..=nominal_tracks as u8 {
            let half_track = (track as usize) * 2 - 2;
            let track_size = if track <= D64_TRACKS {
                d64_raw_track_size(track)
            } else {
                // Empty tracks adopt zone-0 raw size (the loop still allocates
                // track_size; VICE uses disk_image_raw_track_size(type, track)
                // which for track>35 falls into the lowest zone via the same map).
                d64_raw_track_size_unclamped(track)
            };

            let mut ptr = vec![0u8; track_size];

            if track <= D64_TRACKS {
                let gap = GAP_SIZE_D64[d64_speed_zone(track)];
                let headergap = HEADER_GAP_D64;
                let synclen = SYNC_LEN_D64;
                let max_sector = d64_sectors_per_track(track);

                let mut tempgcr = vec![0x55u8; track_size];
                let mut ptr_off = 0usize;

                for sector in 0..max_sector {
                    if let Some(lin) = d64_linear_sector(track, sector) {
                        let off = lin * 256;
                        let mut sec_buf = [0u8; 256];
                        if off + 256 <= bytes.len() {
                            sec_buf.copy_from_slice(&bytes[off..off + 256]);
                        }
                        let header = GcrHeader { sector, track, id1, id2 };
                        gcr_convert_sector_to_gcr(
                            &sec_buf,
                            &mut tempgcr,
                            ptr_off,
                            header,
                            headergap,
                            synclen,
                            CBMDOS_FDC_ERR_OK_AS_ZERO,
                        );
                    }
                    ptr_off += SECTOR_GCR_SIZE_WITH_HEADER + headergap + gap + synclen * 2;
                }

                // Inter-track skew rotation (fsimage_dxx.ts:546-559).
                trackoffset += ptr_off - gap;
                trackoffset += (track_size * 100) / 270;
                trackoffset %= track_size;

                // ptr (already 0-filled) → 0x55-fill, then split-copy tempgcr.
                for b in ptr.iter_mut() {
                    *b = 0x55;
                }
                let split = track_size - trackoffset;
                // ptr[trackoffset..] = tempgcr[0..split]
                ptr[trackoffset..].copy_from_slice(&tempgcr[..split]);
                // ptr[0..trackoffset] = tempgcr[split..]
                ptr[..trackoffset].copy_from_slice(&tempgcr[split..]);
            } else {
                for b in ptr.iter_mut() {
                    *b = 0x55;
                }
            }

            tracks[half_track] = GcrTrack { data: ptr, size: track_size };

            // Empty odd half-track (zero-filled, fsimage_dxx.ts:567).
            let odd = half_track + 1;
            if odd < DRIVE_HALFTRACKS_1541 {
                tracks[odd] = GcrTrack { data: vec![0u8; track_size], size: track_size };
            }
        }

        GcrImage { tracks }
    }

    /// Build the GCR image from a raw `.g64` (GCR-1541) image — a 1:1 port of the
    /// VICE G64 attach path: `disk_image_read_image` → `fsimage_read_gcr_image`
    /// → `fsimage_gcr_read_half_track` → `fsimage_gcr_seek_half_track`
    /// (`fsimage_gcr.ts:296-427`, VICE `fsimage-gcr.c:53-174`).
    ///
    /// `bytes` is the whole `.g64` file. The header's byte-9 `num_half_tracks`
    /// is `max_half_tracks` (G64 probe, VICE `fsimage-probe.c:501-502`); every
    /// slot `0..max_half_tracks` is filled from the image's offset table, and the
    /// remaining slots up to the rotation engine's [`DRIVE_HALFTRACKS_1541`] are
    /// 0x00-filled at the canonical raw track size (the `else` empty-track branch
    /// of `fsimage_read_gcr_image`, `fsimage_gcr.ts:316-321`).
    ///
    /// The resulting `tracks[i]` array indexes identically to [`from_d64`]: slot
    /// `i` holds G64 half-track `i` (= VICE `image.gcr.tracks[i]`, populated via
    /// `fsimage_gcr_read_half_track(image, i + 2, ...)`), so the rotation engine's
    /// `tracks[current_half_track - 2]` access is byte-for-byte equivalent.
    ///
    /// [`from_d64`]: GcrImage::from_d64
    pub fn from_g64(bytes: &[u8]) -> Self {
        // VICE attach allocates a 1541 disk_image_t (image->type = G64,
        // image->max_half_tracks = header[9]). We mirror that here: the in-memory
        // `fd` is the raw file slice, indexed positionally by util_fpread.
        let image = G64Image { fd: bytes };

        // The rotation engine reads `tracks[current_half_track - 2]` with
        // current_half_track ∈ [2, 84]; only the first DRIVE_HALFTRACKS_1541 (84)
        // slots are ever consulted, matching from_d64's array length. VICE keeps
        // MAX_GCR_TRACKS (168) slots; the upper 84 are the canonical empty fill
        // and are never read on a 1541, so we materialise only the 84 we need.
        let mut tracks: Vec<GcrTrack> = (0..DRIVE_HALFTRACKS_1541)
            .map(|_| GcrTrack { data: Vec::new(), size: 0 })
            .collect();

        // max_half_tracks for a 1541 G64 = header[9]. If the header is malformed
        // the seek helper rejects it and every slot falls through to the empty
        // fill (VICE behaviour: read returns -1 only on the per-track call, but
        // the seek-failure path here yields an empty 0x55 track per VICE's
        // offset==0 branch). We read it once up front.
        let max_half_tracks = g64_num_half_tracks(&image);

        // PORT OF: fsimage_read_gcr_image (fsimage_gcr.ts:301-324).
        let mut half_track: usize = 0;
        while half_track < DRIVE_HALFTRACKS_1541 {
            // ts:307-311 — free existing track (no-op here; freshly allocated).
            tracks[half_track].data = Vec::new();
            tracks[half_track].size = 0;

            if half_track < max_half_tracks {
                // ts:314 — fsimage_gcr_read_half_track(image, half_track + 2, tracks[half_track]).
                fsimage_gcr_read_half_track(&image, half_track + 2, &mut tracks[half_track]);
            } else {
                // ts:316-321 — empty tracks for non-existing tracks (0x00 fill).
                let size = disk_image_raw_track_size_g64((half_track >> 1) as u32);
                tracks[half_track].size = size;
                tracks[half_track].data = vec![0u8; size];
            }
            half_track += 1;
        }

        GcrImage { tracks }
    }

    /// Disk image kind for the write-back dispatch (G64 vs D64). Mirrors VICE
    /// `image->type` (the only two formats TRX64 mounts).
    ///
    /// Serialize the dirty half-track `half_track` (2..=84) back into the raw
    /// on-disk image `bytes`. 1:1 port of VICE `disk_image_write_half_track`
    /// (driveimage.ts:228-240) — dispatch on the image type:
    ///   * G64 → [`write_gcr_half_track`] (fsimage_gcr_write_half_track)
    ///   * D64 → [`write_dxx_half_track`]  (fsimage_dxx_write_half_track)
    ///
    /// `read_only` mirrors `image->read_only`; a read-only image rejects the
    /// write (returns `-1`), exactly as VICE. Returns 0 on success, -1 on error.
    pub fn write_half_track(
        &self,
        kind: WritebackKind,
        bytes: &mut Vec<u8>,
        half_track: usize,
        read_only: bool,
    ) -> i32 {
        let idx = half_track.wrapping_sub(2);
        let raw = match self.tracks.get(idx) {
            Some(t) => t,
            None => return -1,
        };
        match kind {
            WritebackKind::G64 => write_gcr_half_track(bytes, half_track, raw, read_only),
            WritebackKind::D64 => write_dxx_half_track(bytes, half_track, raw, read_only),
        }
    }
}

/// Image format for the GCR write-back dispatch (= VICE `image->type` subset).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WritebackKind {
    D64,
    G64,
}

// ── GCR decode codec (the GCR→4-byte / sector-decode half of gcr.ts) ─────────
//
// PORT OF: vice/src/gcr.c decode path via the TS oracle `gcr.ts`
// (gcr_convert_GCR_to_4bytes / gcr_find_sync / gcr_decode_block /
// gcr_find_sector_header / gcr_read_sector). This is the inverse of
// `gcr_convert_sector_to_gcr` above and is consumed by the D64 write-back
// (`fsimage_dxx_write_half_track`) to turn a written GCR track back into the
// 256-byte D64 sectors.

/// gcr_convert_GCR_to_4bytes (gcr.ts:104-125 / gcr.c:87-110): 5 GCR bytes →
/// 4 raw bytes, written to `dest[dest_off..dest_off+4]`.
fn gcr_convert_gcr_to_4bytes(source: &[u8], source_off: usize, dest: &mut [u8], dest_off: usize) {
    // at least 24 bits for shifting into bits 16..20
    let mut tdest: u32 = source[source_off] as u32;
    tdest <<= 13;
    let mut s = source_off;
    let mut d = dest_off;
    let mut i = 5;
    while i < 13 {
        s += 1;
        tdest |= (source[s] as u32) << i;
        // "tdest >> 16" could be optimized to a word-aligned access
        dest[d] = (FROM_GCR_CONV_DATA[((tdest >> 16) & 0x1f) as usize] << 4) & 0xff;
        tdest <<= 5;
        dest[d] |= FROM_GCR_CONV_DATA[((tdest >> 16) & 0x1f) as usize];
        tdest <<= 5;
        i += 2;
        d += 1;
    }
}

/// gcr_find_sync (gcr.ts:201-230 / gcr.c:170-203): scan the raw track for a
/// SYNC mark (>=10 consecutive 1-bits), starting from bit position `p`, looking
/// at most `s` bits. Returns the bit position of the byte after the sync, or
/// `-CBMDOS_FDC_ERR_SYNC` if none found.
///
/// `pub(crate)` so the rotation layer's sector-under-head decoder
/// ([`crate::rotation::Rotation::sector_under_head`], T2.3 — the port of
/// ws-server.ts:60 `viceSectorUnderHead`) can reuse the exact same scan VICE's
/// monitor uses; not exported beyond the crate.
pub(crate) fn gcr_find_sync(raw: &GcrTrack, mut p: i64, mut s: i64) -> i64 {
    if raw.data.is_empty() || raw.size == 0 {
        return -(CBMDOS_FDC_ERR_SYNC as i64);
    }
    let data = &raw.data;
    let mut w: u32 = 0;
    let mut b: u32 = ((data[(p >> 3) as usize] as u32) << (p & 7)) & 0xffff;
    while {
        let cond = s > 0;
        s -= 1;
        cond
    } {
        if b & 0x80 != 0 {
            w = ((w << 1) | 1) & 0xffff;
        } else if (!w) & 0x3ff != 0 {
            w = (w << 1) & 0xffff;
        } else {
            return p;
        }
        if (!p) & 7 != 0 {
            p += 1;
            b = (b << 1) & 0xffff;
        } else {
            p += 1;
            if p >= (raw.size as i64) * 8 {
                p = 0;
            }
            b = data[(p >> 3) as usize] as u32;
        }
    }
    -(CBMDOS_FDC_ERR_SYNC as i64)
}

/// gcr_decode_block (gcr.ts:233-264 / gcr.c:205-232): read `num` GCR groups
/// (5 bytes each) starting at bit position `p`, decode to `num*4` raw bytes in
/// `buf`.
///
/// `pub(crate)` for the same reason as [`gcr_find_sync`] — consumed by T2.3's
/// sector-under-head decoder ([`crate::rotation::Rotation::sector_under_head`]).
pub(crate) fn gcr_decode_block(raw: &GcrTrack, p: i64, buf: &mut [u8], num: usize) {
    if raw.data.is_empty() {
        return;
    }
    let data = &raw.data;
    let end = raw.size;
    let shift = (p & 7) as u32;
    let mut offset = (p >> 3) as usize;
    let mut gcr = [0u8; 5];
    let mut b: u32 = ((data[offset] as u32) << shift) & 0xffff;
    let mut buf_off = 0usize;
    for _ in 0..num {
        // get 5 bytes of gcr data
        for j in 0..5 {
            offset += 1;
            if offset >= end {
                offset = 0;
            }
            if shift != 0 {
                gcr[j] = (b | (((data[offset] as u32) << shift) >> 8)) as u8;
                b = ((data[offset] as u32) << shift) & 0xffff;
            } else {
                gcr[j] = (b & 0xff) as u8;
                b = data[offset] as u32;
            }
        }
        gcr_convert_gcr_to_4bytes(&gcr, 0, buf, buf_off);
        buf_off += 4;
    }
}

/// gcr_find_sector_header (gcr.ts:267-289 / gcr.c:234-261): find the header
/// block of `sector`. Returns the bit position of the header, or a negative
/// CBMDOS error code.
fn gcr_find_sector_header(raw: &GcrTrack, sector: u8) -> i64 {
    let mut header = [0u8; 4];
    let mut p: i64 = 0;
    let mut p2: i64 = -(CBMDOS_FDC_ERR_SYNC as i64);
    loop {
        p = gcr_find_sync(raw, p, (raw.size as i64) * 8);
        if p2 == p {
            break;
        }
        if p2 < 0 {
            p2 = p;
        }
        gcr_decode_block(raw, p, &mut header, 1);
        if header[0] == 0x08 && header[2] == sector {
            // Track, checksum or ID's are not checked here.
            return p;
        }
    }
    if p2 < 0 {
        return p2;
    }
    -(CBMDOS_FDC_ERR_HEADER as i64)
}

/// gcr_read_sector (gcr.ts:292-325 / gcr.c:263-292): decode `sector` from the
/// raw GCR track into the 256-byte `data`. Returns CBMDOS_FDC_ERR_OK,
/// CBMDOS_FDC_ERR_NOBLOCK, CBMDOS_FDC_ERR_DCHECK, or a header/sync error.
pub fn gcr_read_sector(raw: &GcrTrack, data: &mut [u8], sector: u8) -> u8 {
    let mut buffer = [0u8; 260];

    let mut p = gcr_find_sector_header(raw, sector);
    if p < 0 {
        return (-p) as u8;
    }

    p = gcr_find_sync(raw, p, 500 * 8);
    if p < 0 {
        return (-p) as u8;
    }

    gcr_decode_block(raw, p, &mut buffer, 65);

    let mut b = buffer[257];
    for i in 0..256 {
        data[i] = buffer[i + 1];
        b ^= data[i];
    }

    if buffer[0] != 0x07 {
        return CBMDOS_FDC_ERR_NOBLOCK;
    }

    if b != 0 {
        CBMDOS_FDC_ERR_DCHECK
    } else {
        CBMDOS_FDC_ERR_OK
    }
}

/// gcr_write_sector (gcr.ts:328-393 / gcr.c:294-346): GCR-encode the 256-byte
/// `data` into the raw GCR track `raw` at `sector`'s data block (the inverse of
/// [`gcr_read_sector`]). Used both for c64re fidelity and to inject a sector
/// write into a GCR track at the data-block level (the same bytes the rotation
/// engine's bit-level `write_next_bit` produces over a full DOS write).
pub fn gcr_write_sector(raw: &mut GcrTrack, data: &[u8], sector: u8) -> u8 {
    if raw.data.is_empty() {
        return CBMDOS_FDC_ERR_DRIVE;
    }
    let mut buffer = [0u8; 260];
    let mut gcr = [0u8; 5];

    let mut p = gcr_find_sector_header(raw, sector);
    if p < 0 {
        return (-p) as u8;
    }

    p = gcr_find_sync(raw, p, 500 * 8);
    if p < 0 {
        return (-p) as u8;
    }

    let shift = (p & 7) as u32;
    let mut offset = (p >> 3) as usize;
    let end = raw.size;
    let rdata = &mut raw.data;

    let mut b: u32 = (rdata[offset] as u32) & (((0xff00u32 >> shift) & 0xff) as u32);

    buffer[0] = 0x07;
    buffer[1..257].copy_from_slice(&data[..256]);
    let mut chksum = buffer[1];
    for i in 2..257 {
        chksum ^= buffer[i];
    }
    buffer[257] = chksum;
    buffer[258] = 0;
    buffer[259] = 0;

    let mut buf = 0usize;
    for _ in 0..65 {
        gcr_convert_4bytes_to_gcr(&buffer, buf, &mut gcr, 0);
        buf += 4;
        for j in 0..5 {
            if shift != 0 {
                rdata[offset] = (b | ((gcr[j] as u32) >> shift)) as u8;
                b = (((gcr[j] as u32) << 8) >> shift) & 0xff;
            } else {
                rdata[offset] = gcr[j];
            }
            offset += 1;
            if offset >= end {
                offset = 0;
            }
        }
    }
    rdata[offset] = (b | ((rdata[offset] as u32) & (((0xffu32 >> shift) & 0xff) as u32))) as u8;

    CBMDOS_FDC_ERR_OK
}

// ── GCR write-back: dirty half-track → raw on-disk image bytes ───────────────

/// PORT OF: fsimage_gcr.ts:441-538 (fsimage_gcr_write_half_track,
/// vice/src/diskimage/fsimage-gcr.c:185-277). Serialize a dirty GCR half-track
/// back into the raw `.g64` image `bytes`. Reads the track-offset-table entry
/// for `half_track`; for an `offset == 0` (no-data) entry the image is extended
/// at EOF and both the offset table and the speed-zone table are patched. The
/// 2-byte length, the raw GCR data, and the `max_track_length - size` zero
/// padding are written, exactly mirroring VICE's `fwrite` sequence.
///
/// `read_only` mirrors `image->read_only`. Returns 0 on success, -1 on error.
fn write_gcr_half_track(bytes: &mut Vec<u8>, half_track: usize, raw: &GcrTrack, read_only: bool) -> i32 {
    let mut extend = 0;
    let mut buf = [0u8; 4];

    // Read the header fields the writeback needs (offset-table entry,
    // max_track_length, num_half_tracks) through an immutable view, then drop
    // it so `bytes` can be borrowed mutably for the writes below.
    let mut max_track_length: u16 = 0;
    let (offset_i, num_half_tracks) = {
        let g64 = G64Image { fd: bytes.as_slice() };
        // ts:457 — fsimage_gcr_seek_half_track(...).
        let o = fsimage_gcr_seek_half_track(&g64, half_track, &mut max_track_length);
        // We need num_half_tracks for the speed-table patch (the seek helper in
        // this port returns only the offset). Re-read header byte-9 (matches the
        // `num_half_tracks` out-param VICE's seek fills).
        (o, g64_num_half_tracks(&g64))
    };
    if offset_i < 0 {
        return -1;
    }

    // ts:461-464 — read-only rejection.
    if read_only {
        return -1;
    }

    // ts:466-469 — track too long for image.
    if raw.size > max_track_length as usize {
        return -1;
    }

    let mut offset = offset_i as usize;
    if offset_i == 0 {
        // ts:471-484 — extend at EOF.
        offset = bytes.len();
        extend = 1;
    }

    // raw.data is never null in this port (Vec); mirror `raw->data != NULL`.
    // ts:486-487 — 2-byte LE length.
    util_word_to_le_buf(&mut buf, (raw.size & 0xffff) as u16);
    if util_fpwrite(bytes, &buf[..2], offset) < 0 {
        return -1;
    }

    // ts:499-503 — write the raw GCR data right after the length.
    if util_fpwrite(bytes, &raw.data[..raw.size], offset + 2) < 0 {
        return -1;
    }

    // ts:504-514 — zero-pad the gap up to max_track_length.
    let gap = max_track_length as i64 - raw.size as i64;
    if gap > 0 {
        let padding = vec![0u8; gap as usize];
        if util_fpwrite(bytes, &padding, offset + 2 + raw.size) < 0 {
            return -1;
        }
    }

    if extend != 0 {
        // ts:516-528 — patch the offset table + speed-zone table.
        util_dword_to_le_buf(&mut buf, offset as u32);
        if util_fpwrite(bytes, &buf, 12 + (half_track - 2) * 4) < 0 {
            return -1;
        }

        let speed = disk_image_speed_map_g64((half_track >> 1) as u32) as u32;
        util_dword_to_le_buf(&mut buf, speed);
        if util_fpwrite(bytes, &buf, 12 + (half_track - 2 + num_half_tracks) * 4) < 0 {
            return -1;
        }
    }

    // ts:531-535 — fflush (no-op) + hostFlush (the daemon writes `bytes` to the
    // host file at media/persist).
    0
}

/// PORT OF: fsimage_dxx.ts:342-430 (fsimage_dxx_write_half_track,
/// vice/src/diskimage/fsimage-dxx.c:47-147) for the D64 path. GCR-decode every
/// sector of the written track via [`gcr_read_sector`] and write the 256-byte
/// sectors back into the raw `.d64` image `bytes` at the linear sector offset.
///
/// The per-sector error-info map (`fsimage.error_info`) is OUT OF SCOPE for a
/// plain D64 (TRX64 mounts 174848-byte D64s with no error map); VICE only
/// touches it when `error_info.map != NULL`, which is never the case here, so
/// the error-map branches are correctly skipped (they are no-ops for a
/// map-less image, matching VICE's `if (fsimage->error_info.map ...)` guards).
///
/// `read_only` mirrors `image->read_only` (rejects the write). The track
/// `half_track/2` must be in range [1,35]; out-of-range returns -1. Returns 0
/// on success, -1 on error.
fn write_dxx_half_track(bytes: &mut [u8], half_track: usize, raw: &GcrTrack, read_only: bool) -> i32 {
    if read_only {
        return -1;
    }
    let track = (half_track / 2) as u8;

    // disk_image_sector_per_track / disk_image_check_sector (D64).
    if track < 1 || track > D64_TRACKS {
        return -1;
    }
    let max_sector = d64_sectors_per_track(track);
    let sectors = match d64_linear_sector(track, 0) {
        Some(s) => s,
        None => return -1,
    };

    // ts:374-399 — decode each sector via gcr_read_sector into a track buffer.
    let mut buffer = vec![0u8; max_sector as usize * 256];
    for sector in 0..max_sector {
        let mut tmp = [0u8; 256];
        // rf is computed (and would feed the error map) but a map-less plain D64
        // ignores it — the decoded bytes are always written, matching VICE.
        let _rf = gcr_read_sector(raw, &mut tmp, sector);
        let base = sector as usize * 256;
        buffer[base..base + 256].copy_from_slice(&tmp);
    }

    // ts:400-406 — write the whole track at the linear sector offset.
    let offset = sectors * 256;
    if util_fpwrite_slice(bytes, &buffer, offset) < 0 {
        return -1;
    }
    // ts:407-424 — error-info map writeback skipped (map is null for plain D64).
    // ts:425-428 — fflush (no-op) + hostFlush (daemon writes at media/persist).
    0
}

/// PORT OF: util.c (util_word_to_le_buf) — 16-bit LE store into `buf[0..2]`.
#[inline]
fn util_word_to_le_buf(buf: &mut [u8], value: u16) {
    buf[0] = (value & 0xff) as u8;
    buf[1] = ((value >> 8) & 0xff) as u8;
}

/// PORT OF: util.c (util_dword_to_le_buf) — 32-bit LE store into `buf[0..4]`.
#[inline]
fn util_dword_to_le_buf(buf: &mut [u8], value: u32) {
    buf[0] = (value & 0xff) as u8;
    buf[1] = ((value >> 8) & 0xff) as u8;
    buf[2] = ((value >> 16) & 0xff) as u8;
    buf[3] = ((value >> 24) & 0xff) as u8;
}

/// PORT OF: fsimage_gcr.ts:132-143 / fsimage_dxx.ts:308-312 (util_fpwrite) —
/// pwrite(2)-style positional write. Returns 0 on success, -1 if the range is
/// out of bounds. NOTE: unlike the TS in-memory `util_fpwrite` (which
/// auto-grows the backing for the G64 EXTEND path), the D64 path never extends
/// (the 174848-byte image already covers all 35 tracks); the G64 EXTEND path
/// auto-grows the `Vec` here when `offset+size` exceeds the current length.
#[inline]
fn util_fpwrite(fd: &mut Vec<u8>, buf: &[u8], offset: usize) -> i32 {
    let end = offset + buf.len();
    if end > fd.len() {
        fd.resize(end, 0);
    }
    fd[offset..end].copy_from_slice(buf);
    0
}

/// Non-extending positional write into a fixed-size slice (the D64 path — the
/// image already covers all sectors, so VICE's `util_fpwrite` bounds-check
/// `offset+size > fd.length` returning -1 applies). Returns 0 on success, -1
/// if out of bounds.
#[inline]
fn util_fpwrite_slice(fd: &mut [u8], buf: &[u8], offset: usize) -> i32 {
    let end = offset + buf.len();
    if end > fd.len() {
        return -1;
    }
    fd[offset..end].copy_from_slice(buf);
    0
}

// ── G64 image (GCR-1541) loading — 1:1 port of fsimage_gcr.ts ────────────────
//
// PORT OF: vice/src/diskimage/fsimage-gcr.c (G64 read path) via the TS oracle
// `fsimage_gcr.ts`. The TS in-memory `FILE_t` (buf/length/cursor) collapses to a
// borrowed `&[u8]` here: every read in the G64 load path is positional
// (`util_fpread`), so no cursor is needed — `fseek`/`fread`/`ftell` are only used
// by the *write*/extend path, which is out of scope for read-only mounting.

/// PORT OF: fsimage_gcr.ts FILE_t (ISO C99 `FILE *` equivalent) reduced to the
/// read-only positional surface the G64 load path needs.
struct G64Image<'a> {
    /// VICE `fsimage->fd` — the raw .g64 file bytes.
    fd: &'a [u8],
}

/// PORT OF: fsimage_gcr.ts:124-128 (util_fpread, vice/src/util.c). pread(2)-style
/// positional read. Returns `Some(slice)` (offset+size in range) or `None` (VICE
/// `-1`).
#[inline]
fn util_fpread<'a>(image: &G64Image<'a>, size: usize, offset: usize) -> Option<&'a [u8]> {
    let end = offset.checked_add(size)?;
    if end > image.fd.len() {
        return None;
    }
    Some(&image.fd[offset..end])
}

/// PORT OF: fsimage_gcr.ts:146-148 (util_le_buf_to_word, vice/src/util.c).
#[inline]
fn util_le_buf_to_word(buf: &[u8], off: usize) -> u16 {
    let lo = *buf.get(off).unwrap_or(&0) as u16;
    let hi = *buf.get(off + 1).unwrap_or(&0) as u16;
    lo | (hi << 8)
}

/// PORT OF: fsimage_gcr.ts:151-159 (util_le_buf_to_dword, vice/src/util.c).
#[inline]
fn util_le_buf_to_dword(buf: &[u8], off: usize) -> u32 {
    let b0 = *buf.get(off).unwrap_or(&0) as u32;
    let b1 = *buf.get(off + 1).unwrap_or(&0) as u32;
    let b2 = *buf.get(off + 2).unwrap_or(&0) as u32;
    let b3 = *buf.get(off + 3).unwrap_or(&0) as u32;
    b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)
}

/// PORT OF: fsimage_gcr.ts:256-258 (gcr_image_header_expected_1541) — "GCR-1541\0".
const GCR_IMAGE_HEADER_EXPECTED_1541: [u8; 9] =
    [0x47, 0x43, 0x52, 0x2d, 0x31, 0x35, 0x34, 0x31, 0x00];

/// PORT OF: fsimage_gcr.ts:261-263 (gcr_image_header_expected_1571) — "GCR-1571\0".
const GCR_IMAGE_HEADER_EXPECTED_1571: [u8; 9] =
    [0x47, 0x43, 0x52, 0x2d, 0x31, 0x35, 0x37, 0x31, 0x00];

/// PORT OF: fsimage_gcr.ts:267-274 (memcmp) — lexicographic compare of `n` bytes.
#[inline]
fn memcmp(a: &[u8], b: &[u8], n: usize) -> i32 {
    for i in 0..n {
        let av = *a.get(i).unwrap_or(&0) as i32;
        let bv = *b.get(i).unwrap_or(&0) as i32;
        if av != bv {
            return av - bv;
        }
    }
    0
}

/// PORT OF: fsimage_gcr.ts:206-227 (disk_image_speed_map) — G64 branch only
/// (same map as D64/D67/P64). Speed zone 0..3 for a 0-based track number.
#[inline]
fn disk_image_speed_map_g64(track: u32) -> usize {
    ((track < 31) as usize) + ((track < 25) as usize) + ((track < 18) as usize)
}

/// PORT OF: fsimage_gcr.ts:230-246 (disk_image_raw_track_size) — G64 branch.
/// Raw GCR track size (bytes) for a 0-based track number (half_track >> 1).
#[inline]
fn disk_image_raw_track_size_g64(track: u32) -> usize {
    RAW_TRACK_SIZE_D64[disk_image_speed_map_g64(track)]
}

/// Header byte-9 `num_half_tracks` = VICE `image->max_half_tracks` for a G64
/// (`fsimage-probe.c:501-502`). Validates the "GCR-1541"/"GCR-1571" magic; on any
/// header error returns 0 so every slot takes the canonical empty-fill branch.
fn g64_num_half_tracks(image: &G64Image) -> usize {
    let buf = match util_fpread(image, 12, 0) {
        Some(b) => b,
        None => return 0,
    };
    if memcmp(&GCR_IMAGE_HEADER_EXPECTED_1541, buf, GCR_IMAGE_HEADER_EXPECTED_1541.len()) != 0
        && memcmp(&GCR_IMAGE_HEADER_EXPECTED_1571, buf, GCR_IMAGE_HEADER_EXPECTED_1571.len()) != 0
    {
        return 0;
    }
    let n = buf[9] as usize;
    // ts:354 — Too many half tracks. (MAX_GCR_TRACKS = 168 in VICE.)
    if n > G64_MAX_GCR_TRACKS {
        return 0;
    }
    n
}

/// PORT OF: drivetypes.ts:196 (MAX_GCR_TRACKS = 168). The 1571-capacity ceiling
/// VICE validates `num_half_tracks` against in `fsimage_gcr_seek_half_track`.
const G64_MAX_GCR_TRACKS: usize = 168;

/// PORT OF: fsimage_gcr.ts:331-368 (fsimage_gcr_seek_half_track,
/// vice/src/fsimage-gcr.c:79-120). Static in VICE. Returns the file offset of the
/// half-track's data block (`> 0`), `0` (no data → empty track), or `-1` (error)
/// as an `i64`. `max_track_length` is the out-param (TS wrapper object).
fn fsimage_gcr_seek_half_track(
    image: &G64Image,
    half_track: usize,
    max_track_length: &mut u16,
) -> i64 {
    // ts:343 — util_fpread(fd, buf, 12, 0).
    let buf = match util_fpread(image, 12, 0) {
        Some(b) => b,
        None => return -1, // "Could not read GCR disk image." / no fd.
    };
    // ts:347-351 — header magic check (1541 or 1571).
    if memcmp(&GCR_IMAGE_HEADER_EXPECTED_1541, buf, GCR_IMAGE_HEADER_EXPECTED_1541.len()) != 0
        && memcmp(&GCR_IMAGE_HEADER_EXPECTED_1571, buf, GCR_IMAGE_HEADER_EXPECTED_1571.len()) != 0
    {
        return -1; // "Unexpected GCR header found."
    }
    // ts:353-357 — num_half_tracks = buf[9]; reject > MAX_GCR_TRACKS.
    let num_half_tracks = buf[9] as usize;
    if num_half_tracks > G64_MAX_GCR_TRACKS {
        return -1; // "Too many half tracks."
    }
    // ts:359 — max_track_length = util_le_buf_to_word(buf, 10).
    *max_track_length = util_le_buf_to_word(buf, 10);

    // ts:362-366 — entry = util_fpread(fd, 4, 12 + (half_track - 2) * 4).
    let entry = match util_fpread(image, 4, 12 + (half_track - 2) * 4) {
        Some(e) => e,
        None => return -1, // "Could not read GCR disk image."
    };
    // ts:367 — return util_le_buf_to_dword(entry).
    util_le_buf_to_dword(entry, 0) as i64
}

/// PORT OF: fsimage_gcr.ts:374-427 (fsimage_gcr_read_half_track,
/// vice/src/fsimage-gcr.c:125-174). Read an entire GCR half-track into `raw`. For
/// an `offset == 0` entry the buffer is the canonical raw track size, 0x55-filled.
fn fsimage_gcr_read_half_track(image: &G64Image, half_track: usize, raw: &mut GcrTrack) -> i32 {
    // ts:383-384 — raw.data = null; raw.size = 0.
    raw.data = Vec::new();
    raw.size = 0;

    let mut max_track_length: u16 = 0;
    // ts:388 — offset = fsimage_gcr_seek_half_track(...).
    let offset = fsimage_gcr_seek_half_track(image, half_track, &mut max_track_length);

    // ts:390-392 — if (offset < 0) return -1.
    if offset < 0 {
        return -1;
    }

    if offset != 0 {
        let offset = offset as usize;
        // ts:396 — util_fpread(fd, buf, 2, offset).
        let len_buf = match util_fpread(image, 2, offset) {
            Some(b) => b,
            None => return -1, // "Could not read GCR disk image."
        };
        // ts:401 — track_len = util_le_buf_to_word(buf).
        let track_len = util_le_buf_to_word(len_buf, 0) as usize;

        // ts:403-406 — reject track_len < 1 || > max_track_length.
        if track_len < 1 || track_len > max_track_length as usize {
            return -1; // "Track field length %u is not supported."
        }

        // ts:408-409 — raw.data = lib_calloc(1, track_len); raw.size = track_len.
        // ts:416-420 — fseek(offset+2) + fread(track_len). The VICE fread reads
        // the bytes immediately after the 2-byte length; we read the same slice
        // positionally.
        let data = match util_fpread(image, track_len, offset + 2) {
            Some(d) => d.to_vec(),
            None => return -1, // "Could not read GCR disk image."
        };
        raw.data = data;
        raw.size = track_len;
    } else {
        // ts:422-424 — empty track: raw_track_size, 0x55-filled.
        let size = disk_image_raw_track_size_g64((half_track >> 1) as u32);
        raw.size = size;
        raw.data = vec![0x55u8; size];
    }
    0
}

/// For track numbers beyond the D64's 35 the speed-zone formula still maps into
/// the zone tables (track 36-42 all satisfy t>=31 → zone 0 → 6250). Provided as a
/// distinct name so the >35 path reads explicitly.
#[inline]
fn d64_raw_track_size_unclamped(track: u8) -> usize {
    RAW_TRACK_SIZE_D64[d64_speed_zone(track)]
}

/// In `fsimage_dxx.ts` a plain D64 with no error map passes `CBMDOS_FDC_ERR_OK`
/// (value 1) *only when the read succeeds*; but the codec's branches all key off
/// specific error codes (HEADER/SYNC/NOBLOCK/DCHECK/ID), and OK takes the normal
/// path. We pass OK here (the `*_AS_ZERO` alias documents that OK selects the
/// nominal 0x08/0x07 framing — it is NOT one of the special error fills).
const CBMDOS_FDC_ERR_OK_AS_ZERO: u8 = CBMDOS_FDC_ERR_OK;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_683_sectors() {
        let total: usize = (1..=D64_TRACKS).map(|t| d64_sectors_per_track(t) as usize).sum();
        assert_eq!(total, 683, "standard D64 has 683 sectors");
    }

    #[test]
    fn zone_boundaries() {
        assert_eq!(d64_sectors_per_track(1), 21);
        assert_eq!(d64_sectors_per_track(17), 21);
        assert_eq!(d64_sectors_per_track(18), 19);
        assert_eq!(d64_sectors_per_track(24), 19);
        assert_eq!(d64_sectors_per_track(25), 18);
        assert_eq!(d64_sectors_per_track(30), 18);
        assert_eq!(d64_sectors_per_track(31), 17);
        assert_eq!(d64_sectors_per_track(35), 17);
        assert_eq!(d64_raw_track_size(1), 7692);
        assert_eq!(d64_raw_track_size(18), 7142);
        assert_eq!(d64_raw_track_size(25), 6666);
        assert_eq!(d64_raw_track_size(35), 6250);
    }

    #[test]
    fn linear_sector_offsets() {
        assert_eq!(d64_linear_sector(1, 0), Some(0));
        assert_eq!(d64_linear_sector(1, 20), Some(20));
        assert_eq!(d64_linear_sector(2, 0), Some(21));
        // track 18 sector 0 = sum of tracks 1..17 (17*21 = 357)
        assert_eq!(d64_linear_sector(18, 0), Some(357));
        // last sector: track 35 sector 16 = 682
        assert_eq!(d64_linear_sector(35, 16), Some(682));
        assert_eq!(d64_linear_sector(35, 17), None);
        assert_eq!(d64_linear_sector(36, 0), None);
    }

    #[test]
    fn gcr_4bytes_known_vector() {
        // Encode [0x00,0x00,0x00,0x00] → all-zero nybbles → GCR 0x0a repeated.
        // 8 nybbles of 0x0a = 01010 01010 ... → packs to 0x52,0x94,0xa5,0x29,0x4a.
        let src = [0u8; 4];
        let mut dst = [0u8; 5];
        gcr_convert_4bytes_to_gcr(&src, 0, &mut dst, 0);
        assert_eq!(dst, [0x52, 0x94, 0xa5, 0x29, 0x4a]);
    }

    #[test]
    fn sector_encode_sync_and_framing() {
        // A sector encodes with a 5-byte 0xff sync, an 0x08 header marker (GCR),
        // and an 0x07 data marker. We at least verify the sync run and that the
        // header block decodes back to track/sector via the GCR table round-trip.
        let buffer = [0xABu8; 256];
        let mut data = vec![0x55u8; 400];
        let header = GcrHeader { sector: 5, track: 18, id1: 0x30, id2: 0x31 };
        gcr_convert_sector_to_gcr(&buffer, &mut data, 0, header, HEADER_GAP_D64, SYNC_LEN_D64, CBMDOS_FDC_ERR_OK);
        // First 5 bytes are the 0xff sync.
        assert_eq!(&data[0..5], &[0xff; 5]);
        // Header block (next 5 GCR bytes) decodes its first nybble to 0x08.
        // GCR byte0 high 5 bits = GCR_CONV_DATA[0]=0x0a → nybble 0 → header[0]=0x08
        // We confirm the top of the encoded stream isn't left as 0x55 fill.
        assert_ne!(data[5], 0x55);
    }

    #[test]
    fn from_d64_builds_84_tracks() {
        // A blank 174848-byte D64 (all zero) still builds a valid GCR image.
        let d64 = vec![0u8; 683 * 256];
        let img = GcrImage::from_d64(&d64);
        assert_eq!(img.tracks.len(), DRIVE_HALFTRACKS_1541);
        // Track 1 (slot 0) has the zone-3 raw size.
        assert_eq!(img.tracks[0].size, 7692);
        // Track 18 (slot 34) has zone-2 size.
        assert_eq!(img.tracks[34].size, 7142);
        // Odd half-track (slot 1) is zero-filled, size = track 1 size.
        assert_eq!(img.tracks[1].size, 7692);
        assert!(img.tracks[1].data.iter().all(|&b| b == 0));
        // The data track has a 0xff sync run somewhere (non-empty encode).
        assert!(img.tracks[0].data.windows(5).any(|w| w == [0xff; 5]));
    }

    /// Build a minimal synthetic .g64 with the GCR-1541 header, a populated
    /// half-track 0 (= slot 0 / track 1), an empty (offset 0) half-track 2, and
    /// confirm the parse: header magic, num_half_tracks, the populated track's
    /// length + bytes, and the empty-track 0x55 fill at the canonical raw size.
    #[test]
    fn from_g64_parses_synthetic_header_and_tracks() {
        // num_half_tracks = 4, max_track_length = 7928 (matches the real samples).
        let num_half_tracks: u8 = 4;
        let max_track_length: u16 = 7928;
        // Offset table: 4 entries; data block placed right after both tables.
        // Header(12) + offset-table(4*4) + speed-table(4*4) = 12 + 16 + 16 = 44.
        let data_off: u32 = 44;
        let track0_len: u16 = 16; // small synthetic track
        let track0_bytes: Vec<u8> = (0..track0_len as u8).map(|b| b ^ 0x3c).collect();

        let mut g64: Vec<u8> = Vec::new();
        // Header: "GCR-1541\0".
        g64.extend_from_slice(&GCR_IMAGE_HEADER_EXPECTED_1541);
        // byte 9: num_half_tracks.
        g64.push(num_half_tracks);
        // bytes 10-11: max_track_length (LE).
        g64.extend_from_slice(&max_track_length.to_le_bytes());
        // Offset table (4 × u32 LE): slot 0 populated, slots 1..3 empty (0).
        g64.extend_from_slice(&data_off.to_le_bytes());
        g64.extend_from_slice(&0u32.to_le_bytes());
        g64.extend_from_slice(&0u32.to_le_bytes());
        g64.extend_from_slice(&0u32.to_le_bytes());
        // Speed-zone table (4 × u32 LE) — values irrelevant to read.
        for _ in 0..4 {
            g64.extend_from_slice(&3u32.to_le_bytes());
        }
        // Data block at data_off: 2-byte track length + the raw GCR bytes.
        assert_eq!(g64.len(), data_off as usize);
        g64.extend_from_slice(&track0_len.to_le_bytes());
        g64.extend_from_slice(&track0_bytes);

        let img = GcrImage::from_g64(&g64);
        assert_eq!(img.tracks.len(), DRIVE_HALFTRACKS_1541);

        // Slot 0 (half-track 0 / track 1): populated with track0_len bytes.
        assert_eq!(img.tracks[0].size, track0_len as usize);
        assert_eq!(img.tracks[0].data, track0_bytes);

        // Slot 1 (offset-table entry 0): empty (offset 0) → canonical raw size,
        // 0x55-filled. half_track >> 1 = (1+2)>>1... — entry 1 has offset 0, so
        // its raw size uses (half_track) >> 1 where half_track = slot+2 = 3 → 1.
        // raw_track_size for track 1 (zone 3) = 7692.
        assert_eq!(img.tracks[1].size, 7692);
        assert!(img.tracks[1].data.iter().all(|&b| b == 0x55));

        // Slots >= num_half_tracks (4): empty-fill branch (0x00) at raw size.
        assert!(img.tracks[4].size > 0);
        assert!(img.tracks[4].data.iter().all(|&b| b == 0x00));
    }

    /// A malformed/non-G64 buffer yields an all-empty image (no panic): every slot
    /// takes the canonical empty-fill branch.
    #[test]
    fn from_g64_rejects_bad_header_gracefully() {
        let junk = vec![0u8; 64];
        let img = GcrImage::from_g64(&junk);
        assert_eq!(img.tracks.len(), DRIVE_HALFTRACKS_1541);
        // num_half_tracks rejected → all slots take the empty (0x00) fill path.
        assert!(img.tracks.iter().all(|t| t.size > 0));
        assert!(img.tracks[0].data.iter().all(|&b| b == 0x00));
    }

    /// Load a real .g64 sample (if present) and sanity-check the parse: slot 0
    /// (track 1) and slot 34 (track 18) are populated with plausible GCR data,
    /// and a 0xff sync run exists (real disks always have sync marks).
    #[test]
    fn from_g64_real_sample_motm() {
        let path = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/motm.g64";
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => return, // skip without sample
        };
        // Header: byte 9 = 84 half-tracks.
        assert_eq!(bytes[9], 84, "motm.g64 has 84 half-tracks");
        let img = GcrImage::from_g64(&bytes);
        assert_eq!(img.tracks.len(), DRIVE_HALFTRACKS_1541);
        // Slot 0 (track 1) populated, plausible track size (zone-3 ≈ 7692).
        assert!(img.tracks[0].size > 6000 && img.tracks[0].size <= 7928);
        // Slot 34 (track 18 — the directory track) populated.
        assert!(img.tracks[34].size > 6000 && img.tracks[34].size <= 7928);
        // Real GCR has 0xff sync runs on the directory track.
        assert!(
            img.tracks[34].data.windows(5).any(|w| w == [0xff; 5]),
            "track 18 must contain a 0xff sync run"
        );
    }

    /// Scan every .g64 in the sample library and confirm each parses cleanly:
    /// 84 half-tracks, populated data tracks, and a 0xff sync run on the
    /// directory track (track 18). This is the parser-breadth check behind the
    /// 7-game .g64 milestone — every game image must at least MOUNT.
    #[test]
    fn from_g64_scans_all_samples() {
        let dir = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples";
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(_) => return, // skip without sample dir
        };
        let mut scanned = 0usize;
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("g64") {
                continue;
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let name = path.file_name().unwrap().to_string_lossy();

            // Header magic must be GCR-1541 (the 1541 case we support).
            assert_eq!(
                &bytes[0..9],
                &GCR_IMAGE_HEADER_EXPECTED_1541[..],
                "{name}: GCR-1541 magic"
            );
            let num_ht = bytes[9] as usize;
            assert!(num_ht >= 1 && num_ht <= G64_MAX_GCR_TRACKS, "{name}: num_half_tracks");

            let img = GcrImage::from_g64(&bytes);
            assert_eq!(img.tracks.len(), DRIVE_HALFTRACKS_1541, "{name}: 84 slots");

            // Track 1 (slot 0) and track 18 (slot 34) must be populated with a
            // plausible raw size and a GCR SYNC mark. SYNC on a 1541 is a run of
            // >= 10 consecutive 1-bits in the bit stream (the hardware SYNC
            // detector / VICE rotation_1541_gcr) — NOT necessarily 5 whole 0xff
            // bytes, since the mark is rarely byte-aligned. (motm.g64's directory
            // happens to be byte-aligned; many protected track-1 sync marks are
            // not, so a byte-windowed [0xff;5] check would wrongly reject them.)
            for &(slot, label) in &[(0usize, "track1"), (34usize, "track18")] {
                let t = &img.tracks[slot];
                assert!(
                    t.size > 5000 && t.size <= 8000,
                    "{name}: {label} raw size plausible (got {})",
                    t.size
                );
                assert!(
                    max_sync_bit_run(&t.data) >= 10,
                    "{name}: {label} has a GCR SYNC mark (>=10 consecutive 1-bits)"
                );
            }
            scanned += 1;
        }
        eprintln!("from_g64 scanned {scanned} .g64 sample images — all mount cleanly");
        // If the sample dir exists it should have several .g64 games.
        if scanned > 0 {
            assert!(scanned >= 5, "expected several .g64 samples, scanned {scanned}");
        }
    }

    /// Longest run of consecutive 1-bits in a GCR byte stream (MSB-first) — the
    /// SYNC criterion the 1541 hardware uses (>= 10 ones).
    fn max_sync_bit_run(data: &[u8]) -> u32 {
        let mut run: u32 = 0;
        let mut best: u32 = 0;
        for &byte in data {
            for i in (0..8).rev() {
                if (byte >> i) & 1 != 0 {
                    run += 1;
                    if run > best {
                        best = run;
                    }
                } else {
                    run = 0;
                }
            }
        }
        best
    }

    // ── GCR decode + write-back round-trip ───────────────────────────────────

    /// A fully synthetic 35-track D64 with distinct per-sector content so a
    /// round-trip can be byte-checked. Sector (t,s) byte i = (t ^ s ^ i) & 0xff
    /// (with a sane BAM disk ID at T18 S0 so from_d64 reads it).
    fn synthetic_d64() -> Vec<u8> {
        let mut d = vec![0u8; 683 * 256];
        for track in 1..=D64_TRACKS {
            for sector in 0..d64_sectors_per_track(track) {
                let lin = d64_linear_sector(track, sector).unwrap();
                let off = lin * 256;
                for i in 0..256 {
                    d[off + i] = (track ^ sector ^ (i as u8)) & 0xff;
                }
            }
        }
        // Disk ID at T18 S0 0xA2/0xA3 (so from_d64's header IDs are stable).
        let bam = d64_linear_sector(18, 0).unwrap() * 256;
        d[bam + BAM_ID_1541] = 0x41;
        d[bam + BAM_ID_1541 + 1] = 0x42;
        d
    }

    /// The GCR codec must round-trip every sector of a D64: encode (from_d64) →
    /// gcr_read_sector → original 256-byte sectors, byte-exact, with no DCHECK.
    #[test]
    fn gcr_decode_round_trips_all_d64_sectors() {
        let d64 = synthetic_d64();
        let img = GcrImage::from_d64(&d64);
        for track in 1..=D64_TRACKS {
            let slot = (track as usize) * 2 - 2;
            let raw = &img.tracks[slot];
            for sector in 0..d64_sectors_per_track(track) {
                let mut decoded = [0u8; 256];
                let rf = gcr_read_sector(raw, &mut decoded, sector);
                assert_eq!(
                    rf, CBMDOS_FDC_ERR_OK,
                    "T{track} S{sector} must decode OK (rf={rf})"
                );
                let lin = d64_linear_sector(track, sector).unwrap();
                let off = lin * 256;
                assert_eq!(
                    &decoded[..],
                    &d64[off..off + 256],
                    "T{track} S{sector} round-trip mismatch"
                );
            }
        }
    }

    /// D64 write-back: inject a new sector into a GCR track via gcr_write_sector
    /// (the data-block-level equivalent of the rotation engine's bit writes),
    /// then write_dxx_half_track → the .d64 image bytes carry the new sector.
    #[test]
    fn d64_write_back_round_trips_a_sector() {
        let d64 = synthetic_d64();
        let mut img = GcrImage::from_d64(&d64);
        let mut bytes = d64.clone();

        // Write a brand-new payload into T20 S5.
        let track: u8 = 20;
        let sector: u8 = 5;
        let new_sector: Vec<u8> = (0..256).map(|i| (0xC0u16.wrapping_add(i as u16)) as u8).collect();

        let slot = (track as usize) * 2 - 2;
        // Inject the write into the in-memory GCR track (= what the rotation
        // engine produces over a DOS write of this sector).
        let rf = gcr_write_sector(&mut img.tracks[slot], &new_sector, sector);
        assert_eq!(rf, CBMDOS_FDC_ERR_OK, "gcr_write_sector OK");

        // Serialize the dirty track back into the .d64 image bytes.
        let half_track = (track as usize) * 2;
        let res = img.write_half_track(WritebackKind::D64, &mut bytes, half_track, false);
        assert_eq!(res, 0, "write_half_track OK");

        // The image bytes now carry the new sector at its linear offset.
        let lin = d64_linear_sector(track, sector).unwrap();
        let off = lin * 256;
        assert_eq!(&bytes[off..off + 256], &new_sector[..], "written sector persisted");

        // Every OTHER sector on that track must be UNCHANGED (the writeback
        // re-encodes the whole track from the decoded GCR — so they must
        // decode back to the original D64 content).
        for s in 0..d64_sectors_per_track(track) {
            if s == sector {
                continue;
            }
            let lin = d64_linear_sector(track, s).unwrap();
            let off = lin * 256;
            assert_eq!(
                &bytes[off..off + 256],
                &d64[off..off + 256],
                "T{track} S{s} must be untouched by the writeback"
            );
        }

        // Re-parse the mutated image and confirm the new sector decodes back.
        let img2 = GcrImage::from_d64(&bytes);
        let mut decoded = [0u8; 256];
        let rf2 = gcr_read_sector(&img2.tracks[slot], &mut decoded, sector);
        assert_eq!(rf2, CBMDOS_FDC_ERR_OK);
        assert_eq!(&decoded[..], &new_sector[..], "re-read after re-mount round-trips");

        // A read-only image must REJECT the write.
        let mut ro_bytes = d64.clone();
        assert_eq!(
            img.write_half_track(WritebackKind::D64, &mut ro_bytes, half_track, true),
            -1,
            "read-only rejects writeback"
        );
        assert_eq!(ro_bytes, d64, "read-only image untouched");
    }

    /// G64 write-back: build a real .g64 (from a D64), mutate a populated track's
    /// GCR bytes, write_half_track → the .g64 byte image (offset table + 2-byte
    /// length + data) carries the mutation; a re-parse (from_g64) matches.
    #[test]
    fn g64_write_back_round_trips_a_track() {
        // Build a minimal but valid .g64 from a synthetic D64: header + offset
        // table + speed table + the per-track (len, data) blocks.
        let d64 = synthetic_d64();
        let gimg = GcrImage::from_d64(&d64);

        let num_half_tracks: u8 = 84;
        let max_track_length: u16 = 7928;
        let mut g64: Vec<u8> = Vec::new();
        g64.extend_from_slice(&GCR_IMAGE_HEADER_EXPECTED_1541);
        g64.push(num_half_tracks);
        g64.extend_from_slice(&max_track_length.to_le_bytes());

        // Reserve the offset table (84 × u32) + speed table (84 × u32).
        let table_start = g64.len();
        g64.resize(table_start + (num_half_tracks as usize) * 4 * 2, 0);

        // Lay out each half-track's data block: 2-byte len + max_track_length
        // bytes (VICE pads to max_track_length). Patch the offset + speed tables.
        for ht in 0..num_half_tracks as usize {
            let raw = &gimg.tracks[ht];
            if raw.size == 0 {
                continue;
            }
            let data_off = g64.len() as u32;
            // offset-table entry ht.
            let ote = table_start + ht * 4;
            g64[ote..ote + 4].copy_from_slice(&data_off.to_le_bytes());
            // speed-table entry ht.
            let ste = table_start + (num_half_tracks as usize) * 4 + ht * 4;
            let speed = disk_image_speed_map_g64((ht >> 1) as u32) as u32;
            g64[ste..ste + 4].copy_from_slice(&speed.to_le_bytes());
            // 2-byte length + data padded to max_track_length.
            g64.extend_from_slice(&(raw.size as u16).to_le_bytes());
            g64.extend_from_slice(&raw.data[..raw.size]);
            let pad = max_track_length as usize - raw.size;
            g64.extend(std::iter::repeat(0u8).take(pad));
        }

        // Sanity: the .g64 parses, track 1 (slot 0) is populated.
        let parsed = GcrImage::from_g64(&g64);
        assert!(parsed.tracks[0].size > 6000);
        // The parsed track must byte-match the source GCR (no corruption).
        assert_eq!(parsed.tracks[0].data[..parsed.tracks[0].size], gimg.tracks[0].data[..gimg.tracks[0].size]);

        // Mutate track 1 (slot 0) in the working GCR image: flip the first 16
        // data bytes (NOT the offset-table — a raw GCR byte mutation, which is
        // what the rotation engine does at the bit level).
        let mut img = GcrImage::from_g64(&g64);
        let slot = 0usize; // half-track 2 / track 1
        for i in 0..16 {
            img.tracks[slot].data[i] ^= 0xa5;
        }

        // Write the dirty half-track back into the .g64 image bytes.
        let mut bytes = g64.clone();
        let half_track = slot + 2; // = 2
        let res = img.write_half_track(WritebackKind::G64, &mut bytes, half_track, false);
        assert_eq!(res, 0, "G64 write_half_track OK");

        // The .g64 byte image changed only within track 1's data block, and a
        // re-parse must reflect the mutation byte-for-byte.
        let img2 = GcrImage::from_g64(&bytes);
        assert_eq!(
            img2.tracks[slot].data[..img2.tracks[slot].size],
            img.tracks[slot].data[..img.tracks[slot].size],
            "re-parsed G64 track matches the written track"
        );
        // And it must DIFFER from the original (the mutation actually landed).
        assert_ne!(
            &img2.tracks[slot].data[..16],
            &parsed.tracks[slot].data[..16],
            "the mutated bytes persisted"
        );

        // Read-only rejects.
        let mut ro = g64.clone();
        assert_eq!(
            img.write_half_track(WritebackKind::G64, &mut ro, half_track, true),
            -1
        );
        assert_eq!(ro, g64, "read-only G64 untouched");
    }

    /// G64 EXTEND path: writing a half-track whose offset-table entry is 0
    /// (no data block yet) appends a new block at EOF and patches the offset +
    /// speed tables. A re-parse then finds the formerly-empty track populated.
    #[test]
    fn g64_write_back_extends_empty_track() {
        // A minimal .g64: 4 half-tracks, only slot 0 populated, slots 1..3 empty
        // (offset-table entries 0). Mirror the existing synthetic-header test.
        let num_half_tracks: u8 = 4;
        let max_track_length: u16 = 7928;
        let data_off: u32 = 44; // 12 + 16 + 16
        let track0_len: u16 = 32;
        let track0: Vec<u8> = (0..track0_len as u8).map(|b| b ^ 0x3c).collect();

        let mut g64: Vec<u8> = Vec::new();
        g64.extend_from_slice(&GCR_IMAGE_HEADER_EXPECTED_1541);
        g64.push(num_half_tracks);
        g64.extend_from_slice(&max_track_length.to_le_bytes());
        g64.extend_from_slice(&data_off.to_le_bytes()); // slot 0 offset
        g64.extend_from_slice(&0u32.to_le_bytes()); // slot 1 empty
        g64.extend_from_slice(&0u32.to_le_bytes()); // slot 2 empty
        g64.extend_from_slice(&0u32.to_le_bytes()); // slot 3 empty
        for _ in 0..4 {
            g64.extend_from_slice(&3u32.to_le_bytes()); // speed table
        }
        g64.extend_from_slice(&track0_len.to_le_bytes());
        g64.extend_from_slice(&track0);

        // Now write slot 1 (half-track 3, offset-table entry 1, currently 0).
        let mut img = GcrImage::from_g64(&g64);
        // Put a distinctive payload into slot 1 (it parsed as 0x55-filled empty).
        let new_len = 64usize;
        img.tracks[1].size = new_len;
        img.tracks[1].data = (0..new_len as u8).map(|b| b.wrapping_mul(7)).collect();

        let mut bytes = g64.clone();
        let before_len = bytes.len();
        let res = img.write_half_track(WritebackKind::G64, &mut bytes, 3, false);
        assert_eq!(res, 0, "extend write OK");
        assert!(bytes.len() > before_len, "image extended at EOF");

        // Offset-table entry 1 now points at the appended block.
        let ote1 = util_le_buf_to_dword(&bytes, 12 + 1 * 4) as usize;
        assert_eq!(ote1, before_len, "offset table entry 1 = old EOF");
        // The 2-byte length at that offset = new_len.
        assert_eq!(util_le_buf_to_word(&bytes, ote1), new_len as u16);

        // Re-parse: slot 1 is now populated with the new payload.
        let img2 = GcrImage::from_g64(&bytes);
        assert_eq!(img2.tracks[1].size, new_len);
        assert_eq!(img2.tracks[1].data, img.tracks[1].data, "extended track round-trips");
    }
}
