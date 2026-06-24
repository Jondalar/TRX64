//! anchor_record.rs — Spec 766.4: recorder ring record framing.
//!
//! 1:1 PORT of the c64re TS
//!   C64ReverseEngineeringMCP/src/runtime/headless/recorder/anchor-record.ts
//!
//! Two record kinds travel through the recorder ring (recorder_ring.rs):
//!   REC_ANCHOR — a full machine anchor: a small fixed header (capture cycle,
//!     wall-clock ms, current disk + cart medium generations) followed by the
//!     anchor codec bytes (RAM + chip state, anchor_codec.rs). NO medium bytes.
//!   REC_MEDIUM — a (large) medium image: a fixed header (kind + generation +
//!     wall-clock ms) followed by the raw .crt / disk image bytes. Shipped only
//!     on a medium gen change (medium_source.rs), so the cart is NOT re-sent every
//!     anchor — that per-second copy was the BUG-049 monster.
//!
//! The headers are FIXED-LAYOUT little-endian, identical byte offsets to the TS.

// anchor-record.ts:18-22 — record + medium kind constants.
pub const REC_ANCHOR: u32 = 1;
pub const REC_MEDIUM: u32 = 2;

pub const MEDIUM_KIND_DISK: u32 = 0;
pub const MEDIUM_KIND_CART: u32 = 1;

// ---- anchor record header (anchor-record.ts:24-30) --------------------------
// off 0  f64 cycle          (machine clock at capture)
// off 8  f64 wallMs         (wall-clock ms at capture)
// off 16 i32 diskGen        (disk medium generation referenced by this anchor)
// off 20 i32 cartGen        (cart medium generation referenced by this anchor)
// off 24 i32 schemaVersion  (RuntimeCheckpoint schema — self-describing for restore)
pub const ANCHOR_HEADER_BYTES: usize = 28;

/// anchor-record.ts:32-38 — `AnchorHeader`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnchorHeader {
    pub cycle: f64,
    pub wall_ms: f64,
    pub disk_gen: i32,
    pub cart_gen: i32,
    pub schema_version: i32,
}

/// anchor-record.ts:40-47 — `writeAnchorHeader(dst, off, h)`. Fills the fixed
/// header in place into `dst` at `off`. `dst[off..off+28]` must exist.
pub fn write_anchor_header(dst: &mut [u8], off: usize, h: &AnchorHeader) {
    dst[off..off + 8].copy_from_slice(&h.cycle.to_le_bytes());
    dst[off + 8..off + 16].copy_from_slice(&h.wall_ms.to_le_bytes());
    dst[off + 16..off + 20].copy_from_slice(&h.disk_gen.to_le_bytes());
    dst[off + 20..off + 24].copy_from_slice(&h.cart_gen.to_le_bytes());
    dst[off + 24..off + 28].copy_from_slice(&h.schema_version.to_le_bytes());
}

/// anchor-record.ts:49-58 — `readAnchorHeader(src, off)`.
pub fn read_anchor_header(src: &[u8], off: usize) -> AnchorHeader {
    let f64_at = |o: usize| {
        let mut a = [0u8; 8];
        a.copy_from_slice(&src[o..o + 8]);
        f64::from_le_bytes(a)
    };
    let i32_at = |o: usize| {
        let mut a = [0u8; 4];
        a.copy_from_slice(&src[o..o + 4]);
        i32::from_le_bytes(a)
    };
    AnchorHeader {
        cycle: f64_at(off),
        wall_ms: f64_at(off + 8),
        disk_gen: i32_at(off + 16),
        cart_gen: i32_at(off + 20),
        schema_version: i32_at(off + 24),
    }
}

// ---- medium record header (anchor-record.ts:60-64) --------------------------
// off 0  u32 kind        (MEDIUM_KIND_DISK | MEDIUM_KIND_CART)
// off 4  i32 generation  (the medium content generation these bytes are)
// off 8  f64 wallMs      (wall-clock ms at capture)
pub const MEDIUM_HEADER_BYTES: usize = 16;

/// anchor-record.ts:66-70 — `MediumHeader`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediumHeader {
    pub kind: u32,
    pub generation: i32,
    pub wall_ms: f64,
}

/// anchor-record.ts:72-77 — `writeMediumHeader(dst, off, h)`.
pub fn write_medium_header(dst: &mut [u8], off: usize, h: &MediumHeader) {
    dst[off..off + 4].copy_from_slice(&h.kind.to_le_bytes());
    dst[off + 4..off + 8].copy_from_slice(&h.generation.to_le_bytes());
    dst[off + 8..off + 16].copy_from_slice(&h.wall_ms.to_le_bytes());
}

/// anchor-record.ts:79-86 — `readMediumHeader(src, off)`.
pub fn read_medium_header(src: &[u8], off: usize) -> MediumHeader {
    let u32_at = |o: usize| {
        let mut a = [0u8; 4];
        a.copy_from_slice(&src[o..o + 4]);
        u32::from_le_bytes(a)
    };
    let i32_at = |o: usize| {
        let mut a = [0u8; 4];
        a.copy_from_slice(&src[o..o + 4]);
        i32::from_le_bytes(a)
    };
    let mut f = [0u8; 8];
    f.copy_from_slice(&src[off + 8..off + 16]);
    MediumHeader {
        kind: u32_at(off),
        generation: i32_at(off + 4),
        wall_ms: f64::from_le_bytes(f),
    }
}

// ---- alloc helpers (anchor-record.ts:88-106) --------------------------------

/// anchor-record.ts:90-95 — `encodeAnchorRecord(h, codec)`.
pub fn encode_anchor_record(h: &AnchorHeader, codec: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; ANCHOR_HEADER_BYTES + codec.len()];
    write_anchor_header(&mut out, 0, h);
    out[ANCHOR_HEADER_BYTES..].copy_from_slice(codec);
    out
}

/// anchor-record.ts:97-102 — `encodeMediumRecord(h, bytes)`.
pub fn encode_medium_record(h: &MediumHeader, bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; MEDIUM_HEADER_BYTES + bytes.len()];
    write_medium_header(&mut out, 0, h);
    out[MEDIUM_HEADER_BYTES..].copy_from_slice(bytes);
    out
}

/// anchor-record.ts:105 — `anchorBody(rec)` — the codec body past the header.
pub fn anchor_body(rec: &[u8]) -> &[u8] {
    &rec[ANCHOR_HEADER_BYTES..]
}

/// anchor-record.ts:106 — `mediumBody(rec)` — the medium bytes past the header.
pub fn medium_body(rec: &[u8]) -> &[u8] {
    &rec[MEDIUM_HEADER_BYTES..]
}

// ---- cart medium bundle (anchor-record.ts:108-132) --------------------------
// Layout: [u32 romLen][rom bytes][u32 flashLen][flash bytes].

/// anchor-record.ts:114-123 — `encodeCartMedium(rom, flash)`.
pub fn encode_cart_medium(rom: &[u8], flash: Option<&[u8]>) -> Vec<u8> {
    let flash_len = flash.map_or(0, |f| f.len());
    let mut out = vec![0u8; 4 + rom.len() + 4 + flash_len];
    out[0..4].copy_from_slice(&(rom.len() as u32).to_le_bytes());
    out[4..4 + rom.len()].copy_from_slice(rom);
    let off = 4 + rom.len();
    out[off..off + 4].copy_from_slice(&(flash_len as u32).to_le_bytes());
    if let Some(f) = flash {
        if flash_len > 0 {
            out[off + 4..off + 4 + flash_len].copy_from_slice(f);
        }
    }
    out
}

/// anchor-record.ts:125-132 — `decodeCartMedium(bytes)`.
pub fn decode_cart_medium(bytes: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    let u32_at = |o: usize| {
        let mut a = [0u8; 4];
        a.copy_from_slice(&bytes[o..o + 4]);
        u32::from_le_bytes(a) as usize
    };
    let rom_len = u32_at(0);
    let rom = bytes[4..4 + rom_len].to_vec();
    let flash_len = u32_at(4 + rom_len);
    let flash = if flash_len > 0 {
        Some(bytes[8 + rom_len..8 + rom_len + flash_len].to_vec())
    } else {
        None
    };
    (rom, flash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_header_roundtrips_at_offset() {
        let h = AnchorHeader {
            cycle: 123456.0,
            wall_ms: 987654321.0,
            disk_gen: 7,
            cart_gen: 3,
            schema_version: 2,
        };
        // Write at a non-zero offset to exercise the reserve path.
        let mut buf = vec![0u8; ANCHOR_HEADER_BYTES + 5];
        write_anchor_header(&mut buf, 5, &h);
        assert_eq!(read_anchor_header(&buf, 5), h);
    }

    #[test]
    fn medium_header_roundtrips() {
        let h = MediumHeader {
            kind: MEDIUM_KIND_CART,
            generation: 11,
            wall_ms: 42.0,
        };
        let mut buf = vec![0u8; MEDIUM_HEADER_BYTES];
        write_medium_header(&mut buf, 0, &h);
        assert_eq!(read_medium_header(&buf, 0), h);
    }

    #[test]
    fn anchor_record_header_then_body() {
        let h = AnchorHeader {
            cycle: 1.0,
            wall_ms: 2.0,
            disk_gen: 0,
            cart_gen: 0,
            schema_version: 1,
        };
        let codec = vec![0xAA, 0xBB, 0xCC];
        let rec = encode_anchor_record(&h, &codec);
        assert_eq!(read_anchor_header(&rec, 0), h);
        assert_eq!(anchor_body(&rec), &codec[..]);
    }

    #[test]
    fn cart_medium_roundtrips_with_and_without_flash() {
        let rom = vec![1u8, 2, 3, 4, 5];
        let flash = vec![9u8, 8, 7];
        let with = encode_cart_medium(&rom, Some(&flash));
        let (r, f) = decode_cart_medium(&with);
        assert_eq!(r, rom);
        assert_eq!(f, Some(flash));

        let without = encode_cart_medium(&rom, None);
        let (r2, f2) = decode_cart_medium(&without);
        assert_eq!(r2, rom);
        assert_eq!(f2, None);
    }
}
