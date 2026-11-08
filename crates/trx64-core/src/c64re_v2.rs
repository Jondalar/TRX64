//! c64re_v2.rs — `.c64re` format 2 (Spec 919): the writer, the reader, the board's file in TRX64.
//!
//! All little-endian. The 41-byte head is format 1's. Behind it:
//!
//! ```text
//!   u32 header length | header JSON (ASCII) | pad to 4
//!   u16 section count | u16 0
//!   section table, 8 bytes each: u8 id, u8 flags, u16 0, u32 length
//!   the payloads in table order, each padded to 4 with zeros
//! ```
//!
//! Sections 1-7 are the board's (`U8-c64re-v2-state-and-medium.md`): BLOCK, RAM, COLOR,
//! CARTRAM, CARTROM, DRIVE, EEPROM. Section 128 is TRX64's: a gzipped `{ manifest, checkpoint,
//! mediaPayloads, trx64 }` document with everything sections 2-7 already hold taken out, so a
//! TRX64 file restores exactly from it and the board skips it by length.
//!
//! A file without section 128 was written by the board. It is read into a fresh machine's
//! checkpoint with the file's values written in, and the result carries the report of what
//! came in and what did not (D12).

use serde_json::{json, Value};

use crate::c64re_block::{self as blk, Block, BLOCK_BYTES, BLOCK_WORDS};
use crate::c64re_cart as cartx;
use crate::native_snapshot::{
    build_doc, check_digest, gunzip, gzip, now_iso, read_doc, sha256_bytes, sha256_hex, ta_u8,
    ta_u8_decode, ReadNativeSnapshotResult, ResolvedMedia, SnapshotMediaRef,
    WriteNativeSnapshotArgs, NATIVE_SNAPSHOT_FORMAT_VERSION, NATIVE_SNAPSHOT_MAGIC,
};
use crate::vsf::{Fidelity, VsfLoadReport};

pub const SEC_BLOCK: u8 = 1;
pub const SEC_RAM: u8 = 2;
pub const SEC_COLOR: u8 = 3;
pub const SEC_CARTRAM: u8 = 4;
pub const SEC_CARTROM: u8 = 5;
pub const SEC_DRIVE: u8 = 6;
pub const SEC_EEPROM: u8 = 7;
/// TRX64's own section: the rest of the checkpoint, gzipped.
pub const SEC_TRX64: u8 = 128;

const HEAD: usize = 41;
/// `sizeof(TrxDriveState)` on the board.
pub const DRIVE_BYTES: usize = 2092;

fn pad4(n: usize) -> usize {
    (4 - (n & 3)) & 3
}

pub fn sec_name(id: u8) -> String {
    match id {
        SEC_BLOCK => "BLOCK".into(),
        SEC_RAM => "RAM".into(),
        SEC_COLOR => "COLOR".into(),
        SEC_CARTRAM => "CARTRAM".into(),
        SEC_CARTROM => "CARTROM".into(),
        SEC_DRIVE => "DRIVE".into(),
        SEC_EEPROM => "EEPROM".into(),
        SEC_TRX64 => "TRX64".into(),
        n => format!("id {n}"),
    }
}

// ── the container ───────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Section {
    pub id: u8,
    pub flags: u8,
    /// Offset of the payload in the file.
    pub off: usize,
    pub len: usize,
}

#[derive(Debug, Clone)]
pub struct Layout {
    pub header: Value,
    pub sections: Vec<Section>,
}

impl Layout {
    fn find(&self, id: u8) -> Option<&Section> {
        self.sections.iter().find(|s| s.id == id)
    }
}

/// The container of a format 2 file, checked: digest, header, section table, every length and
/// the end of the file. Refuses by name. An id this reader does not know is carried in the
/// layout and skipped by whoever reads sections.
pub fn layout(b: &[u8]) -> Result<Layout, String> {
    if b.len() < HEAD {
        return Err("native-snapshot: file too small / not a .c64re container".into());
    }
    if &b[..8] != NATIVE_SNAPSHOT_MAGIC {
        return Err("native-snapshot: not a .c64re (the magic is missing)".into());
    }
    if b[8] != NATIVE_SNAPSHOT_FORMAT_VERSION {
        return Err(format!("native-snapshot: format {} is not format 2", b[8]));
    }
    check_digest(b)?;
    let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as usize;
    let mut o = HEAD;
    if o + 4 > b.len() {
        return Err("native-snapshot: truncated before the header length".into());
    }
    let hl = u32at(o);
    o += 4;
    if o.checked_add(hl).map_or(true, |e| e > b.len()) {
        return Err("native-snapshot: the header runs past the end of the file".into());
    }
    let header: Value = serde_json::from_slice(&b[o..o + hl])
        .map_err(|e| format!("native-snapshot: the header is not JSON: {e}"))?;
    o += hl + pad4(hl);
    if o + 4 > b.len() {
        return Err("native-snapshot: truncated before the section table".into());
    }
    let count = u16::from_le_bytes([b[o], b[o + 1]]) as usize;
    o += 4;
    if o + count * 8 > b.len() {
        return Err(format!("native-snapshot: the section table ({count} entries) runs past the end of the file"));
    }
    let mut table = Vec::with_capacity(count);
    for i in 0..count {
        let e = o + i * 8;
        table.push((b[e], b[e + 1], u32at(e + 4)));
    }
    o += count * 8;
    let mut sections = Vec::with_capacity(count);
    for (id, flags, len) in table {
        if o.checked_add(len).map_or(true, |e| e > b.len()) {
            return Err(format!("native-snapshot: section {} ({len} bytes) runs past the end of the file", sec_name(id)));
        }
        sections.push(Section { id, flags, off: o, len });
        o += len;
        if o + pad4(len) > b.len() || b[o..o + pad4(len)].iter().any(|&x| x != 0) {
            return Err(format!("native-snapshot: the padding after section {} is missing or not zero", sec_name(id)));
        }
        o += pad4(len);
    }
    if o != b.len() {
        return Err(format!("native-snapshot: {} bytes after the last section", b.len() - o));
    }
    // the lengths the format pins down, and the block layout the words are laid out by
    let bw = header.get("blockWords").and_then(|v| v.as_u64());
    if bw != Some(BLOCK_WORDS as u64) {
        return Err(format!(
            "native-snapshot: the file's chip block (blockWords {}) is not this reader's ({BLOCK_WORDS})",
            bw.map_or("absent".to_string(), |n| n.to_string())
        ));
    }
    let want = |id: u8| match id {
        SEC_BLOCK => Some(BLOCK_BYTES),
        SEC_RAM | SEC_CARTRAM => Some(65536),
        SEC_COLOR => Some(1024),
        SEC_EEPROM => Some(cartx::EEPROM_BYTES),
        _ => None,
    };
    for s in &sections {
        if let Some(w) = want(s.id) {
            if s.len != w {
                return Err(format!("native-snapshot: section {} is {} bytes, it should be {w}", sec_name(s.id), s.len));
            }
        }
    }
    Ok(Layout { header, sections })
}

// ── JSON by hand: the header keeps the board's key order and is ASCII ───────────────────────

fn jstr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7E => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", u));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn num(v: &Value, path: &[&str]) -> Option<i64> {
    let mut cur = v;
    for k in path {
        cur = cur.get(*k)?;
    }
    cur.as_i64()
}

// ── drive A for the board ───────────────────────────────────────────────────────────────────

/// Drive A as the board's `TrxDriveState` (2092 bytes): `regs`, a x y sp p, pc, VIA1 and VIA2 as
/// read, the cycle count and the 2 KiB of RAM. Out of the `drive1541` blob; `None` when drive A
/// is a 1581, is off, or has no state.
fn drive_section(cp: &Value) -> Option<Vec<u8>> {
    let blob = cp.get("drive1541").and_then(ta_u8_decode)?;
    let part = cp.get("drivePart")?;
    if part.get("powered").and_then(|v| v.as_bool()) != Some(true) || num(part, &["boardType"]).unwrap_or(1541) != 1541 {
        return None;
    }
    let mut d = crate::drive::Drive1541::new();
    crate::drive_snapshot::restore_drive1541(&mut d, &blob).ok()?;
    let mut out = vec![0u8; DRIVE_BYTES];
    out[0] = 1;
    out[1] = d.core.reg_a;
    out[2] = d.core.reg_x;
    out[3] = d.core.reg_y;
    out[4] = d.core.reg_sp;
    out[5] = d.core.status();
    out[6..8].copy_from_slice(&d.core.reg_pc.to_le_bytes());
    d.snapshot_via1(|ctx, _| {
        for i in 0..16 {
            out[8 + i] = crate::viacore::viacore_peek_no_hooks(ctx, i as u16);
        }
    });
    d.snapshot_via2(|ctx, _| {
        for i in 0..16 {
            out[24 + i] = crate::viacore::viacore_peek_no_hooks(ctx, i as u16);
        }
    });
    out[40..44].copy_from_slice(&(d.core.clk as u32).to_le_bytes());
    out[44..44 + 0x800].copy_from_slice(&d.snapshot_ram()[..]);
    Some(out)
}

// ── the writer ──────────────────────────────────────────────────────────────────────────────

/// What the board would drop without refusing (D6), named under `trx64.notOnBoard`.
fn not_on_board(cp: &Value) -> Vec<String> {
    let mut v = Vec::new();
    if cp.get("driveB").is_some_and(|d| !d.is_null()) {
        v.push("drive B".to_string());
    }
    let a1581 = num(cp.get("drivePart").unwrap_or(&Value::Null), &["boardType"]) == Some(1581);
    let b1581 = num(cp.get("driveB").and_then(|d| d.get("drivePart")).unwrap_or(&Value::Null), &["boardType"]) == Some(1581);
    if a1581 || b1581 {
        v.push("1581".to_string());
    }
    if let Some(e) = cp.get("expansion").filter(|e| !e.is_null()) {
        let kind = e.get("kind").and_then(|k| k.as_str()).unwrap_or("expansion");
        let kb = num(e, &["sizeKb"]).unwrap_or(0);
        v.push(format!("{} {kb} KB", kind.to_uppercase()));
    }
    if cp.get("sid").and_then(|s| s.get("chips")).and_then(|c| c.as_array()).is_some_and(|c| !c.is_empty()) {
        v.push("extra SID chips".to_string());
    }
    if cp.get("folders").is_some_and(|f| !f.is_null()) {
        v.push("folder devices".to_string());
    }
    if cp.get("iecDevices").is_some_and(|f| !f.is_null()) {
        v.push("host IEC devices".to_string());
    }
    if cp.get("hostFdc").is_some_and(|f| !f.is_null()) {
        v.push("host disk controllers".to_string());
    }
    v
}

fn header_json(
    args: &WriteNativeSnapshotArgs,
    cp: &Value,
    plan: Option<&cartx::CartPlan>,
    created: &str,
) -> String {
    let frame = num(cp, &["vicPresentation", "litStableFrameCount"]).unwrap_or(0).max(0);
    let cycles = num(cp, &["cpu", "cycles"]).unwrap_or(args.cycle).max(0);
    let pc = num(cp, &["cpu", "pc"]).unwrap_or(args.pc) & 0xFFFF;
    let source = args
        .provenance
        .as_ref()
        .and_then(|p| p.get("cartSource"))
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty());
    let mut h = format!(
        "{{\"kind\":\"c64re\",\"version\":{},\"writer\":\"trx64\",\"appVersion\":{},\"machine\":{},\
         \"bitstreamId\":0,\"caps\":0,\"caps2\":0,\"blockWords\":{},\"createdAt\":{},\"frame\":{frame},\
         \"cycles\":{cycles},\"pc\":{pc}",
        NATIVE_SNAPSHOT_FORMAT_VERSION,
        jstr(env!("CARGO_PKG_VERSION")),
        jstr(&args.machine_model),
        BLOCK_WORDS,
        jstr(created),
    );
    if let Some(p) = plan {
        h.push_str(&format!(",\"cartridgeType\":{}", p.fw));
        if let Some(s) = source {
            h.push_str(&format!(",\"source\":{}", jstr(s)));
        }
        h.push_str(&format!(",\"mediumBytes\":{}", p.flat.len()));
    }
    h.push_str(",\"machineState\":{");
    if let Some(p) = plan {
        h.push_str(&format!(
            "\"cartridge\":{{\"type\":{},\"bytes\":{},\"require\":{},\"prohibit\":{}",
            p.fw,
            p.flat.len(),
            p.require,
            p.prohibit
        ));
        if let Some(s) = source {
            h.push_str(&format!(",\"source\":{}", jstr(s)));
        }
        h.push_str("},");
    }
    if let Some(img) = cp.get("media").and_then(|m| m.get("diskPath")).and_then(|p| p.as_str()).filter(|p| !p.is_empty()) {
        h.push_str(&format!("\"drive\":{{\"image\":{},\"mode\":\"rw\"}},", jstr(img)));
    }
    let reu = cp.get("expansion").filter(|e| e.get("kind").and_then(|k| k.as_str()) == Some("reu"));
    let reu_code = reu
        .and_then(|e| num(e, &["sizeKb"]))
        .map(|kb| (kb.max(128) as u64 / 128).trailing_zeros().min(7))
        .unwrap_or(0);
    h.push_str(&format!(
        "\"settings\":{{\"uci\":false,\"reu\":{},\"reuSize\":{},\"sampler\":false}}}}",
        reu.is_some() as u8,
        reu_code
    ));
    let nob: Vec<String> = not_on_board(cp).iter().map(|s| jstr(s)).collect();
    let zero: Vec<String> = blk::ZEROED.iter().map(|s| jstr(s)).collect();
    h.push_str(&format!(
        ",\"trx64\":{{\"format\":1,\"notOnBoard\":[{}],\"blockZeroed\":[{}]}}}}",
        nob.join(","),
        zero.join(",")
    ));
    h
}

/// Writes a format 2 file. Total, like format 1's writer was: a checkpoint that does not
/// project (no 64 KiB RAM, no cpu or vic node) keeps whatever it cannot put in a section in the
/// TRX64 section, so nothing is lost and the sections that could be built are still there.
pub fn write(args: &WriteNativeSnapshotArgs) -> Vec<u8> {
    let created = now_iso();
    let mut cp = args.checkpoint.clone();
    let mut secs: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut stripped: Vec<&str> = Vec::new();

    let plan = cartx::plan_for_write(&cp);
    let regs = plan.as_ref().map(|p| p.regs);
    let header_cp = cp.clone();
    if let Ok(b) = blk::block_from_checkpoint(&cp, regs) {
        secs.push((SEC_BLOCK, b.to_bytes()));
    }
    if let Some(ram) = cp.get("ram").and_then(ta_u8_decode).filter(|r| r.len() == 65536) {
        secs.push((SEC_RAM, ram));
        cp.as_object_mut().map(|o| o.remove("ram"));
        stripped.push("ram");
    }
    let color: Option<Vec<u8>> = cp
        .get("vic")
        .and_then(|v| v.get("color_ram"))
        .and_then(|c| c.as_array())
        .filter(|c| c.len() == 1024)
        .map(|c| c.iter().map(|x| x.as_u64().unwrap_or(0) as u8 & 0x0F).collect());
    if let Some(col) = color {
        secs.push((SEC_COLOR, col));
        if let Some(v) = cp.get_mut("vic").and_then(|v| v.as_object_mut()) {
            v.remove("color_ram");
        }
        stripped.push("vic.color_ram");
    }
    let mut cart_node = None;
    if let Some(p) = &plan {
        let mut cram = vec![0u8; cartx::CART_RAM_BYTES];
        if let Some(io) = &p.io_ram {
            cram[cartx::IO2_RAM_AT..cartx::IO2_RAM_AT + cartx::IO2_RAM_BYTES].copy_from_slice(io);
        }
        secs.push((SEC_CARTRAM, cram));
        secs.push((SEC_CARTROM, p.flat.clone()));
        if let Some(e) = &p.eeprom {
            secs.push((SEC_EEPROM, e.clone()));
        }
        cart_node = Some(p.node());
        cartx::strip(&mut cp, p);
    }
    if let Some(d) = drive_section(&header_cp) {
        secs.push((SEC_DRIVE, d));
    }

    let mut doc = build_doc(args, &cp, &created);
    let mut t = json!({ "format": 1, "stripped": stripped });
    if let Some(n) = cart_node {
        t["cart"] = n;
    }
    doc["trx64"] = t;
    let gz = gzip(&serde_json::to_vec(&doc).expect("doc serialize"));
    secs.push((SEC_TRX64, gz));
    secs.sort_by_key(|s| s.0);

    let header = header_json(args, &header_cp, plan.as_ref(), &created);
    let mut body = Vec::new();
    body.extend_from_slice(&(header.len() as u32).to_le_bytes());
    body.extend_from_slice(header.as_bytes());
    body.resize(body.len() + pad4(header.len()), 0);
    body.extend_from_slice(&(secs.len() as u16).to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    for (id, data) in &secs {
        body.push(*id);
        body.push(0);
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    }
    for (_, data) in &secs {
        body.extend_from_slice(data);
        body.resize(body.len() + pad4(data.len()), 0);
    }
    let mut out = Vec::with_capacity(HEAD + body.len());
    out.extend_from_slice(NATIVE_SNAPSHOT_MAGIC);
    out.push(NATIVE_SNAPSHOT_FORMAT_VERSION);
    out.extend_from_slice(&sha256_bytes(&body));
    out.extend_from_slice(&body);
    out
}

// ── the reader ──────────────────────────────────────────────────────────────────────────────

pub fn read(bytes: &[u8], base_dir: Option<&std::path::Path>) -> Result<ReadNativeSnapshotResult, String> {
    let lay = layout(bytes)?;
    let sec = |id: u8| lay.find(id).map(|s| &bytes[s.off..s.off + s.len]);
    match sec(SEC_TRX64) {
        Some(gz) => read_trx64(&lay, bytes, gz),
        None => read_board(&lay, bytes, base_dir),
    }
}

/// A file TRX64 wrote: the document, with what sections 2-7 hold put back.
fn read_trx64(lay: &Layout, bytes: &[u8], gz: &[u8]) -> Result<ReadNativeSnapshotResult, String> {
    let sec = |id: u8| lay.find(id).map(|s| &bytes[s.off..s.off + s.len]);
    let raw = gunzip(gz).map_err(|e| format!("native-snapshot: section TRX64: {e}"))?;
    let mut doc: Value = serde_json::from_slice(&raw).map_err(|e| format!("native-snapshot: section TRX64 is not JSON: {e}"))?;
    let t = doc.get("trx64").cloned().unwrap_or(Value::Null);
    let mut cp = doc.get("checkpoint").cloned().ok_or("native-snapshot: section TRX64 has no checkpoint")?;
    let stripped: Vec<String> = t
        .get("stripped")
        .and_then(|s| s.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let need = |id: u8, what: &str| sec(id).ok_or_else(|| format!("native-snapshot: the TRX64 section needs section {} ({what}), which is not in the file", sec_name(id)));
    if stripped.iter().any(|s| s == "ram") {
        cp["ram"] = ta_u8(need(SEC_RAM, "RAM")?);
    }
    if stripped.iter().any(|s| s == "vic.color_ram") {
        let col = need(SEC_COLOR, "colour RAM")?;
        cp["vic"]["color_ram"] = json!(col.iter().map(|&b| b as i64).collect::<Vec<_>>());
    }
    if let Some(node) = t.get("cart") {
        let flat = need(SEC_CARTROM, "the cartridge")?;
        cartx::restore(&mut cp, node, flat, sec(SEC_CARTRAM), sec(SEC_EEPROM))
            .map_err(|e| format!("native-snapshot: {e}"))?;
    }
    doc["checkpoint"] = cp;
    read_doc(&doc)
}

// ── the board's file ────────────────────────────────────────────────────────────────────────

fn model_for(name: &str) -> Result<&'static crate::model::C64Model, String> {
    if crate::model::find(name).is_some() {
        return crate::model::resolve(name);
    }
    match name.split_once('-') {
        Some((profile, rest)) if crate::vic::SpeedProfile::parse(profile).is_some() => {
            let row = format!("c64-{rest}");
            if crate::model::find(&row).is_none() && crate::model::find(rest).is_some() {
                crate::model::resolve(rest)
            } else {
                crate::model::resolve(&row)
            }
        }
        _ => crate::model::resolve(name),
    }
}

fn image_format(path: &str) -> String {
    path.rsplit('.').next().map(|e| e.to_ascii_lowercase()).filter(|e| e.len() <= 4).unwrap_or_default()
}

fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

fn set(v: &mut Value, path: &[&str], x: Value) {
    let mut cur = v;
    for k in &path[..path.len() - 1] {
        cur = &mut cur[*k];
    }
    cur[path[path.len() - 1]] = x;
}

/// A file named by the board's path: as it stands, and then by name beside the file loaded.
fn find_file(named: &str, base_dir: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(named);
    if p.is_file() {
        return Some(p.to_path_buf());
    }
    let beside = base_dir?.join(basename(named));
    beside.is_file().then_some(beside)
}

/// Drive A out of section 6 as a `drive1541` blob: the 2 KiB RAM, the 6502 registers, and VIA1
/// and VIA2 written back the way the board's own load writes them (bus writes: the counters
/// reloaded from what was read, the latches, the control registers, the interrupt enables).
/// Built on a scratch drive without a disk, so the writes rotate nothing; the machine mounts
/// the image first and restores this over it. What a VIA2 `PRB` write does to the head is put
/// back: the file does not know the track.
fn drive_blob_from_section(sec: &[u8]) -> Result<(Vec<u8>, bool), String> {
    use crate::viacore::{viacore_store, ViaBackend, ViaContext};
    if sec.len() != DRIVE_BYTES {
        return Err(format!("section DRIVE is {} bytes, not {DRIVE_BYTES}", sec.len()));
    }
    let mut d = crate::drive::Drive1541::new();
    let cycles = u32::from_le_bytes([sec[40], sec[41], sec[42], sec[43]]) as u64;
    // the VIA's store works with the cycle before the current one, which a drive at cycle 0 has not got
    d.core.clk = cycles.max(100);
    d.snapshot_sync_drive_clk();
    d.snapshot_ram_mut().copy_from_slice(&sec[44..44 + 0x800]);
    d.snapshot_clear_pending_reset();
    let half_track = d.rotation.current_half_track;
    if sec[0] != 0 {
        d.core.reg_a = sec[1];
        d.core.reg_x = sec[2];
        d.core.reg_y = sec[3];
        d.core.reg_sp = sec[4];
        d.core.set_status_composite(sec[5]);
        d.core.reg_pc = u16::from_le_bytes([sec[6], sec[7]]);
        fn load<B: ViaBackend>(ctx: &mut ViaContext, be: &mut B, v: &[u8]) {
            let mut w = |a: u16, b: u8| viacore_store(ctx, be, a, b);
            w(0xA, v[0xA]); // SR
            w(0xB, v[0xB]); // ACR
            w(0xC, v[0xC]); // PCR
            w(0x2, v[0x2]); // DDRB
            w(0x3, v[0x3]); // DDRA
            w(0x0, v[0x0]); // PRB
            w(0x1, v[0x1]); // PRA
            w(0x6, v[0x4]); // T1 latch low: the counter's low byte, then the counter loads
            w(0x7, v[0x5]);
            w(0x5, v[0x5]); // T1CH: the counter loads from the latch and runs
            w(0x6, v[0x6]); // the latches as they were
            w(0x7, v[0x7]);
            w(0x8, v[0x8]); // T2 latch low
            w(0x9, v[0x9]); // T2CH: the counter loads and runs
            w(0xE, 0x7F);
            w(0xE, 0x80 | (v[0xE] & 0x7F));
        }
        let (v1, v2) = (sec[8..24].to_vec(), sec[24..40].to_vec());
        d.snapshot_via1(|ctx, be| load(ctx, be, &v1));
        d.snapshot_via2(|ctx, be| load(ctx, be, &v2));
        d.rotation.set_half_track(half_track);
    }
    Ok((crate::drive_snapshot::capture_drive1541(&mut d), sec[0] != 0))
}

/// A file the board wrote, D5 and D12: a fresh machine's checkpoint with the file's values
/// written in, and the report of what came in.
fn read_board(lay: &Layout, bytes: &[u8], base_dir: Option<&std::path::Path>) -> Result<ReadNativeSnapshotResult, String> {
    let sec = |id: u8| lay.find(id).map(|s| &bytes[s.off..s.off + s.len]);
    let h = &lay.header;
    let machine = h.get("machine").and_then(|m| m.as_str()).unwrap_or("c64-pal").to_string();
    let model = model_for(&machine).map_err(|e| format!("native-snapshot: the file is for machine \"{machine}\": {e}"))?;
    let b = Block::from_bytes(sec(SEC_BLOCK).ok_or("native-snapshot: the file has no BLOCK section: no machine state")?)?;
    let ram = sec(SEC_RAM).ok_or("native-snapshot: the file has no RAM section: no machine state")?;
    let color = sec(SEC_COLOR).ok_or("native-snapshot: the file has no COLOR section: no machine state")?;
    let cycles = h.get("cycles").and_then(|v| v.as_u64()).unwrap_or(0);

    let mut loaded: Vec<String> = Vec::new();
    let mut coarse: Vec<String> = Vec::new();
    let mut absent: Vec<String> = Vec::new();

    let mut m = crate::Machine::new();
    if !std::ptr::eq(m.model(), model) {
        m.put_on_model(model).map_err(|e| format!("native-snapshot: {e}"))?;
    }
    if let Some((p, _)) = machine.split_once('-') {
        if let Some(prof) = crate::vic::SpeedProfile::parse(p).filter(|p| *p != crate::vic::SpeedProfile::C64) {
            m.vic.speed_profile = prof;
        }
    }
    let n = blk::nodes_from_block(&b, cycles);
    // the interrupt lines the block's chips were asserting, through the machine's own setters
    {
        use crate::c64_6510core::{INT_SRC_CIA1, INT_SRC_CIA2, INT_SRC_VIC};
        m.c64_int.set_irq(INT_SRC_CIA1, n.irq_cia1, cycles);
        m.c64_int.set_nmi(INT_SRC_CIA2, n.nmi_cia2, cycles);
        m.c64_int.set_irq(INT_SRC_VIC, n.irq_vic, cycles);
    }
    // a snap is on line 0 and the stop is at pix_x[9:3] == the cycle: pix_x is the VIC's raster_x
    // and cycle_num is raster_x[9:3], 0 based like TRX64's raster_cycle, so x >> 3 is the cycle
    let (line, cycle) = (0i64, (b.x >> 3) as i64);
    let mut cp = crate::c64re_snapshot::capture_runtime_checkpoint(&m, "", "", None, None, None, None);
    // cpu
    for (k, v) in n.cpu.as_object().into_iter().flatten() {
        cp["cpu"][k.as_str()] = v.clone();
    }
    cp["ram"] = ta_u8(ram);
    if let Some((dir, val)) = n.port {
        cp["cpuPortDirection"] = json!(dir);
        cp["cpuPortValue"] = json!(val);
    }
    cp["cia1"] = n.cia1.clone();
    cp["cia2"] = n.cia2.clone();
    // vic: the registers, the raster position, the colour RAM
    for (i, r) in n.vic_regs.iter().enumerate() {
        cp["vic"]["regs"][i] = json!(*r);
    }
    let regs = &n.vic_regs;
    set(&mut cp, &["vic", "raster_line"], json!(line));
    set(&mut cp, &["vic", "raster_cycle"], json!(cycle));
    set(&mut cp, &["vic", "irq_status"], json!(n.vic_irq_status));
    set(&mut cp, &["vic", "raster_irq_line"], json!(regs[0x12] as i64 | ((regs[0x11] as i64 & 0x80) << 1)));
    set(&mut cp, &["vic", "ysmooth"], json!(regs[0x11] & 7));
    set(&mut cp, &["vic", "sprite_sprite_collisions"], json!(n.sprite_sprite));
    set(&mut cp, &["vic", "sprite_background_collisions"], json!(n.sprite_background));
    set(&mut cp, &["vic", "color_ram"], json!(color.iter().map(|&c| (c & 0x0F) as i64).collect::<Vec<_>>()));
    set(&mut cp, &["vicPresentation", "litLastRasterLine"], json!(line));
    loaded.extend(["MAINCPU", "C64MEM", "COLOR RAM"].map(String::from));
    coarse.extend(
        [
            "CIA1 (registers, timers, TOD, interrupt flags; no IFR delay line, no serial shifter)",
            "CIA2 (registers, timers, TOD, interrupt flags; no IFR delay line, no serial shifter)",
            "VIC-II registers",
            "VIC-II raster position (line 0, the cycle from x: a snap is on line 0)",
            "CPU interrupt state (from the chips' interrupt flags)",
        ]
        .map(String::from),
    );

    // turbo
    match b.turbo {
        Some(pack) if machine.starts_with("u64") => {
            cp["turbo"] = blk::turbo_node(pack);
            coarse.push("turbo (reset hold to 256 cycles)".to_string());
        }
        Some(_) => absent.push("turbo (the board's speed setting; this machine runs at 1 MHz)".to_string()),
        None => {}
    }

    // the cartridge
    let ms = h.get("machineState");
    let fw = ms
        .and_then(|s| s.get("cartridge"))
        .and_then(|c| c.get("type"))
        .and_then(|t| t.as_u64())
        .or_else(|| h.get("cartridgeType").and_then(|t| t.as_u64()));
    match (fw, sec(SEC_CARTROM)) {
        (Some(fw), Some(flat)) => {
            let source = ms
                .and_then(|s| s.get("cartridge"))
                .and_then(|c| c.get("source"))
                .or_else(|| h.get("source"))
                .and_then(|s| s.as_str())
                .and_then(|p| find_file(p, base_dir))
                .and_then(|p| std::fs::read(p).ok())
                .filter(|d| crate::cart::is_crt(d));
            let regs = blk::board_cart_regs(&b).unwrap_or((0, 0));
            match cartx::from_board(fw as u8, flat, sec(SEC_EEPROM), sec(SEC_CARTRAM), regs, source) {
                Ok(c) => {
                    cp["cartBytes"] = ta_u8(&c.crt);
                    cp["cartFlash"] = c.flash.as_deref().map(ta_u8).unwrap_or(Value::Null);
                    cp["cartState"] = c.state;
                    loaded.push(format!("C64CART ({}, {} bytes)", cartx::fw_name(c.mapper), flat.len()));
                    loaded.push("C64CART banking registers".to_string());
                    for note in c.notes {
                        loaded.push(format!("note: {note}"));
                    }
                }
                Err(e) => absent.push(format!("C64CART (type {fw:02x}): {e}")),
            }
        }
        (Some(fw), None) => absent.push(format!("C64CART (type {fw:02x}): the file has no CARTROM section")),
        (None, _) => {}
    }

    // the disk, and then drive A's state over it (U9: the image first, the state after)
    let mut media: Vec<ResolvedMedia> = Vec::new();
    let drive = ms.and_then(|s| s.get("drive"));
    let image = drive.and_then(|d| d.get("image")).and_then(|i| i.as_str()).filter(|i| !i.is_empty());
    let mut disk_missing = false;
    if let Some(img) = image {
        match find_file(img, base_dir).and_then(|p| std::fs::read(&p).ok().map(|d| (p, d))) {
            Some((found, data)) => {
                let format = image_format(img);
                let found_s = found.to_string_lossy().to_string();
                cp["media"] = json!({ "diskPath": found_s, "imageFormat": format });
                let ro = drive.and_then(|d| d.get("mode")).and_then(|m| m.as_str()) == Some("ro");
                media.push(ResolvedMedia {
                    reference: SnapshotMediaRef {
                        role: "drive8".into(),
                        format,
                        sha256: sha256_hex(&data),
                        source_name: Some(basename(img)),
                        embedded_payload_ref: None,
                        writable_delta_ref: None,
                    },
                    bytes: Some(data),
                    read_only: ro,
                });
                loaded.push(format!("DRIVE8 image {found_s}{}", if ro { " (read-only, as the file says)" } else { "" }));
            }
            None => {
                disk_missing = true;
                absent.push(format!(
                    "DRIVE8 image {img}: looked for it as named and as {} beside the file, not found; the drive is left empty",
                    basename(img)
                ));
            }
        }
    }
    match sec(SEC_DRIVE) {
        Some(d) if !disk_missing => match drive_blob_from_section(d) {
            Ok((blob, regs)) => {
                cp["drive1541"] = ta_u8(&blob);
                coarse.push(if regs {
                    "DRIVE8 RAM, 6502 registers, VIA1 and VIA2 (counters reloaded from the read values, no interrupt flags; motor and speed zone from VIA2 PB)".to_string()
                } else {
                    "DRIVE8 RAM (the board could not read the registers of its drive)".to_string()
                });
                absent.push("DRIVE8 mechanics (head position (half-track), GCR byte position, rotation phase: the file does not hold them, the head starts on track 18)".to_string());
            }
            Err(e) => absent.push(format!("DRIVE8 state (section 6): {e}")),
        },
        Some(_) => absent.push("DRIVE8 state (section 6) not loaded: its image was not found".to_string()),
        None => absent.push("DRIVE8 (the file holds no drive)".to_string()),
    }
    absent.push("SID (starts silent until the program writes it)".to_string());
    absent.push("VIC-II internals (sprite DMA, bad lines, pixel pipeline, write-only latches)".to_string());
    absent.push("IEC bus lines (start released)".to_string());

    let created = h.get("createdAt").and_then(|c| c.as_str()).map(String::from).unwrap_or_else(now_iso);
    let writer = h.get("writer").and_then(|w| w.as_str()).unwrap_or("unknown");
    let app = h.get("appVersion").and_then(|w| w.as_str()).unwrap_or("?");
    let pc = n.cpu["pc"].as_i64().unwrap_or(0);
    let manifest = json!({
        "kind": "c64re-runtime-snapshot",
        "version": crate::c64re_snapshot::RUNTIME_CHECKPOINT_SCHEMA_VERSION,
        "createdAt": created,
        "machine": { "model": machine, "runtimeVersion": format!("{writer}/{app}") },
        "checkpoint": { "encoding": "c64re-v2-sections", "payloadRef": "sections", "cycle": cycles, "pc": pc },
        "media": [],
    });
    let man: crate::native_snapshot::NativeSnapshotManifest =
        serde_json::from_value(manifest).map_err(|e| format!("native-snapshot: {e}"))?;
    Ok(ReadNativeSnapshotResult {
        schema_version: man.version,
        manifest: man,
        checkpoint: cp,
        media,
        report: Some(VsfLoadReport { loaded, coarse, absent, fidelity: Fidelity::Partial, source: "c64re-v2" }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section() -> Vec<u8> {
        let mut s = vec![0u8; DRIVE_BYTES];
        s[0] = 1;
        s[1..6].copy_from_slice(&[0x11, 0x22, 0x33, 0xEE, 0x24]); // a x y sp p
        s[6..8].copy_from_slice(&0xEBE7u16.to_le_bytes());
        // VIA1 as read: DDRB $1A, ACR $40 (T1 free-run), PCR $0E, T1 counter $1000, latch $4E20, IER $C2
        s[8 + 2] = 0x1A;
        s[8 + 0xB] = 0x40;
        s[8 + 0xC] = 0x0E;
        s[8 + 4] = 0x00;
        s[8 + 5] = 0x10;
        s[8 + 6] = 0x20;
        s[8 + 7] = 0x4E;
        s[8 + 0xE] = 0xC2;
        // VIA2: motor on (PB2), zone 1 (PB5), DDRB $EF
        s[24 + 2] = 0xEF;
        s[24] = 0x04 | 0x20;
        s[40..44].copy_from_slice(&5_000_000u32.to_le_bytes());
        for (i, b) in s[44..].iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(3);
        }
        s
    }

    #[test]
    fn drive_a_comes_back_from_section_6() {
        let (blob, regs) = drive_blob_from_section(&section()).expect("blob");
        assert!(regs);
        let mut d = crate::drive::Drive1541::new();
        crate::drive_snapshot::restore_drive1541(&mut d, &blob).expect("restore");
        assert_eq!((d.core.reg_a, d.core.reg_x, d.core.reg_y, d.core.reg_sp, d.core.reg_pc), (0x11, 0x22, 0x33, 0xEE, 0xEBE7));
        assert_eq!(d.core.status(), 0x24);
        assert_eq!(d.ram()[0x123], (0x123u32 as u8).wrapping_mul(3));
        assert_eq!(d.core.clk, 5_000_000);
        let mut via1 = [0u8; 16];
        d.snapshot_via1(|ctx, _| {
            for i in 0..16 {
                via1[i] = crate::viacore::viacore_peek_no_hooks(ctx, i as u16);
            }
        });
        assert_eq!(via1[2], 0x1A, "DDRB");
        assert_eq!(via1[0xB], 0x40, "ACR");
        assert_eq!((via1[6], via1[7]), (0x20, 0x4E), "T1 latch");
        assert_eq!(via1[0xE], 0xC2, "IER as read");
        let t1 = via1[4] as i32 | (via1[5] as i32) << 8;
        assert!((0x0FF0..=0x1000).contains(&t1), "T1 counter reloaded from what was read: {t1:#x}");
        assert!(d.rotation.byte_ready_active & crate::rotation::BRA_MOTOR_ON != 0, "motor on from VIA2 PB");
        assert_eq!(d.rotation.current_half_track, 36, "the file does not know the track");
    }

    #[test]
    fn an_all_zero_drive_section_loads() {
        let mut s = vec![0u8; DRIVE_BYTES];
        s[0] = 1;
        drive_blob_from_section(&s).expect("blob");
    }

    #[test]
    fn a_short_section_is_refused_by_name() {
        assert!(drive_blob_from_section(&[0u8; 100]).unwrap_err().contains("DRIVE"));
    }
}
