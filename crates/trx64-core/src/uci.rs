//! Spec 852 — the Ultimate Command Interface, as the U64 hardware it is.
//!
//! A port of `firmware/1541ultimate/fpga/io/command_interface/vhdl_source/` —
//! `command_protocol.vhd` (the state machine and both register maps), `command_if_pkg.vhd`
//! (offsets and buffer bounds) and `command_interface.vhd` (the 2048-byte dual-port RAM
//! and the split of the firmware's window). The block is identical on every Ultimate
//! firmware target, so it lives on the `u64` profile and is not a cartridge.
//!
//! The C64 side sits on Spec 850's port: register reads and writes in `$DE00-$DFFF`, the
//! IRQ on `INT_SRC_EXPANSION`, freeze as the device's hold line, the `$FF00` trigger and
//! the `$D038`/`$D036` unlock through the write snoop. The firmware side is an API
//! (`fw_read`/`fw_write`/`fw_irq`/`take_events`/`set_routed`) that a host running the
//! firmware maps onto `CMD_IF_BASE`.
//!
//! Granularity: the VHDL is clocked by the FPGA, many times per PHI2 cycle. Here every
//! C64 access and every firmware access is one atomic step, and the two registered valid
//! flags (`:131-140`) are computed at the moment they are looked at — which is what the
//! FPGA's next clock would have registered long before the C64's next access.

use crate::expansion::{Access, AccessKind, ExpansionDevice, PortLines};

// ── command_if_pkg.vhd:7-25 — the firmware side, offsets from CMD_IF_BASE ────────────
const IO_SLOT_BASE: u16 = 0x0;
const IO_SLOT_ENABLE: u16 = 0x1;
const IO_HANDSHAKE_OUT: u16 = 0x2;
const IO_HANDSHAKE_IN: u16 = 0x3;
const IO_COMMAND_START: u16 = 0x4; // read; a write is IRQ_MASK_SET
const IO_COMMAND_END: u16 = 0x5; // read; a write is IRQ_MASK_CLEAR
const IO_RESPONSE_START: u16 = 0x6;
const IO_RESPONSE_END: u16 = 0x7;
const IO_STATUS_START: u16 = 0x8;
const IO_STATUS_END: u16 = 0x9;
const IO_STATUS_LENGTH: u16 = 0xA;
const IO_IRQ_MASK: u16 = 0xB;
const IO_RESPONSE_LEN_L: u16 = 0xC;
const IO_RESPONSE_LEN_H: u16 = 0xD;
const IO_COMMAND_LEN_L: u16 = 0xE;
const IO_COMMAND_LEN_H: u16 = 0xF;
const IO_IRQ_MASK_SET: u16 = 0x4;
const IO_IRQ_MASK_CLEAR: u16 = 0x5;

// ── command_if_pkg.vhd:27-31 — the C64 side, address bits 2:0 ──────────────────────
const SLOT_BUS_ID: u16 = 3;
const SLOT_CONTROL: u16 = 4;
const SLOT_COMMAND: u16 = 5;
const SLOT_RESPONSE: u16 = 6;
const SLOT_STATUS: u16 = 7;

// ── command_if_pkg.vhd:33-41 — the three buffers inside the 2048-byte RAM ───────────
const COMMAND_BUFFER_ADDR: u16 = 0;
const RESPONSE_BUFFER_ADDR: u16 = 896;
const STATUS_BUFFER_ADDR: u16 = 1792;
const COMMAND_BUFFER_END: u16 = 895;
const RESPONSE_BUFFER_END: u16 = 1791;
const STATUS_BUFFER_END: u16 = 2047;
const RAM_SIZE: usize = 2048;

/// `command_interface.vhd:47-62` splits the firmware's window on address bits 12:11: port 0
/// the registers (which decode bits 3:0 only, so they repeat every 16 bytes), port 1 the
/// RAM. Ports 2 and 3 do not exist and read 0.
const FW_RAM_BASE: u16 = 0x800;
const FW_WINDOW_END: u16 = 0x1000;

/// The `$FF00` trigger (`slot_server_v4.vhd:863`, a C64 write cycle at `$FF00`) and the U64
/// unlock (`megabyter.tas:71-74`: `$AB` to `$D038`, then `$CD` to `$D036`).
const TRIGGER_ADDR: u16 = 0xff00;
const UNLOCK_KEY1_REG: u16 = 0xd038;
const UNLOCK_KEY1_VAL: u8 = 0xab;
const UNLOCK_KEY2_REG: u16 = 0xd036;
const UNLOCK_KEY2_VAL: u8 = 0xcd;
const SNOOPED: [u16; 3] = [TRIGGER_ADDR, UNLOCK_KEY2_REG, UNLOCK_KEY1_REG];

/// What happened on the C64 side that the firmware cannot see for itself (Spec 852 D4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UciEvents {
    /// The C64 was reset (`warm_reset`/`cold_reset`). The firmware learns this through ITU
    /// low bit 7 and aborts a pending command (`command_intf.cc` `ResetInterruptHandlerCmdIf`).
    pub c64_reset: bool,
    /// `$AB → $D038` followed by `$CD → $D036`. The firmware's `unlock_irq` then enables the
    /// block at `$DF1C` (`u64_config.cc:1012-1019`); standalone TRX64 only reports it.
    pub unlock: bool,
}

/// Everything the block holds, read-only, for the monitor's `uci` verb and the daemon's
/// `session/uci` (Spec 852 D6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UciStatus {
    pub enabled: bool,
    /// Address bits 8:3 the block answers on.
    pub slot_base: u8,
    /// First address of the eight-byte window (`$DF18` for the firmware's `0x47`).
    pub window: u16,
    pub routed_io1: bool,
    pub routed_io2: bool,
    /// The C64 control register: b7 response valid, b6 status valid, b5:4 state, b3 error,
    /// b2 abort, b1 data accepted, b0 new command.
    pub status_byte: u8,
    pub state: u8,
    pub response_valid: bool,
    pub status_valid: bool,
    pub error: bool,
    pub abort: bool,
    pub data_accepted: bool,
    pub new_command: bool,
    pub command_length: u16,
    pub response_pointer: u16,
    pub response_length: u16,
    pub status_pointer: u16,
    pub status_length: u16,
    pub cmd_irq_en: bool,
    /// The C64 IRQ the block drives: state(1) AND `cmd_irq_en`.
    pub irq: bool,
    /// The hold on the 6510.
    pub freeze: bool,
    pub trigger: bool,
    /// The firmware's IRQ: (handshake_in AND NOT irq_mask) ≠ 0.
    pub firmware_irq: bool,
    pub irq_mask: u8,
    pub bus_id: u8,
    /// Events not yet taken by the firmware side.
    pub pending: UciEvents,
}

/// The block. Power-on state = the VHDL reset (`command_protocol.vhd:292-306`): disabled,
/// so a `u64` machine without a firmware reads the open bus in the window.
#[derive(Clone)]
pub struct Uci {
    enabled: bool,
    /// `slot_base : unsigned(6 downto 1)` — six bits, compared with address bits 8:3.
    slot_base: u8,
    /// Bits 4:0 settable, 7:5 read 0. Not in the reset block: FPGA configuration inits it.
    bus_id: u8,
    irq_mask: u8,
    command_pointer: u16,
    response_pointer: u16,
    status_pointer: u16,
    response_length: u16,
    status_length: u16,
    freeze: bool,
    trigger: bool,
    cmd_irq_en: bool,
    /// `slot_status(5 downto 4)`.
    state: u8,
    error_busy: bool,
    /// `slot_status(2 downto 0)`: abort, data accepted, new command.
    handshake_in: u8,
    ram: [u8; RAM_SIZE],
    /// The U64 bus multiplexer's `C64_BUS_INTERNAL` bits 0/1, fed by the host (Spec 852 §5).
    /// Not block state: a reset leaves them.
    routed_io1: bool,
    routed_io2: bool,
    /// The unlock detector saw `$AB → $D038` as the last C64 write it was shown.
    unlock_armed: bool,
    events: UciEvents,
}

impl Default for Uci {
    fn default() -> Self {
        Self::new()
    }
}

impl Uci {
    /// FPGA power-on: the reset values, bus id 0, the RAM zeroed, both ranges routed.
    pub fn new() -> Self {
        Self {
            enabled: false,
            slot_base: 0,
            bus_id: 0,
            irq_mask: 0b111,
            command_pointer: COMMAND_BUFFER_ADDR,
            response_pointer: RESPONSE_BUFFER_ADDR,
            status_pointer: STATUS_BUFFER_ADDR,
            response_length: 0,
            status_length: 0,
            freeze: false,
            trigger: false,
            cmd_irq_en: false,
            state: 0,
            error_busy: false,
            handshake_in: 0,
            ram: [0; RAM_SIZE],
            routed_io1: true,
            routed_io2: true,
            unlock_armed: false,
            events: UciEvents::default(),
        }
    }

    /// Back to power-on, keeping the host's routing (Spec 852 D7: a restore resets the block).
    pub(crate) fn reset_to_power_on(&mut self) {
        *self = Self { routed_io1: self.routed_io1, routed_io2: self.routed_io2, ..Self::new() };
    }

    /// A C64 reset happened. Nothing in the block changes (`:292-306` is the FPGA reset
    /// only); the firmware is told through `take_events`.
    pub(crate) fn note_c64_reset(&mut self) {
        self.events.c64_reset = true;
    }

    // ── the registered flags, computed where they are looked at (`:131-140`) ──────────

    #[inline]
    fn response_valid(&self) -> bool {
        (self.response_pointer.wrapping_sub(RESPONSE_BUFFER_ADDR) & 0x7ff) < self.response_length
            && self.state & 0b10 != 0
            && self.handshake_in & 0b100 == 0
    }

    #[inline]
    fn status_valid(&self) -> bool {
        (self.status_pointer.wrapping_sub(STATUS_BUFFER_ADDR) & 0x7ff) < self.status_length
            && self.state & 0b10 != 0
            && self.handshake_in & 0b100 == 0
    }

    /// `slot_status`.
    #[inline]
    fn status_byte(&self) -> u8 {
        (u8::from(self.response_valid()) << 7)
            | (u8::from(self.status_valid()) << 6)
            | (self.state << 4)
            | (u8::from(self.error_busy) << 3)
            | self.handshake_in
    }

    #[inline]
    fn command_length(&self) -> u16 {
        self.command_pointer.wrapping_sub(COMMAND_BUFFER_ADDR) & 0x7ff
    }

    /// `slot_resp.irq` (`:109`).
    #[inline]
    fn irq_active(&self) -> bool {
        self.state & 0b10 != 0 && self.cmd_irq_en
    }

    // ── the C64 side ─────────────────────────────────────────────────────────────────

    /// The register an access in `$DE00-$DFFF` hits, or None. `:108`/`:141`: address bits
    /// 8:3 equal `slot_base` and the block is enabled; IO1 (`$DE`, bit 8 clear) and IO2
    /// (`$DF`) must each be routed onto the internal bus.
    #[inline]
    fn decode(&self, addr: u16) -> Option<u16> {
        if !self.enabled || ((addr >> 3) & 0x3f) as u8 != self.slot_base {
            return None;
        }
        let routed = if addr & 0x100 != 0 { self.routed_io2 } else { self.routed_io1 };
        routed.then_some(addr & 7)
    }

    /// `:97-106`, combinational from the address and the state.
    #[inline]
    fn c64_value(&self, reg: u16) -> u8 {
        match reg {
            SLOT_CONTROL => self.status_byte(),
            // `irq_n & "1001001"`: $C9, and $49 while the IRQ is active.
            SLOT_COMMAND => (u8::from(!self.irq_active()) << 7) | 0x49,
            SLOT_RESPONSE if self.response_valid() => self.ram[self.response_pointer as usize],
            SLOT_STATUS if self.status_valid() => self.ram[self.status_pointer as usize],
            SLOT_RESPONSE | SLOT_STATUS => 0x00,
            SLOT_BUS_ID => self.bus_id,
            _ => 0xff,
        }
    }

    /// `:141-173` — a C64 write to one of the block's registers.
    fn c64_write(&mut self, reg: u16, value: u8) {
        match reg {
            SLOT_COMMAND => {
                // `:117-121` — the RAM takes the byte at the pointer, clamped or not.
                self.ram[self.command_pointer as usize] = value;
                if self.command_pointer != COMMAND_BUFFER_END {
                    self.command_pointer += 1;
                }
            }
            SLOT_CONTROL => {
                // VHDL signals: every condition below reads the state from BEFORE the write.
                let old_state = self.state;
                if value & 0x08 != 0 {
                    self.error_busy = false;
                }
                if value & 0x01 != 0 {
                    // PUSH_CMD
                    self.freeze = value & 0x80 != 0;
                    self.trigger = value & 0x40 != 0;
                    if old_state == 0 {
                        self.state = 0b01;
                        self.handshake_in |= 0b001;
                    } else {
                        self.error_busy = true;
                    }
                    self.cmd_irq_en = value & 0x20 != 0;
                }
                if value & 0x02 != 0 && old_state & 0b10 != 0 {
                    // DATA_ACC: data accepted only for "more"; leave the data state.
                    self.handshake_in = (self.handshake_in & !0b010) | ((old_state & 0b01) << 1);
                    self.state &= !0b10;
                    self.cmd_irq_en = false;
                }
                if value & 0x04 != 0 {
                    // ABORT — only the firmware clears it.
                    self.handshake_in |= 0b100;
                }
            }
            _ => {}
        }
    }

    /// Spec 852 D4 — the unlock rule, shown every C64 write the device sees: the next one
    /// after `$D038 = $AB` must be `$D036 = $CD`, anything else re-arms.
    #[inline]
    fn unlock_step(&mut self, addr: u16, value: u8) {
        if self.unlock_armed && addr == UNLOCK_KEY2_REG && value == UNLOCK_KEY2_VAL {
            self.events.unlock = true;
        }
        self.unlock_armed = addr == UNLOCK_KEY1_REG && value == UNLOCK_KEY1_VAL;
    }

    // ── the firmware side (Spec 852 D3) ──────────────────────────────────────────────

    /// A read of `CMD_IF_BASE + off`: registers at 0x000-0x7FF (repeating every 16 bytes),
    /// the RAM at 0x800-0xFFF. No side effects — none of the VHDL's firmware reads has one.
    pub fn fw_read(&self, off: u16) -> u8 {
        if off >= FW_WINDOW_END {
            return 0;
        }
        if off >= FW_RAM_BASE {
            return self.ram[(off - FW_RAM_BASE) as usize];
        }
        // `:252-289`. Bits the VHDL does not drive read 0 (`c_io_resp_init`).
        match off & 0xf {
            IO_SLOT_BASE => self.slot_base << 1,
            IO_SLOT_ENABLE => u8::from(self.enabled),
            IO_HANDSHAKE_OUT => {
                (u8::from(self.freeze) << 7) | (u8::from(self.trigger) << 6) | (self.state << 4)
            }
            IO_HANDSHAKE_IN => self.status_byte(),
            IO_COMMAND_START => (COMMAND_BUFFER_ADDR >> 3) as u8,
            IO_COMMAND_END => (COMMAND_BUFFER_END >> 3) as u8,
            IO_RESPONSE_START => (RESPONSE_BUFFER_ADDR >> 3) as u8,
            IO_RESPONSE_END => (RESPONSE_BUFFER_END >> 3) as u8,
            IO_STATUS_START => (STATUS_BUFFER_ADDR >> 3) as u8,
            IO_STATUS_END => (STATUS_BUFFER_END >> 3) as u8,
            // "fixme" in the VHDL: the pointer's low byte, not the length.
            IO_STATUS_LENGTH => self.status_pointer as u8,
            IO_IRQ_MASK => self.irq_mask,
            // The response read POINTER, not the length that was written.
            IO_RESPONSE_LEN_L => self.response_pointer as u8,
            IO_RESPONSE_LEN_H => ((self.response_pointer >> 8) & 7) as u8,
            IO_COMMAND_LEN_L => self.command_length() as u8,
            IO_COMMAND_LEN_H => ((self.command_length() >> 8) & 7) as u8,
            _ => unreachable!(),
        }
    }

    /// A write to `CMD_IF_BASE + off`. A change to freeze or the IRQ takes effect at the
    /// C64's next instruction boundary.
    pub fn fw_write(&mut self, off: u16, value: u8) {
        if off >= FW_WINDOW_END {
            return;
        }
        if off >= FW_RAM_BASE {
            self.ram[(off - FW_RAM_BASE) as usize] = value;
            return;
        }
        // `:198-249`.
        match off & 0xf {
            IO_SLOT_BASE => self.slot_base = (value >> 1) & 0x3f,
            IO_SLOT_ENABLE => {
                if value & 0x80 == 0 {
                    self.enabled = value & 0x01 != 0;
                } else {
                    self.bus_id = value & 0x1f;
                }
            }
            IO_HANDSHAKE_OUT => {
                if value & 0x01 != 0 {
                    // accept command: clear new-command, rewind the command pointer
                    self.handshake_in &= !0b001;
                    self.command_pointer = COMMAND_BUFFER_ADDR;
                }
                if value & 0x02 != 0 {
                    self.handshake_in &= !0b010;
                }
                if value & 0x04 != 0 {
                    self.handshake_in &= !0b100;
                }
                if value & 0x10 != 0 {
                    // validate: state 1x, x = the more bit
                    self.trigger = false;
                    self.freeze = false;
                    self.state = 0b10 | ((value >> 5) & 1);
                    self.reset_response();
                }
                if value & 0x80 != 0 {
                    self.freeze = false;
                    self.trigger = false;
                    self.reset_response();
                    self.state = 0;
                }
            }
            IO_STATUS_LENGTH => {
                self.status_pointer = STATUS_BUFFER_ADDR;
                self.status_length = (self.status_length & !0xff) | u16::from(value);
            }
            IO_RESPONSE_LEN_L => {
                self.response_pointer = RESPONSE_BUFFER_ADDR;
                self.response_length = (self.response_length & !0xff) | u16::from(value);
            }
            IO_RESPONSE_LEN_H => {
                self.response_length = (self.response_length & 0xff) | (u16::from(value & 7) << 8);
            }
            IO_IRQ_MASK => self.irq_mask = value & 7,
            IO_IRQ_MASK_SET => self.irq_mask |= value & 7,
            IO_IRQ_MASK_CLEAR => self.irq_mask &= !(value & 7),
            _ => {}
        }
    }

    /// `reset_response` (`:124-128`).
    fn reset_response(&mut self) {
        self.response_pointer = RESPONSE_BUFFER_ADDR;
        self.status_pointer = STATUS_BUFFER_ADDR;
    }

    /// The firmware's interrupt (ITU low bit 4), a level (`:310`).
    pub fn fw_irq(&self) -> bool {
        self.handshake_in & !self.irq_mask & 7 != 0
    }

    /// The events since the last call. On a `Machine`, reach the block through
    /// `Machine::uci_mut`, which hands it the C64 resets first.
    pub fn take_events(&mut self) -> UciEvents {
        std::mem::take(&mut self.events)
    }

    /// Whether the U64 bus multiplexer puts the internal IO1 / IO2 range on the C64 bus
    /// (`C64_BUS_INTERNAL` bits 0 and 1). The window answers only in a routed range.
    /// Standalone both are routed.
    pub fn set_routed(&mut self, io1: bool, io2: bool) {
        self.routed_io1 = io1;
        self.routed_io2 = io2;
    }

    pub fn status(&self) -> UciStatus {
        UciStatus {
            enabled: self.enabled,
            slot_base: self.slot_base,
            window: 0xde00 + (u16::from(self.slot_base) << 3),
            routed_io1: self.routed_io1,
            routed_io2: self.routed_io2,
            status_byte: self.status_byte(),
            state: self.state,
            response_valid: self.response_valid(),
            status_valid: self.status_valid(),
            error: self.error_busy,
            abort: self.handshake_in & 0b100 != 0,
            data_accepted: self.handshake_in & 0b010 != 0,
            new_command: self.handshake_in & 0b001 != 0,
            command_length: self.command_length(),
            response_pointer: self.response_pointer,
            response_length: self.response_length,
            status_pointer: self.status_pointer,
            status_length: self.status_length,
            cmd_irq_en: self.cmd_irq_en,
            irq: self.irq_active(),
            freeze: self.freeze,
            trigger: self.trigger,
            firmware_irq: self.fw_irq(),
            irq_mask: self.irq_mask,
            bus_id: self.bus_id,
            pending: self.events,
        }
    }
}

impl ExpansionDevice for Uci {
    /// `:97-106` for the value, then `:174-189`: a read of the response or status register
    /// turns the command IRQ off and advances its pointer, byte available or not. A read
    /// the VIC stretched is on the bus `stalled_on_bus + 1` cycles, and the FPGA counts
    /// each (Spec 852 D5).
    fn read(&mut self, a: Access, _cart: Option<u8>) -> Option<u8> {
        let reg = self.decode(a.addr)?;
        let value = self.c64_value(reg);
        let advance = |ptr: &mut u16, end: u16| {
            *ptr = (u32::from(*ptr) + a.stalled_on_bus + 1).min(u32::from(end)) as u16;
        };
        match reg {
            SLOT_RESPONSE => {
                self.cmd_irq_en = false;
                advance(&mut self.response_pointer, RESPONSE_BUFFER_END);
            }
            SLOT_STATUS => {
                self.cmd_irq_en = false;
                advance(&mut self.status_pointer, STATUS_BUFFER_END);
            }
            _ => {}
        }
        Some(value)
    }

    fn peek(&self, addr: u16, _cart: Option<u8>) -> Option<u8> {
        self.decode(addr).map(|reg| self.c64_value(reg))
    }

    fn write(&mut self, a: Access, value: u8) {
        if a.kind != AccessKind::Host {
            self.unlock_step(a.addr, value);
        }
        if let Some(reg) = self.decode(a.addr) {
            self.c64_write(reg, value);
        }
    }

    fn snoop_addresses(&self) -> &[u16] {
        &SNOOPED
    }

    /// `$FF00` (`:192-195`) and the unlock keys. A host write is not a C64 bus cycle — the
    /// firmware's DMA or a debugger — so it neither triggers nor counts for the unlock.
    fn snoop_write(&mut self, a: Access, value: u8) {
        if a.kind == AccessKind::Host {
            return;
        }
        if a.addr == TRIGGER_ADDR && self.trigger {
            self.freeze = true;
            self.trigger = false;
        }
        self.unlock_step(a.addr, value);
    }

    fn lines(&self) -> PortLines {
        PortLines { irq: self.irq_active(), nmi: false, hold: self.freeze }
    }

    fn clone_device(&self) -> Option<Box<dyn ExpansionDevice>> {
        Some(Box::new(self.clone()))
    }
}
