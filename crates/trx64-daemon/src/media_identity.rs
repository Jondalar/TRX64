//! Spec 909 — which image is in each media slot, by its sha256.
//!
//! A consumer that files what a trace saw under the medium it ran on needs the
//! medium's identity, not its file name: two cartridges can share a name, and a
//! renamed file is the same image. Three places carry it:
//!
//! * `session/state` — `media.cart` / `media.disk` and each `drives[]` entry;
//! * a `.c64retrace` header — `media` (versioned), the image of every slot as
//!   mounted; and the older top-level `mediaSha` / `mediaName`, drive 8 as the drive
//!   has written it (the run descriptor's `media`, which the readers already knew —
//!   the two agree until the drive writes);
//! * inside a trace — a MARK record (0x01) whenever a medium goes in or out while
//!   it records, and one when the machine is switched off (which ends the trace).
//!
//! The MARK label grammar, one line, space-separated (the name is last and may
//! hold spaces; a label is cut at 200 bytes):
//!
//! ```text
//! media/1 <slot> in  sha256=<hex> format=<crt|d64|g64|d81> [type=<n>] [mapper=<m>] [banks=<n>] [name=<file>]
//! media/1 <slot> out sha256=<hex>
//! media/1 power off
//! ```
//!
//! `<slot>` is `cart` or `drive<unit>`. A swap is an `out` and an `in` at one cycle.
//!
//! The hash of a cartridge is over the `.crt` bytes as mounted (the image keeps them
//! unchanged; flash writes go elsewhere). The hash of a disk is over its bytes when
//! this module first saw it in the drive: the mount paths tell it the file's hash,
//! so for those it is the file as mounted, however much the drive writes later.

use std::sync::Mutex;

use serde_json::{json, Map, Value};
use trx64_core::Machine;
use trx64_session::{MediaSeen, Session};

/// Which image: its byte buffer's address and length, and its host path. Survives the
/// drive's own writes (they land in the same buffer); a new mount is a new buffer.
pub type MediaKey = (usize, usize, Option<String>);

/// One medium in one slot.
pub struct MediaIdentity {
    pub slot: String,
    pub key: MediaKey,
    pub sha256: String,
    /// The host file's name; empty when the image came without one.
    pub source_name: String,
    pub format: &'static str,
    pub cart: Option<CartHeader>,
}

/// What a `.crt` header says about itself.
pub struct CartHeader {
    /// The CRT hardware type (header $16, big-endian); `None` for an image that is not
    /// a CRT file (a raw ROM the loader accepted).
    pub hw_type: Option<u16>,
    pub mapper: &'static str,
    /// Number of banks the image carries.
    pub banks: usize,
    /// The name field of the header ($20..$3F), empty when blank.
    pub crt_name: String,
}

/// The hashes already taken, newest last. Small: a session holds a few media at once.
static SHA_CACHE: Mutex<Vec<(MediaKey, String)>> = Mutex::new(Vec::new());
const SHA_CACHE_MAX: usize = 16;

fn cached_sha(key: &MediaKey, bytes: &[u8]) -> String {
    let mut cache = SHA_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, sha)) = cache.iter().find(|(k, _)| k == key) {
        return sha.clone();
    }
    let sha = super::sha256_hex(bytes);
    remember(&mut cache, key.clone(), sha.clone());
    sha
}

fn remember(cache: &mut Vec<(MediaKey, String)>, key: MediaKey, sha: String) {
    cache.retain(|(k, _)| *k != key);
    cache.push((key, sha));
    if cache.len() > SHA_CACHE_MAX {
        cache.remove(0);
    }
}

/// A mount path that hashed the file it read records that hash for the image it
/// put into the drive, so the identity is the file as mounted.
pub fn note_mounted_disk(m: &Machine, unit: u8, sha256: &str) {
    if let Some(id) = identities_of(m, "").into_iter().find(|i| i.slot == format!("drive{unit}")) {
        let mut cache = SHA_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        remember(&mut cache, id.key, sha256.to_string());
    }
}

fn basename(p: &str) -> String {
    p.rsplit('/').next().unwrap_or("").to_string()
}

/// Every medium in the session's machine: the cartridge, then each drive with a disk.
pub fn identities(session: &Session) -> Vec<MediaIdentity> {
    identities_of(&session.machine, &session.cart_path)
}

fn identities_of(m: &Machine, cart_path: &str) -> Vec<MediaIdentity> {
    let mut out = Vec::new();
    if let Some(img) = m.cartridge_image.as_ref() {
        let raw = &img.raw_bytes;
        let path = (!cart_path.is_empty()).then(|| cart_path.to_string());
        let key: MediaKey = (raw.as_ptr() as usize, raw.len(), path);
        let is_crt = raw.len() >= 0x40 && raw.starts_with(b"C64 CARTRIDGE");
        let hw_type = is_crt.then(|| u16::from_be_bytes([raw[0x16], raw[0x17]]));
        let crt_name = if is_crt {
            let f = &raw[0x20..0x40];
            let end = f.iter().position(|&b| b == 0).unwrap_or(f.len());
            String::from_utf8_lossy(&f[..end]).trim().to_string()
        } else {
            String::new()
        };
        out.push(MediaIdentity {
            slot: "cart".into(),
            sha256: cached_sha(&key, raw),
            source_name: if cart_path.is_empty() { img.name.clone() } else { basename(cart_path) },
            key,
            format: "crt",
            cart: Some(CartHeader {
                hw_type,
                mapper: trx64_monitor::verbs::mapper_type_str(img.mapper_type),
                banks: img.banks.len(),
                crt_name,
            }),
        });
    }
    out.extend([&m.drive8, &m.drive_b].into_iter().filter_map(disk_identity));
    out
}

/// The disk in `drive`, `None` when it is empty.
pub fn disk_identity(drive: &trx64_core::drive::Drive1541) -> Option<MediaIdentity> {
    let disk = drive.get_attached_disk()?;
    let key: MediaKey = (disk.bytes.as_ptr() as usize, disk.bytes.len(), disk.backing_path.clone());
    Some(MediaIdentity {
        slot: format!("drive{}", drive.unit()),
        sha256: cached_sha(&key, &disk.bytes),
        source_name: disk.backing_path.as_deref().map(basename).unwrap_or_default(),
        key,
        format: disk.kind.name(),
        cart: None,
    })
}

impl MediaIdentity {
    /// The JSON shape used in `session/state` and in the trace header.
    pub fn to_json(&self) -> Value {
        let mut o = Map::new();
        o.insert("sha256".into(), json!(self.sha256));
        o.insert("sourceName".into(), json!(self.source_name));
        o.insert("format".into(), json!(self.format));
        if let Some(c) = &self.cart {
            o.insert("hwType".into(), json!(c.hw_type));
            o.insert("mapperType".into(), json!(c.mapper));
            o.insert("banks".into(), json!(c.banks));
            o.insert("crtName".into(), json!(c.crt_name));
        }
        Value::Object(o)
    }

    fn seen(&self) -> MediaSeen {
        MediaSeen { slot: self.slot.clone(), key: self.key.clone(), sha256: self.sha256.clone() }
    }

    /// The `in` record's label.
    fn in_label(&self) -> String {
        let mut s = format!("media/1 {} in sha256={} format={}", self.slot, self.sha256, self.format);
        if let Some(c) = &self.cart {
            if let Some(t) = c.hw_type {
                s.push_str(&format!(" type={t}"));
            }
            s.push_str(&format!(" mapper={} banks={}", c.mapper, c.banks));
        }
        if !self.source_name.is_empty() {
            s.push_str(&format!(" name={}", self.source_name));
        }
        s
    }
}

fn out_label(seen: &MediaSeen) -> String {
    format!("media/1 {} out sha256={}", seen.slot, seen.sha256)
}

/// The header's `media` object (version 1), and the slots as the trace starts.
///
/// ```json
/// "media": { "v": 1,
///            "cart":   { "sha256", "sourceName", "format", "hwType", "mapperType", "banks", "crtName" },
///            "drives": { "8": { "sha256", "sourceName", "format" }, "9": … } }
/// ```
/// `cart` is absent with no cartridge in, `drives` holds only drives with a disk.
pub fn header_media(session: &Session) -> (Value, Vec<MediaSeen>) {
    let ids = identities(session);
    let mut media = json!({ "v": 1, "drives": {} });
    for id in &ids {
        match id.slot.strip_prefix("drive") {
            Some(unit) => media["drives"][unit] = id.to_json(),
            None => media["cart"] = id.to_json(),
        }
    }
    (media, ids.iter().map(MediaIdentity::seen).collect())
}

/// Add the media identity to a trace header's meta JSON: `media`, and the older
/// `mediaSha` / `mediaName` — drive 8 as the drive has written it, the run descriptor's
/// `media` (empty without a disk: left out).
pub fn stamp_meta(meta_json: &str, session: &Session, media_sha: &str, media_name: &str) -> (String, Vec<MediaSeen>) {
    let (media, seen) = header_media(session);
    let mut meta: Value = serde_json::from_str(meta_json).unwrap_or_else(|_| json!({}));
    if !media_sha.is_empty() {
        meta["mediaSha"] = json!(media_sha);
        meta["mediaName"] = json!(media_name);
    }
    meta["media"] = media;
    (serde_json::to_string(&meta).unwrap_or_else(|_| meta_json.to_string()), seen)
}

/// The labels for what changed since `before`, and the slots now. Outs first, then ins,
/// each in slot order, so a swap reads `out` then `in`.
pub fn changes(session: &Session, before: &[MediaSeen]) -> Option<(Vec<String>, Vec<MediaSeen>)> {
    let ids = identities(session);
    let now: Vec<MediaSeen> = ids.iter().map(MediaIdentity::seen).collect();
    if now == before {
        return None;
    }
    let mut labels = Vec::new();
    for b in before {
        if !now.iter().any(|n| n.slot == b.slot && n.key == b.key) {
            labels.push(out_label(b));
        }
    }
    for id in &ids {
        if !before.iter().any(|b| b.slot == id.slot && b.key == id.key) {
            labels.push(id.in_label());
        }
    }
    Some((labels, now))
}

/// The record written when the machine is switched off with a trace running: the
/// trace ends there, with what was in the machine still named by the records before.
pub const POWER_OFF_LABEL: &str = "media/1 power off";

