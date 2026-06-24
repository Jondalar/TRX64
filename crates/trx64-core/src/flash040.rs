//! flash040.rs — STRICT 1:1 port of the (AM)29F0[14]0(B) flash command state
//! machine. Source of truth (port VERBATIM, `ts:`/`vice:` line tags):
//!
//!   C64ReverseEngineeringMCP/src/runtime/headless/cartridge.ts
//!     Flash040Type rows (ts:537-580), Flash040StateName (ts:582-586),
//!     FLASH040_STATES (ts:588-592), class Flash040 (ts:594-818).
//!   cross-checked verbatim against VICE src/core/flash040core.c:
//!     flash_types[] (vice:70-107), flash_magic_1/2 (vice:111-119),
//!     flash_program_byte (vice:171-182), flash_write/erase_operation_status
//!     (vice:184-209), erase_alarm_handler (vice:213-259),
//!     flash040core_store_internal (vice:263-394), flash040core_read (vice:409+).
//!
//! The erase "alarm" is modelled LAZILY (no tick): `erase_alarm_clk` is the
//! absolute maincpu_clk at which the next erase step completes; it is applied on
//! the next flash access at-or-after that clk (visible behaviour identical to
//! VICE's alarm — software only observes the flash via reads, and each read
//! catches the alarm up). The live `clk` is passed into read()/store() by the
//! bus (the c64re TS wired a `()->clk` closure; the Rust port threads the value
//! through the call instead — the FullBus already has `self.clk` at every
//! access, so no stored closure is needed).

// Verbatim 1:1 port: the `& 0xff` / `(1 << x) & 0xff` masks document the VICE
// uint8_t semantics and are kept even where Rust's u8 makes them redundant.
#![allow(clippy::identity_op)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::collapsible_else_if)]

/// ts:537-543 / vice:51-67 — flash_types_s row. One of these parametrizes the
/// core so EasyFlash (TYPE_B), GMOD2 (TYPE_NORMAL), C64MegaCart (TYPE_160) and
/// MegaByter (MX29F800CB) share one faithful implementation.
#[derive(Clone, Copy)]
pub struct Flash040Type {
    pub manufacturer_id: u8,
    pub device_id: u8,
    pub device_id_addr: u32,
    pub size: u32,
    pub sector_mask: u32,
    pub sector_size: u32,
    pub sector_shift: u32,
    pub magic1_addr: u32,
    pub magic2_addr: u32,
    pub magic1_mask: u32,
    pub magic2_mask: u32,
    pub status_toggle_bits: u8,
    pub erase_sector_timeout_cycles: u64,
    pub erase_sector_cycles: u64,
    pub erase_chip_cycles: u64,
}

/// ts:545-551 / vice:71-77 — AM29F040 (FLASH040_TYPE_NORMAL) — GMOD2.
pub const FLASH040_NORMAL: Flash040Type = Flash040Type {
    manufacturer_id: 0x01,
    device_id: 0xa4,
    device_id_addr: 1,
    size: 0x80000,
    sector_mask: 0x70000,
    sector_size: 0x10000,
    sector_shift: 16,
    magic1_addr: 0x5555,
    magic2_addr: 0x2aaa,
    magic1_mask: 0x7fff,
    magic2_mask: 0x7fff,
    status_toggle_bits: 0x40,
    erase_sector_timeout_cycles: 80,
    erase_sector_cycles: 2_000_000,
    erase_chip_cycles: 14_000_000,
};

/// ts:553-559 / vice:78-84 — AM29F040B (FLASH040_TYPE_B) — EasyFlash.
pub const FLASH040B: Flash040Type = Flash040Type {
    manufacturer_id: 0x01,
    device_id: 0xa4,
    device_id_addr: 1,
    size: 0x80000,
    sector_mask: 0x70000,
    sector_size: 0x10000,
    sector_shift: 16,
    magic1_addr: 0x555,
    magic2_addr: 0x2aa,
    magic1_mask: 0x7ff,
    magic2_mask: 0x7ff,
    status_toggle_bits: 0x40,
    erase_sector_timeout_cycles: 50,
    erase_sector_cycles: 1_000_000,
    erase_chip_cycles: 8_000_000,
};

/// ts:563-569 — M29F160FT (FLASH040_TYPE_160, martinpiper fork) — C64MegaCart.
/// 2MB, device id 0xd2 at addr 2, magic 0xaaa/0x555 mask 0xfff.
pub const FLASH040_160: Flash040Type = Flash040Type {
    manufacturer_id: 0x01,
    device_id: 0xd2,
    device_id_addr: 2,
    size: 0x200000,
    sector_mask: 0x7f0000,
    sector_size: 0x10000,
    sector_shift: 16,
    magic1_addr: 0xaaa,
    magic2_addr: 0x555,
    magic1_mask: 0xfff,
    magic2_mask: 0xfff,
    status_toggle_bits: 0x40,
    erase_sector_timeout_cycles: 50,
    erase_sector_cycles: 1_000_000,
    erase_chip_cycles: 8_000_000,
};

/// ts:574-580 — MX29F800CB (FLASH800_TYPE_CB) — MegaByter. The same AMD command
/// state machine as flash040core (identical states + `old & byte` program), just
/// a different device row. 1MB, mfg 0xc2 / dev 0x58, magic 0xaaa/0x555 mask 0xfff.
pub const FLASH800_CB: Flash040Type = Flash040Type {
    manufacturer_id: 0xc2,
    device_id: 0x58,
    device_id_addr: 1,
    size: 0x100000,
    sector_mask: 0x0f0000,
    sector_size: 0x10000,
    sector_shift: 16,
    magic1_addr: 0xaaa,
    magic2_addr: 0x555,
    magic1_mask: 0xfff,
    magic2_mask: 0xfff,
    status_toggle_bits: 0x40,
    erase_sector_timeout_cycles: 40,
    erase_sector_cycles: 700_000,
    erase_chip_cycles: 8_000_000,
};

/// vice:flash040.h FLASH040_ERASE_MASK_SIZE.
const FLASH040_ERASE_MASK_SIZE: usize = 8;

/// ts:582-592 — Flash040 state. Index order = VICE flash040_state_s enum (for
/// snapshot serialization parity).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlashState {
    Read = 0,
    Magic1 = 1,
    Magic2 = 2,
    Autoselect = 3,
    ByteProgram = 4,
    ByteProgramError = 5,
    EraseMagic1 = 6,
    EraseMagic2 = 7,
    EraseSelect = 8,
    ChipErase = 9,
    SectorErase = 10,
    SectorEraseTimeout = 11,
    SectorEraseSuspend = 12,
}

impl FlashState {
    fn from_index(i: u8) -> FlashState {
        match i {
            0 => FlashState::Read,
            1 => FlashState::Magic1,
            2 => FlashState::Magic2,
            3 => FlashState::Autoselect,
            4 => FlashState::ByteProgram,
            5 => FlashState::ByteProgramError,
            6 => FlashState::EraseMagic1,
            7 => FlashState::EraseMagic2,
            8 => FlashState::EraseSelect,
            9 => FlashState::ChipErase,
            10 => FlashState::SectorErase,
            11 => FlashState::SectorEraseTimeout,
            12 => FlashState::SectorEraseSuspend,
            _ => FlashState::Read,
        }
    }
}

/// Persistable Flash040 continuation (= Flash040SnapState, types.ts:86-93). The
/// flash DATA is NOT here — it rides in the separate writable image — only the
/// command-FSM continuation + the pending erase-alarm clock.
#[derive(Clone, Copy)]
pub struct Flash040SnapState {
    pub state: u8,
    pub base_state: u8,
    pub program_byte: u8,
    pub last_read: u8,
    pub dirty: bool,
    pub erase_mask: [u8; FLASH040_ERASE_MASK_SIZE],
    pub erase_alarm_clk: i64,
}

/// ts:594-818 / vice:flash040core.c — the AMD Am29F0[14]0(B) flash chip.
#[derive(Clone)]
pub struct Flash040 {
    /// The flash array. `pub` so the mapper can slice it for the writable image /
    /// CRT re-pack (= getData() with a catch_up first).
    pub data: Vec<u8>,
    label: &'static str,
    t: Flash040Type,
    state: FlashState,
    base_state: FlashState,
    program_byte: u8,
    last_read: u8,
    dirty: bool,
    generation: u64,
    erase_mask: [u8; FLASH040_ERASE_MASK_SIZE],
    /// Absolute maincpu_clk of the next erase step; -1 = unset (= alarm_unset).
    erase_alarm_clk: i64,
}

impl Flash040 {
    /// ts:604 — constructor: `data` is the linear chip array, `t` the device row.
    pub fn new(data: Vec<u8>, label: &'static str, t: Flash040Type) -> Self {
        Flash040 {
            data,
            label,
            t,
            state: FlashState::Read,
            base_state: FlashState::Read,
            program_byte: 0,
            last_read: 0,
            dirty: false,
            generation: 0,
            erase_mask: [0; FLASH040_ERASE_MASK_SIZE],
            erase_alarm_clk: -1,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
    /// Monotonic mutation counter for an auto-persist debounce (ts:608-609).
    pub fn writable_generation(&self) -> u64 {
        self.generation
    }
    /// "operation active" status (command sequence / erase busy). Exposed for
    /// UI/debug (ts:610-612).
    pub fn is_busy(&self) -> bool {
        self.state != FlashState::Read
    }
    /// ts:621 — debug label:state string.
    pub fn mode(&self) -> String {
        format!("{}:{:?}", self.label, self.state)
    }

    /// ts:613-619 — apply any erase steps due at the live `clk`, then hand back
    /// the array (caller serializes post-alarm data, not stale lazy data).
    pub fn get_data(&mut self, clk: u64) -> &[u8] {
        self.catch_up_erase(clk as i64);
        &self.data
    }
    /// ts:620 — load a writable image back into the array (DATA only).
    pub fn load_data(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(self.data.len());
        self.data[..n].copy_from_slice(&bytes[..n]);
    }

    // ts:623-625 / vice:111-119,137-143 — address predicates.
    fn magic1(&self, addr: u32) -> bool {
        (addr & self.t.magic1_mask) == self.t.magic1_addr
    }
    fn magic2(&self, addr: u32) -> bool {
        (addr & self.t.magic2_mask) == self.t.magic2_addr
    }
    fn sector_num(&self, addr: u32) -> u32 {
        (addr & self.t.sector_mask) >> self.t.sector_shift
    }

    /// ts:627-658 / vice:213-259 — erase_alarm_handler applied LAZILY for every
    /// elapsed step. Each step chains the next alarm off the FIRED alarm's
    /// scheduled clk (`fire_clk`), not the current clk, so the multi-step erase
    /// timeline matches VICE's real alarm exactly regardless of when a read
    /// happens to catch it up.
    fn catch_up_erase(&mut self, clk: i64) {
        let mut guard = 0;
        while self.erase_alarm_clk >= 0 && clk >= self.erase_alarm_clk && guard < 256 {
            guard += 1;
            let fire_clk = self.erase_alarm_clk;
            self.erase_alarm_clk = -1; // alarm_unset
            match self.state {
                FlashState::SectorEraseTimeout => {
                    self.erase_alarm_clk = fire_clk + self.t.erase_sector_cycles as i64;
                    self.state = FlashState::SectorErase;
                }
                FlashState::SectorErase => {
                    for i in 0..(8 * FLASH040_ERASE_MASK_SIZE) {
                        let j = i >> 3;
                        let m = (1u8 << (i & 7)) & 0xff;
                        if self.erase_mask[j] & m != 0 {
                            self.erase_sector(i as u32);
                            self.erase_mask[j] &= !m & 0xff;
                            break;
                        }
                    }
                    let mut any = 0u8;
                    for i in 0..FLASH040_ERASE_MASK_SIZE {
                        any |= self.erase_mask[i];
                    }
                    if any != 0 {
                        self.erase_alarm_clk = fire_clk + self.t.erase_sector_cycles as i64;
                    } else {
                        self.state = self.base_state;
                    }
                }
                FlashState::ChipErase => {
                    self.erase_chip();
                    self.state = self.base_state;
                }
                _ => {}
            }
        }
    }

    /// ts:665-667 — side-effect-free flash array byte. Ignores the command-state
    /// machine + erase busy status (no DQ6/DQ3/DQ7 toggling, no catch-up, no
    /// last_read mutation): just the stored array byte the CPU sees in plain READ.
    pub fn peek(&self, addr: u32) -> u8 {
        *self.data.get(addr as usize).unwrap_or(&0xff)
    }

    /// ts:669-703 / vice:409+ — read: the command-FSM-aware byte. Hot path: in
    /// READ state with no pending erase (the overwhelming common case — the CPU
    /// fetching code/data from flash) just index the array.
    pub fn read(&mut self, addr: u32, clk: u64) -> u8 {
        if self.state == FlashState::Read && self.erase_alarm_clk < 0 {
            self.last_read = *self.data.get(addr as usize).unwrap_or(&0xff);
            return self.last_read;
        }
        self.catch_up_erase(clk as i64);
        let v: u8 = match self.state {
            FlashState::Autoselect => {
                let a = addr & 0xff;
                if a == 0 {
                    self.t.manufacturer_id
                } else if a == self.t.device_id_addr {
                    self.t.device_id
                } else if a == 2 {
                    0
                } else {
                    *self.data.get(addr as usize).unwrap_or(&0xff)
                }
            }
            FlashState::ByteProgramError => self.write_operation_status(clk),
            FlashState::SectorEraseSuspend
            | FlashState::ChipErase
            | FlashState::SectorErase
            | FlashState::SectorEraseTimeout => self.erase_operation_status(),
            // read + any in-command state (a read does NOT reset the state).
            _ => *self.data.get(addr as usize).unwrap_or(&0xff),
        };
        self.last_read = v & 0xff;
        self.last_read
    }

    /// ts:706-708 / vice:184-190 — DQ7 inverse-of-data, DQ6 toggle, DQ5 timeout.
    fn write_operation_status(&self, clk: u64) -> u8 {
        ((((self.program_byte ^ 0x80) as u32 & 0x80)
            | (((clk & 2) as u32) << 5)
            | 0x20)
            & 0xff) as u8
    }
    /// ts:710-714 / vice:192-209 — DQ6 toggle (status_toggle_bits), DQ3 timer.
    fn erase_operation_status(&mut self) -> u8 {
        let v = self.program_byte;
        self.program_byte = (self.program_byte ^ self.t.status_toggle_bits) & 0xff;
        (if self.state != FlashState::SectorEraseTimeout {
            v | 0x08
        } else {
            v
        }) & 0xff
    }

    /// ts:717-777 / vice:263-394 — flash040core_store_internal. The 13-state AMD
    /// command machine.
    pub fn store(&mut self, addr: u32, byte: u8, clk: u64) {
        self.catch_up_erase(clk as i64);
        let b = byte & 0xff;
        match self.state {
            FlashState::Read => {
                if self.magic1(addr) && b == 0xaa {
                    self.state = FlashState::Magic1;
                }
            }
            FlashState::Magic1 => {
                self.state = if self.magic2(addr) && b == 0x55 {
                    FlashState::Magic2
                } else {
                    self.base_state
                };
            }
            FlashState::Magic2 => {
                if self.magic1(addr) {
                    match b {
                        0x90 => {
                            self.state = FlashState::Autoselect;
                            self.base_state = FlashState::Autoselect;
                        }
                        0xf0 => {
                            self.state = FlashState::Read;
                            self.base_state = FlashState::Read;
                        }
                        0xa0 => self.state = FlashState::ByteProgram,
                        0x80 => self.state = FlashState::EraseMagic1,
                        _ => self.state = self.base_state,
                    }
                } else {
                    self.state = self.base_state;
                }
            }
            FlashState::ByteProgram => {
                self.state = if self.program_byte_op(addr, b) {
                    self.base_state
                } else {
                    FlashState::ByteProgramError
                };
            }
            FlashState::EraseMagic1 => {
                self.state = if self.magic1(addr) && b == 0xaa {
                    FlashState::EraseMagic2
                } else {
                    self.base_state
                };
            }
            FlashState::EraseMagic2 => {
                self.state = if self.magic2(addr) && b == 0x55 {
                    FlashState::EraseSelect
                } else {
                    self.base_state
                };
            }
            FlashState::EraseSelect => {
                if self.magic1(addr) && b == 0x10 {
                    self.state = FlashState::ChipErase;
                    self.program_byte = 0;
                    self.erase_alarm_clk = clk as i64 + self.t.erase_chip_cycles as i64;
                } else if b == 0x30 {
                    self.add_sector_to_erase_mask(addr);
                    self.program_byte = 0;
                    self.state = FlashState::SectorEraseTimeout;
                    self.erase_alarm_clk =
                        clk as i64 + self.t.erase_sector_timeout_cycles as i64;
                } else {
                    self.state = self.base_state;
                }
            }
            FlashState::SectorEraseTimeout => {
                if b == 0x30 {
                    self.add_sector_to_erase_mask(addr);
                } else {
                    self.state = self.base_state;
                    self.erase_mask.fill(0);
                    self.erase_alarm_clk = -1;
                }
            }
            FlashState::SectorErase => {
                if b == 0xb0 {
                    self.state = FlashState::SectorEraseSuspend;
                    self.erase_alarm_clk = -1;
                }
            }
            FlashState::SectorEraseSuspend => {
                if b == 0x30 {
                    self.state = FlashState::SectorErase;
                    self.erase_alarm_clk = clk as i64 + self.t.erase_sector_cycles as i64;
                }
            }
            FlashState::ByteProgramError | FlashState::Autoselect => {
                if self.magic1(addr) && b == 0xaa {
                    self.state = FlashState::Magic1;
                }
                if b == 0xf0 {
                    self.state = FlashState::Read;
                    self.base_state = FlashState::Read;
                }
            }
            FlashState::ChipErase => {}
        }
    }

    /// ts:779-787 / vice:171-182 — AM29F040: a program can only clear bits (1→0).
    /// Returns false → byte_program_error (a 0→1 was requested).
    fn program_byte_op(&mut self, addr: u32, byte: u8) -> bool {
        let old = *self.data.get(addr as usize).unwrap_or(&0xff);
        let next = old & byte;
        self.program_byte = byte;
        if let Some(slot) = self.data.get_mut(addr as usize) {
            *slot = next;
        }
        self.dirty = true;
        self.generation += 1;
        next == byte
    }
    /// ts:788-793 / vice:152-162 — erase one sector to 0xFF.
    fn erase_sector(&mut self, sector: u32) {
        let start = (sector * self.t.sector_size) as usize;
        let end = (start + self.t.sector_size as usize).min(self.data.len());
        if start < self.data.len() {
            self.data[start..end].fill(0xff);
        }
        self.dirty = true;
        self.generation += 1;
    }
    /// ts:794 / vice:164-169 — erase the whole chip to 0xFF.
    fn erase_chip(&mut self) {
        self.data.fill(0xff);
        self.dirty = true;
        self.generation += 1;
    }
    /// ts:795-798 / vice:145-150.
    fn add_sector_to_erase_mask(&mut self, addr: u32) {
        let s = self.sector_num(addr);
        self.erase_mask[(s >> 3) as usize] |= (1u8 << (s & 7)) & 0xff;
    }

    /// ts:800-808 — snapshot the command-FSM continuation (catches the alarm up
    /// first so the captured state is post-alarm, not stale).
    pub fn snapshot_state(&mut self, clk: u64) -> Flash040SnapState {
        self.catch_up_erase(clk as i64);
        Flash040SnapState {
            state: self.state as u8,
            base_state: self.base_state as u8,
            program_byte: self.program_byte,
            last_read: self.last_read,
            dirty: self.dirty,
            erase_mask: self.erase_mask,
            erase_alarm_clk: self.erase_alarm_clk,
        }
    }
    /// ts:809-817 — restore the command-FSM continuation.
    pub fn restore_state(&mut self, s: &Flash040SnapState) {
        self.state = FlashState::from_index(s.state);
        self.base_state = FlashState::from_index(s.base_state);
        self.program_byte = s.program_byte & 0xff;
        self.last_read = s.last_read & 0xff;
        self.dirty = s.dirty;
        self.erase_mask = s.erase_mask;
        self.erase_alarm_clk = s.erase_alarm_clk;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AM29F040B autoselect: AA→55→90 enters autoselect; $00 reads manufacturer
    /// id, $01 reads device id; F0 returns to read.
    #[test]
    fn autoselect_ids() {
        let mut f = Flash040::new(vec![0u8; 0x80000], "t", FLASH040B);
        f.store(0x555, 0xaa, 0);
        f.store(0x2aa, 0x55, 0);
        f.store(0x555, 0x90, 0);
        assert_eq!(f.read(0x00, 0), 0x01); // manufacturer
        assert_eq!(f.read(0x01, 0), 0xa4); // device
        f.store(0x000, 0xf0, 0); // reset to read
        assert_eq!(f.read(0x00, 0), 0x00); // back to array data
    }

    /// Byte program: AA→55→A0→<addr,data> programs (clearing bits only).
    #[test]
    fn byte_program() {
        let mut f = Flash040::new(vec![0xffu8; 0x80000], "t", FLASH040B);
        f.store(0x555, 0xaa, 0);
        f.store(0x2aa, 0x55, 0);
        f.store(0x555, 0xa0, 0);
        f.store(0x1234, 0x3c, 0); // program 0x3c into 0x1234
        assert_eq!(f.peek(0x1234), 0x3c);
        assert!(f.is_dirty());
    }

    /// Program can only clear bits: programming 0x0f over 0x3c yields 0x0c
    /// (0x3c & 0x0f), and requesting a 1→0... a 0→1 sets the error flag.
    #[test]
    fn program_clears_bits_only() {
        let mut f = Flash040::new(vec![0x3cu8; 0x80000], "t", FLASH040B);
        f.store(0x555, 0xaa, 0);
        f.store(0x2aa, 0x55, 0);
        f.store(0x555, 0xa0, 0);
        f.store(0x10, 0x0f, 0); // 0x3c & 0x0f = 0x0c
        assert_eq!(f.peek(0x10), 0x0c);
    }

    /// Sector erase: AA→55→80→AA→55→30 schedules an erase that the lazy clk
    /// applies once the timeout + erase cycles elapse. Sector 0 = $00000-$0FFFF.
    #[test]
    fn sector_erase_lazy_clk() {
        let mut f = Flash040::new(vec![0x00u8; 0x80000], "t", FLASH040B);
        // command sequence at clk 0
        f.store(0x555, 0xaa, 0);
        f.store(0x2aa, 0x55, 0);
        f.store(0x555, 0x80, 0);
        f.store(0x555, 0xaa, 0);
        f.store(0x2aa, 0x55, 0);
        f.store(0x0000, 0x30, 0); // sector 0 erase armed at clk 0
        // before timeout: still 0x00 (peek = raw array, unchanged)
        assert_eq!(f.peek(0x0000), 0x00);
        // advance well past timeout (50) + sector cycles (1_000_000): a read at
        // this clk catches the alarm up → sector 0 wiped to 0xff.
        let v = f.read(0x4000, 2_000_000);
        // after both alarm steps the chip is back in READ and the sector is FF
        assert_eq!(f.peek(0x0000), 0xff);
        assert_eq!(f.peek(0x0fff0), 0xff);
        // a byte just past the sector is untouched
        assert_eq!(f.peek(0x10000), 0x00);
        let _ = v;
    }

    /// Chip erase: AA→55→80→AA→55→10 wipes the whole array after erase_chip_cycles.
    #[test]
    fn chip_erase_lazy_clk() {
        let mut f = Flash040::new(vec![0x42u8; 0x80000], "t", FLASH040B);
        f.store(0x555, 0xaa, 0);
        f.store(0x2aa, 0x55, 0);
        f.store(0x555, 0x80, 0);
        f.store(0x555, 0xaa, 0);
        f.store(0x2aa, 0x55, 0);
        f.store(0x555, 0x10, 0); // chip erase armed at clk 0
        assert_eq!(f.peek(0x0), 0x42);
        f.read(0x0, 10_000_000); // past erase_chip_cycles (8_000_000)
        assert_eq!(f.peek(0x0), 0xff);
        assert_eq!(f.peek(0x7ffff), 0xff);
    }

    /// Program-then-readback round-trip via the FSM read path.
    #[test]
    fn program_then_readback() {
        let mut f = Flash040::new(vec![0xffu8; 0x80000], "t", FLASH040B);
        for (a, d) in [(0x100u32, 0xa9u8), (0x101, 0x00), (0x102, 0x60)] {
            f.store(0x555, 0xaa, 0);
            f.store(0x2aa, 0x55, 0);
            f.store(0x555, 0xa0, 0);
            f.store(a, d, 0);
        }
        f.store(0, 0xf0, 0); // ensure read state
        assert_eq!(f.read(0x100, 0), 0xa9);
        assert_eq!(f.read(0x101, 0), 0x00);
        assert_eq!(f.read(0x102, 0), 0x60);
    }
}
