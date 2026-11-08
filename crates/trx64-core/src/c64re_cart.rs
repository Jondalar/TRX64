//! c64re_cart.rs — the cartridge of a format 2 `.c64re` (Spec 919 D7, D10).
//!
//! Format 2 holds a cartridge once, as section 5: the contents at the freeze, laid out the
//! way the board's firmware lays them out in cartridge memory. Every CHIP packet goes to
//! `bank * $4000 + (load & $2000)`: ROML at the bottom of a 16 KiB slot, ROMH on top of it,
//! erased flash `$FF`. An 8 KiB-bank cartridge uses the lower half of one slot per bank.
//! A flash cartridge always has its whole chip (the firmware's `flash_chip_slots`).
//!
//! TRX64 keeps the `.crt` as a skeleton (header, packet headers, the few payloads that are not
//! flat contents) and the flash as the flat image; the load rebuilds `cartBytes` and
//! `cartFlash` from the two.

use base64::Engine as _;
use serde_json::{json, Value};

use crate::cart::{self, BankInfo, MapperType};
use crate::native_snapshot::{ta_u8, ta_u8_decode};

/// One 16 KiB slot of cartridge memory.
pub const SLOT: usize = 0x4000;
/// The 93C86 of a GMod2.
pub const EEPROM_BYTES: usize = 2048;
/// Where the EasyFlash IO2 RAM (`$DF00-$DFFF`) sits in the board's 64 KiB cartridge RAM window
/// (`c64_cartridge.v`: IO2 at 0, IO1 at $100). Only the EasyFlash family has it.
pub const IO2_RAM_AT: usize = 0;
pub const IO2_RAM_BYTES: usize = 256;
/// The board's cartridge RAM window.
pub const CART_RAM_BYTES: usize = 65536;

// ── D7 — the firmware's cart byte and TRX64's mapper ────────────────────────────────────────

/// The firmware's `C64_CARTRIDGE_TYPE` (`base | variant << 5`) for every mapper TRX64 has a
/// board counterpart for. `0x08` is Ocean; an Ocean with `$A000` chips is `0x69` on the board
/// (`OCEAN_16K | VARIANT_3`), which reads back as the same mapper.
pub const FW_CART_TABLE: &[(u8, MapperType)] = &[
    (0x41, MapperType::Normal8k),
    (0x01, MapperType::Normal16k),
    (0xA1, MapperType::Ultimax),
    (0x08, MapperType::Ocean),
    (0x28, MapperType::MagicDesk),
    (0xA8, MapperType::MagicDesk16),
    (0x11, MapperType::EasyFlash),
    (0x31, MapperType::EasyFlashXl),
    (0x12, MapperType::EasyFlash3),
    (0x48, MapperType::Gmod2),
    (0x0F, MapperType::MegaByter),
    (0x2F, MapperType::TwoMegabyter),
    (0x68, MapperType::C64MegaCart),
    (0x1B, MapperType::ActionReplay),
];

/// The Ocean cart byte for a cartridge with `$A000` chips (`OCEAN_16K | VARIANT_3`).
pub const FW_OCEAN_16K: u8 = 0x69;

/// The mapper a firmware cart byte names, or `None` for a cartridge this build has no mapper for.
pub fn mapper_of_fw(b: u8) -> Option<MapperType> {
    if let Some((_, m)) = FW_CART_TABLE.iter().find(|(c, _)| *c == b) {
        return Some(*m);
    }
    match (b & 0x1F, b >> 5) {
        (0x09, _) => Some(MapperType::Ocean), // OCEAN_16K, any variant
        (0x01, 1) => Some(MapperType::Ultimax), // UMAX variant 1 (5 = serving the VIC)
        _ => None,
    }
}

/// The firmware cart byte of a mapper; `None` for one the firmware has no byte for.
pub fn fw_of_mapper(m: MapperType, ocean_16k: bool) -> Option<u8> {
    if m == MapperType::Ocean && ocean_16k {
        return Some(FW_OCEAN_16K);
    }
    FW_CART_TABLE.iter().find(|(_, t)| *t == m).map(|(c, _)| *c)
}

fn hw_type_of(m: MapperType) -> u16 {
    match m {
        MapperType::Normal8k | MapperType::Normal16k | MapperType::Ultimax => 0,
        MapperType::ActionReplay => 1,
        MapperType::Ocean => 5,
        MapperType::MagicDesk => 19,
        MapperType::MagicDesk16 => 85,
        MapperType::EasyFlash => 32,
        MapperType::EasyFlashXl => 232,
        MapperType::Gmod2 => 60,
        MapperType::MegaByter => 86,
        MapperType::C64MegaCart => 88,
        MapperType::TwoMegabyter => 89,
        MapperType::EasyFlash3 => 90,
        _ => 0,
    }
}

fn exrom_game_of(m: MapperType) -> (u8, u8) {
    match m {
        MapperType::Normal8k => (0, 1),
        MapperType::Normal16k => (0, 0),
        MapperType::Ultimax => (1, 0),
        MapperType::Ocean => (0, 0),
        MapperType::MagicDesk => (0, 1),
        MapperType::MagicDesk16 => (0, 0),
        MapperType::EasyFlash | MapperType::EasyFlashXl | MapperType::EasyFlash3 => (1, 0),
        MapperType::Gmod2 | MapperType::MegaByter | MapperType::C64MegaCart => (0, 1),
        MapperType::TwoMegabyter => (0, 0),
        MapperType::ActionReplay => (0, 1),
        _ => (0, 1),
    }
}

pub fn fw_name(m: MapperType) -> &'static str {
    match m {
        MapperType::Normal8k => "8K cartridge",
        MapperType::Normal16k => "16K cartridge",
        MapperType::Ultimax => "Ultimax cartridge",
        MapperType::Ocean => "Ocean",
        MapperType::MagicDesk => "Magic Desk",
        MapperType::MagicDesk16 => "Magic Desk 16",
        MapperType::EasyFlash => "EasyFlash",
        MapperType::EasyFlashXl => "EasyFlash XL",
        MapperType::EasyFlash3 => "EasyFlash 3",
        MapperType::Gmod2 => "GMod2",
        MapperType::MegaByter => "MegaByter",
        MapperType::TwoMegabyter => "TwoMegabyter",
        MapperType::C64MegaCart => "C64MegaCart",
        MapperType::ActionReplay => "Action Replay",
        _ => "cartridge",
    }
}

/// `cart_def.require` / `.prohibit` the firmware's `configure_cart` derives (with the TRX64
/// bitstream's `CART_REU | CART_MAXREU` on top), from the CRT header's subtype byte.
fn require_prohibit(m: MapperType, subtype: u8) -> (u16, u16) {
    const REU: u16 = 0x0C;
    const DEXX: u16 = 0x01;
    const IO: u16 = 0x9F;
    match m {
        MapperType::Normal8k | MapperType::Normal16k | MapperType::Ultimax => (0, REU),
        MapperType::ActionReplay | MapperType::C64MegaCart => (0, IO),
        MapperType::EasyFlash | MapperType::EasyFlashXl | MapperType::EasyFlash3 => {
            (0x40, if subtype == 1 { 0x83 | REU } else { IO })
        }
        _ => (0, DEXX | REU),
    }
}

/// The banking registers the board replays as bus writes: nothing, `$DE00`, or `$DE00` and `$DE02`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Regs {
    None,
    De00,
    De00De02,
}

fn regs_of(m: MapperType) -> Regs {
    match m {
        MapperType::Normal8k | MapperType::Normal16k | MapperType::Ultimax => Regs::None,
        MapperType::Ocean | MapperType::MagicDesk | MapperType::MagicDesk16 | MapperType::Gmod2 => Regs::De00,
        _ => Regs::De00De02,
    }
}

// ── geometry of the flash ───────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Geom {
    /// `lo ++ hi`, 8 KiB per bank each (EasyFlash, EasyFlash XL).
    Ef,
    /// One flash, 8 KiB banks (GMod2 with its EEPROM behind it, MegaByter, C64MegaCart).
    Lin8,
    /// One flash, 16 KiB banks (TwoMegabyter).
    Lin16,
    /// The EasyFlash 3's 8 MB chip in chip order.
    Ef3,
}

fn geom_of(m: MapperType) -> Option<Geom> {
    match m {
        MapperType::EasyFlash | MapperType::EasyFlashXl => Some(Geom::Ef),
        MapperType::Gmod2 | MapperType::MegaByter | MapperType::C64MegaCart => Some(Geom::Lin8),
        MapperType::TwoMegabyter => Some(Geom::Lin16),
        MapperType::EasyFlash3 => Some(Geom::Ef3),
        _ => None,
    }
}

/// The slots of a flash cartridge's whole chip (`flash_chip_slots`): a flash cartridge's image
/// always has all of them, the banks its file leaves out are erased flash.
fn flash_slots(m: MapperType) -> usize {
    match m {
        MapperType::EasyFlash | MapperType::Gmod2 => 64,
        MapperType::MegaByter | MapperType::TwoMegabyter => 128,
        MapperType::C64MegaCart | MapperType::EasyFlashXl => 256,
        MapperType::EasyFlash3 => 512,
        _ => 0,
    }
}

/// The live writable image laid over the flat contents. `Some(eeprom)` when it fits the
/// geometry (the EEPROM only for a GMod2), `None` when it does not.
fn overlay(m: MapperType, flat: &mut [u8], w: &[u8]) -> Option<Option<Vec<u8>>> {
    match geom_of(m)? {
        Geom::Ef => {
            if w.is_empty() || w.len() % 2 != 0 || (w.len() / 2) % 0x2000 != 0 {
                return None;
            }
            let l = w.len() / 2;
            let banks = l / 0x2000;
            if flat.len() != banks * SLOT {
                return None;
            }
            for b in 0..banks {
                flat[b * SLOT..b * SLOT + 0x2000].copy_from_slice(&w[b * 0x2000..(b + 1) * 0x2000]);
                flat[b * SLOT + 0x2000..(b + 1) * SLOT].copy_from_slice(&w[l + b * 0x2000..l + (b + 1) * 0x2000]);
            }
            Some(None)
        }
        Geom::Lin8 => {
            let ee = if m == MapperType::Gmod2 { EEPROM_BYTES } else { 0 };
            if w.len() < ee || (w.len() - ee) % 0x2000 != 0 || w.len() == ee {
                return None;
            }
            let banks = (w.len() - ee) / 0x2000;
            if flat.len() != banks * SLOT {
                return None;
            }
            for b in 0..banks {
                flat[b * SLOT..b * SLOT + 0x2000].copy_from_slice(&w[b * 0x2000..(b + 1) * 0x2000]);
            }
            Some(if ee > 0 { Some(w[w.len() - ee..].to_vec()) } else { None })
        }
        Geom::Lin16 => {
            if w.len() != flat.len() {
                return None;
            }
            flat.copy_from_slice(w);
            Some(None)
        }
        Geom::Ef3 => {
            if w.len() != crate::ef3::EF3_FLASH_SIZE || flat.len() != crate::ef3::EF3_BANKS as usize * SLOT {
                return None;
            }
            for key in 0..crate::ef3::EF3_BANKS as usize {
                let (slot, b) = ((key >> 6) as u8, (key & 63) as u8);
                for (romh, at) in [(false, 0usize), (true, 0x2000usize)] {
                    let a = crate::ef3::ef3_flash_addr(slot, b, romh, 0);
                    flat[key * SLOT + at..key * SLOT + at + 0x2000].copy_from_slice(&w[a..a + 0x2000]);
                }
            }
            Some(None)
        }
    }
}

/// The writable image (`cartFlash`) of a mapper out of the flat contents.
pub fn writable_from_flat(m: MapperType, flat: &[u8], eeprom: Option<&[u8]>) -> Option<Vec<u8>> {
    match geom_of(m)? {
        Geom::Ef => {
            if flat.is_empty() || flat.len() % SLOT != 0 {
                return None;
            }
            let banks = flat.len() / SLOT;
            let mut w = vec![0xFFu8; banks * 0x4000];
            let l = banks * 0x2000;
            for b in 0..banks {
                w[b * 0x2000..(b + 1) * 0x2000].copy_from_slice(&flat[b * SLOT..b * SLOT + 0x2000]);
                w[l + b * 0x2000..l + (b + 1) * 0x2000].copy_from_slice(&flat[b * SLOT + 0x2000..(b + 1) * SLOT]);
            }
            Some(w)
        }
        Geom::Lin8 => {
            if flat.is_empty() || flat.len() % SLOT != 0 {
                return None;
            }
            let banks = flat.len() / SLOT;
            let mut w = Vec::with_capacity(banks * 0x2000 + EEPROM_BYTES);
            for b in 0..banks {
                w.extend_from_slice(&flat[b * SLOT..b * SLOT + 0x2000]);
            }
            if m == MapperType::Gmod2 {
                match eeprom {
                    Some(e) if e.len() == EEPROM_BYTES => w.extend_from_slice(e),
                    _ => w.extend(std::iter::repeat(0xFFu8).take(EEPROM_BYTES)),
                }
            }
            Some(w)
        }
        Geom::Lin16 => Some(flat.to_vec()),
        Geom::Ef3 => {
            if flat.len() != crate::ef3::EF3_BANKS as usize * SLOT {
                return None;
            }
            let mut w = vec![0xFFu8; crate::ef3::EF3_FLASH_SIZE];
            for key in 0..crate::ef3::EF3_BANKS as usize {
                let (slot, b) = ((key >> 6) as u8, (key & 63) as u8);
                for (romh, at) in [(false, 0usize), (true, 0x2000usize)] {
                    let a = crate::ef3::ef3_flash_addr(slot, b, romh, 0);
                    w[a..a + 0x2000].copy_from_slice(&flat[key * SLOT + at..key * SLOT + at + 0x2000]);
                }
            }
            Some(w)
        }
    }
}

// ── the .crt as packets ─────────────────────────────────────────────────────────────────────

fn be16(b: &[u8], o: usize) -> usize {
    ((b[o] as usize) << 8) | b[o + 1] as usize
}
fn be32(b: &[u8], o: usize) -> usize {
    ((b[o] as usize) << 24) | ((b[o + 1] as usize) << 16) | ((b[o + 2] as usize) << 8) | b[o + 3] as usize
}

#[derive(Clone)]
struct Pkt {
    hdr: [u8; 16],
    bank: usize,
    load: usize,
    size: usize,
    ty: usize,
}

impl Pkt {
    fn from(b: &[u8], o: usize) -> Pkt {
        let mut hdr = [0u8; 16];
        hdr.copy_from_slice(&b[o..o + 16]);
        Pkt { hdr, bank: be16(b, o + 10), load: be16(b, o + 12), size: be16(b, o + 14), ty: be16(b, o + 8) }
    }
    fn is_eeprom(&self) -> bool {
        self.load == 0xDE00
    }
    /// A packet whose payload is cartridge memory (the firmware reads type 0 and 2).
    fn placed(&self) -> bool {
        !self.is_eeprom() && self.size > 0 && (self.ty == 0 || self.ty == 2)
    }
    fn offset(&self) -> usize {
        self.bank * SLOT + (self.load & 0x2000)
    }
}

/// Header length, the CHIP packets, and where the packets end. `None` for a file the format
/// cannot describe: a packet whose length is not its header plus its payload.
fn walk(crt: &[u8]) -> Option<(usize, Vec<Pkt>, usize)> {
    if !cart::is_crt(crt) || crt.len() < 0x40 {
        return None;
    }
    let hl = be32(crt, 0x10);
    if hl < 0x40 || hl > crt.len() {
        return None;
    }
    let mut off = hl;
    let mut v = Vec::new();
    while off + 16 <= crt.len() && &crt[off..off + 4] == b"CHIP" {
        let p = Pkt::from(crt, off);
        if be32(crt, off + 4) != 16 + p.size || off + 16 + p.size > crt.len() {
            return None;
        }
        off += 16 + p.size;
        v.push(p);
    }
    Some((hl, v, off))
}

/// The firmware's `read_chip_packet` + `auto_mirror`: the packets laid into cartridge memory.
fn lay_out(crt: &[u8], pkts: &[Pkt], hl: usize, min_banks: usize) -> Option<Vec<u8>> {
    let mut at = hl;
    let mut highest = 0usize;
    for p in pkts {
        if p.load == 0xC000 || p.size == 0x8000 {
            return None; // a C128 cartridge: 32 KiB banks
        }
        if p.placed() {
            if !matches!(p.load, 0x8000 | 0xA000 | 0xE000) {
                return None;
            }
            highest = highest.max(p.bank);
        }
        if p.is_eeprom() && p.size > EEPROM_BYTES {
            return None;
        }
    }
    let mut banks = 1usize;
    while banks <= highest {
        banks <<= 1;
    }
    banks = banks.max(min_banks);
    let mut flat = vec![0xFFu8; banks * SLOT];
    for p in pkts {
        let data = &crt[at + 16..at + 16 + p.size];
        at += 16 + p.size;
        if !p.placed() {
            continue;
        }
        let off = p.offset();
        if off + p.size > flat.len() {
            return None;
        }
        flat[off..off + p.size].copy_from_slice(data);
        if p.size < 8192 {
            let mut done = p.size;
            let mut i = 256usize;
            while i < p.size {
                if done < i {
                    done = i;
                    break;
                }
                i *= 2;
            }
            while done < 8192 {
                let n = done.min(8192 - done);
                flat.copy_within(off..off + n, off + done);
                done <<= 1;
            }
        }
    }
    Some(flat)
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}
fn unb64(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .map_err(|e| format!("cartridge skeleton: bad base64: {e}"))
}

/// The `.crt` without the contents the flat image holds. A packet is `[header, 0]` (payload in
/// the flat image), `[header, 1]` (the EEPROM) or `[header, 2, payload]` (kept as it is).
fn skeleton(crt: &[u8], hl: usize, pkts: &[Pkt], end: usize) -> Value {
    let mut at = hl;
    let mut packets = Vec::with_capacity(pkts.len());
    for p in pkts {
        let data = &crt[at + 16..at + 16 + p.size];
        at += 16 + p.size;
        let hdr = b64(&p.hdr);
        packets.push(if p.placed() {
            json!([hdr, 0])
        } else if p.is_eeprom() {
            json!([hdr, 1])
        } else {
            json!([hdr, 2, b64(data)])
        });
    }
    json!({ "head": b64(&crt[..hl]), "packets": packets, "tail": b64(&crt[end..]) })
}

/// `cartBytes` from the skeleton and the contents.
pub fn rebuild_crt(sk: &Value, flat: &[u8], eeprom: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let s = |k: &str| sk.get(k).and_then(|v| v.as_str()).ok_or_else(|| format!("cartridge skeleton has no {k}"));
    let mut out = unb64(s("head")?)?;
    for pk in sk.get("packets").and_then(|v| v.as_array()).ok_or("cartridge skeleton has no packets")? {
        let a = pk.as_array().ok_or("cartridge skeleton: bad packet")?;
        let hdr = unb64(a.first().and_then(|v| v.as_str()).ok_or("cartridge skeleton: bad packet")?)?;
        if hdr.len() != 16 {
            return Err("cartridge skeleton: bad packet header".into());
        }
        let p = Pkt::from(&hdr, 0);
        out.extend_from_slice(&hdr);
        match a.get(1).and_then(|v| v.as_u64()) {
            Some(0) => {
                let off = p.offset();
                let d = flat.get(off..off + p.size).ok_or("cartridge skeleton: a packet lies outside the contents")?;
                out.extend_from_slice(d);
            }
            Some(1) => {
                let e = eeprom.ok_or("cartridge skeleton: the EEPROM section is missing")?;
                let d = e.get(..p.size).ok_or("cartridge skeleton: the EEPROM is shorter than its packet")?;
                out.extend_from_slice(d);
            }
            Some(2) => out.extend_from_slice(&unb64(a.get(2).and_then(|v| v.as_str()).ok_or("cartridge skeleton: bad payload")?)?),
            _ => return Err("cartridge skeleton: bad packet kind".into()),
        }
    }
    out.extend_from_slice(&unb64(s("tail")?)?);
    Ok(out)
}

// ── the cartridge of a checkpoint, for the writer ───────────────────────────────────────────

/// What the writer takes out of a checkpoint's cartridge nodes.
pub struct CartPlan {
    pub mapper: MapperType,
    pub fw: u8,
    /// Section 5.
    pub flat: Vec<u8>,
    /// Section 7 (GMod2).
    pub eeprom: Option<Vec<u8>>,
    /// The EasyFlash IO2 RAM, for section 4.
    pub io_ram: Option<Vec<u8>>,
    /// `cartFlash` existed (a writable image), and is rebuilt from the flat contents.
    pub has_flash: bool,
    pub require: u16,
    pub prohibit: u16,
    /// The `$DE00` / `$DE02` values for BLOCK word 26.
    pub regs: (u8, u8),
    /// What the TRX64 section keeps of the `.crt`.
    pub skeleton: Value,
}

impl CartPlan {
    /// The `trx64.cart` node of the TRX64 section.
    pub fn node(&self) -> Value {
        let mut n = self.skeleton.clone();
        n["mapper"] = json!(format!("{:?}", self.mapper));
        n["fw"] = json!(self.fw);
        n["flash"] = json!(self.has_flash);
        n["ioRam"] = json!(self.io_ram.is_some());
        n["eeprom"] = json!(self.eeprom.is_some());
        n
    }
}

fn u8s(v: &Value) -> Option<Vec<u8>> {
    v.as_array()?.iter().map(|x| x.as_u64().filter(|&b| b <= 255).map(|b| b as u8)).collect()
}

/// The cartridge of `cp` as format 2 holds it, or `None` when the board's table has nothing
/// for it or the `.crt` is not one the flat layout reproduces exactly: then no section 5 is
/// written, `cartridgeType` is absent and the bytes stay in the TRX64 section.
pub fn plan_for_write(cp: &Value) -> Option<CartPlan> {
    let crt = ta_u8_decode(cp.get("cartBytes")?)?;
    let (hl, pkts, end) = walk(&crt)?;
    let image = cart::parse_crt(&crt, "", None).ok()?;
    let mapper = image.mapper_type;
    let ocean_16k = pkts.iter().any(|p| p.load == 0xA000);
    let fw = fw_of_mapper(mapper, ocean_16k)?;
    let min_banks = flash_slots(mapper);
    let flat0 = lay_out(&crt, &pkts, hl, min_banks)?;
    // the layout has to give the file back, packet for packet, before it is trusted
    let sk = skeleton(&crt, hl, &pkts, end);
    let mut at = hl;
    let mut crt_eeprom: Option<Vec<u8>> = None;
    for p in &pkts {
        if p.is_eeprom() {
            crt_eeprom = Some(crt[at + 16..at + 16 + p.size].to_vec());
        }
        at += 16 + p.size;
    }
    if rebuild_crt(&sk, &flat0, crt_eeprom.as_deref()).ok()? != crt {
        return None;
    }
    if crt_eeprom.is_some() && mapper != MapperType::Gmod2 {
        return None;
    }
    let state = cp.get("cartState").filter(|s| !s.is_null());
    let mut flat = flat0;
    let mut eeprom: Option<Vec<u8>> = None;
    let mut has_flash = false;
    if let Some(w) = cp.get("cartFlash").and_then(ta_u8_decode).filter(|w| !w.is_empty()) {
        eeprom = overlay(mapper, &mut flat, &w)?;
        if writable_from_flat(mapper, &flat, eeprom.as_deref())? != w {
            return None;
        }
        has_flash = true;
    }
    if mapper == MapperType::Gmod2 {
        // the saved game of a GMod2 is its EEPROM; the live one is in cartState
        let live = state
            .and_then(|s| s.get("eepromState"))
            .and_then(|e| e.get("data"))
            .and_then(u8s)
            .filter(|d| d.len() == EEPROM_BYTES);
        eeprom = Some(live.or(eeprom).or(crt_eeprom)?);
    }
    let io_ram = match mapper {
        MapperType::EasyFlash | MapperType::EasyFlashXl | MapperType::EasyFlash3 => state
            .and_then(|s| s.get("easyflashRam"))
            .and_then(u8s)
            .filter(|d| d.len() == IO2_RAM_BYTES),
        _ => None,
    };
    let regs = regs_from_state(mapper, state);
    let (require, prohibit) = require_prohibit(mapper, crt[0x1A]);
    Some(CartPlan { mapper, fw, flat, eeprom, io_ram, has_flash, require, prohibit, regs, skeleton: sk })
}

/// The cartridge nodes of a checkpoint without what sections 4, 5 and 7 hold.
pub fn strip(cp: &mut Value, plan: &CartPlan) {
    cp["cartBytes"] = Value::Null;
    if plan.has_flash {
        cp["cartFlash"] = Value::Null;
    }
    if let Some(s) = cp.get_mut("cartState").filter(|s| s.is_object()) {
        if plan.io_ram.is_some() {
            s.as_object_mut().unwrap().remove("easyflashRam");
        }
        if plan.eeprom.is_some() {
            if let Some(e) = s.get_mut("eepromState").and_then(|e| e.as_object_mut()) {
                e.remove("data");
            }
        }
    }
}

/// The opposite of [`strip`]: `cartBytes`, `cartFlash`, the IO2 RAM and the EEPROM array put
/// back from the sections.
pub fn restore(
    cp: &mut Value,
    node: &Value,
    flat: &[u8],
    cartram: Option<&[u8]>,
    eeprom: Option<&[u8]>,
) -> Result<(), String> {
    let mapper_name = node.get("mapper").and_then(|v| v.as_str()).unwrap_or("");
    let fw = node.get("fw").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
    let mapper = mapper_of_fw(fw).filter(|m| format!("{m:?}") == mapper_name).ok_or_else(|| {
        format!("the TRX64 section names a {mapper_name} cartridge (type {fw:02x}) this build does not know")
    })?;
    let has_eeprom = node.get("eeprom").and_then(|v| v.as_bool()).unwrap_or(false);
    let crt = rebuild_crt(node, flat, if has_eeprom { eeprom } else { None })?;
    cp["cartBytes"] = ta_u8(&crt);
    if node.get("flash").and_then(|v| v.as_bool()).unwrap_or(false) {
        let w = writable_from_flat(mapper, flat, eeprom).ok_or("the cartridge contents do not fit the flash of their type")?;
        cp["cartFlash"] = ta_u8(&w);
    }
    if node.get("ioRam").and_then(|v| v.as_bool()).unwrap_or(false) {
        let c = cartram.ok_or("the TRX64 section needs the cartridge RAM section, which is missing")?;
        let io = c.get(IO2_RAM_AT..IO2_RAM_AT + IO2_RAM_BYTES).ok_or("the cartridge RAM section is short")?;
        cp["cartState"]["easyflashRam"] = json!(io.iter().map(|&b| b as i64).collect::<Vec<_>>());
    }
    if has_eeprom {
        let e = eeprom.ok_or("the TRX64 section needs the EEPROM section, which is missing")?;
        cp["cartState"]["eepromState"]["data"] = json!(e.iter().map(|&b| b as i64).collect::<Vec<_>>());
    }
    Ok(())
}

// ── the cartridge of a board file, for the reader ───────────────────────────────────────────

/// A board cartridge as TRX64 holds one.
pub struct BoardCart {
    pub mapper: MapperType,
    pub crt: Vec<u8>,
    pub flash: Option<Vec<u8>>,
    pub state: Value,
    /// What was taken from where, for the report.
    pub notes: Vec<String>,
}

fn synth_crt(m: MapperType, flat: &[u8]) -> Vec<u8> {
    let (exrom, game) = exrom_game_of(m);
    let mut out = vec![0u8; 0x40];
    out[..16].copy_from_slice(b"C64 CARTRIDGE   ");
    out[0x10..0x14].copy_from_slice(&0x40u32.to_be_bytes());
    out[0x14..0x16].copy_from_slice(&0x0100u16.to_be_bytes());
    out[0x16..0x18].copy_from_slice(&hw_type_of(m).to_be_bytes());
    out[0x18] = exrom;
    out[0x19] = game;
    let name = b"TRX64 state";
    out[0x20..0x20 + name.len()].copy_from_slice(name);
    let ty: u16 = if geom_of(m).is_some() { 2 } else { 0 };
    let sixteen = matches!(m, MapperType::Normal16k | MapperType::Ultimax);
    let upper_load = if m == MapperType::Ultimax { 0xE000u16 } else { 0xA000 };
    for slot in 0..flat.len() / SLOT {
        let lo = &flat[slot * SLOT..slot * SLOT + 0x2000];
        let hi = &flat[slot * SLOT + 0x2000..(slot + 1) * SLOT];
        let blank = |h: &[u8]| h.iter().all(|&b| b == 0xFF);
        let mut emit = |load: u16, data: &[u8]| {
            out.extend_from_slice(b"CHIP");
            out.extend_from_slice(&(0x10u32 + 0x2000).to_be_bytes());
            out.extend_from_slice(&ty.to_be_bytes());
            out.extend_from_slice(&(slot as u16).to_be_bytes());
            out.extend_from_slice(&load.to_be_bytes());
            out.extend_from_slice(&0x2000u16.to_be_bytes());
            out.extend_from_slice(data);
        };
        if slot == 0 || !blank(lo) {
            emit(0x8000, lo);
        }
        if !blank(hi) || (slot == 0 && sixteen) {
            emit(upper_load, hi);
        }
    }
    out
}

/// `flat` — section 5 of a board file — as a TRX64 cartridge: the `.crt` (the file `source`
/// names when it is there and fits, else one laid out from the contents), the flash, and the
/// mapper's state with the banking registers replayed as the board replays them.
pub fn from_board(
    fw: u8,
    flat: &[u8],
    eeprom: Option<&[u8]>,
    cartram: Option<&[u8]>,
    regs: (u8, u8),
    source: Option<Vec<u8>>,
) -> Result<BoardCart, String> {
    let mapper = mapper_of_fw(fw).ok_or_else(|| format!("cartridge type {fw:02x} has no mapper in this build"))?;
    if flat.is_empty() || flat.len() % SLOT != 0 {
        return Err(format!("the cartridge section is {} bytes, not a whole number of 16 KiB banks", flat.len()));
    }
    let mut notes = Vec::new();
    let mut crt: Option<Vec<u8>> = None;
    if let Some(src) = source {
        let fits = (|| {
            let (hl, pkts, end) = walk(&src)?;
            let img = cart::parse_crt(&src, "", None).ok()?;
            let mut at = hl;
            let mut ee: Option<Vec<u8>> = None;
            for p in &pkts {
                if p.is_eeprom() {
                    ee = Some(src[at + 16..at + 16 + p.size].to_vec());
                }
                at += 16 + p.size;
            }
            let f0 = lay_out(&src, &pkts, hl, flash_slots(img.mapper_type))?;
            let sk = skeleton(&src, hl, &pkts, end);
            if img.mapper_type != mapper || f0.len() != flat.len() || rebuild_crt(&sk, &f0, ee.as_deref()).ok()? != src {
                return None;
            }
            let e = eeprom.map(|e| e.to_vec()).or(ee);
            rebuild_crt(&sk, flat, e.as_deref()).ok()
        })();
        match fits {
            Some(c) => {
                crt = Some(c);
                notes.push("cartridge packets from the .crt the file names".to_string());
            }
            None => notes.push("the .crt the file names does not fit its contents: packets laid out from the contents".to_string()),
        }
    }
    let crt = crt.unwrap_or_else(|| synth_crt(mapper, flat));
    let flash = if geom_of(mapper).is_some() { writable_from_flat(mapper, flat, eeprom) } else { None };
    let (_img, mut mp) = cart::load_cartridge_from_bytes(&crt, "state", Some(mapper))
        .map_err(|e| format!("cartridge rebuilt from the file does not load: {e}"))?;
    if let Some(w) = &flash {
        mp.set_writable_image(w);
    }
    let bi = BankInfo {
        cpu_port_direction: 0x2F,
        cpu_port_value: 0x37,
        basic_visible: true,
        kernal_visible: true,
        io_visible: true,
        char_visible: false,
        cartridge_attached: true,
        cartridge_exrom: None,
        cartridge_game: None,
        phi1: 0xFF,
        ram_under: 0,
    };
    match regs_of(mapper) {
        Regs::None => {}
        Regs::De00 => {
            mp.write(0xDE00, regs.0, &bi, 0);
        }
        Regs::De00De02 => {
            mp.write(0xDE00, regs.0, &bi, 0);
            mp.write(0xDE02, regs.1, &bi, 0);
        }
    }
    let mut st = serde_json::to_value(crate::c64re_snapshot::capture_cart_state(mp.as_ref()))
        .map_err(|e| e.to_string())?;
    if matches!(mapper, MapperType::EasyFlash | MapperType::EasyFlashXl | MapperType::EasyFlash3) {
        if let Some(io) = cartram.and_then(|c| c.get(IO2_RAM_AT..IO2_RAM_AT + IO2_RAM_BYTES)) {
            st["easyflashRam"] = json!(io.iter().map(|&b| b as i64).collect::<Vec<_>>());
        }
    }
    if let (Some(e), Some(es)) = (eeprom, st.get_mut("eepromState")) {
        es["data"] = json!(e.iter().map(|&b| b as i64).collect::<Vec<_>>());
    }
    Ok(BoardCart { mapper, crt, flash, state: st, notes })
}

/// The `$DE00` / `$DE02` values of the board's BLOCK word 26 for a mapper, out of its `cartState`.
fn regs_from_state(mapper: MapperType, state: Option<&Value>) -> (u8, u8) {
    let cur = state.and_then(|s| s.get("currentBank")).and_then(|v| v.as_u64()).unwrap_or(0) as u8;
    let ctl = state.and_then(|s| s.get("controlRegister")).and_then(|v| v.as_u64()).map(|v| v as u8);
    match regs_of(mapper) {
        Regs::None => (0, 0),
        Regs::De00 => (ctl.unwrap_or(cur), 0),
        Regs::De00De02 => (cur, ctl.unwrap_or(0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_maps_both_ways() {
        for (b, m) in FW_CART_TABLE {
            assert_eq!(mapper_of_fw(*b), Some(*m), "{b:02x}");
            assert_eq!(fw_of_mapper(*m, false), Some(*b), "{m:?}");
        }
        assert_eq!(mapper_of_fw(FW_OCEAN_16K), Some(MapperType::Ocean));
        assert_eq!(fw_of_mapper(MapperType::Ocean, true), Some(FW_OCEAN_16K));
        assert_eq!(mapper_of_fw(0x11), Some(MapperType::EasyFlash));
        assert_eq!(mapper_of_fw(0x2F), Some(MapperType::TwoMegabyter));
        assert_eq!(mapper_of_fw(0x00), None);
        assert_eq!(fw_of_mapper(MapperType::Gmod4, false), None);
    }
}
