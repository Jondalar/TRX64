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

// ── public entry ─────────────────────────────────────────────────────────────

/// Diff two RuntimeCheckpoint JSON trees → the Spec 794 component-diff value:
/// `{ schema, fromCycle, toCycle, verdict{identical,differing,excluded,scope},
///    components{ <comp>: {identical, summary, changes[]} } }`.
///
/// STRICT: every component is in scope, no masking (the `exclude` mask is a later
/// slice). This is the Spec 792 byte-exact bar — any single differing field/byte in
/// any component (incl. color RAM, drive RAM, internal chip state) flips
/// `verdict.identical` to false and names the component.
pub fn diff_checkpoints(a: &Value, b: &Value) -> Value {
    let mut deltas: Vec<Delta> = Vec::new();
    walk(a, b, "", &mut deltas);

    // Decompose the opaque drive blob into addressable sub-module components so
    // Floppy RAM etc. are diffed at byte granularity, not "drive.blob changed".
    let drive_a = a.get("drive1541");
    let drive_b = b.get("drive1541");
    drive_blob_components(drive_a, drive_b, &mut deltas);

    assemble(deltas, a, b)
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
    // Each (module-name, component, address-base) — DRIVE8 carries the 2 KB RAM.
    for (module, comp) in [
        ("DRIVE8", "drive.ram"),
        ("DRIVECPU0", "drive.cpu"),
        ("1541VIA1D0", "drive.via1"),
        ("1541VIA2D0", "drive.via2"),
    ] {
        let ma = vice_module(&ba, module);
        let mb = vice_module(&bb, module);
        match (ma, mb) {
            (Some(da), Some(db)) if da != db => diff_bytes(da, db, comp, out),
            _ => {}
        }
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

// ── assemble output ──────────────────────────────────────────────────────────

fn assemble(deltas: Vec<Delta>, a: &Value, b: &Value) -> Value {
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
                let mut e = json!({ "kind": "range", "path": path, "start": start, "end": end, "count": count, "sample": sample });
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
            "excluded": [],
            "scope": scope,
        },
        "components": Value::Object(cmap),
    })
}

fn summarize(ch: &[Value]) -> String {
    let mut scal = 0;
    let mut rng = 0;
    let mut bytes: i64 = 0;
    for c in ch {
        if c.get("kind").and_then(|k| k.as_str()) == Some("range") {
            rng += 1;
            bytes += c.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
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
    parts.join(", ")
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
        let d = diff_checkpoints(&m, &m.clone());
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
        let d = diff_checkpoints(&a, &b);
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
        let d = diff_checkpoints(&a, &b);
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
        let d = diff_checkpoints(&a, &b);
        // color RAM must NOT fold into `vic` — it is its own component.
        assert!(d["verdict"]["differing"].as_array().unwrap().iter().any(|c| c == "colorram"));
        assert_eq!(d["components"]["vic"]["identical"], json!(true));
    }

    #[test]
    fn cia_internal_field_covered() {
        let a = mini();
        let mut b = mini();
        b["cia1"]["ta_latch"] = json!(0x1ff);
        let d = diff_checkpoints(&a, &b);
        assert!(d["verdict"]["differing"].as_array().unwrap().iter().any(|c| c == "cia1"));
    }

    #[test]
    fn raster_change_tagged_volatile_lane() {
        let a = mini();
        let mut b = mini();
        b["vic"]["raster_line"] = json!(42);
        let d = diff_checkpoints(&a, &b);
        // strict mode: vic differs (lane not masked yet)…
        assert!(d["verdict"]["differing"].as_array().unwrap().iter().any(|c| c == "vic"));
        // …but the change carries the `raster` lane tag for later masking.
        let ch = &d["components"]["vic"]["changes"][0];
        assert_eq!(ch["lane"], json!("raster"));
    }
}
