//! vice_snapshot_stream.rs — VICE in-memory snapshot module-stream layer.
//!
//! 1:1 PORT of the c64re TS
//!   C64ReverseEngineeringMCP/src/runtime/headless/vice1541/snapshot.ts
//! which itself is a port of vice/src/snapshot.c (the module-stream layer). This
//! is the byte-exact FOUNDATION for the `cp.drive1541` VICE drive snapshot-module
//! blob (part 4 of the .c64re work): a random-access in-memory byte buffer with a
//! VICE-shaped module framing — 16-byte padded name + major + minor + LE dword
//! size (back-patched on close) + LE byte/word/dword/qword/byte_array primitives.
//!
//! The bytes this writes are IDENTICAL to what VICE's snapshot.c writes, so the
//! `drive_snapshot_write_module` / `viacore_snapshot_write_module` /
//! `drivecpu_snapshot_write_module` ports (PENDING — see c64re_snapshot.rs drive
//! field notes) layer cleanly on top to build a drive blob a live c64re daemon's
//! `drive_snapshot_read_module` can read, and vice-versa.
//!
//! Naming: VICE function names verbatim (snake_case) so the drive-module port can
//! mechanically transcribe `snapshot_module_write_dword(m, …)` etc.

/// snapshot.ts:21 — `SNAPSHOT_MODULE_NAME_LEN = 16`.
pub const SNAPSHOT_MODULE_NAME_LEN: usize = 16;

/// In-memory stand-in for VICE's `snapshot_t` (a FILE* + first_module_offset).
/// `buf` is the byte store; `pos` is ftell. Writes past the end extend it; writes
/// within it overwrite (used by `module_close` to back-patch the size dword).
#[derive(Debug, Clone, Default)]
pub struct SnapshotT {
    pub buf: Vec<u8>,
    pub pos: usize,
    pub first_module_offset: usize,
}

/// PORT OF: struct snapshot_module_s (snapshot.c).
#[derive(Debug, Clone)]
pub struct SnapshotModule {
    pub write_mode: bool,
    pub size: u32,        // size of the module (incl. header)
    pub offset: usize,    // offset of the module in the buffer
    pub size_offset: usize, // offset of the size field
}

impl SnapshotT {
    /// snapshot.ts:51 — `snapshot_create_in_memory`.
    pub fn create_in_memory() -> Self {
        Self::default()
    }

    /// snapshot.ts:56 — `snapshot_open_in_memory`.
    pub fn open_in_memory(bytes: &[u8]) -> Self {
        Self { buf: bytes.to_vec(), pos: 0, first_module_offset: 0 }
    }

    /// snapshot.ts:61 — `snapshot_to_bytes`.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.buf.clone()
    }

    // ── low-level FILE* equivalents (snapshot.c static helpers) ────────────────

    fn write_byte(&mut self, b: u8) {
        if self.pos >= self.buf.len() {
            self.buf.resize(self.pos + 1, 0);
        }
        self.buf[self.pos] = b;
        self.pos += 1;
    }
    fn write_word(&mut self, w: u16) {
        self.write_byte((w & 0xff) as u8);
        self.write_byte((w >> 8) as u8);
    }
    fn write_dword(&mut self, dw: u32) {
        self.write_word((dw & 0xffff) as u16);
        self.write_word((dw >> 16) as u16);
    }
    fn write_qword(&mut self, qw: u64) {
        self.write_dword((qw & 0xffff_ffff) as u32);
        self.write_dword((qw >> 32) as u32);
    }
    fn write_padded_string(&mut self, s: &str, pad: u8, len: usize) {
        let bytes = s.as_bytes();
        for i in 0..len {
            self.write_byte(if i < bytes.len() { bytes[i] } else { pad });
        }
    }
    fn read_byte(&mut self) -> Option<u8> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let v = self.buf[self.pos];
        self.pos += 1;
        Some(v)
    }
    fn read_word(&mut self) -> Option<u16> {
        let lo = self.read_byte()? as u16;
        let hi = self.read_byte()? as u16;
        Some(lo | (hi << 8))
    }
    fn read_dword(&mut self) -> Option<u32> {
        let lo = self.read_word()? as u32;
        let hi = self.read_word()? as u32;
        Some(lo | (hi << 16))
    }
    fn read_qword(&mut self) -> Option<u64> {
        let lo = self.read_dword()? as u64;
        let hi = self.read_dword()? as u64;
        Some((hi << 32) | lo)
    }

    // ── module create / open / close (snapshot.c:677-800) ──────────────────────

    /// snapshot.ts:112 — `snapshot_module_create`. Writes the 16-byte padded name
    /// + major + minor + a zero size dword (back-patched on close).
    pub fn module_create(&mut self, name: &str, major: u8, minor: u8) -> SnapshotModule {
        let offset = self.pos;
        self.write_padded_string(name, 0, SNAPSHOT_MODULE_NAME_LEN);
        self.write_byte(major);
        self.write_byte(minor);
        self.write_dword(0);
        let size = (self.pos - offset) as u32;
        let size_offset = self.pos - 4;
        SnapshotModule { write_mode: true, size, offset, size_offset }
    }

    /// snapshot.ts:128 — `snapshot_module_open`. Linear-scans the module list from
    /// `first_module_offset` for `name`. Returns (module, major, minor) on match.
    pub fn module_open(&mut self, name: &str) -> Option<(SnapshotModule, u8, u8)> {
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len();
        self.pos = self.first_module_offset;
        let mut m = SnapshotModule {
            write_mode: false,
            size: 0,
            offset: self.first_module_offset,
            size_offset: 0,
        };
        loop {
            let mut n = [0u8; SNAPSHOT_MODULE_NAME_LEN];
            for slot in n.iter_mut() {
                *slot = self.read_byte()?;
            }
            let major = self.read_byte()?;
            let minor = self.read_byte()?;
            let size = self.read_dword()?;
            m.size = size;
            let mut matched = name_len <= SNAPSHOT_MODULE_NAME_LEN;
            for i in 0..name_len {
                if n.get(i).copied() != Some(name_bytes[i]) {
                    matched = false;
                    break;
                }
            }
            if matched && (name_len == SNAPSHOT_MODULE_NAME_LEN || n[name_len] == 0) {
                m.size_offset = self.pos - 4;
                return Some((m, major, minor));
            }
            m.offset += m.size as usize;
            if m.offset >= self.buf.len() {
                return None;
            }
            self.pos = m.offset;
        }
    }

    /// snapshot.ts:154 — `snapshot_module_close`. Back-patches the size dword (in
    /// write mode), then skips past the module.
    pub fn module_close(&mut self, m: &SnapshotModule) {
        if m.write_mode {
            self.pos = m.size_offset;
            self.write_dword(m.size);
        }
        self.pos = m.offset + m.size as usize;
    }

    // ── module write primitives (snapshot.c:384-475; bump m.size) ──────────────

    pub fn smw_b(&mut self, m: &mut SnapshotModule, b: u8) {
        self.write_byte(b);
        m.size += 1;
    }
    pub fn smw_w(&mut self, m: &mut SnapshotModule, w: u16) {
        self.write_word(w);
        m.size += 2;
    }
    pub fn smw_dw(&mut self, m: &mut SnapshotModule, dw: u32) {
        self.write_dword(dw);
        m.size += 4;
    }
    /// SMW_CLOCK — qword (8 bytes LE) for VICE CLOCK fields.
    pub fn smw_clock(&mut self, m: &mut SnapshotModule, qw: u64) {
        self.write_qword(qw);
        m.size += 8;
    }
    pub fn smw_ba(&mut self, m: &mut SnapshotModule, b: &[u8], num: usize) {
        for i in 0..num {
            self.write_byte(b.get(i).copied().unwrap_or(0));
        }
        m.size += num as u32;
    }
    pub fn smw_padded_string(&mut self, m: &mut SnapshotModule, s: &str, pad: u8, len: usize) {
        self.write_padded_string(s, pad, len);
        m.size += len as u32;
    }

    // ── module read primitives (snapshot.c:489-560) ────────────────────────────

    pub fn smr_b(&mut self) -> Option<u8> {
        self.read_byte()
    }
    pub fn smr_w(&mut self) -> Option<u16> {
        self.read_word()
    }
    pub fn smr_dw(&mut self) -> Option<u32> {
        self.read_dword()
    }
    pub fn smr_clock(&mut self) -> Option<u64> {
        self.read_qword()
    }
    pub fn smr_ba(&mut self, out: &mut [u8], num: usize) -> bool {
        for slot in out.iter_mut().take(num) {
            match self.read_byte() {
                Some(v) => *slot = v,
                None => return false,
            }
        }
        true
    }
}

/// snapshot.ts:239 — `snapshot_version_is_bigger`.
pub fn snapshot_version_is_bigger(maj: u8, min: u8, ref_maj: u8, ref_min: u8) -> bool {
    maj > ref_maj || (maj == ref_maj && min > ref_min)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_framing_roundtrip() {
        // Write two modules with mixed primitives, then read them back — the
        // size back-patch + the 16-byte name framing must round-trip byte-exact.
        let mut s = SnapshotT::create_in_memory();
        let mut m = s.module_create("DRIVE", 2, 0);
        s.smw_b(&mut m, 0xab);
        s.smw_w(&mut m, 0x1234);
        s.smw_dw(&mut m, 0xdead_beef);
        s.smw_clock(&mut m, 0x0000_0001_0000_0002);
        s.smw_ba(&mut m, &[1, 2, 3, 4, 5], 5);
        s.module_close(&m);

        let mut m2 = s.module_create("VIA2D0", 1, 1);
        s.smw_dw(&mut m2, 0xcafe_babe);
        s.module_close(&m2);

        let bytes = s.to_bytes();

        // The first module name is null-padded to 16 bytes, then 2 version bytes,
        // then the LE size dword.
        assert_eq!(&bytes[0..5], b"DRIVE");
        assert_eq!(&bytes[5..16], &[0u8; 11]);
        assert_eq!(bytes[16], 2); // major
        assert_eq!(bytes[17], 0); // minor

        // Read it back through a fresh stream.
        let mut r = SnapshotT::open_in_memory(&bytes);
        let (m_open, major, minor) = r.module_open("DRIVE").expect("open DRIVE");
        assert_eq!(major, 2);
        assert_eq!(minor, 0);
        let _ = m_open;
        assert_eq!(r.smr_b().unwrap(), 0xab);
        assert_eq!(r.smr_w().unwrap(), 0x1234);
        assert_eq!(r.smr_dw().unwrap(), 0xdead_beef);
        assert_eq!(r.smr_clock().unwrap(), 0x0000_0001_0000_0002);
        let mut ba = [0u8; 5];
        assert!(r.smr_ba(&mut ba, 5));
        assert_eq!(ba, [1, 2, 3, 4, 5]);

        // The second module is found by the linear scan past the first.
        let (_m2, maj2, min2) = r.module_open("VIA2D0").expect("open VIA2D0");
        assert_eq!((maj2, min2), (1, 1));
        assert_eq!(r.smr_dw().unwrap(), 0xcafe_babe);
    }

    #[test]
    fn module_open_missing_returns_none() {
        let mut s = SnapshotT::create_in_memory();
        let m = s.module_create("DRIVE", 1, 0);
        s.module_close(&m);
        let mut r = SnapshotT::open_in_memory(&s.to_bytes());
        assert!(r.module_open("NOPE").is_none());
    }

    #[test]
    fn qword_le_order() {
        // VICE writes the low dword then the high dword (LE).
        let mut s = SnapshotT::create_in_memory();
        let mut m = s.module_create("X", 1, 0);
        s.smw_clock(&mut m, 0x1122_3344_5566_7788);
        s.module_close(&m);
        let bytes = s.to_bytes();
        // After the 16+1+1+4 = 22-byte header, the qword is 88 77 66 55 44 33 22 11.
        let q = &bytes[22..30];
        assert_eq!(q, &[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]);
    }

    #[test]
    fn version_compare() {
        assert!(snapshot_version_is_bigger(2, 0, 1, 9));
        assert!(snapshot_version_is_bigger(1, 5, 1, 4));
        assert!(!snapshot_version_is_bigger(1, 4, 1, 5));
    }
}
