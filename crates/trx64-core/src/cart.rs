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
//! Scope: the READ-ONLY mappers (Normal 8K/16K/Ultimax, Magic Desk, Magic
//! Desk 16, Ocean) PLUS the WRITABLE flash/EEPROM tier (EasyFlash, GMOD2,
//! MegaByter) — flash carts that write back. The writable mappers (ts:829-1137)
//! own a `Flash040` (flash040.rs) and, for GMOD2, an `M93c86` EEPROM (m93c86.rs);
//! a flash command sequence (AA/55/A0/...) programs/erases the flash array, and
//! the written image persists in the mapper (exposed via `writable_image`).
//!
//! In BOTH the TS oracle and VICE the cartridge is a PER-MEMORY-ACCESS bus hook,
//! NOT a clocked device (no `cartridge.tick()`): the mapper is consulted
//! synchronously from the FullBus `read()`/`write()` (full.rs). The flash erase
//! busy-window / DQ6 toggle need a clock, modelled LAZILY: the live maincpu_clk
//! is THREADED INTO `read`/`write`/`peek` (the bus passes `self.clk`), and the
//! flash catches its erase alarm up on the next access at-or-after the due clk.
//! (c64re's TS wired a `()->clk` closure; the Rust port passes the value through
//! the call, which fits the FullBus where `self.clk` is live at every access.)
//!
//! NOTE: `read`/`peek` take `&mut self` (flash reads advance the command FSM,
//! latch last_read, toggle DQ status, and catch the erase alarm up). The
//! read-only mappers ignore the new `clk` arg and stay byte-identical.

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
    /// Writable flash mappers (the WRITABLE tier).
    EasyFlash, // CARTRIDGE_EASYFLASH (hw 0x20)
    Gmod2,     // CARTRIDGE_GMOD2 (hw 0x3c) — flash + M93C86 EEPROM
    MegaByter, // CARTRIDGE_MEGABYTER (hw 0x56) — MX29F800CB flash, ROML only
    C64MegaCart, // CARTRIDGE_C64MEGACART (hw 61, martinpiper fork) — M29F160FT flash
    /// Spec 790 S2 — a raw `.bin` attached with `CartType::Auto` that the S1
    /// structural detect could not settle, now driven by the runtime
    /// self-configuring harness (`SelfConfigCartMapper`). This is the harness's
    /// mapper type UNTIL it locks a concrete family at runtime, at which point
    /// `mapper_type()` returns the concrete type (never `SelfConfig`). It is not a
    /// real cartridge hardware family — it carries no `.bin` geometry and is never
    /// built by `mapper_from_image` (the harness is constructed directly).
    SelfConfig,
    /// A CRT hardware-type that maps to a serial/SPI family not yet built
    /// (GMOD3 SPI-flash). Parsed but produces no mapper.
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
    /// The live phi1 float-bus byte (= memory-bus.ts openBusProvider / VICE
    /// vicii_read_phi1()). GMOD2's IO1 read mixes the EEPROM DO bit (bit 7) with
    /// open-bus low bits; the read-only mappers ignore this. No-cart bus → 0xFF.
    pub phi1: u8,
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
#[derive(Clone, Default)]
pub struct CartState {
    pub current_bank: u16,
    pub control_register: Option<u8>,
    /// Writable-tier continuation (= HeadlessCartridgeState flash fields). Present
    /// only for the flash mappers; None for the read-only tier. The flash DATA is
    /// NOT here (it rides in the separate writable image) — only the command-FSM
    /// continuation + EEPROM serial state + the EasyFlash jumper/IO2-RAM.
    pub flash: Option<FlashCartState>,
}

/// The writable mapper continuation (small; the flash array itself is the
/// separate writable image). Captured for VSF/checkpoint parity.
#[derive(Clone, Default)]
pub struct FlashCartState {
    pub flash_lo: Option<crate::flash040::Flash040SnapState>,
    pub flash_hi: Option<crate::flash040::Flash040SnapState>,
    pub eeprom: Option<crate::m93c86::M93c86SnapState>,
    pub easyflash_jumper: u8,
    pub easyflash_ram: Vec<u8>, // 256 bytes IO2 RAM
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
    /// ts:46 read(address, bankInfo, clk): the ROM-window byte, or None ⇒ not
    /// handled (the bus falls back to RAM / open-bus). `&mut self` + `clk` because
    /// the flash read advances the command FSM / latches DQ status / catches the
    /// erase alarm up; the read-only mappers ignore `clk` and do a pure index.
    fn read(&mut self, address: u16, bank_info: &BankInfo, clk: u64) -> Option<u8>;
    /// ts:55 peek(address, bankInfo): side-effect-free read. For flash this is the
    /// raw array byte (no command/DQ mutation); for the read-only tier == read.
    fn peek(&self, address: u16, bank_info: &BankInfo) -> Option<u8>;
    /// ts:59 write(address, value, bankInfo, clk) → consumed? true = the cart
    /// handled the write (does NOT fall through to RAM); false = pass to RAM
    /// underneath. `clk` drives the flash erase-alarm schedule.
    fn write(&mut self, address: u16, value: u8, bank_info: &BankInfo, clk: u64) -> bool;
    /// ts:106/420 reset(): the expansion-port RESET line — bank + mode/control
    /// return to the boot config so GAME/EXROM re-vector $FFFC from the cart.
    fn reset(&mut self);
    fn get_state(&self) -> CartState;
    fn set_state(&mut self, state: CartState);
    /// Clone into a fresh box (the `Machine` derives `Clone` for snapshots; a
    /// `dyn` trait object is not `Clone` directly). Each read-only mapper's state
    /// is small (bank + register + the cloned bank ROM map), so this is cheap.
    fn clone_box(&self) -> Box<dyn CartMapper>;

    // ── Writable tier (flash/EEPROM) — default no-op for the read-only mappers ──

    /// Whether this mapper's flash/EEPROM has been mutated since attach (= the TS
    /// isWritableDirty). Read-only mappers are never dirty.
    fn is_writable_dirty(&self) -> bool {
        false
    }
    /// A monotonic mutation counter (flash + EEPROM generations) for an
    /// auto-persist debounce (= the TS writableGeneration).
    fn writable_generation(&self) -> u64 {
        0
    }
    /// Spec 714.5 — true when this mapper's full mutable hardware state (flash,
    /// EEPROM, …) is faithfully captured/restored by `writable_image`/
    /// `set_writable_image`, so a DIRTY cartridge IS persistable (no reject from
    /// the media-ingress dirty guard / checkpoint chokepoint). Read-only mappers
    /// and families without a writable port return `false` (default) and stay
    /// reject-on-dirty. (= the TS `persistsWritableState?(): boolean`,
    /// cartridge.ts:67; the writable families EasyFlash/Megabyter/Gmod2/Gmod3/
    /// C64MegaCart all return true.)
    fn persists_writable_state(&self) -> bool {
        false
    }
    /// The live writable image (flash array + any EEPROM), for snapshot /
    /// write-back. None ⇒ this mapper has no writable backing (read-only tier).
    /// `clk` so the flash catches its erase alarm up before serializing.
    fn writable_image(&mut self, _clk: u64) -> Option<Vec<u8>> {
        None
    }
    /// Load a previously-saved writable image (flash + EEPROM) back into the
    /// mapper. No-op for the read-only tier.
    fn set_writable_image(&mut self, _bytes: &[u8]) {}
    /// Re-pack the live flash back into the original `.crt` structure (preserving
    /// header / CHIP packets / load addresses; only data changes) for write-back
    /// to a `.crt` file. None ⇒ unsupported. `clk` to catch the erase alarm up.
    fn crt_image(&mut self, _clk: u64) -> Option<Vec<u8>> {
        None
    }
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
        // ts:250-259 — flash/serial families. The WRITABLE tier now builds these:
        32 => Some(MapperType::EasyFlash),  // CARTRIDGE_EASYFLASH
        60 => Some(MapperType::Gmod2),      // CARTRIDGE_GMOD2 (flash + M93C86)
        86 => Some(MapperType::MegaByter),  // CARTRIDGE_MEGABYTER (MX29F800CB)
        // C64MegaCart (martinpiper VICE fork): M29F160FT 2MB flash, GMOD2-derived.
        61 => Some(MapperType::C64MegaCart), // CARTRIDGE_C64MEGACART
        // serial/SPI families not yet built (GMOD3 SPI-flash).
        62 => Some(MapperType::Unsupported), // gmod3
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
    /// Spec 790 — a raw `.bin` whose byte length is not a whole number of the
    /// type's bank units, or exceeds the type's bank capacity. Never silently
    /// truncated (the whole point of the typed attach is faithful geometry).
    BadBinSize {
        len: usize,
        bank_unit: usize,
        max_banks: usize,
    },
    /// Spec 790 — `resolve_cart_type` was handed a string that is neither a known
    /// VICE numeric id nor a known mnemonic. Carries the offending string; the
    /// Display lists the valid set.
    UnknownCartType(String),
    /// Spec 790 S1 — a raw `.bin` was attached with `CartType::Auto`, and the
    /// structural-only first-cut detect (eapi / CBM80 / reset-vector + size) could
    /// not settle on a single confident type. The caller must pass an explicit
    /// `--cart-type`. Resolving the genuinely ambiguous cases (watch $DE00/$DE02/
    /// $DF00 + the AMD flash command sequence, lock the type in-place) is the
    /// runtime self-configuring cart harness = Spec 790 S2, NOT this slice.
    BinTypeAmbiguous,
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
            CrtError::BadBinSize { len, bank_unit, max_banks } => {
                write!(
                    f,
                    "Bad raw .bin size {len} bytes: must be a whole number of {bank_unit}-byte bank units, 1..={max_banks} banks (no silent truncation)."
                )
            }
            CrtError::UnknownCartType(s) => {
                write!(
                    f,
                    "Unknown cart type '{s}'. Valid: a VICE numeric id (5, 19, 32, 60, 85, 86, 61, -2, -3, -6, 0) \
                     or a mnemonic (ef/easyflash, gmod2, megabyter/mb, c64megacart/c64mc, magicdesk/md, md16, ocean, 8k, 16k, ultimax, crt/auto)."
                )
            }
            CrtError::BinTypeAmbiguous => {
                write!(
                    f,
                    "Raw .bin cartridge type could not be structurally auto-detected (Spec 790 S1). \
                     Pass an explicit --cart-type <id|mnemonic> (e.g. megabyter, c64megacart, ef, magicdesk). \
                     Automatic resolution of ambiguous flash carts is the runtime self-configuring harness (Spec 790 S2)."
                )
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

/// ts:285-302 — buildLinearChipData: lay the per-bank ROM windows out into one
/// linear flash array (bank b at `b * 0x2000`), 0xFF-padding absent banks. The
/// `selector` picks ROML (lo flash) or ROMH (hi flash) from each bank; `bank_count`
/// is the device's bank capacity (so a partially-populated flash is full size).
fn build_linear_chip_data(
    image: &ParsedCartridgeImage,
    selector: impl Fn(&CrtBank) -> Option<&[u8; 0x2000]>,
    bank_count: usize,
) -> Vec<u8> {
    let highest = image.banks.keys().copied().fold(0u16, u16::max) as usize;
    let total_banks = bank_count.max(highest + 1);
    let mut result = vec![0xffu8; total_banks * 0x2000];
    for (&bank_number, bank) in image.banks.iter() {
        if let Some(segment) = selector(bank) {
            let off = (bank_number as usize) * 0x2000;
            if off + 0x2000 <= result.len() {
                result[off..off + 0x2000].copy_from_slice(segment);
            }
        }
    }
    result
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
    fn read(&mut self, address: u16, _bank_info: &BankInfo, _clk: u64) -> Option<u8> {
        self.base.read(address)
    }
    fn peek(&self, address: u16, _bank_info: &BankInfo) -> Option<u8> {
        self.base.read(address)
    }
    /// ts:383-388 — BaseMapper.write: never consumes.
    fn write(&mut self, _address: u16, _value: u8, _bank_info: &BankInfo, _clk: u64) -> bool {
        false
    }
    /// ts:420 — reset to bank 0 (static lines).
    fn reset(&mut self) {
        self.base.current_bank = 0;
    }
    fn get_state(&self) -> CartState {
        CartState { current_bank: self.base.current_bank, control_register: None, flash: None }
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
    fn read(&mut self, address: u16, _bank_info: &BankInfo, _clk: u64) -> Option<u8> {
        self.base.read(address)
    }
    fn peek(&self, address: u16, _bank_info: &BankInfo) -> Option<u8> {
        self.base.read(address)
    }
    /// ts:443-450 — IO1 store sets regval + currentBank, consumes.
    fn write(&mut self, address: u16, value: u8, _bank_info: &BankInfo, _clk: u64) -> bool {
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
        CartState { current_bank: self.base.current_bank, control_register: Some(self.regval), flash: None }
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
    fn read(&mut self, address: u16, _bank_info: &BankInfo, _clk: u64) -> Option<u8> {
        self.base.read(address)
    }
    fn peek(&self, address: u16, _bank_info: &BankInfo) -> Option<u8> {
        self.base.read(address)
    }
    /// ts:466-473.
    fn write(&mut self, address: u16, value: u8, _bank_info: &BankInfo, _clk: u64) -> bool {
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
        CartState { current_bank: self.base.current_bank, control_register: Some(self.regval), flash: None }
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

    /// ts:502-509 — own read: ROML at $8000-$9FFF; the 16K mirror reads the SAME
    /// 8K ROML bank at $A000-$BFFF (ocean.c romh_read). Ignores the Base
    /// visibility predicates (verbatim — the TS OceanMapper overrides read()).
    fn ocean_read(&self, address: u16) -> Option<u8> {
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
    fn read(&mut self, address: u16, _bank_info: &BankInfo, _clk: u64) -> Option<u8> {
        self.ocean_read(address)
    }
    fn peek(&self, address: u16, _bank_info: &BankInfo) -> Option<u8> {
        self.ocean_read(address)
    }
    /// ts:491-498.
    fn write(&mut self, address: u16, value: u8, _bank_info: &BankInfo, _clk: u64) -> bool {
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
        CartState { current_bank: self.base.current_bank, control_register: Some(self.regval), flash: None }
    }
    fn set_state(&mut self, state: CartState) {
        self.base.current_bank = state.current_bank & 0xff;
        self.regval = state.control_register.unwrap_or(0);
    }
    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }
}

// ── WRITABLE flash tier ──────────────────────────────────────────────────────

use crate::flash040::{Flash040, FLASH040B, FLASH040_160, FLASH040_NORMAL, FLASH800_CB};
use crate::m93c86::M93c86;

/// resolveRelativeOffset (ts:281-283): the $2000-window offset of `address`.
#[inline]
fn resolve_relative_offset(base: u16, address: u16) -> u32 {
    ((address.wrapping_sub(base)) & 0x1fff) as u32
}

/// ts:822-825 / easyflash.c — easyflash_memconfig[(jumper<<3)|(register_02 & 7)]
/// → CMODE (0=8k, 1=16k, 2=RAM/off, 3=ultimax).
const EASYFLASH_MEMCONFIG: [u8; 16] = [
    3, 3, 1, 1, 2, 3, 0, 1, // jumper off
    2, 3, 0, 1, 2, 3, 0, 1, // jumper on
];

/// ts:826-827 — the 4 EasyFlash modes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EasyFlashMode {
    M8k,
    M16k,
    Off,
    Ultimax,
}

/// VICE eapiam29f040[768] — the EAPI replacement block. On attach, if the cart's
/// romh bank-0 $1800 (= $B800) holds the "eapi" signature, VICE replaces the
/// cart's EAPI with this known-good implementation (cart EAPIs vary / assume
/// real-HW timing; the replacement drives this flash040 port correctly).
/// Verbatim from c64re eapi-am29f040.ts (= VICE src/c64/cart/easyflash.c).
#[rustfmt::skip]
const EAPI_AM29F040: [u8; 768] = [
    0x65,0x61,0x70,0x69,0xc1,0x4d,0x2f,0xcd,0x32,0x39,0xc6,0x30,0x34,0x30,0x20,0xd6,0x31,0x2e,0x34,0x00,0x08,0x78,0xa5,0x4b,0x48,0xa5,0x4c,0x48,0xa9,0x60,0x85,0x4b,0x20,0x4b,0x00,0xba,0xbd,0x00,0x01,0x85,0x4c,0xca,0xbd,0x00,0x01,0x85,0x4b,0x18,0x90,0x70,0x4c,0x67,0x01,0x4c,0xa4,0x01,0x4c,0x39,0x02,0x4c,0x40,0x02,0x4c,0x44,0x02,0x4c,0x4e,0x02,0x4c,0x58,0x02,0x4c,0x8e,0x02,0x4c,0xd9,0x02,0x4c,0xd9,0x02,0x8d,0x02,0xde,0xa9,0xaa,0x8d,0x55,0x85,0xa9,0x55,0x8d,0xaa,0x82,0xa9,0xa0,0x8d,0x55,0x85,0xad,0xf2,0xdf,0x8d,0x00,0xde,0xa9,0x00,0x8d,0xff,0xff,0xa2,0x07,0x8e,0x02,0xde,0x60,0x8d,0x02,0xde,0xa9,0xaa,0x8d,0x55,0xe5,0xa9,0x55,0x8d,0xaa,0xe2,0xa9,0xa0,0x8d,0x55,0xe5,0xd0,0xdb,0xa2,0x55,0x8e,0xe3,0xdf,0x8c,0xe4,0xdf,0xa2,0x85,0x8e,0x02,0xde,0x8d,0xff,0xff,0x4c,0xbb,0xdf,0xad,0xff,0xff,0x60,0xcd,0xff,0xff,0x60,0xa2,0x6f,0xa0,0x7f,0xb1,0x4b,0x9d,0x80,0xdf,0xdd,0x80,0xdf,0xd0,0x21,0x88,0xca,0x10,0xf2,0xa2,0x00,0xe8,0x18,0xbd,0x80,0xdf,0x65,0x4b,0x9d,0x80,0xdf,0xe8,0xbd,0x80,0xdf,0x65,0x4c,0x9d,0x80,0xdf,0xe8,0xe0,0x1e,0xd0,0xe8,0x18,0x90,0x06,0xa9,0x01,0x8d,0xb9,0xdf,0x38,0x68,0x85,0x4c,0x68,0x85,0x4b,0xb0,0x48,0xa9,0xaa,0xa0,0xe5,0x20,0xd5,0xdf,0xa0,0x85,0x20,0xd5,0xdf,0xa9,0x55,0xa2,0xaa,0xa0,0xe2,0x20,0xd7,0xdf,0xa2,0xaa,0xa0,0x82,0x20,0xd7,0xdf,0xa9,0x90,0xa0,0xe5,0x20,0xd5,0xdf,0xa0,0x85,0x20,0xd5,0xdf,0xad,0x00,0xa0,0x8d,0xf1,0xdf,0xae,0x01,0xa0,0x8e,0xb9,0xdf,0xc9,0x01,0xd0,0x06,0xe0,0xa4,0xd0,0x02,0xf0,0x0c,0xc9,0x20,0xd0,0x39,0xe0,0xe2,0xd0,0x35,0xf0,0x02,0xb0,0x50,0xad,0x00,0x80,0xae,0x01,0x80,0xc9,0x01,0xd0,0x06,0xe0,0xa4,0xd0,0x02,0xf0,0x08,0xc9,0x20,0xd0,0x19,0xe0,0xe2,0xd0,0x15,0xa0,0x3f,0x8c,0x00,0xde,0xae,0x02,0x80,0xd0,0x13,0xae,0x02,0xa0,0xd0,0x12,0x88,0x10,0xf0,0x18,0x90,0x12,0xa9,0x02,0xd0,0x0a,0xa9,0x03,0xd0,0x06,0xa9,0x04,0xd0,0x02,0xa9,0x05,0x8d,0xb9,0xdf,0x38,0xa9,0x00,0x8d,0x00,0xde,0xa0,0xe0,0xa9,0xf0,0x20,0xd7,0xdf,0xa0,0x80,0x20,0xd7,0xdf,0xad,0xb9,0xdf,0xb0,0x08,0xae,0xf1,0xdf,0xa0,0x40,0x28,0x18,0x60,0x28,0x38,0x60,0x8d,0xb7,0xdf,0x8e,0xb9,0xdf,0x8e,0xed,0xdf,0x8c,0xba,0xdf,0x08,0x78,0x98,0x29,0xbf,0x8d,0xee,0xdf,0xa9,0x00,0x8d,0x00,0xde,0xa9,0x85,0xc0,0xe0,0x90,0x05,0x20,0xc1,0xdf,0xb0,0x03,0x20,0x9e,0xdf,0xa2,0x14,0x20,0xec,0xdf,0xf0,0x06,0xca,0xd0,0xf8,0x18,0x90,0x63,0xad,0xf2,0xdf,0x8d,0x00,0xde,0x18,0x90,0x72,0x8d,0xb7,0xdf,0x8e,0xb9,0xdf,0x8c,0xba,0xdf,0x08,0x78,0x98,0xc0,0x80,0xf0,0x04,0xa0,0xe0,0xa9,0xa0,0x8d,0xee,0xdf,0xc8,0xc8,0xc8,0xc8,0xc8,0xa9,0xaa,0x20,0xd5,0xdf,0xa9,0x55,0xa2,0xaa,0x88,0x88,0x88,0x20,0xd7,0xdf,0xa9,0x80,0xc8,0xc8,0xc8,0x20,0xd5,0xdf,0xa9,0xaa,0x20,0xd5,0xdf,0xa9,0x55,0xa2,0xaa,0x88,0x88,0x88,0x20,0xd7,0xdf,0xad,0xb7,0xdf,0x8d,0x00,0xde,0xa2,0x00,0x8e,0xed,0xdf,0x88,0x88,0xa9,0x30,0x20,0xd7,0xdf,0xa9,0xff,0xaa,0xa8,0xd0,0x24,0xad,0xf2,0xdf,0x8d,0x00,0xde,0xa0,0x80,0xa9,0xf0,0x20,0xd7,0xdf,0xa0,0xe0,0xa9,0xf0,0x20,0xd7,0xdf,0x28,0x38,0xb0,0x02,0x28,0x18,0xac,0xba,0xdf,0xae,0xb9,0xdf,0xad,0xb7,0xdf,0x60,0x20,0xec,0xdf,0xf0,0x09,0xca,0xd0,0xf8,0x88,0xd0,0xf5,0x18,0x90,0xce,0xad,0xf2,0xdf,0x8d,0x00,0xde,0x18,0x90,0xdd,0x8d,0xf2,0xdf,0x8d,0x00,0xde,0x60,0xad,0xf2,0xdf,0x60,0x8d,0xf3,0xdf,0x8e,0xe9,0xdf,0x8c,0xea,0xdf,0x60,0x8e,0xf4,0xdf,0x8c,0xf5,0xdf,0x8d,0xf6,0xdf,0x60,0xad,0xf2,0xdf,0x8d,0x00,0xde,0x20,0xe8,0xdf,0x8d,0xb7,0xdf,0x8e,0xf0,0xdf,0x8c,0xf1,0xdf,0xa9,0x00,0x8d,0xba,0xdf,0xf0,0x3b,0xad,0xf4,0xdf,0xd0,0x10,0xad,0xf5,0xdf,0xd0,0x08,0xad,0xf6,0xdf,0xf0,0x0b,0xce,0xf6,0xdf,0xce,0xf5,0xdf,0xce,0xf4,0xdf,0x90,0x45,0x38,0xb0,0x42,0x8d,0xb7,0xdf,0x8e,0xf0,0xdf,0x8c,0xf1,0xdf,0xae,0xe9,0xdf,0xad,0xea,0xdf,0xc9,0xa0,0x90,0x02,0x09,0x40,0xa8,0xad,0xb7,0xdf,0x20,0x80,0xdf,0xb0,0x24,0xee,0xe9,0xdf,0xd0,0x19,0xee,0xea,0xdf,0xad,0xf3,0xdf,0x29,0xe0,0xcd,0xea,0xdf,0xd0,0x0c,0xad,0xf3,0xdf,0x0a,0x0a,0x0a,0x8d,0xea,0xdf,0xee,0xf2,0xdf,0x18,0xad,0xba,0xdf,0xf0,0xa1,0xac,0xf1,0xdf,0xae,0xf0,0xdf,0xad,0xb7,0xdf,0x60,0xff,0xff,0xff,0xff,
];

/// ts:829-1052 — EasyFlashMapper. 1MB cart = 2 × AM29F040B flash chips (lo = the
/// 64 ROML banks, hi = the 64 ROMH banks). IO1 ($DE00) decodes `addr & 2`:
/// even = bank register (& 0x3f), odd = mode register (& 0x87). IO2 ($DF00) is a
/// 256-byte RAM. The memconfig table selects 8k/16k/off/ultimax from the mode
/// register + jumper. Flash is PROGRAMMED only in ultimax ($8000→lo, $E000→hi).
#[derive(Clone)]
pub struct EasyFlashMapper {
    image: ParsedCartridgeImage,
    current_bank: u16,
    register02: u8, // VICE easyflash_register_02 (& 0x87)
    jumper: u8,
    io_ram: [u8; 256], // VICE easyflash_ram ($DF00 IO2)
    lo_flash: Flash040,
    hi_flash: Flash040,
}

impl EasyFlashMapper {
    pub fn new(image: &ParsedCartridgeImage) -> Self {
        let lo_data = build_linear_chip_data(image, |b| b.roml.as_ref(), 64);
        let mut hi_data = build_linear_chip_data(
            image,
            |b| b.romh_a000.as_ref().or(b.romh_e000.as_ref()),
            64,
        );
        // ts:840-849 — if the EAPI signature "eapi" is in hi bank-0 $1800, replace
        // the cart's EAPI with VICE's known-good eapiam29f040 block.
        if hi_data.len() >= 0x1800 + 768
            && hi_data[0x1800] == 0x65
            && hi_data[0x1801] == 0x61
            && hi_data[0x1802] == 0x70
            && hi_data[0x1803] == 0x69
        {
            hi_data[0x1800..0x1800 + 768].copy_from_slice(&EAPI_AM29F040);
        }
        let mut io_ram = [0u8; 256];
        // ts:852-855 — easyflash_powerup IO2 RAM pattern: FF 00 00 FF FF 00 00 FF...
        for (i, slot) in io_ram.iter_mut().enumerate() {
            *slot = if (((i + 1) >> 1) & 1) != 0 { 0x00 } else { 0xff };
        }
        EasyFlashMapper {
            image: image.clone(),
            current_bank: 0,
            register02: 0,
            jumper: 0,
            io_ram,
            lo_flash: Flash040::new(lo_data, "easyflash-lo", FLASH040B),
            hi_flash: Flash040::new(hi_data, "easyflash-hi", FLASH040B),
        }
    }

    /// ts:865-870 — the flash offset for a ROM-window address in the current bank.
    fn chip_offset(&self, address: u16) -> u32 {
        let relative = if address >= 0xe000 {
            resolve_relative_offset(0xe000, address)
        } else if address >= 0xa000 {
            resolve_relative_offset(0xa000, address)
        } else {
            resolve_relative_offset(0x8000, address)
        };
        ((self.current_bank as u32) << 13) | relative
    }

    /// ts:872-875 — the live memconfig mode.
    fn current_mode(&self) -> EasyFlashMode {
        let cmode = EASYFLASH_MEMCONFIG
            [(((self.jumper << 3) | (self.register02 & 0x07)) & 0x0f) as usize];
        match cmode {
            0 => EasyFlashMode::M8k,
            1 => EasyFlashMode::M16k,
            3 => EasyFlashMode::Ultimax,
            _ => EasyFlashMode::Off,
        }
    }
}

impl CartMapper for EasyFlashMapper {
    fn mapper_type(&self) -> MapperType {
        MapperType::EasyFlash
    }
    /// ts:877-884 — lines per the current memconfig mode.
    fn get_lines(&self) -> CartLines {
        match self.current_mode() {
            EasyFlashMode::Off => CartLines { exrom: 1, game: 1 },
            EasyFlashMode::Ultimax => CartLines { exrom: 1, game: 0 },
            EasyFlashMode::M8k => CartLines { exrom: 0, game: 1 },
            EasyFlashMode::M16k => CartLines { exrom: 0, game: 0 },
        }
    }
    /// ts:971-978 — read: IO2 RAM + the flash windows (the bus PLA-gates which
    /// window it calls; the mapper responds purely by address).
    fn read(&mut self, address: u16, _bank_info: &BankInfo, clk: u64) -> Option<u8> {
        if (0xdf00..=0xdfff).contains(&address) {
            return Some(self.io_ram[(address & 0xff) as usize]); // IO2 RAM
        }
        let offset = self.chip_offset(address);
        if (0x8000..=0x9fff).contains(&address) {
            return Some(self.lo_flash.read(offset, clk)); // ROML
        }
        if (0xa000..=0xbfff).contains(&address) {
            return Some(self.hi_flash.read(offset, clk)); // ROMH @ $A000 (16k)
        }
        if (0xe000..=0xffff).contains(&address) {
            return Some(self.hi_flash.read(offset, clk)); // ROMH @ $E000 (ultimax)
        }
        None
    }
    /// ts:983-996 — side-effect-free peek (flash array byte, IO RAM, register
    /// shadows). IO1 reads the write-only register shadow (monitor lane).
    fn peek(&self, address: u16, _bank_info: &BankInfo) -> Option<u8> {
        if (0xde00..=0xdeff).contains(&address) {
            return Some(if address & 2 != 0 { self.register02 } else { self.current_bank as u8 });
        }
        if (0xdf00..=0xdfff).contains(&address) {
            return Some(self.io_ram[(address & 0xff) as usize]);
        }
        let offset = self.chip_offset(address);
        if (0x8000..=0x9fff).contains(&address) {
            return Some(self.lo_flash.peek(offset));
        }
        if (0xa000..=0xbfff).contains(&address) {
            return Some(self.hi_flash.peek(offset));
        }
        if (0xe000..=0xffff).contains(&address) {
            return Some(self.hi_flash.peek(offset));
        }
        None
    }
    /// ts:1008-1033 — write: IO1 bank/mode registers, IO2 RAM, and flash
    /// programming (ONLY in ultimax: $8000→lo, $E000→hi).
    fn write(&mut self, address: u16, value: u8, _bank_info: &BankInfo, clk: u64) -> bool {
        if (0xde00..=0xdeff).contains(&address) {
            if address & 2 != 0 {
                self.register02 = value & 0x87;
            } else {
                self.current_bank = (value & 0x3f) as u16;
            }
            return true;
        }
        if (0xdf00..=0xdfff).contains(&address) {
            self.io_ram[(address & 0xff) as usize] = value;
            return true;
        }
        if self.current_mode() == EasyFlashMode::Ultimax {
            let offset = self.chip_offset(address);
            if (0x8000..=0x9fff).contains(&address) {
                self.lo_flash.store(offset, value, clk);
                return true;
            }
            if (0xe000..=0xffff).contains(&address) {
                self.hi_flash.store(offset, value, clk);
                return true;
            }
        }
        false
    }
    /// ts:1051 — reset: register_02 = 0 (memconfig[jumper<<3] = ULTIMAX so $FFFC
    /// re-vectors INTO the cart). Bank + jumper + IO2 RAM + flash DATA preserved.
    fn reset(&mut self) {
        self.current_bank = 0;
        self.register02 = 0x00;
    }
    fn get_state(&self) -> CartState {
        let mut lo = self.lo_flash.clone();
        let mut hi = self.hi_flash.clone();
        CartState {
            current_bank: self.current_bank,
            control_register: Some(self.register02),
            flash: Some(FlashCartState {
                flash_lo: Some(lo.snapshot_state(0)),
                flash_hi: Some(hi.snapshot_state(0)),
                eeprom: None,
                easyflash_jumper: self.jumper,
                easyflash_ram: self.io_ram.to_vec(),
            }),
        }
    }
    fn set_state(&mut self, state: CartState) {
        self.current_bank = state.current_bank & 0x3f;
        self.register02 = state.control_register.unwrap_or(0) & 0x87;
        if let Some(f) = &state.flash {
            self.jumper = f.easyflash_jumper & 1;
            if f.easyflash_ram.len() >= 256 {
                self.io_ram.copy_from_slice(&f.easyflash_ram[..256]);
            }
            if let Some(s) = &f.flash_lo {
                self.lo_flash.restore_state(s);
            }
            if let Some(s) = &f.flash_hi {
                self.hi_flash.restore_state(s);
            }
        }
    }
    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }
    fn is_writable_dirty(&self) -> bool {
        self.lo_flash.is_dirty() || self.hi_flash.is_dirty()
    }
    // ts:911 — EasyFlash persists its full flash state (writable_image/crt_image),
    // so a dirty EasyFlash is captured, not rejected by the dirty-media guard.
    fn persists_writable_state(&self) -> bool {
        true
    }
    fn writable_generation(&self) -> u64 {
        self.lo_flash.writable_generation() + self.hi_flash.writable_generation()
    }
    /// ts:913-920 — writable image = lo flash array ++ hi flash array.
    fn writable_image(&mut self, clk: u64) -> Option<Vec<u8>> {
        let lo = self.lo_flash.get_data(clk).to_vec();
        let hi = self.hi_flash.get_data(clk).to_vec();
        let mut out = Vec::with_capacity(lo.len() + hi.len());
        out.extend_from_slice(&lo);
        out.extend_from_slice(&hi);
        Some(out)
    }
    fn set_writable_image(&mut self, bytes: &[u8]) {
        let lo_len = self.lo_flash.data.len();
        self.lo_flash.load_data(&bytes[..bytes.len().min(lo_len)]);
        if bytes.len() > lo_len {
            self.hi_flash.load_data(&bytes[lo_len..]);
        }
    }
    /// ts:933-964 — re-pack the live flash into the original .crt structure
    /// (header / CHIP packets / load addresses preserved, only data changes).
    fn crt_image(&mut self, clk: u64) -> Option<Vec<u8>> {
        let orig = &self.image.raw_bytes;
        if orig.len() < 0x40 {
            return None;
        }
        let mut out = orig.clone();
        let lo = self.lo_flash.get_data(clk).to_vec();
        let hi = self.hi_flash.get_data(clk).to_vec();
        let header_len = read_u32_be(&out, 0x10) as usize;
        let mut offset = header_len;
        while offset + 0x10 <= out.len() {
            if out.get(offset..offset + 4) != Some(b"CHIP") {
                break;
            }
            let packet_len = read_u32_be(&out, offset + 4) as usize;
            let bank = read_u16_be(&out, offset + 10) as usize;
            let load_address = read_u16_be(&out, offset + 12);
            let size = read_u16_be(&out, offset + 14) as usize;
            let data_off = offset + 16;
            let bank_off = bank << 13;
            let first = size.min(0x2000);
            let src = if load_address == 0x8000 { &lo } else { &hi };
            if bank_off + first <= src.len() && data_off + first <= out.len() {
                out[data_off..data_off + first]
                    .copy_from_slice(&src[bank_off..bank_off + first]);
            }
            // 16K chip ($8000 carrying ROML+ROMH): second 8K from hiFlash.
            if load_address == 0x8000 && size > 0x2000 {
                let second = size - 0x2000;
                if bank_off + second <= hi.len() && data_off + 0x2000 + second <= out.len() {
                    out[data_off + 0x2000..data_off + 0x2000 + second]
                        .copy_from_slice(&hi[bank_off..bank_off + second]);
                }
            }
            offset += packet_len;
        }
        Some(out)
    }
}

/// ts:1148-1291 — Gmod2Mapper. 512KB AM29F040 flash (64×8K banks) + M93C86 serial
/// EEPROM. IO1 ($DE00) store: bits 0-5 = ROM bank; bits 7-6 select cmode
/// (0xc0=ULTIMAX, b6=0 → 8K, b6=1(b7=0) → off); bit 6 = EEPROM CS, bit 4 = DI,
/// bit 5 = CLK. IO1 read: CS ? (eeprom.read_data()<<7)|phi1(0x7f) : phi1. Flash
/// is READ at $8000 in 8K mode; PROGRAMMED in ULTIMAX ($8000/$E000).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gmod2Cmode {
    M8k,
    Off,
    Ultimax,
}

#[derive(Clone)]
pub struct Gmod2Mapper {
    current_bank: u16,
    register: u8,
    cmode: Gmod2Cmode,
    eeprom_cs: u8,
    eeprom_data: u8,
    eeprom_clock: u8,
    flash: Flash040,
    eeprom: M93c86,
}

impl Gmod2Mapper {
    pub fn new(image: &ParsedCartridgeImage) -> Self {
        Gmod2Mapper {
            current_bank: 0,
            register: 0,
            cmode: Gmod2Cmode::M8k,
            eeprom_cs: 0,
            eeprom_data: 0,
            eeprom_clock: 0,
            flash: Flash040::new(
                build_linear_chip_data(image, |b| b.roml.as_ref(), 64),
                "gmod2",
                FLASH040_NORMAL,
            ),
            eeprom: M93c86::new(),
        }
    }

    /// ts:1167-1169 — (addr & 0x1fff) + (bank << 13).
    fn flash_offset(&self, address: u16) -> u32 {
        ((address & 0x1fff) as u32) + ((self.current_bank as u32) << 13)
    }
}

impl CartMapper for Gmod2Mapper {
    fn mapper_type(&self) -> MapperType {
        MapperType::Gmod2
    }
    /// ts:1171-1177 — lines per cmode.
    fn get_lines(&self) -> CartLines {
        match self.cmode {
            Gmod2Cmode::M8k => CartLines { exrom: 0, game: 1 },
            Gmod2Cmode::Ultimax => CartLines { exrom: 1, game: 0 },
            Gmod2Cmode::Off => CartLines { exrom: 1, game: 1 },
        }
    }
    /// ts:1188-1201 — read: IO1 EEPROM (CS ? DO<<7|phi1 : phi1); ROML flash in 8K.
    fn read(&mut self, address: u16, bank_info: &BankInfo, clk: u64) -> Option<u8> {
        if (0xde00..=0xdeff).contains(&address) {
            let phi1 = bank_info.phi1;
            return Some(if self.eeprom_cs != 0 {
                ((self.eeprom.read_data() & 1) << 7) | (phi1 & 0x7f)
            } else {
                phi1
            });
        }
        if self.cmode == Gmod2Cmode::M8k && (0x8000..=0x9fff).contains(&address) {
            return Some(self.flash.read(self.flash_offset(address), clk));
        }
        None
    }
    /// ts:1209-1215 — peek: IO1 → open-bus (don't clock the EEPROM); flash array.
    fn peek(&self, address: u16, _bank_info: &BankInfo) -> Option<u8> {
        if (0xde00..=0xdeff).contains(&address) {
            return None; // → bus open-bus (EEPROM read_data advances the FSM)
        }
        if self.cmode == Gmod2Cmode::M8k && (0x8000..=0x9fff).contains(&address) {
            return Some(self.flash.peek(self.flash_offset(address)));
        }
        None
    }
    /// ts:1217-1242 — write: IO1 bank/cmode + EEPROM lines; flash program in ultimax.
    fn write(&mut self, address: u16, value: u8, _bank_info: &BankInfo, clk: u64) -> bool {
        if (0xde00..=0xdeff).contains(&address) {
            self.register = value;
            self.current_bank = (value & 0x3f) as u16;
            if (value & 0xc0) == 0xc0 {
                self.cmode = Gmod2Cmode::Ultimax;
            } else if (value & 0x40) == 0x00 {
                self.cmode = Gmod2Cmode::M8k;
            } else {
                self.cmode = Gmod2Cmode::Off;
            }
            self.eeprom_cs = (value >> 6) & 1;
            self.eeprom_data = (value >> 4) & 1;
            self.eeprom_clock = (value >> 5) & 1;
            self.eeprom.write_select(self.eeprom_cs);
            if self.eeprom_cs != 0 {
                self.eeprom.write_data(self.eeprom_data);
                self.eeprom.write_clock(self.eeprom_clock);
            }
            return true;
        }
        if self.cmode == Gmod2Cmode::Ultimax
            && ((0x8000..=0x9fff).contains(&address) || (0xe000..=0xffff).contains(&address))
        {
            self.flash.store(self.flash_offset(address), value, clk);
            return true;
        }
        false
    }
    /// ts:1284-1290 — reset: 8K mode, EEPROM CS 0. Flash DATA preserved.
    fn reset(&mut self) {
        self.current_bank = 0;
        self.register = 0;
        self.cmode = Gmod2Cmode::M8k;
        self.eeprom_cs = 0;
        self.eeprom.write_select(0);
    }
    fn get_state(&self) -> CartState {
        let mut flash = self.flash.clone();
        CartState {
            current_bank: self.current_bank,
            control_register: Some(self.register),
            flash: Some(FlashCartState {
                flash_lo: Some(flash.snapshot_state(0)),
                flash_hi: None,
                eeprom: Some(self.eeprom.snapshot_state()),
                easyflash_jumper: 0,
                easyflash_ram: Vec::new(),
            }),
        }
    }
    fn set_state(&mut self, state: CartState) {
        self.register = state.control_register.unwrap_or(0);
        self.current_bank = state.current_bank & 0x3f;
        if (self.register & 0xc0) == 0xc0 {
            self.cmode = Gmod2Cmode::Ultimax;
        } else if (self.register & 0x40) == 0x00 {
            self.cmode = Gmod2Cmode::M8k;
        } else {
            self.cmode = Gmod2Cmode::Off;
        }
        self.eeprom_cs = (self.register >> 6) & 1;
        if let Some(f) = &state.flash {
            if let Some(s) = &f.flash_lo {
                self.flash.restore_state(s);
            }
            if let Some(s) = &f.eeprom {
                self.eeprom.restore_state(s);
            }
        }
    }
    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }
    fn is_writable_dirty(&self) -> bool {
        self.flash.is_dirty() || self.eeprom.is_dirty()
    }
    // ts:1266 — GMOD2 persists flash + M93C86 EEPROM (writable_image/crt_image).
    fn persists_writable_state(&self) -> bool {
        true
    }
    fn writable_generation(&self) -> u64 {
        self.flash.writable_generation() + self.eeprom.writable_generation()
    }
    /// ts:1270-1275 — writable image = flash array ++ EEPROM 2KB.
    fn writable_image(&mut self, clk: u64) -> Option<Vec<u8>> {
        let flash = self.flash.get_data(clk).to_vec();
        let eeprom = self.eeprom.get_data().to_vec();
        let mut out = Vec::with_capacity(flash.len() + eeprom.len());
        out.extend_from_slice(&flash);
        out.extend_from_slice(&eeprom);
        Some(out)
    }
    fn set_writable_image(&mut self, bytes: &[u8]) {
        let flash_len = self.flash.data.len();
        self.flash.load_data(&bytes[..bytes.len().min(flash_len)]);
        if bytes.len() > flash_len {
            self.eeprom.load_data(&bytes[flash_len..]);
        }
    }
}

/// ts:1060-1137 — MegabyterMapper. Protovision MegaByter: 1MB MX29F800CB flash
/// (128×8K banks, ROML only). IO1 ($DE00): addr bit1 → register_02 (mode in bits
/// 0..1, LED in bit7), else register_00 (ROM bank & 0x7f). Mode selects
/// 8K/16K/RAM(off)/ULTIMAX. Flash read AND programmed at ROML $8000-$9FFF; no ROMH.
#[derive(Clone)]
pub struct MegabyterMapper {
    register00: u8, // bank
    register02: u8, // mode (bits 0-1) + LED (bit 7)
    flash: Flash040,
}

impl MegabyterMapper {
    pub fn new(image: &ParsedCartridgeImage) -> Self {
        MegabyterMapper {
            register00: 0,
            register02: 0,
            flash: Flash040::new(
                build_linear_chip_data(image, |b| b.roml.as_ref(), 128),
                "megabyter",
                FLASH800_CB,
            ),
        }
    }
    fn flash_offset(&self, address: u16) -> u32 {
        ((self.register00 as u32) * 0x2000) + ((address & 0x1fff) as u32)
    }
}

impl CartMapper for MegabyterMapper {
    fn mapper_type(&self) -> MapperType {
        MapperType::MegaByter
    }
    /// ts:1073-1080 — lines per register_02 mode bits.
    fn get_lines(&self) -> CartLines {
        match self.register02 & 0x03 {
            0x00 => CartLines { exrom: 0, game: 1 }, // 8K
            0x01 => CartLines { exrom: 0, game: 0 }, // 16K
            0x02 => CartLines { exrom: 1, game: 1 }, // RAM (off)
            _ => CartLines { exrom: 1, game: 0 },    // ULTIMAX
        }
    }
    /// ts:1082-1085 — read: ROML flash only.
    fn read(&mut self, address: u16, _bank_info: &BankInfo, clk: u64) -> Option<u8> {
        if (0x8000..=0x9fff).contains(&address) {
            return Some(self.flash.read(self.flash_offset(address), clk));
        }
        None
    }
    /// ts:1088-1091 — peek the ROML flash window.
    fn peek(&self, address: u16, _bank_info: &BankInfo) -> Option<u8> {
        if (0x8000..=0x9fff).contains(&address) {
            return Some(self.flash.peek(self.flash_offset(address)));
        }
        None
    }
    /// ts:1093-1110 — write: IO1 bank/mode; flash program only in ultimax.
    fn write(&mut self, address: u16, value: u8, _bank_info: &BankInfo, clk: u64) -> bool {
        if (0xde00..=0xdeff).contains(&address) {
            if address & 2 != 0 {
                self.register02 = value & 0x83;
            } else {
                self.register00 = value & 0x7f;
            }
            return true;
        }
        if (0x8000..=0x9fff).contains(&address) {
            if (self.register02 & 0x03) == 0x03 {
                self.flash.store(self.flash_offset(address), value, clk);
                return true;
            }
            return false;
        }
        false
    }
    /// ts:1136 — reset: bank 0, mode 0 (8K game). Flash DATA preserved.
    fn reset(&mut self) {
        self.register00 = 0;
        self.register02 = 0;
    }
    fn get_state(&self) -> CartState {
        let mut flash = self.flash.clone();
        CartState {
            current_bank: self.register00 as u16,
            control_register: Some(self.register02),
            flash: Some(FlashCartState {
                flash_lo: Some(flash.snapshot_state(0)),
                flash_hi: None,
                eeprom: None,
                easyflash_jumper: 0,
                easyflash_ram: Vec::new(),
            }),
        }
    }
    fn set_state(&mut self, state: CartState) {
        self.register00 = (state.current_bank & 0x7f) as u8;
        self.register02 = state.control_register.unwrap_or(0) & 0x83;
        if let Some(f) = &state.flash {
            if let Some(s) = &f.flash_lo {
                self.flash.restore_state(s);
            }
        }
    }
    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }
    fn is_writable_dirty(&self) -> bool {
        self.flash.is_dirty()
    }
    // ts:1439 — Megabyter persists its MX29F800 flash (writable_image/crt_image).
    fn persists_writable_state(&self) -> bool {
        true
    }
    fn writable_generation(&self) -> u64 {
        self.flash.writable_generation()
    }
    fn writable_image(&mut self, clk: u64) -> Option<Vec<u8>> {
        Some(self.flash.get_data(clk).to_vec())
    }
    fn set_writable_image(&mut self, bytes: &[u8]) {
        self.flash.load_data(bytes);
    }
}

/// C64MegaCart (Replica Software; Martin Piper's VICE fork `c64/cart/c64megacart.c`,
/// itself GMOD2-derived). Micron M29F160FT — 2MB flash (256×8K ROML banks, "top
/// boot"), device `{0x01,0xd2,2}` (`FLASH040_160`). Two write-only registers:
///   $DE00 (IO1, BANK):    low bank byte → bank bits 0-7.
///   $DF00 (IO2, CONTROL): bits 5-0 → bank bits 8-13; bits 7/6 → mode —
///     0xC0 ULTIMAX (flash-write mode, ROMH replaces KERNAL at $E000),
///     0x00 8K GAME, 0x80 Kill/RAM. (0x40 "illegal/float" leaves mode unchanged,
///     mirroring the fork's if/else-if that only handles 0xC0/0x00/0x80.)
/// Flash reads at ROML $8000-$9FFF AND (ultimax) ROMH $E000-$FFFF, and programs
/// ONLY in ultimax via the $E000 window (manual §4 — unlock $AAA/$555). Same
/// offset for both windows: (addr & $1FFF) + (bank << 13). Register reads are
/// open-bus (vicii_read_phi1 in the fork — the CPU never reads them back), so the
/// mapper does not claim IO reads. The fork's separate VIC-side config (cmodeVIC)
/// is folded into the single CPU-side CartLines this trait exposes, as
/// EasyFlash/Megabyter also do here.
#[derive(Clone, Copy, PartialEq)]
enum C64MegaCartMode {
    Game8k,  // $00: EXROM low, GAME high  → ROML $8000-$9FFF
    Ram,     // $80: EXROM high, GAME high → cart killed, internal RAM
    Ultimax, // $C0: EXROM high, GAME low  → ROMH replaces KERNAL at $E000 (flash mode)
}

#[derive(Clone)]
pub struct C64MegaCartMapper {
    bank: u16, // 14-bit ROML bank: bits 0-7 via $DE00, bits 8-13 via $DF00
    mode: C64MegaCartMode,
    flash: Flash040,
}

impl C64MegaCartMapper {
    pub fn new(image: &ParsedCartridgeImage) -> Self {
        C64MegaCartMapper {
            bank: 0,
            mode: C64MegaCartMode::Game8k,
            flash: Flash040::new(
                build_linear_chip_data(image, |b| b.roml.as_ref(), 256),
                "c64megacart",
                FLASH040_160,
            ),
        }
    }
    /// c64megacart.c — (addr & $1FFF) + (roml_bank << 13). Same for ROML ($8000)
    /// and the ultimax ROMH ($E000) window (both use the low 13 address bits).
    fn flash_offset(&self, address: u16) -> u32 {
        ((self.bank as u32) << 13) | ((address & 0x1fff) as u32)
    }
}

impl CartMapper for C64MegaCartMapper {
    fn mapper_type(&self) -> MapperType {
        MapperType::C64MegaCart
    }
    /// CONTROL bits 7/6 → EXROM/GAME (manual §2 Control Bit Mapping).
    fn get_lines(&self) -> CartLines {
        match self.mode {
            C64MegaCartMode::Game8k => CartLines { exrom: 0, game: 1 },
            C64MegaCartMode::Ram => CartLines { exrom: 1, game: 1 },
            C64MegaCartMode::Ultimax => CartLines { exrom: 1, game: 0 },
        }
    }
    /// c64megacart_roml_read (+ ultimax ROMH): the current flash bank at both the
    /// $8000 and $E000 windows. The bus PLA-gates which window is live.
    fn read(&mut self, address: u16, _bank_info: &BankInfo, clk: u64) -> Option<u8> {
        if (0x8000..=0x9fff).contains(&address) || (0xe000..=0xffff).contains(&address) {
            return Some(self.flash.read(self.flash_offset(address), clk));
        }
        None
    }
    fn peek(&self, address: u16, _bank_info: &BankInfo) -> Option<u8> {
        if (0x8000..=0x9fff).contains(&address) || (0xe000..=0xffff).contains(&address) {
            return Some(self.flash.peek(self.flash_offset(address)));
        }
        None
    }
    /// c64megacart_io1_store / io2_store + romh_store. Flash program ONLY in
    /// ultimax via the $E000 ROMH window (manual §4: unlock/erase/program cycles).
    fn write(&mut self, address: u16, value: u8, _bank_info: &BankInfo, clk: u64) -> bool {
        if (0xde00..=0xdeff).contains(&address) {
            self.bank = (self.bank & 0xff00) | value as u16; // BANK: bits 0-7
            return true;
        }
        if (0xdf00..=0xdfff).contains(&address) {
            self.bank = (self.bank & 0x00ff) | (((value & 0x3f) as u16) << 8); // bits 8-13
            match value & 0xc0 {
                0xc0 => self.mode = C64MegaCartMode::Ultimax,
                0x00 => self.mode = C64MegaCartMode::Game8k,
                0x80 => self.mode = C64MegaCartMode::Ram,
                _ => {} // 0x40 illegal/float: mode unchanged (fork).
            }
            return true;
        }
        if self.mode == C64MegaCartMode::Ultimax && (0xe000..=0xffff).contains(&address) {
            self.flash.store(self.flash_offset(address), value, clk);
            return true;
        }
        false
    }
    /// c64megacart_reset: bank 0, 8K GAME. Flash DATA (state machine) preserved —
    /// the fork resets the FSM here too, harmlessly.
    fn reset(&mut self) {
        self.bank = 0;
        self.mode = C64MegaCartMode::Game8k;
    }
    fn get_state(&self) -> CartState {
        let mut flash = self.flash.clone();
        CartState {
            current_bank: self.bank,
            control_register: Some(match self.mode {
                C64MegaCartMode::Game8k => 0x00,
                C64MegaCartMode::Ram => 0x80,
                C64MegaCartMode::Ultimax => 0xc0,
            }),
            flash: Some(FlashCartState {
                flash_lo: Some(flash.snapshot_state(0)),
                flash_hi: None,
                eeprom: None,
                easyflash_jumper: 0,
                easyflash_ram: Vec::new(),
            }),
        }
    }
    fn set_state(&mut self, state: CartState) {
        self.bank = state.current_bank & 0x3fff;
        self.mode = match state.control_register.unwrap_or(0) & 0xc0 {
            0xc0 => C64MegaCartMode::Ultimax,
            0x80 => C64MegaCartMode::Ram,
            _ => C64MegaCartMode::Game8k,
        };
        if let Some(f) = &state.flash {
            if let Some(s) = &f.flash_lo {
                self.flash.restore_state(s);
            }
        }
    }
    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }
    fn is_writable_dirty(&self) -> bool {
        self.flash.is_dirty()
    }
    // C64MegaCart persists its M29F160FT flash (writable_image / crt_image).
    fn persists_writable_state(&self) -> bool {
        true
    }
    fn writable_generation(&self) -> u64 {
        self.flash.writable_generation()
    }
    fn writable_image(&mut self, clk: u64) -> Option<Vec<u8>> {
        Some(self.flash.get_data(clk).to_vec())
    }
    fn set_writable_image(&mut self, bytes: &[u8]) {
        self.flash.load_data(bytes);
    }
}

/// ts:120-154 — mapperFromImage: build the concrete mapper for a parsed image.
/// The read-only families build their banked mapper; the writable flash families
/// (EasyFlash, GMOD2, MegaByter, C64MegaCart) build their flash + EEPROM mapper;
/// the remaining serial/SPI families (GMOD3) yield `Err`.
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
        MapperType::EasyFlash => Ok(Box::new(EasyFlashMapper::new(image))),
        MapperType::Gmod2 => Ok(Box::new(Gmod2Mapper::new(image))),
        MapperType::MegaByter => Ok(Box::new(MegabyterMapper::new(image))),
        MapperType::C64MegaCart => Ok(Box::new(C64MegaCartMapper::new(image))),
        // The self-config harness is constructed directly (SelfConfigCartMapper::new
        // / load_self_config_from_bin), never from a parsed image, and locks a
        // concrete family at runtime — so it has no image-driven build here.
        MapperType::SelfConfig => Err(CrtError::Unsupported(MapperType::SelfConfig)),
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

// ══════════════════════════════════════════════════════════════════════════════
// Spec 790 — raw `.bin` cartridge front-end (typed attach)
//
// A raw `.bin` is the full LINEAR flash/ROM image (every bank present, NO CHIP
// packets), so it carries no cartridge type in-band; the type is supplied out of
// band (CLI `--cart-type` / API `cart_type`). This front-end splits the linear
// image into the SAME `ParsedCartridgeImage` shape `parse_crt` builds, per the
// type's geometry descriptor (§790.7), so every existing mapper works unchanged —
// it is a second FRONT-END, not a second mapper tier.
// ══════════════════════════════════════════════════════════════════════════════

/// The attach intent for a set of cart bytes: either header/structure-driven
/// auto-detect, or a caller-forced concrete type (= VICE `cartridge_attach_image`
/// `type == CARTRIDGE_CRT(0)` vs a positive `CARTRIDGE_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CartType {
    /// Detect from the bytes: `.crt` → header hw type; raw `.bin` → the Spec 790 S1
    /// structural first-cut detect (may yield `BinTypeAmbiguous`).
    Auto,
    /// Force a concrete mapper type: `.crt` → header override; raw `.bin` → the
    /// geometry for this type drives the linear split.
    Forced(MapperType),
}

/// How a bank's bank-unit slice maps onto the ROM windows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BinLayout {
    /// 8 KiB bank → ROML `$8000-$9FFF` only.
    Roml8k,
    /// 16 KiB bank → ROML `$8000-$9FFF` + ROMH `$A000-$BFFF`.
    Roml16kRomhA000,
    /// 16 KiB ultimax bank → ROML `$8000-$9FFF` + ROMH `$E000-$FFFF`.
    Roml16kRomhE000,
}

/// §790.7 — the per-type geometry descriptor. This is the SINGLE source of truth
/// for the `.bin` linear split; `max_banks` is kept in lock-step with each mapper's
/// existing `build_linear_chip_data(image, accessor, bank_count)` capacity so the
/// front-end and the mapper agree on geometry.
struct BinGeometry {
    /// Bytes per bank on disk (0x2000 for an 8K-bank type, 0x4000 for 16K/ultimax).
    bank_unit: usize,
    /// Window mapping for each bank slice.
    layout: BinLayout,
    /// Boot EXROM line (authoritative for the generic NormalMapper; a boot hint for
    /// the flash/banked mappers, which self-determine lines from their mode reg).
    exrom: u8,
    /// Boot GAME line (see `exrom`).
    game: u8,
    /// Bank capacity (matches the mapper's `build_linear_chip_data` bank_count).
    max_banks: usize,
}

/// §790.7 — resolve the geometry descriptor for a concrete mapper type. The
/// serial/SPI `Unsupported` family has no `.bin` geometry (no mapper is built).
fn bin_geometry(mapper_type: MapperType) -> Result<BinGeometry, CrtError> {
    use BinLayout::*;
    let g = match mapper_type {
        // Generic single-bank carts — lines authoritative (NormalMapper).
        MapperType::Normal8k => BinGeometry { bank_unit: 0x2000, layout: Roml8k, exrom: 0, game: 1, max_banks: 1 },
        MapperType::Normal16k => BinGeometry { bank_unit: 0x4000, layout: Roml16kRomhA000, exrom: 0, game: 0, max_banks: 1 },
        MapperType::Ultimax => BinGeometry { bank_unit: 0x4000, layout: Roml16kRomhE000, exrom: 1, game: 0, max_banks: 1 },
        // Banked read-only carts.
        MapperType::Ocean => BinGeometry { bank_unit: 0x2000, layout: Roml8k, exrom: 0, game: 0, max_banks: 64 },
        MapperType::MagicDesk => BinGeometry { bank_unit: 0x2000, layout: Roml8k, exrom: 0, game: 1, max_banks: 128 },
        MapperType::MagicDesk16 => BinGeometry { bank_unit: 0x4000, layout: Roml16kRomhA000, exrom: 0, game: 0, max_banks: 128 },
        // Writable flash carts — boot lines are a hint (mapper resets its own mode).
        // max_banks mirror the mapper build_linear_chip_data bank_count:
        //   EasyFlash 64 (cart.rs new()), Gmod2 64, Megabyter 128, C64MegaCart 256.
        MapperType::EasyFlash => BinGeometry { bank_unit: 0x4000, layout: Roml16kRomhA000, exrom: 1, game: 0, max_banks: 64 },
        MapperType::Gmod2 => BinGeometry { bank_unit: 0x2000, layout: Roml8k, exrom: 0, game: 1, max_banks: 64 },
        MapperType::MegaByter => BinGeometry { bank_unit: 0x2000, layout: Roml8k, exrom: 0, game: 1, max_banks: 128 },
        MapperType::C64MegaCart => BinGeometry { bank_unit: 0x2000, layout: Roml8k, exrom: 0, game: 1, max_banks: 256 },
        // The harness has no static geometry — it re-derives the concrete type's
        // geometry via `bin_geometry(concrete)` at lock time.
        MapperType::SelfConfig => return Err(CrtError::Unsupported(MapperType::SelfConfig)),
        MapperType::Unsupported => return Err(CrtError::Unsupported(MapperType::Unsupported)),
    };
    Ok(g)
}

/// §790.1 — `parse_bin`: build a `ParsedCartridgeImage` from a raw linear `.bin`
/// image of a KNOWN `mapper_type`. Same output shape as `parse_crt`; only the
/// source differs (linear `N*bank_unit` split vs CHIP packets).
///
/// - Optional 2-byte load-address strip when `data.len() % bank_unit == 2` (VICE
///   `UTIL_FILE_LOAD_SKIP_ADDRESS`, a PRG-style prepended address).
/// - Size rule (more lenient than VICE's exact gate, matching our 0xFF-pad): accept
///   `len == bank_unit * k`, `1 ≤ k ≤ max_banks`; absent trailing banks are 0xFF via
///   the mapper's `build_linear_chip_data`. A non-multiple or over-max size is a hard
///   `CrtError::BadBinSize` — never a silent truncation.
pub fn parse_bin(
    data: &[u8],
    path: &str,
    name: &str,
    mapper_type: MapperType,
) -> Result<ParsedCartridgeImage, CrtError> {
    let geom = bin_geometry(mapper_type)?;

    // Optional 2-byte load-address strip (§790.1 / VICE util.c:368 SKIP_ADDRESS).
    let slice: &[u8] = if geom.bank_unit != 0 && data.len() % geom.bank_unit == 2 {
        &data[2..]
    } else {
        data
    };

    // Size gate: a whole number of bank units, 1..=max_banks.
    if geom.bank_unit == 0 || slice.is_empty() || slice.len() % geom.bank_unit != 0 {
        return Err(CrtError::BadBinSize {
            len: data.len(),
            bank_unit: geom.bank_unit,
            max_banks: geom.max_banks,
        });
    }
    let k = slice.len() / geom.bank_unit;
    if k < 1 || k > geom.max_banks {
        return Err(CrtError::BadBinSize {
            len: data.len(),
            bank_unit: geom.bank_unit,
            max_banks: geom.max_banks,
        });
    }

    // Linear split: bank N at `N * bank_unit`.
    let mut banks: BTreeMap<u16, CrtBank> = BTreeMap::new();
    let mut profiles: std::collections::BTreeSet<u8> = std::collections::BTreeSet::new();
    for n in 0..k {
        let off = n * geom.bank_unit;
        let mut bank = CrtBank::default();
        match geom.layout {
            BinLayout::Roml8k => {
                bank.roml = Some(normalize_bank_data(&slice[off..off + 0x2000]));
                profiles.insert(CrtLoadProfile::Roml as u8);
            }
            BinLayout::Roml16kRomhA000 => {
                bank.roml = Some(normalize_bank_data(&slice[off..off + 0x2000]));
                bank.romh_a000 = Some(normalize_bank_data(&slice[off + 0x2000..off + 0x4000]));
                profiles.insert(CrtLoadProfile::Roml as u8);
                profiles.insert(CrtLoadProfile::RomhA000 as u8);
            }
            BinLayout::Roml16kRomhE000 => {
                bank.roml = Some(normalize_bank_data(&slice[off..off + 0x2000]));
                bank.romh_e000 = Some(normalize_bank_data(&slice[off + 0x2000..off + 0x4000]));
                profiles.insert(CrtLoadProfile::Roml as u8);
                profiles.insert(CrtLoadProfile::RomhE000 as u8);
            }
        }
        banks.insert(n as u16, bank);
    }

    let display_name = if name.trim().is_empty() {
        std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string())
    } else {
        name.to_string()
    };

    Ok(ParsedCartridgeImage {
        path: path.to_string(),
        name: display_name,
        mapper_type,
        exrom: geom.exrom,
        game: geom.game,
        banks,
        profiles,
        raw_bytes: slice.to_vec(),
    })
}

/// True when `bytes` carry the `C64 CARTRIDGE   ` signature (a `.crt` container,
/// vs a raw `.bin`). The one structural discriminator the smart-attach door uses.
pub fn is_crt(bytes: &[u8]) -> bool {
    bytes.len() >= 16 && bytes[0..16] == CRT_SIGNATURE[..]
}

/// §790.1 — parse a raw `.bin` and build its mapper in one step (= the `.crt`
/// `load_cartridge_from_bytes`). `path` defaults to `name` for the image record.
pub fn load_cartridge_from_bin(
    data: &[u8],
    name: &str,
    mapper_type: MapperType,
) -> Result<(ParsedCartridgeImage, Box<dyn CartMapper>), CrtError> {
    let image = parse_bin(data, name, name, mapper_type)?;
    let mapper = mapper_from_image(&image)?;
    Ok((image, mapper))
}

/// §790.2 — resolve a `--cart-type` / `cart_type` string (a VICE numeric id OR a
/// mnemonic, case-insensitive) into a `CartType`. `crt`/`auto`/`0` (the
/// `CARTRIDGE_CRT` sentinel) → `CartType::Auto`; every concrete name/id →
/// `CartType::Forced`. Unknown → `CrtError::UnknownCartType`.
///
/// (The declared signature in the spec is `-> Result<MapperType, CrtError>`, but
/// the sentinel `crt`/`auto`/`0` cannot be a `MapperType`; `CartType` is the type
/// that carries BOTH a concrete mapper and the auto sentinel, which is exactly what
/// "→ a sentinel meaning detect from header/auto" requires — so it is the coherent
/// return type and the CLI/attach layer consumes it directly.)
pub fn resolve_cart_type(s: &str) -> Result<CartType, CrtError> {
    let t = s.trim().to_ascii_lowercase();
    // Auto/detect sentinel (VICE CARTRIDGE_CRT == 0).
    if matches!(t.as_str(), "crt" | "auto" | "0") {
        return Ok(CartType::Auto);
    }
    // Numeric VICE id (positive = also the .crt header type; negatives = the
    // VICE-internal generic families).
    if let Ok(id) = t.parse::<i32>() {
        let mt = match id {
            -3 => MapperType::Normal8k,   // CARTRIDGE_GENERIC_8KB
            -2 => MapperType::Normal16k,  // CARTRIDGE_GENERIC_16KB
            -6 => MapperType::Ultimax,    // CARTRIDGE_ULTIMAX
            5 => MapperType::Ocean,
            19 => MapperType::MagicDesk,
            32 => MapperType::EasyFlash,
            60 => MapperType::Gmod2,
            85 => MapperType::MagicDesk16,
            86 => MapperType::MegaByter,
            61 => MapperType::C64MegaCart, // martinpiper fork
            _ => return Err(CrtError::UnknownCartType(s.to_string())),
        };
        return Ok(CartType::Forced(mt));
    }
    // Mnemonic (LLM-friendly).
    let mt = match t.as_str() {
        "ef" | "easyflash" => MapperType::EasyFlash,
        "gmod2" => MapperType::Gmod2,
        "megabyter" | "mb" => MapperType::MegaByter,
        "c64megacart" | "c64mc" => MapperType::C64MegaCart,
        "magicdesk" | "md" => MapperType::MagicDesk,
        "md16" | "magicdesk16" => MapperType::MagicDesk16,
        "ocean" => MapperType::Ocean,
        "8k" | "generic8k" => MapperType::Normal8k,
        "16k" | "generic16k" => MapperType::Normal16k,
        "ultimax" => MapperType::Ultimax,
        _ => return Err(CrtError::UnknownCartType(s.to_string())),
    };
    Ok(CartType::Forced(mt))
}

/// §790.3 (S1) — STRUCTURAL-ONLY first-cut type detect for a raw `.bin` attached
/// with `CartType::Auto`. This slice only settles the unambiguous structural cases;
/// the genuinely ambiguous flash carts are left to the caller's explicit
/// `--cart-type` (or, later, the Spec 790 S2 runtime self-configuring harness).
///
/// 1. EAPI signature `65 61 70 69` ("eapi") at bank-0 ROMH `$1800` (= file offset
///    `$3800` under the 16K-interleaved EasyFlash layout) → EasyFlash.
/// 2. CBM80 autostart signature (`C3 C2 CD 38 30`) at ROML `$8004` (= file offset
///    `$0004`) on a single-bank image → generic 8K (`$2000`) / 16K (`$4000`).
/// 3. A 16K image with no CBM80 whose reset vector (`$3FFC/$3FFD`, i.e. ROMH
///    `$1FFC/$1FFD`) points into `$E000-$FFFF` → ultimax.
/// Anything else → `BinTypeAmbiguous`.
pub fn detect_bin_type(data: &[u8]) -> Result<MapperType, CrtError> {
    // 1. EasyFlash EAPI at bank-0 ROMH $1800 (16K-interleaved: file offset $3800).
    if data.len() >= 0x3804 && &data[0x3800..0x3804] == b"eapi" {
        return Ok(MapperType::EasyFlash);
    }
    // 2. CBM80 autostart signature at ROML $8004.
    const CBM80: [u8; 5] = [0xc3, 0xc2, 0xcd, 0x38, 0x30];
    let has_cbm80 = data.len() >= 9 && data[4..9] == CBM80;
    if has_cbm80 {
        match data.len() {
            0x2000 => return Ok(MapperType::Normal8k),
            0x4000 => return Ok(MapperType::Normal16k),
            _ => {} // CBM80 but a multi-bank/odd size ⇒ not confidently generic.
        }
    }
    // 3. Ultimax: a 16K image, no low CBM80, reset vector into $E000-$FFFF.
    if data.len() == 0x4000 && !has_cbm80 {
        let reset = (data[0x3ffc] as u16) | ((data[0x3ffd] as u16) << 8);
        if (0xe000..=0xffff).contains(&reset) {
            return Ok(MapperType::Ultimax);
        }
    }
    Err(CrtError::BinTypeAmbiguous)
}

// ══════════════════════════════════════════════════════════════════════════════
// Spec 790 S2 — runtime self-configuring cart harness
//
// A raw multi-bank flash `.bin` carries no reliable STATIC type marker (S1's
// `detect_bin_type` returns `BinTypeAmbiguous` for it). Instead of erroring, the
// `Auto` path attaches this harness: it boots the image as a generic $DE00-banked
// 8K-game cart, then WATCHES the register accesses the loader makes and LOCKS the
// concrete family in-place on the first type-specific access. The nice property
// (confirmed by observing two real flash dumps of the same title): the FIRST
// cart-register write a loader makes is already the discriminator —
//   * a C64MegaCart image writes its $DF00 control register first, and never $DE02;
//   * a Megabyter image writes its $DE02 mode register first, and never $DF00.
// So very little type-specific behaviour is needed before the lock, and the lock
// is unambiguous.
//
// Detection rules (SPECIFIC-FIRST — a specific discriminator always pre-empts the
// generic $DE00 fallback):
//   * $DF00-$DFFF write (IO2)          → C64MegaCart   (EF's $DF00 IO2-RAM is
//                                        guarded by the eapi cue → EasyFlash).
//   * $DE00-$DEFF write with bit1 set  → EasyFlash if the eapi signature is
//     ($DE02-family = mode register)     present, else Megabyter.
//   * $DE00-$DEFF read used as an       → GMOD2 (needs a prior EEPROM clocking
//     M93C86 EEPROM DO poll               pattern, so it never misfires on a plain
//                                         C64MegaCart high-bank number).
//   * only $DE00 banking for a long     → MagicDesk (Ocean if 512 KiB) — the
//     while, no specific access           residual $DE00-only family (fallback).
//
// On lock the harness re-parses the raw image with the concrete type's geometry
// (`load_cartridge_from_bin`), transfers the tracked bank-low, and delegates ALL
// subsequent read/write/peek/get_lines/state/writable calls to the concrete
// mapper; `mapper_type()` then returns the concrete type (never `SelfConfig`).
// ══════════════════════════════════════════════════════════════════════════════

/// The residual $DE00-only fallback fires after this many $DE00 bank-select writes
/// with NO specific discriminator ($DF00 / $DE02-family / EEPROM read) seen — the
/// cart is then behaviourally a Magic Desk / Ocean ($DE00-banked 8K game). The two
/// real flash families this harness targets (C64MegaCart, Megabyter) fire their
/// specific register on the FIRST cart access (observed ~1.8M cycles into boot,
/// before any $DE00 write), so this fallback is never reached for them; it is the
/// clean answer only for a genuinely $DE00-only image. Kept generous so a
/// late-banking specific cart still locks correctly on its eventual mode write.
pub const SELFCONFIG_MAGICDESK_FALLBACK_WRITES: u32 = 256;

/// Spec 790 S2 — the runtime self-configuring cart mapper. See the module banner
/// above. Holds the raw `.bin`, a generic AMD flash FSM over its ROML image, the
/// pre-lock banking state, and (once resolved) the concrete delegate mapper.
pub struct SelfConfigCartMapper {
    /// The raw linear `.bin` (re-parsed with the concrete geometry on lock).
    raw: Vec<u8>,
    name: String,
    // ── structural boot-config (just enough to BOOT before discriminators fire) ──
    /// eapi OR a 16K reset-vector into $E000-$FFFF → boot ultimax; else 8K game.
    boot_ultimax: bool,
    /// The EAPI signature is present at bank-0 ROMH $1800 — the EasyFlash tiebreak.
    eapi_present: bool,
    // ── pre-lock generic banking ──
    /// ROML bank low byte = the last $DE00-family ($DExx, bit1 clear) write value.
    bank_low: u16,
    /// The last $DE00-family write value (the GMOD2 EEPROM CS/CLK/DI line state).
    last_de00_write: u8,
    /// Count of GMOD2-shaped EEPROM clock edges (CS held, CLK bit toggling) — the
    /// GMOD2 read discriminator requires ≥2 so it can never trip on a C64MegaCart
    /// high-bank number write followed by an incidental read.
    eeprom_clock_edges: u32,
    /// $DE00-family bank-select write count (the Magic Desk / Ocean fallback timer).
    de00_write_count: u32,
    /// A generic AMD flash serving ROML reads; the concrete mapper owns the flash
    /// post-lock (flash programming only ever happens in ultimax = post-lock).
    flash: Flash040,
    // ── lock ──
    /// The concrete mapper once the type is locked; None while still detecting.
    resolved: Option<Box<dyn CartMapper>>,
    /// The locked concrete type (mirrors `resolved`), for `mapper_type()`.
    resolved_type: Option<MapperType>,
}

impl Clone for SelfConfigCartMapper {
    fn clone(&self) -> Self {
        SelfConfigCartMapper {
            raw: self.raw.clone(),
            name: self.name.clone(),
            boot_ultimax: self.boot_ultimax,
            eapi_present: self.eapi_present,
            bank_low: self.bank_low,
            last_de00_write: self.last_de00_write,
            eeprom_clock_edges: self.eeprom_clock_edges,
            de00_write_count: self.de00_write_count,
            flash: self.flash.clone(),
            // Box<dyn CartMapper> clones via clone_box (the blanket impl above).
            resolved: self.resolved.clone(),
            resolved_type: self.resolved_type,
        }
    }
}

/// Compute the minimal structural boot-config from the raw bytes: enough to BOOT
/// the image correctly (right GAME/EXROM) before the runtime discriminators fire.
fn selfconfig_boot_config(raw: &[u8]) -> (bool /*ultimax*/, bool /*eapi*/) {
    // eapi at bank-0 ROMH $1800 (16K-interleaved file offset $3800) → EasyFlash,
    // which boots ultimax.
    let eapi = raw.len() >= 0x3804 && &raw[0x3800..0x3804] == b"eapi";
    if eapi {
        return (true, true);
    }
    // A 16K image with no CBM80 whose reset vector ($3FFC/$3FFD) points into
    // $E000-$FFFF → ultimax (the machine reboots into the cart ROMH).
    if raw.len() == 0x4000 {
        let reset = (raw[0x3ffc] as u16) | ((raw[0x3ffd] as u16) << 8);
        if (0xe000..=0xffff).contains(&reset) {
            return (true, false);
        }
    }
    // else boot the common banked-flash config: 8K game (EXROM low, GAME high), so
    // bank-0 ROML is at $8000 and its CBM80 autostart triggers the loader.
    (false, false)
}

impl SelfConfigCartMapper {
    /// Construct the harness for a raw `.bin` (its concrete type is unknown until
    /// the loader runs and touches a type-specific register).
    pub fn new(raw: &[u8], name: &str) -> Self {
        let (boot_ultimax, eapi_present) = selfconfig_boot_config(raw);
        SelfConfigCartMapper {
            raw: raw.to_vec(),
            name: name.to_string(),
            boot_ultimax,
            eapi_present,
            bank_low: 0,
            last_de00_write: 0,
            eeprom_clock_edges: 0,
            de00_write_count: 0,
            // A generic 2 MiB AM29F-class row covers every candidate image; the row
            // is only load-bearing post-lock (autoselect/program), which the
            // concrete mapper owns, so pre-lock it just serves ROML array reads.
            flash: Flash040::new(raw.to_vec(), "self-config", FLASH040_160),
            resolved: None,
            resolved_type: None,
        }
    }

    /// Whether a concrete family has been locked.
    #[inline]
    pub fn is_resolved(&self) -> bool {
        self.resolved.is_some()
    }

    /// The pre-lock boot EXROM/GAME lines from the structural boot-config.
    #[inline]
    fn boot_lines(&self) -> CartLines {
        if self.boot_ultimax {
            CartLines { exrom: 1, game: 0 } // ultimax (eapi / reset-vector cart)
        } else {
            CartLines { exrom: 0, game: 1 } // 8K game (bank-0 ROML autostart)
        }
    }

    /// The linear ROML flash offset for `address` in the current pre-lock bank.
    #[inline]
    fn roml_offset(&self, address: u16) -> u32 {
        ((self.bank_low as u32) << 13) | ((address & 0x1fff) as u32)
    }

    /// Lock the concrete family: re-parse the raw image with `t`'s geometry, build
    /// the concrete mapper, and transfer the tracked bank-low into it (via a $DE00
    /// bank-select write — every candidate maps a $DE00 write to its bank register).
    /// The TRIGGERING register write is re-applied by the caller after this returns.
    fn lock(&mut self, t: MapperType, bank_info: &BankInfo, clk: u64) {
        if self.resolved.is_some() {
            return;
        }
        // Geometry mismatch (e.g. size exceeds the concrete family's capacity) keeps
        // the harness detecting generically (mapper_type stays SelfConfig) rather than
        // attaching a wrong-geometry mapper — practically unreachable for the families
        // this harness locks.
        if let Ok((_img, mut mapper)) = load_cartridge_from_bin(&self.raw, &self.name, t) {
            if self.bank_low != 0 {
                mapper.write(0xde00, self.bank_low as u8, bank_info, clk);
            }
            self.resolved = Some(mapper);
            self.resolved_type = Some(t);
        }
    }
}

impl CartMapper for SelfConfigCartMapper {
    fn mapper_type(&self) -> MapperType {
        self.resolved_type.unwrap_or(MapperType::SelfConfig)
    }

    fn get_lines(&self) -> CartLines {
        match &self.resolved {
            Some(m) => m.get_lines(),
            None => self.boot_lines(),
        }
    }

    fn read(&mut self, address: u16, bank_info: &BankInfo, clk: u64) -> Option<u8> {
        if let Some(m) = self.resolved.as_mut() {
            return m.read(address, bank_info, clk);
        }
        // ── pre-lock generic read ──
        if (0xde00..=0xdeff).contains(&address) {
            // A meaningful IO1 read is GMOD2's M93C86 DO poll — but only once we've
            // seen the EEPROM being clocked (CS held while CLK toggles), so an
            // incidental read on a $DE00-banked cart can't trip it.
            if self.eeprom_clock_edges >= 2 && (self.last_de00_write & 0x40) != 0 {
                self.lock(MapperType::Gmod2, bank_info, clk);
                if let Some(m) = self.resolved.as_mut() {
                    return m.read(address, bank_info, clk);
                }
            }
            return None; // open bus (no cart IO1 read pre-lock)
        }
        if (0xdf00..=0xdfff).contains(&address) {
            return None; // IO2 read → open bus pre-lock
        }
        if (0x8000..=0x9fff).contains(&address) {
            return Some(self.flash.read(self.roml_offset(address), clk));
        }
        // ROMH windows only for an ultimax boot (best-effort ROML-bank serving; the
        // interleaved-EF case is caught structurally before the harness).
        if self.boot_ultimax
            && ((0xa000..=0xbfff).contains(&address) || (0xe000..=0xffff).contains(&address))
        {
            return Some(self.flash.read(self.roml_offset(address), clk));
        }
        None
    }

    fn peek(&self, address: u16, bank_info: &BankInfo) -> Option<u8> {
        if let Some(m) = self.resolved.as_ref() {
            return m.peek(address, bank_info);
        }
        if (0x8000..=0x9fff).contains(&address) {
            return Some(self.flash.peek(self.roml_offset(address)));
        }
        if self.boot_ultimax
            && ((0xa000..=0xbfff).contains(&address) || (0xe000..=0xffff).contains(&address))
        {
            return Some(self.flash.peek(self.roml_offset(address)));
        }
        None
    }

    fn write(&mut self, address: u16, value: u8, bank_info: &BankInfo, clk: u64) -> bool {
        if let Some(m) = self.resolved.as_mut() {
            return m.write(address, value, bank_info, clk);
        }
        // ── pre-lock discriminator watch (SPECIFIC-FIRST) ──
        // IO2 ($DF00) write → C64MegaCart control register (unless eapi ⇒ EF IO2-RAM).
        if (0xdf00..=0xdfff).contains(&address) {
            let t = if self.eapi_present {
                MapperType::EasyFlash
            } else {
                MapperType::C64MegaCart
            };
            self.lock(t, bank_info, clk);
            if let Some(m) = self.resolved.as_mut() {
                return m.write(address, value, bank_info, clk);
            }
            return true;
        }
        if (0xde00..=0xdeff).contains(&address) {
            // $DExx with bit1 set = the EasyFlash / Megabyter MODE register.
            if address & 2 != 0 {
                let t = if self.eapi_present {
                    MapperType::EasyFlash
                } else {
                    MapperType::MegaByter
                };
                self.lock(t, bank_info, clk);
                if let Some(m) = self.resolved.as_mut() {
                    return m.write(address, value, bank_info, clk);
                }
                return true;
            }
            // $DExx bit1 clear = the generic ROML bank-low select (every candidate).
            // Track the GMOD2 EEPROM clocking pattern: CS (bit6) held while the CLK
            // (bit5) bit toggles between successive writes.
            if (value & 0x40) != 0 && (self.last_de00_write & 0x40) != 0 && ((value ^ self.last_de00_write) & 0x20) != 0 {
                self.eeprom_clock_edges = self.eeprom_clock_edges.saturating_add(1);
            }
            self.last_de00_write = value;
            self.bank_low = value as u16;
            self.de00_write_count = self.de00_write_count.saturating_add(1);
            // Residual $DE00-only fallback → Magic Desk (Ocean if 512 KiB).
            if self.de00_write_count >= SELFCONFIG_MAGICDESK_FALLBACK_WRITES {
                let t = if self.raw.len() == 0x80000 {
                    MapperType::Ocean
                } else {
                    MapperType::MagicDesk
                };
                self.lock(t, bank_info, clk);
                if let Some(m) = self.resolved.as_mut() {
                    return m.write(address, value, bank_info, clk);
                }
            }
            return true;
        }
        // ROML/ROMH writes: program the flash only in an ultimax boot (faithful to
        // "flash program only in ultimax"); else non-consumed (falls to RAM).
        if self.boot_ultimax
            && ((0x8000..=0x9fff).contains(&address) || (0xe000..=0xffff).contains(&address))
        {
            self.flash.store(self.roml_offset(address), value, clk);
            return true;
        }
        false
    }

    fn reset(&mut self) {
        if let Some(m) = self.resolved.as_mut() {
            m.reset();
            return;
        }
        // Pre-lock reset → boot config: bank 0, cleared detection counters. The
        // pre-lock flash array is never programmed (programming is post-lock in
        // ultimax), so it needs no re-init.
        self.bank_low = 0;
        self.last_de00_write = 0;
        self.eeprom_clock_edges = 0;
        self.de00_write_count = 0;
    }

    fn get_state(&self) -> CartState {
        match &self.resolved {
            Some(m) => m.get_state(),
            None => CartState { current_bank: self.bank_low, control_register: None, flash: None },
        }
    }

    fn set_state(&mut self, state: CartState) {
        match self.resolved.as_mut() {
            Some(m) => m.set_state(state),
            None => self.bank_low = state.current_bank & 0xff,
        }
    }

    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }

    // ── writable tier: delegate to the concrete mapper once locked ──
    fn is_writable_dirty(&self) -> bool {
        self.resolved.as_ref().map(|m| m.is_writable_dirty()).unwrap_or(false)
    }
    fn writable_generation(&self) -> u64 {
        self.resolved.as_ref().map(|m| m.writable_generation()).unwrap_or(0)
    }
    fn persists_writable_state(&self) -> bool {
        self.resolved.as_ref().map(|m| m.persists_writable_state()).unwrap_or(false)
    }
    fn writable_image(&mut self, clk: u64) -> Option<Vec<u8>> {
        self.resolved.as_mut().and_then(|m| m.writable_image(clk))
    }
    fn set_writable_image(&mut self, bytes: &[u8]) {
        if let Some(m) = self.resolved.as_mut() {
            m.set_writable_image(bytes);
        }
    }
    fn crt_image(&mut self, clk: u64) -> Option<Vec<u8>> {
        self.resolved.as_mut().and_then(|m| m.crt_image(clk))
    }
}

/// Spec 790 S2 — build the self-configuring harness for a raw `.bin` whose type S1
/// could not settle. Returns a `SelfConfig`-typed image record (raw bytes + name,
/// 8K-game boot lines) alongside the harness. The concrete type is resolved at
/// runtime (query `mapper_type()` after the loader banks).
pub fn load_self_config_from_bin(
    data: &[u8],
    name: &str,
) -> Result<(ParsedCartridgeImage, Box<dyn CartMapper>), CrtError> {
    let image = ParsedCartridgeImage {
        path: name.to_string(),
        name: name.to_string(),
        mapper_type: MapperType::SelfConfig,
        exrom: 0,
        game: 1, // 8K-game boot default; the harness's get_lines() is authoritative
        banks: BTreeMap::new(),
        profiles: std::collections::BTreeSet::new(),
        raw_bytes: data.to_vec(),
    };
    let mapper: Box<dyn CartMapper> = Box::new(SelfConfigCartMapper::new(data, name));
    Ok((image, mapper))
}
