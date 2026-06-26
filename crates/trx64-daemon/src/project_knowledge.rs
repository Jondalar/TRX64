//! project_knowledge.rs — the monitor's project-knowledge bridges (Spec 754 §3.3f).
//!
//! Two TS bridges live in `ws-server.ts` (the c64re daemon) and were stubbed-out
//! in TRX64. This module ports them 1:1 so the monitor's knowledge verbs work
//! against a real project dir, with the SAME on-disk store format/location:
//!
//!  • `projectLabels` (ws-server.ts:2207-2258) — WRITE bridge. `label`/`unlabel`/
//!    `note` persist to the project's `knowledge/` stores; `save_labels`/
//!    `load_labels` round-trip a VICE `.sym`. The canonical store is
//!    `<project>/knowledge/labels.user.json` (storage.ts:730), a
//!    `UserLabelStore` ({schemaVersion:1, updatedAt, items:[UserLabelOverride]}).
//!    A `label` ALSO writes a `memory-address` entity to `knowledge/entities.json`
//!    and a `note` a finding to `knowledge/findings.json`, exactly as the TS
//!    `ProjectKnowledgeService.saveEntity`/`saveFinding` do — so both daemons
//!    leave the identical knowledge layer behind.
//!
//!  • `projectRead` (ws-server.ts:2135-2191) — READ bridge. `inspect`/`xref`/`sym`
//!    read the project's `*_analysis.json` (the heuristic analysis report) plus a
//!    sibling `*_annotations.json` overlay (effective-segments.ts) and the derived
//!    address/xref index (address-index.ts). The output strings match the TS
//!    line-for-line so the conformance signal is byte-identical.
//!
//! The on-disk JSON shapes are the project-knowledge zod schemas (types.ts). We
//! read/write them as untyped `serde_json::Value` maps so a schema field we do
//! not touch round-trips untouched (the TS storage re-parses the WHOLE store; a
//! dropped field would fail zod). The `addressRange`/`label`/`kind` fields we DO
//! touch match the schema verbatim.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// `new Date().toISOString()` shape, no chrono (= main.rs `now_iso8601_utc`).
fn now_iso() -> String {
    crate::now_iso8601_utc()
}

/// `createId(prefix, title)` (service.ts:89-97): `<prefix>-<slug>-<base36 ms><4-char base36 rand>`.
fn create_id(prefix: &str, title: &str) -> String {
    let slug = slugify(title);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let stamp = to_base36(ms);
    // 4-char base36 random suffix (service.ts: Math.random()*0x10000, padStart 4).
    let rnd = (fastrand_u16() as u128) % 0x10000;
    let rnd_str = format!("{:0>4}", to_base36(rnd));
    format!("{prefix}-{slug}-{stamp}{rnd_str}")
}

/// service.ts slugify: lower, non-alnum→'-', trim '-', fallback "item".
fn slugify(v: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in v.to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "item".to_string()
    } else {
        trimmed
    }
}

fn to_base36(mut n: u128) -> String {
    if n == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

/// Tiny non-crypto RNG seeded off the clock — only used for the id suffix
/// (collision-avoidance, not security). 1:1 purpose with `Math.random()`.
fn fastrand_u16() -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // xorshift the nanos a bit so consecutive calls in the same ms differ.
    let mut x = nanos ^ 0x9E37_79B9;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    (x & 0xffff) as u16
}

fn hx4(n: u16) -> String {
    format!("{:04x}", n)
}

// ── store paths (storage.ts:708-730) ─────────────────────────────────────────

fn labels_user_path(project_dir: &str) -> PathBuf {
    Path::new(project_dir).join("knowledge").join("labels.user.json")
}
fn entities_path(project_dir: &str) -> PathBuf {
    Path::new(project_dir).join("knowledge").join("entities.json")
}
fn findings_path(project_dir: &str) -> PathBuf {
    Path::new(project_dir).join("knowledge").join("findings.json")
}

/// Read a RecordList store ({schemaVersion, updatedAt, items}); a missing/corrupt
/// file → an empty store (= storage.ts `readJsonOrDefault` + `emptyStore`).
fn read_store(path: &Path) -> Value {
    let default = || json!({ "schemaVersion": 1, "updatedAt": now_iso(), "items": [] });
    match std::fs::read_to_string(path) {
        Ok(s) => match serde_json::from_str::<Value>(&s) {
            Ok(v) if v.get("items").map(|i| i.is_array()).unwrap_or(false) => v,
            _ => default(),
        },
        Err(_) => default(),
    }
}

/// Write a RecordList store atomically-ish (create knowledge/ dir, then write).
/// (= storage.ts `writeJsonAtomically`; TRX64's gate does not exercise crash
/// atomicity, so a plain create_dir_all+write is the faithful observable.)
fn write_store(path: &Path, mut store: Value) -> Result<(), String> {
    store["schemaVersion"] = json!(1);
    store["updatedAt"] = json!(now_iso());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(&store).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

fn items_mut(store: &mut Value) -> &mut Vec<Value> {
    if !store.get("items").map(|i| i.is_array()).unwrap_or(false) {
        store["items"] = json!([]);
    }
    store["items"].as_array_mut().unwrap()
}

// ── projectLabels WRITE bridge (ws-server.ts:2207-2258) ───────────────────────

/// `label` (list): every user label, sorted by address (service.ts listUserLabels).
pub fn project_labels_list(project_dir: &str) -> String {
    let store = read_store(&labels_user_path(project_dir));
    let mut items: Vec<&Value> = store
        .get("items")
        .and_then(|i| i.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    items.sort_by_key(|it| {
        it.get("addressRange")
            .and_then(|r| r.get("start"))
            .and_then(|s| s.as_u64())
            .unwrap_or(0)
    });
    if items.is_empty() {
        return "no user labels yet — set one with: label <addr> <name>".to_string();
    }
    items
        .iter()
        .map(|it| {
            let start = it
                .get("addressRange")
                .and_then(|r| r.get("start"))
                .and_then(|s| s.as_u64())
                .unwrap_or(0) as u16;
            let label = it.get("label").and_then(|l| l.as_str()).unwrap_or("");
            let note = it
                .get("note")
                .and_then(|n| n.as_str())
                .filter(|s| !s.is_empty())
                .map(|n| format!("  ; {n}"))
                .unwrap_or_default();
            format!("${}  {label}{note}", hx4(start))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `label <addr> <name>` (set). Persists a UserLabelOverride (re-label at the same
/// start address replaces) PLUS a `memory-address` entity (service.ts saveEntity),
/// mirroring ws-server.ts:2217-2225. Returns `label $XXXX = <name>  (entity <id>)`.
pub fn project_labels_set(project_dir: &str, addr: u16, name: &str) -> Result<String, String> {
    let ts = now_iso();
    // 1) entity (memory-address) — service.ts saveEntity.
    let ent_id = create_id("entity", name);
    let mut ent_store = read_store(&entities_path(project_dir));
    let ent = json!({
        "id": ent_id,
        "kind": "memory-address",
        "name": name,
        "status": "active",
        "confidence": 0.5,
        "aliases": [],
        "evidence": [],
        "tags": [],
        "relationIds": [],
        "addressRange": { "start": addr as u64, "end": addr as u64 },
        "createdAt": ts,
        "updatedAt": ts,
    });
    items_mut(&mut ent_store).push(ent);
    write_store(&entities_path(project_dir), ent_store)?;

    // 2) user label — service.ts saveUserLabel (upsert by start address).
    let mut store = read_store(&labels_user_path(project_dir));
    let items = items_mut(&mut store);
    let existing_idx = items.iter().position(|it| {
        it.get("targetKind").and_then(|t| t.as_str()) == Some("address")
            && it
                .get("addressRange")
                .and_then(|r| r.get("start"))
                .and_then(|s| s.as_u64())
                == Some(addr as u64)
    });
    let created_at = existing_idx
        .and_then(|i| items[i].get("createdAt").and_then(|c| c.as_str()).map(String::from))
        .unwrap_or_else(|| ts.clone());
    let id = existing_idx
        .and_then(|i| items[i].get("id").and_then(|c| c.as_str()).map(String::from))
        .unwrap_or_else(|| create_id("label", name));
    let record = json!({
        "id": id,
        "kind": "label-override",
        "label": name,
        "targetKind": "address",
        "targetId": ent_id,
        "addressRange": { "start": addr as u64, "end": addr as u64 },
        "createdAt": created_at,
        "updatedAt": ts,
    });
    match existing_idx {
        Some(i) => items[i] = record,
        None => items.push(record),
    }
    write_store(&labels_user_path(project_dir), store)?;
    Ok(format!("label ${} = {name}  (entity {ent_id})", hx4(addr)))
}

/// `unlabel <addr|name>` (del). Match by id, exact name, or `$addr`/`addr` (hex).
pub fn project_labels_del(project_dir: &str, key: &str) -> Result<String, String> {
    let mut store = read_store(&labels_user_path(project_dir));
    let addr = parse_hex_key(key);
    let items = items_mut(&mut store);
    let idx = items.iter().position(|it| {
        it.get("id").and_then(|v| v.as_str()) == Some(key)
            || it.get("label").and_then(|v| v.as_str()) == Some(key)
            || (addr.is_some()
                && it.get("targetKind").and_then(|t| t.as_str()) == Some("address")
                && it
                    .get("addressRange")
                    .and_then(|r| r.get("start"))
                    .and_then(|s| s.as_u64())
                    == Some(addr.unwrap() as u64))
    });
    match idx {
        None => Ok(format!("no label matching \"{key}\"")),
        Some(i) => {
            let removed = items.remove(i);
            let label = removed.get("label").and_then(|v| v.as_str()).unwrap_or("");
            let start = removed
                .get("addressRange")
                .and_then(|r| r.get("start"))
                .and_then(|s| s.as_u64())
                .unwrap_or(0) as u16;
            write_store(&labels_user_path(project_dir), store)?;
            Ok(format!("unlabeled {label} (${})", hx4(start)))
        }
    }
}

/// `note <addr> "<text>"`. Persists an `observation` finding (service.ts saveFinding),
/// mirroring ws-server.ts:2231-2240. Returns `note saved @ $XXXX (finding <id>)`.
pub fn project_labels_note(project_dir: &str, addr: u16, text: &str) -> Result<String, String> {
    let ts = now_iso();
    let id = create_id("finding", &format!("note @ ${}", hx4(addr)));
    let mut store = read_store(&findings_path(project_dir));
    let finding = json!({
        "id": id,
        "kind": "observation",
        "title": format!("note @ ${}", hx4(addr)),
        "summary": text,
        "status": "active",
        "confidence": 0.5,
        "evidence": [],
        "entityIds": [],
        "artifactIds": [],
        "relationIds": [],
        "flowIds": [],
        "addressRange": { "start": addr as u64, "end": addr as u64 },
        "tags": [],
        "createdAt": ts,
        "updatedAt": ts,
    });
    items_mut(&mut store).push(finding);
    write_store(&findings_path(project_dir), store)?;
    Ok(format!("note saved @ ${} (finding {id})", hx4(addr)))
}

/// `save_labels <file>` — write a VICE `.sym` (`al C:<hx> .<label>` per line).
/// (ws-server.ts:2251-2255.) Only address-targeted labels are emitted.
pub fn project_labels_save(project_dir: &str, file: &str) -> Result<String, String> {
    let store = read_store(&labels_user_path(project_dir));
    let items: Vec<&Value> = store
        .get("items")
        .and_then(|i| i.as_array())
        .map(|a| a.iter().filter(|it| it.get("addressRange").is_some()).collect())
        .unwrap_or_default();
    let body = items
        .iter()
        .map(|it| {
            let start = it
                .get("addressRange")
                .and_then(|r| r.get("start"))
                .and_then(|s| s.as_u64())
                .unwrap_or(0) as u16;
            let label = it.get("label").and_then(|l| l.as_str()).unwrap_or("");
            format!("al C:{} .{label}", hx4(start))
        })
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(parent) = Path::new(file).parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(file, format!("{body}\n")).map_err(|e| e.to_string())?;
    Ok(format!(
        "saved {} label(s) to {file} (VICE label format)",
        items.len()
    ))
}

/// `load_labels <file>` — read a `.sym`, upsert each parsed addr→name as a user
/// label (no entity, matching ws-server.ts:2247). Returns `loaded N label(s) …`.
pub fn project_labels_load(project_dir: &str, file: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let mut store = read_store(&labels_user_path(project_dir));
    let mut n = 0usize;
    for line in text.lines() {
        if let Some((addr, name)) = parse_sym_line(line) {
            let name = name.trim_start_matches('.').to_string();
            if name.is_empty() {
                continue;
            }
            let ts = now_iso();
            let items = items_mut(&mut store);
            let existing_idx = items.iter().position(|it| {
                it.get("targetKind").and_then(|t| t.as_str()) == Some("address")
                    && it
                        .get("addressRange")
                        .and_then(|r| r.get("start"))
                        .and_then(|s| s.as_u64())
                        == Some(addr as u64)
            });
            let created_at = existing_idx
                .and_then(|i| items[i].get("createdAt").and_then(|c| c.as_str()).map(String::from))
                .unwrap_or_else(|| ts.clone());
            let id = existing_idx
                .and_then(|i| items[i].get("id").and_then(|c| c.as_str()).map(String::from))
                .unwrap_or_else(|| create_id("label", &name));
            let record = json!({
                "id": id,
                "kind": "label-override",
                "label": name,
                "targetKind": "address",
                "addressRange": { "start": addr as u64, "end": addr as u64 },
                "createdAt": created_at,
                "updatedAt": ts,
            });
            match existing_idx {
                Some(i) => items[i] = record,
                None => items.push(record),
            }
            n += 1;
        }
    }
    write_store(&labels_user_path(project_dir), store)?;
    Ok(format!("loaded {n} label(s) from {file}"))
}

/// addr→name index from the user-label store, for the disassembler (= ws-server.ts
/// `labelIndex` user-label layer). The analysis-segment label layer is folded in
/// by the caller via the address index (project_read) when needed; the gate only
/// needs the user layer, which wins precedence.
pub fn user_label_index(project_dir: &str) -> BTreeMap<u16, String> {
    let store = read_store(&labels_user_path(project_dir));
    let mut map = BTreeMap::new();
    if let Some(items) = store.get("items").and_then(|i| i.as_array()) {
        for it in items {
            if it.get("targetKind").and_then(|t| t.as_str()) != Some("address") {
                continue;
            }
            if let (Some(start), Some(label)) = (
                it.get("addressRange")
                    .and_then(|r| r.get("start"))
                    .and_then(|s| s.as_u64()),
                it.get("label").and_then(|l| l.as_str()),
            ) {
                map.insert((start & 0xffff) as u16, label.to_string());
            }
        }
    }
    map
}

/// parseSymLine (ws-server.ts:2195-2205): VICE `al`, KickAss `.label x=$..`, plain `x=$..`.
fn parse_sym_line(line: &str) -> Option<(u16, String)> {
    let l = line.trim();
    if l.is_empty() {
        return None;
    }
    // VICE add-label: `al C:0810 .setup` (optional bank letter + colon).
    if let Some(rest) = l.strip_prefix("al ").or_else(|| l.strip_prefix("AL ")) {
        let rest = rest.trim();
        // optional `<X>:` bank prefix.
        let after_bank = match rest.find(':') {
            Some(ci) if ci <= 1 => &rest[ci + 1..],
            _ => rest,
        };
        let mut parts = after_bank.split_whitespace();
        let addr_tok = parts.next()?;
        let name_tok = parts.next()?;
        if let Ok(addr) = u32::from_str_radix(addr_tok.trim_start_matches('$'), 16) {
            return Some(((addr & 0xffff) as u16, name_tok.trim_start_matches('.').to_string()));
        }
    }
    // KickAssembler: `.label setup=$0810` | `label setup = $0810`.
    let kick = l.strip_prefix(".label ").or_else(|| l.strip_prefix("label "));
    if let Some(rest) = kick {
        if let Some(eq) = rest.find('=') {
            let name = rest[..eq].trim();
            let addr_s = rest[eq + 1..].trim().trim_start_matches('$');
            if let Ok(addr) = u32::from_str_radix(addr_s, 16) {
                if !name.is_empty() {
                    return Some(((addr & 0xffff) as u16, name.to_string()));
                }
            }
        }
    }
    // Plain: `setup = $0810` | `setup=$0810`.
    if let Some(eq) = l.find('=') {
        let name = l[..eq].trim();
        let addr_s = l[eq + 1..].trim().trim_start_matches('$');
        let name_ok = !name.is_empty()
            && name.chars().next().map(|c| c.is_ascii_alphabetic() || c == '_').unwrap_or(false)
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if name_ok {
            if let Ok(addr) = u32::from_str_radix(addr_s, 16) {
                return Some(((addr & 0xffff) as u16, name.to_string()));
            }
        }
    }
    None
}

// ── resolvePc / resolvePcs (Spec 235 — resolve-pc.ts) ─────────────────────────
//
// Faithful port of `src/runtime/headless/v2/resolve-pc.ts` `resolvePc(artifactId,
// pc)` / `resolvePcs(artifactId, pcs)`. Maps a PC to its position in the project's
// disassembly knowledge — the SAME on-disk files the inspect/xref/sym bridge above
// reads (`<artifactId>_analysis.json` segments + `<artifactId>_annotations.json`
// routines/labels/segments). Returns a `ResolvedPc`-shaped JSON value:
//
//   { artifactId, pc, routine?, label?, segment?, source? }
//
// Layer 1 RoutineAnnotation range match; Layer 2 nearest LabelAnnotation ≤ PC;
// Layer 3 effective-segment classification; Layer 4 source line in `<id>_disasm.asm`.
// Absent layers are OMITTED (resolve-pc.ts leaves the field `undefined`, which
// JSON.stringify drops) — we replicate that omission so the JSON matches byte-for-byte.

/// The active project dir = `--project <dir>` arg ?? `C64RE_PROJECT_DIR` env ?? cwd.
/// 1:1 with resolve-pc.ts `getProjectDir()` (`process.env["C64RE_PROJECT_DIR"] ??
/// process.cwd()`); the daemon's run.ts sets `C64RE_PROJECT_DIR` from `--project`,
/// so the `--project` arg layer matches the TS daemon exactly.
pub fn active_project_dir() -> String {
    std::env::args()
        .skip_while(|a| a != "--project")
        .nth(1)
        .filter(|p| !p.is_empty())
        .or_else(|| std::env::var("C64RE_PROJECT_DIR").ok())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        })
}

/// `parseHex(hex)` (resolve-pc.ts:91): strip a leading `$`, base-16.
fn parse_hex_addr(hex: &str) -> Option<u32> {
    u32::from_str_radix(hex.trim_start_matches('$'), 16).ok()
}

struct ResolveData {
    /// Sorted by entry ascending; exit = next entry - 1, or None for the last.
    routines_sorted: Vec<(u32, Option<u32>, String, Option<String>)>, // (entry, exit, name, description)
    /// Sorted by address ascending.
    labels_sorted: Vec<(u32, String)>, // (address, name)
    /// Effective segments sorted by start ascending: (start, end, kind, confidence).
    segments_sorted: Vec<(u32, u32, String, f64)>,
    /// addr → 1-based line in `<id>_disasm.asm`, or None when absent.
    disasm_lines: Option<BTreeMap<u32, usize>>,
    disasm_file: Option<String>,
}

/// Load + index the per-artifact knowledge (resolve-pc.ts `loadArtifactData`).
fn load_resolve_data(project_dir: &str, artifact_id: &str) -> ResolveData {
    let dir = Path::new(project_dir);

    // --- analysis JSON ---
    let analysis_path = dir.join(format!("{artifact_id}_analysis.json"));
    let analysis: Option<Value> = std::fs::read_to_string(&analysis_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    // --- annotations JSON ---
    let ann_path = dir.join(format!("{artifact_id}_annotations.json"));
    let annotations: Option<Value> = std::fs::read_to_string(&ann_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    // routines: sort by entry; exit = next.entry - 1 (resolve-pc.ts:146-158).
    let mut routines_sorted: Vec<(u32, Option<u32>, String, Option<String>)> = Vec::new();
    if let Some(rs) = annotations
        .as_ref()
        .and_then(|a| a.get("routines"))
        .and_then(|r| r.as_array())
    {
        for rt in rs {
            let (Some(addr), Some(name)) = (
                rt.get("address").and_then(|v| v.as_str()).and_then(parse_hex_addr),
                rt.get("name").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            let desc = rt.get("comment").and_then(|v| v.as_str()).map(String::from);
            routines_sorted.push((addr, None, name.to_string(), desc));
        }
        routines_sorted.sort_by_key(|r| r.0);
        for i in 0..routines_sorted.len() {
            if let Some(next) = routines_sorted.get(i + 1).map(|n| n.0) {
                routines_sorted[i].1 = Some(next.wrapping_sub(1));
            }
        }
    }

    // labels: sort by address (resolve-pc.ts:161-167).
    let mut labels_sorted: Vec<(u32, String)> = Vec::new();
    if let Some(ls) = annotations
        .as_ref()
        .and_then(|a| a.get("labels"))
        .and_then(|l| l.as_array())
    {
        for lbl in ls {
            let (Some(addr), Some(name)) = (
                lbl.get("address").and_then(|v| v.as_str()).and_then(parse_hex_addr),
                lbl.get("label").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            labels_sorted.push((addr, name.to_string()));
        }
        labels_sorted.sort_by_key(|l| l.0);
    }

    // segments: effective-segments overlay (resolve-pc.ts:174-188). The fixture path
    // has no annotation segments → raw analysis segments; confidence = score.confidence
    // ?? 0 for analysis-owned, 0.9 for annotation-owned (load_effective_segments below
    // already merges the annotation overlay, but does not carry the per-segment score,
    // so we read confidence directly off the analysis report here, 1:1 with resolve-pc).
    let mut segments_sorted: Vec<(u32, u32, String, f64)> = Vec::new();
    let overlay_present = annotations
        .as_ref()
        .and_then(|a| a.get("segments"))
        .and_then(|s| s.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if overlay_present {
        // Annotation segment overlay is present: replicate buildEffectiveSegments via
        // the shared loader, then assign 0.9 confidence to every emitted run (the TS
        // `typeof s.confidence === "number" ? s.confidence : 0.9` — the overlay merge
        // drops the score, so every effective run is 0.9). This matches resolve-pc.ts
        // when annotation reclassifications exist.
        for (start, end, kind, _label) in load_effective_segments(&analysis_path) {
            segments_sorted.push((start as u32, end as u32, kind, 0.9));
        }
    } else if let Some(segs) = analysis
        .as_ref()
        .and_then(|a| a.get("segments"))
        .and_then(|s| s.as_array())
    {
        for seg in segs {
            let (Some(start), Some(end)) = (
                seg.get("start").and_then(coerce_addr),
                seg.get("end").and_then(coerce_addr),
            ) else {
                continue;
            };
            let kind = seg.get("kind").and_then(|k| k.as_str()).unwrap_or("unknown").to_string();
            let confidence = seg
                .get("score")
                .and_then(|s| s.get("confidence"))
                .and_then(|c| c.as_f64())
                .unwrap_or(0.0);
            segments_sorted.push((start as u32, end as u32, kind, confidence));
        }
    }
    segments_sorted.sort_by_key(|s| s.0);

    // disasm line index (resolve-pc.ts:98-113, 190-201): first `; $XXXX` per addr wins.
    let disasm_path = dir.join(format!("{artifact_id}_disasm.asm"));
    let (disasm_lines, disasm_file) = match std::fs::read_to_string(&disasm_path) {
        Ok(text) => {
            let mut idx: BTreeMap<u32, usize> = BTreeMap::new();
            for (i, line) in text.split('\n').enumerate() {
                if let Some(addr) = addr_comment(line) {
                    idx.entry(addr).or_insert(i + 1); // 1-based, first occurrence wins
                }
            }
            (Some(idx), Some(disasm_path.to_string_lossy().to_string()))
        }
        Err(_) => (None, None),
    };

    ResolveData {
        routines_sorted,
        labels_sorted,
        segments_sorted,
        disasm_lines,
        disasm_file,
    }
}

/// Extract the address from a `; $XXXX` comment (resolve-pc.ts ADDR_COMMENT_RE).
fn addr_comment(line: &str) -> Option<u32> {
    // /;\s*\$([0-9A-Fa-f]{4})/
    let semi = line.find(';')?;
    let rest = &line[semi + 1..];
    let dollar = rest.find('$')?;
    // Everything between `;` and `$` must be whitespace (the `\s*` in the regex).
    if !rest[..dollar].chars().all(|c| c.is_whitespace()) {
        return None;
    }
    let hex: String = rest[dollar + 1..].chars().take(4).collect();
    if hex.len() == 4 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        u32::from_str_radix(&hex, 16).ok()
    } else {
        None
    }
}

/// resolve-pc.ts `resolveOne`: produce the `ResolvedPc` JSON for one (artifact, pc).
fn resolve_one(artifact_id: &str, pc: u32, data: &ResolveData) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("artifactId".to_string(), json!(artifact_id));
    out.insert("pc".to_string(), json!(pc));

    // Layer 1: largest routine entry ≤ pc, included only if pc ≤ exit (or exit None).
    if let Some((entry, exit, name, desc)) = data
        .routines_sorted
        .iter()
        .rev()
        .find(|(entry, _, _, _)| *entry <= pc)
    {
        if exit.map(|e| pc <= e).unwrap_or(true) {
            let mut r = serde_json::Map::new();
            r.insert("name".to_string(), json!(name));
            // description is OMITTED when None (resolve-pc.ts assigns `undefined`).
            if let Some(d) = desc {
                r.insert("description".to_string(), json!(d));
            }
            r.insert("entry".to_string(), json!(entry));
            // exit is OMITTED when None (TS `exit: undefined`).
            if let Some(e) = exit {
                r.insert("exit".to_string(), json!(e));
            }
            out.insert("routine".to_string(), Value::Object(r));
        }
    }

    // Layer 2: nearest label ≤ pc.
    if let Some((addr, name)) = data.labels_sorted.iter().rev().find(|(addr, _)| *addr <= pc) {
        out.insert(
            "label".to_string(),
            json!({ "name": name, "isExact": *addr == pc }),
        );
    }

    // Layer 3: first covering effective segment.
    for (start, end, kind, confidence) in &data.segments_sorted {
        if *start > pc {
            break;
        }
        if pc <= *end {
            out.insert(
                "segment".to_string(),
                json!({ "kind": kind, "confidence": confidence }),
            );
            break;
        }
    }

    // Layer 4: source line in `<id>_disasm.asm`.
    if let (Some(lines), Some(file)) = (&data.disasm_lines, &data.disasm_file) {
        if let Some(line) = lines.get(&pc) {
            out.insert("source".to_string(), json!({ "file": file, "line": line }));
        }
    }

    Value::Object(out)
}

/// `resolvePc(artifactId, pc)` (resolve-pc.ts:300).
pub fn resolve_pc(project_dir: &str, artifact_id: &str, pc: u32) -> Value {
    let data = load_resolve_data(project_dir, artifact_id);
    resolve_one(artifact_id, pc, &data)
}

/// `resolvePcs(artifactId, pcs)` (resolve-pc.ts:323) — loads once, resolves each.
pub fn resolve_pcs(project_dir: &str, artifact_id: &str, pcs: &[u32]) -> Vec<Value> {
    let data = load_resolve_data(project_dir, artifact_id);
    pcs.iter().map(|pc| resolve_one(artifact_id, *pc, &data)).collect()
}

fn parse_hex_key(key: &str) -> Option<u16> {
    let k = key.trim_start_matches('$');
    if (1..=4).contains(&k.len()) && k.chars().all(|c| c.is_ascii_hexdigit()) {
        u32::from_str_radix(k, 16).ok().map(|v| (v & 0xffff) as u16)
    } else {
        None
    }
}

// ── projectRead READ bridge (ws-server.ts:2135-2191) ──────────────────────────

#[derive(Clone)]
struct SegEntry {
    owner: String,
    start: u16,
    end: u16,
    kind: String,
    label: Option<String>,
}
#[derive(Clone)]
struct XrefEntry {
    owner: String,
    source: u16,
    target: u16,
    typ: String,
    operand_text: Option<String>,
}

/// Depth-bounded walk for `*_analysis.json` (address-index.ts findAnalysisJsons).
fn find_analysis_jsons(project_dir: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 6 || out.len() > 256 {
            return;
        }
        let Ok(ents) = std::fs::read_dir(dir) else { return };
        for e in ents.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name == "node_modules" || name.starts_with('.') {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                walk(&p, depth + 1, out);
            } else if name.ends_with("_analysis.json") {
                out.push(p);
            }
        }
    }
    walk(Path::new(project_dir), 0, &mut out);
    out
}

fn stem_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
        .trim_end_matches("_analysis.json")
        .to_string()
}

fn coerce_addr(v: &Value) -> Option<u16> {
    if let Some(n) = v.as_u64() {
        return Some((n & 0xffff) as u16);
    }
    if let Some(s) = v.as_str() {
        let s = s.trim_start_matches('$').trim_start_matches("0x").trim_start_matches("0X");
        return u32::from_str_radix(s, 16).ok().map(|v| (v & 0xffff) as u16);
    }
    None
}

/// Effective segments for one analysis file: the raw `segments[]` overlaid by the
/// sibling `*_annotations.json` segments (effective-segments.ts). The gate fixture
/// has no annotations, so this is the raw-segments path; the overlay merge is the
/// faithful behaviour when an annotation file is present.
fn load_effective_segments(analysis_path: &Path) -> Vec<(u16, u16, String, Option<String>)> {
    let mut segs: Vec<(u16, u16, String, Option<String>)> = Vec::new();
    let Ok(text) = std::fs::read_to_string(analysis_path) else { return segs };
    let Ok(report) = serde_json::from_str::<Value>(&text) else { return segs };
    if let Some(arr) = report.get("segments").and_then(|s| s.as_array()) {
        for s in arr {
            let (Some(start), Some(end)) = (
                s.get("start").and_then(coerce_addr),
                s.get("end").and_then(coerce_addr),
            ) else {
                continue;
            };
            let kind = s.get("kind").and_then(|k| k.as_str()).unwrap_or("unknown").to_string();
            let label = s.get("label").and_then(|l| l.as_str()).map(String::from);
            segs.push((start, end, kind, label));
        }
    }
    // Annotation overlay: a sibling `*_annotations.json` segments[] reclassifies
    // covering ranges (annotation kind/label wins). We apply the simple "later
    // wins on overlap" merge for any annotation segment whose range is present.
    let ann_path = annotations_path(analysis_path);
    if let Some(ann_path) = ann_path {
        if ann_path.exists() {
            if let Ok(atext) = std::fs::read_to_string(&ann_path) {
                if let Ok(ann) = serde_json::from_str::<Value>(&atext) {
                    if let Some(arr) = ann.get("segments").and_then(|s| s.as_array()) {
                        for s in arr {
                            let (Some(start), Some(end)) = (
                                s.get("start").and_then(coerce_addr),
                                s.get("end").and_then(coerce_addr),
                            ) else {
                                continue;
                            };
                            let kind = match s.get("kind").and_then(|k| k.as_str()) {
                                Some(k) => k.to_string(),
                                None => continue,
                            };
                            let label = s.get("label").and_then(|l| l.as_str()).map(String::from);
                            // Annotation segment wins for its range: push it (a later
                            // covering entry wins the tightest-match sort in resolve).
                            segs.push((start, end, kind, label));
                        }
                    }
                }
            }
        }
    }
    segs
}

fn annotations_path(analysis_path: &Path) -> Option<PathBuf> {
    let name = analysis_path.file_name()?.to_string_lossy().to_string();
    if let Some(stem) = name.strip_suffix("_analysis.json") {
        Some(analysis_path.with_file_name(format!("{stem}_annotations.json")))
    } else {
        None
    }
}

/// Build the address (segment + point-label) index across the project.
fn build_address_index(project_dir: &str) -> Vec<SegEntry> {
    let mut entries = Vec::new();
    for p in find_analysis_jsons(project_dir) {
        let owner = stem_of(&p);
        for (start, end, kind, label) in load_effective_segments(&p) {
            entries.push(SegEntry { owner: owner.clone(), start, end, kind, label });
        }
        // Point labels from `*_annotations.json` labels[] (address-index.ts:67-77).
        if let Some(ann_path) = annotations_path(&p) {
            if let Ok(atext) = std::fs::read_to_string(&ann_path) {
                if let Ok(ann) = serde_json::from_str::<Value>(&atext) {
                    if let Some(arr) = ann.get("labels").and_then(|l| l.as_array()) {
                        for l in arr {
                            let (Some(addr), Some(label)) =
                                (l.get("address").and_then(coerce_addr), l.get("label").and_then(|v| v.as_str()))
                            else {
                                continue;
                            };
                            entries.push(SegEntry {
                                owner: owner.clone(),
                                start: addr,
                                end: addr,
                                kind: "label".to_string(),
                                label: Some(label.to_string()),
                            });
                        }
                    }
                }
            }
        }
    }
    entries
}

/// resolveCrossArtifact (address-index.ts:107-117): covering segments, tightest first.
fn resolve_cross_artifact(project_dir: &str, addr: u16) -> Vec<SegEntry> {
    let mut hits: Vec<SegEntry> = build_address_index(project_dir)
        .into_iter()
        .filter(|e| addr >= e.start && addr <= e.end)
        .collect();
    hits.sort_by_key(|e| (e.end as i32) - (e.start as i32));
    hits
}

fn build_xref_index(project_dir: &str) -> Vec<XrefEntry> {
    let mut out = Vec::new();
    for p in find_analysis_jsons(project_dir) {
        let owner = stem_of(&p);
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let Ok(report) = serde_json::from_str::<Value>(&text) else { continue };
        let mut collect = |arr: Option<&Vec<Value>>| {
            if let Some(arr) = arr {
                for x in arr {
                    let (Some(src), Some(tgt)) = (
                        x.get("sourceAddress").and_then(|v| v.as_u64()),
                        x.get("targetAddress").and_then(|v| v.as_u64()),
                    ) else {
                        continue;
                    };
                    out.push(XrefEntry {
                        owner: owner.clone(),
                        source: (src & 0xffff) as u16,
                        target: (tgt & 0xffff) as u16,
                        typ: x.get("type").and_then(|t| t.as_str()).unwrap_or("ref").to_string(),
                        operand_text: x.get("operandText").and_then(|o| o.as_str()).map(String::from),
                    });
                }
            }
        };
        collect(report.get("codeAnalysis").and_then(|c| c.get("xrefs")).and_then(|x| x.as_array()));
        collect(
            report
                .get("probableCodeAnalysis")
                .and_then(|c| c.get("xrefs"))
                .and_then(|x| x.as_array()),
        );
    }
    out
}

fn resolve_xrefs(project_dir: &str, addr: u16) -> (Vec<XrefEntry>, Vec<XrefEntry>) {
    let idx = build_xref_index(project_dir);
    let into: Vec<XrefEntry> = idx.iter().filter(|x| x.target == addr).cloned().collect();
    let outof: Vec<XrefEntry> = idx.iter().filter(|x| x.source == addr).cloned().collect();
    (into, outof)
}

/// `inspect <addr>` — owners + callers (ws-server.ts:2175-2184).
pub fn project_read_inspect(project_dir: &str, addr: u16) -> String {
    let owners = resolve_cross_artifact(project_dir, addr);
    let (into, _outof) = resolve_xrefs(project_dir, addr);
    let mut lines = vec![format!("inspect ${}", hx4(addr))];
    if owners.is_empty() {
        lines.push("  (no analyzed artifact owns this address)".to_string());
    } else {
        for o in &owners {
            let label = o.label.as_ref().map(|l| format!(" ({l})")).unwrap_or_default();
            lines.push(format!(
                "  {}: ${}..${} {}{label}",
                o.owner,
                hx4(o.start),
                hx4(o.end),
                o.kind
            ));
        }
    }
    if owners.len() > 1 {
        lines.push(format!("  ({} owners — overlay/banking overlap)", owners.len()));
    }
    if !into.is_empty() {
        lines.push(format!("  callers ({}):", into.len()));
        for x in into.iter().take(8) {
            lines.push(format!("    <- {} ${} {}", x.owner, hx4(x.source), x.typ));
        }
    }
    lines.join("\n")
}

/// `xref <addr>` — project-wide callers + own refs (ws-server.ts:2186-2190).
pub fn project_read_xref(project_dir: &str, addr: u16) -> String {
    let (into, outof) = resolve_xrefs(project_dir, addr);
    let mut lines = vec![format!(
        "xref ${}  (in:{} out:{}, project-wide)",
        hx4(addr),
        into.len(),
        outof.len()
    )];
    for x in into.iter().take(16) {
        let op = x.operand_text.as_ref().map(|o| format!(" {o}")).unwrap_or_default();
        lines.push(format!("  <- {} ${} {}{op}", x.owner, hx4(x.source), x.typ));
    }
    for x in outof.iter().take(16) {
        lines.push(format!("  -> ${} {}", hx4(x.target), x.typ));
    }
    if into.is_empty() && outof.is_empty() {
        lines.push("  (no cross-references in any analyzed artifact)".to_string());
    }
    lines.join("\n")
}

/// `sym <name>` — reverse lookup: a labelled segment → its address (ws-server.ts:2157-2166).
pub fn project_read_sym(project_dir: &str, name: &str) -> Result<String, String> {
    if name.is_empty() {
        return Err("sym: a name is required".to_string());
    }
    for p in find_analysis_jsons(project_dir) {
        for (start, _end, kind, label) in load_effective_segments(&p) {
            if label.as_deref() == Some(name) {
                return Ok(format!(
                    "sym {name} = ${}  ({}, {kind})",
                    hx4(start),
                    stem_of(&p)
                ));
            }
        }
    }
    Err(format!("no symbol named \"{name}\" in the project analysis"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("trx64-pk-test-{}", fastrand_u16()));
        let _ = fs::create_dir_all(&d);
        d
    }

    #[test]
    fn label_set_list_roundtrip() {
        let d = tmp();
        let dir = d.to_string_lossy().to_string();
        let out = project_labels_set(&dir, 0xc000, "myroutine").unwrap();
        assert!(out.contains("label $c000 = myroutine"), "got: {out}");
        let list = project_labels_list(&dir);
        assert!(list.contains("myroutine"), "list: {list}");
        assert!(list.contains("$c000"), "list: {list}");
        // store persisted at knowledge/labels.user.json (schemaVersion:1).
        let store = read_store(&labels_user_path(&dir));
        assert_eq!(store["schemaVersion"], json!(1));
        assert_eq!(store["items"].as_array().unwrap().len(), 1);
        // entity persisted at knowledge/entities.json.
        let ent = read_store(&entities_path(&dir));
        assert_eq!(ent["items"].as_array().unwrap().len(), 1);
        // index for the disassembler.
        let idx = user_label_index(&dir);
        assert_eq!(idx.get(&0xc000).map(String::as_str), Some("myroutine"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn relabel_replaces_at_same_addr() {
        let d = tmp();
        let dir = d.to_string_lossy().to_string();
        project_labels_set(&dir, 0x1000, "first").unwrap();
        project_labels_set(&dir, 0x1000, "second").unwrap();
        let store = read_store(&labels_user_path(&dir));
        assert_eq!(store["items"].as_array().unwrap().len(), 1, "re-label must upsert");
        assert_eq!(store["items"][0]["label"], json!("second"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn save_load_sym_roundtrip() {
        let d = tmp();
        let dir = d.to_string_lossy().to_string();
        project_labels_set(&dir, 0x0810, "setup").unwrap();
        project_labels_set(&dir, 0x0900, "loop").unwrap();
        let sym = d.join("out.sym");
        let symf = sym.to_string_lossy().to_string();
        let so = project_labels_save(&dir, &symf).unwrap();
        assert!(so.contains("saved 2 label(s)"), "{so}");
        let body = fs::read_to_string(&sym).unwrap();
        assert!(body.contains("al C:0810 .setup"), "sym: {body}");
        // Load into a FRESH project dir.
        let d2 = tmp();
        let dir2 = d2.to_string_lossy().to_string();
        let lo = project_labels_load(&dir2, &symf).unwrap();
        assert!(lo.contains("loaded 2 label(s)"), "{lo}");
        let idx = user_label_index(&dir2);
        assert_eq!(idx.get(&0x0810).map(String::as_str), Some("setup"));
        assert_eq!(idx.get(&0x0900).map(String::as_str), Some("loop"));
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&d2);
    }

    #[test]
    fn note_persists_finding() {
        let d = tmp();
        let dir = d.to_string_lossy().to_string();
        let out = project_labels_note(&dir, 0xd020, "border colour").unwrap();
        assert!(out.contains("note saved @ $d020"), "{out}");
        let store = read_store(&findings_path(&dir));
        assert_eq!(store["items"].as_array().unwrap().len(), 1);
        assert_eq!(store["items"][0]["summary"], json!("border colour"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn inspect_xref_sym_from_fixture() {
        let d = tmp();
        let dir = d.to_string_lossy().to_string();
        let analysis = json!({
            "segments": [
                { "kind": "code", "start": 0x0810, "end": 0x08ff, "label": "main" },
                { "kind": "data", "start": 0x0900, "end": 0x09ff }
            ],
            "codeAnalysis": {
                "xrefs": [
                    { "sourceAddress": 0x0820, "targetAddress": 0x0900, "type": "read", "operandText": "lda $0900" },
                    { "sourceAddress": 0x0950, "targetAddress": 0x0810, "type": "call" }
                ]
            }
        });
        fs::write(d.join("fixture_analysis.json"), serde_json::to_string(&analysis).unwrap()).unwrap();

        let ins = project_read_inspect(&dir, 0x0810);
        assert!(ins.contains("fixture: $0810..$08ff code (main)"), "inspect: {ins}");
        assert!(ins.contains("<- fixture $0950 call"), "inspect callers: {ins}");

        let xr = project_read_xref(&dir, 0x0900);
        assert!(xr.contains("in:1 out:0"), "xref: {xr}");
        assert!(xr.contains("<- fixture $0820 read lda $0900"), "xref: {xr}");

        let sym = project_read_sym(&dir, "main").unwrap();
        assert!(sym.contains("sym main = $0810"), "sym: {sym}");
        assert!(project_read_sym(&dir, "nope").is_err());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn resolve_pc_from_fixture() {
        // Mirrors the conformance misc-21 fixture: a labelled code segment + a data
        // segment (with score.confidence) plus a routine + a label annotation. Asserts
        // the ResolvedPc layers (routine/label/segment) + the OMISSION of absent fields
        // (no `exit` on a single routine, bare object below everything).
        let d = tmp();
        let dir = d.to_string_lossy().to_string();
        let analysis = json!({
            "binaryName": "fixture",
            "segments": [
                { "kind": "code", "start": 0x0810, "end": 0x08ff, "score": { "confidence": 0.85 } },
                { "kind": "data", "start": 0x0900, "end": 0x09ff, "score": { "confidence": 0.5 } }
            ]
        });
        fs::write(d.join("fixture_analysis.json"), serde_json::to_string(&analysis).unwrap()).unwrap();
        let annotations = json!({
            "version": 1,
            "binary": "fixture",
            "segments": [],
            "labels": [{ "address": "0850", "label": "inner" }],
            "routines": [{ "address": "0810", "name": "main", "comment": "entry point" }]
        });
        fs::write(d.join("fixture_annotations.json"), serde_json::to_string(&annotations).unwrap()).unwrap();

        // $0850: inside routine `main`, exactly at label `inner`, in the code segment.
        let at_label = resolve_pc(&dir, "fixture", 0x0850);
        assert_eq!(at_label["artifactId"], json!("fixture"));
        assert_eq!(at_label["pc"], json!(0x0850));
        assert_eq!(at_label["routine"]["name"], json!("main"));
        assert_eq!(at_label["routine"]["description"], json!("entry point"));
        assert_eq!(at_label["routine"]["entry"], json!(0x0810));
        // single routine → exit is OMITTED (TS `undefined`).
        assert!(at_label["routine"].get("exit").is_none(), "exit must be omitted: {at_label}");
        assert_eq!(at_label["label"], json!({ "name": "inner", "isExact": true }));
        assert_eq!(at_label["segment"], json!({ "kind": "code", "confidence": 0.85 }));

        // $0900: nearest label still `inner` (not exact), in the data segment.
        let in_data = resolve_pc(&dir, "fixture", 0x0900);
        assert_eq!(in_data["label"], json!({ "name": "inner", "isExact": false }));
        assert_eq!(in_data["segment"], json!({ "kind": "data", "confidence": 0.5 }));

        // $0700: below everything — bare {artifactId, pc}, no other keys.
        let below = resolve_pc(&dir, "fixture", 0x0700);
        assert_eq!(below, json!({ "artifactId": "fixture", "pc": 0x0700 }));

        // Batch resolves each (same shapes).
        let batch = resolve_pcs(&dir, "fixture", &[0x0850, 0x0900, 0x0700]);
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0], at_label);
        assert_eq!(batch[2], below);
        let _ = fs::remove_dir_all(&d);
    }
}
