//! Action Replay 4.2 / V5 / V6 (CRT hardware type 1) — a port of VICE's
//! `c64/cart/actionreplay.c`, function by function, plus the part of `c64cartmem.c` /
//! `c64cart.c` / `c64carthooks.c` that decides what the cartridge does to the machine.
//!
//! 32 KiB ROM in four 8 KiB banks, 8 KiB RAM, a freeze button. One register at
//! `$DE00-$DEFF` (written; a read rewrites it, see [`ActionReplayMapper::io1_read`]):
//!
//! ```text
//!   7  extra ROM bank selector (A15), unused
//!   6  1 = reset FREEZE mode (release /NMI)
//!   5  1 = RAM at ROML ($8000-$9FFF) and IO2 ($DF00-$DFFF = $9F00-$9FFF)
//!   4  ROM bank selector high (A14)
//!   3  ROM bank selector low (A13)
//!   2  1 = disable the cartridge (kill: $DE00 stops answering)
//!   1  1 = /EXROM high
//!   0  1 = /GAME low
//! ```
//!
//! `value & 3` is the cartridge mode as VICE numbers them (`CMODE_*`): 0 = 8K game,
//! 1 = 16K game, 2 = off, 3 = ultimax. Mode `$22` (RAM on, 8K, EXROM high) is broken in the
//! original and reads C64 RAM OR cartridge RAM at ROML.
//!
//! The freeze: the button's press, `cart_freeze_allowed()` (always for this cartridge), /NMI
//! asserted, and when the CPU takes it (after the pushes, at the `$FFFA` fetch) the cartridge's own `freeze()` — ultimax with RAM at
//! ROML, bank 0, `active` set — so the NMI vector at `$FFFA` comes out of the ROM. /NMI is
//! released by a register write with bit 6.

use std::sync::Arc;

use crate::cart::{
    BankInfo, CartLines, CartMapper, CartState, CrtError, FreezeSched, FreezerCartState,
    MapperType, ParsedCartridgeImage,
};

const ROM_SIZE: usize = 0x8000;
const RAM_SIZE: usize = 0x2000;

/// `CMODE_8KGAME` .. `CMODE_ULTIMAX` (`c64cartsystem.h`).
const CMODE_8KGAME: u8 = 0;
const CMODE_ULTIMAX: u8 = 3;

/// `ram_init_with_pattern(export_ram0, CART_RAM_SIZE, &ramparam)` with the cartridge's
/// `ramparam` (`actionreplay.c`): start value 255, inverted every 2 bytes from offset 1,
/// the pattern inverted every `$100` bytes, no random bits.
fn power_up_ram() -> Vec<u8> {
    (0..RAM_SIZE)
        .map(|offset| {
            let j: u8 = if offset.div_ceil(2) & 1 != 0 { 0xff } else { 0x00 };
            let k: u8 = if (offset / 0x100) & 1 != 0 { 0xff } else { 0x00 };
            0xff ^ j ^ k
        })
        .collect()
}

#[derive(Clone)]
pub struct ActionReplayMapper {
    /// `roml_banks` and `romh_banks`: the same 32 KiB (`actionreplay_config_setup`).
    rom: Arc<[u8]>,
    /// `export_ram0`.
    ram: Vec<u8>,
    /// `ar_active`.
    active: bool,
    /// `regvalue`.
    regvalue: u8,
    /// `mode_phi2 & 3` of the last `cart_config_changed_slotmain`.
    mode: u8,
    /// `roml_bank` / `romh_bank`.
    bank: u8,
    /// `export_ram`.
    export_ram: bool,
    /// `cartridge_int_num` is asserted.
    nmi: bool,
    sched: FreezeSched,
    /// A READ changed the configuration; the bus re-runs the PLA (a store is told so by
    /// being consumed).
    config_dirty: bool,
}

impl ActionReplayMapper {
    /// `actionreplay_crt_attach`: four CHIP packets of 8 KiB, banks 0..3. A CRT with any
    /// other layout is refused, as VICE refuses it.
    pub fn new(image: &ParsedCartridgeImage) -> Result<Self, CrtError> {
        let mut rom = vec![0xffu8; ROM_SIZE];
        if let Some((&bank, _)) = image.banks.iter().find(|(&b, _)| b > 3) {
            return Err(CrtError::BankOutOfRange { bank, limit: 4 });
        }
        for bank in 0..4u16 {
            let chip = image.banks.get(&bank).ok_or_else(|| {
                CrtError::BadLayout(format!("Action Replay: bank {bank} is missing (four 8 KiB banks at $8000 are needed)"))
            })?;
            let (Some(roml), None, None) = (chip.roml.as_ref(), chip.romh_a000.as_ref(), chip.romh_e000.as_ref()) else {
                return Err(CrtError::BadLayout(format!(
                    "Action Replay: bank {bank} is not a single 8 KiB chip at $8000"
                )));
            };
            rom[bank as usize * 0x2000..(bank as usize + 1) * 0x2000].copy_from_slice(roml);
        }
        let mut m = ActionReplayMapper {
            rom: rom.into(),
            ram: power_up_ram(),
            active: true,
            regvalue: 0,
            mode: CMODE_8KGAME,
            bank: 0,
            export_ram: false,
            nmi: false,
            sched: FreezeSched::default(),
            config_dirty: false,
        };
        m.config_init();
        Ok(m)
    }

    /// `cart_config_changed_slotmain(mode, mode | bank << 2, wflag)`: the lines, the bank,
    /// RAM-or-ROM, and the release of /NMI when asked. The PLA re-run is the bus's.
    fn config_changed(&mut self, mode: u8, bank: u8, export_ram: bool, release_freeze: bool) {
        self.mode = mode & 3;
        self.bank = bank & 3;
        self.export_ram = export_ram;
        if release_freeze {
            self.nmi = false;
        }
    }

    /// `actionreplay_config_init`.
    fn config_init(&mut self) {
        self.active = true;
        self.regvalue = 0;
        self.config_changed(CMODE_8KGAME, 0, false, false);
    }

    /// `actionreplay_io1_store`.
    fn io1_store(&mut self, value: u8) {
        if !self.active {
            return;
        }
        self.regvalue = value;
        let release = value & 0x40 != 0;
        let export_ram = value & 0x20 != 0;
        if value & 4 != 0 {
            self.active = false;
        }
        let bank = (value >> 3) & 3;
        // Mode $22 is broken in the original AR: the lines are those of an 8K game.
        if (value & 0x23) == 0x22 {
            self.config_changed(CMODE_8KGAME, bank, export_ram, release);
        } else {
            self.config_changed(value & 3, bank, export_ram, release);
        }
    }

    /// `actionreplay_io1_read`: the R/W line is not decoded, so a read writes the register
    /// with whatever was on the bus before — the VIC's last phi1 byte. Returns that byte.
    fn io1_read(&mut self, phi1: u8) -> Option<u8> {
        if !self.active {
            return None;
        }
        self.io1_store(phi1);
        self.config_dirty = true;
        Some(phi1)
    }

    /// `actionreplay_io2_read` / `_peek`: the top page of RAM, or of the ROM bank.
    fn io2_byte(&self, addr: u16) -> u8 {
        if self.export_ram {
            return self.ram[0x1f00 + (addr as usize & 0xff)];
        }
        self.rom[(0xdf00usize & 0x1fff) + (addr as usize & 0xff) + self.bank as usize * 0x2000]
    }

    /// `actionreplay_roml_read`.
    fn roml_byte(&self, addr: u16, ram_under: u8) -> u8 {
        if (self.regvalue & 0x23) == 0x22 {
            // Mode $22: both the C64 RAM and the cartridge RAM drive the bus.
            return ram_under | self.ram[addr as usize & 0x1fff];
        }
        if self.export_ram {
            return self.ram[addr as usize & 0x1fff];
        }
        self.rom[(addr as usize & 0x1fff) + self.bank as usize * 0x2000]
    }

    /// `generic_romh_read`: the same bank at `$A000` (16K game) and `$E000` (ultimax).
    fn romh_byte(&self, addr: u16) -> u8 {
        self.rom[(addr as usize & 0x1fff) + self.bank as usize * 0x2000]
    }

    /// Whether the cartridge is in ultimax (its ROML store goes to cartridge RAM only).
    fn ultimax(&self) -> bool {
        self.mode == CMODE_ULTIMAX
    }

    /// The cartridge's RAM, for the host (a test, a monitor lens).
    pub fn ram(&self) -> &[u8] {
        &self.ram
    }

    /// `ar_active`, `regvalue`.
    pub fn register(&self) -> (bool, u8) {
        (self.active, self.regvalue)
    }
}

impl CartMapper for ActionReplayMapper {
    fn mapper_type(&self) -> MapperType {
        MapperType::ActionReplay
    }

    /// `export_slotmain.game = mode & 1` (asserted), `.exrom = ((mode >> 1) & 1) ^ 1`
    /// (asserted); here as the pin levels the PLA indexes with, 0 = low.
    fn get_lines(&self) -> CartLines {
        CartLines { exrom: (self.mode >> 1) & 1, game: (self.mode & 1) ^ 1 }
    }

    fn read(&mut self, address: u16, bank_info: &BankInfo, _clk: u64) -> Option<u8> {
        match address {
            0x8000..=0x9fff => Some(self.roml_byte(address, bank_info.ram_under)),
            0xa000..=0xbfff | 0xe000..=0xffff => Some(self.romh_byte(address)),
            0xde00..=0xdeff => self.io1_read(bank_info.phi1),
            0xdf00..=0xdfff => self.active.then(|| self.io2_byte(address)),
            _ => None,
        }
    }

    fn peek(&self, address: u16, bank_info: &BankInfo) -> Option<u8> {
        match address {
            0x8000..=0x9fff => Some(self.roml_byte(address, bank_info.ram_under)),
            0xa000..=0xbfff | 0xe000..=0xffff => Some(self.romh_byte(address)),
            0xde00..=0xdeff => Some(self.regvalue),
            0xdf00..=0xdfff => Some(if self.active { self.io2_byte(address) } else { 0 }),
            _ => None,
        }
    }

    fn write(&mut self, address: u16, value: u8, _bank_info: &BankInfo, _clk: u64) -> bool {
        match address {
            0xde00..=0xdeff => {
                let was_active = self.active;
                self.io1_store(value);
                was_active
            }
            0xdf00..=0xdfff => {
                if self.active && self.export_ram {
                    self.ram[0x1f00 + (address as usize & 0xff)] = value;
                    return true;
                }
                false
            }
            // `roml_store` (ultimax): the cartridge RAM takes it, and nothing else does —
            // there is no C64 RAM at $8000 under ultimax.
            0x8000..=0x9fff if self.ultimax() => {
                if self.export_ram {
                    self.ram[address as usize & 0x1fff] = value;
                }
                true
            }
            _ => false,
        }
    }

    /// `actionreplay_reset` and the `actionreplay_config_init` the reset's
    /// `mem_initialize_memory` runs: active, register 0, 8K game, bank 0; the alarms and
    /// /NMI gone. The RAM is not touched — a reset is not a power cycle.
    fn reset(&mut self) {
        self.sched = FreezeSched::default();
        self.nmi = false;
        self.config_init();
    }

    fn power_up(&mut self) {
        self.ram = power_up_ram();
        self.reset();
    }

    fn active_bank(&self, _addr: u16) -> u16 {
        self.bank as u16
    }

    /// Under ultimax the VIC fetches `$3000-$3FFF` of its bank out of ROMH
    /// (`ultimax_memptr_update`).
    fn vic_romh(&self) -> Option<&[u8]> {
        let b = self.bank as usize * 0x2000;
        Some(&self.rom[b..b + 0x2000])
    }

    fn get_state(&self) -> CartState {
        CartState {
            current_bank: self.bank as u16,
            control_register: Some(self.regvalue),
            flash: None,
            freezer: Some(FreezerCartState {
                active: self.active,
                regvalue: self.regvalue,
                mode: self.mode,
                bank: self.bank,
                export_ram: self.export_ram,
                nmi: self.nmi,
                freeze_at: self.sched.freeze_at,
                nmi_at: self.sched.nmi_at,
                ram: self.ram.clone(),
            }),
        }
    }

    fn set_state(&mut self, state: CartState) {
        match state.freezer {
            Some(f) => {
                self.active = f.active;
                self.regvalue = f.regvalue;
                self.mode = f.mode & 3;
                self.bank = f.bank & 3;
                self.export_ram = f.export_ram;
                self.nmi = f.nmi;
                self.sched = FreezeSched { freeze_at: f.freeze_at, nmi_at: f.nmi_at };
                if f.ram.len() == RAM_SIZE {
                    self.ram.copy_from_slice(&f.ram);
                }
            }
            // A state without the freezer part (another cartridge's, or a checkpoint
            // that predates it): the register and bank are all there is.
            None => {
                self.bank = (state.current_bank & 3) as u8;
                self.regvalue = state.control_register.unwrap_or(0);
            }
        }
    }

    fn clone_box(&self) -> Box<dyn CartMapper> {
        Box::new(self.clone())
    }

    fn buttons(&self) -> &'static [&'static str] {
        &["freeze"]
    }

    fn press_button(&mut self, button: &str) -> Result<(), String> {
        Err(format!("the {button} button needs the machine's cycle: press it through the machine"))
    }

    /// `cartridge_trigger_freeze` — without the random delay of up to one frame VICE puts
    /// in front of it: the freeze takes effect on the cycle of the press, so the same press
    /// is the same run.
    fn press_button_at(&mut self, button: &str, clk: u64) -> Result<(), String> {
        match button {
            "freeze" => {
                self.sched.freeze_at = Some(clk);
                Ok(())
            }
            other => Err(format!("unknown button '{other}' (freeze)")),
        }
    }

    fn is_freezer(&self) -> bool {
        true
    }

    fn nmi_line(&self) -> bool {
        self.nmi
    }

    fn drive_nmi(&mut self, on: bool) {
        self.nmi = on;
    }

    /// `cart_freeze_allowed()`: yes for every Action Replay, killed or not.
    fn freeze_allowed(&self) -> bool {
        true
    }

    /// `actionreplay_freeze`.
    fn freeze(&mut self) {
        self.active = true;
        self.config_changed(CMODE_ULTIMAX, 0, true, false);
    }

    fn freeze_sched(&mut self) -> Option<&mut FreezeSched> {
        Some(&mut self.sched)
    }

    fn take_config_changed(&mut self) -> bool {
        std::mem::take(&mut self.config_dirty)
    }

    /// `roml_no_ultimax_store` / `raml_no_ultimax_store` → `actionreplay_roml_store`:
    /// in 8K and 16K game the cartridge RAM takes the store as well as the C64's.
    fn write_under_roml(&mut self, address: u16, value: u8) {
        if self.mode <= 1 && self.export_ram {
            self.ram[address as usize & 0x1fff] = value;
        }
    }

    /// A ROML read served out of RAM (RAM on, or mode $22) is not ROM payload; the IO2
    /// window's own page is not a ROM window at all.
    fn read_is_rom(&self, address: u16) -> bool {
        !(matches!(address, 0x8000..=0x9fff) && (self.export_ram || (self.regvalue & 0x23) == 0x22))
    }
}
