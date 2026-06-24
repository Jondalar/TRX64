//! vic_inspect.rs — Spec 710/721: checkpoint-bound VIC inspect resolver + asset
//! origin join.
//!
//! 1:1 PORT of the c64re TS engine:
//!   C64ReverseEngineeringMCP/src/runtime/headless/inspect/
//!     vic-inspect.ts        (buildVicInspectSnapshot / resolveNodeAt /
//!                            spriteBoundsAtVisible / resolveVisibleNodeAt /
//!                            resolveVisibleRegion / resolveRegion /
//!                            assembleInspectEvidence + geometry consts)
//!     vic-inspect-types.ts  (MemoryRef / VisualNode / VicInspectSnapshot /
//!                            VicFrameProvenance / FrozenInspectEvidence)
//!     asset-extract.ts      (extractAssetCandidates — sprite/charset/bitmap)
//!     asset-join.ts         (matchVisualNodeToAsset / hashRamRange / dataRefOf)
//!     asset-join-knowledge.ts (assetJoinToKnowledge)
//!     asset-origin.ts       (resolveVisualOrigin)
//!
//! PURE over a frozen RuntimeCheckpoint `serde_json::Value` (the ring's stored
//! payload tree, ADR-077/078). NEVER advances execution — it only reads the
//! checkpoint state (regs, RAM, color RAM, sprites). The literal `viciisc`
//! checkpoint is the visual authority; the `VicIIVice` model is NOT consulted
//! (Spec 710 §2.1). The TS reads typed `RuntimeCheckpoint` fields; here we read
//! the SAME fields off the JSON tree the checkpoint ring stores
//! (`cp.vic.regs[i]`, `cp.ram[addr]`, `cp.vic.color_ram[idx]`, `cp.cia2.c_cia[0]`).
//!
//! Coordinates: `resolveNodeAt`/`resolveRegion` are C64 DISPLAY-area pixels
//! (x in [0,320), y in [0,200)). The `vic/inspect/at|region|origin|promote`
//! WS methods send VISIBLE-frame coords (the 384x272 PAL window); the
//! `resolveVisible*` entry points own the border-aware conversion.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

// ── checkpoint field accessors (vic-inspect.ts:20-22, :60) ──────────────────────

/// vic-inspect.ts:20 — `reg(cp, i) = (cp.vic.regs[i] ?? 0) & 0xff`.
fn reg(cp: &Value, i: usize) -> i64 {
    cp.get("vic")
        .and_then(|v| v.get("regs"))
        .and_then(|r| r.get(i))
        .and_then(|n| n.as_i64())
        .unwrap_or(0)
        & 0xff
}

/// vic-inspect.ts:22 — `colorRam(cp, idx) = (cp.vic.color_ram[idx] ?? 0) & 0x0f`.
fn color_ram(cp: &Value, idx: usize) -> i64 {
    cp.get("vic")
        .and_then(|v| v.get("color_ram"))
        .and_then(|r| r.get(idx))
        .and_then(|n| n.as_i64())
        .unwrap_or(0)
        & 0x0f
}

/// The decoded RAM blob (the `{ $ta }` Uint8Array). Decoded once per resolve; the
/// TS holds `cp.ram` as a live Uint8Array. None when the checkpoint has no RAM.
fn decode_ram(cp: &Value) -> Option<Vec<u8>> {
    cp.get("ram").and_then(crate::native_snapshot::ta_u8_decode)
}

/// vic-inspect.ts:21 — `ram(cp, addr) = (cp.ram[addr & 0xffff] ?? 0) & 0xff`.
fn ram_at(ram: &[u8], addr: i64) -> i64 {
    (ram.get((addr as usize) & 0xffff).copied().unwrap_or(0)) as i64 & 0xff
}

/// vic-inspect.ts:60 — `bankBaseOf = (3 - (cp.cia2.c_cia[0] & 0x03)) * 0x4000`.
fn bank_base_of(cp: &Value) -> i64 {
    let c = cp
        .get("cia2")
        .and_then(|v| v.get("c_cia"))
        .and_then(|a| a.get(0))
        .and_then(|n| n.as_i64())
        .unwrap_or(0);
    (3 - (c & 0x03)) * 0x4000
}

// ── types (vic-inspect-types.ts) ────────────────────────────────────────────────

/// vic-inspect-types.ts:37-42 — `VicInspectMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VicInspectMode {
    StandardText,
    MulticolorText,
    ExtendedBgText,
    HiresBitmap,
    MulticolorBitmap,
}

impl VicInspectMode {
    fn as_str(self) -> &'static str {
        match self {
            VicInspectMode::StandardText => "standard_text",
            VicInspectMode::MulticolorText => "multicolor_text",
            VicInspectMode::ExtendedBgText => "extended_bg_text",
            VicInspectMode::HiresBitmap => "hires_bitmap",
            VicInspectMode::MulticolorBitmap => "multicolor_bitmap",
        }
    }
    fn is_bitmap(self) -> bool {
        matches!(self, VicInspectMode::HiresBitmap | VicInspectMode::MulticolorBitmap)
    }
}

/// vic-inspect-types.ts:16-35 — `MemoryRef`. `value`/`bank`/`note` optional.
#[derive(Debug, Clone)]
pub struct MemoryRef {
    pub kind: &'static str,
    pub addr: i64,
    pub length: i64,
    pub value: Option<i64>,
    pub bank: Option<i64>,
    pub note: Option<String>,
}

impl MemoryRef {
    fn to_json(&self) -> Value {
        let mut o = serde_json::Map::new();
        o.insert("kind".into(), json!(self.kind));
        o.insert("addr".into(), json!(self.addr));
        o.insert("length".into(), json!(self.length));
        // Only emit value/bank/note when present (TS omits undefined keys).
        if let Some(v) = self.value {
            o.insert("value".into(), json!(v));
        }
        if let Some(b) = self.bank {
            o.insert("bank".into(), json!(b));
        }
        if let Some(n) = &self.note {
            o.insert("note".into(), json!(n));
        }
        Value::Object(o)
    }
}

/// vic-inspect-types.ts:49-64 — `VisualNode`.
#[derive(Debug, Clone)]
pub struct VisualNode {
    pub node_type: &'static str,
    pub pixel: (i64, i64),
    pub cell: Option<(i64, i64, i64)>, // (col, row, index)
    pub raster: Option<(i64, Option<i64>)>, // (line, cycle?)
    pub mode: VicInspectMode,
    pub value: Option<i64>,
    pub color_index: Option<i64>,
    pub refs: Vec<MemoryRef>,
}

impl VisualNode {
    pub fn to_json(&self) -> Value {
        let mut o = serde_json::Map::new();
        o.insert("type".into(), json!(self.node_type));
        o.insert("pixel".into(), json!({ "x": self.pixel.0, "y": self.pixel.1 }));
        if let Some((col, row, index)) = self.cell {
            o.insert("cell".into(), json!({ "col": col, "row": row, "index": index }));
        }
        if let Some((line, cycle)) = self.raster {
            let mut r = serde_json::Map::new();
            r.insert("line".into(), json!(line));
            if let Some(c) = cycle {
                r.insert("cycle".into(), json!(c));
            }
            o.insert("raster".into(), Value::Object(r));
        }
        o.insert("mode".into(), json!(self.mode.as_str()));
        if let Some(v) = self.value {
            o.insert("value".into(), json!(v));
        }
        if let Some(ci) = self.color_index {
            o.insert("colorIndex".into(), json!(ci));
        }
        o.insert("refs".into(), Value::Array(self.refs.iter().map(|r| r.to_json()).collect()));
        Value::Object(o)
    }
}

/// vic-inspect-types.ts:67-90 — `VicInspectSnapshot`.
#[derive(Debug, Clone)]
pub struct VicInspectSnapshot {
    pub mode: VicInspectMode,
    pub bank_base: i64,
    pub screen_base: i64,
    pub char_base: i64,
    pub char_rom_shadow: bool,
    pub bitmap_base: i64,
    pub regs: Vec<i64>,
    pub border: i64,
    pub background: i64,
}

impl VicInspectSnapshot {
    pub fn to_json(&self) -> Value {
        json!({
            "mode": self.mode.as_str(),
            "bankBase": self.bank_base,
            "screenBase": self.screen_base,
            "charBase": self.char_base,
            "charRomShadow": self.char_rom_shadow,
            "bitmapBase": self.bitmap_base,
            "colorBase": 0xd800,
            "regs": self.regs,
            "border": self.border,
            "background": self.background,
            "displayWidth": 320,
            "displayHeight": 200,
        })
    }
}

/// vic-inspect.ts:27-34 — `ModeBases`.
#[derive(Debug, Clone, Copy)]
struct ModeBases {
    mode: VicInspectMode,
    bank_base: i64,
    screen_base: i64,
    char_base: i64,
    char_rom_shadow: bool,
    bitmap_base: i64,
}

/// vic-inspect.ts:25 — `FIRST_DISPLAY_RASTER = 51`.
const FIRST_DISPLAY_RASTER: i64 = 51;

/// vic-inspect.ts:38-58 — derive display mode + memory bases from raw VIC regs +
/// bank (shared by the frame snapshot and per-line raster/FLI override).
fn derive_bases(d011: i64, d016: i64, d018: i64, bank_base: i64) -> ModeBases {
    let bmm = (d011 & 0x20) != 0;
    let ecm = (d011 & 0x40) != 0;
    let mcm = (d016 & 0x10) != 0;
    let mode = if bmm {
        if mcm {
            VicInspectMode::MulticolorBitmap
        } else {
            VicInspectMode::HiresBitmap
        }
    } else if ecm {
        VicInspectMode::ExtendedBgText
    } else if mcm {
        VicInspectMode::MulticolorText
    } else {
        VicInspectMode::StandardText
    };
    let screen_offset = ((d018 & 0xf0) >> 4) * 0x400;
    let char_offset = ((d018 & 0x0e) >> 1) * 0x800;
    let bitmap_offset = if (d018 & 0x08) != 0 { 0x2000 } else { 0 };
    // Char ROM is shadowed into the VIC at $1000-$1FFF only for banks based at
    // $0000 and $8000 (vic-inspect.ts:47-49).
    let char_rom_shadow = (bank_base == 0x0000 || bank_base == 0x8000)
        && char_offset >= 0x1000
        && char_offset < 0x2000;
    ModeBases {
        mode,
        bank_base,
        screen_base: bank_base + screen_offset,
        char_base: bank_base + char_offset,
        char_rom_shadow,
        bitmap_base: bank_base + bitmap_offset,
    }
}

/// vic-inspect.ts:63-74 — `buildVicInspectSnapshot(cp)`. Frame-wide VIC state.
pub fn build_vic_inspect_snapshot(cp: &Value) -> VicInspectSnapshot {
    let b = derive_bases(reg(cp, 0x11), reg(cp, 0x16), reg(cp, 0x18), bank_base_of(cp));
    VicInspectSnapshot {
        mode: b.mode,
        bank_base: b.bank_base,
        screen_base: b.screen_base,
        char_base: b.char_base,
        char_rom_shadow: b.char_rom_shadow,
        bitmap_base: b.bitmap_base,
        regs: (0..0x40).map(|i| reg(cp, i)).collect(),
        border: reg(cp, 0x20) & 0x0f,
        background: reg(cp, 0x21) & 0x0f,
    }
}

// ── same-frame provenance sidecar (vic-inspect-types.ts:99-106) ─────────────────

/// vic-inspect-types.ts:99-101 — `SpriteLineRec`.
#[derive(Debug, Clone)]
struct SpriteLineRec {
    i: i64,
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    ptr: i64,
    color: i64,
}

/// vic-inspect-types.ts:103-106 — one `VicFrameProvenance.lines[]` record.
#[derive(Debug, Clone)]
struct ProvenanceLine {
    line: i64,
    d011: i64,
    d016: i64,
    d018: i64,
    bank: i64,
    sprites: Vec<SpriteLineRec>,
}

/// Parse the optional `cp.vicProvenance` sidecar (null/absent in the 710.1/710.2
/// slice — TRX64 captures none yet, so this is `[]` and the resolver uses the
/// frozen 8 hardware regs). Kept so the wire path is 1:1 when provenance lands.
fn parse_provenance(provenance: Option<&Value>) -> Vec<ProvenanceLine> {
    let Some(p) = provenance else { return Vec::new() };
    if p.is_null() {
        return Vec::new();
    }
    let Some(lines) = p.get("lines").and_then(|l| l.as_array()) else {
        return Vec::new();
    };
    lines
        .iter()
        .map(|ln| {
            let g = |k: &str| ln.get(k).and_then(|n| n.as_i64()).unwrap_or(0);
            let sprites = ln
                .get("sprites")
                .and_then(|s| s.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|s| {
                            let sg = |k: &str| s.get(k).and_then(|n| n.as_i64()).unwrap_or(0);
                            SpriteLineRec {
                                i: sg("i"),
                                x: sg("x"),
                                y: sg("y"),
                                w: sg("w"),
                                h: sg("h"),
                                ptr: sg("ptr"),
                                color: sg("color"),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            ProvenanceLine {
                line: g("line"),
                d011: g("d011"),
                d016: g("d016"),
                d018: g("d018"),
                bank: g("bank"),
                sprites,
            }
        })
        .collect()
}

// ── display-area resolve (vic-inspect.ts:80-161) ────────────────────────────────

/// vic-inspect.ts:80-105 — display-area sprite bounding-box hit-test, front-to-back.
fn sprite_bounds_at(cp: &Value, ram: &[u8], snap: &VicInspectSnapshot, x: i64, y: i64) -> Option<VisualNode> {
    let enable = reg(cp, 0x15);
    if enable == 0 {
        return None;
    }
    let msbx = reg(cp, 0x10);
    let xexp = reg(cp, 0x1d);
    let yexp = reg(cp, 0x17);
    for i in 0..8i64 {
        if (enable & (1 << i)) == 0 {
            continue;
        }
        let sx = reg(cp, (i * 2) as usize) | if (msbx & (1 << i)) != 0 { 0x100 } else { 0 };
        let sy = reg(cp, (i * 2 + 1) as usize);
        let w = if (xexp & (1 << i)) != 0 { 48 } else { 24 };
        let h = if (yexp & (1 << i)) != 0 { 42 } else { 21 };
        let dx = sx - 24;
        let dy = sy - 50; // VIC sprite coords → display-area origin
        if x >= dx && x < dx + w && y >= dy && y < dy + h {
            let ptr_addr = snap.screen_base + 0x3f8 + i;
            let ptr = ram_at(ram, ptr_addr);
            let refs = vec![
                MemoryRef { kind: "sprite_ptr", addr: ptr_addr, length: 1, value: Some(ptr), bank: Some(snap.bank_base), note: None },
                MemoryRef { kind: "sprite_data", addr: snap.bank_base + ptr * 64, length: 63, value: None, bank: Some(snap.bank_base), note: Some("bounding-box hit; not pixel-exact (no transparency/priority)".into()) },
                MemoryRef { kind: "vic_reg", addr: 0xd000 + i * 2, length: 1, value: Some(sx & 0xff), bank: None, note: Some("sprite X".into()) },
                MemoryRef { kind: "vic_reg", addr: 0xd001 + i * 2, length: 1, value: Some(sy), bank: None, note: Some("sprite Y".into()) },
                MemoryRef { kind: "vic_reg", addr: 0xd027 + i, length: 1, value: Some(reg(cp, (0x27 + i) as usize) & 0x0f), bank: None, note: Some("sprite color".into()) },
            ];
            return Some(VisualNode {
                node_type: "sprite_bounds",
                pixel: (x, y),
                cell: None,
                raster: None,
                mode: snap.mode,
                value: Some(i),
                color_index: Some(reg(cp, (0x27 + i) as usize) & 0x0f),
                refs,
            });
        }
    }
    None
}

/// vic-inspect.ts:112-161 — `resolveNodeAt(cp, x, y, provenance?)`. Display-area.
fn resolve_node_at(cp: &Value, ram: &[u8], x: i64, y: i64, provenance: &[ProvenanceLine]) -> VisualNode {
    let snap = build_vic_inspect_snapshot(cp);

    if let Some(sprite) = sprite_bounds_at(cp, ram, &snap, x, y) {
        return sprite;
    }

    // Per-line raster/FLI override (710.4): map display y → raster line → record.
    let mut bases = ModeBases {
        mode: snap.mode,
        bank_base: snap.bank_base,
        screen_base: snap.screen_base,
        char_base: snap.char_base,
        char_rom_shadow: snap.char_rom_shadow,
        bitmap_base: snap.bitmap_base,
    };
    let mut raster: Option<(i64, Option<i64>)> = None;
    if !provenance.is_empty() {
        let line = FIRST_DISPLAY_RASTER + y;
        if let Some(ln) = provenance.iter().find(|l| l.line == line) {
            bases = derive_bases(ln.d011, ln.d016, ln.d018, ln.bank);
            raster = Some((line, None));
        }
    }

    let col = (x >> 3).clamp(0, 39);
    let row = (y >> 3).clamp(0, 24);
    let index = row * 40 + col;
    let mut refs: Vec<MemoryRef> = Vec::new();

    if bases.mode.is_bitmap() {
        let screen_addr = bases.screen_base + index;
        refs.push(MemoryRef { kind: "screen_ram", addr: screen_addr, length: 1, value: Some(ram_at(ram, screen_addr)), bank: Some(bases.bank_base), note: Some("fg/bg colour nibbles".into()) });
        refs.push(MemoryRef { kind: "bitmap", addr: bases.bitmap_base + index * 8, length: 8, value: None, bank: Some(bases.bank_base), note: None });
        if bases.mode == VicInspectMode::MulticolorBitmap {
            refs.push(MemoryRef { kind: "color_ram", addr: 0xd800 + index, length: 1, value: Some(color_ram(cp, index as usize)), bank: None, note: None });
        }
        refs.push(MemoryRef { kind: "vic_reg", addr: 0xd011, length: 1, value: Some(reg(cp, 0x11)), bank: None, note: None });
        refs.push(MemoryRef { kind: "vic_reg", addr: 0xd018, length: 1, value: Some(reg(cp, 0x18)), bank: None, note: None });
        return VisualNode {
            node_type: "bitmap_cell",
            pixel: (x, y),
            cell: Some((col, row, index)),
            raster,
            mode: bases.mode,
            value: None,
            color_index: None,
            refs,
        };
    }

    // text modes
    let screen_addr = bases.screen_base + index;
    let code = ram_at(ram, screen_addr);
    let color_index = color_ram(cp, index as usize);
    refs.push(MemoryRef { kind: "screen_ram", addr: screen_addr, length: 1, value: Some(code), bank: Some(bases.bank_base), note: None });
    refs.push(MemoryRef { kind: "color_ram", addr: 0xd800 + index, length: 1, value: Some(color_index), bank: None, note: None });
    refs.push(MemoryRef { kind: "charset", addr: bases.char_base + code * 8, length: 8, value: None, bank: Some(bases.bank_base), note: if bases.char_rom_shadow { Some("char ROM shadow".into()) } else { None } });
    refs.push(MemoryRef { kind: "vic_reg", addr: 0xd018, length: 1, value: Some(reg(cp, 0x18)), bank: None, note: None });
    VisualNode {
        node_type: "text_cell",
        pixel: (x, y),
        cell: Some((col, row, index)),
        raster,
        mode: bases.mode,
        value: Some(code),
        color_index: Some(color_index),
        refs,
    }
}

// ── visible-frame → display-area geometry (vic-inspect.ts:209-211) ──────────────

/// vic-inspect.ts:209 — the rendered visible PAL window.
pub const VISIBLE_FRAME_W: i64 = 384;
pub const VISIBLE_FRAME_H: i64 = 272;
/// vic-inspect.ts:210 — literal renderer crop: first visible raster line (fb Y0).
const CANVAS_Y0: i64 = 16;
/// vic-inspect.ts:211 — DISPLAY_ORIGIN = { x:32, y:35 }.
pub const DISPLAY_ORIGIN_X: i64 = 32;
pub const DISPLAY_ORIGIN_Y: i64 = FIRST_DISPLAY_RASTER - CANVAS_Y0; // 35

/// vic-inspect.ts:214-227 — build a `sprite_bounds` node (visible-frame variant).
#[allow(clippy::too_many_arguments)]
fn make_sprite_node(
    i: i64,
    sx: i64,
    sy: i64,
    ptr: i64,
    color: i64,
    ptr_addr: i64,
    bank_base: i64,
    mode: VicInspectMode,
    vx: f64,
    vy: f64,
    multiplexed: bool,
) -> VisualNode {
    let in_border = vy < DISPLAY_ORIGIN_Y as f64 || vy >= (DISPLAY_ORIGIN_Y + 200) as f64;
    let data_note = format!(
        "bounding-box; not pixel-exact{}{}",
        if multiplexed { "; MULTIPLEXED (per-raster)" } else { "" },
        if in_border { "; OPEN BORDER" } else { "" }
    );
    let refs = vec![
        MemoryRef { kind: "sprite_ptr", addr: ptr_addr, length: 1, value: Some(ptr), bank: Some(bank_base), note: None },
        MemoryRef { kind: "sprite_data", addr: bank_base + ptr * 64, length: 63, value: None, bank: Some(bank_base), note: Some(data_note) },
        MemoryRef { kind: "vic_reg", addr: 0xd000 + i * 2, length: 1, value: Some(sx & 0xff), bank: None, note: Some("sprite X".into()) },
        MemoryRef { kind: "vic_reg", addr: 0xd001 + i * 2, length: 1, value: Some(sy), bank: None, note: Some("sprite Y (raster)".into()) },
        MemoryRef { kind: "vic_reg", addr: 0xd027 + i, length: 1, value: Some(color & 0x0f), bank: None, note: Some("sprite color".into()) },
    ];
    VisualNode {
        node_type: "sprite_bounds",
        pixel: (vx.round() as i64, vy.round() as i64),
        cell: None,
        raster: Some((vy.round() as i64 + CANVAS_Y0, None)),
        mode,
        value: Some(i),
        color_index: Some(color & 0x0f),
        refs,
    }
}

/// vic-inspect.ts:238-274 — visible-frame sprite hit-test across the WHOLE frame
/// (incl. the open border). Per-raster provenance sprites win when present.
fn sprite_bounds_at_visible(
    cp: &Value,
    ram: &[u8],
    snap: &VicInspectSnapshot,
    vx: f64,
    vy: f64,
    provenance: &[ProvenanceLine],
) -> Option<VisualNode> {
    let raster = vy.round() as i64 + CANVAS_Y0;

    // Multiplexer: per-raster sprite state is authoritative for THIS line.
    if let Some(ln) = provenance.iter().find(|l| l.line == raster) {
        for s in &ln.sprites {
            let bx = (s.x - 24 + DISPLAY_ORIGIN_X) as f64;
            let by = (s.y - CANVAS_Y0) as f64;
            if vx >= bx && vx < bx + s.w as f64 && vy >= by && vy < by + s.h as f64 {
                let lbank = ln.bank;
                let lscreen = lbank + ((ln.d018 & 0xf0) >> 4) * 0x400;
                return Some(make_sprite_node(
                    s.i, s.x, s.y, s.ptr, s.color, lscreen + 0x3f8 + s.i, lbank, snap.mode, vx, vy, true,
                ));
            }
        }
        return None; // per-raster state known → no sprite covers this pixel
    }

    // No provenance for this raster → frozen 8 hardware sprite registers.
    let enable = reg(cp, 0x15);
    if enable == 0 {
        return None;
    }
    let msbx = reg(cp, 0x10);
    let xexp = reg(cp, 0x1d);
    let yexp = reg(cp, 0x17);
    for i in 0..8i64 {
        if (enable & (1 << i)) == 0 {
            continue;
        }
        let sx = reg(cp, (i * 2) as usize) | if (msbx & (1 << i)) != 0 { 0x100 } else { 0 };
        let sy = reg(cp, (i * 2 + 1) as usize);
        let w = if (xexp & (1 << i)) != 0 { 48 } else { 24 };
        let h = if (yexp & (1 << i)) != 0 { 42 } else { 21 };
        let bx = (sx - 24 + DISPLAY_ORIGIN_X) as f64;
        let by = (sy - CANVAS_Y0) as f64;
        if vx >= bx && vx < bx + w as f64 && vy >= by && vy < by + h as f64 {
            let ptr_addr = snap.screen_base + 0x3f8 + i;
            return Some(make_sprite_node(
                i, sx, sy, ram_at(ram, ptr_addr), reg(cp, (0x27 + i) as usize), ptr_addr, snap.bank_base, snap.mode, vx, vy, false,
            ));
        }
    }
    None
}

/// vic-inspect.ts:277-282 — visible-frame pixel → display-area pixel (clamped).
fn visible_to_display(vx: f64, vy: f64) -> (i64, i64) {
    (
        (vx.round() as i64 - DISPLAY_ORIGIN_X).clamp(0, 319),
        (vy.round() as i64 - DISPLAY_ORIGIN_Y).clamp(0, 199),
    )
}

/// vic-inspect.ts:298-317 — `resolveVisibleNodeAt(cp, vx, vy, provenance?)`.
/// Sprite-first + border-aware. The public entry for the `vic/inspect/at` method.
pub fn resolve_visible_node_at(cp: &Value, vx: f64, vy: f64, provenance: Option<&Value>) -> VisualNode {
    let ram = decode_ram(cp).unwrap_or_default();
    let prov = parse_provenance(provenance);
    resolve_visible_node_at_inner(cp, &ram, vx, vy, &prov)
}

fn resolve_visible_node_at_inner(cp: &Value, ram: &[u8], vx: f64, vy: f64, provenance: &[ProvenanceLine]) -> VisualNode {
    let snap = build_vic_inspect_snapshot(cp);
    if let Some(sprite) = sprite_bounds_at_visible(cp, ram, &snap, vx, vy, provenance) {
        return sprite;
    }

    let in_display = vx >= DISPLAY_ORIGIN_X as f64
        && vx < (DISPLAY_ORIGIN_X + 320) as f64
        && vy >= DISPLAY_ORIGIN_Y as f64
        && vy < (DISPLAY_ORIGIN_Y + 200) as f64;
    if in_display {
        let (dx, dy) = visible_to_display(vx, vy);
        return resolve_node_at(cp, ram, dx, dy, provenance);
    }
    // open border, no sprite → border colour ($D020)
    VisualNode {
        node_type: "border",
        pixel: (vx.round() as i64, vy.round() as i64),
        cell: None,
        raster: None,
        mode: snap.mode,
        value: None,
        color_index: Some(snap.border),
        refs: vec![MemoryRef { kind: "vic_reg", addr: 0xd020, length: 1, value: Some(snap.border), bank: None, note: Some("border colour".into()) }],
    }
}

/// vic-inspect.ts:322-338 — `resolveVisibleRegion(cp, region, provenance?)`.
/// Samples in VISIBLE space (8px step), dedups by type:value:cellIndex:rasterLine.
pub fn resolve_visible_region(cp: &Value, region: (f64, f64, f64, f64), provenance: Option<&Value>) -> Vec<VisualNode> {
    let ram = decode_ram(cp).unwrap_or_default();
    let prov = parse_provenance(provenance);
    let (rx, ry, rw, rh) = region;
    let mut nodes: Vec<VisualNode> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let x1 = rx + rw;
    let y1 = ry + rh;
    let mut vy = ry;
    while vy < y1 {
        let mut vx = rx;
        while vx < x1 {
            let n = resolve_visible_node_at_inner(cp, &ram, vx, vy, &prov);
            let key = node_dedup_key_visible(&n);
            if seen.insert(key) {
                nodes.push(n);
            }
            vx += 8.0;
        }
        vy += 8.0;
    }
    nodes
}

/// vic-inspect.ts:333 — `${type}:${value ?? ""}:${cell.index ?? ""}:${raster.line ?? ""}`.
fn node_dedup_key_visible(n: &VisualNode) -> String {
    let value = n.value.map(|v| v.to_string()).unwrap_or_default();
    let cell = n.cell.map(|c| c.2.to_string()).unwrap_or_default();
    let raster = n.raster.map(|r| r.0.to_string()).unwrap_or_default();
    format!("{}:{}:{}:{}", n.node_type, value, cell, raster)
}

/// vic-inspect.ts:343-359 — `resolveRegion(cp, region, provenance?)` (display-area).
/// Dedup key: `${type}:${cell.index ?? value}:${raster.line ?? ""}`.
pub fn resolve_region(cp: &Value, region: (i64, i64, i64, i64), provenance: Option<&Value>) -> Vec<VisualNode> {
    let ram = decode_ram(cp).unwrap_or_default();
    let prov = parse_provenance(provenance);
    let (rx, ry, rw, rh) = region;
    let mut nodes: Vec<VisualNode> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let x1 = rx + rw;
    let y1 = ry + rh;
    let mut cy = ry;
    while cy < y1 {
        let mut cx = rx;
        while cx < x1 {
            let n = resolve_node_at(cp, &ram, cx, cy, &prov);
            let cell_or_value = n.cell.map(|c| c.2).or(n.value).map(|v| v.to_string()).unwrap_or_default();
            let raster = n.raster.map(|r| r.0.to_string()).unwrap_or_default();
            let key = format!("{}:{}:{}", n.node_type, cell_or_value, raster);
            if seen.insert(key) {
                nodes.push(n);
            }
            cx += 8;
        }
        cy += 8;
    }
    nodes
}

// ── frozen-inspect evidence (vic-inspect.ts:172-200) ────────────────────────────

/// vic-inspect.ts:172-200 — `assembleInspectEvidence(cp, checkpointId, opts)`.
/// Points/region are VISIBLE-frame coords → border-aware resolve.
#[allow(clippy::too_many_arguments)]
pub fn assemble_inspect_evidence(
    cp: &Value,
    checkpoint_id: &str,
    points: &[(f64, f64)],
    region: Option<(f64, f64, f64, f64)>,
    trace_mark_id: Option<&str>,
    snapshot_ref: Option<&str>,
    experiment_id: Option<&str>,
    provenance: Option<&Value>,
) -> Value {
    let ram = decode_ram(cp).unwrap_or_default();
    let prov = parse_provenance(provenance);
    let mut selected: Vec<VisualNode> = Vec::new();
    for (x, y) in points {
        selected.push(resolve_visible_node_at_inner(cp, &ram, *x, *y, &prov));
    }
    if let Some(r) = region {
        selected.extend(resolve_visible_region(cp, r, provenance));
    }
    let media_state = cp.get("media").cloned().unwrap_or(Value::Null);
    let provenance_out = provenance.filter(|p| !p.is_null()).cloned();
    let mut o = serde_json::Map::new();
    o.insert("checkpointId".into(), json!(checkpoint_id));
    if let Some(s) = snapshot_ref {
        o.insert("snapshotRef".into(), json!(s));
    }
    if let Some(e) = experiment_id {
        o.insert("experimentId".into(), json!(e));
    }
    o.insert("mediaState".into(), media_state);
    if let Some(t) = trace_mark_id {
        o.insert("traceMarkId".into(), json!(t));
    }
    o.insert("frame".into(), build_vic_inspect_snapshot(cp).to_json());
    if let Some(p) = provenance_out {
        o.insert("provenance".into(), p);
    }
    o.insert("selectedNodes".into(), Value::Array(selected.iter().map(|n| n.to_json()).collect()));
    Value::Object(o)
}

// ── asset extraction (asset-extract.ts) ─────────────────────────────────────────

/// asset-extract.ts:11-28 — `AssetCandidate`.
#[derive(Debug, Clone)]
pub struct AssetCandidate {
    pub id: String,
    pub artifact_id: String,
    pub kind: &'static str,
    pub file_ref: Option<String>,
    pub medium_ref: Option<String>,
    pub offset: i64,
    pub length: i64,
    pub format: &'static str,
    pub preview_hash: String,
    pub confidence: f64,
}

impl AssetCandidate {
    fn to_json(&self) -> Value {
        let mut src = serde_json::Map::new();
        if let Some(f) = &self.file_ref {
            src.insert("fileRef".into(), json!(f));
        }
        if let Some(m) = &self.medium_ref {
            src.insert("mediumRef".into(), json!(m));
        }
        src.insert("offset".into(), json!(self.offset));
        src.insert("length".into(), json!(self.length));
        json!({
            "id": self.id,
            "artifactId": self.artifact_id,
            "kind": self.kind,
            "source": Value::Object(src),
            "format": self.format,
            "preview": { "hash": self.preview_hash },
            "confidence": self.confidence,
        })
    }
}

const SPRITE_BLOCK: i64 = 64;
const CHARSET_2K: i64 = 0x800;
const BITMAP_HIRES: i64 = 8000;

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// asset-extract.ts:31-53 — `scanBlocks`: fixed-size, hashed candidate blocks.
fn scan_blocks(
    bytes: &[u8],
    artifact_id: &str,
    medium_ref: Option<&str>,
    base_offset: i64,
    min_distinct: usize,
    kind: &'static str,
    format: &'static str,
    block_len: i64,
    step: i64,
    id_prefix: &str,
) -> Vec<AssetCandidate> {
    let mut out = Vec::new();
    let block_len_u = block_len as usize;
    let step_u = step as usize;
    let mut off = 0usize;
    while off + block_len_u <= bytes.len() {
        let block = &bytes[off..off + block_len_u];
        let distinct = {
            let mut seen = [false; 256];
            let mut c = 0usize;
            for &b in block {
                if !seen[b as usize] {
                    seen[b as usize] = true;
                    c += 1;
                }
            }
            c
        };
        if distinct >= min_distinct {
            out.push(AssetCandidate {
                id: format!("{artifact_id}:{id_prefix}:{:x}", base_offset + off as i64),
                artifact_id: artifact_id.to_string(),
                kind,
                file_ref: None,
                medium_ref: medium_ref.map(|s| s.to_string()),
                offset: base_offset + off as i64,
                length: block_len,
                format,
                preview_hash: sha256_hex(block),
                confidence: (distinct as f64 / 32.0).min(1.0),
            });
        }
        off += step_u;
    }
    out
}

/// asset-extract.ts:71-76 — `extractAssetCandidates`: sprite + charset + bitmap.
pub fn extract_asset_candidates(bytes: &[u8], artifact_id: &str, medium_ref: Option<&str>) -> Vec<AssetCandidate> {
    let mut out = Vec::new();
    // asset-extract.ts:56-58 — 64-byte sprite blocks, 64-stepped.
    out.extend(scan_blocks(bytes, artifact_id, medium_ref, 0, 3, "sprite", "sprite-block", SPRITE_BLOCK, SPRITE_BLOCK, "spr"));
    // asset-extract.ts:61-63 — 2KB charset sets, 2KB-stepped.
    out.extend(scan_blocks(bytes, artifact_id, medium_ref, 0, 3, "charset", "charset-2k", CHARSET_2K, CHARSET_2K, "chr"));
    // asset-extract.ts:66-68 — 8KB hires bitmaps, 8KB-stepped.
    out.extend(scan_blocks(bytes, artifact_id, medium_ref, 0, 3, "bitmap", "bitmap-hires", BITMAP_HIRES, 0x2000, "bmp"));
    out
}

// ── asset-join (asset-join.ts) ──────────────────────────────────────────────────

/// asset-join.ts:36-40 — sha256 (hex) of `length` RAM bytes at `addr`.
fn hash_ram_range(ram: &[u8], addr: i64, length: i64) -> String {
    let start = (addr as usize) & 0xffff;
    let end = ((addr + length) as usize).min(0x10000);
    let slice = if start < end && start < ram.len() {
        &ram[start..end.min(ram.len())]
    } else {
        &[]
    };
    sha256_hex(slice)
}

/// asset-join.ts:43-64 — `dataRefOf(node)`: the data range + hash length to match.
fn data_ref_of(node: &VisualNode) -> Option<(i64, i64)> {
    match node.node_type {
        "sprite_bounds" => {
            let r = node.refs.iter().find(|r| r.kind == "sprite_data")?;
            Some((r.addr, SPRITE_BLOCK)) // hash the full 64-byte block
        }
        "bitmap_cell" => {
            let r = node.refs.iter().find(|r| r.kind == "bitmap")?;
            let base = r.addr - node.cell.map(|c| c.2).unwrap_or(0) * 8;
            Some((base, 8000))
        }
        "text_cell" => {
            let r = node.refs.iter().find(|r| r.kind == "charset")?;
            let base = r.addr - node.value.unwrap_or(0) * 8;
            Some((base, 0x800))
        }
        _ => None,
    }
}

fn kind_for_node(node_type: &str) -> Option<&'static str> {
    match node_type {
        "sprite_bounds" => Some("sprite"),
        "bitmap_cell" => Some("bitmap"),
        "text_cell" => Some("charset"),
        _ => None,
    }
}

/// asset-join.ts:38-50 — the classification + the matched candidate + evidence.
#[derive(Debug, Clone)]
pub struct AssetJoinResult {
    pub classification: &'static str,
    pub memory_range: (i64, i64),
    pub ram_hash: String,
    pub candidate: Option<AssetCandidate>,
    pub evidence: String,
}

impl AssetJoinResult {
    fn to_json(&self) -> Value {
        let mut o = serde_json::Map::new();
        o.insert("classification".into(), json!(self.classification));
        o.insert("memoryRange".into(), json!({ "addr": self.memory_range.0, "length": self.memory_range.1 }));
        o.insert("ramHash".into(), json!(self.ram_hash));
        if let Some(c) = &self.candidate {
            o.insert("candidate".into(), c.to_json());
        }
        o.insert("evidence".into(), json!(self.evidence));
        Value::Object(o)
    }
}

/// asset-join.ts:79-117 — `matchVisualNodeToAsset`. J1 exact (hash) match;
/// otherwise honest `runtime_generated` (no trace source ported → no J2 chain).
pub fn match_visual_node_to_asset(ram: &[u8], node: &VisualNode, candidates: &[AssetCandidate]) -> AssetJoinResult {
    let Some((addr, hash_len)) = data_ref_of(node) else {
        return AssetJoinResult {
            classification: "unresolved",
            memory_range: (0, 0),
            ram_hash: String::new(),
            candidate: None,
            evidence: format!("{} has no resolvable data range", node.node_type),
        };
    };
    let ram_hash = hash_ram_range(ram, addr, hash_len);
    let memory_range = (addr, hash_len);
    let want_kind = kind_for_node(node.node_type);

    // Step 1 (J1) — exact: runtime bytes == an extracted asset, placed verbatim.
    let exact = candidates
        .iter()
        .find(|c| c.preview_hash == ram_hash && want_kind.map(|wk| c.kind == wk).unwrap_or(true));
    if let Some(c) = exact {
        let where_ = c.file_ref.as_deref().or(c.medium_ref.as_deref()).unwrap_or("?");
        return AssetJoinResult {
            classification: "exact_asset",
            memory_range,
            ram_hash,
            candidate: Some(c.clone()),
            evidence: format!(
                "RAM ${:x}..+{} == {} ({} {} @ {}+${:x})",
                addr, hash_len, c.id, c.kind, c.format, where_, c.offset
            ),
        };
    }

    // Step 3 — honest no-origin (no trace source supplied — J2 not ported).
    AssetJoinResult {
        classification: "runtime_generated",
        memory_range,
        ram_hash,
        candidate: None,
        evidence: "no exact asset hash match (no trace source supplied)".to_string(),
    }
}

// ── asset-join-knowledge (asset-join-knowledge.ts) ──────────────────────────────

fn hx(n: i64) -> String {
    format!("${:x}", (n as u32))
}

/// asset-join-knowledge.ts:49-104 — `assetJoinToKnowledge(r, ctx)`.
pub fn asset_join_to_knowledge(r: &AssetJoinResult, artifact_id: &str) -> Value {
    let (mr_addr, mr_len) = r.memory_range;
    let mem_ref = format!("{artifact_id}:mem:{}+{}", hx(mr_addr), mr_len);
    let vis_ref = format!("{artifact_id}:visual:{}", hx(mr_addr));
    let mut ev: Vec<Value> = vec![json!(r.evidence)];
    if !r.ram_hash.is_empty() {
        ev.push(json!(format!("ramHash={}", &r.ram_hash[..r.ram_hash.len().min(12)])));
    }
    let mut relations: Vec<Value> = Vec::new();
    let mut annotations: Vec<Value> = Vec::new();

    // VisualElement → MemoryRange (always).
    relations.push(json!({
        "from": { "kind": "VisualElement", "ref": vis_ref },
        "to": { "kind": "MemoryRange", "ref": mem_ref },
        "relation": "maps-to",
        "evidence": format!("frozen-inspect node @ {}", hx(mr_addr)),
    }));

    if r.classification == "runtime_generated" || r.classification == "unresolved" {
        let which = if r.classification == "unresolved" { "unresolved" } else { "generated" };
        annotations.push(json!({
            "kind": "segment", "addr": mr_addr, "length": mr_len,
            "comment": format!("runtime-{which}: no static asset origin ({})", r.evidence),
            "provenance": "runtime-join", "evidence": ev,
        }));
        return json!({
            "classification": r.classification,
            "relations": relations,
            "annotations": annotations,
            "finding": {
                "kind": "observation",
                "title": format!("No static origin for {} ({})", hx(mr_addr), r.classification),
                "summary": r.evidence,
                "tags": ["vic-inspect", "asset-join", r.classification],
                "addressRange": { "start": mr_addr, "end": mr_addr + mr_len },
            },
        });
    }

    // exact_asset / derived_asset → there is a source candidate.
    let c = r.candidate.as_ref().expect("exact/derived has a candidate");
    let art_ref = format!("{}:file:{}+{}", c.artifact_id, hx(c.offset), c.length);
    let where_ = c.file_ref.as_deref().or(c.medium_ref.as_deref()).unwrap_or("?");

    relations.push(json!({
        "from": { "kind": "MemoryRange", "ref": mem_ref },
        "to": { "kind": "ArtifactRange", "ref": art_ref },
        "relation": "derived-from",
        "evidence": r.evidence,
    }));
    if let Some(m) = &c.medium_ref {
        relations.push(json!({
            "from": { "kind": "ArtifactRange", "ref": art_ref },
            "to": { "kind": "MediaRegion", "ref": format!("{}@{}", m, hx(c.offset)) },
            "relation": "contains",
            "evidence": format!("{} {} on {}", c.kind, c.format, m),
        }));
    }

    // Data label on the destination range (exact_asset path; derived chain omitted).
    let verbatim = if r.classification == "exact_asset" { "verbatim" } else { "derived" };
    annotations.push(json!({
        "kind": "label", "addr": mr_addr, "length": mr_len,
        "name": format!("{}_{}", c.kind, &hx(mr_addr)[1..]),
        "comment": format!("{verbatim} {} {} ⇐ {} ({}+{})", c.kind, c.format, c.id, where_, hx(c.offset)),
        "provenance": "runtime-join", "evidence": ev,
    }));

    json!({
        "classification": r.classification,
        "relations": relations,
        "annotations": annotations,
        "finding": {
            "kind": "observation",
            "title": format!("{}: {} ⇐ {} ({} {})", r.classification, hx(mr_addr), c.id, c.kind, c.format),
            "summary": r.evidence,
            "tags": ["vic-inspect", "asset-join", r.classification, c.kind],
            "addressRange": { "start": mr_addr, "end": mr_addr + mr_len },
        },
    })
}

// ── resolveVisualOrigin (asset-origin.ts) ───────────────────────────────────────

/// asset-origin.ts:22-32 — `resolveVisualOrigin(cp, node, candidates, ctx)`.
/// Returns `{ result, knowledge }` JSON values (the node is supplied by the
/// caller; the WS handler echoes it alongside). 1:1 wire shape.
pub fn resolve_visual_origin(cp: &Value, node: &VisualNode, candidates: &[AssetCandidate], artifact_id: &str) -> (Value, Value) {
    let ram = decode_ram(cp).unwrap_or_default();
    let result = match_visual_node_to_asset(&ram, node, candidates);
    let knowledge = asset_join_to_knowledge(&result, artifact_id);
    (result.to_json(), knowledge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A minimal checkpoint: standard text mode, bank 0, screen $0400, char ROM
    /// shadow at $1000. RAM filled so screen RAM holds a known code at the cell.
    fn mk_text_cp(screen_code: u8, color: u8) -> Value {
        let mut ram = vec![0u8; 0x10000];
        // cell (0,0) → screen RAM $0400 index 0.
        ram[0x0400] = screen_code;
        let regs: Vec<i64> = {
            let mut r = vec![0i64; 0x40];
            r[0x11] = 0x1b; // DEN + RSEL, standard text
            r[0x16] = 0xc8; // CSEL, no MCM
            r[0x18] = 0x14; // screen $0400 (1<<4), char $1000 (2<<1)
            r[0x20] = 0x0e; // border light blue
            r[0x21] = 0x06; // bg blue
            r
        };
        let mut color_ram = vec![0i64; 0x400];
        color_ram[0] = color as i64;
        json!({
            "ram": crate::native_snapshot::ta_u8(&ram),
            "vic": { "regs": regs, "color_ram": color_ram },
            "cia2": { "c_cia": [0x3f, 0, 0, 0] }, // bank 0
            "media": Value::Null,
        })
    }

    #[test]
    fn snapshot_standard_text_bank0() {
        let cp = mk_text_cp(0x41, 0x01);
        let snap = build_vic_inspect_snapshot(&cp);
        assert_eq!(snap.mode, VicInspectMode::StandardText);
        assert_eq!(snap.bank_base, 0x0000);
        assert_eq!(snap.screen_base, 0x0400);
        assert_eq!(snap.char_base, 0x1000);
        assert!(snap.char_rom_shadow, "char $1000 in bank 0 = char ROM shadow");
        assert_eq!(snap.border, 0x0e);
        assert_eq!(snap.background, 0x06);
    }

    #[test]
    fn resolve_text_cell_at_origin() {
        let cp = mk_text_cp(0x41, 0x01); // code $41 ('A'), white
        // Visible-frame click inside the display window at cell (0,0):
        // display (4,4) → visible (4+32, 4+35) = (36, 39).
        let node = resolve_visible_node_at(&cp, 36.0, 39.0, None);
        assert_eq!(node.node_type, "text_cell");
        assert_eq!(node.value, Some(0x41));
        assert_eq!(node.color_index, Some(0x01));
        assert_eq!(node.cell, Some((0, 0, 0)));
        // screen RAM ref at $0400, charset ref at $1000 + 0x41*8.
        let screen = node.refs.iter().find(|r| r.kind == "screen_ram").unwrap();
        assert_eq!(screen.addr, 0x0400);
        assert_eq!(screen.value, Some(0x41));
        let charset = node.refs.iter().find(|r| r.kind == "charset").unwrap();
        assert_eq!(charset.addr, 0x1000 + 0x41 * 8);
        assert_eq!(charset.note.as_deref(), Some("char ROM shadow"));
    }

    #[test]
    fn border_pixel_resolves_to_border_node() {
        let cp = mk_text_cp(0x41, 0x01);
        // visible (5,5) is in the open border (display origin is 32,35).
        let node = resolve_visible_node_at(&cp, 5.0, 5.0, None);
        assert_eq!(node.node_type, "border");
        assert_eq!(node.color_index, Some(0x0e));
        let r = &node.refs[0];
        assert_eq!(r.addr, 0xd020);
    }

    #[test]
    fn sprite_bounds_visible_hit() {
        let mut ram = vec![0u8; 0x10000];
        // sprite 0 pointer at $07f8 → block 13 (data $0340).
        ram[0x07f8] = 13;
        let mut regs = vec![0i64; 0x40];
        regs[0x11] = 0x1b;
        regs[0x16] = 0xc8;
        regs[0x18] = 0x14;
        regs[0x15] = 0x01; // sprite 0 enabled
        regs[0x00] = 100; // sprite 0 X
        regs[0x01] = 100; // sprite 0 Y
        regs[0x27] = 0x07; // sprite 0 color yellow
        let cp = json!({
            "ram": crate::native_snapshot::ta_u8(&ram),
            "vic": { "regs": regs, "color_ram": vec![0i64; 0x400] },
            "cia2": { "c_cia": [0x3f, 0, 0, 0] },
            "media": Value::Null,
        });
        // sprite box: x in [100-24+32, ..+24) = [108,132); y in [100-16, ..+21) = [84,105).
        let node = resolve_visible_node_at(&cp, 120.0, 90.0, None);
        assert_eq!(node.node_type, "sprite_bounds");
        assert_eq!(node.value, Some(0));
        assert_eq!(node.color_index, Some(0x07));
        let ptr = node.refs.iter().find(|r| r.kind == "sprite_ptr").unwrap();
        assert_eq!(ptr.addr, 0x0400 + 0x3f8);
        assert_eq!(ptr.value, Some(13));
        let data = node.refs.iter().find(|r| r.kind == "sprite_data").unwrap();
        assert_eq!(data.addr, 13 * 64); // bank 0 + ptr*64 = $0340
    }

    #[test]
    fn origin_runtime_generated_when_no_candidate() {
        let cp = mk_text_cp(0x41, 0x01);
        let node = resolve_visible_node_at(&cp, 36.0, 39.0, None);
        let (result, knowledge) = resolve_visual_origin(&cp, &node, &[], "sess1");
        assert_eq!(result["classification"], "runtime_generated");
        assert_eq!(knowledge["classification"], "runtime_generated");
        // VisualElement → MemoryRange relation always present.
        let rels = knowledge["relations"].as_array().unwrap();
        assert_eq!(rels[0]["relation"], "maps-to");
    }

    #[test]
    fn origin_exact_asset_when_candidate_matches() {
        // Put a known 2KB charset into RAM at $1000 and extract it as a candidate;
        // the text node's charBase ($1000) must exact-match the candidate hash.
        let mut ram = vec![0u8; 0x10000];
        // distinct charset bytes ($1000..$1800).
        for i in 0..0x800usize {
            ram[0x1000 + i] = (i & 0xff) as u8;
        }
        ram[0x0400] = 0x41;
        let mut regs = vec![0i64; 0x40];
        regs[0x11] = 0x1b;
        regs[0x16] = 0xc8;
        // screen $0400, char $1000 (d018=0x14). The join hashes RAM at $1000
        // regardless of the char-ROM-shadow note (it reads the live checkpoint RAM).
        regs[0x18] = 0x14;
        let cp = json!({
            "ram": crate::native_snapshot::ta_u8(&ram),
            "vic": { "regs": regs, "color_ram": vec![0i64; 0x400] },
            "cia2": { "c_cia": [0x3f, 0, 0, 0] },
            "media": Value::Null,
        });
        // candidate over the 2KB at $1000 (offset 0 of a synthetic medium == $1000 RAM).
        let charset_bytes = &ram[0x1000..0x1800];
        let cands = extract_asset_candidates(charset_bytes, "sess1", Some("disk"));
        // the charset candidate hashes the whole 2KB → must match RAM hash at $1000.
        let node = resolve_visible_node_at(&cp, 36.0, 39.0, None);
        let charset = node.refs.iter().find(|r| r.kind == "charset").unwrap();
        assert_eq!(charset.addr, 0x1000 + 0x41 * 8);
        let (result, _k) = resolve_visual_origin(&cp, &node, &cands, "sess1");
        assert_eq!(result["classification"], "exact_asset", "charset bytes match the candidate");
    }
}
