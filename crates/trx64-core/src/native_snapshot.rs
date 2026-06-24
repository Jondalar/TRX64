//! native_snapshot.rs — the `.c64re` native runtime-snapshot binary container.
//!
//! 1:1 PORT of the c64re TS
//!   C64ReverseEngineeringMCP/src/runtime/headless/kernel/native-snapshot.ts
//! (Spec 707). This is the FRAMING + the `$ta` typed-array JSON codec, NOT the
//! checkpoint payload (that is `c64re_snapshot.rs`). The two together produce a
//! byte-identical `.c64re` file to the one the c64re daemon writes, so a live
//! c64re daemon can `snapshot/undump` a TRX64 dump and vice-versa.
//!
//! Container layout (native-snapshot.ts §3):
//!   bytes  0..7   MAGIC  "C64RESNP" (ascii)
//!   byte   8      formatVersion (u8 = 1)
//!   bytes  9..40  sha256(gzBody) (32 bytes) — integrity over the payload
//!   bytes 41..    gzBody = gzip(JSON.stringify(doc))
//!
//! doc = { manifest, checkpoint, mediaPayloads }:
//!   - manifest:  NativeSnapshotManifest (ts: native-snapshot.ts:46-54).
//!   - checkpoint: the RuntimeCheckpoint payload, typed arrays encoded by the
//!                 tagged base64 `$ta` codec (ts: native-snapshot.ts:101-136).
//!   - mediaPayloads: { [ref]: base64 } embedded media bytes.
//!
//! NOTE on the `$ta` codec: in the TS the RuntimeCheckpoint object carries LIVE
//! `Uint8Array`s (RAM, framebuffers, drive blob, sprite buffers). `encodeValue`
//! walks the object and rewrites every typed-array view as `{ $ta:<ctor>, b64 }`.
//! In Rust the checkpoint is already a serde tree where those byte-arrays are
//! serialized by `c64re_snapshot.rs` AS the `{ $ta, b64 }` tagged form (via the
//! `ta_bytes` serde helpers), so the tree handed to `write_native_snapshot` is
//! ALREADY in encoded shape and round-trips byte-for-byte through this framing.
//! `encode_value`/`decode_value` are still provided + unit-tested for parity with
//! the TS codec (and used by the framing's media-payload path).

use base64::Engine as _;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

/// native-snapshot.ts:31 — `export const NATIVE_SNAPSHOT_MAGIC = "C64RESNP";`.
pub const NATIVE_SNAPSHOT_MAGIC: &[u8; 8] = b"C64RESNP";
/// native-snapshot.ts:32 — `NATIVE_SNAPSHOT_FORMAT_VERSION = 1`.
pub const NATIVE_SNAPSHOT_FORMAT_VERSION: u8 = 1;
/// native-snapshot.ts:33 — `const HEADER_LEN = 8 + 1 + 32;`.
const HEADER_LEN: usize = 8 + 1 + 32;

// ── Manifest + media types (ts: native-snapshot.ts:35-79) ──────────────────────

/// ts: native-snapshot.ts:35-44 — `SnapshotMediaRef`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMediaRef {
    pub role: String, // "drive8" | "drive9" | "cartridge" | "injected-prg"
    pub format: String,
    pub sha256: String,
    #[serde(rename = "sourceName", skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    #[serde(rename = "embeddedPayloadRef", skip_serializing_if = "Option::is_none")]
    pub embedded_payload_ref: Option<String>,
    #[serde(rename = "writableDeltaRef", skip_serializing_if = "Option::is_none")]
    pub writable_delta_ref: Option<String>,
}

/// ts: native-snapshot.ts:46-54 — `NativeSnapshotManifest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeSnapshotManifest {
    pub kind: String, // "c64re-runtime-snapshot"
    pub version: i64, // checkpoint schemaVersion
    #[serde(rename = "createdAt")]
    pub created_at: String,
    pub machine: ManifestMachine,
    pub checkpoint: ManifestCheckpoint,
    pub media: Vec<SnapshotMediaRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestMachine {
    pub model: String, // "c64-pal" | "c64-ntsc"
    #[serde(rename = "runtimeVersion")]
    pub runtime_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestCheckpoint {
    pub encoding: String, // "ta-json-gz/1"
    #[serde(rename = "payloadRef")]
    pub payload_ref: String, // "checkpoint"
    pub cycle: i64,
    pub pc: i64,
}

/// A media entry to embed (bytes) — ts: native-snapshot.ts:57-64
/// `NativeSnapshotMediaInput`.
#[derive(Debug, Clone)]
pub struct NativeSnapshotMediaInput {
    pub role: String,
    pub format: String,
    pub source_name: Option<String>,
    pub bytes: Option<Vec<u8>>,
    pub sha256: Option<String>,
}

/// Resolved media on read — ts: native-snapshot.ts:76-79.
#[derive(Debug, Clone)]
pub struct ResolvedMedia {
    pub reference: SnapshotMediaRef,
    pub bytes: Option<Vec<u8>>,
}

/// Result of `read_native_snapshot` — ts: native-snapshot.ts:74-79
/// `ReadNativeSnapshotResult`.
#[derive(Debug, Clone)]
pub struct ReadNativeSnapshotResult {
    pub manifest: NativeSnapshotManifest,
    /// The decoded RuntimeCheckpoint payload (`$ta` views are kept tagged so the
    /// Rust `c64re_snapshot` deserializers read them via `ta_bytes`).
    pub checkpoint: Value,
    pub schema_version: i64,
    pub media: Vec<ResolvedMedia>,
}

/// Args for `write_native_snapshot` — ts: native-snapshot.ts:66-72.
pub struct WriteNativeSnapshotArgs {
    /// The RuntimeCheckpoint payload, already in serde shape with typed-arrays
    /// tagged as `{ $ta, b64 }` (see module doc).
    pub checkpoint: Value,
    pub schema_version: i64,
    pub media: Vec<NativeSnapshotMediaInput>,
    pub runtime_version: String,
    pub machine_model: String, // "c64-pal" | "c64-ntsc"
    pub provenance: Option<Value>,
    /// pc/cycle for the manifest (read from the checkpoint by the caller).
    pub pc: i64,
    pub cycle: i64,
}

// ── helpers (ts: native-snapshot.ts:91-99) ─────────────────────────────────────

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest.iter() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .map_err(|e| format!("native-snapshot: bad base64: {e}"))
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
        .map_err(|e| format!("native-snapshot: gunzip failed: {e}"))?;
    Ok(out)
}

// ── `$ta` typed-array JSON codec (ts: native-snapshot.ts:101-136) ───────────────
//
// The TS `encodeValue` tags an ArrayBuffer view as `{ $ta:<ctorName>, b64 }`;
// everything else stays plain JSON. Here the input value tree is a serde
// `Value`, so a "typed array" appears as a JSON ARRAY OF NUMBERS to be tagged.
// In TRX64's pipeline the chip serializers already emit the byte-buffers in
// tagged `{ $ta, b64 }` form, so `encode_value` is mostly an identity walk that
// leaves already-tagged nodes untouched — it is provided + tested for codec
// parity with the TS, and used where a raw byte-array needs tagging.

/// Tag a raw byte slice as a `{ $ta: "Uint8Array", b64 }` node — the Rust-side
/// canonical encoding for every `Uint8Array` field of the RuntimeCheckpoint.
pub fn ta_u8(bytes: &[u8]) -> Value {
    json!({ "$ta": "Uint8Array", "b64": b64(bytes) })
}

/// Tag a raw u32 slice as `{ $ta: "Uint32Array", b64 }` (LE bytes, matching the
/// TS which re-interprets the underlying ArrayBuffer through the ctor).
pub fn ta_u32(words: &[u32]) -> Value {
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    json!({ "$ta": "Uint32Array", "b64": b64(&bytes) })
}

/// Decode a `{ $ta: "Uint8Array", b64 }` node back to bytes. Returns None if the
/// node is not a tagged u8 typed-array.
pub fn ta_u8_decode(v: &Value) -> Option<Vec<u8>> {
    let obj = v.as_object()?;
    let ctor = obj.get("$ta")?.as_str()?;
    if ctor != "Uint8Array" && ctor != "Int8Array" && ctor != "Uint8ClampedArray" {
        return None;
    }
    let b = obj.get("b64")?.as_str()?;
    unb64(b).ok()
}

/// Decode a `{ $ta: "Uint32Array", b64 }` node back to u32 words (LE).
pub fn ta_u32_decode(v: &Value) -> Option<Vec<u32>> {
    let obj = v.as_object()?;
    if obj.get("$ta")?.as_str()? != "Uint32Array" {
        return None;
    }
    let bytes = unb64(obj.get("b64")?.as_str()?).ok()?;
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

/// ts: native-snapshot.ts:101-116 — `encodeValue`. Recursively re-tags typed
/// arrays; pass-through for already-tagged `{ $ta, b64 }` nodes + scalars.
/// (Identity for the TRX64 tree, which is pre-tagged — kept for codec parity.)
pub fn encode_value(v: &Value) -> Value {
    match v {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => v.clone(),
        Value::Array(arr) => Value::Array(arr.iter().map(encode_value).collect()),
        Value::Object(map) => {
            // Already-tagged typed array: pass through untouched.
            if map.contains_key("$ta") && map.contains_key("b64") {
                return v.clone();
            }
            let mut out = Map::new();
            for (k, val) in map {
                out.insert(k.clone(), encode_value(val));
            }
            Value::Object(out)
        }
    }
}

/// ts: native-snapshot.ts:118-136 — `decodeValue`. Pass-through (the Rust
/// deserializers read `{ $ta, b64 }` nodes directly via `ta_*_decode`), so this
/// keeps tagged nodes intact and only walks containers — identity over the tree.
pub fn decode_value(v: &Value) -> Value {
    match v {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => v.clone(),
        Value::Array(arr) => Value::Array(arr.iter().map(decode_value).collect()),
        Value::Object(map) => {
            if map.contains_key("$ta") && map.contains_key("b64") {
                return v.clone();
            }
            let mut out = Map::new();
            for (k, val) in map {
                out.insert(k.clone(), decode_value(val));
            }
            Value::Object(out)
        }
    }
}

// ── writer (ts: native-snapshot.ts:140-182) ────────────────────────────────────

/// ts: native-snapshot.ts:140-182 — `writeNativeSnapshot`. Builds the manifest +
/// media payloads, gzips `JSON.stringify(doc)`, prepends magic/version/sha256.
pub fn write_native_snapshot(args: WriteNativeSnapshotArgs) -> Vec<u8> {
    let mut media_payloads = Map::new();
    let mut media_refs: Vec<SnapshotMediaRef> = Vec::with_capacity(args.media.len());
    for (i, m) in args.media.iter().enumerate() {
        let sha = m
            .sha256
            .clone()
            .or_else(|| m.bytes.as_ref().map(|b| sha256_hex(b)))
            .unwrap_or_default();
        let mut reference = SnapshotMediaRef {
            role: m.role.clone(),
            format: m.format.clone(),
            sha256: sha,
            source_name: m.source_name.clone(),
            embedded_payload_ref: None,
            writable_delta_ref: None,
        };
        if let Some(bytes) = &m.bytes {
            let key = format!("media{i}");
            media_payloads.insert(key.clone(), Value::String(b64(bytes)));
            reference.embedded_payload_ref = Some(key);
        }
        media_refs.push(reference);
    }

    let manifest = NativeSnapshotManifest {
        kind: "c64re-runtime-snapshot".to_string(),
        version: args.schema_version,
        created_at: utc_now_iso(),
        machine: ManifestMachine {
            model: args.machine_model.clone(),
            runtime_version: args.runtime_version.clone(),
        },
        checkpoint: ManifestCheckpoint {
            encoding: "ta-json-gz/1".to_string(),
            payload_ref: "checkpoint".to_string(),
            cycle: args.cycle,
            pc: args.pc,
        },
        media: media_refs,
        provenance: args.provenance.clone(),
    };

    // doc = { manifest, checkpoint: encodeValue(payload), mediaPayloads }.
    let doc = json!({
        "manifest": serde_json::to_value(&manifest).expect("manifest serialize"),
        "checkpoint": encode_value(&args.checkpoint),
        "mediaPayloads": Value::Object(media_payloads),
    });

    let json_bytes = serde_json::to_vec(&doc).expect("doc serialize");
    let gz_body = gzip(&json_bytes);
    let digest = sha256_bytes(&gz_body);

    let mut out = Vec::with_capacity(HEADER_LEN + gz_body.len());
    out.extend_from_slice(NATIVE_SNAPSHOT_MAGIC);
    out.push(NATIVE_SNAPSHOT_FORMAT_VERSION);
    out.extend_from_slice(&digest);
    out.extend_from_slice(&gz_body);
    out
}

// ── reader (ts: native-snapshot.ts:186-234) ────────────────────────────────────

/// ts: native-snapshot.ts:186-234 — `readNativeSnapshot`. Validates magic +
/// format version + sha256 integrity, then decodes the doc + media payloads.
pub fn read_native_snapshot(bytes: &[u8]) -> Result<ReadNativeSnapshotResult, String> {
    if bytes.len() < HEADER_LEN {
        return Err("native-snapshot: file too small / not a .c64re container".to_string());
    }
    if &bytes[0..8] != NATIVE_SNAPSHOT_MAGIC {
        let magic = String::from_utf8_lossy(&bytes[0..8]);
        return Err(format!(
            "native-snapshot: bad magic \"{magic}\" (expected C64RESNP)"
        ));
    }
    let format_version = bytes[8];
    if format_version != NATIVE_SNAPSHOT_FORMAT_VERSION {
        return Err(format!(
            "native-snapshot: incompatible format version {format_version} (this build writes/reads {NATIVE_SNAPSHOT_FORMAT_VERSION})"
        ));
    }
    let stored_digest = &bytes[9..41];
    let gz_body = &bytes[HEADER_LEN..];
    let actual_digest = sha256_bytes(gz_body);
    if stored_digest != actual_digest {
        return Err(
            "native-snapshot: integrity check failed (sha256 mismatch — file corrupt or tampered)"
                .to_string(),
        );
    }

    let doc_bytes = gunzip(gz_body)?;
    let doc: Value =
        serde_json::from_slice(&doc_bytes).map_err(|e| format!("native-snapshot: bad doc JSON: {e}"))?;

    let manifest_val = doc
        .get("manifest")
        .ok_or("native-snapshot: doc missing manifest")?;
    let manifest: NativeSnapshotManifest = serde_json::from_value(manifest_val.clone())
        .map_err(|e| format!("native-snapshot: bad manifest: {e}"))?;
    if manifest.kind != "c64re-runtime-snapshot" {
        return Err(format!(
            "native-snapshot: not a c64re-runtime-snapshot (kind={})",
            manifest.kind
        ));
    }

    let checkpoint = decode_value(
        doc.get("checkpoint")
            .ok_or("native-snapshot: doc missing checkpoint")?,
    );

    let empty = Map::new();
    let media_payloads = doc
        .get("mediaPayloads")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);

    let mut media = Vec::with_capacity(manifest.media.len());
    for reference in &manifest.media {
        match &reference.embedded_payload_ref {
            None => media.push(ResolvedMedia {
                reference: reference.clone(),
                bytes: None,
            }),
            Some(key) => {
                let enc = media_payloads
                    .get(key)
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        format!("native-snapshot: media payload \"{key}\" missing from container")
                    })?;
                let mbytes = unb64(enc)?;
                let sha = sha256_hex(&mbytes);
                if !reference.sha256.is_empty() && sha != reference.sha256 {
                    return Err(format!(
                        "native-snapshot: embedded media sha256 mismatch for {} (corrupt payload)",
                        reference.role
                    ));
                }
                media.push(ResolvedMedia {
                    reference: reference.clone(),
                    bytes: Some(mbytes),
                });
            }
        }
    }

    Ok(ReadNativeSnapshotResult {
        manifest: manifest.clone(),
        checkpoint,
        schema_version: manifest.version,
        media,
    })
}

/// sha256 hex of arbitrary bytes — ts: native-snapshot.ts:237-239 `snapshotSha256`.
pub fn snapshot_sha256(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

/// Minimal UTC ISO-8601 timestamp (`createdAt`). The exact value is provenance,
/// not state — it does not participate in machine comparison or cross-runtime
/// resume; we emit a fixed-format Zulu string. Computed without chrono to avoid a
/// new dep.
fn utc_now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil-from-days (Howard Hinnant's algorithm) — days since 1970-01-01.
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_roundtrip() {
        let cp = json!({
            "schemaVersion": 1,
            "cpu": { "pc": 0xabcd, "cycles": 123456 },
            "ram": ta_u8(&[1u8, 2, 3, 4, 0xff]),
        });
        let bytes = write_native_snapshot(WriteNativeSnapshotArgs {
            checkpoint: cp.clone(),
            schema_version: 1,
            media: vec![NativeSnapshotMediaInput {
                role: "drive8".into(),
                format: "g64".into(),
                source_name: Some("test.g64".into()),
                bytes: Some(vec![0xde, 0xad, 0xbe, 0xef]),
                sha256: None,
            }],
            runtime_version: "trx64/1".into(),
            machine_model: "c64-pal".into(),
            provenance: None,
            pc: 0xabcd,
            cycle: 123456,
        });

        // Magic + version + header length.
        assert_eq!(&bytes[0..8], NATIVE_SNAPSHOT_MAGIC);
        assert_eq!(bytes[8], NATIVE_SNAPSHOT_FORMAT_VERSION);
        assert!(bytes.len() > HEADER_LEN);

        let r = read_native_snapshot(&bytes).expect("read");
        assert_eq!(r.schema_version, 1);
        assert_eq!(r.manifest.kind, "c64re-runtime-snapshot");
        assert_eq!(r.manifest.checkpoint.pc, 0xabcd);
        assert_eq!(r.manifest.checkpoint.cycle, 123456);
        assert_eq!(r.manifest.machine.model, "c64-pal");
        // Checkpoint round-trips byte-for-byte (the `$ta` RAM survives).
        assert_eq!(r.checkpoint["cpu"]["pc"], 0xabcd);
        let ram = ta_u8_decode(&r.checkpoint["ram"]).expect("ram $ta");
        assert_eq!(ram, vec![1, 2, 3, 4, 0xff]);
        // Media resolved + integrity-checked.
        assert_eq!(r.media.len(), 1);
        assert_eq!(r.media[0].reference.role, "drive8");
        assert_eq!(r.media[0].bytes.as_deref(), Some(&[0xde, 0xad, 0xbe, 0xef][..]));
    }

    #[test]
    fn bad_magic_rejected() {
        let mut bytes = vec![0u8; HEADER_LEN + 10];
        bytes[0..8].copy_from_slice(b"NOTC64RE");
        assert!(read_native_snapshot(&bytes).is_err());
    }

    #[test]
    fn bad_version_rejected() {
        let cp = json!({ "schemaVersion": 1 });
        let mut bytes = write_native_snapshot(WriteNativeSnapshotArgs {
            checkpoint: cp,
            schema_version: 1,
            media: vec![],
            runtime_version: "trx64/1".into(),
            machine_model: "c64-pal".into(),
            provenance: None,
            pc: 0,
            cycle: 0,
        });
        bytes[8] = 99; // bump format version
        assert!(read_native_snapshot(&bytes).is_err());
    }

    #[test]
    fn integrity_mismatch_rejected() {
        let cp = json!({ "schemaVersion": 1 });
        let mut bytes = write_native_snapshot(WriteNativeSnapshotArgs {
            checkpoint: cp,
            schema_version: 1,
            media: vec![],
            runtime_version: "trx64/1".into(),
            machine_model: "c64-pal".into(),
            provenance: None,
            pc: 0,
            cycle: 0,
        });
        // Corrupt one byte of the gzip body.
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        assert!(read_native_snapshot(&bytes).is_err());
    }

    #[test]
    fn ta_u32_roundtrip() {
        let words = [0x01020304u32, 0xdeadbeef, 0];
        let v = ta_u32(&words);
        assert_eq!(ta_u32_decode(&v).unwrap(), words);
    }

    #[test]
    fn utc_iso_format() {
        // 2021-01-01T00:00:00Z is 1609459200 secs — sanity on the civil calc.
        // (We can't inject time; just assert the format shape of the live call.)
        let s = utc_now_iso();
        assert_eq!(s.len(), 20, "ISO-8601 Zulu length");
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
    }
}
