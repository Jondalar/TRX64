//! cart.rs — STRICT 1:1 port of the c64re TS cartridge layer (read-only mapper
//! tier). Source of truth (port VERBATIM, `ts:` line tags):
//!
//!   C64ReverseEngineeringMCP/src/runtime/headless/cartridge.ts
//!     CrtBank/ParsedCartridgeImage (ts:14-32), parseCrt (ts:156-214),
//!     inferMapperType (ts:226-263), HeadlessCartridgeMapper (ts:39-107),
//!     BaseMapper (ts:320-421), Normal8k/16k/Ultimax (ts:423-435),
//!     MagicDeskMapper (ts:440-459), MagicDesk16Mapper (ts:463-482),
//!     OceanMapper (ts:487-514), bankMaskForImage/totalImageBytes (ts:308-318),
//!     normalizeBankData (ts:216-224), cloneBankData (ts:273-279),
//!     readU16Be/readU32Be (ts:265-271), mapperFromImage (ts:120-154).
//!   types.ts: HeadlessBankInfo (ts:11-22), HeadlessCartridgeMapperType (ts:40-52).
//!
//! Scope: READ-ONLY mappers only (Normal 8K/16K/Ultimax, Magic Desk, Magic
//! Desk 16, Ocean). The flash/EEPROM/serial tier (EasyFlash, MegaByter, GMOD2/3,
//! C64MegaCart) is OUT OF SCOPE (ADR-066) — those CRT hardware types parse but
//! produce no mapper.
//!
//! In BOTH the TS oracle and VICE the cartridge is a PER-MEMORY-ACCESS bus hook,
//! NOT a clocked device (no `cartridge.tick()`): the mapper is consulted
//! synchronously from the FullBus `read()`/`write()` (full.rs). So no tick.

use std::collections::BTreeMap;

/// ts:40-52 — HeadlessCartridgeMapperType (the read-only subset we build mappers
/// for; the flash/serial families parse but are unsupported in this tier).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapperType {
    Normal8k,
    Normal16k,
    Ultimax,
    MagicDesk,
    MagicDesk16,
    Ocean,
    /// A CRT hardware-type that maps to a flash/serial family (EasyFlash,
    /// MegaByter, GMOD2/3, C64MegaCart). Parsed but NOT built into a mapper in
    /// this read-only tier.
    Unsupported,
}

/// ts:14-18 — CrtBank. Per-bank ROM windows; any may be absent.
#[derive(Clone, Default)]
pub struct CrtBank {
    pub roml: Option<[u8; 0x2000]>,     // $8000-$9FFF
    pub romh_a000: Option<[u8; 0x2000]>, // $A000-$BFFF
    pub romh_e000: Option<[u8; 0x2000]>, // $E000-$FFFF (ultimax ROMH)
}

/// ts:12 — CrtLoadProfile.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CrtLoadProfile {
    Roml,
    RomhA000,
    RomhE000,
}

/// ts:20-32 — ParsedCartridgeImage (the read-only-relevant fields; the writable
/// `rawBytes` is kept so a future writable tier can re-pack, but the read-only
/// mappers never use it for banking).
#[derive(Clone)]
pub struct ParsedCartridgeImage {
    pub path: String,
    pub name: String,
    pub mapper_type: MapperType,
    pub exrom: u8,
    pub game: u8,
    /// Bank index → CrtBank. BTreeMap so `keys()` iterate in ascending order
    /// (= the JS Map insertion/`Math.max` reduce ordering the mask helpers use).
    pub banks: BTreeMap<u16, CrtBank>,
    pub profiles: std::collections::BTreeSet<u8>, // bitset via CrtLoadProfile as u8
    pub raw_bytes: Vec<u8>,
}

/// ts:11-22 (types.ts) — HeadlessBankInfo. The banking-context struct passed into
/// every read/write/peek. Only `cpu_port_direction`/`cpu_port_value` are consumed
/// by a read path (GMOD3 fake-ultimax memconfig); the read-only mappers ignore
/// `bank_info` for read decisions (PLA-gated before the mapper is called) — the
/// visibility predicates are constant. Carried for 1:1 fidelity.
#[derive(Clone, Copy)]
pub struct BankInfo {
    pub cpu_port_direction: u8,
    pub cpu_port_value: u8,
    pub basic_visible: bool,
    pub kernal_visible: bool,
    pub io_visible: bool,
    pub char_visible: bool,
    pub cartridge_attached: bool,
    pub cartridge_exrom: Option<u8>,
    pub cartridge_game: Option<u8>,
}

/// ts:34-37 — HeadlessCartridgeLines: the EXROM/GAME expansion-port lines (each a
/// 0/1 level). No cart → both 1 (released).
#[derive(Clone, Copy)]
pub struct CartLines {
    pub exrom: u8,
    pub game: u8,
}

/// Persistable mapper continuation state (ts:54+ HeadlessCartridgeState subset
/// the read-only tier round-trips: currentBank + controlRegister). Used by
/// get_state/set_state for VSF/checkpoint parity.
#[derive(Clone, Copy, Default)]
pub struct CartState {
    pub current_bank: u16,
    pub control_register: Option<u8>,
}

/// ts:39-107 — HeadlessCartridgeMapper. The bus depends on read/write/peek,
/// get_lines, reset, get_state/set_state (the read-only surface; the writable
/// flash/clock/phi1 hooks are out of scope for this tier).
///
/// `Send` supertrait: the daemon owns the `Machine` (which owns the cartridge)
/// inside an `Arc<Mutex<State>>` moved across tokio tasks, so the trait object
/// must be `Send`. The read-only mappers are plain owned data ⇒ naturally `Send`.
pub trait CartMapper: Send {
    fn mapper_type(&self) -> MapperType;
    /// ts:45 getLines(): the EXROM/GAME lines for the live bank/mode.
    fn get_lines(&self) -> CartLines;
    /// ts:46 read(address, bankInfo): the ROM-window byte, or None ⇒ not handled
    /// (the bus falls back to RAM / open-bus).
    fn read(&self, address: u16, bank_info: &BankInfo) -> Option<u8>;
    /// ts:55 peek(address, bankInfo): side-effect-free read (== read for the
    /// read-only tier — pure array index, no command/latch mutation).
    fn peek(&self, address: u16, bank_info: &BankInfo) -> Option<u8> {
        self.read(address, bank_info)
    }
    /// ts:59 write(address, value, bankInfo) → consumed? true = the cart handled
    /// the write (does NOT fall through to RAM); false = pass to RAM underneath.
    fn write(&mut self, address: u16, value: u8, bank_info: &BankInfo) -> bool;
    /// ts:106/420 reset(): the expansion-port RESET line — bank + mode/control
    /// return to the boot config so GAME/EXROM re-vector $FFFC from the cart.
    fn reset(&mut self);
    fn get_state(&self) -> CartState;
    fn set_state(&mut self, state: CartState);
    /// Clone into a fresh box (the `Machine` derives `Clone` for snapshots; a
    /// `dyn` trait object is not `Clone` directly). Each read-only mapper's state
    /// is small (bank + register + the cloned bank ROM map), so this is cheap.
    fn clone_box(&self) -> Box<dyn CartMapper>;
}

impl Clone for Box<dyn CartMapper> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

// ── parse_crt support ───────────────────────────────────────────────────────

/// ts:8 — CRT_SIGNATURE.
const CRT_SIGNATURE: &[u8; 16] = b"C64 CARTRIDGE   ";

/// ts:265-267 — readU16Be.
#[inline]
fn read_u16_be(data: &[u8], offset: usize) -> u16 {
    let hi = *data.get(offset).unwrap_or(&0) as u16;
    let lo = *data.get(offset + 1).unwrap_or(&0) as u16;
    (hi << 8) | lo
}

/// ts:269-271 — readU32Be.
#[inline]
fn read_u32_be(data: &[u8], offset: usize) -> u32 {
    let b0 = *data.get(offset).unwrap_or(&0) as u32;
    let b1 = *data.get(offset + 1).unwrap_or(&0) as u32;
    let b2 = *data.get(offset + 2).unwrap_or(&0) as u32;
    let b3 = *data.get(offset + 3).unwrap_or(&0) as u32;
    (b0 << 24) | (b1 << 16) | (b2 << 8) | b3
}

/// ts:216-224 — normalizeBankData: copy `data` into a fixed-size $2000 window,
/// 0xFF-padding short data and truncating long data.
fn normalize_bank_data(data: &[u8]) -> [u8; 0x2000] {
    let mut result = [0xffu8; 0x2000];
    let n = data.len().min(0x2000);
    result[..n].copy_from_slice(&data[..n]);
    result
}

/// ts:226-263 — inferMapperType: the VICE hardwareType → mapper map. Returns None
/// only for a truly unknown hardware type (the bus then errors); a known
/// flash/serial type maps to `Unsupported`.
fn infer_mapper_type(
    hardware_type: u16,
    exrom: u8,
    game: u8,
    profiles: &std::collections::BTreeSet<u8>,
) -> Option<MapperType> {
    let has = |p: CrtLoadProfile| profiles.contains(&(p as u8));
    match hardware_type {
        0 => {
            // ts:234-242
            if has(CrtLoadProfile::RomhE000) || (exrom == 1 && game == 1) {
                Some(MapperType::Ultimax)
            } else if has(CrtLoadProfile::RomhA000) {
                Some(MapperType::Normal16k)
            } else if has(CrtLoadProfile::Roml) {
                Some(MapperType::Normal8k)
            } else {
                None
            }
        }
        5 => Some(MapperType::Ocean),       // ts:244-245
        19 => Some(MapperType::MagicDesk),  // ts:246-247 CARTRIDGE_MAGIC_DESK
        85 => Some(MapperType::MagicDesk16), // ts:248-249 CARTRIDGE_MAGIC_DESK_16
        // ts:250-259 — flash/serial families: parse but unsupported in this tier.
        86 => Some(MapperType::Unsupported), // megabyter
        60 => Some(MapperType::Unsupported), // gmod2
        61 => Some(MapperType::Unsupported), // c64megacart
        62 => Some(MapperType::Unsupported), // gmod3
        32 => Some(MapperType::Unsupported), // easyflash
        _ => None,                           // ts:260-261
    }
}

/// CRT parse error.
#[derive(Debug)]
pub enum CrtError {
    /// Not a "C64 CARTRIDGE   " image.
    NotCrt,
    /// Hardware type has no mapper mapping at all (truly unknown).
    UnknownHardware(u16),
    /// A known flash/serial family that this read-only tier does not implement.
    Unsupported(MapperType),
}

impl std::fmt::Display for CrtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CrtError::NotCrt => write!(f, "Not a CRT image"),
            CrtError::UnknownHardware(t) => {
                write!(f, "Unsupported CRT hardware type {t}. Pass mapper_type explicitly if the layout is known.")
            }
            CrtError::Unsupported(t) => {
                write!(f, "Unsupported cartridge type {t:?} — no authoritative read-only VICE implementation (flash/serial tier out of scope).")
            }
        }
    }
}

impl std::error::Error for CrtError {}

/// ts:156-214 — parseCrt. Verbatim: the "C64 CARTRIDGE" signature check,
/// headerLen/hardwareType/EXROM/GAME extraction, and the CHIP-packet walk into a
/// bank map.
pub fn parse_crt(
    data: &[u8],
    path: &str,
    mapper_type: Option<MapperType>,
) -> Result<ParsedCartridgeImage, CrtError> {
    // ts:157-159
    if data.len() < 16 || data[0..16] != CRT_SIGNATURE[..] {
        return Err(CrtError::NotCrt);
    }
    // ts:160-166
    let header_len = read_u32_be(data, 0x10) as usize;
    let hardware_type = read_u16_be(data, 0x16);
    let exrom = *data.get(0x18).unwrap_or(&0);
    let game = *data.get(0x19).unwrap_or(&0);
    let name = {
        let raw = data.get(0x20..0x40).unwrap_or(&[]);
        let zero = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        let s = String::from_utf8_lossy(&raw[..zero]).trim().to_string();
        if s.is_empty() {
            // ts:166 basename(path) fallback.
            std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string())
        } else {
            s
        }
    };

    // ts:168-197 — CHIP-packet walk.
    let mut banks: BTreeMap<u16, CrtBank> = BTreeMap::new();
    let mut profiles: std::collections::BTreeSet<u8> = std::collections::BTreeSet::new();
    let mut offset = header_len;
    while offset + 0x10 <= data.len() {
        // ts:172-174
        if data.get(offset..offset + 4) != Some(b"CHIP") {
            break;
        }
        let packet_len = read_u32_be(data, offset + 4) as usize;
        let bank = read_u16_be(data, offset + 10);
        let load_address = read_u16_be(data, offset + 12);
        let size = read_u16_be(data, offset + 14) as usize;
        let rom_end = (offset + 16 + size).min(data.len());
        let rom = &data[offset + 16..rom_end];
        let existing = banks.entry(bank).or_default();
        // ts:181-194
        if load_address == 0x8000 {
            existing.roml = Some(normalize_bank_data(&rom[..rom.len().min(0x2000)]));
            profiles.insert(CrtLoadProfile::Roml as u8);
            if rom.len() > 0x2000 {
                existing.romh_a000 = Some(normalize_bank_data(&rom[0x2000..]));
                profiles.insert(CrtLoadProfile::RomhA000 as u8);
            }
        } else if load_address == 0xa000 {
            existing.romh_a000 = Some(normalize_bank_data(rom));
            profiles.insert(CrtLoadProfile::RomhA000 as u8);
        } else if load_address == 0xe000 {
            existing.romh_e000 = Some(normalize_bank_data(rom));
            profiles.insert(CrtLoadProfile::RomhE000 as u8);
        }
        // ts:196 — advance by the declared packet length (a malformed 0 length
        // would loop forever; guard with the minimum CHIP-packet stride).
        offset += packet_len.max(0x10 + size);
    }

    // ts:199-202
    let inferred = mapper_type
        .map(Some)
        .unwrap_or_else(|| infer_mapper_type(hardware_type, exrom, game, &profiles))
        .ok_or(CrtError::UnknownHardware(hardware_type))?;

    Ok(ParsedCartridgeImage {
        path: path.to_string(),
        name,
        mapper_type: inferred,
        exrom,
        game,
        banks,
        profiles,
        raw_bytes: data.to_vec(),
    })
}

/// ts:308-313 — bankMaskForImage: round the highest 8K bank index up to the next
/// power-of-two-minus-one, then AND with the family cap.
fn bank_mask_for_image(image: &ParsedCartridgeImage, cap: u16) -> u16 {
    let highest = image.banks.keys().copied().fold(0u16, u16::max);
    let mut mask: u16 = 1;
    while mask < highest {
        mask = (mask << 1) | 1;
    }
    mask & cap
}

/// ts:315-318 — totalImageBytes: (highestBankIndex + 1) * 0x2000.
fn total_image_bytes(image: &ParsedCartridgeImage) -> u32 {
    let highest = image.banks.keys().copied().fold(0u16, u16::max) as u32;
    (highest + 1) * 0x2000
}

// ── BaseMapper + the read-only mapper families ──────────────────────────────

/// ts:320-421 — BaseMapper. The shared bank store + the pure-array-index read.
/// The Rust port folds the TS overridable visibility predicates
/// (romlVisible/romhA000Visible/romhE000Visible) into a `Visibility` config the
/// concrete mappers set at construction (Rust has no protected-method override).
#[derive(Clone)]
struct Base {
    mapper_type: MapperType,
    exrom: u8,
    game: u8,
    current_bank: u16,
    banks: BTreeMap<u16, CrtBank>,
    // ts:390-400 — the visibility predicates (constant per family).
    roml_visible: bool,
    romh_a000_visible: bool,
    romh_e000_visible: bool,
}

impl Base {
    /// ts:324-328 — constructor: clone the image's bank map.
    fn new(image: &ParsedCartridgeImage) -> Self {
        Base {
            mapper_type: image.mapper_type,
            exrom: image.exrom,
            game: image.game,
            current_bank: 0,
            // ts:273-279 cloneBankData — owned per-mapper copy.
            banks: image.banks.clone(),
            roml_visible: true,        // ts:398-400
            romh_a000_visible: true,   // ts:390-392
            romh_e000_visible: false,  // ts:394-396
        }
    }

    /// ts:356-371 — read: pure array index over the current bank's windows, gated
    /// by the visibility predicates.
    fn read(&self, address: u16) -> Option<u8> {
        let bank = self.banks.get(&self.current_bank)?;
        if (0x8000..=0x9fff).contains(&address) && self.roml_visible {
            if let Some(roml) = &bank.roml {
                return Some(roml[(address - 0x8000) as usize]);
            }
        }
        if (0xa000..=0xbfff).contains(&address) && self.romh_a000_visible {
            if let Some(romh) = &bank.romh_a000 {
                return Some(romh[(address - 0xa000) as usize]);
            }
        }
        if (0xe000..=0xffff).contains(&address) && self.romh_e000_visible {
            if let Some(romh) = &bank.romh_e000 {
                return Some(romh[(address - 0xe000) as usize]);
            }
        }
        None
    }
}

/// ts:423/425/427 — Normal8k / Normal16k / Ultimax: BaseMapper with the only
/// difference being the ultimax visibility predicates. `control_register` is
/// always None (no banking register).
#[derive(Clone)]
pub struct NormalMapper {
    base: Base,
}

impl NormalMapper {
    pub fn new(image: &ParsedCartridgeImage) -> Self {
        let mut base = Base::new(image);
        // ts:427-435 — UltimaxMapper overrides: ROMH@A000 off, ROMH@E000 on.
        if image.mapper_type == MapperType::Ultimax {
            base.romh_a000_visible = false;
            base.romh_e000_visible = true;
        }
        NormalMapper { base }
    }
}

impl CartMapper for NormalMapper {
    fn mapper_type(&self) -> MapperType {
        self.base.mapper_type
    }
    /// ts:349-354 — static GAME/EXROM from the CRT header.
    fn get_lines(&self) -> CartLines {
        CartLines { exrom: self.base.exrom, game: self.base.game }
    }
    fn read(&self, address: u16, _bank_info: &BankInfo) -> Option<u8> {
        self.base.read(address)
    }
    /// ts:383-388 — BaseMapper.write: never consumes.
    fn write(&mut self, _address: u16, _value: u8, _bank_info: &BankInfo) -> bool {
        false
    }
    /// ts:420 — reset to bank 0 (static lines).
    fn reset(&mut self) {
        self.base.current_bank = 0;
    }
    fn get_state(&self) -> CartState {
        CartState { current_bank: self.base.current_bank, control_register: None }
    }
    fn set_state(&mut self, state: CartState) {
        self.base.current_bank = state.current_bank & 0xff;
    }
    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }
}

/// ts:440-459 — MagicDeskMapper. 8K-game banked cart. IO1 ($DE00-$DEFF) store:
/// bit 7 = disable (EXROM released → cart off), bits 0..6 = ROM bank (& bankmask).
/// ROML only; $A000-$BFFF stays BASIC. `regval` is the snapshot register.
#[derive(Clone)]
pub struct MagicDeskMapper {
    base: Base,
    regval: u8,
    bankmask: u16,
}

impl MagicDeskMapper {
    pub fn new(image: &ParsedCartridgeImage) -> Self {
        let mut base = Base::new(image);
        // ts:454 — romhA000Visible() = false.
        base.romh_a000_visible = false;
        MagicDeskMapper {
            regval: 0,
            // ts:442 — bankMaskForImage(image, 0x7f).
            bankmask: bank_mask_for_image(image, 0x7f),
            base,
        }
    }
}

impl CartMapper for MagicDeskMapper {
    fn mapper_type(&self) -> MapperType {
        self.base.mapper_type
    }
    /// ts:451-453 — bit7 set → cart off {1,1}; else {0,1} (8K game).
    fn get_lines(&self) -> CartLines {
        if self.regval & 0x80 != 0 {
            CartLines { exrom: 1, game: 1 }
        } else {
            CartLines { exrom: 0, game: 1 }
        }
    }
    fn read(&self, address: u16, _bank_info: &BankInfo) -> Option<u8> {
        self.base.read(address)
    }
    /// ts:443-450 — IO1 store sets regval + currentBank, consumes.
    fn write(&mut self, address: u16, value: u8, _bank_info: &BankInfo) -> bool {
        if (0xde00..=0xdeff).contains(&address) {
            self.regval = value & (0x80 | self.bankmask as u8);
            self.base.current_bank = (value & self.bankmask as u8) as u16;
            return true;
        }
        false
    }
    /// ts:458 — reset: bank 0, regval 0 (EXROM asserted, 8K).
    fn reset(&mut self) {
        self.base.current_bank = 0;
        self.regval = 0;
    }
    fn get_state(&self) -> CartState {
        CartState { current_bank: self.base.current_bank, control_register: Some(self.regval) }
    }
    fn set_state(&mut self, state: CartState) {
        self.base.current_bank = state.current_bank & 0xff;
        self.regval = state.control_register.unwrap_or(0);
    }
    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }
}

/// ts:463-482 — MagicDesk16Mapper. 16K-game banked cart. IO1 store: bit 7 =
/// disable, bits 0..6 = bank; the bank maps to BOTH ROML ($8000) and ROMH ($A000).
#[derive(Clone)]
pub struct MagicDesk16Mapper {
    base: Base,
    regval: u8,
    bankmask: u16,
}

impl MagicDesk16Mapper {
    pub fn new(image: &ParsedCartridgeImage) -> Self {
        // ts:463 — no romhA000Visible override → BaseMapper default (true), so the
        // bank's romhA000 window (from the CRT's $A000 CHIP / the $8000 CHIP's
        // second $2000) is read at $A000-$BFFF when the bus maps bankA="cart_hi".
        MagicDesk16Mapper {
            regval: 0,
            bankmask: bank_mask_for_image(image, 0x7f), // ts:465
            base: Base::new(image),
        }
    }
}

impl CartMapper for MagicDesk16Mapper {
    fn mapper_type(&self) -> MapperType {
        self.base.mapper_type
    }
    /// ts:474-477 — bit7 set → cart off {1,1}; else 16K game {0,0}.
    fn get_lines(&self) -> CartLines {
        if self.regval & 0x80 != 0 {
            CartLines { exrom: 1, game: 1 }
        } else {
            CartLines { exrom: 0, game: 0 }
        }
    }
    fn read(&self, address: u16, _bank_info: &BankInfo) -> Option<u8> {
        self.base.read(address)
    }
    /// ts:466-473.
    fn write(&mut self, address: u16, value: u8, _bank_info: &BankInfo) -> bool {
        if (0xde00..=0xdeff).contains(&address) {
            self.regval = value & (0x80 | self.bankmask as u8);
            self.base.current_bank = (value & self.bankmask as u8) as u16;
            return true;
        }
        false
    }
    /// ts:481 — reset: bank 0, regval 0 (16K game).
    fn reset(&mut self) {
        self.base.current_bank = 0;
        self.regval = 0;
    }
    fn get_state(&self) -> CartState {
        CartState { current_bank: self.base.current_bank, control_register: Some(self.regval) }
    }
    fn set_state(&mut self, state: CartState) {
        self.base.current_bank = state.current_bank & 0xff;
        self.regval = state.control_register.unwrap_or(0);
    }
    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }
}

/// ts:487-514 — OceanMapper. Banked cart, 8K bank → ROML. 512KB images use
/// 8K-game config; every other size uses 16K-game and MIRRORS the same 8K bank to
/// ROML and ROMH. IO1 store: bank = value & io1_mask & 0x3f. No disable bit.
#[derive(Clone)]
pub struct OceanMapper {
    base: Base,
    regval: u8,
    io1_mask: u16,
    is_8k: bool,
}

impl OceanMapper {
    pub fn new(image: &ParsedCartridgeImage) -> Self {
        OceanMapper {
            regval: 0,
            // ts:489 — bankMaskForImage(image, 0x3f).
            io1_mask: bank_mask_for_image(image, 0x3f),
            // ts:490 — is8k only for exactly 512KB.
            is_8k: total_image_bytes(image) == 0x80000,
            base: Base::new(image),
        }
    }
}

impl CartMapper for OceanMapper {
    fn mapper_type(&self) -> MapperType {
        self.base.mapper_type
    }
    /// ts:499-501 — 8K config → {0,1}; else 16K → {0,0}.
    fn get_lines(&self) -> CartLines {
        if self.is_8k {
            CartLines { exrom: 0, game: 1 }
        } else {
            CartLines { exrom: 0, game: 0 }
        }
    }
    /// ts:502-509 — own read: ROML at $8000-$9FFF; the 16K mirror reads the SAME
    /// 8K ROML bank at $A000-$BFFF (ocean.c romh_read). Note: ignores the Base
    /// visibility predicates (verbatim — the TS OceanMapper overrides read()).
    fn read(&self, address: u16, _bank_info: &BankInfo) -> Option<u8> {
        let bank = self.base.banks.get(&self.base.current_bank)?;
        let roml = bank.roml.as_ref()?;
        if (0x8000..=0x9fff).contains(&address) {
            return Some(roml[(address - 0x8000) as usize]);
        }
        if !self.is_8k && (0xa000..=0xbfff).contains(&address) {
            return Some(roml[(address - 0xa000) as usize]);
        }
        None
    }
    /// ts:491-498.
    fn write(&mut self, address: u16, value: u8, _bank_info: &BankInfo) -> bool {
        if (0xde00..=0xdeff).contains(&address) {
            self.regval = value;
            self.base.current_bank = (value as u16) & self.io1_mask & 0x3f;
            return true;
        }
        false
    }
    /// ts:513 — reset: bank 0 (size-fixed lines).
    fn reset(&mut self) {
        self.base.current_bank = 0;
        self.regval = 0;
    }
    fn get_state(&self) -> CartState {
        CartState { current_bank: self.base.current_bank, control_register: Some(self.regval) }
    }
    fn set_state(&mut self, state: CartState) {
        self.base.current_bank = state.current_bank & 0xff;
        self.regval = state.control_register.unwrap_or(0);
    }
    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }
}

/// ts:120-154 — mapperFromImage: build the concrete read-only mapper for a parsed
/// image. The flash/serial families (Unsupported) yield `Err` — this tier does
/// not implement them.
pub fn mapper_from_image(
    image: &ParsedCartridgeImage,
) -> Result<Box<dyn CartMapper>, CrtError> {
    match image.mapper_type {
        MapperType::Normal8k | MapperType::Normal16k | MapperType::Ultimax => {
            Ok(Box::new(NormalMapper::new(image)))
        }
        MapperType::MagicDesk => Ok(Box::new(MagicDeskMapper::new(image))),
        MapperType::MagicDesk16 => Ok(Box::new(MagicDesk16Mapper::new(image))),
        MapperType::Ocean => Ok(Box::new(OceanMapper::new(image))),
        MapperType::Unsupported => Err(CrtError::Unsupported(MapperType::Unsupported)),
    }
}

/// Convenience: parse `data` and build the mapper in one step (= the TS
/// loadCartridgeMapperFromBytes, ts:114-118). Returns the parsed image (for the
/// attach record / media bytes) alongside the mapper.
pub fn load_cartridge_from_bytes(
    data: &[u8],
    name: &str,
    mapper_type: Option<MapperType>,
) -> Result<(ParsedCartridgeImage, Box<dyn CartMapper>), CrtError> {
    let image = parse_crt(data, name, mapper_type)?;
    let mapper = mapper_from_image(&image)?;
    Ok((image, mapper))
}
