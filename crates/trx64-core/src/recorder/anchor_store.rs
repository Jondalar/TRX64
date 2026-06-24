//! anchor_store.rs — Spec 766.4: the recorder's anchor store.
//!
//! 1:1 PORT of the c64re TS
//!   C64ReverseEngineeringMCP/src/runtime/headless/recorder/anchor-store.ts
//! (class `AnchorStore`).
//!
//! In c64re this lives ENTIRELY on the recorder worker thread (BUG-049: no store
//! read/eval on the emu hot path). TRX64's daemon is single-threaded, so the store
//! is owned directly by the recorder (runtime_recorder.rs); the OBSERVABLE policy
//! (byte-ring depth, eviction, medium dedup, the stats numbers) is reproduced 1:1.
//!
//! Byte-ring discipline (anchor-store.ts:12-18): writes are CONTIGUOUS in an
//! absolute (never-wrapping) position space; the physical slab offset is
//! `abs % capacity`. A record that would straddle the slab end skips the tail gap
//! (advances abs to the next slab boundary) so every stored body is one contiguous
//! physical run — trivial byte-exact read-back. An index entry is alive while its
//! start is within the last `capacity` bytes of the frontier; older entries are
//! evicted (their bytes were overwritten).

use super::anchor_record::{AnchorHeader, MEDIUM_KIND_CART, MEDIUM_KIND_DISK};

/// anchor-store.ts:25-35 — `AnchorIndexEntry`.
#[derive(Debug, Clone)]
pub struct AnchorIndexEntry {
    pub seq: u64,
    pub cycle: f64,
    pub wall_ms: f64,
    pub disk_gen: i32,
    pub cart_gen: i32,
    pub schema_version: i32,
    /// Absolute byte position of the codec body.
    pub abs_start: u64,
    /// Physical slab offset of the codec body.
    pub phys: usize,
    /// Codec body byte length.
    pub len: usize,
}

/// anchor-store.ts:37 — `StoredMedium`.
#[derive(Debug, Clone)]
pub struct StoredMedium {
    pub kind: u32,
    pub generation: i32,
    pub bytes: Vec<u8>,
    pub wall_ms: f64,
}

/// anchor-store.ts:39-48 — `AnchorStoreStats`.
#[derive(Debug, Clone, PartialEq)]
pub struct AnchorStoreStats {
    pub anchor_count: u64,
    pub oldest_cycle: Option<f64>,
    pub newest_cycle: Option<f64>,
    pub slab_bytes: u64,
    pub slab_used: u64,
    pub evicted: u64,
    pub medium_disk: Option<i32>,
    pub medium_cart: Option<i32>,
}

/// anchor-store.ts:50 — `class AnchorStore`.
pub struct AnchorStore {
    slab: Vec<u8>,
    capacity: usize,
    write_abs: u64,
    next_seq: u64,
    evicted: u64,
    /// oldest → newest.
    entries: Vec<AnchorIndexEntry>,
    medium_keep: usize,
    /// kind → recent stored media (oldest → newest).
    medium_by_kind: std::collections::HashMap<u32, Vec<StoredMedium>>,
}

impl AnchorStore {
    /// anchor-store.ts:62-66 — `new AnchorStore(capacityBytes = 32 MiB, mediumKeep = 3)`.
    pub fn new(capacity_bytes: usize, medium_keep: usize) -> Self {
        Self {
            slab: vec![0u8; capacity_bytes],
            capacity: capacity_bytes,
            write_abs: 0,
            next_seq: 0,
            evicted: 0,
            entries: Vec::new(),
            medium_keep: medium_keep.max(1),
            medium_by_kind: std::collections::HashMap::new(),
        }
    }

    /// anchor-store.ts:69-97 — `putAnchor(header, codec)`. Stores one anchor
    /// (header carried in the index, codec body in the slab). Returns the seq.
    pub fn put_anchor(&mut self, header: &AnchorHeader, codec: &[u8]) -> Result<u64, String> {
        let len = codec.len();
        if len > self.capacity {
            return Err(format!(
                "anchor ({len}B) exceeds slab capacity ({}B)",
                self.capacity
            ));
        }

        let mut phys = (self.write_abs % self.capacity as u64) as usize;
        if phys + len > self.capacity {
            // would straddle the end → skip the tail gap.
            self.write_abs += (self.capacity - phys) as u64;
            phys = 0;
        }
        let abs_start = self.write_abs;
        self.slab[phys..phys + len].copy_from_slice(codec);
        self.write_abs += len as u64;

        let entry = AnchorIndexEntry {
            seq: self.next_seq,
            cycle: header.cycle,
            wall_ms: header.wall_ms,
            disk_gen: header.disk_gen,
            cart_gen: header.cart_gen,
            schema_version: header.schema_version,
            abs_start,
            phys,
            len,
        };
        self.next_seq += 1;
        let seq = entry.seq;
        self.entries.push(entry);

        // anchor-store.ts:91-95 — evict everything the new frontier overwrote.
        let live_floor = self.write_abs.saturating_sub(self.capacity as u64);
        while !self.entries.is_empty() && self.entries[0].abs_start < live_floor {
            self.entries.remove(0);
            self.evicted += 1;
        }
        Ok(seq)
    }

    /// anchor-store.ts:100-106 — `putMedium(kind, generation, bytes, wallMs)`.
    /// Stores / dedups a medium image by (kind, generation). Idempotent per gen.
    pub fn put_medium(&mut self, kind: u32, generation: i32, bytes: &[u8], wall_ms: f64) {
        let list = self.medium_by_kind.entry(kind).or_default();
        if list.iter().any(|m| m.generation == generation) {
            return; // already have this version
        }
        list.push(StoredMedium {
            kind,
            generation,
            bytes: bytes.to_vec(),
            wall_ms,
        });
        while list.len() > self.medium_keep {
            list.remove(0); // evict oldest version
        }
    }

    /// anchor-store.ts:109-113 — `getAnchorBytes(seq)`. Copy out the codec body
    /// (None if evicted).
    pub fn get_anchor_bytes(&self, seq: u64) -> Option<Vec<u8>> {
        let e = self.entries.iter().find(|x| x.seq == seq)?;
        Some(self.slab[e.phys..e.phys + e.len].to_vec())
    }

    /// anchor-store.ts:118-122 — `getAnchorHeader(seq)`. Cheap header (carried in
    /// the index; None if evicted).
    pub fn get_anchor_header(&self, seq: u64) -> Option<AnchorHeader> {
        let e = self.entries.iter().find(|x| x.seq == seq)?;
        Some(AnchorHeader {
            cycle: e.cycle,
            wall_ms: e.wall_ms,
            disk_gen: e.disk_gen,
            cart_gen: e.cart_gen,
            schema_version: e.schema_version,
        })
    }

    /// anchor-store.ts:125-131 — `findByCycle(cycle)`. The newest stored anchor at
    /// or before `cycle`, or None.
    pub fn find_by_cycle(&self, cycle: f64) -> Option<&AnchorIndexEntry> {
        let mut best: Option<&AnchorIndexEntry> = None;
        for e in &self.entries {
            if e.cycle <= cycle && (best.is_none() || e.cycle > best.unwrap().cycle) {
                best = Some(e);
            }
        }
        best
    }

    /// anchor-store.ts:134 — `list()`. Light listing (oldest → newest), no bodies.
    pub fn list(&self) -> Vec<AnchorIndexEntry> {
        self.entries.clone()
    }

    /// anchor-store.ts:137-139 — `getMedium(kind, generation)`.
    pub fn get_medium(&self, kind: u32, generation: i32) -> Option<&StoredMedium> {
        self.medium_by_kind
            .get(&kind)?
            .iter()
            .find(|m| m.generation == generation)
    }

    /// anchor-store.ts:142-145 — `latestMedium(kind)`.
    pub fn latest_medium(&self, kind: u32) -> Option<&StoredMedium> {
        self.medium_by_kind.get(&kind).and_then(|l| l.last())
    }

    /// anchor-store.ts:147-161 — `stats()`.
    pub fn stats(&self) -> AnchorStoreStats {
        let n = self.entries.len();
        let mut used = 0u64;
        for e in &self.entries {
            used += e.len as u64;
        }
        AnchorStoreStats {
            anchor_count: n as u64,
            oldest_cycle: self.entries.first().map(|e| e.cycle),
            newest_cycle: self.entries.last().map(|e| e.cycle),
            slab_bytes: self.capacity as u64,
            slab_used: used,
            evicted: self.evicted,
            medium_disk: self.latest_medium(MEDIUM_KIND_DISK).map(|m| m.generation),
            medium_cart: self.latest_medium(MEDIUM_KIND_CART).map(|m| m.generation),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(cycle: f64) -> AnchorHeader {
        AnchorHeader {
            cycle,
            wall_ms: cycle,
            disk_gen: 0,
            cart_gen: 0,
            schema_version: 1,
        }
    }

    #[test]
    fn put_get_roundtrip_and_seq() {
        let mut s = AnchorStore::new(1 << 16, 3);
        let s0 = s.put_anchor(&hdr(100.0), &[1, 2, 3]).unwrap();
        let s1 = s.put_anchor(&hdr(200.0), &[4, 5, 6, 7]).unwrap();
        assert_eq!(s0, 0);
        assert_eq!(s1, 1);
        assert_eq!(s.get_anchor_bytes(s0).unwrap(), vec![1, 2, 3]);
        assert_eq!(s.get_anchor_bytes(s1).unwrap(), vec![4, 5, 6, 7]);
        assert_eq!(s.get_anchor_header(s1).unwrap().cycle, 200.0);
        assert_eq!(s.stats().anchor_count, 2);
    }

    #[test]
    fn find_by_cycle_returns_newest_at_or_before() {
        let mut s = AnchorStore::new(1 << 16, 3);
        s.put_anchor(&hdr(100.0), &[0]).unwrap();
        s.put_anchor(&hdr(200.0), &[0]).unwrap();
        s.put_anchor(&hdr(300.0), &[0]).unwrap();
        assert_eq!(s.find_by_cycle(250.0).unwrap().cycle, 200.0);
        assert_eq!(s.find_by_cycle(300.0).unwrap().cycle, 300.0);
        assert!(s.find_by_cycle(50.0).is_none());
    }

    #[test]
    fn byte_ring_evicts_oldest_and_skips_tail_gap() {
        // Tiny slab (10 bytes). Each anchor body is 4 bytes.
        let mut s = AnchorStore::new(10, 3);
        let a = s.put_anchor(&hdr(1.0), &[0xA; 4]).unwrap(); // abs 0..4
        let b = s.put_anchor(&hdr(2.0), &[0xB; 4]).unwrap(); // abs 4..8
                                                             // Next 4 bytes would straddle (8+4>10) → skip to abs 10, phys 0,
                                                             // overwriting a's bytes. a evicted (abs_start 0 < frontier-cap).
        let c = s.put_anchor(&hdr(3.0), &[0xC; 4]).unwrap(); // abs 10..14
        assert!(s.get_anchor_bytes(a).is_none(), "a evicted");
        assert_eq!(s.get_anchor_bytes(b).unwrap(), vec![0xB; 4], "b survives");
        assert_eq!(s.get_anchor_bytes(c).unwrap(), vec![0xC; 4]);
        assert_eq!(s.stats().evicted, 1);
    }

    #[test]
    fn medium_dedup_and_keep_window() {
        let mut s = AnchorStore::new(1 << 16, 2);
        s.put_medium(MEDIUM_KIND_DISK, 1, &[0; 8], 0.0);
        s.put_medium(MEDIUM_KIND_DISK, 1, &[0; 8], 0.0); // dup gen → ignored
        assert_eq!(s.medium_by_kind.get(&MEDIUM_KIND_DISK).unwrap().len(), 1);
        s.put_medium(MEDIUM_KIND_DISK, 2, &[0; 8], 0.0);
        s.put_medium(MEDIUM_KIND_DISK, 3, &[0; 8], 0.0); // keep=2 → gen1 evicted
        assert!(s.get_medium(MEDIUM_KIND_DISK, 1).is_none());
        assert!(s.get_medium(MEDIUM_KIND_DISK, 2).is_some());
        assert_eq!(s.latest_medium(MEDIUM_KIND_DISK).unwrap().generation, 3);
        assert_eq!(s.stats().medium_disk, Some(3));
    }
}
