//! Spec 875 §12.1 — `SectorFdc`, the U64's split in one struct, as a test controller.
//!
//! The register block is shaped on `wd177x.vhd`: a command store sets BUSY and pushes
//! the command into a FIFO (taken while BUSY only for `$Dx`), a data read clears DRQ when
//! a byte was valid, a data store marks the byte valid, BUSY ends when the last byte of a
//! read has been taken or 64 µs after the last byte of a write, and the reset state is
//! track 1, sector 0, status 0, FIFO and DMA idle.
//!
//! The "firmware" serves the FIFO from an in-memory D81 at sector level, as
//! `wd177x.cc` does: type I commands set a stepper; READ SECTOR / WRITE SECTOR / READ
//! ADDRESS find the sector by stepper track, the side the board selects (`side0`) and
//! the ID registers; WRITE TRACK takes one revolution of bytes and decodes the ID and
//! data fields it laid down. A command is served no earlier than 48 drive cycles after
//! its store (the WD's command-accept time, the bound of Spec 875 §3), and the bytes of a
//! transfer pass the data register one per 64 drive cycles (32 µs, MFM at 250 kbit/s).
//!
//! Everything the board tells it is recorded with the drive clock it came at.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use trx64_core::fdc_controller::{FdcBoardIn, FdcBoardOut, FdcController};

const ST_BUSY: u8 = 0x01;
const ST_DRQ: u8 = 0x02; // index in type I
const ST_T0: u8 = 0x04;
const ST_RNF: u8 = 0x10;
const ST_SU: u8 = 0x20;
const ST_WP: u8 = 0x40;

/// The drive cycles from a command store to its service (§3).
pub const SERVE_DELAY: u64 = 48;
/// The drive cycles per byte through the data register.
pub const BYTE_CYCLES: u64 = 64;
/// One revolution: 6 250 bytes.
pub const TRACK_BYTES: usize = 6250;
/// The index pulse: the first 16 bytes of the revolution.
const INDEX_BYTES: u64 = 16;
/// `wd177x.vhd` `write_delay`: 64 µs after the last byte of a write.
const WRITE_DELAY: u64 = 128;

/// What the board did to the controller, with the drive clock it came at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ev {
    Store { clk: u64, reg: u8, val: u8 },
    BoardOut { clk: u64, out: (bool, bool) },
    DriveReset { clk: u64 },
    Power { on: bool },
    Rebase { clk: u64 },
    /// The first `clock_to` after a `drive_reset` or `rebase`.
    FirstClockTo { clk: u64 },
    /// A command served by the firmware: the command byte, the clock of its service,
    /// the stepper track and the side the board selected then.
    Served { clk: u64, cmd: u8, track: u8, side0: bool },
    Restored,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Dma {
    Idle,
    /// Bytes to the 6502: the next to present, and when it may appear.
    Read { buf: Vec<u8>, next: usize, avail_at: u64 },
    /// Bytes from the 6502: what arrived, how many are wanted, when DRQ may rise
    /// again, and where they go.
    Write { buf: Vec<u8>, len: usize, drq_at: u64, to: WriteTo },
    /// The 64 µs after a write: BUSY falls at `until`.
    WriteDelay { until: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum WriteTo {
    Sector(usize),
    Track { track: u8, side_id: u8 },
}

/// The controller state a checkpoint carries (the log does not ride).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Core {
    pub status: u8,
    pub track: u8,
    pub sector: u8,
    pub data: u8,
    pub command: u8,
    rdata_valid: bool,
    fifo: Vec<(u8, u64)>,
    dma: Dma,
    /// The physical track the stepper stands on (0-83).
    pub stepper: u8,
    pub side0: bool,
    pub motor_on: bool,
    motor_since: u64,
    pub clk: u64,
    /// `board_in` as the host set it; `disk_changed` is cleared by a step pulse when
    /// `clear_change_on_step` (the U64's `do_step` clears `diskchange`).
    pub input: (bool, bool, bool),
    pub clear_change_on_step: bool,
    /// The medium, hex (a checkpoint's `state`).
    #[serde(with = "hex_bytes")]
    pub d81: Vec<u8>,
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let mut out = String::with_capacity(v.len() * 2);
        for b in v {
            out.push_str(&format!("{b:02x}"));
        }
        s.serialize_str(&out)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(serde::de::Error::custom)).collect()
    }
}

#[derive(Clone, Debug)]
pub struct SectorFdc {
    pub core: Core,
    pub name: String,
    /// Checkpoint hooks: `checkpoint()` gives the core, `restore` takes it back.
    pub hooks: bool,
    /// `clone_device` gives a copy (a straight run to compare a restore against).
    pub cloneable: bool,
    /// The contract breaker of §3: serve inside the store, a type I command finished
    /// before the DOS's first status read.
    pub serve_in_store: bool,
    pub log: Vec<Ev>,
    /// Every `clock_to`: how many, the last clock, and any that went backwards.
    pub clock_tos: u64,
    pub last_clock_to: Option<u64>,
    pub clock_regressions: Vec<(u64, u64)>,
    await_first_clock_to: bool,
    /// The clock of every read of the status register that saw BUSY.
    pub busy_seen: u64,
}

impl SectorFdc {
    pub fn new(name: &str, d81: Vec<u8>) -> Self {
        SectorFdc {
            core: Core {
                status: 0,
                track: 1,
                sector: 0,
                data: 0,
                command: 0,
                rdata_valid: false,
                fifo: Vec::new(),
                dma: Dma::Idle,
                stepper: 0,
                side0: true,
                motor_on: false,
                motor_since: 0,
                clk: 0,
                input: (true, false, false),
                clear_change_on_step: true,
                d81,
            },
            name: name.to_string(),
            hooks: false,
            cloneable: false,
            serve_in_store: false,
            log: Vec::new(),
            clock_tos: 0,
            last_clock_to: None,
            clock_regressions: Vec::new(),
            await_first_clock_to: false,
            busy_seen: 0,
        }
    }

    /// `board_in` as the host drives it: ready, disk changed, write protected.
    pub fn set_input(&mut self, ready: bool, disk_changed: bool, write_protected: bool) {
        self.core.input = (ready, disk_changed, write_protected);
    }

    pub fn d81(&self) -> &[u8] {
        &self.core.d81
    }

    fn tracks(&self) -> u8 {
        (self.core.d81.len() / (40 * 256)) as u8
    }

    /// The physical head the board selects: `side0` high reads head 0.
    fn head(&self) -> u8 {
        if self.core.side0 {
            0
        } else {
            1
        }
    }

    /// The side byte the IDs under the head carry (fdd.c: `head ^ head_invert`).
    fn side_id(&self) -> u8 {
        self.head() ^ 1
    }

    /// The byte offset of the 512-byte physical sector `sec` (1-10) at physical track
    /// `t` under the ID side byte `side_id` (the D81 order of fdd.c).
    fn sector_offset(&self, t: u8, side_id: u8, sec: u8) -> Option<usize> {
        if !(1..=10).contains(&sec) || t >= self.tracks() {
            return None;
        }
        Some(((t as usize * 2 + side_id as usize) * 10 + (sec as usize - 1)) * 512)
    }

    fn index(&self, clk: u64) -> bool {
        self.core.motor_on && (clk.saturating_sub(self.core.motor_since) / BYTE_CYCLES) % (TRACK_BYTES as u64) < INDEX_BYTES
    }

    /// Run the block and the firmware up to drive cycle `to`.
    fn advance(&mut self, to: u64) {
        loop {
            // The firmware: the next command whose time has come.
            if let Some(&(cmd, at)) = self.core.fifo.first() {
                if at <= to && self.dma_quiet_before(at) {
                    self.core.fifo.remove(0);
                    self.serve(cmd, at);
                    continue;
                }
            }
            // The block's DMA.
            match &mut self.core.dma {
                Dma::Read { buf, next, avail_at } => {
                    if !self.core.rdata_valid {
                        if *next == buf.len() {
                            self.core.status &= !(ST_BUSY | ST_DRQ);
                            self.core.dma = Dma::Idle;
                            continue;
                        }
                        if *avail_at <= to {
                            self.core.data = buf[*next];
                            *next += 1;
                            self.core.rdata_valid = true;
                            self.core.status |= ST_DRQ;
                            continue;
                        }
                    }
                }
                Dma::Write { buf, len, drq_at, .. } => {
                    if buf.len() < *len && *drq_at <= to && self.core.status & ST_DRQ == 0 {
                        self.core.status |= ST_DRQ;
                    }
                }
                Dma::WriteDelay { until } => {
                    if *until <= to {
                        self.core.status &= !ST_BUSY;
                        self.core.dma = Dma::Idle;
                        continue;
                    }
                }
                Dma::Idle => {}
            }
            break;
        }
        self.core.clk = self.core.clk.max(to);
    }

    /// The firmware serves a command only when the block would have raised its IRQ by
    /// then — any command at its time; ordering with the DMA is the block's.
    fn dma_quiet_before(&self, _at: u64) -> bool {
        true
    }

    fn step_to(&mut self, target: u8) {
        let target = target.min(83);
        if target != self.core.stepper {
            self.core.stepper = target;
            self.pulse();
        }
    }

    fn pulse(&mut self) {
        if self.core.clear_change_on_step {
            self.core.input.1 = false;
        }
    }

    /// `handle_wd177x_command`, at drive cycle `t`.
    fn serve(&mut self, cmd: u8, t: u64) {
        self.log.push(Ev::Served { clk: t, cmd, track: self.core.stepper, side0: self.core.side0 });
        self.core.status &= !(ST_RNF | ST_WP | 0x08 | 0x04);
        let (_, _, protected) = self.core.input;
        match cmd >> 4 {
            0x0 => {
                // RESTORE
                self.core.track = 0;
                self.step_to(0);
                self.type1_end(cmd);
            }
            0x1 => {
                // SEEK: the data register holds the target.
                self.core.track = self.core.data;
                self.step_to(self.core.data);
                self.type1_end(cmd);
            }
            0x2..=0x7 => {
                // STEP (0x2/0x3), STEP IN (0x4/0x5), STEP OUT (0x6/0x7): one pulse.
                let inward = match cmd >> 5 {
                    2 => true,
                    3 => false,
                    _ => self.core.stepper < 83, // STEP: the last direction, inward here
                };
                if inward {
                    if self.core.stepper < 83 {
                        self.core.stepper += 1;
                    }
                    self.pulse();
                    if cmd & 0x10 != 0 {
                        self.core.track = self.core.track.wrapping_add(1);
                    }
                } else {
                    if self.core.stepper > 0 {
                        self.core.stepper -= 1;
                        self.pulse();
                    }
                    if cmd & 0x10 != 0 && self.core.track != 0 {
                        self.core.track -= 1;
                    }
                }
                self.type1_end(cmd);
            }
            0x8 | 0x9 => {
                // READ SECTOR
                match self.find(self.core.track, self.core.sector) {
                    Some(o) => {
                        let buf = self.core.d81[o..o + 512].to_vec();
                        self.core.rdata_valid = false;
                        self.core.dma = Dma::Read { buf, next: 0, avail_at: t };
                    }
                    None => self.rnf(),
                }
            }
            0xa | 0xb => {
                // WRITE SECTOR
                if protected {
                    self.core.status |= ST_WP;
                    self.core.status &= !ST_BUSY;
                    return;
                }
                match self.find(self.core.track, self.core.sector) {
                    Some(o) => self.core.dma = Dma::Write { buf: Vec::new(), len: 512, drq_at: t, to: WriteTo::Sector(o) },
                    None => self.rnf(),
                }
            }
            0xc => {
                // READ ADDRESS: the next ID under the head.
                if self.core.stepper >= self.tracks() {
                    self.rnf();
                    return;
                }
                let rev = (t.saturating_sub(self.core.motor_since) / BYTE_CYCLES) % TRACK_BYTES as u64;
                let sec = (rev * 10 / TRACK_BYTES as u64) as u8 + 1;
                let mut b = vec![self.core.stepper, self.side_id(), sec, 2];
                let mut crc = 0xb230u16;
                for &x in &b {
                    crc = trx64_core::fdd::fdd_crc(crc, x);
                }
                b.push((crc >> 8) as u8);
                b.push(crc as u8);
                // The WD1772 puts the ID's track into the sector register.
                self.core.sector = self.core.stepper;
                self.core.rdata_valid = false;
                self.core.dma = Dma::Read { buf: b, next: 0, avail_at: t };
            }
            0xe => {
                // READ TRACK: not implemented (wd177x.cc).
                self.core.status &= !ST_BUSY;
            }
            0xf => {
                // WRITE TRACK
                if protected {
                    self.core.status |= ST_WP;
                    self.core.status &= !ST_BUSY;
                    return;
                }
                let to = WriteTo::Track { track: self.core.stepper, side_id: self.side_id() };
                self.core.dma = Dma::Write { buf: Vec::new(), len: TRACK_BYTES, drq_at: t, to };
            }
            _ => {
                // FORCE INTERRUPT: stop the DMA, BUSY off.
                self.core.dma = Dma::Idle;
                self.core.rdata_valid = false;
                self.core.status &= !(ST_BUSY | ST_DRQ);
            }
        }
    }

    fn type1_end(&mut self, cmd: u8) {
        if cmd & 0x08 == 0 {
            self.core.status |= ST_SU;
        }
        if self.core.stepper == 0 {
            self.core.status |= ST_T0;
        }
        self.core.status &= !ST_BUSY;
    }

    fn rnf(&mut self) {
        self.core.status |= ST_RNF;
        self.core.status &= !ST_BUSY;
    }

    /// The sector the ID registers name under the head, by stepper track and side.
    fn find(&self, track_reg: u8, sector_reg: u8) -> Option<usize> {
        if track_reg != self.core.stepper {
            return None;
        }
        self.sector_offset(self.core.stepper, self.side_id(), sector_reg)
    }

    /// The last byte of a write arrived at `c`: the firmware's completion.
    fn complete_write(&mut self, c: u64) {
        let Dma::Write { buf, to, .. } = std::mem::replace(&mut self.core.dma, Dma::WriteDelay { until: c + WRITE_DELAY }) else {
            return;
        };
        match to {
            WriteTo::Sector(o) => self.core.d81[o..o + 512].copy_from_slice(&buf),
            WriteTo::Track { track, side_id } => self.decode_write_track(&buf, track, side_id),
        }
    }

    /// `decode_write_track`: the WD's write-track encoding — `F5` a sync `A1` that
    /// presets the CRC, `F6` a `C2`, `F7` the two CRC bytes — read back into ID and data
    /// fields; each data field whose ID names this track, this side, a sector 1-10 and
    /// size 2 is written to its sector.
    fn decode_write_track(&mut self, raw: &[u8], track: u8, side_id: u8) {
        let mut i = 0;
        let mut id: Option<(u8, u8, u8, u8)> = None;
        while i < raw.len() {
            if raw[i] != 0xf5 {
                i += 1;
                continue;
            }
            while i < raw.len() && raw[i] == 0xf5 {
                i += 1;
            }
            let Some(&mark) = raw.get(i) else { break };
            i += 1;
            match mark {
                0xfe if i + 4 <= raw.len() => {
                    id = Some((raw[i], raw[i + 1], raw[i + 2], raw[i + 3]));
                    i += 4;
                }
                0xfb => {
                    if let Some((t, s, sec, size)) = id.take() {
                        if t == track && s == side_id && size == 2 && i + 512 <= raw.len() {
                            if let Some(o) = self.sector_offset(track, side_id, sec) {
                                self.core.d81[o..o + 512].copy_from_slice(&raw[i..i + 512]);
                            }
                            i += 512;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

impl FdcController for SectorFdc {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn read(&mut self, clk: u64, reg: u8) -> u8 {
        self.advance(clk);
        match reg & 3 {
            0 => {
                if self.core.status & ST_BUSY != 0 {
                    self.busy_seen += 1;
                }
                let mut st = self.core.status;
                if self.core.command & 0x80 == 0 {
                    st = (st & !ST_DRQ) | if self.index(clk) { ST_DRQ } else { 0 };
                }
                st
            }
            1 => self.core.track,
            2 => self.core.sector,
            _ => {
                if self.core.rdata_valid {
                    self.core.rdata_valid = false;
                    self.core.status &= !ST_DRQ;
                    if let Dma::Read { avail_at, .. } = &mut self.core.dma {
                        *avail_at = (*avail_at + BYTE_CYCLES).max(clk);
                    }
                    // The last byte taken ends the read at once.
                    self.advance(clk);
                }
                self.core.data
            }
        }
    }

    fn store(&mut self, clk: u64, reg: u8, val: u8) {
        self.advance(clk);
        self.log.push(Ev::Store { clk, reg, val });
        match reg & 3 {
            0 => {
                if self.core.status & ST_BUSY == 0 || val & 0xf0 == 0xd0 {
                    self.core.command = val;
                    self.core.status |= ST_BUSY;
                    if self.serve_in_store {
                        self.serve(val, clk);
                    } else {
                        self.core.fifo.push((val, clk + SERVE_DELAY));
                    }
                }
            }
            1 => self.core.track = val,
            2 => self.core.sector = val,
            _ => {
                self.core.data = val;
                if let Dma::Write { buf, len, drq_at, .. } = &mut self.core.dma {
                    if buf.len() < *len {
                        buf.push(val);
                        self.core.status &= !ST_DRQ;
                        *drq_at = clk + BYTE_CYCLES;
                        if buf.len() == *len {
                            self.complete_write(clk);
                        }
                    }
                }
            }
        }
    }

    fn peek(&self, reg: u8) -> u8 {
        match reg & 3 {
            0 => self.core.status,
            1 => self.core.track,
            2 => self.core.sector,
            _ => self.core.data,
        }
    }

    fn clock_to(&mut self, clk: u64) {
        self.clock_tos += 1;
        if let Some(last) = self.last_clock_to {
            if clk < last {
                self.clock_regressions.push((last, clk));
            }
        }
        if self.await_first_clock_to {
            self.await_first_clock_to = false;
            self.log.push(Ev::FirstClockTo { clk });
        }
        self.last_clock_to = Some(clk);
        self.advance(clk);
    }

    fn board_out(&mut self, clk: u64, out: FdcBoardOut) {
        self.advance(clk);
        self.log.push(Ev::BoardOut { clk, out: (out.side0, out.motor_on) });
        if out.motor_on && !self.core.motor_on {
            self.core.motor_since = clk;
        }
        self.core.side0 = out.side0;
        self.core.motor_on = out.motor_on;
    }

    fn board_in(&self) -> FdcBoardIn {
        let (ready, disk_changed, write_protected) = self.core.input;
        FdcBoardIn { ready, disk_changed, write_protected }
    }

    fn head(&self) -> (u8, u8) {
        (self.core.stepper, self.head())
    }

    fn drive_reset(&mut self, clk: u64) {
        self.log.push(Ev::DriveReset { clk });
        // wd177x.vhd:384-398 — the stepper is the mechanism's and stays.
        let c = &mut self.core;
        c.track = 1;
        c.sector = 0;
        c.command = 0;
        c.status = 0;
        c.rdata_valid = false;
        c.fifo.clear();
        c.dma = Dma::Idle;
        c.side0 = true;
        c.motor_on = false;
        c.clk = clk;
        self.last_clock_to = None;
        self.await_first_clock_to = true;
    }

    fn power(&mut self, on: bool) {
        self.log.push(Ev::Power { on });
        if !on {
            self.core.motor_on = false;
        }
    }

    fn rebase(&mut self, clk: u64) {
        self.log.push(Ev::Rebase { clk });
        // Every pending time moves with the clock.
        let d = clk as i128 - self.core.clk as i128;
        let mv = |t: &mut u64| *t = (*t as i128 + d).max(0) as u64;
        for (_, at) in self.core.fifo.iter_mut() {
            mv(at);
        }
        match &mut self.core.dma {
            Dma::Read { avail_at, .. } => mv(avail_at),
            Dma::Write { drq_at, .. } => mv(drq_at),
            Dma::WriteDelay { until } => mv(until),
            Dma::Idle => {}
        }
        mv(&mut self.core.motor_since);
        self.core.clk = clk;
        self.last_clock_to = None;
        self.await_first_clock_to = true;
    }

    fn checkpoint(&self) -> Option<serde_json::Value> {
        self.hooks.then(|| serde_json::to_value(&self.core).unwrap())
    }

    fn restore(&mut self, state: &serde_json::Value) -> Result<(), String> {
        if !self.hooks {
            return Err(format!("{}: carries no checkpoint state", self.name));
        }
        self.core = serde_json::from_value(state.clone()).map_err(|e| format!("{}: {e}", self.name))?;
        self.log.push(Ev::Restored);
        self.last_clock_to = None;
        Ok(())
    }

    fn clone_device(&self) -> Option<Box<dyn FdcController>> {
        self.cloneable.then(|| Box::new(self.clone()) as Box<dyn FdcController>)
    }
}
