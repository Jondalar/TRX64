//! anchor_codec.rs — Spec 766.2: generic compact-binary codec for a recorder
//! anchor payload.
//!
//! 1:1 PORT of the c64re TS
//!   C64ReverseEngineeringMCP/src/runtime/headless/recorder/anchor-codec.ts
//! (class `AnchorEncoder` + `decodeAnchor`).
//!
//! Why generic (TS comment, anchor-codec.ts:1-16): the anchor carries the
//! subsystems' OWN snapshot objects (a living, shape-changing graph). A hand-laid
//! parallel layout would need lockstep maintenance forever. This codec FOLLOWS the
//! actual value shape and emits a compact self-describing BINARY stream (tag +
//! value, NOT JSON, NOT gzip). Byte-exact round-trip (gated by probe-766-codec).
//!
//! TRX64 note: the c64re anchor value is a JS object graph of typed arrays. In
//! TRX64 the equivalent anchor is the RuntimeCheckpoint `serde_json::Value` tree
//! (ADR-077/078), where the native_snapshot codec tags typed arrays as
//! `{ "$ta": "Uint8Array", "b64": "…" }` (native_snapshot.rs). This codec walks a
//! `serde_json::Value` and emits the SAME tag/value byte stream the TS emits — a
//! `$ta` node maps to `T_TYPED` (Uint8Array, ctor id 0), a JSON object/array/
//! string/number/bool/null map to the matching TS tags. The byte layout (tag
//! ordering, little-endian u32 lengths, key-then-value object order, utf-8 keys)
//! is identical to the TS, so a round-trip is byte-exact within TRX64 and the
//! stream is wire-compatible with the c64re codec for the value shapes TRX64 emits.

use serde_json::{Map, Value};

// anchor-codec.ts:19-27 — value tags.
const T_NULL: u8 = 0;
const T_UNDEF: u8 = 1; // serde has no `undefined`; decode maps it to Null.
const T_FALSE: u8 = 2;
const T_TRUE: u8 = 3;
const T_DOUBLE: u8 = 4; // f64
const T_STRING: u8 = 5; // u32 byteLen + utf8
const T_ARRAY: u8 = 6; // u32 count + values
const T_OBJECT: u8 = 7; // u32 keyCount + (string key, value)*
const T_TYPED: u8 = 8; // u8 ctorId + u32 byteLen + raw bytes

// anchor-codec.ts:30-34 — typed-array constructors we round-trip (id → ctor).
// TRX64's anchor typed arrays are all Uint8Array (native_snapshot `$ta`), id 0.
const TYPED_ID_U8: u8 = 0;

/// anchor-codec.ts:48 — Encoder with a reused, growable scratch buffer. In Rust
/// the growth is `Vec` reallocation; the encoded byte stream is identical to the
/// TS. NOT thread-shared: one per producer.
pub struct AnchorEncoder {
    buf: Vec<u8>,
}

impl AnchorEncoder {
    /// anchor-codec.ts:53 — `new AnchorEncoder(initialBytes = 1<<17)`. The initial
    /// capacity is a perf hint only (Rust `Vec` grows transparently); the encoded
    /// bytes do not depend on it.
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(1 << 17),
        }
    }

    /// anchor-codec.ts:60-64 — `encode(value)`: encode into the scratch and return
    /// the encoded bytes. (Rust returns an owned `Vec<u8>` slice — the ring copies
    /// it out, same as the TS `subarray` the caller copies.)
    pub fn encode(&mut self, value: &Value) -> &[u8] {
        self.buf.clear();
        write_value(&mut self.buf, value);
        &self.buf
    }

    /// anchor-codec.ts:71-76 — `encodeWithReserve(reserve, value)`: leave `reserve`
    /// bytes free at the front for a fixed record header the caller fills in place
    /// (writeAnchorHeader). Returns `[0, reserve + encodedLen)`.
    pub fn encode_with_reserve(&mut self, reserve: usize, value: &Value) -> &[u8] {
        self.buf.clear();
        self.buf.resize(reserve, 0);
        write_value(&mut self.buf, value);
        &self.buf
    }
}

impl Default for AnchorEncoder {
    fn default() -> Self {
        Self::new()
    }
}

// anchor-codec.ts:89-91 — primitive writers (little-endian, matching DataView).
fn u8(buf: &mut Vec<u8>, v: u8) {
    buf.push(v);
}
fn u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn f64(buf: &mut Vec<u8>, v: f64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// anchor-codec.ts:93-132 — `writeValue(v)`. Walks the value and emits the
/// self-describing tag+value stream. The TS branch order is preserved.
fn write_value(buf: &mut Vec<u8>, v: &Value) {
    // anchor-codec.ts:94 — null.
    if v.is_null() {
        u8(buf, T_NULL);
        return;
    }
    // anchor-codec.ts:97 — boolean.
    if let Some(b) = v.as_bool() {
        u8(buf, if b { T_TRUE } else { T_FALSE });
        return;
    }
    // anchor-codec.ts:98 — number (all JS numbers are f64).
    if let Some(n) = v.as_f64() {
        if v.is_number() {
            u8(buf, T_DOUBLE);
            f64(buf, n);
            return;
        }
    }
    // anchor-codec.ts:99-105 — string.
    if let Some(s) = v.as_str() {
        u8(buf, T_STRING);
        let bytes = s.as_bytes();
        u32(buf, bytes.len() as u32);
        buf.extend_from_slice(bytes);
        return;
    }
    // anchor-codec.ts:106-114 — typed array. In TRX64 a typed array is a
    // native_snapshot `{ "$ta": "Uint8Array", "b64": "…" }` node. Detect it BEFORE
    // the generic-object branch and emit T_TYPED (ctor id 0 = Uint8Array), matching
    // the TS which special-cases ArrayBuffer.isView before plain objects.
    if let Some(bytes) = ta_u8_decode_node(v) {
        u8(buf, T_TYPED);
        u8(buf, TYPED_ID_U8);
        u32(buf, bytes.len() as u32);
        buf.extend_from_slice(&bytes);
        return;
    }
    // anchor-codec.ts:115-119 — array.
    if let Some(a) = v.as_array() {
        u8(buf, T_ARRAY);
        u32(buf, a.len() as u32);
        for item in a {
            write_value(buf, item);
        }
        return;
    }
    // anchor-codec.ts:120-130 — object. serde_json's default `Map` is a BTreeMap
    // (sorted keys), so the encode order is deterministic; decode rebuilds the same
    // `Map`, making the codec round-trip byte-exact within TRX64 regardless of the
    // `preserve_order` feature. (The c64re stream uses JS insertion order; cross-
    // runtime byte-equality of the OBJECT layer is therefore not guaranteed, but the
    // recorder never relies on it — it decodes its own anchors back into the same
    // checkpoint tree restore consumes.)
    if let Some(o) = v.as_object() {
        u8(buf, T_OBJECT);
        u32(buf, o.len() as u32);
        for (k, val) in o {
            let kb = k.as_bytes();
            u32(buf, kb.len() as u32);
            buf.extend_from_slice(kb);
            write_value(buf, val);
        }
        return;
    }
    // anchor-codec.ts:131 — unreachable for the value shapes the recorder emits.
    panic!("anchor-codec: cannot encode value");
}

/// anchor-codec.ts:137-172 — `decodeAnchor(bytes)`. Decodes the binary stream back
/// to its value graph. A `T_TYPED` Uint8Array decodes to a native_snapshot `$ta`
/// node (so the round-trip is byte-exact against the TRX64 checkpoint tree).
/// Returns Err on a malformed stream (the TS throws).
pub fn decode_anchor(bytes: &[u8]) -> Result<Value, String> {
    let mut off = 0usize;
    let v = read_value(bytes, &mut off)?;
    Ok(v)
}

fn read_u8(bytes: &[u8], off: &mut usize) -> Result<u8, String> {
    let b = *bytes
        .get(*off)
        .ok_or_else(|| "anchor-codec: truncated (u8)".to_string())?;
    *off += 1;
    Ok(b)
}
fn read_u32(bytes: &[u8], off: &mut usize) -> Result<u32, String> {
    let end = *off + 4;
    let slice = bytes
        .get(*off..end)
        .ok_or_else(|| "anchor-codec: truncated (u32)".to_string())?;
    let v = u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]);
    *off = end;
    Ok(v)
}
fn read_f64(bytes: &[u8], off: &mut usize) -> Result<f64, String> {
    let end = *off + 8;
    let slice = bytes
        .get(*off..end)
        .ok_or_else(|| "anchor-codec: truncated (f64)".to_string())?;
    let mut a = [0u8; 8];
    a.copy_from_slice(slice);
    *off = end;
    Ok(f64::from_le_bytes(a))
}
fn read_str(bytes: &[u8], off: &mut usize) -> Result<String, String> {
    let n = read_u32(bytes, off)? as usize;
    let end = *off + n;
    let slice = bytes
        .get(*off..end)
        .ok_or_else(|| "anchor-codec: truncated (string)".to_string())?;
    let s = String::from_utf8(slice.to_vec()).map_err(|e| format!("anchor-codec: utf8 {e}"))?;
    *off = end;
    Ok(s)
}

fn read_value(bytes: &[u8], off: &mut usize) -> Result<Value, String> {
    let tag = read_u8(bytes, off)?;
    match tag {
        // anchor-codec.ts:148-149 — T_NULL / T_UNDEF both → JSON null in TRX64.
        T_NULL | T_UNDEF => Ok(Value::Null),
        T_FALSE => Ok(Value::Bool(false)),
        T_TRUE => Ok(Value::Bool(true)),
        T_DOUBLE => {
            let n = read_f64(bytes, off)?;
            // JS has no int/float distinction (all numbers are f64), but serde_json
            // does, and downstream readers (restore_runtime_checkpoint) call
            // `as_i64()` on fields like `schemaVersion`. Decode a WHOLE, in-range
            // f64 back to a JSON integer so those reads work — mirroring how the TS
            // value (a plain JS number) was originally an integer. Non-integral or
            // out-of-range values stay floats.
            if n.is_finite() && n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
                Ok(serde_json::json!(n as i64))
            } else {
                Ok(serde_json::json!(n))
            }
        }
        T_STRING => Ok(Value::String(read_str(bytes, off)?)),
        T_ARRAY => {
            let n = read_u32(bytes, off)? as usize;
            let mut a = Vec::with_capacity(n);
            for _ in 0..n {
                a.push(read_value(bytes, off)?);
            }
            Ok(Value::Array(a))
        }
        T_OBJECT => {
            let n = read_u32(bytes, off)? as usize;
            let mut o = Map::new();
            for _ in 0..n {
                let k = read_str(bytes, off)?;
                let val = read_value(bytes, off)?;
                o.insert(k, val);
            }
            Ok(Value::Object(o))
        }
        T_TYPED => {
            let id = read_u8(bytes, off)?;
            let n = read_u32(bytes, off)? as usize;
            let end = *off + n;
            let raw = bytes
                .get(*off..end)
                .ok_or_else(|| "anchor-codec: truncated (typed)".to_string())?
                .to_vec();
            *off = end;
            if id != TYPED_ID_U8 {
                // TRX64 only emits Uint8Array typed arrays; a foreign id from a
                // c64re stream of another typed array is decoded as a Uint8Array
                // of the raw bytes (the native_snapshot codec is u8-only).
                return Err(format!("anchor-codec: unsupported typed-array id {id}"));
            }
            Ok(crate::native_snapshot::ta_u8(&raw))
        }
        other => Err(format!("anchor-codec: bad tag {other} at off {}", *off - 1)),
    }
}

/// Decode a native_snapshot `{ "$ta": "Uint8Array", "b64": "…" }` node to its
/// bytes, or None if `v` is not such a node. Thin wrapper over the native_snapshot
/// codec so the codec layer here stays self-contained.
fn ta_u8_decode_node(v: &Value) -> Option<Vec<u8>> {
    if v.get("$ta").is_some() {
        crate::native_snapshot::ta_u8_decode(v)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Round-trip a scalar/object/array graph. Whole numbers decode back to JSON
    /// integers (JS-number semantics — all numbers are f64, but a whole value reads
    /// as an int so downstream `as_i64()` works); fractional values stay floats.
    #[test]
    fn roundtrip_scalars_and_containers() {
        let mut enc = AnchorEncoder::new();
        // Use integer literals so the round-trip is exact-equal (whole f64 → i64).
        let v = json!({
            "a": 1,
            "b": true,
            "c": false,
            "d": Value::Null,
            "e": "hello",
            "f": [1, 2, 3],
            "g": { "nested": "x" },
        });
        let bytes = enc.encode(&v).to_vec();
        let back = decode_anchor(&bytes).unwrap();
        assert_eq!(back, v);
    }

    /// A fractional number stays a float through the round-trip.
    #[test]
    fn roundtrip_fractional_number_stays_float() {
        let mut enc = AnchorEncoder::new();
        let v = json!({ "wallMs": 1234.5 });
        let bytes = enc.encode(&v).to_vec();
        let back = decode_anchor(&bytes).unwrap();
        assert_eq!(back["wallMs"].as_f64(), Some(1234.5));
        // A whole number reads back as an integer (as_i64 works).
        let v2 = json!({ "schemaVersion": 1 });
        let b2 = enc.encode(&v2).to_vec();
        let back2 = decode_anchor(&b2).unwrap();
        assert_eq!(back2["schemaVersion"].as_i64(), Some(1));
    }

    /// A `$ta` Uint8Array node round-trips as T_TYPED.
    #[test]
    fn roundtrip_typed_array() {
        let mut enc = AnchorEncoder::new();
        let blob = crate::native_snapshot::ta_u8(&[0u8, 1, 2, 0xff, 0x80]);
        let v = json!({ "ram": blob });
        let bytes = enc.encode(&v).to_vec();
        let back = decode_anchor(&bytes).unwrap();
        // The decoded $ta node carries the same bytes.
        let got = crate::native_snapshot::ta_u8_decode(&back["ram"]).unwrap();
        assert_eq!(got, vec![0u8, 1, 2, 0xff, 0x80]);
    }

    /// encode_with_reserve leaves the header gap, codec body follows it.
    #[test]
    fn reserve_leaves_header_gap() {
        let mut enc = AnchorEncoder::new();
        let v = json!(42);
        let with = enc.encode_with_reserve(28, &v).to_vec();
        let plain = {
            let mut e2 = AnchorEncoder::new();
            e2.encode(&v).to_vec()
        };
        assert_eq!(with.len(), 28 + plain.len());
        assert_eq!(&with[28..], &plain[..]);
        // The codec body after the reserve decodes back to the value (whole → int).
        assert_eq!(decode_anchor(&with[28..]).unwrap(), v);
    }

    /// Empty utf-8 keys + multibyte keys round-trip (length-prefixed bytes).
    #[test]
    fn multibyte_keys_roundtrip() {
        let mut enc = AnchorEncoder::new();
        let v = json!({ "kÿ": "vül", "": "empty-key" });
        let bytes = enc.encode(&v).to_vec();
        assert_eq!(decode_anchor(&bytes).unwrap(), v);
    }

    /// A truncated stream errors instead of panicking on the decode path.
    #[test]
    fn truncated_stream_errors() {
        // T_STRING tag + a length that overruns the buffer.
        let bad = vec![T_STRING, 0xff, 0xff, 0x00, 0x00, b'h'];
        assert!(decode_anchor(&bad).is_err());
    }
}
