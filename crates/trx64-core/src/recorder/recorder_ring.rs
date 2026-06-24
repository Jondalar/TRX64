//! recorder_ring.rs — Spec 766.1: the runtime recorder's record-handoff ring.
//!
//! 1:1 PORT of the c64re TS
//!   C64ReverseEngineeringMCP/src/runtime/headless/recorder/recorder-ring.ts
//! (`RecorderRingProducer` + `RecorderRingConsumer`), carrying the SAME framing,
//! slot policy, lossy-overwrite discipline, and the drop accounting.
//!
//! WHAT DIFFERS FROM THE TS (and why it is still 1:1 on the OBSERVABLE contract):
//!   The TS ring is a LOSSY single-producer / single-consumer ring over a
//!   SharedArrayBuffer: the emulation thread (producer) memcpy's a framed record
//!   and bumps an Atomics write-counter, NEVER blocking on the recorder worker
//!   thread (consumer). The SAB + Atomics + seqlock + worker exist solely to keep
//!   the producer off the consumer's back across a THREAD boundary (BUG-049 — the
//!   recorder must be structurally unable to touch fps). TRX64's daemon is
//!   SINGLE-THREADED: the recorder's `captureAnchor` writes a record and the store
//!   drains it in the same call stack — there is no thread boundary to decouple,
//!   so the SAB/Atomics/seqlock machinery has no analog and no purpose.
//!
//!   This port therefore keeps the OBSERVABLE ring contract — fixed-size slots,
//!   the lossy overwrite-oldest-when-full policy, the `write`/`drain` framing
//!   (type + payload), and the `dropped` counter — over a plain in-process
//!   `VecDeque` of slots, which the single drain consumes. A record larger than a
//!   slot returns false (a caller sizing bug, NOT a transient "ring full"), exactly
//!   like the TS. With the recorder + store in one thread the ring never actually
//!   laps (the drain runs every capture), so `dropped` stays 0 on the normal path;
//!   the counter + the overwrite policy are kept so the contract (and a stress test
//!   that writes without draining) match the TS.

use std::collections::VecDeque;

/// recorder-ring.ts:42-47 — `RecorderRingLayout`.
#[derive(Debug, Clone, Copy)]
pub struct RecorderRingLayout {
    /// Usable payload bytes per slot.
    pub slot_payload_bytes: usize,
    /// Number of slots.
    pub slot_count: usize,
}

/// recorder-ring.ts:115-118 — `RecorderRecord` (a fresh copy handed back).
#[derive(Debug, Clone)]
pub struct RecorderRecord {
    pub r#type: u32,
    pub payload: Vec<u8>,
}

/// The combined producer+consumer ring (the TS split — producer on the emu thread,
/// consumer on the worker — collapses to one object in the single-threaded daemon).
pub struct RecorderRing {
    payload_cap: usize,
    slot_count: usize,
    /// Live slots, oldest → newest. Capped at `slot_count`: a write past capacity
    /// overwrites the oldest (lossy) and counts it dropped IFF it was never drained.
    slots: VecDeque<RecorderRecord>,
    /// recorder-ring.ts:CTRL_WRITE — monotonic write index (producer owns).
    write_count: u64,
    /// recorder-ring.ts:CTRL_READ — monotonic read index (consumer owns).
    read_count: u64,
    /// recorder-ring.ts:CTRL_DROPPED — slots overwritten before the consumer read them.
    dropped: u64,
}

impl RecorderRing {
    pub fn new(layout: RecorderRingLayout) -> Self {
        Self {
            payload_cap: layout.slot_payload_bytes,
            slot_count: layout.slot_count.max(1),
            slots: VecDeque::with_capacity(layout.slot_count.max(1)),
            write_count: 0,
            read_count: 0,
            dropped: 0,
        }
    }

    /// recorder-ring.ts:85-108 — `write(type, payload)`. Returns true if written,
    /// false ONLY if the payload exceeds the slot capacity (a caller sizing bug —
    /// never a transient "ring full": a full ring overwrites the oldest and still
    /// writes). The payload is copied out.
    pub fn write(&mut self, r#type: u32, payload: &[u8]) -> bool {
        if payload.len() > self.payload_cap {
            return false;
        }
        // Lossy overwrite-oldest when full. If the slot being evicted was never
        // drained (write_count - read_count would exceed slot_count), it is a drop.
        if self.slots.len() >= self.slot_count {
            // The unread depth before this write.
            if self.write_count - self.read_count >= self.slot_count as u64 {
                self.dropped += 1;
                self.read_count += 1; // the lapped slot is gone for the consumer
            }
            self.slots.pop_front();
        }
        self.slots.push_back(RecorderRecord {
            r#type,
            payload: payload.to_vec(),
        });
        self.write_count += 1;
        true
    }

    /// recorder-ring.ts:146-186 — `drain(out)`. Drains all currently-available
    /// records, oldest first, into `out`. Returns the count appended. (In the
    /// single-threaded port there is no lap/torn-read path — those guard a thread
    /// race the daemon does not have; the drop accounting is handled in `write`.)
    pub fn drain(&mut self, out: &mut Vec<RecorderRecord>) -> usize {
        let mut appended = 0;
        while let Some(rec) = self.slots.pop_front() {
            out.push(rec);
            self.read_count += 1;
            appended += 1;
        }
        appended
    }

    /// recorder-ring.ts:110/188 — `writeCount()`.
    pub fn write_count(&self) -> u64 {
        self.write_count
    }
    /// recorder-ring.ts:189 — `readCount()`.
    pub fn read_count(&self) -> u64 {
        self.read_count
    }
    /// recorder-ring.ts:111/190 — `droppedCount()`.
    pub fn dropped_count(&self) -> u64 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(cap: usize, n: usize) -> RecorderRing {
        RecorderRing::new(RecorderRingLayout {
            slot_payload_bytes: cap,
            slot_count: n,
        })
    }

    #[test]
    fn write_then_drain_roundtrips_records() {
        let mut r = ring(64, 4);
        assert!(r.write(1, &[0xAA, 0xBB]));
        assert!(r.write(2, &[0xCC]));
        let mut out = Vec::new();
        assert_eq!(r.drain(&mut out), 2);
        assert_eq!(out[0].r#type, 1);
        assert_eq!(out[0].payload, vec![0xAA, 0xBB]);
        assert_eq!(out[1].r#type, 2);
        assert_eq!(r.write_count(), 2);
        assert_eq!(r.read_count(), 2);
        assert_eq!(r.dropped_count(), 0);
    }

    #[test]
    fn oversized_payload_returns_false() {
        let mut r = ring(4, 4);
        assert!(!r.write(1, &[0; 5]), "5 > 4-byte slot cap");
        assert_eq!(r.write_count(), 0);
    }

    #[test]
    fn lapping_without_drain_counts_drops() {
        // 2 slots; write 4 without draining → 2 oldest lapped = 2 drops.
        let mut r = ring(8, 2);
        for i in 0..4u8 {
            r.write(1, &[i]);
        }
        assert_eq!(r.dropped_count(), 2, "two oldest overwritten unread");
        let mut out = Vec::new();
        r.drain(&mut out);
        // Only the 2 most-recent survive.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].payload, vec![2]);
        assert_eq!(out[1].payload, vec![3]);
    }

    #[test]
    fn drain_between_writes_never_drops() {
        let mut r = ring(8, 2);
        let mut out = Vec::new();
        for i in 0..10u8 {
            r.write(1, &[i]);
            r.drain(&mut out); // consumer keeps up → no lap
        }
        assert_eq!(r.dropped_count(), 0);
        assert_eq!(out.len(), 10);
    }
}
