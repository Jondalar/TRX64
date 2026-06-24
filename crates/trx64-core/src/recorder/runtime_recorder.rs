//! runtime_recorder.rs — Spec 766.5: the runtime recorder orchestrator.
//!
//! 1:1 PORT of the c64re TS
//!   C64ReverseEngineeringMCP/src/runtime/headless/recorder/runtime-recorder.ts
//! (class `RuntimeRecorder`), carrying the SAME anchor cadence touchpoint, the
//! two-ring (anchor + medium) handoff, the gen-gated medium shipping, and the
//! async query API (stats/list/getAnchor/findByCycle/getMedium/reconstruct).
//!
//! WHAT DIFFERS FROM THE TS (and why it is still 1:1 on the OBSERVABLE contract):
//!   The TS owns a worker thread + two SharedArrayBuffer rings; `captureAnchor` is
//!   the ONLY emu-thread touchpoint and the worker drains the rings into the store
//!   off-thread (BUG-049: no store read/eval on the hot path). TRX64's daemon is
//!   single-threaded — there is no thread to decouple — so `capture_anchor` writes
//!   the framed records into the (collapsed) ring and drains them into the store
//!   inline in the same call. The store is owned directly (no postMessage
//!   request/response). Every OBSERVABLE field is preserved: `produced` (anchors
//!   handed to the ring), `medium_shipped` (gen changes), the RecorderStats numbers,
//!   the medium gen-gate (an unchanged image is NOT re-shipped), and `reconstruct`
//!   re-injecting the referenced disk/cart media into the decoded anchor payload.
//!
//! The anchor `payload` is the RuntimeCheckpoint `serde_json::Value` tree
//! (capture_runtime_checkpoint), encoded by anchor_codec. A `MediumDescriptor` is
//! supplied by the caller (medium_source.rs) describing the live disk/cart media
//! generation + lazy bytes; the recorder ships bytes only on a gen change.

use serde_json::{json, Value};

use super::anchor_codec::{decode_anchor, AnchorEncoder};
use super::anchor_record::{
    decode_cart_medium, encode_medium_record, read_medium_header, write_anchor_header, AnchorHeader,
    MediumHeader, ANCHOR_HEADER_BYTES, MEDIUM_KIND_CART, MEDIUM_KIND_DISK, REC_ANCHOR, REC_MEDIUM,
};
use super::anchor_store::AnchorStore;
use super::medium_source::{MediumDescriptor, MediumKind};
use super::recorder_ring::{RecorderRecord, RecorderRing, RecorderRingLayout};

/// runtime-recorder.ts:46-47 — default ring geometries (the byte sizes match;
/// they bound the largest single record the ring accepts, NOT a per-anchor copy).
pub const DEFAULT_ANCHOR_LAYOUT: RecorderRingLayout = RecorderRingLayout {
    slot_payload_bytes: 384 * 1024,
    slot_count: 16,
};
pub const DEFAULT_MEDIUM_LAYOUT: RecorderRingLayout = RecorderRingLayout {
    slot_payload_bytes: 2 * 1024 * 1024,
    slot_count: 4,
};

/// runtime-recorder.ts:35-44 — `RuntimeRecorderOptions`.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeRecorderOptions {
    pub anchor_layout: RecorderRingLayout,
    pub medium_layout: RecorderRingLayout,
    /// Worker store slab bytes (scrub depth).
    pub capacity_bytes: usize,
    /// Medium versions retained per kind.
    pub medium_keep: usize,
}

impl Default for RuntimeRecorderOptions {
    fn default() -> Self {
        Self {
            anchor_layout: DEFAULT_ANCHOR_LAYOUT,
            medium_layout: DEFAULT_MEDIUM_LAYOUT,
            capacity_bytes: 32 * 1024 * 1024,
            medium_keep: 3,
        }
    }
}

/// runtime-recorder.ts:49-51 — `RecorderAnchorRef` (the public listing view).
#[derive(Debug, Clone, PartialEq)]
pub struct RecorderAnchorRef {
    pub seq: u64,
    pub cycle: f64,
    pub wall_ms: f64,
    pub disk_gen: i32,
    pub cart_gen: i32,
    pub schema_version: i32,
}

impl RecorderAnchorRef {
    /// JSON wire shape for the `recorder/list` reply (camelCase, matching the TS
    /// `RecorderAnchorRef` fields).
    pub fn to_json(&self) -> Value {
        json!({
            "seq": self.seq,
            "cycle": self.cycle,
            "wallMs": self.wall_ms,
            "diskGen": self.disk_gen,
            "cartGen": self.cart_gen,
            "schemaVersion": self.schema_version,
        })
    }
}

/// runtime-recorder.ts:52-56 — `RecorderStats`.
#[derive(Debug, Clone, PartialEq)]
pub struct RecorderStats {
    pub anchor_count: u64,
    pub oldest_cycle: Option<f64>,
    pub newest_cycle: Option<f64>,
    pub slab_bytes: u64,
    pub slab_used: u64,
    pub evicted: u64,
    pub medium_disk: Option<i32>,
    pub medium_cart: Option<i32>,
    pub dropped: u64,
}

impl RecorderStats {
    pub fn to_json(&self) -> Value {
        json!({
            "anchorCount": self.anchor_count,
            "oldestCycle": self.oldest_cycle,
            "newestCycle": self.newest_cycle,
            "slabBytes": self.slab_bytes,
            "slabUsed": self.slab_used,
            "evicted": self.evicted,
            "mediumDisk": self.medium_disk,
            "mediumCart": self.medium_cart,
            "dropped": self.dropped,
        })
    }
}

/// runtime-recorder.ts:58 — `class RuntimeRecorder`.
pub struct RuntimeRecorder {
    anchor_ring: RecorderRing,
    medium_ring: RecorderRing,
    store: AnchorStore,
    enc: AnchorEncoder,
    last_disk_gen: i32,
    last_cart_gen: i32,
    /// runtime-recorder.ts:69 — anchors handed to the ring (producer side).
    pub produced: u64,
    /// runtime-recorder.ts:71 — medium images shipped (gen changes).
    pub medium_shipped: u64,
    /// Scratch reused across captures for the drain (mirrors the worker's drain buf).
    drain_buf: Vec<RecorderRecord>,
}

impl RuntimeRecorder {
    /// runtime-recorder.ts:73-102 — constructor (wires the rings + store).
    pub fn new(opts: RuntimeRecorderOptions) -> Self {
        Self {
            anchor_ring: RecorderRing::new(opts.anchor_layout),
            medium_ring: RecorderRing::new(opts.medium_layout),
            store: AnchorStore::new(opts.capacity_bytes, opts.medium_keep),
            enc: AnchorEncoder::new(),
            last_disk_gen: -1,
            last_cart_gen: -1,
            produced: 0,
            medium_shipped: 0,
            drain_buf: Vec::new(),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(RuntimeRecorderOptions::default())
    }

    /// runtime-recorder.ts:113-127 — `captureAnchor(payload, cycle, wallMs,
    /// schemaVersion, media)`. The hot-path touchpoint (~2×/s): encode the codec
    /// into the reserved scratch, fill the header in place, write to the anchor
    /// ring; then gen-gate any media. In the single-threaded port the rings are
    /// drained into the store inline (the worker's job).
    ///
    /// `media` is the live disk/cart descriptors (medium_source::collect…); the
    /// recorder reads their O(1) generation and ships bytes only on a change.
    pub fn capture_anchor(
        &mut self,
        payload: &Value,
        cycle: f64,
        wall_ms: f64,
        schema_version: i32,
        media: &[MediumDescriptor],
    ) {
        let mut disk_gen = 0i32;
        let mut cart_gen = 0i32;
        for m in media {
            match m.kind {
                MediumKind::Disk => disk_gen = m.generation,
                MediumKind::Cart => cart_gen = m.generation,
            }
        }

        // runtime-recorder.ts:121-123 — encode into the reserved scratch, fill the
        // header in place, one ring write.
        let rec = self
            .enc
            .encode_with_reserve(ANCHOR_HEADER_BYTES, payload)
            .to_vec();
        let mut rec = rec;
        write_anchor_header(
            &mut rec,
            0,
            &AnchorHeader {
                cycle,
                wall_ms,
                disk_gen,
                cart_gen,
                schema_version,
            },
        );
        if self.anchor_ring.write(REC_ANCHOR, &rec) {
            self.produced += 1;
        }

        // runtime-recorder.ts:126 — medium gen-gate: ship bytes only on a change.
        for m in media {
            self.maybe_ship_medium(m, wall_ms);
        }

        // Single-threaded collapse: drain both rings into the store now (the
        // worker's drain loop). Preserves the store contract; the ring still
        // accounts for any lossy lapping in `dropped`.
        self.drain_into_store();
    }

    /// runtime-recorder.ts:129-143 — `maybeShipMedium(m, wallMs)`. Ships bytes only
    /// on a content-generation change; if the image exceeds the medium slot it is
    /// dropped and the gen is NOT advanced (a later capture retries).
    fn maybe_ship_medium(&mut self, m: &MediumDescriptor, wall_ms: f64) {
        let last = match m.kind {
            MediumKind::Disk => self.last_disk_gen,
            MediumKind::Cart => self.last_cart_gen,
        };
        if m.generation == last {
            return;
        }
        let Some(bytes) = (m.get_bytes)() else {
            return;
        };
        let kind = match m.kind {
            MediumKind::Disk => MEDIUM_KIND_DISK,
            MediumKind::Cart => MEDIUM_KIND_CART,
        };
        let mrec = encode_medium_record(
            &MediumHeader {
                kind,
                generation: m.generation,
                wall_ms,
            },
            &bytes,
        );
        if self.medium_ring.write(REC_MEDIUM, &mrec) {
            match m.kind {
                MediumKind::Disk => self.last_disk_gen = m.generation,
                MediumKind::Cart => self.last_cart_gen = m.generation,
            }
            self.medium_shipped += 1;
        }
    }

    /// The worker's drain loop, collapsed inline: pull every framed record off the
    /// rings and apply it to the store (anchors → put_anchor, media → put_medium).
    /// recorder-worker.ts:applyAnchorRecord / applyMediumRecord.
    fn drain_into_store(&mut self) {
        self.drain_buf.clear();
        self.anchor_ring.drain(&mut self.drain_buf);
        // Take the records out so we can mutably borrow self.store while iterating.
        let anchor_recs = std::mem::take(&mut self.drain_buf);
        for rec in &anchor_recs {
            if rec.r#type == REC_ANCHOR {
                let header = super::anchor_record::read_anchor_header(&rec.payload, 0);
                let codec = super::anchor_record::anchor_body(&rec.payload);
                let _ = self.store.put_anchor(&header, codec);
            }
        }
        let mut medium_recs = Vec::new();
        self.medium_ring.drain(&mut medium_recs);
        for rec in &medium_recs {
            if rec.r#type == REC_MEDIUM {
                let h = read_medium_header(&rec.payload, 0);
                let body = super::anchor_record::medium_body(&rec.payload);
                self.store.put_medium(h.kind, h.generation, body, h.wall_ms);
            }
        }
    }

    // ---- query API (runtime-recorder.ts:145-207) — synchronous in the daemon ----

    /// runtime-recorder.ts:157 — `stats()`.
    pub fn stats(&self) -> RecorderStats {
        let s = self.store.stats();
        let dropped = self.anchor_ring.dropped_count() + self.medium_ring.dropped_count();
        RecorderStats {
            anchor_count: s.anchor_count,
            oldest_cycle: s.oldest_cycle,
            newest_cycle: s.newest_cycle,
            slab_bytes: s.slab_bytes,
            slab_used: s.slab_used,
            evicted: s.evicted,
            medium_disk: s.medium_disk,
            medium_cart: s.medium_cart,
            dropped,
        }
    }

    /// runtime-recorder.ts:158 — `list()`.
    pub fn list(&self) -> Vec<RecorderAnchorRef> {
        self.store
            .list()
            .into_iter()
            .map(|e| RecorderAnchorRef {
                seq: e.seq,
                cycle: e.cycle,
                wall_ms: e.wall_ms,
                disk_gen: e.disk_gen,
                cart_gen: e.cart_gen,
                schema_version: e.schema_version,
            })
            .collect()
    }

    /// runtime-recorder.ts:161-164 — `getAnchor(seq)`: the codec bytes + header
    /// (None if evicted).
    pub fn get_anchor(&self, seq: u64) -> Option<(RecorderAnchorRef, Vec<u8>)> {
        let header = self.store.get_anchor_header(seq)?;
        let bytes = self.store.get_anchor_bytes(seq)?;
        Some((
            RecorderAnchorRef {
                seq,
                cycle: header.cycle,
                wall_ms: header.wall_ms,
                disk_gen: header.disk_gen,
                cart_gen: header.cart_gen,
                schema_version: header.schema_version,
            },
            bytes,
        ))
    }

    /// runtime-recorder.ts:166-169 — `findByCycle(cycle)`.
    pub fn find_by_cycle(&self, cycle: f64) -> Option<RecorderAnchorRef> {
        self.store.find_by_cycle(cycle).map(|e| RecorderAnchorRef {
            seq: e.seq,
            cycle: e.cycle,
            wall_ms: e.wall_ms,
            disk_gen: e.disk_gen,
            cart_gen: e.cart_gen,
            schema_version: e.schema_version,
        })
    }

    /// runtime-recorder.ts:171-174 — `getMedium(kind, gen)`.
    pub fn get_medium(&self, kind: u32, gen: i32) -> Option<(u32, i32, Vec<u8>)> {
        self.store
            .get_medium(kind, gen)
            .map(|m| (m.kind, m.generation, m.bytes.clone()))
    }

    /// runtime-recorder.ts:184-207 — `reconstruct(seq)`. Reassemble a full
    /// restorable checkpoint payload from a stored anchor: decode the core payload,
    /// then re-inject the LARGE medium fields it referenced (disk image, cart
    /// rom+flash) from the medium store. None if the anchor was evicted or a
    /// referenced medium is no longer retained.
    pub fn reconstruct(&self, seq: u64) -> Option<(RecorderAnchorRef, i32, Value)> {
        let (header, bytes) = self.get_anchor(seq)?;
        let mut payload = decode_anchor(&bytes).ok()?;
        let r#ref = header.clone();

        if header.disk_gen > 0 {
            let (_, _, mbytes) = self.get_medium(MEDIUM_KIND_DISK, header.disk_gen)?;
            payload["driveDiskImage"] = crate::native_snapshot::ta_u8(&mbytes);
        }
        // runtime-recorder.ts:198-205 — a cartridge is present iff the anchor
        // metadata carries it (cartGen alone is ambiguous).
        let has_cart = payload
            .get("media")
            .and_then(|m| m.get("cartridge"))
            .map(|c| !c.is_null())
            .unwrap_or(false);
        if has_cart {
            let (_, _, mbytes) = self.get_medium(MEDIUM_KIND_CART, header.cart_gen)?;
            let (rom, flash) = decode_cart_medium(&mbytes);
            payload["cartBytes"] = crate::native_snapshot::ta_u8(&rom);
            payload["cartFlash"] = match flash {
                Some(f) => crate::native_snapshot::ta_u8(&f),
                None => Value::Null,
            };
        }
        Some((r#ref, header.schema_version, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::medium_source::MediumDescriptor;
    use serde_json::json;

    fn mk_payload(fill: u8) -> Value {
        json!({
            "schemaVersion": 1,
            "ram": crate::native_snapshot::ta_u8(&vec![fill; 64]),
            "media": Value::Null,
        })
    }

    #[test]
    fn capture_then_list_and_get() {
        let mut r = RuntimeRecorder::with_defaults();
        r.capture_anchor(&mk_payload(0x11), 1000.0, 1.0, 1, &[]);
        r.capture_anchor(&mk_payload(0x22), 2000.0, 2.0, 1, &[]);
        assert_eq!(r.produced, 2);
        let list = r.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].cycle, 1000.0);
        assert_eq!(list[1].seq, 1);
        // get_anchor → decode → same RAM fill.
        let (_, bytes) = r.get_anchor(1).unwrap();
        let payload = decode_anchor(&bytes).unwrap();
        let ram = crate::native_snapshot::ta_u8_decode(&payload["ram"]).unwrap();
        assert_eq!(ram, vec![0x22; 64]);
        // stats roll up from the store.
        let s = r.stats();
        assert_eq!(s.anchor_count, 2);
        assert_eq!(s.dropped, 0);
    }

    #[test]
    fn find_by_cycle_seeks_the_anchor() {
        let mut r = RuntimeRecorder::with_defaults();
        for i in 1..=5u64 {
            r.capture_anchor(&mk_payload(i as u8), (i * 1000) as f64, i as f64, 1, &[]);
        }
        let a = r.find_by_cycle(3500.0).unwrap();
        assert_eq!(a.cycle, 3000.0);
        assert_eq!(a.seq, 2);
    }

    #[test]
    fn medium_gen_gate_ships_once_then_reconstruct_reinjects() {
        let mut r = RuntimeRecorder::with_defaults();
        let disk_bytes = vec![0xDD; 256];
        // Two captures at the SAME disk gen → the image ships once.
        for i in 0..2u64 {
            let db = disk_bytes.clone();
            let media = vec![MediumDescriptor {
                kind: MediumKind::Disk,
                generation: 7,
                get_bytes: Box::new(move || Some(db.clone())),
            }];
            // The anchor payload references the disk gen via its header (diskGen=7).
            r.capture_anchor(&mk_payload(i as u8), (i * 1000) as f64, i as f64, 1, &media);
        }
        assert_eq!(r.medium_shipped, 1, "unchanged disk shipped once (gen-gate)");
        // reconstruct re-injects the disk image into the decoded payload.
        let (_, _, payload) = r.reconstruct(1).unwrap();
        let got = crate::native_snapshot::ta_u8_decode(&payload["driveDiskImage"]).unwrap();
        assert_eq!(got, disk_bytes);
    }
}
