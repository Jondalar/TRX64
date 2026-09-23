//! wd177x.rs — the 1581's floppy controller (Spec 872 D2): a 1:1 port of VICE
//! `drive/iec/wd1770.c`, with the mechanism (`fdd.rs`) under it.
//!
//! It runs lazily against the drive clock, as VICE's does: `execute` catches the
//! microcode up to `*cpu_clk_ptr` on every register read or write. Nothing is clocked
//! between accesses; the elapsed time is turned into bytes under the head
//! (`fdd.rotate`) when the controller next looks.
//!
//! The board is the standard one (Spec 872 §10.1): a **WD1772** (`is1772 = 1`), MFM
//! (`/DDEN` tied low, `dden = 0`), clocked at 2 MHz (`clock_frequency = 2`). Neither
//! INTRQ nor DRQ reaches the CPU: the DOS polls the status register.
//!
//! **Timing** in drive cycles at 2 MHz, as ported (`wd1770.c:54-63`):
//! byte 64 (32 µs), command accept → BUSY 48, step 12 000 / 24 000 / 4 000 / 6 000 for
//! r1r0 = 0..3 (the 1772 table), head settle 60 000, spin-up 6 index pulses, motor-off
//! 10, verify gives up after 6, type II after 5, READ ADDRESS after 6, lost data on a
//! write 9 bytes after DRQ.

use crate::fdd::{fdd_crc, Fdd};

/// wd1770.c:54-57 `wd1770_step_rate[is1772][r1r0]`, in µs × clock_frequency.
const WD1770_STEP_RATE: [[u64; 4]; 2] = [[6000, 12000, 20000, 30000], [6000, 12000, 2000, 3000]];

// wd1770.c:68-106 — registers, command flags, status bits.
const WD_STATUS: u16 = 0;
const WD_TRACK: u16 = 1;
const WD_SECTOR: u16 = 2;

const WD_A: u8 = 0x01;
const WD_R: u8 = 0x03;
const WD_V: u8 = 0x04;
const WD_E: u8 = 0x04;
const WD_H: u8 = 0x08;
const WD_U: u8 = 0x10;
const WD_M: u8 = 0x10;
const WD_I2: u8 = 0x04;
const WD_I3: u8 = 0x08;

pub const WD_MO: u8 = 0x80;
pub const WD_WP: u8 = 0x40;
pub const WD_SU: u8 = 0x20;
pub const WD_RT: u8 = 0x20;
pub const WD_SE: u8 = 0x10;
pub const WD_RNF: u8 = 0x10;
pub const WD_CRC: u8 = 0x08;
pub const WD_T0: u8 = 0x04;
pub const WD_LD: u8 = 0x04;
pub const WD_IP: u8 = 0x02;
pub const WD_DRQ: u8 = 0x02;
pub const WD_BSY: u8 = 0x01;

/// wd1770.c:108-120 `wd_cmd_t`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WdCmd {
    Restore = 0x00,
    Seek = 0x10,
    Step = 0x20,
    StepIn = 0x40,
    StepOut = 0x60,
    ReadSector = 0x80,
    WriteSector = 0xa0,
    ReadAddress = 0xc0,
    ForceInterrupt = 0xd0,
    ReadTrack = 0xe0,
    WriteTrack = 0xf0,
}

impl WdCmd {
    fn from_u8(v: u8) -> Self {
        match v {
            0x10 => WdCmd::Seek,
            0x20 => WdCmd::Step,
            0x40 => WdCmd::StepIn,
            0x60 => WdCmd::StepOut,
            0x80 => WdCmd::ReadSector,
            0xa0 => WdCmd::WriteSector,
            0xc0 => WdCmd::ReadAddress,
            0xd0 => WdCmd::ForceInterrupt,
            0xe0 => WdCmd::ReadTrack,
            0xf0 => WdCmd::WriteTrack,
            _ => WdCmd::Restore,
        }
    }
}

/// wd1770.c:123-139 `wd_commands[11]` — mask, command, type.
const WD_COMMANDS: [(u8, WdCmd, i32); 11] = [
    (0xf0, WdCmd::Restore, 1),
    (0xf0, WdCmd::Seek, 1),
    (0xe0, WdCmd::Step, 1),
    (0xe0, WdCmd::StepIn, 1),
    (0xe0, WdCmd::StepOut, 1),
    (0xe0, WdCmd::ReadSector, 2),
    (0xe0, WdCmd::WriteSector, 2),
    (0xf0, WdCmd::ReadAddress, 3),
    (0xf0, WdCmd::ReadTrack, 3),
    (0xf0, WdCmd::ForceInterrupt, 4),
    (0xf0, WdCmd::WriteTrack, 3),
];

/// `struct wd1770_s`.
#[derive(Clone, Debug)]
pub struct Wd1770 {
    pub myname: String,
    pub data: u8,
    pub track: u8,
    pub sector: u8,
    pub status: u8,
    pub cmd: u8,
    pub crc: u16,
    pub command: WdCmd,
    /// -1 = idle after a type I command, 0 = idle, 1-4 = the command's type.
    pub type_: i32,
    pub fdd: Fdd,
    pub step: i32,
    pub byte_count: i32,
    pub tmp: u32,
    pub direction: i32,
    pub clock_frequency: u64,
    /// The controller's own clock — how far the microcode has run.
    pub clk: u64,
    pub irq: bool,
    pub dden: bool,
    pub sync: bool,
    pub is1772: bool,
    /// Diagnostic, not machine state (in no snapshot): the head position — physical
    /// track and side — of every WRITE TRACK command accepted, in order. What a format
    /// test counts.
    pub write_track_log: Vec<(u8, u8)>,
}

impl Wd1770 {
    /// wd1770.c:161-183 `wd1770d_init` for drive number `mynumber`, with the board's
    /// part: a WD1772 (Spec 872 §10.1; VICE leaves `is1772 = 0`).
    pub fn new(mynumber: u32) -> Self {
        Self {
            myname: format!("WD1770{mynumber}"),
            data: 0,
            track: 0,
            sector: 0,
            status: 0,
            cmd: 0,
            crc: 0,
            command: WdCmd::Restore,
            type_: 0,
            fdd: Fdd::new(4 * mynumber as i32),
            step: 0,
            byte_count: 0,
            tmp: 0,
            direction: 0,
            clock_frequency: 2,
            clk: 0,
            irq: false,
            dden: false,
            sync: false,
            is1772: true,
            write_track_log: Vec::new(),
        }
    }

    #[inline]
    pub fn settling(&self) -> u64 {
        self.clock_frequency * 30000
    }
    #[inline]
    pub fn byte_rate(&self) -> u64 {
        self.clock_frequency * 8000 / 250
    }
    #[inline]
    pub fn step_rate(&self) -> u64 {
        self.clock_frequency * WD1770_STEP_RATE[self.is1772 as usize][(self.cmd & WD_R) as usize]
    }
    #[inline]
    pub fn prepare(&self) -> u64 {
        self.clock_frequency * 24
    }

    /// `drv->clk += fdd_rotate(fdd, (cpu_clk - drv->clk) / BYTE_RATE) * BYTE_RATE`.
    #[inline]
    fn rotate_to(&mut self, cpu_clk: u64) {
        let br = self.byte_rate();
        let bytes = cpu_clk.saturating_sub(self.clk) / br;
        self.clk += self.fdd.rotate(bytes) * br;
    }

    /// wd1770.c:195-706 `wd1770_execute` — run the microcode up to `cpu_clk`.
    pub fn execute(&mut self, cpu_clk: u64) {
        loop {
            // `break` out of a type's switch lands on the command-end tail below;
            // `return` leaves; `continue` re-runs the loop.
            match self.type_ {
                -1 | 0 => {
                    if self.type_ == -1 {
                        self.status &= !(WD_WP | WD_IP | WD_T0);
                        if self.fdd.index() {
                            self.status |= WD_IP;
                        }
                        if self.fdd.track0() {
                            self.status |= WD_T0;
                        }
                        if self.fdd.write_protect() {
                            self.status |= WD_WP;
                        }
                    }
                    if cpu_clk < self.clk + self.prepare() {
                        return;
                    }
                    self.status &= !WD_BSY;
                    self.rotate_to(cpu_clk);
                    if self.fdd.index_count() >= 10 {
                        self.status &= !WD_MO;
                    }
                    if self.cmd & WD_I2 != 0 && self.fdd.index_count() != self.tmp {
                        self.irq = true;
                        self.tmp = self.fdd.index_count();
                    }
                    return;
                }
                1 => match self.type1(cpu_clk) {
                    Flow::Return => return,
                    Flow::Continue => continue,
                    Flow::End => {}
                },
                2 => match self.type2(cpu_clk) {
                    Flow::Return => return,
                    Flow::Continue => continue,
                    Flow::End => {}
                },
                3 => match self.type3(cpu_clk) {
                    Flow::Return => return,
                    Flow::Continue => continue,
                    Flow::End => {}
                },
                4 => {
                    if cpu_clk < self.clk + self.prepare() {
                        return;
                    }
                    self.clk += self.prepare();
                    self.status &= WD_BSY;
                    if self.cmd & WD_I3 != 0 {
                        self.irq = true;
                    }
                    self.fdd.index_count_reset();
                    self.tmp = self.fdd.index_count();
                    self.type_ = if self.status & WD_BSY != 0 { 0 } else { -1 };
                    continue;
                }
                _ => {}
            }
            self.cmd = 0;
            self.irq = true;
            self.fdd.index_count_reset();
        }
    }

    /// The type I microcode (wd1770.c:231-382). `Flow::End` is the switch's `break`.
    fn type1(&mut self, cpu_clk: u64) -> Flow {
        loop {
            match self.step {
                0 => {
                    if cpu_clk < self.clk + self.prepare() {
                        return Flow::Return;
                    }
                    self.clk += self.prepare();
                    self.status |= WD_BSY;
                    self.status &= !(WD_CRC | WD_SE | WD_DRQ);
                    self.irq = false;
                    self.step += 1;
                }
                1 => {
                    if self.cmd & WD_H != 0 || self.status & WD_MO != 0 {
                        self.status |= WD_MO;
                        self.step += 2;
                        return Flow::Continue;
                    }
                    self.status |= WD_MO;
                    self.fdd.index_count_reset();
                    self.step += 1;
                }
                2 => {
                    self.rotate_to(cpu_clk);
                    if self.fdd.index_count() < 6 {
                        return Flow::Return;
                    }
                    self.step += 1;
                }
                3 => {
                    match self.command {
                        WdCmd::Step => {}
                        WdCmd::StepIn => self.direction = 1,
                        WdCmd::StepOut => self.direction = 0,
                        c => {
                            if c == WdCmd::Restore {
                                self.track = 0xff;
                                self.data = 0x00;
                            }
                            self.step += 1;
                            return Flow::Continue;
                        }
                    }
                    self.step = if self.cmd & WD_U != 0 { 5 } else { 6 };
                    return Flow::Continue;
                }
                4 => {
                    if self.data == self.track {
                        self.step = 8;
                        return Flow::Continue;
                    }
                    self.direction = (self.data > self.track) as i32;
                    self.step += 1;
                }
                5 => {
                    self.track = if self.direction != 0 { self.track.wrapping_add(1) } else { self.track.wrapping_sub(1) };
                    self.step += 1;
                }
                6 => {
                    if self.fdd.track0() && self.direction == 0 {
                        self.track = 0;
                        self.step = 8;
                        return Flow::Continue;
                    }
                    self.fdd.seek_pulse(self.direction != 0);
                    self.step += 1;
                }
                7 => {
                    if cpu_clk < self.clk + self.step_rate() {
                        return Flow::Return;
                    }
                    self.clk += self.step_rate();
                    if self.cmd < WdCmd::Step as u8 {
                        self.step = 4;
                        return Flow::Continue;
                    }
                    self.step += 1;
                }
                8 => {
                    if self.cmd & WD_V == 0 {
                        self.type_ = -1;
                        return Flow::End;
                    }
                    self.step += 1;
                }
                9 => {
                    if cpu_clk < self.clk + self.settling() {
                        return Flow::Return;
                    }
                    self.clk += self.settling();
                    self.fdd.index_count_reset();
                    self.sync = false;
                    self.step += 1;
                }
                10 => {
                    if self.fdd.index_count() >= 6 {
                        self.status |= WD_SE;
                        self.type_ = -1;
                        return Flow::End;
                    }
                    if cpu_clk < self.clk + self.byte_rate() {
                        return Flow::Return;
                    }
                    self.clk += self.byte_rate();
                    let res = self.fdd.read();
                    if (!self.dden || res != 0x1fe) && (!self.sync || res != 0xfe) {
                        self.sync = res == 0x1a1;
                        return Flow::Continue;
                    }
                    self.sync = false;
                    self.crc = 0xb230;
                    self.byte_count = 6;
                    self.step += 1;
                }
                11 => {
                    if cpu_clk < self.clk + self.byte_rate() {
                        return Flow::Return;
                    }
                    self.clk += self.byte_rate();
                    let res = self.fdd.read();
                    if self.byte_count == 6 && res != self.track as u16 {
                        self.step -= 1;
                        return Flow::Continue;
                    }
                    self.crc = fdd_crc(self.crc, res as u8);
                    self.byte_count -= 1;
                    if self.byte_count != 0 {
                        return Flow::Continue;
                    }
                    if self.crc != 0 {
                        self.status |= WD_CRC;
                        self.step -= 1;
                        return Flow::Continue;
                    }
                    self.status &= !WD_CRC;
                    self.type_ = -1;
                    return Flow::End;
                }
                _ => return Flow::End,
            }
        }
    }

    /// The type II microcode (wd1770.c:384-593).
    fn type2(&mut self, cpu_clk: u64) -> Flow {
        loop {
            match self.step {
                0 => {
                    if cpu_clk < self.clk + self.prepare() {
                        return Flow::Return;
                    }
                    self.clk += self.prepare();
                    self.status |= WD_BSY;
                    self.status &= !(WD_DRQ | WD_LD | WD_RNF | WD_RT | WD_WP);
                    self.step += 1;
                }
                1 => {
                    if self.cmd & WD_H != 0 || self.status & WD_MO != 0 {
                        self.status |= WD_MO;
                        self.step += 2;
                        return Flow::Continue;
                    }
                    self.status |= WD_MO;
                    self.fdd.index_count_reset();
                    self.step += 1;
                }
                2 => {
                    self.rotate_to(cpu_clk);
                    if self.fdd.index_count() < 6 {
                        return Flow::Return;
                    }
                    self.step += 1;
                }
                3 => {
                    if self.cmd & WD_E == 0 {
                        self.step += 2;
                        return Flow::Continue;
                    }
                    self.step += 1;
                }
                4 => {
                    if cpu_clk < self.clk + self.settling() {
                        return Flow::Return;
                    }
                    self.clk += self.settling();
                    self.step += 1;
                }
                5 => {
                    if self.command == WdCmd::WriteSector && self.fdd.write_protect() {
                        self.status |= WD_WP;
                        self.type_ = 0;
                        return Flow::End;
                    }
                    self.fdd.index_count_reset();
                    self.sync = false;
                    self.step += 1;
                }
                6 => {
                    if self.fdd.index_count() >= 5 {
                        self.status |= WD_RNF;
                        self.type_ = 0;
                        return Flow::End;
                    }
                    if cpu_clk < self.clk + self.byte_rate() {
                        return Flow::Return;
                    }
                    self.clk += self.byte_rate();
                    let res = self.fdd.read();
                    if (!self.dden || res != 0x1fe) && (!self.sync || res != 0xfe) {
                        self.sync = res == 0x1a1;
                        return Flow::Continue;
                    }
                    self.sync = false;
                    self.crc = 0xb230;
                    self.byte_count = 6;
                    self.step += 1;
                }
                7 => {
                    if cpu_clk < self.clk + self.byte_rate() {
                        return Flow::Return;
                    }
                    self.clk += self.byte_rate();
                    let res = self.fdd.read();
                    if self.byte_count == 6 && res != self.track as u16 {
                        self.step -= 1;
                        return Flow::Continue;
                    }
                    if self.byte_count == 4 && res != self.sector as u16 {
                        self.step -= 1;
                        return Flow::Continue;
                    }
                    if self.byte_count == 3 {
                        self.tmp = res as u32;
                    }
                    self.crc = fdd_crc(self.crc, res as u8);
                    self.byte_count -= 1;
                    if self.byte_count != 0 {
                        return Flow::Continue;
                    }
                    if self.crc != 0 {
                        self.status |= WD_CRC;
                        self.step -= 1;
                        return Flow::Continue;
                    }
                    self.status &= !WD_CRC;
                    self.crc = 0xffff;
                    if self.command == WdCmd::WriteSector {
                        self.byte_count = 0;
                        self.step = 10;
                        return Flow::Continue;
                    }
                    self.byte_count = 43;
                    self.step += 1;
                }
                8 => {
                    if cpu_clk < self.clk + self.byte_rate() {
                        return Flow::Return;
                    }
                    let was = self.byte_count;
                    self.byte_count -= 1;
                    if was == 0 {
                        self.step -= 2;
                        return Flow::Continue;
                    }
                    self.clk += self.byte_rate();
                    let res = self.fdd.read();
                    if (!self.dden || (res != 0x1fb && res != 0x1f8))
                        && (!self.sync || (res != 0xfb && res != 0xf8))
                    {
                        if !self.sync {
                            self.crc = 0xffff;
                        }
                        self.crc = fdd_crc(self.crc, res as u8);
                        self.sync = res == 0x1a1;
                        return Flow::Continue;
                    }
                    self.crc = fdd_crc(self.crc, res as u8);
                    if res & 0xff == 0xf8 {
                        self.status |= WD_RT;
                    }
                    self.byte_count = (128 << self.tmp) + 2;
                    self.step += 1;
                }
                9 => {
                    if cpu_clk < self.clk + self.byte_rate() {
                        return Flow::Return;
                    }
                    self.clk += self.byte_rate();
                    let res = self.fdd.read();
                    if self.byte_count > 2 {
                        self.status |= if self.status & WD_DRQ != 0 { WD_LD } else { WD_DRQ };
                        self.data = res as u8;
                    }
                    self.crc = fdd_crc(self.crc, res as u8);
                    self.byte_count -= 1;
                    if self.byte_count != 0 {
                        return Flow::Continue;
                    }
                    if self.crc != 0 {
                        self.status |= WD_CRC;
                        self.type_ = 0;
                        return Flow::End;
                    }
                    if self.cmd & WD_M != 0 {
                        self.sector = self.sector.wrapping_add(1);
                        self.step = 5;
                        return Flow::Continue;
                    }
                    self.type_ = 0;
                    return Flow::End;
                }
                10 => {
                    if cpu_clk < self.clk + self.byte_rate() {
                        return Flow::Return;
                    }
                    self.clk += self.byte_rate();
                    self.byte_count += 1;
                    if self.byte_count == 2 {
                        self.status |= WD_DRQ;
                    }
                    if self.byte_count == 2 + 9 && self.status & WD_DRQ != 0 {
                        self.status ^= WD_DRQ | WD_LD;
                        self.type_ = 0;
                        return Flow::End;
                    }
                    if self.byte_count <= (if self.dden { 0 } else { 11 }) + 2 + 9 {
                        self.fdd.read();
                        return Flow::Continue;
                    }
                    if self.byte_count <= (if self.dden { 6 } else { 11 + 12 }) + 2 + 9 {
                        self.fdd.write(0);
                        return Flow::Continue;
                    }
                    if !self.dden && self.byte_count <= 11 + 12 + 2 + 9 + 3 {
                        self.fdd.write(0x1a1);
                        self.crc = fdd_crc(self.crc, 0xa1);
                        return Flow::Continue;
                    }
                    let res: u16 = (if self.cmd & WD_A != 0 { 0xf8 } else { 0xfb }) | if self.dden { 0x100 } else { 0 };
                    self.fdd.write(res);
                    self.crc = fdd_crc(self.crc, res as u8);
                    self.byte_count = (128 << self.tmp) + 3;
                    self.step += 1;
                }
                11 => {
                    if cpu_clk < self.clk + self.byte_rate() {
                        return Flow::Return;
                    }
                    self.clk += self.byte_rate();
                    self.byte_count -= 1;
                    match self.byte_count {
                        0 => {
                            self.fdd.write(0xff);
                        }
                        1 => {
                            self.fdd.write(self.crc & 0xff);
                            return Flow::Continue;
                        }
                        2 => {
                            self.fdd.write(self.crc >> 8);
                            return Flow::Continue;
                        }
                        _ => {
                            self.status |= if self.status & WD_DRQ != 0 { WD_LD } else { WD_DRQ };
                            self.crc = fdd_crc(self.crc, self.data);
                            self.fdd.write(self.data as u16);
                            self.data = 0;
                            return Flow::Continue;
                        }
                    }
                    if self.cmd & WD_M != 0 {
                        self.sector = self.sector.wrapping_add(1);
                        self.step = 5;
                        return Flow::Continue;
                    }
                    self.type_ = 0;
                    return Flow::End;
                }
                _ => return Flow::End,
            }
        }
    }

    /// The type III microcode (wd1770.c:594-793).
    fn type3(&mut self, cpu_clk: u64) -> Flow {
        loop {
            match self.step {
                0 => {
                    if cpu_clk < self.clk + self.prepare() {
                        return Flow::Return;
                    }
                    self.clk += self.prepare();
                    self.status |= WD_BSY;
                    self.status &= !(WD_DRQ | WD_LD | WD_RNF | WD_CRC);
                    self.step += 1;
                }
                1 => {
                    if self.cmd & WD_H != 0 || self.status & WD_MO != 0 {
                        self.status |= WD_MO;
                        self.step += 2;
                        return Flow::Continue;
                    }
                    self.status |= WD_MO;
                    self.fdd.index_count_reset();
                    self.step += 1;
                }
                2 => {
                    self.rotate_to(cpu_clk);
                    if self.fdd.index_count() < 6 {
                        return Flow::Return;
                    }
                    self.step += 1;
                }
                3 => {
                    if self.cmd & WD_E == 0 {
                        self.step += 2;
                        return Flow::Continue;
                    }
                    self.step += 1;
                }
                4 => {
                    if cpu_clk < self.clk + self.settling() {
                        return Flow::Return;
                    }
                    self.clk += self.settling();
                    self.step += 1;
                }
                5 => {
                    self.fdd.index_count_reset();
                    self.sync = false;
                    self.step += 1;
                    if self.command == WdCmd::WriteTrack {
                        if self.fdd.write_protect() {
                            self.status |= WD_WP;
                            self.type_ = 0;
                            return Flow::End;
                        }
                        self.status |= WD_DRQ;
                        self.byte_count = 3;
                        self.step = 9;
                        return Flow::Continue;
                    }
                    if self.command != WdCmd::ReadTrack {
                        self.step += 1;
                        return Flow::Continue;
                    }
                    // falls through to 6 (READ TRACK)
                }
                6 => {
                    if self.fdd.index_count() < 1 {
                        self.rotate_to(cpu_clk);
                        return Flow::Return;
                    }
                    if self.fdd.index_count() > 1 {
                        self.type_ = 0;
                        return Flow::End;
                    }
                    if cpu_clk < self.clk + self.byte_rate() {
                        return Flow::Return;
                    }
                    self.clk += self.byte_rate();
                    self.data = self.fdd.read() as u8;
                    self.status |= if self.status & WD_DRQ != 0 { WD_LD } else { WD_DRQ };
                    return Flow::Continue;
                }
                7 => {
                    if self.fdd.index_count() >= 6 {
                        self.status |= WD_RNF;
                        self.type_ = 0;
                        return Flow::End;
                    }
                    if cpu_clk < self.clk + self.byte_rate() {
                        return Flow::Return;
                    }
                    self.clk += self.byte_rate();
                    let res = self.fdd.read();
                    if (!self.dden || res != 0x1fe) && (!self.sync || res != 0xfe) {
                        self.sync = res == 0x1a1;
                        return Flow::Continue;
                    }
                    self.crc = 0xb230;
                    self.byte_count = 6;
                    self.step += 1;
                }
                8 => {
                    if cpu_clk < self.clk + self.byte_rate() {
                        return Flow::Return;
                    }
                    self.status |= if self.status & WD_DRQ != 0 { WD_LD } else { WD_DRQ };
                    self.clk += self.byte_rate();
                    self.data = self.fdd.read() as u8;
                    if self.byte_count == 6 {
                        self.sector = self.data;
                    }
                    self.crc = fdd_crc(self.crc, self.data);
                    self.byte_count -= 1;
                    if self.byte_count != 0 {
                        return Flow::Continue;
                    }
                    if self.crc != 0 {
                        self.status |= WD_CRC;
                    }
                    self.type_ = 0;
                    return Flow::End;
                }
                9 => {
                    if cpu_clk < self.clk + self.byte_rate() {
                        return Flow::Return;
                    }
                    self.clk += self.byte_rate();
                    self.fdd.read();
                    self.byte_count -= 1;
                    if self.byte_count != 0 {
                        return Flow::Continue;
                    }
                    if self.status & WD_DRQ != 0 {
                        self.status ^= WD_DRQ | WD_LD;
                        self.type_ = 0;
                        return Flow::End;
                    }
                    self.byte_count = 0;
                    self.tmp = 0;
                    self.step += 1;
                }
                10 => {
                    if self.fdd.index_count() < 1 {
                        self.rotate_to(cpu_clk);
                        return Flow::Return;
                    }
                    if self.fdd.index_count() > 1 {
                        self.status &= !WD_DRQ;
                        self.type_ = 0;
                        return Flow::End;
                    }
                    if cpu_clk < self.clk + self.byte_rate() {
                        return Flow::Return;
                    }
                    self.clk += self.byte_rate();
                    let mut res: u16 = self.data as u16;
                    if self.byte_count != 0 {
                        self.fdd.write(self.crc & 0xff);
                        self.byte_count -= 1;
                    } else {
                        self.status |= if self.status & WD_DRQ != 0 { WD_LD } else { WD_DRQ };
                        if self.dden {
                            match res {
                                0xf7 => {
                                    self.byte_count = 1;
                                    res = self.crc >> 8;
                                    self.tmp = 0;
                                }
                                0xf8 | 0xf9 | 0xfa | 0xfb | 0xfe => {
                                    if self.tmp == 0 {
                                        self.crc = 0xffff;
                                        self.tmp = 1;
                                    }
                                    res |= 0x100;
                                }
                                0xfc => res |= 0x100,
                                _ => {}
                            }
                        } else {
                            match res {
                                0xf5 => {
                                    res = 0x1a1;
                                    if self.tmp == 0 {
                                        self.crc = 0xffff;
                                        self.tmp = 1;
                                    }
                                }
                                0xf6 => res = 0x1c2,
                                0xf7 => {
                                    self.byte_count = 1;
                                    res = self.crc >> 8;
                                    self.tmp = 0;
                                }
                                _ => {}
                            }
                        }
                        if self.tmp != 0 {
                            self.crc = fdd_crc(self.crc, res as u8);
                        }
                        self.fdd.write(res);
                        self.data = 0;
                    }
                    return Flow::Continue;
                }
                _ => return Flow::End,
            }
        }
    }

    /// wd1770.c:709-773 `wd1770_store` — `addr` already `& 3`.
    pub fn store(&mut self, cpu_clk: u64, addr: u16, byte: u8) {
        self.execute(cpu_clk);
        match addr & 3 {
            WD_STATUS => {
                self.cmd = byte;
                let mut i = 0;
                while i < WD_COMMANDS.len() {
                    if WD_COMMANDS[i].1 as u8 == WD_COMMANDS[i].0 & byte {
                        break;
                    }
                    i += 1;
                }
                // The table covers every byte, so `i` is always found.
                let (_, command, type_) = WD_COMMANDS[i.min(WD_COMMANDS.len() - 1)];
                self.command = command;
                self.type_ = type_;
                if command == WdCmd::WriteTrack {
                    self.write_track_log.push((self.fdd.track as u8, self.fdd.head as u8));
                }
                self.rotate_to(cpu_clk);
                self.step = 0;
                self.execute(cpu_clk);
            }
            WD_TRACK => self.track = byte,
            WD_SECTOR => self.sector = byte,
            _ => {
                // WD_DATA
                self.status &= !WD_DRQ;
                self.data = byte;
            }
        }
    }

    /// wd1770.c:775-794 `wd1770_read`.
    pub fn read(&mut self, cpu_clk: u64, addr: u16) -> u8 {
        self.execute(cpu_clk);
        match addr & 3 {
            WD_STATUS => {
                self.irq = false;
                self.status
            }
            WD_TRACK => self.track,
            WD_SECTOR => self.sector,
            _ => {
                self.status &= !WD_DRQ;
                self.data
            }
        }
    }

    /// wd1770.c:797-811 `wd1770_peek` — no side effects.
    pub fn peek(&self, addr: u16) -> u8 {
        match addr & 3 {
            WD_STATUS => self.status,
            WD_TRACK => self.track,
            WD_SECTOR => self.sector,
            _ => self.data,
        }
    }

    /// wd1770.c:820-830 `wd1770_reset`. The mechanism (head, motor, disk) is not reset.
    pub fn reset(&mut self, cpu_clk: u64) {
        self.type_ = 0;
        self.status = 0;
        self.track = 0;
        self.sector = 0;
        self.data = 0;
        self.cmd = 0;
        self.step = -1;
        self.clk = cpu_clk;
    }

    // ── WD1770 1.0 snapshot module (wd1770.c:1014-1128) ──────────────────────

    pub fn snapshot_write_module(&self, s: &mut crate::vice_snapshot_stream::SnapshotT) {
        let mut m = s.module_create(&self.myname, 1, 0);
        s.smw_b(&mut m, self.data);
        s.smw_b(&mut m, self.track);
        s.smw_b(&mut m, self.sector);
        s.smw_b(&mut m, self.status);
        s.smw_b(&mut m, self.cmd);
        s.smw_w(&mut m, self.crc);
        s.smw_b(&mut m, self.command as u8);
        s.smw_dw(&mut m, self.type_ as u32);
        s.smw_dw(&mut m, self.step as u32);
        s.smw_dw(&mut m, self.byte_count as u32);
        s.smw_dw(&mut m, self.tmp);
        s.smw_dw(&mut m, self.direction as u32);
        s.smw_clock(&mut m, self.clk);
        s.smw_b(&mut m, self.irq as u8);
        s.smw_b(&mut m, self.dden as u8);
        s.smw_b(&mut m, self.sync as u8);
        s.smw_b(&mut m, self.is1772 as u8);
        s.module_close(&m);
        self.fdd.snapshot_write_module(s);
    }

    /// Read the module and the FDD module that follows it. VICE compares the minor
    /// against `WD1770_SNAP_MAJOR` (`wd1770.c:1093`); ported with the minor where it
    /// belongs (Spec 872 §6).
    pub fn snapshot_read_module(&mut self, s: &mut crate::vice_snapshot_stream::SnapshotT) -> Result<(), String> {
        let name = self.myname.clone();
        let (m, major, minor) = s.module_open(&name).ok_or_else(|| format!("{name}: module missing"))?;
        if crate::vice_snapshot_stream::snapshot_version_is_bigger(major, minor, 1, 0) {
            return Err(format!("{name}: module version {major}.{minor} is newer than 1.0"));
        }
        macro_rules! rb {
            () => {
                s.smr_b().ok_or_else(|| format!("{name}: truncated"))?
            };
        }
        macro_rules! rdw {
            () => {
                s.smr_dw().ok_or_else(|| format!("{name}: truncated"))?
            };
        }
        self.data = rb!();
        self.track = rb!();
        self.sector = rb!();
        self.status = rb!();
        self.cmd = rb!();
        self.crc = s.smr_w().ok_or_else(|| format!("{name}: truncated"))?;
        self.command = WdCmd::from_u8(rb!());
        self.type_ = rdw!() as i32;
        self.step = rdw!() as i32;
        self.byte_count = rdw!() as i32;
        self.tmp = rdw!();
        self.direction = rdw!() as i32;
        self.clk = s.smr_clock().ok_or_else(|| format!("{name}: truncated"))?;
        self.irq = rb!() != 0;
        self.dden = rb!() != 0;
        self.sync = rb!() != 0;
        self.is1772 = rb!() != 0;
        s.module_close(&m);
        self.fdd.snapshot_read_module(s)
    }
}

/// How a microcode step left the `switch`: `return` from `wd1770_execute`, `continue`
/// its loop, or `break` to the command-end tail.
enum Flow {
    Return,
    Continue,
    End,
}

#[cfg(test)]
mod tests {
    //! Spec 872 §9.9 — the §3 timing table, in drive cycles, as the drive sees it. There
    //! is no VICE oracle: the constants are ported, and these check what they say.
    use super::*;

    const BYTE: u64 = 64;
    const REV: u64 = 400_000;

    /// A D81 whose every byte is its logical track number; mounted, motor on, head at
    /// physical track 0, side 0 (logical side 1).
    fn wd_with_disk(read_only: bool) -> Wd1770 {
        let mut img = vec![0u8; 819_200];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i / 10_240 + 1) as u8;
        }
        let mut wd = Wd1770::new(0);
        wd.fdd.image_attach(img, read_only);
        wd.fdd.set_motor(true);
        wd.reset(0);
        wd
    }

    /// Run `cmd` issued at `at`, polling the status every cycle until `pred` holds;
    /// returns the first cycle it holds.
    fn until(wd: &mut Wd1770, from: u64, limit: u64, mut pred: impl FnMut(&mut Wd1770, u64) -> bool) -> Option<u64> {
        (from..from + limit).find(|&c| pred(wd, c))
    }

    #[test]
    fn the_constants_are_the_2mhz_table() {
        let wd = Wd1770::new(0);
        assert_eq!(wd.byte_rate(), BYTE, "32 µs per byte");
        assert_eq!(wd.prepare(), 48, "command accept → BUSY");
        assert_eq!(wd.settling(), 60_000, "30 ms settle");
        assert!(wd.is1772, "the fitted part");
        let mut wd = wd;
        for (r, want) in [(0u8, 12_000u64), (1, 24_000), (2, 4_000), (3, 6_000)] {
            wd.cmd = r;
            assert_eq!(wd.step_rate(), want, "r1r0 = {r}");
        }
        assert_eq!(wd.fdd.raw.size, 0, "no surface before a disk");
        let wd = wd_with_disk(false);
        assert_eq!(wd.fdd.raw.size as u64 * BYTE, REV, "a revolution is 200 ms");
    }

    /// BUSY comes PREPARE after the controller's own clock, which idles on the byte
    /// grid: exactly 48 cycles for a write on a byte boundary, and never later than 48
    /// for any other (the data sheet's bound, §10.2).
    #[test]
    fn busy_rises_48_cycles_after_a_command_write() {
        let mut wd = wd_with_disk(false);
        let t0 = 16 * BYTE;
        wd.store(t0, 0, 0x08); // RESTORE, h = 1 (no spin-up)
        let busy = until(&mut wd, t0, 200, |w, c| w.read(c, 0) & WD_BSY != 0).unwrap();
        assert_eq!(busy - t0, 48, "BUSY after PREPARE");
        for off in 1..BYTE {
            let mut wd = wd_with_disk(false);
            let t = 16 * BYTE + off;
            wd.store(t, 0, 0x08);
            let busy = until(&mut wd, t, 200, |w, c| w.read(c, 0) & WD_BSY != 0).unwrap();
            assert!(busy - t <= 48, "a write {off} cycles into a byte: BUSY after {}", busy - t);
        }
    }

    #[test]
    fn a_seek_takes_step_rate_per_track_at_each_rate() {
        for (r, rate) in [(0u8, 12_000u64), (1, 24_000), (2, 4_000), (3, 6_000)] {
            let mut wd = wd_with_disk(false);
            wd.data = 5; // target track
            wd.store(0, 3, 5);
            let t0 = 16 * BYTE;
            wd.store(t0, 0, 0x18 | r); // SEEK, h = 1
            // Busy ends after PREPARE + 5 steps (track 0 → 5).
            let done = until(&mut wd, t0, 200_000, |w, c| {
                let st = w.read(c, 0);
                st & WD_BSY == 0 && c > t0 + 48
            })
            .unwrap();
            assert_eq!(wd.fdd.track, 5, "head at 5 (r1r0 {r})");
            assert_eq!(wd.track, 5);
            // BUSY drops once the idle state has run PREPARE again after the last step.
            assert_eq!(done - t0, 48 + 5 * rate + 48, "r1r0 {r}");
        }
    }

    #[test]
    fn verify_waits_the_settle_delay() {
        let mut wd = wd_with_disk(false);
        wd.store(0, 3, 0);
        let t0 = 16 * BYTE;
        wd.store(t0, 0, 0x0c); // RESTORE, h = 1, V = 1 — at track 0: no step
        // The ID search starts only after SETTLING: nothing is read before it.
        wd.execute(t0 + 48 + 60_000 - 1);
        assert_eq!(wd.step, 9, "still settling one cycle before the delay ends");
        wd.execute(t0 + 48 + 60_000);
        assert_eq!(wd.step, 10, "settled at exactly 60 000");
    }

    #[test]
    fn spin_up_is_six_index_pulses_before_the_first_drq() {
        let mut wd = wd_with_disk(false);
        wd.store(0, 1, 0); // track register 0
        wd.store(0, 2, 1); // sector 1
        wd.fdd.raw.head = 0;
        let t0 = 64; // command written; no MO, h = 0 → spin-up
        wd.store(t0, 0, 0x80);
        let drq = until(&mut wd, t0, 4 * 1_000_000, |w, c| {
            w.execute(c);
            w.status & WD_DRQ != 0
        })
        .unwrap();
        assert!(drq - t0 >= 48 + 5 * REV, "at least the spin-up: {}", drq - t0);
        assert!(drq - t0 < 48 + 7 * REV, "found in the first revolution after it: {}", drq - t0);
    }

    #[test]
    fn consecutive_drqs_are_one_byte_apart() {
        let mut wd = wd_with_disk(false);
        wd.store(0, 1, 0);
        wd.store(0, 2, 1);
        wd.status |= WD_MO; // motor already up: no spin-up
        let t0 = 10;
        wd.store(t0, 0, 0x80);
        let mut last = None;
        let mut gaps = Vec::new();
        let mut c = t0;
        while gaps.len() < 16 && c < t0 + REV * 2 {
            c += 1;
            wd.execute(c);
            if wd.status & WD_DRQ != 0 {
                let _ = wd.read(c, 3); // take the byte: DRQ drops
                if let Some(l) = last {
                    gaps.push(c - l);
                }
                last = Some(c);
            }
        }
        assert_eq!(gaps.len(), 16);
        assert!(gaps.iter().all(|&g| g == BYTE), "one byte per 64 cycles: {gaps:?}");
    }

    #[test]
    fn a_missing_sector_is_rnf_after_five_index_pulses() {
        let mut wd = wd_with_disk(false);
        wd.store(0, 1, 0);
        wd.store(0, 2, 11); // no sector 11 on a D81 track
        wd.status |= WD_MO;
        let t0 = 10;
        wd.store(t0, 0, 0x80);
        let rnf = until(&mut wd, t0, 7 * REV, |w, c| {
            w.execute(c);
            w.status & WD_RNF != 0
        })
        .unwrap();
        let elapsed = rnf - t0;
        assert!(elapsed > 48 + 4 * REV && elapsed <= 48 + 5 * REV + 2 * BYTE, "RNF after 5 index pulses: {elapsed}");
    }

    #[test]
    fn a_write_not_served_is_lost_data_nine_bytes_after_drq() {
        let mut wd = wd_with_disk(false);
        wd.store(0, 1, 0);
        wd.store(0, 2, 1);
        wd.status |= WD_MO;
        let t0 = 10;
        wd.store(t0, 0, 0xa0); // WRITE SECTOR, never served
        let drq = until(&mut wd, t0, 2 * REV, |w, c| {
            w.execute(c);
            w.status & WD_DRQ != 0
        })
        .unwrap();
        let lost = until(&mut wd, drq, 20 * BYTE, |w, c| {
            w.execute(c);
            w.status & WD_LD != 0
        })
        .unwrap();
        assert_eq!(lost - drq, 9 * BYTE, "lost data 9 bytes after DRQ");
        assert_eq!(wd.status & WD_DRQ, 0, "DRQ cleared");
        wd.execute(lost + 48);
        assert_eq!(wd.status & WD_BSY, 0, "the command ended");
    }

    #[test]
    fn a_write_protected_disk_refuses_a_write() {
        let mut wd = wd_with_disk(true);
        wd.status |= WD_MO;
        wd.store(10, 0, 0xa0);
        wd.execute(10_000);
        assert_ne!(wd.status & WD_WP, 0);
        assert_eq!(wd.status & WD_BSY, 0);
    }

    #[test]
    fn read_address_puts_the_ids_track_into_the_sector_register() {
        let mut wd = wd_with_disk(false);
        wd.status |= WD_MO;
        wd.fdd.track = 7;
        wd.store(10, 0, 0xc0);
        wd.execute(10 + 2 * REV);
        assert_eq!(wd.sector, 7, "the ID's track byte");
        assert_eq!(wd.status & (WD_BSY | WD_CRC | WD_RNF), 0);
    }
}
