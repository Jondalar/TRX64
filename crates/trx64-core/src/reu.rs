//! Spec 853 — the 17xx RAM Expansion Unit.
//!
//! 1:1 PORT of `vice/src/c64/cart/reu.c`. Where VICE and the Ultimate's VHDL differ
//! (mirroring below 512 KB, `$DF20-$DFFF`, the end state after a verify error), VICE
//! wins — it is the implementation measured against real hardware for twenty years and
//! the Ultimate's core is closed.
//!
//! This is the first device on the port that DRIVES the bus instead of answering it.
//! Spec 850 built the hold and gave the device no bus; [`DmaBus`] is that bus, and
//! `Machine` implements it over the same `FullBus` the CPU executes through.
//!
//! The transfer does NOT run inside [`ExpansionDevice::write`] — a device has no bus
//! there. A write that starts one only arms it (`dma_pending`), exactly as VICE arms
//! `MAINCPU_BA_LOW_REU` and lets `maincpu_steal_cycles()` call `reu_dma_start()`
//! (`mainc64cpu.c:122-125`).

use crate::expansion::{Access, AccessKind, ExpansionDevice, PortLines};

// ── registers (reu.c:129-141) ──────────────────────────────────────────────────────

const REG_STATUS: u16 = 0x00;
const REG_COMMAND: u16 = 0x01;
const REG_BASEADDR_LOW: u16 = 0x02;
const REG_BASEADDR_HIGH: u16 = 0x03;
const REG_RAMADDR_LOW: u16 = 0x04;
const REG_RAMADDR_HIGH: u16 = 0x05;
const REG_BANK: u16 = 0x06;
const REG_BLOCKLEN_LOW: u16 = 0x07;
const REG_BLOCKLEN_HIGH: u16 = 0x08;
const REG_INTERRUPT: u16 = 0x09;
const REG_ADDR_CONTROL: u16 = 0x0A;
/// The highest address the used registers occupy (`REU_REG_FIRST_UNUSED`).
const REG_FIRST_UNUSED: u16 = 0x0B;

/// status (reu.c:146-150)
const STATUS_256K_CHIPS: u8 = 0x10;
const STATUS_VERIFY_ERROR: u8 = 0x20;
const STATUS_END_OF_BLOCK: u8 = 0x40;
const STATUS_INTERRUPT_PENDING: u8 = 0x80;

/// command (reu.c:155-163)
const CMD_TRANSFER_TYPE_MASK: u8 = 0x03;
const CMD_TYPE_TO_REU: u8 = 0x00;
const CMD_TYPE_FROM_REU: u8 = 0x01;
const CMD_TYPE_SWAP: u8 = 0x02;
const CMD_TYPE_VERIFY: u8 = 0x03;
const CMD_FF00_TRIGGER_DISABLED: u8 = 0x10;
const CMD_AUTOLOAD: u8 = 0x20;
const CMD_EXECUTE: u8 = 0x80;

/// bank (reu.c:168)
const BANK_UNUSED: u8 = 0xF8;

/// interrupt mask (reu.c:171-177)
const INT_UNUSED_MASK: u8 = 0x1F;
const INT_VERIFY_ENABLED: u8 = 0x20;
const INT_END_OF_BLOCK_ENABLED: u8 = 0x40;
const INT_INTERRUPTS_ENABLED: u8 = 0x80;

/// address control (reu.c:180-184)
const ADDR_CONTROL_UNUSED_MASK: u8 = 0x3F;
const ADDR_CONTROL_FIX_REC: u8 = 0x40;
const ADDR_CONTROL_FIX_C64: u8 = 0x80;

/// The `$FF00` trigger (reu.c:192-195; `mainc64cpu.c:290-303`).
const FF00: u16 = 0xFF00;

// ── the bus a transfer drives ──────────────────────────────────────────────────────

/// What a DMA device needs from the machine: C64 memory, and cycles.
///
/// The three clock calls are VICE's `reu_ba` callbacks (`reu.c:590-599`) rather than one
/// "advance N cycles", because a transfer's cost is not a number it can compute: the VIC
/// steals underneath it, and how much depends on where in the frame the transfer landed.
pub trait DmaBus {
    /// A DMA read of C64 memory. A real bus cycle, not a host peek.
    fn dma_read(&mut self, addr: u16) -> u8;
    /// A DMA write to C64 memory.
    fn dma_write(&mut self, addr: u16, value: u8);
    /// `maincpu_clk++` — one cycle for this byte.
    fn clk_inc(&mut self);
    /// VICE `reu_ba.check()` — is the VIC holding BA low right now?
    fn ba_low(&mut self) -> bool;
    /// VICE `reu_ba.steal()` — run out the VIC's steal.
    fn steal(&mut self);
}

/// Per-model behaviour (`rec_options_s`, set in `set_reu_size`, reu.c:387-432).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecOptions {
    /// Where the REC chip wraps — always 512K, except the 1700.
    wrap_around: u32,
    /// Where the DRAM address space wraps.
    dram_wrap_around: u32,
    /// From here up to the wrap there is no DRAM at all.
    not_backedup_addresses: u32,
    /// Mask applied when the result goes back into `base_reu`/`bank_reu`.
    wrap_around_mask_when_storing: u32,
    /// Bits of the bank register stuck at 1.
    reg_bank_unused: u8,
    /// Preset for the status register: `STATUS_256K_CHIPS`, or 0 for the 1700.
    status_preset: u8,
}

impl RecOptions {
    /// reu.c:387-432, verbatim. `size_kb` is 128 (1700), 256 (1764), 512 (1750), or one
    /// of the oversized "hacked REU" sizes — of which only the CMD 1750XL ever existed,
    /// and which the 1541U and the Chameleon reproduce, wraparound bug included.
    fn for_size(size_kb: u32) -> Option<Self> {
        let mut o = RecOptions {
            wrap_around: 0x80000,
            dram_wrap_around: 0x80000,
            not_backedup_addresses: size_kb << 10,
            wrap_around_mask_when_storing: 0x80000 - 1,
            reg_bank_unused: BANK_UNUSED,
            status_preset: STATUS_256K_CHIPS,
        };
        match size_kb {
            128 => {
                // Not 256K chips but 64K ones, and a wrap of its own.
                o.status_preset = 0;
                o.wrap_around = 0x20000;
                o.dram_wrap_around = 0x20000;
            }
            256 | 512 => {}
            1024 | 2048 | 4096 | 8192 | 16384 => {
                // The upper bank bits are a latch wired straight to the DRAM's high
                // address lines; the REC chip never sees them. So `wrap_around` STAYS at
                // 512K — that is the wraparound bug, not an oversight.
                o.reg_bank_unused = 0;
                o.dram_wrap_around = size_kb * 1024;
                o.wrap_around_mask_when_storing = size_kb * 1024 - 1;
            }
            _ => return None,
        }
        Some(o)
    }
}

/// The REC register file (`rec_s`, reu.c:189-215), shadow registers included — they are
/// the "Half-Autoload-Bug" and they are part of the contract, not an implementation
/// detail.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Rec {
    status: u8,
    command: u8,
    base_computer: u16,
    base_reu: u16,
    bank_reu: u8,
    transfer_length: u16,
    int_mask_reg: u8,
    address_control_reg: u8,
    base_computer_shadow: u16,
    base_reu_shadow: u16,
    bank_reu_shadow: u8,
    transfer_length_shadow: u16,
}

/// What a pending transfer is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    /// `EXECUTE` written with the `$FF00` trigger disabled: run at the next opportunity.
    Immediate,
    /// `EXECUTE` written with the trigger enabled: armed, waiting for a `$FF00` write.
    ArmedForFf00,
}

/// The 17xx RAM Expansion Unit.
pub struct Reu {
    rec: Rec,
    opt: RecOptions,
    size_kb: u32,
    ram: Vec<u8>,
    /// The latch that drives the bus for addresses with no DRAM behind them
    /// (`floating_bus_value`, reu.c:261).
    floating_bus: u8,
    /// The IRQ line into `INT_SRC_EXPANSION`.
    irq: bool,
    pending: Option<Pending>,
    /// True while a transfer runs: registers read 0 and writes are ignored
    /// (`reu_dma_active`, reu.c:228).
    dma_active: bool,
    /// `reu_ba.delay` / `reu_ba.last_cycle` (reu.c:248-252) — the write path accumulates
    /// before it steals, the read path steals at once.
    ba_delay: u32,
    ba_last_cycle: bool,
}

impl Clone for Reu {
    fn clone(&self) -> Self {
        Reu {
            rec: self.rec.clone(),
            opt: self.opt,
            size_kb: self.size_kb,
            ram: self.ram.clone(),
            floating_bus: self.floating_bus,
            irq: self.irq,
            pending: self.pending,
            dma_active: self.dma_active,
            ba_delay: self.ba_delay,
            ba_last_cycle: self.ba_last_cycle,
        }
    }
}

impl Reu {
    /// A new REU of `size_kb` KiB: 128 (1700), 256 (1764), 512 (1750), or 1024..16384.
    /// `None` for any other size — VICE logs "Unknown REU size" and refuses, and a size
    /// nobody built is not a thing to silently round.
    pub fn new(size_kb: u32) -> Option<Self> {
        let opt = RecOptions::for_size(size_kb)?;
        let mut reu = Reu {
            rec: Rec::default(),
            opt,
            size_kb,
            ram: vec![0; (size_kb as usize) << 10],
            floating_bus: 0xFF,
            irq: false,
            pending: None,
            dma_active: false,
            ba_delay: 0,
            ba_last_cycle: false,
        };
        reu.reset();
        Some(reu)
    }

    pub fn size_kb(&self) -> u32 {
        self.size_kb
    }

    pub fn ram(&self) -> &[u8] {
        &self.ram
    }

    pub fn ram_mut(&mut self) -> &mut [u8] {
        &mut self.ram
    }

    /// reu.c:602-615. Note what it does NOT do: the RAM is untouched, and the `$FF00`
    /// trigger comes up DISABLED — software enables it deliberately.
    pub fn reset(&mut self) {
        let ram_keeps = std::mem::take(&mut self.ram);
        self.rec = Rec::default();
        self.ram = ram_keeps;
        self.rec.status = (self.rec.status & !STATUS_256K_CHIPS) | self.opt.status_preset;
        self.rec.command = CMD_FF00_TRIGGER_DISABLED;
        self.rec.transfer_length = 0xFFFF;
        self.rec.transfer_length_shadow = 0xFFFF;
        self.rec.bank_reu = self.opt.reg_bank_unused;
        self.rec.bank_reu_shadow = self.opt.reg_bank_unused;
        self.rec.int_mask_reg = INT_UNUSED_MASK;
        self.rec.address_control_reg = ADDR_CONTROL_UNUSED_MASK;
        self.irq = false;
        self.pending = None;
        self.ba_delay = 0;
        self.ba_last_cycle = false;
    }

    // ── REU RAM, with the holes a real one has ────────────────────────────────────

    /// reu.c:1110-1119. A store above the fitted DRAM is dropped, not wrapped into it.
    fn store_to_reu(&mut self, reu_addr: u32, value: u8) {
        let a = reu_addr & (self.opt.dram_wrap_around - 1);
        if a < self.opt.not_backedup_addresses {
            self.ram[a as usize] = value;
        }
    }

    /// reu.c:1138-1152. A read with no DRAM behind it returns the bus latch.
    fn read_from_reu(&self, reu_addr: u32) -> u8 {
        let a = reu_addr & (self.opt.dram_wrap_around - 1);
        if a < self.opt.not_backedup_addresses {
            self.ram[a as usize]
        } else {
            self.floating_bus
        }
    }

    /// reu.c:1085-1094 — only the low 19 bits belong to the REC chip.
    fn increment_reu(&self, reu_addr: u32, step: u32) -> u32 {
        let mut next = (reu_addr & 0x0007_FFFF) + step;
        if next == self.opt.wrap_around {
            next = 0;
        }
        (reu_addr & 0x00F8_0000) | next
    }

    // ── the register file ─────────────────────────────────────────────────────────

    /// reu.c:868-912 — `reu_read_without_sideeffects`.
    fn read_without_sideeffects(&self, addr: u16) -> u8 {
        match addr {
            REG_STATUS => self.rec.status,
            REG_COMMAND => self.rec.command,
            REG_BASEADDR_LOW => self.rec.base_computer as u8,
            REG_BASEADDR_HIGH => (self.rec.base_computer >> 8) as u8,
            REG_RAMADDR_LOW => self.rec.base_reu as u8,
            REG_RAMADDR_HIGH => (self.rec.base_reu >> 8) as u8,
            REG_BANK => self.rec.bank_reu | self.opt.reg_bank_unused,
            REG_BLOCKLEN_LOW => self.rec.transfer_length as u8,
            REG_BLOCKLEN_HIGH => (self.rec.transfer_length >> 8) as u8,
            REG_INTERRUPT => self.rec.int_mask_reg,
            REG_ADDR_CONTROL => self.rec.address_control_reg,
            _ => 0xFF,
        }
    }

    /// reu.c:926-965 — `reu_store_without_sideeffects`. Every address write lands in the
    /// register AND its shadow: that is the Half-Autoload-Bug, and it is why an autoload
    /// restores what was last written rather than what the transfer consumed.
    fn store_without_sideeffects(&mut self, addr: u16, byte: u8) {
        match addr {
            // The status register is read only.
            REG_STATUS => {}
            REG_COMMAND => self.rec.command = byte,
            REG_BASEADDR_LOW => {
                self.rec.base_computer_shadow = (self.rec.base_computer_shadow & 0xFF00) | byte as u16;
                self.rec.base_computer = self.rec.base_computer_shadow;
            }
            REG_BASEADDR_HIGH => {
                self.rec.base_computer_shadow =
                    (self.rec.base_computer_shadow & 0xFF) | ((byte as u16) << 8);
                self.rec.base_computer = self.rec.base_computer_shadow;
            }
            REG_RAMADDR_LOW => {
                self.rec.base_reu_shadow = (self.rec.base_reu_shadow & 0xFF00) | byte as u16;
                self.rec.base_reu = self.rec.base_reu_shadow;
            }
            REG_RAMADDR_HIGH => {
                self.rec.base_reu_shadow = (self.rec.base_reu_shadow & 0xFF) | ((byte as u16) << 8);
                self.rec.base_reu = self.rec.base_reu_shadow;
            }
            REG_BANK => {
                self.rec.bank_reu_shadow = byte & !self.opt.reg_bank_unused;
                self.rec.bank_reu = self.rec.bank_reu_shadow;
            }
            REG_BLOCKLEN_LOW => {
                self.rec.transfer_length_shadow =
                    (self.rec.transfer_length_shadow & 0xFF00) | byte as u16;
                self.rec.transfer_length = self.rec.transfer_length_shadow;
            }
            REG_BLOCKLEN_HIGH => {
                self.rec.transfer_length_shadow =
                    (self.rec.transfer_length_shadow & 0xFF) | ((byte as u16) << 8);
                self.rec.transfer_length = self.rec.transfer_length_shadow;
            }
            REG_INTERRUPT => self.rec.int_mask_reg = byte | INT_UNUSED_MASK,
            REG_ADDR_CONTROL => self.rec.address_control_reg = byte | ADDR_CONTROL_UNUSED_MASK,
            _ => {}
        }
    }

    /// reu.c:979-1010 — the live read. Bits 7-5 of the status clear on read and take the
    /// pending IRQ with them, which is why `peek` must be a different door (850 D2).
    fn io_read(&mut self, addr: u16) -> Option<u8> {
        if self.dma_active {
            return Some(0);
        }
        if addr >= REG_FIRST_UNUSED {
            return Some(0xFF);
        }
        let mut v = self.read_without_sideeffects(addr);
        match addr {
            REG_STATUS => {
                self.rec.status &=
                    !(STATUS_VERIFY_ERROR | STATUS_END_OF_BLOCK | STATUS_INTERRUPT_PENDING);
                self.irq = false;
            }
            // On a modded REU the upper bank bits cannot be read back from the latch.
            REG_BANK => v |= 0xF8,
            _ => {}
        }
        Some(v)
    }

    /// reu.c:1034-1067 — the live write, and the two things that start something.
    fn io_write(&mut self, addr: u16, byte: u8) {
        if self.dma_active || addr >= REG_FIRST_UNUSED {
            return;
        }
        self.store_without_sideeffects(addr, byte);
        match addr {
            REG_COMMAND => {
                if self.rec.command & CMD_EXECUTE != 0 {
                    // Trigger disabled -> run now; trigger enabled -> only arm.
                    self.pending = if self.rec.command & CMD_FF00_TRIGGER_DISABLED != 0 {
                        Some(Pending::Immediate)
                    } else {
                        Some(Pending::ArmedForFf00)
                    };
                }
            }
            REG_INTERRUPT => {
                // Enabling an interrupt whose condition already happened raises it now.
                let m = self.rec.int_mask_reg;
                if m & (INT_END_OF_BLOCK_ENABLED | INT_INTERRUPTS_ENABLED)
                    == (INT_END_OF_BLOCK_ENABLED | INT_INTERRUPTS_ENABLED)
                    && self.rec.status & STATUS_END_OF_BLOCK != 0
                {
                    self.rec.status |= STATUS_INTERRUPT_PENDING;
                    self.irq = true;
                }
                if m & (INT_VERIFY_ENABLED | INT_INTERRUPTS_ENABLED)
                    == (INT_VERIFY_ENABLED | INT_INTERRUPTS_ENABLED)
                    && self.rec.status & STATUS_VERIFY_ERROR != 0
                {
                    self.rec.status |= STATUS_INTERRUPT_PENDING;
                    self.irq = true;
                }
            }
            _ => {}
        }
    }

    // ── the transfer ──────────────────────────────────────────────────────────────

    /// Is a transfer waiting to run? `Machine` asks this at the instruction boundary.
    pub fn dma_pending(&self) -> bool {
        self.pending == Some(Pending::Immediate)
    }

    /// VICE `reu_clk_inc_post_read` (reu.c:842-851): a cycle, then keep stealing while
    /// the VIC holds BA low.
    fn clk_inc_post_read(&mut self, bus: &mut dyn DmaBus) {
        bus.clk_inc();
        if bus.ba_low() {
            bus.steal();
        }
    }

    /// VICE `reu_clk_inc_post_write` (reu.c:824-840): the write path accumulates a delay
    /// and steals only once it has run over, which is not the same rule as the read path.
    fn clk_inc_post_write(&mut self, bus: &mut dyn DmaBus) {
        bus.clk_inc();
        if bus.ba_low() {
            self.ba_delay += 1;
        } else {
            self.ba_delay = 0;
        }
        self.ba_last_cycle = self.ba_delay > 1;
        if self.ba_last_cycle {
            bus.steal();
            self.ba_delay = 0;
        }
    }

    /// Run the armed transfer. `reu_dma_start`, reu.c:1524-1560.
    pub fn run_dma(&mut self, bus: &mut dyn DmaBus) {
        if self.pending != Some(Pending::Immediate) {
            return;
        }
        self.pending = None;

        let host_addr = self.rec.base_computer;
        let reu_addr = self.rec.base_reu as u32 | ((self.rec.bank_reu as u32) << 16);
        // A length of 0 is 64 KiB, not nothing.
        let len = if self.rec.transfer_length == 0 { 0x10000 } else { self.rec.transfer_length as u32 };
        let host_step =
            if self.rec.address_control_reg & ADDR_CONTROL_FIX_C64 != 0 { 0 } else { 1 };
        let reu_step =
            if self.rec.address_control_reg & ADDR_CONTROL_FIX_REC != 0 { 0 } else { 1 };

        self.dma_active = true;
        self.ba_delay = 0;
        self.ba_last_cycle = false;
        match self.rec.command & CMD_TRANSFER_TYPE_MASK {
            CMD_TYPE_TO_REU => self.dma_host_to_reu(bus, host_addr, reu_addr, host_step, reu_step, len),
            CMD_TYPE_FROM_REU => self.dma_reu_to_host(bus, host_addr, reu_addr, host_step, reu_step, len),
            CMD_TYPE_SWAP => self.dma_swap(bus, host_addr, reu_addr, host_step, reu_step, len),
            CMD_TYPE_VERIFY => self.dma_compare(bus, host_addr, reu_addr, host_step, reu_step, len),
            // The mask is two bits and all four values are named above.
            _ => unreachable!("transfer type is masked to two bits"),
        }
        self.dma_active = false;
        self.rec.command = (self.rec.command & !CMD_EXECUTE) | CMD_FF00_TRIGGER_DISABLED;
    }

    /// reu.c:1174-1224 — what the registers say afterwards. The `len` handed in is the
    /// already-corrected one (VICE passes `++len`).
    fn update_regs(&mut self, host_addr: u16, reu_addr: u32, len: u32, new_status: u8) {
        let reu_addr = reu_addr & self.opt.wrap_around_mask_when_storing;
        self.rec.status |= new_status;

        if self.rec.command & CMD_AUTOLOAD == 0 {
            if self.rec.address_control_reg & ADDR_CONTROL_FIX_C64 == 0 {
                self.rec.base_computer = host_addr;
            }
            if self.rec.address_control_reg & ADDR_CONTROL_FIX_REC == 0 {
                self.rec.base_reu = reu_addr as u16;
                self.rec.bank_reu = ((reu_addr >> 16) & 0xFF) as u8;
            }
            self.rec.transfer_length = (len & 0xFFFF) as u16;
        } else {
            self.rec.base_computer = self.rec.base_computer_shadow;
            self.rec.base_reu = self.rec.base_reu_shadow;
            self.rec.bank_reu = self.rec.bank_reu_shadow;
            self.rec.transfer_length = self.rec.transfer_length_shadow;
        }

        let m = self.rec.int_mask_reg;
        if new_status & STATUS_END_OF_BLOCK != 0
            && m & (INT_END_OF_BLOCK_ENABLED | INT_INTERRUPTS_ENABLED)
                == (INT_END_OF_BLOCK_ENABLED | INT_INTERRUPTS_ENABLED)
        {
            self.rec.status |= STATUS_INTERRUPT_PENDING;
            self.irq = true;
        }
        if new_status & STATUS_VERIFY_ERROR != 0
            && m & (INT_VERIFY_ENABLED | INT_INTERRUPTS_ENABLED)
                == (INT_VERIFY_ENABLED | INT_INTERRUPTS_ENABLED)
        {
            self.rec.status |= STATUS_INTERRUPT_PENDING;
            self.irq = true;
        }
    }

    /// reu.c:1243-1268 — C64 → REU (stash).
    fn dma_host_to_reu(
        &mut self,
        bus: &mut dyn DmaBus,
        mut host_addr: u16,
        mut reu_addr: u32,
        host_step: u16,
        reu_step: u32,
        mut len: u32,
    ) {
        let mut value = 0u8;
        while len != 0 {
            value = bus.dma_read(host_addr);
            self.clk_inc_post_read(bus);
            self.store_to_reu(reu_addr, value);
            host_addr = host_addr.wrapping_add(host_step);
            reu_addr = self.increment_reu(reu_addr, reu_step);
            len -= 1;
        }
        self.update_regs(host_addr, reu_addr, len + 1, STATUS_END_OF_BLOCK);
        // The last value written stays in the latch that drives the bus.
        self.floating_bus = value;
    }

    /// reu.c:1288-1320 — REU → C64 (fetch).
    fn dma_reu_to_host(
        &mut self,
        bus: &mut dyn DmaBus,
        mut host_addr: u16,
        mut reu_addr: u32,
        host_step: u16,
        reu_step: u32,
        mut len: u32,
    ) {
        while len != 0 {
            let value = self.read_from_reu(reu_addr);
            self.floating_bus = value;
            bus.dma_write(host_addr, value);
            self.clk_inc_post_write(bus);
            host_addr = host_addr.wrapping_add(host_step);
            reu_addr = self.increment_reu(reu_addr, reu_step);
            len -= 1;
        }
        // An extra cycle if the transfer ended while BA was still set.
        if self.ba_last_cycle {
            self.clk_inc_post_read(bus);
        }
        self.update_regs(host_addr, reu_addr, len + 1, STATUS_END_OF_BLOCK);
        self.floating_bus = self.read_from_reu(reu_addr);
    }

    /// reu.c:1341-1375 — swap.
    fn dma_swap(
        &mut self,
        bus: &mut dyn DmaBus,
        mut host_addr: u16,
        mut reu_addr: u32,
        host_step: u16,
        reu_step: u32,
        mut len: u32,
    ) {
        while len != 0 {
            let from_reu = self.read_from_reu(reu_addr);
            let from_c64 = bus.dma_read(host_addr);
            self.clk_inc_post_read(bus);
            self.store_to_reu(reu_addr, from_c64);
            bus.dma_write(host_addr, from_reu);
            self.clk_inc_post_write(bus);
            host_addr = host_addr.wrapping_add(host_step);
            reu_addr = self.increment_reu(reu_addr, reu_step);
            len -= 1;
        }
        if self.ba_last_cycle {
            self.clk_inc_post_read(bus);
        }
        self.update_regs(host_addr, reu_addr, len + 1, STATUS_END_OF_BLOCK);
    }

    /// reu.c:1394-1478 — verify, with all three of the 17xx's documented weirdnesses.
    /// Note what it does NOT do first: clear the verify-error and end-of-block bits. The
    /// real chip does not, so neither does this.
    fn dma_compare(
        &mut self,
        bus: &mut dyn DmaBus,
        mut host_addr: u16,
        mut reu_addr: u32,
        host_step: u16,
        reu_step: u32,
        mut len: u32,
    ) {
        let mut new_status = 0u8;
        while len != 0 {
            let from_reu = self.read_from_reu(reu_addr);
            let from_c64 = bus.dma_read(host_addr);
            self.clk_inc_post_read(bus);
            reu_addr = self.increment_reu(reu_addr, reu_step);
            host_addr = host_addr.wrapping_add(host_step);
            len -= 1;

            if from_reu != from_c64 {
                new_status |= STATUS_VERIFY_ERROR;
                // Weirdness 1: a failed verify costs one extra cycle, unless it failed
                // on the last byte of the buffer.
                if len >= 1 {
                    self.clk_inc_post_read(bus);
                }
                break;
            }
        }

        if len == 0 {
            len += 1;
            // Weirdness 2: if the LAST byte failed, end-of-block is set as well.
            new_status |= STATUS_END_OF_BLOCK;
        } else if len == 1 {
            // Weirdness 3: if the next-to-last byte failed, end-of-block is set — but
            // only if the last byte compares equal.
            let from_reu = self.read_from_reu(reu_addr);
            let from_c64 = bus.dma_read(host_addr);
            if from_reu == from_c64 {
                new_status |= STATUS_END_OF_BLOCK;
            }
        }

        self.update_regs(host_addr, reu_addr, len, new_status);
    }

    // ── what the monitor shows ────────────────────────────────────────────────────

    pub fn status(&self) -> ReuStatus {
        ReuStatus {
            size_kb: self.size_kb,
            status: self.rec.status,
            command: self.rec.command,
            base_computer: self.rec.base_computer,
            base_reu: self.rec.base_reu,
            bank_reu: self.rec.bank_reu,
            transfer_length: self.rec.transfer_length,
            int_mask: self.rec.int_mask_reg,
            address_control: self.rec.address_control_reg,
            irq: self.irq,
            armed_for_ff00: self.pending == Some(Pending::ArmedForFf00),
            dma_pending: self.dma_pending(),
        }
    }

    /// The 16 register bytes a snapshot carries, read without side effects
    /// (`reu_write_snapshot_module`, reu.c:1591-1618).
    pub fn snapshot_registers(&self) -> [u8; 16] {
        let mut out = [0xFFu8; 16];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.read_without_sideeffects(i as u16);
        }
        out
    }
}

/// What the monitor and `session/reu` report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReuStatus {
    pub size_kb: u32,
    pub status: u8,
    pub command: u8,
    pub base_computer: u16,
    pub base_reu: u16,
    pub bank_reu: u8,
    pub transfer_length: u16,
    pub int_mask: u8,
    pub address_control: u8,
    pub irq: bool,
    pub armed_for_ff00: bool,
    pub dma_pending: bool,
}

impl ExpansionDevice for Reu {
    fn read(&mut self, a: Access, _cart: Option<u8>) -> Option<u8> {
        // `$DF00-$DF1F` are the registers, `$DF20-$DFFF` their mirrors. IO1 is not ours.
        if a.addr & 0xFF00 != 0xDF00 {
            return None;
        }
        self.io_read(a.addr & 0x1F)
    }

    fn peek(&self, addr: u16, _cart: Option<u8>) -> Option<u8> {
        if addr & 0xFF00 != 0xDF00 {
            return None;
        }
        let reg = addr & 0x1F;
        if reg >= REG_FIRST_UNUSED {
            return Some(0xFF);
        }
        Some(self.read_without_sideeffects(reg))
    }

    fn write(&mut self, a: Access, value: u8) {
        if a.addr & 0xFF00 != 0xDF00 {
            return;
        }
        self.io_write(a.addr & 0x1F, value);
    }

    fn snoop_addresses(&self) -> &[u16] {
        &[FF00]
    }

    fn snoop_write(&mut self, a: Access, _value: u8) {
        if a.addr != FF00 {
            return;
        }
        // A host write is not a C64 bus cycle and does not trigger (850 D5).
        if a.kind == AccessKind::Host {
            return;
        }
        // 850's snoop reports BOTH write cycles of a read-modify-write, one cycle apart,
        // where VICE's CPU-core hook sees one (`reu_dma_triggered`, mainc64cpu.c:290-303).
        // No latch is needed for that: arming is a STATE, and the first write consumes it.
        // The second cycle finds `Immediate` rather than `ArmedForFf00` and does nothing,
        // and the transfer itself cannot run in between — it runs at the boundary.
        if self.pending == Some(Pending::ArmedForFf00) {
            self.pending = Some(Pending::Immediate);
        }
    }

    fn lines(&self) -> PortLines {
        PortLines { irq: self.irq, nmi: false, hold: false }
    }

    fn dma_pending(&self) -> bool {
        Reu::dma_pending(self)
    }

    fn clone_device(&self) -> Option<Box<dyn ExpansionDevice>> {
        Some(Box::new(self.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bus with no VIC: every cycle costs exactly one, nothing is ever stolen. Lets the
    /// transfer semantics be tested apart from the badline arithmetic.
    struct FlatBus {
        mem: [u8; 0x10000],
        cycles: u64,
    }

    impl FlatBus {
        fn new() -> Self {
            FlatBus { mem: [0; 0x10000], cycles: 0 }
        }
    }

    impl DmaBus for FlatBus {
        fn dma_read(&mut self, addr: u16) -> u8 {
            self.mem[addr as usize]
        }
        fn dma_write(&mut self, addr: u16, value: u8) {
            self.mem[addr as usize] = value;
        }
        fn clk_inc(&mut self) {
            self.cycles += 1;
        }
        fn ba_low(&mut self) -> bool {
            false
        }
        fn steal(&mut self) {}
    }

    fn armed(reu: &mut Reu, cmd: u8) {
        // The trigger-disabled bit makes it immediate rather than armed for $FF00.
        reu.io_write(REG_COMMAND, cmd | CMD_EXECUTE | CMD_FF00_TRIGGER_DISABLED);
    }

    fn setup(reu: &mut Reu, host: u16, reu_addr: u32, len: u16) {
        reu.io_write(REG_BASEADDR_LOW, host as u8);
        reu.io_write(REG_BASEADDR_HIGH, (host >> 8) as u8);
        reu.io_write(REG_RAMADDR_LOW, reu_addr as u8);
        reu.io_write(REG_RAMADDR_HIGH, (reu_addr >> 8) as u8);
        reu.io_write(REG_BANK, (reu_addr >> 16) as u8);
        reu.io_write(REG_BLOCKLEN_LOW, len as u8);
        reu.io_write(REG_BLOCKLEN_HIGH, (len >> 8) as u8);
    }

    #[test]
    fn reset_disables_the_ff00_trigger_and_sets_length_ffff() {
        let reu = Reu::new(512).unwrap();
        assert_eq!(reu.rec.command, CMD_FF00_TRIGGER_DISABLED);
        assert_eq!(reu.rec.transfer_length, 0xFFFF);
        assert_eq!(reu.rec.bank_reu, BANK_UNUSED);
        assert_eq!(reu.rec.int_mask_reg, INT_UNUSED_MASK);
        assert_eq!(reu.rec.address_control_reg, ADDR_CONTROL_UNUSED_MASK);
    }

    #[test]
    fn the_1700_has_64k_chips_and_its_own_wrap() {
        let o = RecOptions::for_size(128).unwrap();
        assert_eq!(o.status_preset, 0);
        assert_eq!(o.wrap_around, 0x20000);
        let o = RecOptions::for_size(256).unwrap();
        assert_eq!(o.status_preset, STATUS_256K_CHIPS);
        assert_eq!(o.wrap_around, 0x80000);
        assert!(RecOptions::for_size(384).is_none());
    }

    #[test]
    fn an_oversized_reu_keeps_the_512k_rec_wrap() {
        // The wraparound bug: the REC chip still wraps at 512K, the DRAM does not.
        let o = RecOptions::for_size(2048).unwrap();
        assert_eq!(o.wrap_around, 0x80000);
        assert_eq!(o.dram_wrap_around, 2048 * 1024);
        assert_eq!(o.reg_bank_unused, 0);
    }

    #[test]
    fn stash_then_fetch_round_trips() {
        let mut reu = Reu::new(512).unwrap();
        let mut bus = FlatBus::new();
        for i in 0..64u16 {
            bus.mem[0x1000 + i as usize] = i as u8 ^ 0x5A;
        }
        setup(&mut reu, 0x1000, 0, 64);
        armed(&mut reu, CMD_TYPE_TO_REU);
        reu.run_dma(&mut bus);
        assert_eq!(&reu.ram[0..64], &bus.mem[0x1000..0x1040]);

        for b in bus.mem[0x1000..0x1040].iter_mut() {
            *b = 0;
        }
        setup(&mut reu, 0x1000, 0, 64);
        armed(&mut reu, CMD_TYPE_FROM_REU);
        reu.run_dma(&mut bus);
        for i in 0..64u16 {
            assert_eq!(bus.mem[0x1000 + i as usize], i as u8 ^ 0x5A, "byte {i}");
        }
    }

    #[test]
    fn a_length_of_zero_is_64k() {
        let mut reu = Reu::new(512).unwrap();
        let mut bus = FlatBus::new();
        setup(&mut reu, 0, 0, 0);
        armed(&mut reu, CMD_TYPE_TO_REU);
        reu.run_dma(&mut bus);
        // 64 KiB moved, one cycle a byte on a bus that never steals.
        assert_eq!(bus.cycles, 0x10000);
    }

    #[test]
    fn swap_exchanges_both_sides() {
        let mut reu = Reu::new(512).unwrap();
        let mut bus = FlatBus::new();
        bus.mem[0x2000] = 0xAA;
        bus.mem[0x2001] = 0xBB;
        reu.ram[0] = 0x11;
        reu.ram[1] = 0x22;
        setup(&mut reu, 0x2000, 0, 2);
        armed(&mut reu, CMD_TYPE_SWAP);
        reu.run_dma(&mut bus);
        assert_eq!((bus.mem[0x2000], bus.mem[0x2001]), (0x11, 0x22));
        assert_eq!((reu.ram[0], reu.ram[1]), (0xAA, 0xBB));
    }

    #[test]
    fn verify_sets_end_of_block_when_everything_matches() {
        let mut reu = Reu::new(512).unwrap();
        let mut bus = FlatBus::new();
        for i in 0..16usize {
            bus.mem[0x3000 + i] = i as u8;
            reu.ram[i] = i as u8;
        }
        setup(&mut reu, 0x3000, 0, 16);
        armed(&mut reu, CMD_TYPE_VERIFY);
        reu.run_dma(&mut bus);
        assert_eq!(reu.rec.status & STATUS_VERIFY_ERROR, 0);
        assert_ne!(reu.rec.status & STATUS_END_OF_BLOCK, 0);
    }

    #[test]
    fn verify_reports_a_mismatch_in_the_middle_without_end_of_block() {
        let mut reu = Reu::new(512).unwrap();
        let mut bus = FlatBus::new();
        for i in 0..16usize {
            bus.mem[0x3000 + i] = i as u8;
            reu.ram[i] = i as u8;
        }
        reu.ram[4] = 0xFF;
        setup(&mut reu, 0x3000, 0, 16);
        armed(&mut reu, CMD_TYPE_VERIFY);
        reu.run_dma(&mut bus);
        assert_ne!(reu.rec.status & STATUS_VERIFY_ERROR, 0);
        assert_eq!(reu.rec.status & STATUS_END_OF_BLOCK, 0);
    }

    #[test]
    fn the_status_register_clears_its_top_bits_on_read() {
        let mut reu = Reu::new(512).unwrap();
        let mut bus = FlatBus::new();
        setup(&mut reu, 0, 0, 4);
        armed(&mut reu, CMD_TYPE_TO_REU);
        reu.run_dma(&mut bus);
        let first = reu.io_read(REG_STATUS).unwrap();
        assert_ne!(first & STATUS_END_OF_BLOCK, 0);
        let second = reu.io_read(REG_STATUS).unwrap();
        assert_eq!(second & (STATUS_END_OF_BLOCK | STATUS_VERIFY_ERROR | STATUS_INTERRUPT_PENDING), 0);
    }

    #[test]
    fn peek_does_not_clear_the_status() {
        let mut reu = Reu::new(512).unwrap();
        let mut bus = FlatBus::new();
        setup(&mut reu, 0, 0, 4);
        armed(&mut reu, CMD_TYPE_TO_REU);
        reu.run_dma(&mut bus);
        for _ in 0..100 {
            let v = ExpansionDevice::peek(&reu, 0xDF00, None).unwrap();
            assert_ne!(v & STATUS_END_OF_BLOCK, 0);
        }
    }

    #[test]
    fn end_of_block_raises_the_irq_only_when_both_mask_bits_are_set() {
        let mut reu = Reu::new(512).unwrap();
        let mut bus = FlatBus::new();
        reu.io_write(REG_INTERRUPT, INT_END_OF_BLOCK_ENABLED | INT_INTERRUPTS_ENABLED);
        setup(&mut reu, 0, 0, 4);
        armed(&mut reu, CMD_TYPE_TO_REU);
        reu.run_dma(&mut bus);
        assert!(reu.lines().irq);
        // Reading the status drops it.
        reu.io_read(REG_STATUS);
        assert!(!reu.lines().irq);
    }

    #[test]
    fn the_ff00_trigger_fires_once_for_an_rmw_write() {
        let mut reu = Reu::new(512).unwrap();
        let mut bus = FlatBus::new();
        bus.mem[0x4000] = 0x77;
        setup(&mut reu, 0x4000, 0, 1);
        // EXECUTE with the trigger ENABLED: armed, not immediate.
        reu.io_write(REG_COMMAND, CMD_TYPE_TO_REU | CMD_EXECUTE);
        assert!(!reu.dma_pending());

        let a = |clk| Access { addr: FF00, clk, kind: AccessKind::Cpu, stalled: 0, stalled_on_bus: 0 };
        // INC $FF00 reports two write cycles in the SAME cycle window (850 R4).
        reu.snoop_write(a(1000), 0x37);
        reu.snoop_write(a(1000), 0x37);
        assert!(reu.dma_pending());
        reu.run_dma(&mut bus);
        assert_eq!(reu.ram[0], 0x77);
        // And it is not armed any more, so a second $FF00 does nothing.
        reu.snoop_write(a(1001), 0x37);
        assert!(!reu.dma_pending());
    }

    #[test]
    fn a_host_write_to_ff00_never_triggers() {
        let mut reu = Reu::new(512).unwrap();
        reu.io_write(REG_COMMAND, CMD_TYPE_TO_REU | CMD_EXECUTE);
        reu.snoop_write(
            Access { addr: FF00, clk: 5, kind: AccessKind::Host, stalled: 0, stalled_on_bus: 0 },
            0x37,
        );
        assert!(!reu.dma_pending());
    }

    #[test]
    fn autoload_restores_the_shadow_registers() {
        let mut reu = Reu::new(512).unwrap();
        let mut bus = FlatBus::new();
        setup(&mut reu, 0x5000, 0, 8);
        reu.io_write(REG_COMMAND, CMD_TYPE_TO_REU | CMD_AUTOLOAD | CMD_EXECUTE | CMD_FF00_TRIGGER_DISABLED);
        reu.run_dma(&mut bus);
        assert_eq!(reu.rec.base_computer, 0x5000);
        assert_eq!(reu.rec.transfer_length, 8);
    }

    #[test]
    fn without_autoload_the_registers_end_where_the_transfer_stopped() {
        let mut reu = Reu::new(512).unwrap();
        let mut bus = FlatBus::new();
        setup(&mut reu, 0x5000, 0, 8);
        armed(&mut reu, CMD_TYPE_TO_REU);
        reu.run_dma(&mut bus);
        assert_eq!(reu.rec.base_computer, 0x5008);
        assert_eq!(reu.rec.base_reu, 8);
        assert_eq!(reu.rec.transfer_length, 1);
    }

    #[test]
    fn a_fixed_c64_address_does_not_advance() {
        let mut reu = Reu::new(512).unwrap();
        let mut bus = FlatBus::new();
        bus.mem[0x6000] = 0x42;
        setup(&mut reu, 0x6000, 0, 4);
        reu.io_write(REG_ADDR_CONTROL, ADDR_CONTROL_FIX_C64);
        armed(&mut reu, CMD_TYPE_TO_REU);
        reu.run_dma(&mut bus);
        assert_eq!(&reu.ram[0..4], &[0x42, 0x42, 0x42, 0x42]);
        assert_eq!(reu.rec.base_computer, 0x6000);
    }

    #[test]
    fn a_read_above_the_fitted_dram_returns_the_bus_latch() {
        // 256K fitted, but the REC chip addresses 512K: the top half has no DRAM.
        let mut reu = Reu::new(256).unwrap();
        reu.floating_bus = 0x5A;
        assert_eq!(reu.read_from_reu(0x40000), 0x5A);
        reu.store_to_reu(0x40000, 0x99);
        assert_eq!(reu.read_from_reu(0x40000), 0x5A);
    }

    #[test]
    fn registers_are_mirrored_across_df20_to_dfff() {
        let mut reu = Reu::new(512).unwrap();
        let a = |addr| Access { addr, clk: 0, kind: AccessKind::Cpu, stalled: 0, stalled_on_bus: 0 };
        reu.write(a(0xDF02), 0x34);
        // $DF22 is the same register as $DF02.
        assert_eq!(reu.read(a(0xDF22), None), Some(0x34));
        // And IO1 is not ours at all.
        assert_eq!(reu.read(a(0xDE02), None), None);
    }

    #[test]
    fn the_registers_are_dead_while_a_transfer_runs() {
        let mut reu = Reu::new(512).unwrap();
        reu.dma_active = true;
        assert_eq!(reu.io_read(REG_STATUS), Some(0));
        reu.io_write(REG_BASEADDR_LOW, 0x99);
        reu.dma_active = false;
        assert_eq!(reu.rec.base_computer & 0xFF, 0);
    }
}
