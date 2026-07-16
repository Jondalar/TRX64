//! checkpoint_diff.rs — Spec 794 whitebox component-diff.
//!
//! Diffs two RuntimeCheckpoint JSON trees (the value
//! `c64re_snapshot::capture_runtime_checkpoint` builds = a `.c64re` file's
//! `checkpoint` node) at COMPONENT granularity and produces an equivalence
//! VERDICT for the whitebox dev-sandbox (yardstick #2).
//!
//! **Full coverage by construction:** every JSON leaf of the checkpoint is walked
//! and classified to a component (+ an optional volatile `lane`). Nothing is
//! silently dropped — an unclassified path lands in an `other:*` component and is
//! still reported. `{ $ta }` typed-array nodes (RAM, framebuffers) and plain
//! numeric arrays (color RAM, VIC buffers) are decoded and diffed element-wise
//! with contiguous runs collapsed into ranges (like Spec 246 `diff_ram`).
//!
//! **Substrate (Spec 794):** this replaces the lossy VSF-module diff (246) for the
//! whitebox path — the checkpoint carries color RAM, drive state and full internal
//! chip state that `save_vsf` never emits. 246 `snapshot_diff.rs` stays for the
//! legacy monitor/VSF path.
//!
//! The drive (`drive1541`) and disk (`driveDiskImage`) ride as opaque base64 blobs
//! in the checkpoint. `drive_blob_components` (below) decodes the drive blob's VICE
//! sub-modules so Floppy RAM (`drive.ram`) and the drive CPU/VIAs become addressable
//! components rather than one opaque "drive changed".

use serde_json::{json, Value};

use crate::native_snapshot::{ta_u32_decode, ta_u8_decode};

// ── canonical component scope ────────────────────────────────────────────────
// The components the verdict always REPORTS (identical or not), so `verdict.scope`
// is stable across diffs. Any `other:*` bucket that shows up is appended.
const CANON: &[&str] = &[
    "cpu",
    "cpu.int",
    "banking",
    "ram",
    "colorram",
    "cia1",
    "cia2",
    "sid",
    "vic",
    "vic.presentation",
    "iec",
    "cart",
    "cart.rom",
    "drive.cpu",
    "drive.ram",
    "drive.rotation",
    "drive.via1",
    "drive.via2",
    "drive.disk",
    "input.keyboard",
    "input",
];

// ── raw deltas ───────────────────────────────────────────────────────────────

#[derive(Clone)]
enum Delta {
    /// A scalar (or type-mismatched) leaf whose value differs.
    Scalar { path: String, before: Value, after: Value },
    /// A contiguous run of differing bytes/elements within a `$ta` node or a
    /// numeric array. `start`/`end` are element indices (inclusive).
    Range { path: String, start: usize, end: usize, count: usize, sample: Vec<Value> },
}

impl Delta {
    fn path(&self) -> &str {
        match self {
            Delta::Scalar { path, .. } => path,
            Delta::Range { path, .. } => path,
        }
    }
}

// ── exclusion mask ───────────────────────────────────────────────────────────

/// A caller-supplied exclusion mask (Spec 794). Removes chosen state from the
/// equivalence verdict — by whole component, by named volatile lane, or by an
/// address range within an addressable component (RAM / color RAM / Floppy RAM).
/// Everything it removes is echoed in `verdict.excluded` (the echo law).
#[derive(Default, Clone)]
pub struct ExcludeMask {
    pub components: Vec<String>,
    pub lanes: Vec<String>,
    pub ranges: Vec<RangeMask>,
}

/// An address window (inclusive) within one addressable component.
#[derive(Clone)]
pub struct RangeMask {
    pub component: String,
    pub from: usize,
    pub to: usize,
}

/// Map a caller `space` name to the addressable component it targets.
fn space_to_component(space: &str) -> Option<&'static str> {
    match space {
        "c64ram" => Some("ram"),
        "colorram" => Some("colorram"),
        "driveram" | "drivezp" => Some("drive.ram"),
        _ => None,
    }
}

fn parse_addr(v: Option<&Value>) -> usize {
    match v {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0) as usize,
        Some(Value::String(s)) => {
            let t = s.trim();
            if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix('$')) {
                usize::from_str_radix(h, 16).unwrap_or(0)
            } else {
                t.parse::<usize>().or_else(|_| usize::from_str_radix(t, 16)).unwrap_or(0)
            }
        }
        _ => 0,
    }
}

impl ExcludeMask {
    /// Parse the `exclude` object: `{ components:[], lanes:[], ranges:[{space,from,to}],
    /// presets:[] }`. Null / missing → an empty (strict) mask.
    pub fn from_json(v: &Value) -> Self {
        let mut m = ExcludeMask::default();
        if v.is_null() {
            return m;
        }
        if let Some(ps) = v.get("presets").and_then(|x| x.as_array()) {
            for p in ps.iter().filter_map(|x| x.as_str()) {
                m.apply_preset(p);
            }
        }
        if let Some(cs) = v.get("components").and_then(|x| x.as_array()) {
            for c in cs.iter().filter_map(|x| x.as_str()) {
                m.components.push(c.to_string());
            }
        }
        if let Some(ls) = v.get("lanes").and_then(|x| x.as_array()) {
            for l in ls.iter().filter_map(|x| x.as_str()) {
                if !m.lanes.iter().any(|x| x == l) {
                    m.lanes.push(l.to_string());
                }
            }
        }
        if let Some(rs) = v.get("ranges").and_then(|x| x.as_array()) {
            for r in rs {
                let space = r.get("space").and_then(|x| x.as_str()).unwrap_or("");
                if let Some(comp) = space_to_component(space) {
                    m.ranges.push(RangeMask {
                        component: comp.to_string(),
                        from: parse_addr(r.get("from")),
                        to: parse_addr(r.get("to")),
                    });
                }
            }
        }
        m
    }

    /// Expand a named preset into lanes/ranges. `equivalence` = the volatile lanes
    /// that always advance between two behaviourally-identical scratch runs.
    fn apply_preset(&mut self, p: &str) {
        if p == "equivalence" {
            for l in ["cycles", "raster", "sid_noise", "open_bus", "framebuffer"] {
                if !self.lanes.iter().any(|x| x == l) {
                    self.lanes.push(l.to_string());
                }
            }
        }
    }
}

/// Subtract the inclusive window `[from,to]` from `[s,e]`. Returns the residual
/// sub-ranges (0, 1 or 2) and whether any overlap was removed.
fn subtract(s: usize, e: usize, from: usize, to: usize) -> (Vec<(usize, usize)>, bool) {
    if to < s || from > e {
        return (vec![(s, e)], false);
    }
    let mut res = Vec::new();
    if from > s {
        res.push((s, from - 1));
    }
    if to < e {
        res.push((to + 1, e));
    }
    (res, true)
}

/// Apply the mask: drop excluded deltas (splitting partially-masked ranges) and
/// build the echo list. Every mask entry appears in the echo, annotated
/// `(matched 0)` when it removed nothing (echo law — never silent).
fn apply_mask(deltas: Vec<Delta>, mask: &ExcludeMask) -> (Vec<Delta>, Vec<String>) {
    let mut kept: Vec<Delta> = Vec::new();
    let mut comp_hit = vec![false; mask.components.len()];
    let mut lane_hit = vec![false; mask.lanes.len()];
    let mut range_hit = vec![false; mask.ranges.len()];

    for d in deltas {
        let path = d.path().to_string();
        let comp = component_of(&path);
        let lane = lane_of(&path);

        if let Some(i) = mask.components.iter().position(|c| *c == comp) {
            comp_hit[i] = true;
            continue;
        }
        if let Some(l) = lane {
            if let Some(i) = mask.lanes.iter().position(|x| x == l) {
                lane_hit[i] = true;
                continue;
            }
        }

        match d {
            Delta::Range { path: rp, start, end, sample, .. } => {
                let relevant: Vec<usize> = mask
                    .ranges
                    .iter()
                    .enumerate()
                    .filter(|(_, rm)| rm.component == comp)
                    .map(|(i, _)| i)
                    .collect();
                if relevant.is_empty() {
                    kept.push(Delta::Range { path: rp, start, end, count: end - start + 1, sample });
                    continue;
                }
                let mut segments = vec![(start, end)];
                for &ri in &relevant {
                    let rm = &mask.ranges[ri];
                    let mut next = Vec::new();
                    for (s, e) in segments.drain(..) {
                        let (residual, hit) = subtract(s, e, rm.from, rm.to);
                        if hit {
                            range_hit[ri] = true;
                        }
                        next.extend(residual);
                    }
                    segments = next;
                }
                for (s, e) in segments {
                    let sub: Vec<Value> = sample
                        .iter()
                        .filter(|sv| {
                            let idx = sv.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                            idx >= s && idx <= e
                        })
                        .cloned()
                        .collect();
                    kept.push(Delta::Range { path: rp.clone(), start: s, end: e, count: e - s + 1, sample: sub });
                }
            }
            other => kept.push(other),
        }
    }

    let mut excluded: Vec<String> = Vec::new();
    for (i, c) in mask.components.iter().enumerate() {
        excluded.push(format!("component:{c}{}", if comp_hit[i] { "" } else { " (matched 0)" }));
    }
    for (i, l) in mask.lanes.iter().enumerate() {
        excluded.push(format!("lane:{l}{}", if lane_hit[i] { "" } else { " (matched 0)" }));
    }
    for (i, r) in mask.ranges.iter().enumerate() {
        excluded.push(format!(
            "range:{} ${:04X}-${:04X}{}",
            r.component,
            r.from,
            r.to,
            if range_hit[i] { "" } else { " (matched 0)" }
        ));
    }
    (kept, excluded)
}

// ── public entry ─────────────────────────────────────────────────────────────

/// Diff two RuntimeCheckpoint JSON trees → the Spec 794 component-diff value:
/// `{ schema, fromCycle, toCycle, verdict{identical,differing,excluded,scope},
///    components{ <comp>: {identical, summary, changes[]} } }`.
///
/// `mask` removes chosen state from the verdict (whole components / volatile lanes /
/// address ranges incl. Floppy RAM), echoed in `verdict.excluded`. An empty mask
/// (`ExcludeMask::default()`) is STRICT = the Spec 792 byte-exact bar: any single
/// differing field/byte in any component (incl. color RAM, drive RAM, internal chip
/// state) flips `verdict.identical` and names the component.
pub fn diff_checkpoints(a: &Value, b: &Value, mask: &ExcludeMask) -> Value {
    let mut deltas: Vec<Delta> = Vec::new();
    walk(a, b, "", &mut deltas);

    // Decompose the opaque drive blob into addressable sub-module components so
    // Floppy RAM etc. are diffed at byte granularity, not "drive.blob changed".
    drive_blob_components(a.get("drive1541"), b.get("drive1541"), &mut deltas);

    let (kept, excluded) = apply_mask(deltas, mask);
    assemble(kept, excluded, a, b)
}

// ── recursive JSON walk ──────────────────────────────────────────────────────

fn walk(a: &Value, b: &Value, path: &str, out: &mut Vec<Delta>) {
    if a == b {
        return;
    }
    match (a, b) {
        (Value::Object(oa), Value::Object(ob)) => {
            // The drive blob (itself a `$ta` node) is decomposed into addressable
            // sub-module components by drive_blob_components — skip it here so it is
            // not ALSO diffed raw as one opaque byte range.
            if path == "drive1541" {
                return;
            }
            // A `{ $ta }` typed-array node → decode + byte-diff (RAM, framebuffers).
            if oa.contains_key("$ta") || ob.contains_key("$ta") {
                diff_ta(a, b, path, out);
                return;
            }
            let mut keys: Vec<&String> = oa.keys().chain(ob.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                let child = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                walk(
                    oa.get(k).unwrap_or(&Value::Null),
                    ob.get(k).unwrap_or(&Value::Null),
                    &child,
                    out,
                );
            }
        }
        (Value::Array(aa), Value::Array(bb)) => {
            if is_numeric_array(aa) && is_numeric_array(bb) {
                diff_numeric_seq(aa, bb, path, out);
            } else {
                let n = aa.len().max(bb.len());
                for i in 0..n {
                    let child = format!("{path}[{i}]");
                    walk(aa.get(i).unwrap_or(&Value::Null), bb.get(i).unwrap_or(&Value::Null), &child, out);
                }
            }
        }
        _ => out.push(Delta::Scalar { path: path.to_string(), before: a.clone(), after: b.clone() }),
    }
}

fn is_numeric_array(a: &[Value]) -> bool {
    !a.is_empty() && a.iter().all(|v| v.is_number())
}

// ── $ta typed-array diff ─────────────────────────────────────────────────────

/// Decode a `{ $ta }` node to bytes (Uint8/Int8/Uint8Clamped directly; Uint32 as
/// its LE bytes). `None` when the node is null / not a decodable typed array.
fn decode_bytes(node: Option<&Value>) -> Option<Vec<u8>> {
    let v = node?;
    if v.is_null() {
        return None;
    }
    ta_u8_decode(v).or_else(|| ta_u32_decode(v).map(|w| w.iter().flat_map(|x| x.to_le_bytes()).collect()))
}

fn diff_ta(a: &Value, b: &Value, path: &str, out: &mut Vec<Delta>) {
    match (decode_bytes(Some(a)), decode_bytes(Some(b))) {
        (Some(ba), Some(bb)) => diff_bytes(&ba, &bb, path, out),
        // One side absent / undecodable but they differ (a==b already returned) →
        // a presence/format change; report as a scalar so it is never lost.
        _ => out.push(Delta::Scalar { path: path.to_string(), before: ta_tag(a), after: ta_tag(b) }),
    }
}

/// A compact stand-in for a `$ta` node in scalar output (never dump the full b64).
fn ta_tag(v: &Value) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let ctor = v.get("$ta").and_then(|c| c.as_str()).unwrap_or("?");
    let len = decode_bytes(Some(v)).map(|b| b.len()).unwrap_or(0);
    json!(format!("<$ta {ctor} {len}B>"))
}

fn diff_bytes(a: &[u8], b: &[u8], path: &str, out: &mut Vec<Delta>) {
    let n = a.len().max(b.len());
    let mut i = 0;
    while i < n {
        if byte_at(a, i) != byte_at(b, i) {
            let start = i;
            let mut sample: Vec<Value> = Vec::new();
            let mut count = 0usize;
            while i < n && byte_at(a, i) != byte_at(b, i) {
                if sample.len() < 8 {
                    sample.push(json!({ "index": i, "before": byte_at(a, i), "after": byte_at(b, i) }));
                }
                count += 1;
                i += 1;
            }
            out.push(Delta::Range { path: path.to_string(), start, end: i - 1, count, sample });
        } else {
            i += 1;
        }
    }
}

fn byte_at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

fn diff_numeric_seq(a: &[Value], b: &[Value], path: &str, out: &mut Vec<Delta>) {
    let n = a.len().max(b.len());
    let mut i = 0;
    let num = |s: &[Value], i: usize| -> i64 { s.get(i).and_then(|v| v.as_i64()).unwrap_or(0) };
    while i < n {
        if num(a, i) != num(b, i) {
            let start = i;
            let mut sample: Vec<Value> = Vec::new();
            let mut count = 0usize;
            while i < n && num(a, i) != num(b, i) {
                if sample.len() < 8 {
                    sample.push(json!({ "index": i, "before": num(a, i), "after": num(b, i) }));
                }
                count += 1;
                i += 1;
            }
            out.push(Delta::Range { path: path.to_string(), start, end: i - 1, count, sample });
        } else {
            i += 1;
        }
    }
}

// ── drive blob decomposition ─────────────────────────────────────────────────

/// Decode the two `drive1541` blobs (VICE-format modules DRIVE8 / DRIVECPU0 /
/// 1541VIA1D0 / 1541VIA2D0) and diff each sub-module into an addressable component
/// (`drive.ram` = the 1541's 2 KB work RAM, `drive.cpu`, `drive.via1`, `drive.via2`).
/// A null / absent blob on either side (drive not attached) yields no drive deltas.
fn drive_blob_components(a: Option<&Value>, b: Option<&Value>, out: &mut Vec<Delta>) {
    let (ba, bb) = match (decode_bytes(a), decode_bytes(b)) {
        (Some(x), Some(y)) => (x, y),
        _ => return,
    };
    // Whole-module components: DRIVE8 = rotation/head state, the two VIAs.
    for (module, comp) in [
        ("DRIVE8", "drive.rotation"),
        ("1541VIA1D0", "drive.via1"),
        ("VIA2D0", "drive.via2"),
    ] {
        if let (Some(da), Some(db)) = (vice_module(&ba, module), vice_module(&bb, module)) {
            if da != db {
                diff_bytes(da, db, comp, out);
            }
        }
    }
    // DRIVECPU0 = the drive 6502 header FOLLOWED by the 2 KB drive RAM as the module's
    // LAST field (drive_snapshot.rs write_drivecpu_module: `smw_ba(&ram, 0x800)` then
    // module_close). So RAM offset = data.len() - 0x800 and its byte indices ARE 1541
    // RAM addresses $0000-$07FF — which is what an `exclude driveram $0000-$07FF` mask
    // and a Floppy-RAM cheat hunt address.
    if let (Some(da), Some(db)) = (vice_module(&ba, "DRIVECPU0"), vice_module(&bb, "DRIVECPU0")) {
        let (hdr_a, ram_a) = split_drive_ram(da);
        let (hdr_b, ram_b) = split_drive_ram(db);
        if hdr_a != hdr_b {
            diff_bytes(hdr_a, hdr_b, "drive.cpu", out);
        }
        if ram_a != ram_b {
            diff_bytes(ram_a, ram_b, "drive.ram", out);
        }
    }
}

/// Split a DRIVECPU0 module's data into (CPU header, 2 KB drive RAM). The RAM is the
/// module's LAST field, so it is the trailing `0x800` bytes; a named fn (not a
/// closure) so both returned slices share the input's lifetime.
fn split_drive_ram(d: &[u8]) -> (&[u8], &[u8]) {
    const DRIVE_RAM: usize = 0x800;
    if d.len() >= DRIVE_RAM {
        d.split_at(d.len() - DRIVE_RAM)
    } else {
        (d, &d[..0])
    }
}

/// Locate a VICE snapshot module's DATA slice by name inside a drive blob. Mirrors
/// the `vsf.rs` module framing: after the 58-byte file header, each module is
/// name(16, null-padded) + major(1) + minor(1) + size(4 LE, TOTAL incl the 22-byte
/// header) + data. Returns the data slice (size - 22 bytes) or None.
fn vice_module<'a>(buf: &'a [u8], want: &str) -> Option<&'a [u8]> {
    const FILE_HEADER: usize = 58;
    const MOD_HEADER: usize = 22;
    if buf.len() < FILE_HEADER {
        return None;
    }
    let mut cur = FILE_HEADER;
    while cur + MOD_HEADER <= buf.len() {
        let name_end = buf[cur..cur + 16].iter().position(|&c| c == 0).unwrap_or(16);
        let name = String::from_utf8_lossy(&buf[cur..cur + name_end]).to_string();
        let size = (buf[cur + 18] as usize)
            | ((buf[cur + 19] as usize) << 8)
            | ((buf[cur + 20] as usize) << 16)
            | ((buf[cur + 21] as usize) << 24);
        if size < MOD_HEADER || cur + size > buf.len() {
            break;
        }
        if name == want {
            return Some(&buf[cur + MOD_HEADER..cur + size]);
        }
        cur += size;
    }
    None
}

// ── classification ───────────────────────────────────────────────────────────

fn component_of(path: &str) -> String {
    let p = path;
    let starts = |s: &str| p == s || p.starts_with(&format!("{s}.")) || p.starts_with(&format!("{s}["));
    if p == "ram" || p.starts_with("ram[") {
        return "ram".into();
    }
    if p.starts_with("vic.color_ram") {
        return "colorram".into();
    }
    if starts("cpuIntStatus") {
        return "cpu.int".into();
    }
    if starts("cpu") {
        return "cpu".into();
    }
    if p == "cpuPortDirection" || p == "cpuPortValue" {
        return "banking".into();
    }
    if starts("cia1") {
        return "cia1".into();
    }
    if starts("cia2") {
        return "cia2".into();
    }
    if starts("sid") {
        return "sid".into();
    }
    if starts("vicPresentation") {
        return "vic.presentation".into();
    }
    if starts("vic") {
        return "vic".into();
    }
    if starts("iec") {
        return "iec".into();
    }
    if p == "cartBytes" || p == "cartFlash" {
        return "cart.rom".into();
    }
    if starts("cartState") {
        return "cart".into();
    }
    // drive.* components are synthesized by drive_blob_components with those exact
    // paths already, so pass them through.
    if starts("drive") {
        return p.split(|c| c == '[').next().unwrap_or(p).to_string();
    }
    if p == "driveDiskImage" {
        return "drive.disk".into();
    }
    if starts("keyboard") {
        return "input.keyboard".into();
    }
    if starts("joystick1") || starts("joystick2") || starts("paddles") {
        return "input".into();
    }
    let seg = p.split(|c| c == '.' || c == '[').next().unwrap_or(p);
    format!("other:{seg}")
}

/// Volatile lanes — state that advances every cycle regardless of behaviour. Tagged
/// (not masked) here; the `equivalence` preset / `exclude.lanes` (later slice) mask
/// them so two behaviourally-identical scratch runs verdict `identical`.
fn lane_of(path: &str) -> Option<&'static str> {
    let p = path;
    if p == "cpu.cycles" || p.ends_with("Clk") || p.ends_with("_clk") || p.ends_with(".rdi") || p.ends_with(".ifr_clock") {
        return Some("cycles");
    }
    if p == "vic.raster_line" || p == "vic.raster_cycle" || p == "vic.cycle_flags" || p == "vic.start_of_frame" {
        return Some("raster");
    }
    if p.starts_with("vic.dbuf") || p.starts_with("vic.vbuf") || p.starts_with("vic.cbuf")
        || p == "vic.gbuf" || p == "vic.dbuf_offset" || p.starts_with("vic.draw_cycle")
        || p.starts_with("vic.presentation") || p.starts_with("vicPresentation")
    {
        return Some("framebuffer");
    }
    if p == "vic.last_bus_phi2" || p == "vic.last_read_phi1" || p == "vic.last_color_reg"
        || p == "vic.last_color_value" || p == "vic.refresh_counter"
    {
        return Some("open_bus");
    }
    None
}

// ── hardware register names ──────────────────────────────────────────────────
// Canonical MOS chip register mnemonics. The checkpoint stores CIA/SID as register
// FILES (`cia1.c_cia[16]`, `sid.regs[32]`) whose Range-delta indices are register
// numbers → name them so a changed register reads `ICR`/`MODEVOL`, not `$0D`/`$18`.
// (VIC is already stored as named fields, so it needs no index table.)

const CIA_REGS: [&str; 16] = [
    "PRA", "PRB", "DDRA", "DDRB", "TALO", "TAHI", "TBLO", "TBHI", "TOD10TH", "TODSEC", "TODMIN",
    "TODHR", "SDR", "ICR", "CRA", "CRB",
];

const SID_REGS: [&str; 29] = [
    "FREQLO1", "FREQHI1", "PWLO1", "PWHI1", "CTRL1", "ATKDCY1", "SUSREL1", "FREQLO2", "FREQHI2",
    "PWLO2", "PWHI2", "CTRL2", "ATKDCY2", "SUSREL2", "FREQLO3", "FREQHI3", "PWLO3", "PWHI3", "CTRL3",
    "ATKDCY3", "SUSREL3", "FCLO", "FCHI", "RESFILT", "MODEVOL", "POTX", "POTY", "OSC3", "ENV3",
];

/// The HW register mnemonic for a register-file path + index (CIA / SID), else None.
fn reg_name_for(path: &str, index: usize) -> Option<&'static str> {
    if path.ends_with(".c_cia") {
        return CIA_REGS.get(index).copied();
    }
    if path == "sid.regs" {
        return SID_REGS.get(index).copied();
    }
    None
}

// ── assemble output ──────────────────────────────────────────────────────────

fn assemble(deltas: Vec<Delta>, excluded: Vec<String>, a: &Value, b: &Value) -> Value {
    use std::collections::BTreeMap;
    let mut comps: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for d in &deltas {
        let (path, entry) = match d {
            Delta::Scalar { path, before, after } => {
                let mut e = json!({ "kind": "scalar", "path": path, "before": before, "after": after });
                if let Some(l) = lane_of(path) {
                    e["lane"] = json!(l);
                }
                (path.clone(), e)
            }
            Delta::Range { path, start, end, count, sample } => {
                // Attach the HW register mnemonic to each sample on a register-file path.
                let named: Vec<Value> = sample
                    .iter()
                    .map(|sv| {
                        let mut sv = sv.clone();
                        if let Some(idx) = sv.get("index").and_then(|x| x.as_u64()) {
                            if let Some(name) = reg_name_for(path, idx as usize) {
                                sv["reg"] = json!(name);
                            }
                        }
                        sv
                    })
                    .collect();
                let mut e = json!({ "kind": "range", "path": path, "start": start, "end": end, "count": count, "sample": named });
                if let Some(l) = lane_of(path) {
                    e["lane"] = json!(l);
                }
                (path.clone(), e)
            }
        };
        comps.entry(component_of(&path)).or_default().push(entry);
    }

    let mut scope: Vec<String> = CANON.iter().map(|s| s.to_string()).collect();
    for k in comps.keys() {
        if !scope.contains(k) {
            scope.push(k.clone());
        }
    }
    let mut differing: Vec<String> = comps.keys().cloned().collect();
    differing.sort();

    let mut cmap = serde_json::Map::new();
    for comp in &scope {
        let entry = match comps.get(comp) {
            None => json!({ "identical": true }),
            Some(ch) => {
                let summary = summarize(ch);
                let capped: Vec<Value> = ch.iter().take(64).cloned().collect();
                let truncated = ch.len().saturating_sub(64);
                let mut e = json!({ "identical": false, "summary": summary, "changes": capped });
                if truncated > 0 {
                    e["truncated"] = json!(truncated);
                }
                e
            }
        };
        cmap.insert(comp.clone(), entry);
    }

    json!({
        "schema": "c64re.checkpoint-diff/1",
        "fromCycle": nested_i64(a, &["cpu", "cycles"]),
        "toCycle": nested_i64(b, &["cpu", "cycles"]),
        "verdict": {
            "identical": differing.is_empty(),
            "differing": differing,
            "excluded": excluded,
            "scope": scope,
        },
        "components": Value::Object(cmap),
    })
}

fn summarize(ch: &[Value]) -> String {
    let mut scal = 0;
    let mut rng = 0;
    let mut bytes: i64 = 0;
    let mut regs: Vec<String> = Vec::new();
    for c in ch {
        if c.get("kind").and_then(|k| k.as_str()) == Some("range") {
            rng += 1;
            bytes += c.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
            if let Some(sample) = c.get("sample").and_then(|s| s.as_array()) {
                for sv in sample {
                    if let Some(r) = sv.get("reg").and_then(|r| r.as_str()) {
                        if !regs.iter().any(|x| x == r) {
                            regs.push(r.to_string());
                        }
                    }
                }
            }
        } else {
            scal += 1;
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if scal > 0 {
        parts.push(format!("{scal} field{}", if scal == 1 { "" } else { "s" }));
    }
    if rng > 0 {
        parts.push(format!("{rng} range{} ({bytes} bytes)", if rng == 1 { "" } else { "s" }));
    }
    let mut s = parts.join(", ");
    if !regs.is_empty() {
        s.push_str(&format!(" [{}]", regs.join(", ")));
    }
    s
}

fn nested_i64(v: &Value, keys: &[&str]) -> i64 {
    let mut cur = v;
    for k in keys {
        match cur.get(k) {
            Some(next) => cur = next,
            None => return 0,
        }
    }
    cur.as_i64().unwrap_or(0)
}

// ── cheat-candidate finder (Spec 798) ────────────────────────────────────────

/// Find RAM addresses that DECREASED between two checkpoints — candidate life /
/// health / ammo counters for a cheat (the snapshot-diff → decrementer step).
/// Compares the full 64K RAM (not the 794 capped sample). Ranked: smallest delta
/// first (a life counter is usually −1), then by address. Returns up to `max`.
pub fn find_ram_decrements(a: &Value, b: &Value, max: usize) -> Vec<Value> {
    let (ra, rb) = match (decode_bytes(a.get("ram")), decode_bytes(b.get("ram"))) {
        (Some(x), Some(y)) => (x, y),
        _ => return vec![],
    };
    let n = ra.len().min(rb.len());
    let mut out: Vec<(usize, u8, u8, u8)> = Vec::new();
    for i in 0..n {
        if rb[i] < ra[i] {
            out.push((i, ra[i], rb[i], ra[i] - rb[i]));
        }
    }
    out.sort_by(|x, y| x.3.cmp(&y.3).then(x.0.cmp(&y.0)));
    out.into_iter()
        .take(max)
        .map(|(addr, before, after, delta)| {
            json!({ "addr": addr, "before": before, "after": after, "delta": delta })
        })
        .collect()
}

// ── text rendering ───────────────────────────────────────────────────────────

fn str_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// A compact human/agent-readable rendering of a component-diff value — the verdict,
/// the excluded (masked) set, and a one-line summary per differing component. Used by
/// `trx64cli diff` and the daemon monitor `diff` command.
pub fn format_component_diff(d: &Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    let from = d.get("fromCycle").and_then(|v| v.as_i64()).unwrap_or(0);
    let to = d.get("toCycle").and_then(|v| v.as_i64()).unwrap_or(0);
    lines.push(format!("checkpoint diff  cycles {from} → {to}"));

    let verdict = d.get("verdict").cloned().unwrap_or_else(|| json!({}));
    let identical = verdict.get("identical").and_then(|v| v.as_bool()).unwrap_or(false);
    let differing = str_list(&verdict, "differing");
    if identical {
        lines.push("VERDICT: IDENTICAL".to_string());
    } else {
        lines.push(format!("VERDICT: DIFFERS  (differing: {})", differing.join(", ")));
    }
    let excluded = str_list(&verdict, "excluded");
    if !excluded.is_empty() {
        lines.push(format!("excluded: {}", excluded.join(", ")));
    }

    if let Some(comps) = d.get("components").and_then(|v| v.as_object()) {
        for name in &differing {
            let summary = comps
                .get(name)
                .and_then(|c| c.get("summary"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            lines.push(format!("  {name:<14} {summary}"));
        }
    }
    lines.join("\n")
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_snapshot::ta_u8;

    fn mini() -> Value {
        json!({
            "schemaVersion": 1,
            "cpu": { "pc": 0x1000, "a": 0, "x": 0, "y": 0, "sp": 0xff, "flags": 0x20, "cycles": 100 },
            "ram": ta_u8(&vec![0u8; 64]),
            "cpuPortDirection": 0x2f,
            "cpuPortValue": 0x37,
            "cia1": { "c_cia": [0,0,0,0], "ta_latch": 0x100, "ta_clk": 50 },
            "cia2": { "c_cia": [0,0,0,0], "ta_latch": 0x200 },
            "sid": { "c_sid": [0,0,0], "v": 2 },
            "iec": { "cpu_bus": 0xff, "drv_port": 0xff },
            "vic": { "raster_line": 10, "color_ram": vec![0i64; 16], "vc": 0, "last_bus_phi2": 0 },
            "drive1541": Value::Null,
        })
    }

    #[test]
    fn identical_is_identical() {
        let m = mini();
        let d = diff_checkpoints(&m, &m.clone(), &ExcludeMask::default());
        assert_eq!(d["verdict"]["identical"], json!(true));
        assert_eq!(d["verdict"]["differing"], json!([]));
        // scope must always list the canonical components.
        assert!(d["verdict"]["scope"].as_array().unwrap().iter().any(|c| c == "drive.ram"));
    }

    #[test]
    fn cpu_pc_change_named() {
        let a = mini();
        let mut b = mini();
        b["cpu"]["pc"] = json!(0x2000);
        let d = diff_checkpoints(&a, &b, &ExcludeMask::default());
        assert_eq!(d["verdict"]["identical"], json!(false));
        assert!(d["verdict"]["differing"].as_array().unwrap().iter().any(|c| c == "cpu"));
        assert_eq!(d["components"]["cpu"]["identical"], json!(false));
    }

    #[test]
    fn ram_byte_range_detected() {
        let a = mini();
        let mut bytes = vec![0u8; 64];
        bytes[5] = 0x5a;
        let mut b = mini();
        b["ram"] = ta_u8(&bytes);
        let d = diff_checkpoints(&a, &b, &ExcludeMask::default());
        assert!(d["verdict"]["differing"].as_array().unwrap().iter().any(|c| c == "ram"));
        let ch = &d["components"]["ram"]["changes"][0];
        assert_eq!(ch["kind"], json!("range"));
        assert_eq!(ch["start"], json!(5));
        assert_eq!(ch["count"], json!(1));
    }

    #[test]
    fn color_ram_is_its_own_component() {
        let a = mini();
        let mut b = mini();
        b["vic"]["color_ram"][5] = json!(0x0e);
        let d = diff_checkpoints(&a, &b, &ExcludeMask::default());
        // color RAM must NOT fold into `vic` — it is its own component.
        assert!(d["verdict"]["differing"].as_array().unwrap().iter().any(|c| c == "colorram"));
        assert_eq!(d["components"]["vic"]["identical"], json!(true));
    }

    #[test]
    fn cia_internal_field_covered() {
        let a = mini();
        let mut b = mini();
        b["cia1"]["ta_latch"] = json!(0x1ff);
        let d = diff_checkpoints(&a, &b, &ExcludeMask::default());
        assert!(d["verdict"]["differing"].as_array().unwrap().iter().any(|c| c == "cia1"));
    }

    #[test]
    fn raster_change_tagged_volatile_lane() {
        let a = mini();
        let mut b = mini();
        b["vic"]["raster_line"] = json!(42);
        let d = diff_checkpoints(&a, &b, &ExcludeMask::default());
        // strict mode: vic differs (lane not masked yet)…
        assert!(d["verdict"]["differing"].as_array().unwrap().iter().any(|c| c == "vic"));
        // …but the change carries the `raster` lane tag for later masking.
        let ch = &d["components"]["vic"]["changes"][0];
        assert_eq!(ch["lane"], json!("raster"));
    }

    // ── mask tests ───────────────────────────────────────────────────────────

    /// Build a minimal `drive1541` blob = one DRIVECPU0 module whose LAST 0x800
    /// bytes are the drive RAM (as the real writer lays it out), preceded by an
    /// 8-byte stand-in CPU header. Mirrors `vice_module`'s framing.
    fn drive_blob(ram: &[u8]) -> Vec<u8> {
        fn push_module(buf: &mut Vec<u8>, name: &str, data: &[u8]) {
            let mut nm = [0u8; 16];
            let nb = name.as_bytes();
            nm[..nb.len()].copy_from_slice(nb);
            buf.extend_from_slice(&nm);
            buf.push(0); // major
            buf.push(0); // minor
            let size = (22 + data.len()) as u32; // TOTAL incl the 22-byte header
            buf.extend_from_slice(&size.to_le_bytes());
            buf.extend_from_slice(data);
        }
        let mut buf = vec![0u8; 58]; // file header (length only matters to vice_module)
        let mut data = vec![0u8; 8]; // CPU header stand-in
        data.extend_from_slice(ram); // 0x800 RAM at the tail
        push_module(&mut buf, "DRIVECPU0", &data);
        buf
    }

    fn with_drive(ram: &[u8]) -> Value {
        let mut m = mini();
        m["drive1541"] = ta_u8(&drive_blob(ram));
        m
    }

    #[test]
    fn floppy_ram_masked_in_and_out() {
        let mut ram_a = vec![0u8; 0x800];
        let mut ram_b = vec![0u8; 0x800];
        ram_a[5] = 0x03;
        ram_b[5] = 0x00; // a life counter decremented in drive RAM
        let a = with_drive(&ram_a);
        let b = with_drive(&ram_b);

        // Strict: Floppy RAM is a first-class component and differs at $0005.
        let strict = diff_checkpoints(&a, &b, &ExcludeMask::default());
        assert_eq!(strict["verdict"]["identical"], json!(false));
        assert!(strict["verdict"]["differing"].as_array().unwrap().iter().any(|c| c == "drive.ram"));
        let ch = &strict["components"]["drive.ram"]["changes"][0];
        assert_eq!(ch["start"], json!(5));

        // Masked: exclude the whole Floppy RAM → identical, and echoed.
        let mask = ExcludeMask::from_json(&json!({
            "ranges": [{ "space": "driveram", "from": "0x0000", "to": "0x07FF" }]
        }));
        let masked = diff_checkpoints(&a, &b, &mask);
        assert_eq!(masked["verdict"]["identical"], json!(true));
        let ex = masked["verdict"]["excluded"].as_array().unwrap();
        assert!(ex.iter().any(|e| e.as_str().unwrap().starts_with("range:drive.ram")));
    }

    #[test]
    fn partial_range_mask_keeps_residual() {
        let mut ram_a = vec![0u8; 0x800];
        let mut ram_b = vec![0u8; 0x800];
        ram_a[0x10] = 1; // inside the masked window
        ram_b[0x10] = 2;
        ram_a[0x200] = 1; // OUTSIDE the masked window → must survive
        ram_b[0x200] = 2;
        let a = with_drive(&ram_a);
        let b = with_drive(&ram_b);
        let mask = ExcludeMask::from_json(&json!({
            "ranges": [{ "space": "driveram", "from": "0x0000", "to": "0x00FF" }]
        }));
        let d = diff_checkpoints(&a, &b, &mask);
        // $0200 still differs → not identical, drive.ram still listed.
        assert_eq!(d["verdict"]["identical"], json!(false));
        let changes = d["components"]["drive.ram"]["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["start"], json!(0x200));
    }

    #[test]
    fn component_exclude_removes_it() {
        let a = mini();
        let mut b = mini();
        b["cpu"]["pc"] = json!(0x2000);
        let mask = ExcludeMask::from_json(&json!({ "components": ["cpu"] }));
        let d = diff_checkpoints(&a, &b, &mask);
        assert_eq!(d["verdict"]["identical"], json!(true));
        assert!(d["verdict"]["excluded"].as_array().unwrap().iter().any(|e| e == "component:cpu"));
    }

    #[test]
    fn equivalence_preset_masks_volatile_lanes() {
        let a = mini();
        let mut b = mini();
        b["vic"]["raster_line"] = json!(200); // volatile — different scratch run timing
        b["cpu"]["cycles"] = json!(999_999);
        let strict = diff_checkpoints(&a, &b, &ExcludeMask::default());
        assert_eq!(strict["verdict"]["identical"], json!(false));
        let eq = ExcludeMask::from_json(&json!({ "presets": ["equivalence"] }));
        let d = diff_checkpoints(&a, &b, &eq);
        // raster + cycles are volatile lanes → behaviourally identical.
        assert_eq!(d["verdict"]["identical"], json!(true));
    }

    #[test]
    fn echo_law_reports_zero_match() {
        let a = mini();
        let b = mini(); // identical
        let mask = ExcludeMask::from_json(&json!({ "components": ["sid"] }));
        let d = diff_checkpoints(&a, &b, &mask);
        assert!(d["verdict"]["excluded"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == "component:sid (matched 0)"));
    }

    #[test]
    fn cia_register_gets_hw_name() {
        let mut a = mini();
        a["cia1"]["c_cia"] = json!(vec![0i64; 16]);
        let mut regs = vec![0i64; 16];
        regs[13] = 0x81; // CIA register $0D = ICR
        let mut b = mini();
        b["cia1"]["c_cia"] = json!(regs);
        let d = diff_checkpoints(&a, &b, &ExcludeMask::default());
        let sample = &d["components"]["cia1"]["changes"][0]["sample"][0];
        assert_eq!(sample["reg"], json!("ICR"));
        assert!(d["components"]["cia1"]["summary"].as_str().unwrap().contains("ICR"));
    }

    #[test]
    fn find_ram_decrements_ranks_smallest_delta_ignores_increases() {
        let mut a = vec![0u8; 32];
        a[5] = 3;
        a[6] = 9;
        a[10] = 5;
        let mut b = vec![0u8; 32];
        b[5] = 2; // −1 (a life counter)
        b[6] = 1; // −8
        b[10] = 8; // +3 → an increase, must be ignored
        let ca = json!({ "ram": ta_u8(&a) });
        let cb = json!({ "ram": ta_u8(&b) });
        let cands = find_ram_decrements(&ca, &cb, 10);
        assert_eq!(cands.len(), 2, "only the two decreases");
        assert_eq!(cands[0]["addr"], json!(5), "smallest delta first");
        assert_eq!(cands[0]["delta"], json!(1));
        assert_eq!(cands[1]["addr"], json!(6));
        assert!(cands.iter().all(|c| c["addr"] != json!(10)), "increase excluded");
    }
}
