//! m93c86.rs — STRICT 1:1 port of the ST M93C86 serial EEPROM. Source of truth
//! (port VERBATIM, `ts:`/`vice:` line tags):
//!
//!   C64ReverseEngineeringMCP/src/runtime/headless/m93c86.ts (itself a
//!     source-faithful port of VICE src/core/m93c86.c).
//!
//! ST M93C86: 16Kbit (2KB) 3-wire MicroWire serial EEPROM, 16-bit organised,
//! used by the GMOD2 cartridge. Driven by three lines — CS (chip select), CLK
//! (clock) and DI (data in) — returns DO (data out). Commands are shifted in
//! MSB-first on the rising clock edge; reads shift out on the rising edge.
//! Write/erase require a prior EWEN (write-enable). The data layout is
//! `m93c86_data[addr << 1]` / `+ 1` (16-bit words split into 2 bytes).

const M93C86_SIZE: usize = 2048;

// command codes (VICE #defines / ts:14-16).
const CMD00: u8 = 1;
const CMDWRITE: u8 = 2;
const CMDREAD: u8 = 3;
const CMDERASE: u8 = 4;
const CMDWEN: u8 = 5;
const CMDWDS: u8 = 6;
const CMDERAL: u8 = 7;
const CMDWRAL: u8 = 8;
const CMDREADDUMMY: u8 = 9;
const CMDREADDATA: u8 = 10;
const CMDISBUSY: u8 = 11;
const CMDISREADY: u8 = 12;
const STATUSREADY: u8 = 1;
const STATUSBUSY: u8 = 0;

/// Persistable M93C86 continuation (= M93c86SnapState, m93c86.ts:19-26). The full
/// 2KB array + the serial state machine are captured.
#[derive(Clone)]
pub struct M93c86SnapState {
    pub data: Vec<u8>,
    pub cs: u8,
    pub clock: u8,
    pub data_in: u8,
    pub data_out: u8,
    pub input_shiftreg: u32,
    pub input_count: u32,
    pub output_shiftreg: u32,
    pub output_count: u32,
    pub command: u8,
    pub addr: u32,
    pub write_enable: u8,
    pub ready_busy: u8,
}

/// ts:28+ — class M93c86.
#[derive(Clone)]
pub struct M93c86 {
    m93c86_data: Vec<u8>, // M93C86_SIZE bytes
    eeprom_cs: u8,
    eeprom_clock: u8,
    eeprom_data_in: u8,
    eeprom_data_out: u8,
    input_shiftreg: u32,
    input_count: u32,
    output_shiftreg: u32,
    output_count: u32,
    command: u8,
    addr: u32,
    write_enable_status: u8,
    ready_busy_status: u8,
    dirty: bool,
    generation: u64,
}

impl Default for M93c86 {
    fn default() -> Self {
        Self::new()
    }
}

impl M93c86 {
    /// ts:43-46 — constructor: blank 2KB (0xFF).
    pub fn new() -> Self {
        M93c86 {
            m93c86_data: vec![0xff; M93C86_SIZE],
            eeprom_cs: 0,
            eeprom_clock: 0,
            eeprom_data_in: 0,
            eeprom_data_out: 0,
            input_shiftreg: 0,
            input_count: 0,
            output_shiftreg: 0,
            output_count: 0,
            command: 0,
            addr: 0,
            write_enable_status: 0,
            ready_busy_status: STATUSREADY,
            dirty: false,
            generation: 0,
        }
    }

    /// ts:48 — the raw 2KB array (for the writable image).
    pub fn get_data(&self) -> &[u8] {
        &self.m93c86_data
    }
    /// ts:49 — load a 2KB image.
    pub fn load_data(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(M93C86_SIZE);
        self.m93c86_data[..n].copy_from_slice(&bytes[..n]);
    }
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
    /// Monotonic mutation counter for an auto-persist debounce (ts:53-54).
    pub fn writable_generation(&self) -> u64 {
        self.generation
    }

    /// ts:56-59 — reset_input_shiftreg.
    fn reset_input_shiftreg(&mut self) {
        self.input_shiftreg = 0;
        self.input_count = 0;
    }

    /// ts:61-77 — read_data: DO. CMDISBUSY/CMDISREADY return the busy/ready status
    /// (one-shot busy, VICE approximation); otherwise the shifted-out data bit.
    pub fn read_data(&mut self) -> u8 {
        if self.eeprom_cs == 1 {
            match self.command {
                CMDISBUSY => {
                    self.command = CMDISREADY;
                    return STATUSBUSY;
                }
                CMDISREADY => {
                    self.ready_busy_status = STATUSREADY;
                    self.command = 0;
                    return STATUSREADY;
                }
                _ => return self.eeprom_data_out,
            }
        }
        0
    }

    /// ts:79-81 — write_data: latch DI (bit 0) while CS asserted.
    pub fn write_data(&mut self, value: u8) {
        if self.eeprom_cs == 1 {
            self.eeprom_data_in = value & 1;
        }
    }

    /// ts:83-102 — write_select: CS line. A 0→1 rising edge (with CLK low) resets
    /// the input shiftreg; a 1→0 falling edge during a WRITE/WRAL/ERAL transitions
    /// to BUSY; dropping CS clears a pending read command.
    pub fn write_select(&mut self, value: u8) {
        let value = value & 1;
        if self.eeprom_cs == 0 && value == 1 && self.eeprom_clock == 0 {
            self.reset_input_shiftreg();
        } else if self.eeprom_cs == 1 && value == 0 {
            if self.command == CMDWRITE || self.command == CMDWRAL || self.command == CMDERAL {
                self.command = CMDISBUSY;
            }
        }
        self.eeprom_cs = value;
        if self.eeprom_cs == 0
            && (self.command == CMDREAD
                || self.command == CMDREADDUMMY
                || self.command == CMDREADDATA)
        {
            self.command = 0;
        }
    }

    /// ts:104-236 — write_clock: the rising-clock-edge serial state machine.
    pub fn write_clock(&mut self, value: u8) {
        let value = value & 1;
        if self.eeprom_cs == 1 && value == 1 && self.eeprom_clock == 0 {
            if self.command == CMDREADDUMMY {
                self.output_shiftreg = self.m93c86_data[(self.addr << 1) as usize] as u32;
                self.eeprom_data_out = 0;
                self.output_count = 0;
                self.eeprom_data_out = ((self.output_shiftreg >> 7) & 1) as u8;
                self.output_shiftreg <<= 1;
                self.output_count += 1;
                self.command = CMDREADDATA;
            } else if self.command == CMDREADDATA {
                self.eeprom_data_out = ((self.output_shiftreg >> 7) & 1) as u8;
                self.output_shiftreg <<= 1;
                self.output_count += 1;
                match self.output_count {
                    8 => {
                        self.output_shiftreg =
                            self.m93c86_data[((self.addr << 1) + 1) as usize] as u32;
                    }
                    16 => {
                        self.addr = (self.addr + 1) & ((M93C86_SIZE as u32 / 2) - 1);
                        self.output_shiftreg = self.m93c86_data[(self.addr << 1) as usize] as u32;
                        self.output_count = 0;
                    }
                    _ => {}
                }
            } else {
                self.input_shiftreg = (self.input_shiftreg << 1) | self.eeprom_data_in as u32;
                self.input_count += 1;
                match self.input_count {
                    1 => {
                        // start bit
                        if self.eeprom_data_in == 0 {
                            self.reset_input_shiftreg();
                        }
                    }
                    3 => {
                        // 2 command bits received
                        match self.input_shiftreg {
                            0x04 => self.command = CMD00,   // 100
                            0x05 => self.command = CMDWRITE, // 101
                            0x06 => self.command = CMDREAD,  // 110
                            0x07 => self.command = CMDERASE, // 111
                            _ => {}
                        }
                    }
                    5 => {
                        // 5 command bits received (CMD00 sub-opcodes)
                        if self.command == CMD00 {
                            match self.input_shiftreg {
                                0x10 => self.command = CMDWDS,  // 10000
                                0x11 => self.command = CMDWRAL, // 10001
                                0x12 => self.command = CMDERAL, // 10010
                                0x13 => {
                                    self.command = CMDWEN;
                                    self.write_enable_status = 1;
                                } // 10011
                                _ => {}
                            }
                        }
                    }
                    13 => match self.command {
                        CMDREAD => {
                            self.command = CMDREADDUMMY;
                            self.addr = self.input_shiftreg & 0x3ff;
                            self.reset_input_shiftreg();
                        }
                        CMDWDS => {
                            self.write_enable_status = 0;
                            self.reset_input_shiftreg();
                            self.command = 0;
                        }
                        CMDWEN => {
                            self.write_enable_status = 1;
                            self.reset_input_shiftreg();
                            self.command = 0;
                        }
                        CMDERASE => {
                            if self.write_enable_status == 0 {
                                self.reset_input_shiftreg();
                                self.command = 0;
                            } else {
                                self.addr = self.input_shiftreg & 0x3ff;
                                self.ready_busy_status = STATUSBUSY;
                                self.reset_input_shiftreg();
                                self.m93c86_data[(self.addr << 1) as usize] = 0xff;
                                self.m93c86_data[((self.addr << 1) + 1) as usize] = 0xff;
                                self.dirty = true;
                                self.generation += 1;
                            }
                        }
                        CMDERAL => {
                            if self.write_enable_status == 0 {
                                self.reset_input_shiftreg();
                                self.command = 0;
                            } else {
                                self.ready_busy_status = STATUSBUSY;
                                self.reset_input_shiftreg();
                                self.m93c86_data.fill(0xff);
                                self.dirty = true;
                                self.generation += 1;
                            }
                        }
                        _ => {}
                    },
                    29 => match self.command {
                        CMDWRITE => {
                            if self.write_enable_status == 0 {
                                self.reset_input_shiftreg();
                                self.command = 0;
                            } else {
                                self.addr = (self.input_shiftreg >> 16) & 0x3ff;
                                let data0 = ((self.input_shiftreg >> 8) & 0xff) as u8;
                                let data1 = (self.input_shiftreg & 0xff) as u8;
                                self.ready_busy_status = STATUSBUSY;
                                self.reset_input_shiftreg();
                                self.m93c86_data[(self.addr << 1) as usize] = data0;
                                self.m93c86_data[((self.addr << 1) + 1) as usize] = data1;
                                self.dirty = true;
                                self.generation += 1;
                            }
                        }
                        CMDWRAL => {
                            if self.write_enable_status == 0 {
                                self.reset_input_shiftreg();
                                self.command = 0;
                            } else {
                                let data0 = ((self.input_shiftreg >> 8) & 0xff) as u8;
                                let data1 = (self.input_shiftreg & 0xff) as u8;
                                self.ready_busy_status = STATUSBUSY;
                                self.reset_input_shiftreg();
                                for a in 0..(M93C86_SIZE / 2) {
                                    self.m93c86_data[a << 1] = data0;
                                    self.m93c86_data[(a << 1) + 1] = data1;
                                }
                                self.dirty = true;
                                self.generation += 1;
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        self.eeprom_clock = value;
    }

    /// ts:238-248 — snapshot the full chip state.
    pub fn snapshot_state(&self) -> M93c86SnapState {
        M93c86SnapState {
            data: self.m93c86_data.clone(),
            cs: self.eeprom_cs,
            clock: self.eeprom_clock,
            data_in: self.eeprom_data_in,
            data_out: self.eeprom_data_out,
            input_shiftreg: self.input_shiftreg,
            input_count: self.input_count,
            output_shiftreg: self.output_shiftreg,
            output_count: self.output_count,
            command: self.command,
            addr: self.addr,
            write_enable: self.write_enable_status,
            ready_busy: self.ready_busy_status,
        }
    }
    /// ts:250-258 — restore.
    pub fn restore_state(&mut self, s: &M93c86SnapState) {
        let n = s.data.len().min(M93C86_SIZE);
        self.m93c86_data[..n].copy_from_slice(&s.data[..n]);
        self.eeprom_cs = s.cs & 1;
        self.eeprom_clock = s.clock & 1;
        self.eeprom_data_in = s.data_in & 1;
        self.eeprom_data_out = s.data_out & 1;
        self.input_shiftreg = s.input_shiftreg;
        self.input_count = s.input_count;
        self.output_shiftreg = s.output_shiftreg;
        self.output_count = s.output_count;
        self.command = s.command;
        self.addr = s.addr;
        self.write_enable_status = s.write_enable;
        self.ready_busy_status = s.ready_busy;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shift a command's bits MSB-first into the EEPROM via CS/CLK/DI.
    fn shift_bits(e: &mut M93c86, bits: &[u8]) {
        for &bit in bits {
            e.write_data(bit);
            e.write_clock(1);
            e.write_clock(0);
        }
    }

    /// EWEN (write-enable) then WRITE: 16-bit word lands in two bytes.
    #[test]
    fn write_enable_then_write() {
        let mut e = M93c86::new();
        // Assert CS (rising edge resets the input shiftreg).
        e.write_select(1);
        // EWEN = start(1) 00 11 + 9 don't-cares (CMD00 then 10011 at bit5).
        // bits: 1 0 0 1 1 (then to bit 13: pad with anything)
        shift_bits(&mut e, &[1, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
        // Deassert + reassert CS to start a new command.
        e.write_select(0);
        e.write_select(1);
        // WRITE: start(1) 01 (CMDWRITE) + 10 address bits + 16 data bits = 29 clocks.
        // addr = 0x005, data = 0xA53C → word.
        let mut bits = vec![1u8, 0, 1]; // start + command 01
        // 10-bit address = 0x005 = 00 0000 0101
        for i in (0..10).rev() {
            bits.push(((0x005u32 >> i) & 1) as u8);
        }
        // 16-bit data = 0xA53C
        for i in (0..16).rev() {
            bits.push(((0xA53Cu32 >> i) & 1) as u8);
        }
        assert_eq!(bits.len(), 29);
        shift_bits(&mut e, &bits);
        // Falling CS commits → BUSY; the bytes are already stored on the 29th clock.
        e.write_select(0);
        assert_eq!(e.get_data()[0x005 << 1], 0xA5);
        assert_eq!(e.get_data()[(0x005 << 1) + 1], 0x3C);
        assert!(e.is_dirty());
    }

    /// Without EWEN, a WRITE is ignored (write_enable_status == 0).
    #[test]
    fn write_without_enable_ignored() {
        let mut e = M93c86::new();
        e.write_select(1);
        let mut bits = vec![1u8, 0, 1];
        for i in (0..10).rev() {
            bits.push(((0x000u32 >> i) & 1) as u8);
        }
        for i in (0..16).rev() {
            bits.push(((0x1234u32 >> i) & 1) as u8);
        }
        shift_bits(&mut e, &bits);
        e.write_select(0);
        // Unchanged 0xFF blank.
        assert_eq!(e.get_data()[0], 0xff);
        assert_eq!(e.get_data()[1], 0xff);
        assert!(!e.is_dirty());
    }

    /// READ shifts the stored 16-bit word out MSB-first on DO.
    #[test]
    fn read_word() {
        let mut e = M93c86::new();
        // Seed addr 0x002 with 0x8142.
        e.load_data(&{
            let mut d = vec![0xffu8; M93C86_SIZE];
            d[0x002 << 1] = 0x81;
            d[(0x002 << 1) + 1] = 0x42;
            d
        });
        e.write_select(1);
        // READ: start(1) 10 (CMDREAD) + 10-bit addr = 13 clocks → CMDREADDUMMY.
        let mut bits = vec![1u8, 1, 0];
        for i in (0..10).rev() {
            bits.push(((0x002u32 >> i) & 1) as u8);
        }
        assert_eq!(bits.len(), 13);
        shift_bits(&mut e, &bits);
        // First clock after the address: dummy → loads first byte, emits MSB.
        // Now clock out 16 bits and reassemble.
        let mut word: u32 = 0;
        for _ in 0..16 {
            e.write_clock(1);
            let bit = e.read_data() & 1;
            word = (word << 1) | bit as u32;
            e.write_clock(0);
        }
        assert_eq!(word, 0x8142);
    }

    /// ERASE wipes a word to 0xFFFF (requires EWEN).
    #[test]
    fn erase_word() {
        let mut e = M93c86::new();
        e.load_data(&vec![0x00u8; M93C86_SIZE]);
        // EWEN
        e.write_select(1);
        shift_bits(&mut e, &[1, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
        e.write_select(0);
        e.write_select(1);
        // ERASE: start(1) 11 (CMDERASE) + 10-bit addr 0x007 = 13 clocks.
        let mut bits = vec![1u8, 1, 1];
        for i in (0..10).rev() {
            bits.push(((0x007u32 >> i) & 1) as u8);
        }
        shift_bits(&mut e, &bits);
        e.write_select(0);
        assert_eq!(e.get_data()[0x007 << 1], 0xff);
        assert_eq!(e.get_data()[(0x007 << 1) + 1], 0xff);
    }
}
