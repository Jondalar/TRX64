//! ef3.rs — the EasyFlash 3 cartridge (CRT hardware type 90).
//!
//! Source: skoe's EF3 sources as carried in github.com/FrankBuss/kerberos `1bc1352`,
//! `skoe-easyflash/Hardware/ef3-vhdl/src/` — `ef3.vhdl` (mode register, address mux, merge of
//! the cartridges' outputs), `cart_easyflash.vhdl` (registers, lines, boot flag),
//! `cart_io2ram.vhdl`, `cart_usb.vhdl` (the `$DE08` ID), `reset_generator.vhdl` — under their
//! zlib-style licence (behaviour re-implemented; no copied code).
//!
//! The cartridge is one 8 MB MX29LV640EB flash, eight slots of 64 banks of 16 KB, a 256-byte
//! IO2 RAM, three buttons and a mode register. In scope here: the EasyFlash mode (the other
//! cartridges the CPLD hosts — KERNAL, AR/RR/NP, SS5, C128 — are not emulated, and a program
//! that selects one gets the cartridge switched off and told so), the slots, the buttons, flash
//! read/program/erase. USB (`$DE09`/`$DE0A`) answers "no data, not ready".
//!
//! Where the flash byte comes from (`ef3.vhdl` `set_mem_addr`):
//! `slot<<20 | bank[5:3]<<17 | half<<16 | bank[2:0]<<13 | (addr & $1FFF)`, `half` = 0 for
//! ROML and 1 for ROMH. A bank is therefore NOT 16 KB contiguous in the chip: its ROML and
//! ROMH halves lie 64 KB apart, and a 64 KB erase sector is eight banks of ONE half.

use std::collections::BTreeSet;

use crate::cart::{
    BankInfo, CartLines, CartMapper, CartState, CrtError, FlashCartState, MapperType,
    ParsedCartridgeImage,
};
use crate::flash040::{Flash040, FLASH_MX29LV640EB};

/// 8 slots × 64 banks × 16 KB.
pub const EF3_FLASH_SIZE: usize = 0x80_0000;
/// CHIP packets carry `slot*64 + bank`.
pub const EF3_BANKS: u16 = 512;

/// `$DE08` — the CPLD version register, 1.1.1 = `01 001 001` (`cart_usb.vhdl`).
const EF3_VERSION: u8 = 0x49;

/// Flash offset of byte `off` (0..$1FFF) of one half of one bank of one slot.
#[inline]
pub fn ef3_flash_addr(slot: u8, bank: u8, romh: bool, off: u16) -> usize {
    ((slot as usize & 7) << 20)
        | (((bank as usize >> 3) & 7) << 17)
        | ((romh as usize) << 16)
        | ((bank as usize & 7) << 13)
        | (off as usize & 0x1fff)
}

/// What `$DE0F` selected, by CPLD mode number (`ef3.vhdl:690-735`).
fn mode_name(n: u8) -> &'static str {
    match n {
        0 => "EasyFlash + reset",
        1 => "EasyFlash",
        2 => "KERNAL",
        4 => "AR/RR/NP",
        5 => "SS5",
        6 => "C128",
        7 => "off + reset",
        _ => "off",
    }
}

/// The mapper's own state. Everything here rides a checkpoint (`FlashCartState::ef3`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ef3State {
    /// `$DE01`, 3 bits.
    pub slot: u8,
    /// `$DE00` bits 5:0.
    pub bank: u8,
    /// The write-only `$DE02` as the monitor's peek shows it: `& $8F` (bits 6:4 are ignored).
    pub reg02: u8,
    /// `ctrl_game` / `ctrl_exrom` as `cart_easyflash.vhdl` registers them: GAME already has
    /// the boot flag folded in (it reaches the line only at a reset or a `$DE02` write with
    /// M = 0), and `true` means the line is asserted (low).
    pub ctrl_game: bool,
    pub ctrl_exrom: bool,
    pub no_vicii: bool,
    pub led: bool,
    /// `easyflash_boot`.
    pub boot: bool,
    /// The mode enables. `enable_ef` = the EasyFlash mode answers; `enable_menu` = `$DE0F`
    /// decodes. Both clear = the kill state.
    pub enable_ef: bool,
    pub enable_menu: bool,
    /// Buttons answer only after all three have been released once since power-on.
    pub buttons_enabled: bool,
    /// `reset_generator`'s `go_64`: GAME is pulled low from the start of a cart-generated
    /// reset until the first ROMH access.
    pub go64_pull: bool,
    /// A cart-generated reset is waiting for the machine to run it.
    pub reset_request: bool,
    /// The last value `$DE0F` took (`& $0F`), `$FF` = none since the last reset to menu.
    pub last_mode: u8,
    /// `last_mode` named a mode this emulation does not have (2, 4, 5, 6).
    pub not_emulated: bool,
}

impl Ef3State {
    /// Power-on and external reset (`n_sys_reset = 0`): EasyFlash mode, `$DE0F` enabled,
    /// slot 0, bank 0, boot flag set, `$DE02` cleared.
    fn power_on() -> Self {
        Ef3State {
            slot: 0,
            bank: 0,
            reg02: 0,
            ctrl_game: true,
            ctrl_exrom: false,
            no_vicii: false,
            led: false,
            boot: true,
            enable_ef: true,
            enable_menu: true,
            // The emulated buttons are all released when the cartridge powers up.
            buttons_enabled: true,
            go64_pull: false,
            reset_request: false,
            last_mode: 0xff,
            not_emulated: false,
        }
    }
}

/// The status a front-end shows (`ef3Mode` in the daemon's cart status).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ef3Status {
    /// `"ef"` while the EasyFlash mode answers, `"off"` in the kill state.
    pub state: &'static str,
    pub slot: u8,
    pub bank: u8,
    pub boot: bool,
    pub menu_enabled: bool,
    pub buttons_enabled: bool,
    pub led: bool,
    pub no_vicii: bool,
    /// The last `$DE0F` value and its name; `None` since the last reset to menu.
    pub last_mode: Option<(u8, &'static str)>,
    pub not_emulated: bool,
    /// The sentence the log carries for an out-of-scope mode.
    pub notice: Option<String>,
}

#[derive(Clone)]
pub struct Ef3Mapper {
    /// The CRT header the image came with, kept so `crt_image` writes the same one back.
    header: Vec<u8>,
    /// `slot*64+bank` whose original ROMH packet loaded at `$E000` (all others `$A000`).
    romh_e000: BTreeSet<u16>,
    flash: Flash040,
    io_ram: [u8; 256],
    s: Ef3State,
}

impl Ef3Mapper {
    pub fn new(image: &ParsedCartridgeImage) -> Result<Self, CrtError> {
        let mut data = vec![0xffu8; EF3_FLASH_SIZE];
        let mut romh_e000 = BTreeSet::new();
        for (&key, bank) in &image.banks {
            if key >= EF3_BANKS {
                return Err(CrtError::BankOutOfRange { bank: key, limit: EF3_BANKS });
            }
            let (slot, b) = ((key >> 6) as u8, (key & 63) as u8);
            if let Some(roml) = bank.roml.as_ref() {
                let a = ef3_flash_addr(slot, b, false, 0);
                data[a..a + 0x2000].copy_from_slice(roml);
            }
            if let Some(romh) = bank.romh_a000.as_ref().or(bank.romh_e000.as_ref()) {
                let a = ef3_flash_addr(slot, b, true, 0);
                data[a..a + 0x2000].copy_from_slice(romh);
            }
            if bank.romh_e000.is_some() && bank.romh_a000.is_none() {
                romh_e000.insert(key);
            }
        }
        let header = header_of(image);
        // The power-on contents of the IO2 RAM are not in the source: the same pattern
        // as the EasyFlash's (`easyflash_powerup`).
        let mut io_ram = [0u8; 256];
        for (i, slot) in io_ram.iter_mut().enumerate() {
            *slot = if (((i + 1) >> 1) & 1) != 0 { 0x00 } else { 0xff };
        }
        Ok(Ef3Mapper {
            header,
            romh_e000,
            flash: Flash040::new(data, "ef3", FLASH_MX29LV640EB),
            io_ram,
            s: Ef3State::power_on(),
        })
    }

    #[inline]
    fn flash_offset(&self, address: u16) -> usize {
        // ROML = $8000-$9FFF; ROMH = $A000-$BFFF (16K) or $E000-$FFFF (Ultimax).
        ef3_flash_addr(self.s.slot, self.s.bank, address >= 0xa000, address & 0x1fff)
    }

    /// `n_reset = 0` — every reset, the cart's own included (`cart_easyflash.vhdl:
    /// 154-176`): `$DE02` is cleared and GAME follows the boot flag. Slot and bank are NOT
    /// touched here; the callers say which resets clear them.
    fn n_reset(&mut self) {
        self.s.reg02 = 0;
        self.s.ctrl_exrom = false;
        self.s.ctrl_game = self.s.boot;
        self.s.no_vicii = false;
        self.s.led = false;
    }

    /// `$DE0F` (`ef3.vhdl:690-735`).
    fn write_mode(&mut self, value: u8) {
        let n = value & 0x0f;
        self.s.enable_ef = false;
        self.s.enable_menu = false;
        self.s.last_mode = n;
        self.s.not_emulated = false;
        match n {
            0 => {
                self.s.enable_ef = true;
                self.s.reset_request = true;
            }
            1 => self.s.enable_ef = true,
            7 => self.s.reset_request = true,
            // Out of scope: the cartridge acts as mode 7 and says so.
            2 | 4 | 5 | 6 => {
                self.s.reset_request = true;
                self.s.not_emulated = true;
            }
            // `when others => null`: everything disabled, no reset.
            _ => {}
        }
    }

    fn notice(&self) -> Option<String> {
        self.s.not_emulated.then(|| {
            format!(
                "EF3 mode {} ({}) is not emulated — the cartridge is off",
                self.s.last_mode,
                mode_name(self.s.last_mode)
            )
        })
    }
}

fn header_of(image: &ParsedCartridgeImage) -> Vec<u8> {
    let raw = &image.raw_bytes;
    if crate::cart::is_crt(raw) && raw.len() >= 0x40 {
        let len = u32::from_be_bytes([raw[0x10], raw[0x11], raw[0x12], raw[0x13]]) as usize;
        if (0x40..=raw.len()).contains(&len) {
            return raw[..len].to_vec();
        }
    }
    // A raw .bin has no header: write the one a type-90 image has.
    let mut h = Vec::with_capacity(0x40);
    h.extend_from_slice(b"C64 CARTRIDGE   ");
    h.extend_from_slice(&0x40u32.to_be_bytes());
    h.extend_from_slice(&0x0100u16.to_be_bytes());
    h.extend_from_slice(&90u16.to_be_bytes());
    h.push(1); // EXROM
    h.push(0); // GAME
    h.extend_from_slice(&[0u8; 6]);
    let mut name = [0u8; 32];
    let nb = image.name.as_bytes();
    let n = nb.len().min(31);
    name[..n].copy_from_slice(&nb[..n]);
    h.extend_from_slice(&name);
    h
}

impl CartMapper for Ef3Mapper {
    fn mapper_type(&self) -> MapperType {
        MapperType::EasyFlash3
    }

    /// `cart_easyflash.vhdl` `set_game_exrom`, merged with the reset generator's `go_64`
    /// pull on GAME (`ef3.vhdl` `n_game_out`). The no-VIC bit is the VIC half-cycle: these
    /// are the lines the CPU's PHI2 half sees.
    fn get_lines(&self) -> CartLines {
        let (mut exrom, mut game) = (1u8, 1u8);
        if self.s.enable_ef {
            exrom = if self.s.ctrl_exrom { 0 } else { 1 };
            game = if self.s.ctrl_game { 0 } else { 1 };
        }
        if self.s.go64_pull {
            game = 0;
        }
        CartLines { exrom, game }
    }

    fn read(&mut self, address: u16, _bank_info: &BankInfo, clk: u64) -> Option<u8> {
        if !self.s.enable_ef {
            return None;
        }
        match address {
            0xde00..=0xde0f => match address & 0xf {
                1 => Some(self.s.slot),
                8 => Some(EF3_VERSION),
                // USB: no data (RXF high), not ready (TXE high). `$DE0A` is the data port.
                9 | 0xa => Some(0x00),
                _ => None,
            },
            0xdf00..=0xdfff => Some(self.io_ram[(address & 0xff) as usize]),
            0x8000..=0x9fff | 0xa000..=0xbfff | 0xe000..=0xffff => {
                let off = self.flash_offset(address) as u32;
                Some(self.flash.read(off, clk))
            }
            _ => None,
        }
    }

    fn peek(&self, address: u16, _bank_info: &BankInfo) -> Option<u8> {
        if !self.s.enable_ef {
            return None;
        }
        match address {
            0xde00..=0xde0f => match address & 0xf {
                0 => Some(self.s.bank),
                1 => Some(self.s.slot),
                2 => Some(self.s.reg02),
                8 => Some(EF3_VERSION),
                9 | 0xa => Some(0x00),
                _ => None,
            },
            0xdf00..=0xdfff => Some(self.io_ram[(address & 0xff) as usize]),
            0x8000..=0x9fff | 0xa000..=0xbfff | 0xe000..=0xffff => {
                Some(self.flash.peek(self.flash_offset(address) as u32))
            }
            _ => None,
        }
    }

    fn write(&mut self, address: u16, value: u8, _bank_info: &BankInfo, clk: u64) -> bool {
        match address {
            // Only `$DE00-$DE0F` is decoded: no mirrors in `$DE10-$DEFF`.
            0xde00..=0xde0f => {
                let r = address & 0xf;
                let mut consumed = false;
                if self.s.enable_ef {
                    consumed = true;
                    match r {
                        0 => self.s.bank = value & 0x3f,
                        1 => self.s.slot = value & 7,
                        2 => {
                            self.s.ctrl_exrom = value & 2 != 0;
                            self.s.ctrl_game = if value & 4 == 0 { self.s.boot } else { value & 1 != 0 };
                            self.s.no_vicii = value & 8 != 0;
                            self.s.led = value & 0x80 != 0;
                            self.s.reg02 = value & 0x8f;
                        }
                        _ => {}
                    }
                }
                if r == 0xf && self.s.enable_menu {
                    self.write_mode(value);
                    consumed = true;
                }
                consumed
            }
            0xdf00..=0xdfff => {
                if self.s.enable_ef {
                    self.io_ram[(address & 0xff) as usize] = value;
                    true
                } else {
                    false
                }
            }
            // The flash is written only in EF mode and only in a window the PLA asserts for
            // a write: ROML at $8000 and ROMH at $E000, both Ultimax only.
            0x8000..=0x9fff | 0xe000..=0xffff => {
                if !self.s.enable_ef {
                    return false;
                }
                let l = self.get_lines();
                if l.exrom == 1 && l.game == 0 {
                    let off = self.flash_offset(address) as u32;
                    self.flash.store(off, value, clk);
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    /// The expansion-port RESET line, from the C64 (power-on, the reset button): the mode
    /// enables, slot, bank and boot flag all return to their power-on values.
    fn reset(&mut self) {
        let buttons = self.s.buttons_enabled;
        self.s = Ef3State::power_on();
        self.s.buttons_enabled = buttons;
        self.n_reset();
    }

    /// A reset the cartridge generated (`$DE0F` 0/7, the buttons): it is not an external
    /// reset, so mode, `$DE0F` enable, slot, bank and boot flag are kept; only `$DE02` is
    /// cleared. GAME is pulled low until the first ROMH access.
    fn reset_generated(&mut self) {
        self.n_reset();
        self.s.go64_pull = true;
    }

    fn reset_vector_fetched(&mut self) {
        self.s.go64_pull = false;
    }

    fn take_reset_request(&mut self) -> bool {
        std::mem::take(&mut self.s.reset_request)
    }

    fn buttons(&self) -> &'static [&'static str] {
        &["menu", "reset", "special"]
    }

    /// The three buttons (`ef3.vhdl:384-403`, `cart_easyflash.vhdl:107-122`). A press is a
    /// down-and-up inside this call; it raises a generated reset request for the machine.
    fn press_button(&mut self, button: &str) -> Result<(), String> {
        if !self.s.buttons_enabled {
            return Err("buttons are not enabled: all three must have been released once since power-on".into());
        }
        match button {
            // Always enters the menu: EF mode, `$DE0F` back, slot 0, bank 0, boot flag.
            "menu" => {
                self.s.enable_ef = true;
                self.s.enable_menu = true;
                self.s.slot = 0;
                self.s.bank = 0;
                self.s.boot = true;
                self.s.last_mode = 0xff;
                self.s.not_emulated = false;
                self.s.reset_request = true;
            }
            // The EasyFlash mode's own buttons act only while that mode is enabled
            // (`if enable = '1'`); in the kill state they do nothing at all.
            "reset" | "special" => {
                if self.s.enable_ef {
                    self.s.boot = button == "reset";
                    self.s.bank = 0;
                    self.s.reset_request = true;
                }
            }
            other => return Err(format!("unknown button '{other}' (menu, reset, special)")),
        }
        Ok(())
    }

    fn ef3_status(&self) -> Option<Ef3Status> {
        Some(Ef3Status {
            state: if self.s.enable_ef { "ef" } else { "off" },
            slot: self.s.slot,
            bank: self.s.bank,
            boot: self.s.boot,
            menu_enabled: self.s.enable_menu,
            buttons_enabled: self.s.buttons_enabled,
            led: self.s.led,
            no_vicii: self.s.no_vicii,
            last_mode: (self.s.last_mode != 0xff).then(|| (self.s.last_mode, mode_name(self.s.last_mode))),
            not_emulated: self.s.not_emulated,
            notice: self.notice(),
        })
    }

    /// What the VIC sees through ROMH under Ultimax. In the VIC's half-cycle the no-VIC bit
    /// takes the cartridge off the bus, so it shows nothing then.
    fn vic_romh(&self) -> Option<&[u8]> {
        if !self.s.enable_ef || self.s.no_vicii {
            return None;
        }
        let off = ef3_flash_addr(self.s.slot, self.s.bank, true, 0);
        self.flash.data.get(off..off + 0x2000)
    }

    /// `slot*64 + bank`, the CHIP packet's bank number.
    fn active_bank(&self, _addr: u16) -> u16 {
        ((self.s.slot as u16) << 6) | self.s.bank as u16
    }

    fn get_state(&self) -> CartState {
        CartState {
            current_bank: self.s.bank as u16,
            control_register: Some(self.s.reg02),
            flash: Some(FlashCartState {
                flash_lo: Some(self.flash.snapshot_state_ro()),
                flash_hi: None,
                eeprom: None,
                spi: None,
                easyflash_jumper: 0,
                easyflash_ram: self.io_ram.to_vec(),
                ef3: Some(self.s.clone()),
            }),
        }
    }

    fn set_state(&mut self, state: CartState) {
        self.s.bank = (state.current_bank & 0x3f) as u8;
        if let Some(f) = &state.flash {
            if f.easyflash_ram.len() >= 256 {
                self.io_ram.copy_from_slice(&f.easyflash_ram[..256]);
            }
            if let Some(s) = &f.flash_lo {
                self.flash.restore_state(s);
            }
            if let Some(e) = &f.ef3 {
                self.s = e.clone();
                self.s.slot &= 7;
                self.s.bank &= 0x3f;
            }
        }
    }

    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }

    fn is_writable_dirty(&self) -> bool {
        self.flash.is_dirty()
    }
    fn persists_writable_state(&self) -> bool {
        true
    }
    fn writable_generation(&self) -> u64 {
        self.flash.writable_generation()
    }
    /// The 8 MB chip in chip order (the address formula above), not in CRT order.
    fn writable_image(&mut self, clk: u64) -> Option<Vec<u8>> {
        Some(self.flash.get_data(clk).to_vec())
    }
    fn set_writable_image(&mut self, bytes: &[u8]) {
        self.flash.load_data(bytes);
    }

    /// The live flash as a type-90 CRT in the CHIP layout: slot-major, `slot*64 + bank`,
    /// 8 KB per packet, ROML at `$8000`, ROMH at `$A000` (or `$E000` where the image loaded
    /// it there). An erased 8 KB half is not written; a loader fills what is missing with
    /// `$FF`, as erased flash is. Unlike the EasyFlash's repack this is built from the flash,
    /// not patched into the old packets, so a slot written from scratch (EasyProg onto a slot
    /// the image had no packets for) is saved too.
    fn crt_image(&mut self, clk: u64) -> Option<Vec<u8>> {
        let header = &self.header;
        let e000 = &self.romh_e000;
        let data = self.flash.get_data(clk);
        let mut out = header.clone();
        for key in 0..EF3_BANKS {
            let (slot, b) = ((key >> 6) as u8, (key & 63) as u8);
            for romh in [false, true] {
                let a = ef3_flash_addr(slot, b, romh, 0);
                let half = &data[a..a + 0x2000];
                if half.iter().all(|&x| x == 0xff) {
                    continue;
                }
                let load: u16 = if !romh {
                    0x8000
                } else if e000.contains(&key) {
                    0xe000
                } else {
                    0xa000
                };
                out.extend_from_slice(b"CHIP");
                out.extend_from_slice(&(0x10u32 + 0x2000).to_be_bytes());
                out.extend_from_slice(&2u16.to_be_bytes()); // chip type: flash
                out.extend_from_slice(&key.to_be_bytes());
                out.extend_from_slice(&load.to_be_bytes());
                out.extend_from_slice(&0x2000u16.to_be_bytes());
                out.extend_from_slice(half);
            }
        }
        Some(out)
    }

    /// `bank` is `slot*64 + bank` here, as in a CHIP packet.
    fn overlay_bank_write(&mut self, space: &str, bank: u16, addr: u16, byte: u8) -> Result<(), String> {
        let off = overlay_offset(space, bank, addr)?;
        self.flash.data[off] = byte;
        Ok(())
    }
    fn overlay_bank_read(&self, space: &str, bank: u16, addr: u16) -> Result<u8, String> {
        let off = overlay_offset(space, bank, addr)?;
        Ok(self.flash.data[off])
    }
}

fn overlay_offset(space: &str, bank: u16, addr: u16) -> Result<usize, String> {
    if bank >= EF3_BANKS {
        return Err(format!("overlay bank {bank} outside 0..{} (slot*64 + bank)", EF3_BANKS - 1));
    }
    let romh = match (space, addr) {
        ("roml", 0x8000..=0x9fff) => false,
        ("romh", 0xa000..=0xbfff | 0xe000..=0xffff) => true,
        ("roml", _) => return Err(format!("overlay roml addr ${addr:04X} outside $8000-$9FFF")),
        ("romh", _) => return Err(format!("overlay romh addr ${addr:04X} outside $A000-$BFFF/$E000-$FFFF")),
        (other, _) => return Err(format!("unknown cart overlay space {other:?} (want roml|romh)")),
    };
    Ok(ef3_flash_addr((bank >> 6) as u8, (bank & 63) as u8, romh, addr & 0x1fff))
}
