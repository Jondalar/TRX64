//! spi_flash.rs — serial (SPI) flash device, as used by the GMod3 / GMod4 cartridges.
//!
//! Port of VICE's `src/core/spi-flash.c` (trunk) **plus** the extensions in the GMod4
//! patch: the `flash_types[]` identity table, the `$90` READ_ID command, and the ESMT
//! quirk of repeating the ID bytes. Reference wikis: <http://wiki.icomp.de/wiki/GMod3>,
//! <http://wiki.icomp.de/wiki/GMod4>.
//!
//! WHY THIS EXISTS: our other cartridges use `Flash040`, which models a *parallel* flash
//! (`$AAA/$555` unlock sequences, DQ status polling, erase alarms). An SPI flash is a
//! different device class entirely — the host bit-bangs CS / CLK / DI and reads DO, one
//! bit per clock edge, and command + address + data all arrive through that one wire.
//! GMod3 and GMod4 both need it, which is why this is its own module rather than part of
//! either mapper.
//!
//! DIFFERENCE FROM THE C: VICE keeps this state in file-level statics, so one process has
//! exactly one SPI flash. Here it is a plain struct owned by the mapper — same behaviour,
//! but a machine can hold more than one and snapshots serialise it with the cart.
//!
//! The command set is deliberately the same small one VICE implements: enough for the
//! vendor's own example programs (identify / read / write / erase), not a complete SPI
//! flash. Unknown commands are ignored rather than guessed at — see `unknown_commands`.

/// Flash device identities (`flash_types[]` in the GMod4 patch). The order matters only
/// for readability here; unlike the C we key off the enum, not an index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpiFlashType {
    /// EN25QH16A (ESMT) — GMod3, 2 MiB.
    En25qh16a,
    /// EN25QH32A (ESMT) — GMod3, 4 MiB.
    En25qh32a,
    /// EN25QH64A (ESMT) — GMod3 + GMod4, 8 MiB.
    En25qh64a,
    /// EN25QH128A (ESMT) — GMod3 + GMod4, 16 MiB.
    En25qh128a,
    /// W25Q64CV (Winbond) — GMod4, 8 MiB.
    W25q64cv,
    /// ZD25Q32C (Zetta) — GMod4, 4 MiB.
    Zd25q32c,
    /// ZD25Q64B (Zetta) — GMod4, 8 MiB.
    Zd25q64b,
    /// ZD25Q128C (Zetta) — 16 MiB.
    Zd25q128c,
}

/// The four identity bytes a device answers with (`flash_types_t`).
#[derive(Clone, Copy)]
pub struct SpiFlashIdent {
    pub manufacturer_id: u8,
    pub device_id: u8,
    pub memory_type: u8,
    pub memory_capacity: u8,
}

impl SpiFlashType {
    /// `flash_types[]` verbatim from the patch.
    pub fn ident(self) -> SpiFlashIdent {
        let (m, d, t, c) = match self {
            SpiFlashType::En25qh16a => (0x1c, 0x14, 0x70, 0x15),
            SpiFlashType::En25qh32a => (0x1c, 0x15, 0x70, 0x16),
            SpiFlashType::En25qh64a => (0x1c, 0x16, 0x70, 0x17),
            SpiFlashType::En25qh128a => (0x1c, 0x17, 0x70, 0x18),
            SpiFlashType::W25q64cv => (0xef, 0x16, 0x40, 0x17),
            SpiFlashType::Zd25q32c => (0xba, 0x15, 0x60, 0x16),
            SpiFlashType::Zd25q64b => (0xba, 0x16, 0x32, 0x17),
            SpiFlashType::Zd25q128c => (0xba, 0x17, 0x40, 0x18),
        };
        SpiFlashIdent { manufacturer_id: m, device_id: d, memory_type: t, memory_capacity: c }
    }

    /// Capacity in bytes, derived from the capacity ID exactly as `spi_flash_set_image`
    /// does (`0x15`→2 MiB … `0x18`→16 MiB).
    pub fn size(self) -> u32 {
        match self.ident().memory_capacity {
            0x15 => 2 * 1024 * 1024,
            0x16 => 4 * 1024 * 1024,
            0x17 => 8 * 1024 * 1024,
            _ => 16 * 1024 * 1024,
        }
    }

    /// ESMT parts keep re-sending the ID bytes while CS stays low; the others stop after
    /// one round. (`if (flash_types[..].manufacturer_id == 0x1c)` in the patch.)
    fn repeats_id(self) -> bool {
        self.ident().manufacturer_id == 0x1c
    }
}

// Command opcodes (`FLASH_CMD_*`). `$66`/`$99` (reset enable/reset) and `$ff` (disable
// QPI) appear in the vendor's headers but are not implemented upstream either.
const CMD_PAGE_PROGRAM: u32 = 0x02;
const CMD_READ_DATA: u32 = 0x03;
const CMD_READ_STATUS: u32 = 0x05;
const CMD_WRITE_ENABLE: u32 = 0x06;
const CMD_BLOCK_ERASE: u32 = 0xd8; // 64 KiB blocks
const CMD_READ_ID: u32 = 0x90;
const CMD_REMS: u32 = 0x9f; // JEDEC ID

/// `STATUSBUSY` in the C — used as a sentinel "no command pending", not as a real opcode.
const STATUS_BUSY: u32 = 0;

/// One SPI flash device, bit-banged over CS / CLK / DI / DO.
#[derive(Clone)]
pub struct SpiFlash {
    data: Vec<u8>,
    size: u32,
    flash_type: SpiFlashType,

    cs: u8,    // chip select, ACTIVE LOW (1 = deselected)
    clock: u8, // last clock level, for edge detection
    data_in: u8,
    data_out: u8,

    input_shiftreg: u32,
    input_count: u32,
    output_shiftreg: u32,
    output_count: u32,

    command: u32,
    addr: u32,

    write_enable_status: u8,
    ready_busy_status: u8,

    /// Set once the image has been programmed or erased, so a caller can decide whether
    /// the backing file needs writing back.
    dirty: bool,
    /// Opcodes seen that we do not implement. VICE logs and drops them; we count them so a
    /// test can assert the vendor examples never hit this path.
    pub unknown_commands: u32,
}

impl SpiFlash {
    /// `spi_flash_set_image` — the image is padded/truncated to the device's capacity so
    /// the `addr & (size - 1)` masking below is always in bounds.
    pub fn new(mut data: Vec<u8>, flash_type: SpiFlashType) -> Self {
        let size = flash_type.size();
        data.resize(size as usize, 0xff);
        SpiFlash {
            data,
            size,
            flash_type,
            cs: 1,
            clock: 0,
            data_in: 0,
            data_out: 0,
            input_shiftreg: 0,
            input_count: 0,
            output_shiftreg: 0,
            output_count: 0,
            command: STATUS_BUSY,
            addr: 0,
            write_enable_status: 0,
            ready_busy_status: 1,
            dirty: false,
            unknown_commands: 0,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
    pub fn image(&self) -> &[u8] {
        &self.data
    }
    pub fn flash_type(&self) -> SpiFlashType {
        self.flash_type
    }

    /// `spi_flash_read_data` — the DO line, valid only while selected.
    pub fn read_data(&self) -> u8 {
        if self.cs == 0 {
            self.data_out
        } else {
            0
        }
    }

    /// `spi_flash_write_data` — the DI line. Ignored while deselected.
    pub fn write_data(&mut self, value: u8) {
        if self.cs == 0 {
            self.data_in = value;
        }
    }

    /// `spi_flash_write_select`. CS is active low: 1→0 begins an instruction (both shift
    /// registers reset), 0→1 ends it and is where the deferred commands actually execute.
    pub fn write_select(&mut self, value: u8) {
        if self.cs == 1 && value == 0 {
            self.reset_input();
            self.reset_output();
        } else if self.cs == 0 && value == 1 {
            match self.command {
                CMD_REMS | CMD_READ_ID | CMD_READ_STATUS => {}
                CMD_BLOCK_ERASE => {
                    // The 64 KiB block is selected by the top address byte only.
                    let addr = (self.input_shiftreg & 0x00ff_0000) & (self.size - 1);
                    let end = (addr as usize + 0x1_0000).min(self.data.len());
                    self.data[addr as usize..end].fill(0xff);
                    self.addr = addr;
                    self.dirty = true;
                    self.command = STATUS_BUSY;
                }
                CMD_WRITE_ENABLE => {
                    self.write_enable_status = 1;
                }
                CMD_PAGE_PROGRAM | CMD_READ_DATA => {
                    self.command = STATUS_BUSY;
                }
                _ => {
                    self.unknown_commands = self.unknown_commands.wrapping_add(1);
                }
            }
        }
        self.cs = value;
    }

    /// `spi_flash_write_clock` — everything happens on the RISING edge while selected:
    /// one bit is shifted in, the byte/word boundaries drive the command FSM, and finally
    /// one bit is shifted out onto DO.
    pub fn write_clock(&mut self, value: u8) {
        if self.cs == 0 && value == 1 && self.clock == 0 {
            self.shift_input();
            match self.input_count {
                8 => self.on_byte_1(),
                32 => self.on_byte_4(),
                _ => {} // 16 / 24 are address bytes; the C only logs them
            }
            self.shift_output();
        }
        self.clock = value;
    }

    /// First byte: either a data byte for an in-flight PROGRAM/READ, or the opcode.
    fn on_byte_1(&mut self) {
        match self.command {
            CMD_PAGE_PROGRAM => {
                self.addr &= self.size - 1;
                // Flash programming can only clear bits — hence AND, not assignment.
                // Erasing (which sets them back to 1) is the only way to raise a bit.
                self.data[self.addr as usize] &= self.input_shiftreg as u8;
                self.addr = self.addr.wrapping_add(1);
                self.dirty = true;
                self.reset_input();
            }
            CMD_READ_DATA => {
                self.addr &= self.size - 1;
                self.output_shiftreg = (self.data[self.addr as usize] as u32) << 24;
                self.output_count = 8;
                self.addr = self.addr.wrapping_add(1);
                self.reset_input();
            }
            _ => match self.input_shiftreg {
                CMD_REMS => self.command = CMD_REMS,
                CMD_READ_ID => self.command = CMD_READ_ID,
                CMD_READ_STATUS => {
                    self.command = CMD_READ_STATUS;
                    // Status is answerable without a deselect first.
                    self.output_shiftreg = 0x0100_0000;
                    self.output_count = 8;
                }
                CMD_BLOCK_ERASE => self.command = CMD_BLOCK_ERASE,
                CMD_WRITE_ENABLE => self.command = CMD_WRITE_ENABLE,
                CMD_PAGE_PROGRAM => self.command = CMD_PAGE_PROGRAM,
                CMD_READ_DATA => self.command = CMD_READ_DATA,
                _ => {
                    self.unknown_commands = self.unknown_commands.wrapping_add(1);
                    self.reset_input();
                }
            },
        }
    }

    /// Fourth byte: opcode + 24-bit address is complete.
    fn on_byte_4(&mut self) {
        match self.command {
            CMD_REMS => {
                self.start_cmd_9f();
                if self.flash_type.repeats_id() {
                    // ESMT: stay in REMS so the host can keep clocking ID bytes out.
                    self.input_shiftreg = CMD_REMS;
                    self.input_count = 8;
                } else {
                    self.reset_input();
                }
            }
            CMD_READ_ID => {
                self.start_cmd_90();
                self.reset_input();
            }
            CMD_BLOCK_ERASE => { /* address is consumed at deselect */ }
            CMD_PAGE_PROGRAM => {
                self.addr = self.input_shiftreg & (self.size - 1);
                self.reset_input();
            }
            CMD_READ_DATA => {
                self.addr = self.input_shiftreg & (self.size - 1);
                self.output_shiftreg = (self.data[self.addr as usize] as u32) << 24;
                self.output_count = 8;
                self.addr = self.addr.wrapping_add(1);
                self.reset_input();
            }
            _ => {
                self.unknown_commands = self.unknown_commands.wrapping_add(1);
                self.reset_input();
            }
        }
    }

    /// `start_cmd_9f` — JEDEC ID: manufacturer, memory type, capacity.
    fn start_cmd_9f(&mut self) {
        let id = self.flash_type.ident();
        self.command = CMD_REMS;
        self.output_shiftreg = ((id.manufacturer_id as u32) << 24)
            | ((id.memory_type as u32) << 16)
            | ((id.memory_capacity as u32) << 8);
        self.output_count = 3 * 8;
    }

    /// `start_cmd_90` — READ_ID: manufacturer + device.
    fn start_cmd_90(&mut self) {
        let id = self.flash_type.ident();
        self.command = CMD_READ_ID;
        self.output_shiftreg =
            ((id.manufacturer_id as u32) << 24) | ((id.device_id as u32) << 16);
        self.output_count = 2 * 8;
    }

    fn reset_input(&mut self) {
        self.input_shiftreg = 0;
        self.input_count = 0;
    }
    fn reset_output(&mut self) {
        self.output_shiftreg = 0;
        self.output_count = 0;
    }
    fn shift_input(&mut self) {
        self.input_shiftreg = (self.input_shiftreg << 1) | (self.data_in as u32 & 1);
        self.input_count += 1;
    }
    fn shift_output(&mut self) {
        if self.output_count > 0 {
            self.data_out = ((self.output_shiftreg >> 31) & 1) as u8;
            self.output_shiftreg <<= 1;
            self.output_count -= 1;
        } else {
            self.data_out = 0;
        }
    }

    /// Serialisable state (everything except the image, which the cart snapshot carries).
    pub fn snap_state(&self) -> SpiFlashSnapState {
        SpiFlashSnapState {
            cs: self.cs,
            clock: self.clock,
            data_in: self.data_in,
            data_out: self.data_out,
            input_shiftreg: self.input_shiftreg,
            input_count: self.input_count,
            output_shiftreg: self.output_shiftreg,
            output_count: self.output_count,
            command: self.command,
            addr: self.addr,
            write_enable_status: self.write_enable_status,
            ready_busy_status: self.ready_busy_status,
            dirty: self.dirty,
        }
    }

    pub fn restore_snap_state(&mut self, s: &SpiFlashSnapState) {
        self.cs = s.cs;
        self.clock = s.clock;
        self.data_in = s.data_in;
        self.data_out = s.data_out;
        self.input_shiftreg = s.input_shiftreg;
        self.input_count = s.input_count;
        self.output_shiftreg = s.output_shiftreg;
        self.output_count = s.output_count;
        self.command = s.command;
        self.addr = s.addr;
        self.write_enable_status = s.write_enable_status;
        self.ready_busy_status = s.ready_busy_status;
        self.dirty = s.dirty;
    }
}

/// The SPI FSM state a snapshot must carry (VICE's `EN25QH128A` snapshot module).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpiFlashSnapState {
    pub cs: u8,
    pub clock: u8,
    pub data_in: u8,
    pub data_out: u8,
    pub input_shiftreg: u32,
    pub input_count: u32,
    pub output_shiftreg: u32,
    pub output_count: u32,
    pub command: u32,
    pub addr: u32,
    pub write_enable_status: u8,
    pub ready_busy_status: u8,
    pub dirty: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clock one byte in, MSB first, and collect the byte shifted out alongside it.
    ///
    /// The phase here is not arbitrary — it mirrors the vendor's own `spi_read_byte`
    /// (`lib/gmod4.asm`), which comments *"the flash shifts out a bit on the falling edge
    /// of CLK"* and does **clock 1→0, read the bit, clock 0→1**. The device only acts on
    /// the rising edge, so the bit the host samples is the one placed on DO by the
    /// PREVIOUS rising edge. A consequence worth knowing: the first data bit of a read
    /// appears during the last clock of the *address* phase, not the first clock of the
    /// data phase. Sampling after the rising edge instead reads everything shifted by one.
    fn xfer(f: &mut SpiFlash, byte: u8) -> u8 {
        let mut out = 0u8;
        for i in (0..8).rev() {
            f.write_clock(0);
            out = (out << 1) | f.read_data();
            f.write_data((byte >> i) & 1);
            f.write_clock(1);
        }
        out
    }

    fn dev(t: SpiFlashType) -> SpiFlash {
        SpiFlash::new(vec![0xff; t.size() as usize], t)
    }

    #[test]
    fn jedec_id_matches_the_datasheet_triplet() {
        // $9F returns manufacturer / memory type / capacity. Checked for one part of each
        // vendor, because the GMod4 board ships with several.
        for (t, want) in [
            (SpiFlashType::W25q64cv, [0xef, 0x40, 0x17]),
            (SpiFlashType::En25qh64a, [0x1c, 0x70, 0x17]),
            (SpiFlashType::Zd25q64b, [0xba, 0x32, 0x17]),
        ] {
            let mut f = dev(t);
            f.write_select(0);
            xfer(&mut f, 0x9f);
            xfer(&mut f, 0); // 3 dummy address bytes complete the 32-bit frame
            xfer(&mut f, 0);
            xfer(&mut f, 0);
            let got = [xfer(&mut f, 0), xfer(&mut f, 0), xfer(&mut f, 0)];
            assert_eq!(got, want, "JEDEC ID for {t:?}");
            f.write_select(1);
        }
    }

    #[test]
    fn read_id_returns_manufacturer_and_device() {
        let mut f = dev(SpiFlashType::W25q64cv);
        f.write_select(0);
        xfer(&mut f, 0x90);
        xfer(&mut f, 0);
        xfer(&mut f, 0);
        xfer(&mut f, 0);
        assert_eq!([xfer(&mut f, 0), xfer(&mut f, 0)], [0xef, 0x16]);
        f.write_select(1);
    }

    #[test]
    fn read_data_streams_consecutive_bytes() {
        let mut f = dev(SpiFlashType::W25q64cv);
        f.data[0x1234] = 0xa5;
        f.data[0x1235] = 0x5a;
        f.write_select(0);
        xfer(&mut f, 0x03);
        xfer(&mut f, 0x00);
        xfer(&mut f, 0x12);
        xfer(&mut f, 0x34);
        assert_eq!(xfer(&mut f, 0), 0xa5);
        assert_eq!(xfer(&mut f, 0), 0x5a, "address auto-increments");
        f.write_select(1);
    }

    #[test]
    fn page_program_only_clears_bits() {
        // Flash cannot raise a bit — programming ANDs. This is the single most
        // consequential detail in the whole device: a port that assigns instead of
        // ANDing looks right until software programs the same page twice.
        let mut f = dev(SpiFlashType::W25q64cv);
        f.data[0x40] = 0b1111_0000;
        f.write_select(0);
        xfer(&mut f, 0x02);
        xfer(&mut f, 0x00);
        xfer(&mut f, 0x00);
        xfer(&mut f, 0x40);
        xfer(&mut f, 0b1010_1010);
        f.write_select(1);
        assert_eq!(f.data[0x40], 0b1010_0000);
        assert!(f.is_dirty());
    }

    #[test]
    fn block_erase_clears_64k_back_to_ff() {
        let mut f = dev(SpiFlashType::W25q64cv);
        f.data[0x2_0000] = 0x00;
        f.data[0x2_ffff] = 0x00;
        f.data[0x3_0000] = 0x00; // the next block must survive
        f.write_select(0);
        xfer(&mut f, 0xd8);
        xfer(&mut f, 0x02);
        xfer(&mut f, 0x00);
        xfer(&mut f, 0x00);
        f.write_select(1);
        assert_eq!(f.data[0x2_0000], 0xff);
        assert_eq!(f.data[0x2_ffff], 0xff);
        assert_eq!(f.data[0x3_0000], 0x00, "erase must not run past its 64 KiB block");
    }

    #[test]
    fn esmt_parts_repeat_the_id_others_do_not() {
        // ESMT keeps streaming the triplet while CS stays low; Winbond stops.
        let mut esmt = dev(SpiFlashType::En25qh64a);
        esmt.write_select(0);
        xfer(&mut esmt, 0x9f);
        for _ in 0..3 {
            xfer(&mut esmt, 0);
        }
        let first = [xfer(&mut esmt, 0), xfer(&mut esmt, 0), xfer(&mut esmt, 0)];
        let second = [xfer(&mut esmt, 0), xfer(&mut esmt, 0), xfer(&mut esmt, 0)];
        assert_eq!(first, second, "ESMT repeats");

        let mut wb = dev(SpiFlashType::W25q64cv);
        wb.write_select(0);
        xfer(&mut wb, 0x9f);
        for _ in 0..3 {
            xfer(&mut wb, 0);
        }
        let first = [xfer(&mut wb, 0), xfer(&mut wb, 0), xfer(&mut wb, 0)];
        let second = [xfer(&mut wb, 0), xfer(&mut wb, 0), xfer(&mut wb, 0)];
        assert_ne!(first, second, "Winbond stops after one round");
    }

    #[test]
    fn deselected_device_drives_nothing() {
        let mut f = dev(SpiFlashType::W25q64cv);
        assert_eq!(f.read_data(), 0, "DO is released while CS is high");
        f.write_data(1);
        f.write_clock(1);
        assert_eq!(f.input_count, 0, "clocks are ignored while deselected");
    }

    #[test]
    fn sizes_follow_the_capacity_id() {
        assert_eq!(SpiFlashType::En25qh16a.size(), 2 * 1024 * 1024);
        assert_eq!(SpiFlashType::Zd25q32c.size(), 4 * 1024 * 1024);
        assert_eq!(SpiFlashType::W25q64cv.size(), 8 * 1024 * 1024);
        assert_eq!(SpiFlashType::Zd25q128c.size(), 16 * 1024 * 1024);
    }

    #[test]
    fn snapshot_state_round_trips() {
        let mut f = dev(SpiFlashType::W25q64cv);
        f.write_select(0);
        xfer(&mut f, 0x03);
        xfer(&mut f, 0x00);
        xfer(&mut f, 0x00);
        xfer(&mut f, 0x10);
        let s = f.snap_state();
        let mut g = dev(SpiFlashType::W25q64cv);
        g.restore_snap_state(&s);
        assert_eq!(g.snap_state(), s);
        assert_eq!(g.addr, f.addr, "an in-flight read must resume at the same address");
    }
}
