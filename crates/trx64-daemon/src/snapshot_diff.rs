//! snapshot_diff.rs — Spec 246 save-state semantic diff.
//!
//! Faithful port of `src/runtime/headless/v2/snapshot-diff.ts` `diffSnapshots(a, b)`
//! + `formatDiff(diff)`. Takes two c64re-own VSF byte buffers (the SAME framing
//! `vsf::save_vsf` emits and the `runtime/call saveVsf` method returns) and produces
//! the `SnapshotDiff`-shaped JSON value the TS API returns:
//!
//!   { fromCycle, toCycle, ram{changedRanges,sample,totalChanged},
//!     cpu{changedRegs,pcDelta,cyclesDelta}, cia1, cia2, vic, sid,
//!     pla{configBefore,configAfter}, drive?, iecBus{edgesBetween,finalState} }
//!
//! Each chip's register array is compared element-by-element; RAM is diffed
//! byte-granular with contiguous runs collapsed into ranges (first 100 samples).
//! The drive sub-diff is emitted only when BOTH snapshots carry a DRIVECPU module
//! (snapshot-diff.ts:144-172) — VIA1d1541/VIA2d1541/GCRHEAD are separate VSF modules
//! the c64re-own save path does NOT emit, so the drive register diffs come back
//! empty and headPosition 0/0, exactly as the TS reader sees them.

use serde_json::{json, Value};

// VSF framing constants (vsf.rs).
const VSF_MAGIC: &[u8; 19] = b"VICE Snapshot File\x1a";

/// Parse the c64re-own VSF framing into (module-name → data slice). 1:1 with
/// vsf-format.ts `readVsf`: magic(19) + major(1) + minor(1) + null-term machine
/// name, then per module: null-term name + major(1) + minor(1) + 4-byte LE len + data.
fn index_modules(buf: &[u8]) -> Vec<(String, &[u8])> {
    let mut out: Vec<(String, &[u8])> = Vec::new();
    if buf.len() < VSF_MAGIC.len() + 2 || &buf[..VSF_MAGIC.len()] != VSF_MAGIC {
        return out;
    }
    // magic(19) + major + minor, then null-terminated machine name.
    let mut cur = VSF_MAGIC.len() + 2;
    match buf[cur..].iter().position(|&b| b == 0) {
        Some(nul) => cur += nul + 1,
        None => return out,
    }
    while cur < buf.len() {
        let Some(rel) = buf[cur..].iter().position(|&b| b == 0) else { break };
        let name = String::from_utf8_lossy(&buf[cur..cur + rel]).to_string();
        cur += rel + 1;
        if cur + 6 > buf.len() {
            break; // major + minor + 4-byte length
        }
        cur += 2; // major, minor
        let len = (buf[cur] as usize)
            | ((buf[cur + 1] as usize) << 8)
            | ((buf[cur + 2] as usize) << 16)
            | ((buf[cur + 3] as usize) << 24);
        cur += 4;
        if cur + len > buf.len() {
            break;
        }
        out.push((name, &buf[cur..cur + len]));
        cur += len;
    }
    out
}

fn module<'a>(mods: &'a [(String, &'a [u8])], want: &str) -> Option<&'a [u8]> {
    mods.iter().find(|(n, _)| n == want).map(|(_, d)| *d)
}

// ── CPU (MAINCPU) ─────────────────────────────────────────────────────────────
// Layout: PC(2) A X Y SP P cycles(4 LE) = 11 bytes (snapshot-diff.ts:301-315).
struct CpuFields {
    pc: u32,
    a: u32,
    x: u32,
    y: u32,
    sp: u32,
    p: u32,
    cycles: u32,
}

fn parse_cpu(data: Option<&[u8]>) -> CpuFields {
    match data {
        Some(d) if d.len() >= 11 => CpuFields {
            pc: (d[0] as u32) | ((d[1] as u32) << 8),
            a: d[2] as u32,
            x: d[3] as u32,
            y: d[4] as u32,
            sp: d[5] as u32,
            p: d[6] as u32,
            cycles: (d[7] as u32)
                | ((d[8] as u32) << 8)
                | ((d[9] as u32) << 16)
                | ((d[10] as u32) << 24),
        },
        _ => CpuFields { pc: 0, a: 0, x: 0, y: 0, sp: 0, p: 0, cycles: 0 },
    }
}

/// diffCpuModule (snapshot-diff.ts:317-337). Returns (changedRegs, pcDelta,
/// cyclesDelta, cyclesBefore, cyclesAfter).
fn diff_cpu(a: Option<&[u8]>, b: Option<&[u8]>) -> (Vec<Value>, u32, u32, u32, u32) {
    let fa = parse_cpu(a);
    let fb = parse_cpu(b);
    let mut changed: Vec<Value> = Vec::new();
    let fields: [(&str, u32, u32); 6] = [
        ("pc", fa.pc, fb.pc),
        ("a", fa.a, fb.a),
        ("x", fa.x, fb.x),
        ("y", fa.y, fb.y),
        ("sp", fa.sp, fb.sp),
        ("flags", fa.p, fb.p), // f === "p" ? "flags" : f
    ];
    for (name, before, after) in fields {
        if before != after {
            changed.push(json!({ "reg": name, "before": before, "after": after }));
        }
    }
    let pc_delta = fb.pc.wrapping_sub(fa.pc) & 0xffff;
    let cycles_delta = fb.cycles.wrapping_sub(fa.cycles);
    (changed, pc_delta, cycles_delta, fa.cycles, fb.cycles)
}

/// diffCpuModuleAsDriveChip (snapshot-diff.ts:340-360): returns a ChipDiff. Reg index
/// = field position (pc=0,a=1,x=2,y=3,sp=4,p=5); cycles delta → an internalStateNote.
fn diff_cpu_as_drive_chip(a: Option<&[u8]>, b: Option<&[u8]>) -> Value {
    let fa = parse_cpu(a);
    let fb = parse_cpu(b);
    let mut changed: Vec<Value> = Vec::new();
    let fields: [(u32, u32, u32); 6] = [
        (0, fa.pc, fb.pc),
        (1, fa.a, fb.a),
        (2, fa.x, fb.x),
        (3, fa.y, fb.y),
        (4, fa.sp, fb.sp),
        (5, fa.p, fb.p),
    ];
    for (idx, before, after) in fields {
        if before != after {
            changed.push(json!({ "reg": idx, "before": before, "after": after }));
        }
    }
    let mut notes: Vec<String> = Vec::new();
    if fa.cycles != fb.cycles {
        notes.push(format!("cycles: {} → {}", fa.cycles, fb.cycles));
    }
    json!({ "changedRegisters": changed, "internalStateNotes": notes })
}

/// diffRegisterArray (snapshot-diff.ts:362-376): pad both to expectedLen, compare.
fn diff_register_array(a: Option<&[u8]>, b: Option<&[u8]>, expected_len: usize) -> Value {
    let za = vec![0u8; expected_len];
    let zb = vec![0u8; expected_len];
    let av: &[u8] = match a {
        Some(d) if d.len() >= expected_len => d,
        _ => &za,
    };
    let bv: &[u8] = match b {
        Some(d) if d.len() >= expected_len => d,
        _ => &zb,
    };
    let mut changed: Vec<Value> = Vec::new();
    for i in 0..expected_len {
        if av[i] != bv[i] {
            changed.push(json!({ "reg": i, "before": av[i], "after": bv[i] }));
        }
    }
    json!({ "changedRegisters": changed, "internalStateNotes": [] })
}

/// diffRam (snapshot-diff.ts:378-415): byte-granular over min(len), runs collapsed,
/// first 100 samples.
fn diff_ram(a: &[u8], b: &[u8]) -> Value {
    let len = a.len().min(b.len());
    let mut sample: Vec<Value> = Vec::new();
    let mut changed_ranges: Vec<Value> = Vec::new();
    let mut total_changed: u64 = 0;
    let mut range_start: i64 = -1;
    let mut range_end: i64 = -1;

    // Note the TS loop runs i in 0..=len so a trailing run is flushed.
    for i in 0..=len {
        let changed = i < len && a[i] != b[i];
        if changed {
            if sample.len() < 100 {
                sample.push(json!({ "addr": i, "before": a[i], "after": b[i] }));
            }
            total_changed += 1;
            if range_start < 0 {
                range_start = i as i64;
                range_end = i as i64;
            } else {
                range_end = i as i64;
            }
        } else if range_start >= 0 {
            changed_ranges.push(json!({
                "start": range_start,
                "end": range_end,
                "byteCount": range_end - range_start + 1,
            }));
            range_start = -1;
            range_end = -1;
        }
    }
    json!({ "changedRanges": changed_ranges, "sample": sample, "totalChanged": total_changed })
}

/// diffIecBus (snapshot-diff.ts:417-449): 6-byte released-flags → logical ATN/CLK/DATA.
fn diff_iec_bus(a: Option<&[u8]>, b: Option<&[u8]>) -> Value {
    let za = [0u8; 6];
    let zb = [0u8; 6];
    let av: &[u8] = a.filter(|d| d.len() >= 6).unwrap_or(&za);
    let bv: &[u8] = b.filter(|d| d.len() >= 6).unwrap_or(&zb);

    let atn_a = if av[0] != 0 { 1 } else { 0 };
    let clk_a = if av[1] != 0 && av[3] != 0 { 1 } else { 0 };
    let data_a = if av[2] != 0 && av[4] != 0 { 1 } else { 0 };
    let atn_b = if bv[0] != 0 { 1 } else { 0 };
    let clk_b = if bv[1] != 0 && bv[3] != 0 { 1 } else { 0 };
    let data_b = if bv[2] != 0 && bv[4] != 0 { 1 } else { 0 };

    let mut edges = 0;
    if atn_a != atn_b {
        edges += 1;
    }
    if clk_a != clk_b {
        edges += 1;
    }
    if data_a != data_b {
        edges += 1;
    }
    json!({
        "edgesBetween": edges,
        "finalState": { "atn": atn_b, "clk": clk_b, "data": data_b },
    })
}

/// plaSummary (snapshot-diff.ts:451-462): C64MEM bytes [65536]/[65537] = CPU port
/// dir/value; bits 0..2 = LORAM/HIRAM/CHAREN.
fn pla_summary(data: Option<&[u8]>) -> String {
    let Some(d) = data else { return "unknown".to_string() };
    if d.len() < 65538 {
        return "unknown".to_string();
    }
    let dir = d[65536];
    let val = d[65537];
    let loram = if val & 0x01 != 0 { "LORAM" } else { "loram" };
    let hiram = if val & 0x02 != 0 { "HIRAM" } else { "hiram" };
    let charen = if val & 0x04 != 0 { "CHAREN" } else { "charen" };
    format!("${:02X}/${:02X} ({loram},{hiram},{charen})", dir, val)
}

/// `diffSnapshots(a, b)` (snapshot-diff.ts:84-191).
pub fn diff_snapshots(a: &[u8], b: &[u8]) -> Value {
    let ma = index_modules(a);
    let mb = index_modules(b);

    let (cpu_changed, pc_delta, cycles_delta, cycles_before, cycles_after) =
        diff_cpu(module(&ma, "MAINCPU"), module(&mb, "MAINCPU"));

    let mem_a = module(&ma, "C64MEM");
    let mem_b = module(&mb, "C64MEM");
    let zero = [0u8; 65536];
    let ram_a: &[u8] = mem_a.map(|m| &m[..m.len().min(65536)]).unwrap_or(&zero);
    let ram_b: &[u8] = mem_b.map(|m| &m[..m.len().min(65536)]).unwrap_or(&zero);
    let ram = diff_ram(ram_a, ram_b);

    let pla_a = pla_summary(mem_a);
    let pla_b = pla_summary(mem_b);

    let cia1 = diff_register_array(
        module(&ma, "CIA1").map(|d| &d[..d.len().min(16)]),
        module(&mb, "CIA1").map(|d| &d[..d.len().min(16)]),
        16,
    );
    let cia2 = diff_register_array(
        module(&ma, "CIA2").map(|d| &d[..d.len().min(16)]),
        module(&mb, "CIA2").map(|d| &d[..d.len().min(16)]),
        16,
    );
    let vic = diff_register_array(
        module(&ma, "VIC-II").map(|d| &d[..d.len().min(80)]),
        module(&mb, "VIC-II").map(|d| &d[..d.len().min(80)]),
        80,
    );
    let sid = diff_register_array(module(&ma, "SID"), module(&mb, "SID"), 32);

    let iec = diff_iec_bus(module(&ma, "IECBUS"), module(&mb, "IECBUS"));

    // Drive sub-diff — only when BOTH carry DRIVECPU (snapshot-diff.ts:144-172).
    let drive: Value = if module(&ma, "DRIVECPU").is_some() && module(&mb, "DRIVECPU").is_some() {
        let d_cpu = diff_cpu_as_drive_chip(module(&ma, "DRIVECPU"), module(&mb, "DRIVECPU"));
        let via1 = diff_register_array(
            module(&ma, "VIA1d1541").map(|d| &d[..d.len().min(15)]),
            module(&mb, "VIA1d1541").map(|d| &d[..d.len().min(15)]),
            15,
        );
        let via2 = diff_register_array(
            module(&ma, "VIA2d1541").map(|d| &d[..d.len().min(15)]),
            module(&mb, "VIA2d1541").map(|d| &d[..d.len().min(15)]),
            15,
        );
        let head_a = module(&ma, "GCRHEAD");
        let head_b = module(&mb, "GCRHEAD");
        let th_before = head_a.filter(|h| h.len() >= 2).map(|h| (h[0] as u32) | ((h[1] as u32) << 8)).unwrap_or(0);
        let th_after = head_b.filter(|h| h.len() >= 2).map(|h| (h[0] as u32) | ((h[1] as u32) << 8)).unwrap_or(0);
        json!({
            "cpu": d_cpu,
            "via1": via1,
            "via2": via2,
            "headPosition": { "trackHalfBefore": th_before, "trackHalfAfter": th_after },
        })
    } else {
        Value::Null
    };

    let mut out = serde_json::Map::new();
    out.insert("fromCycle".to_string(), json!(cycles_before));
    out.insert("toCycle".to_string(), json!(cycles_after));
    out.insert("ram".to_string(), ram);
    out.insert(
        "cpu".to_string(),
        json!({ "changedRegs": cpu_changed, "pcDelta": pc_delta, "cyclesDelta": cycles_delta }),
    );
    out.insert("cia1".to_string(), cia1);
    out.insert("cia2".to_string(), cia2);
    out.insert("vic".to_string(), vic);
    out.insert("sid".to_string(), sid);
    out.insert(
        "pla".to_string(),
        json!({ "configBefore": pla_a, "configAfter": pla_b }),
    );
    // `drive` is OMITTED (TS leaves it `undefined`) when not both-present.
    if !drive.is_null() {
        out.insert("drive".to_string(), drive);
    }
    out.insert("iecBus".to_string(), iec);
    Value::Object(out)
}

// ── formatDiff (snapshot-diff.ts:195-278) ─────────────────────────────────────

fn hex(n: i64) -> String {
    format!("{:X}", n)
}
fn hex2(n: i64) -> String {
    format!("{:02X}", n & 0xff)
}
fn hex4(n: i64) -> String {
    format!("{:04X}", n & 0xffff)
}

fn as_i64(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

fn format_chip_diff(label: &str, diff: &Value) -> String {
    let regs = diff
        .get("changedRegisters")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let notes = diff
        .get("internalStateNotes")
        .and_then(|n| n.as_array())
        .cloned()
        .unwrap_or_default();
    if regs.is_empty() && notes.is_empty() {
        return format!("{:<8}no changes", label);
    }
    let reg_str = regs
        .iter()
        .take(6)
        .map(|cr| {
            format!(
                "${} ${}→${}",
                hex2(as_i64(cr, "reg")),
                hex2(as_i64(cr, "before")),
                hex2(as_i64(cr, "after"))
            )
        })
        .collect::<Vec<_>>()
        .join("  ");
    let ellipsis = if regs.len() > 6 {
        format!(" (+{} more)", regs.len() - 6)
    } else {
        String::new()
    };
    let notes_str = if !notes.is_empty() {
        let joined = notes
            .iter()
            .map(|n| n.as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join("; ");
        format!("  [{joined}]")
    } else {
        String::new()
    };
    format!("{:<8}{reg_str}{ellipsis}{notes_str}", label)
}

/// `formatDiff(diff)` (snapshot-diff.ts:195-278) — the text-table helper.
pub fn format_diff(diff: &Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    let from = as_i64(diff, "fromCycle");
    let to = as_i64(diff, "toCycle");
    lines.push(format!("Snapshot diff  cycles {from} → {to}  (Δ{})", to - from));
    lines.push(String::new());

    // RAM
    let ram = diff.get("ram").cloned().unwrap_or(json!({}));
    let total = as_i64(&ram, "totalChanged");
    if total == 0 {
        lines.push("RAM:    no changes".to_string());
    } else {
        let ranges = ram.get("changedRanges").and_then(|r| r.as_array()).cloned().unwrap_or_default();
        let range_str = ranges
            .iter()
            .take(8)
            .map(|rng| {
                let start = as_i64(rng, "start");
                let end = as_i64(rng, "end");
                let bc = as_i64(rng, "byteCount");
                if bc == 1 {
                    format!("${}", hex(start))
                } else {
                    format!("${}-${}", hex(start), hex(end))
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let ellipsis = if ranges.len() > 8 { ", ..." } else { "" };
        lines.push(format!("RAM:    {total} bytes changed   ({range_str}{ellipsis})"));
        let sample = ram.get("sample").and_then(|s| s.as_array()).cloned().unwrap_or_default();
        for s in sample.iter().take(6) {
            lines.push(format!(
                "          ${}: ${} → ${}",
                hex4(as_i64(s, "addr")),
                hex2(as_i64(s, "before")),
                hex2(as_i64(s, "after"))
            ));
        }
        if sample.len() > 6 {
            lines.push(format!("          ... ({} more samples)", sample.len() - 6));
        }
    }

    // CPU
    let cpu = diff.get("cpu").cloned().unwrap_or(json!({}));
    let cpu_regs = cpu.get("changedRegs").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    let cyc_delta = as_i64(&cpu, "cyclesDelta");
    if cpu_regs.is_empty() && cyc_delta == 0 {
        lines.push("CPU:    no changes".to_string());
    } else {
        let reg_str = cpu_regs
            .iter()
            .map(|cr| {
                format!(
                    "{} ${}→${}",
                    cr.get("reg").and_then(|r| r.as_str()).unwrap_or(""),
                    hex2(as_i64(cr, "before")),
                    hex2(as_i64(cr, "after"))
                )
            })
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(format!("CPU:    {reg_str}  cycles +{cyc_delta}"));
    }

    lines.push(format_chip_diff("CIA1", &diff.get("cia1").cloned().unwrap_or(json!({}))));
    lines.push(format_chip_diff("CIA2", &diff.get("cia2").cloned().unwrap_or(json!({}))));
    lines.push(format_chip_diff("VIC ", &diff.get("vic").cloned().unwrap_or(json!({}))));
    lines.push(format_chip_diff("SID ", &diff.get("sid").cloned().unwrap_or(json!({}))));

    // PLA
    let pla = diff.get("pla").cloned().unwrap_or(json!({}));
    let before = pla.get("configBefore").and_then(|v| v.as_str()).unwrap_or("");
    let after = pla.get("configAfter").and_then(|v| v.as_str()).unwrap_or("");
    if before != after {
        lines.push(format!("PLA:    {before} → {after}"));
    } else {
        lines.push(format!("PLA:    {before} (unchanged)"));
    }

    // Drive
    if let Some(drive) = diff.get("drive").filter(|d| !d.is_null()) {
        lines.push(format_chip_diff("DRV/CPU", &drive.get("cpu").cloned().unwrap_or(json!({}))));
        lines.push(format_chip_diff("VIA1", &drive.get("via1").cloned().unwrap_or(json!({}))));
        lines.push(format_chip_diff("VIA2", &drive.get("via2").cloned().unwrap_or(json!({}))));
        let hp = drive.get("headPosition").cloned().unwrap_or(json!({}));
        let hb = as_i64(&hp, "trackHalfBefore");
        let ha = as_i64(&hp, "trackHalfAfter");
        if hb != ha {
            lines.push(format!("HEAD:   trackHalf {hb} → {ha}"));
        }
    }

    // IEC bus
    let iec = diff.get("iecBus").cloned().unwrap_or(json!({}));
    let fs = iec.get("finalState").cloned().unwrap_or(json!({}));
    lines.push(format!(
        "IEC:    {} edges  final ATN={} CLK={} DATA={}",
        as_i64(&iec, "edgesBetween"),
        as_i64(&fs, "atn"),
        as_i64(&fs, "clk"),
        as_i64(&fs, "data"),
    ));

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a minimal c64re-own VSF with the given module data.
    fn build_vsf(modules: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(VSF_MAGIC);
        buf.push(2); // major
        buf.push(0); // minor
        buf.extend_from_slice(b"C64\0"); // machine name (null-term)
        for (name, data) in modules {
            buf.extend_from_slice(name.as_bytes());
            buf.push(0); // name null-term
            buf.push(1); // module major
            buf.push(0); // module minor
            let len = data.len() as u32;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(data);
        }
        buf
    }

    fn cpu_module(pc: u16, a: u8, cycles: u32) -> Vec<u8> {
        let mut d = vec![(pc & 0xff) as u8, (pc >> 8) as u8, a, 0, 0, 0xff, 0x24];
        d.extend_from_slice(&cycles.to_le_bytes());
        d
    }

    fn c64mem_module(ram_mut: &[(usize, u8)], port_val: u8) -> Vec<u8> {
        let mut d = vec![0u8; 65550];
        for (addr, v) in ram_mut {
            d[*addr] = *v;
        }
        d[65536] = 0x2f; // dir
        d[65537] = port_val; // value
        d
    }

    #[test]
    fn ram_poke_one_byte() {
        let a = build_vsf(&[
            ("MAINCPU", cpu_module(0xe5cd, 0x00, 1000)),
            ("C64MEM", c64mem_module(&[(0xc000, 0x00)], 0x37)),
            ("DRIVECPU", vec![0u8; 32]),
        ]);
        let b = build_vsf(&[
            ("MAINCPU", cpu_module(0xe5cd, 0x00, 1000)),
            ("C64MEM", c64mem_module(&[(0xc000, 0x42)], 0x37)),
            ("DRIVECPU", vec![0u8; 32]),
        ]);
        let diff = diff_snapshots(&a, &b);
        // RAM: exactly one byte changed at $C000.
        assert_eq!(diff["ram"]["totalChanged"], json!(1));
        assert_eq!(diff["ram"]["changedRanges"][0]["start"], json!(0xc000));
        assert_eq!(diff["ram"]["changedRanges"][0]["byteCount"], json!(1));
        assert_eq!(diff["ram"]["sample"][0]["before"], json!(0));
        assert_eq!(diff["ram"]["sample"][0]["after"], json!(0x42));
        // CPU unchanged (same cycles → cyclesDelta 0, no changedRegs).
        assert_eq!(diff["cpu"]["cyclesDelta"], json!(0));
        assert!(diff["cpu"]["changedRegs"].as_array().unwrap().is_empty());
        // drive present (both have DRIVECPU), empty diffs.
        assert!(diff.get("drive").is_some());
        assert_eq!(diff["drive"]["headPosition"]["trackHalfBefore"], json!(0));
        // PLA unchanged.
        assert_eq!(diff["pla"]["configBefore"], diff["pla"]["configAfter"]);
        // formatDiff mentions the changed byte.
        let text = format_diff(&diff);
        assert!(text.contains("1 bytes changed"), "{text}");
        assert!(text.contains("$C000"), "{text}");
        assert!(text.contains("RAM:"), "{text}");
    }

    #[test]
    fn cpu_and_pla_change() {
        let a = build_vsf(&[
            ("MAINCPU", cpu_module(0x1000, 0x10, 100)),
            ("C64MEM", c64mem_module(&[], 0x37)),
        ]);
        let b = build_vsf(&[
            ("MAINCPU", cpu_module(0x2000, 0x20, 250)),
            ("C64MEM", c64mem_module(&[], 0x36)),
        ]);
        let diff = diff_snapshots(&a, &b);
        assert_eq!(diff["cpu"]["cyclesDelta"], json!(150));
        assert_eq!(diff["cpu"]["pcDelta"], json!(0x1000));
        let regs = diff["cpu"]["changedRegs"].as_array().unwrap();
        assert!(regs.iter().any(|r| r["reg"] == json!("pc")));
        assert!(regs.iter().any(|r| r["reg"] == json!("a")));
        // PLA: bit0 LORAM 1→0.
        assert_ne!(diff["pla"]["configBefore"], diff["pla"]["configAfter"]);
        // no DRIVECPU on either → drive omitted.
        assert!(diff.get("drive").is_none());
    }
}
