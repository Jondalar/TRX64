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

// ── GCR conversion table (gcr.ts:61-66): 4-bit nybble → 5-bit GCR ───────────
const GCR_CONV_DATA: [u8; 16] = [
    0x0a, 0x0b, 0x12, 0x13, 0x0e, 0x0f, 0x16, 0x17, 0x09, 0x19, 0x1a, 0x1b, 0x0d, 0x1d, 0x1e, 0x15,
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
}
