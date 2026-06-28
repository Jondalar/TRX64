//! ring_dump.rs — Spec time-travel-tooling Piece 2: the `.c64rering` container.
//!
//! Serializes the WHOLE reverse-debug buffer (the checkpoint ring + the delta ring +
//! the cpu-history ring + the "current" anchor) into one self-contained, GZIPPED
//! container file so a tester can ship a bug's full context and a dev can reconstruct
//! it elsewhere. After a restore the scrub filmstrip (`checkpoint/list` / thumbnails),
//! `reverse_step`, `who_wrote`, `chis`, and `diffCheckpoints` ALL work on the dumped
//! buffer — the tester's run is fully explorable.
//!
//! CONTAINER FORMAT (versioned, gzip):
//!   bytes  0..7   MAGIC  "C64RERNG" (ascii)
//!   byte   8      FORMAT_VERSION (u8)
//!   bytes  9..    gzip(serde_json(`RingBufferDump`))
//!
//! The per-anchor states REUSE the existing RuntimeCheckpoint Value serialization (the
//! same shape `.c64re` / `restore_runtime_checkpoint` use) — NO new per-state format.
//! The three rings ride their own `to_dump`/`from_dump` (defined in their modules);
//! this module only frames + gzips the combined payload.
//!
//! Raw ring ≈ 90–160 MB; the flat LE slabs + gzip bring a `.c64rering` to ≈ 10–30 MB.

use std::io::{Read, Write};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};

use crate::checkpoint_ring::RuntimeCheckpointRingDump;
use crate::cpu_history::CpuHistoryRingDump;
use crate::delta_ring::DeltaRingDump;

/// Container magic — distinct from the `.c64re` snapshot ("C64RESNP") + the VSF magic.
pub const RING_DUMP_MAGIC: &[u8; 8] = b"C64RERNG";
/// Container format version. Bump on an incompatible payload-shape change.
pub const RING_DUMP_FORMAT_VERSION: u8 = 1;

/// The combined, serializable payload of a whole reverse-debug buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingBufferDump {
    /// The checkpoint ring (anchors = full machine states) + its "current" id.
    pub checkpoint_ring: RuntimeCheckpointRingDump,
    /// The always-on full-delta undo ring (instructions + writes + old_value).
    pub delta_ring: DeltaRingDump,
    /// The always-on cpu-history ring (instructions + opcodes).
    pub cpu_history: CpuHistoryRingDump,
}

/// Summary returned by dump/restore (the `RingDumpInfo` typed record): the anchor /
/// instruction counts + the cycle range + the "current" anchor id + the file size.
#[derive(Debug, Clone)]
pub struct RingDumpInfo {
    pub anchors: u64,
    pub delta_entries: u64,
    pub cpu_history: u64,
    pub cycle_first: u64,
    pub cycle_last: u64,
    pub current_id: Option<String>,
    pub file_bytes: u64,
    pub version: u32,
}

impl RingBufferDump {
    /// Derive the [`RingDumpInfo`] summary (minus `file_bytes`, filled by the caller
    /// once the container is framed). The cycle range spans the checkpoint anchors
    /// (the scrub timeline); empty rings report 0/0.
    pub fn info(&self, file_bytes: u64) -> RingDumpInfo {
        let anchors = &self.checkpoint_ring.entries;
        let cycle_first = anchors.first().map(|a| a.cycles).unwrap_or(0);
        let cycle_last = anchors.last().map(|a| a.cycles).unwrap_or(0);
        RingDumpInfo {
            anchors: anchors.len() as u64,
            delta_entries: self.delta_ring.entry_count() as u64,
            cpu_history: self.cpu_history.entry_count() as u64,
            cycle_first,
            cycle_last,
            current_id: self.checkpoint_ring.current_id.clone(),
            file_bytes,
            version: RING_DUMP_FORMAT_VERSION as u32,
        }
    }
}

/// Frame + gzip a [`RingBufferDump`] into the `.c64rering` container bytes.
pub fn write_ring_dump(dump: &RingBufferDump) -> Vec<u8> {
    let json = serde_json::to_vec(dump).expect("serialize RingBufferDump");
    let gz = gzip(&json);
    let mut out = Vec::with_capacity(9 + gz.len());
    out.extend_from_slice(RING_DUMP_MAGIC);
    out.push(RING_DUMP_FORMAT_VERSION);
    out.extend_from_slice(&gz);
    out
}

/// Parse + gunzip a `.c64rering` container back into a [`RingBufferDump`]. Validates
/// the magic + version.
pub fn read_ring_dump(bytes: &[u8]) -> Result<RingBufferDump, String> {
    if bytes.len() < 9 {
        return Err("ring-dump: file too short (need magic + version)".to_string());
    }
    if &bytes[0..8] != RING_DUMP_MAGIC {
        return Err(format!(
            "ring-dump: bad magic (expected {:?})",
            std::str::from_utf8(RING_DUMP_MAGIC).unwrap_or("C64RERNG")
        ));
    }
    let version = bytes[8];
    if version != RING_DUMP_FORMAT_VERSION {
        return Err(format!(
            "ring-dump: incompatible format version {version} (this build reads {RING_DUMP_FORMAT_VERSION})"
        ));
    }
    let json = gunzip(&bytes[9..])?;
    serde_json::from_slice(&json).map_err(|e| format!("ring-dump: bad payload JSON: {e}"))
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).expect("gzip write");
    enc.finish().expect("gzip finish")
}

fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut dec = GzDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|e| format!("ring-dump: gunzip failed: {e}"))?;
    Ok(out)
}

// ── `b64_bytes` serde adapter — flat byte payloads ride as base64 in JSON ──────────
//
// The three ring `*Dump` structs carry their slabs as flat `Vec<u8>` LE payloads. As
// a JSON number array those would be huge even before the container gzip; base64
// strings are ~4× smaller and gzip well. Used via `#[serde(with = "...::b64_bytes")]`.
pub mod b64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let enc = base64::engine::general_purpose::STANDARD.encode(bytes);
        s.serialize_str(&enc)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(s.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint_ring::{RuntimeCheckpointRing, SLOT_BYTES};
    use crate::cpu_history::CpuHistoryRing;
    use crate::delta_ring::DeltaRing;
    use serde_json::json;

    fn mk_checkpoint_payload(ram_fill: u8) -> serde_json::Value {
        let ram = vec![ram_fill; 0x10000];
        json!({
            "schemaVersion": 1,
            "ram": crate::native_snapshot::ta_u8(&ram),
            "driveDiskImage": serde_json::Value::Null,
        })
    }

    #[test]
    fn container_magic_version_roundtrip() {
        let mut cring = RuntimeCheckpointRing::with_budget(8 * SLOT_BYTES);
        cring.capture(mk_checkpoint_payload(0x11), 1, 100).unwrap();
        cring.capture(mk_checkpoint_payload(0x22), 2, 200).unwrap();
        let mut dring = DeltaRing::with_capacity(64, 64);
        dring.set_enabled(true);
        for i in 0..10u64 {
            dring.begin(0x1000 + i as u16, i as u8, 0, 0, 0xff, 0x20, 500 + i);
            dring.record_write(0x4000 + i as u16, 0, (i + 1) as u8);
            dring.commit();
        }
        let mut chist = CpuHistoryRing::with_capacity(64);
        chist.set_enabled(true);
        for i in 0..10u64 {
            chist.push(0x2000 + i as u16, 0xa9, i as u8, 0, 1, 2, 3, 0xf0, 0x30, 500 + i);
        }

        let dump = RingBufferDump {
            checkpoint_ring: cring.to_dump(Some("cp_2_1".to_string())),
            delta_ring: dring.to_dump(),
            cpu_history: chist.to_dump(),
        };
        let bytes = write_ring_dump(&dump);
        // Magic + version framing.
        assert_eq!(&bytes[0..8], RING_DUMP_MAGIC);
        assert_eq!(bytes[8], RING_DUMP_FORMAT_VERSION);

        let back = read_ring_dump(&bytes).unwrap();
        assert_eq!(back.checkpoint_ring.entries.len(), 2);
        assert_eq!(back.checkpoint_ring.current_id, Some("cp_2_1".to_string()));
        assert_eq!(back.delta_ring.entry_count(), 10);
        assert_eq!(back.cpu_history.entry_count(), 10);

        // Reconstruct the rings → identical observable state.
        let cring2 = RuntimeCheckpointRing::from_dump(&back.checkpoint_ring);
        assert_eq!(cring2.list().len(), 2);
        let ids: Vec<_> = cring2.list().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, vec!["cp_1_0", "cp_2_1"]);

        let dring2 = DeltaRing::from_dump(&back.delta_ring);
        assert_eq!(dring2.len(), 10);
        let newest = dring2.newest().unwrap();
        assert_eq!(newest.pc, 0x1009);
        // who_wrote works on the restored ring.
        let hits = dring2.who_wrote(0x4009, 4);
        assert!(!hits.is_empty(), "who_wrote on restored delta ring");
        assert_eq!(hits[0].1.new_value, 10);

        let chist2 = CpuHistoryRing::from_dump(&back.cpu_history);
        assert_eq!(chist2.len(), 10);
        let mut out = Vec::new();
        chist2.last_n(10, &mut out);
        assert_eq!(out.last().unwrap().pc, 0x2009);
        assert_eq!(out.first().unwrap().pc, 0x2000);
    }

    #[test]
    fn bad_magic_rejected() {
        let mut bad = vec![0u8; 32];
        bad[0..8].copy_from_slice(b"NOTARING");
        assert!(read_ring_dump(&bad).is_err());
    }
}
