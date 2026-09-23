//! Spec 874 §12.1 — `ProbeListener`: a minimal host device for the gates.
//!
//! A listener only. On ATN falling it pulls DATA at once ("I am here"); it takes bytes
//! under ATN with the listener handshake; a primary that is not `$20+u` / `$40+u` /
//! `$3F` / `$5F` releases both lines until ATN rises; after ATN it listens when it was
//! told to. It never talks. The line machine and its times are the ones Spec 873's
//! folder runs with its default (Ultimate) profile — settle 20 µs, ready-for-data on
//! CLK high, EOI after 1475 µs, EOI acknowledge 70 µs, frame acknowledge 50 µs (5 µs
//! under ATN) — so a probe and a folder answer a held ATN alike.
//!
//! It records every call with its cycle, can be told to hold DATA until a set cycle,
//! claims whatever `units()` it is given, and opts out of checkpoints unless `hooks`.

#![allow(dead_code)]

use trx64_core::iec_device::{IecDevice, IecLines, IecOut};

/// One call the machine made, in the order it made them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ev {
    Edge(u64, bool),
    Clock(u64, IecLines),
    Rebase(u64, IecLines),
    Reset,
    Hz(u32),
}

const PRE0: u8 = 0;
const PRE1: u8 = 1;
const PRE2: u8 = 2;
const READY: u8 = 3;
const EOI: u8 = 4;
const EOIW: u8 = 5;
const BIT0: u8 = 6;
const BIT7W: u8 = 21;
const DONE0: u8 = 22;
const ACK: u8 = 26;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Line {
    pub state: u8,
    pub under_atn: bool,
    pub listening: bool,
    pub byte: u8,
    pub primary: u8,
    pub secondary: u8,
    pub timeout: u64,
    pub pull_clk: bool,
    pub pull_data: bool,
    /// The lines as the rest drove them at the last call.
    pub bus_atn: bool,
    pub bus_clk: bool,
    pub bus_data: bool,
    pub now: u64,
}

#[derive(Clone)]
pub struct ProbeListener {
    pub name: String,
    /// The unit it answers to on the wire (`$20+u`, `$40+u`); `None`: no unit.
    pub unit: Option<u8>,
    /// What `units()` reports.
    pub claims: u16,
    /// Record every call in `log` (off for long runs).
    pub record: bool,
    pub log: Vec<Ev>,
    /// Pull DATA until this cycle (exclusive), whatever the line machine does.
    pub hold_data_until: Option<u64>,
    /// Checkpoint hooks on; off = opted out.
    pub hooks: bool,
    /// `clone_device` answers a copy; off = `None`, as a host's device.
    pub clonable: bool,
    pub cpu_hz: u32,
    pub line: Line,
    /// Bytes taken under ATN and while listening.
    pub under_atn: Vec<u8>,
    pub listened: Vec<u8>,
}

impl ProbeListener {
    pub fn new(name: &str, unit: Option<u8>) -> ProbeListener {
        ProbeListener {
            name: name.to_string(),
            unit,
            claims: unit.map(|u| 1u16 << u).unwrap_or(0),
            record: true,
            log: Vec::new(),
            hold_data_until: None,
            hooks: false,
            clonable: false,
            cpu_hz: 985_248,
            line: Line {
                state: PRE0,
                under_atn: false,
                listening: false,
                byte: 0,
                primary: 0,
                secondary: 0,
                timeout: 0,
                pull_clk: false,
                pull_data: false,
                bus_atn: true,
                bus_clk: true,
                bus_data: true,
                now: 0,
            },
            under_atn: Vec::new(),
            listened: Vec::new(),
        }
    }

    fn cyc(&self, us: u32) -> u64 {
        ((us as f64) * (self.cpu_hz as f64) / 1_000_000.0 + 0.5) as u64
    }

    fn due(&self) -> Option<u64> {
        let l = &self.line;
        if !(l.under_atn || l.listening) {
            return None;
        }
        match l.state {
            PRE0 | EOI | ACK => Some(l.timeout),
            READY if !l.under_atn => Some(l.timeout),
            _ => None,
        }
    }

    fn settle(&mut self, now: u64) {
        self.line.now = now;
        for _ in 0..64 {
            let before = self.line.clone();
            self.step(now);
            if before == self.line {
                break;
            }
        }
    }

    fn release(&mut self) {
        self.line.pull_clk = false;
        self.line.pull_data = false;
    }

    fn pull_data(&mut self) {
        self.line.pull_clk = false;
        self.line.pull_data = true;
    }

    fn for_me(&self, primary: u8) -> bool {
        primary == 0x3f || primary == 0x5f || self.unit.is_some_and(|u| primary == 0x20 + u || primary == 0x40 + u)
    }

    /// The folder's `step`, listener half (`folder_device.rs`), without a DOS.
    fn step(&mut self, now: u64) {
        let data_high = self.line.bus_data && !self.line.pull_data;
        let clk_high = self.line.bus_clk && !self.line.pull_clk;
        let atn_high = self.line.bus_atn;

        if !self.line.under_atn && !atn_high {
            let settle = self.cyc(20);
            let l = &mut self.line;
            l.state = PRE0;
            l.under_atn = true;
            l.primary = 0;
            l.secondary = 0;
            l.timeout = now + settle;
            self.pull_data();
        } else if self.line.under_atn && atn_high {
            self.line.under_atn = false;
            let p = self.line.primary;
            if self.unit.is_some_and(|u| p == 0x20 + u) {
                self.line.listening = true;
                self.line.state = PRE1;
                self.pull_data();
            } else if p == 0x3f {
                self.line.listening = false;
            }
            if !self.line.listening {
                self.release();
            }
        }

        if !(self.line.under_atn || self.line.listening) {
            return;
        }
        let under_atn = self.line.under_atn;
        match self.line.state {
            PRE0 => {
                if now >= self.line.timeout {
                    self.line.state = PRE1;
                }
            }
            PRE1 => {
                if !clk_high {
                    self.line.state = PRE2;
                }
            }
            PRE2 => {
                if clk_high {
                    self.release();
                    self.line.timeout = now + self.cyc(1475);
                    self.line.state = READY;
                }
            }
            READY => {
                if !clk_high {
                    self.line.state = BIT0;
                } else if !under_atn && now >= self.line.timeout {
                    self.pull_data();
                    self.line.state = EOI;
                    self.line.timeout = now + self.cyc(70);
                }
            }
            EOI => {
                if now >= self.line.timeout {
                    self.release();
                    self.line.state = EOIW;
                }
            }
            EOIW => {
                if !clk_high {
                    self.line.state = BIT0;
                }
            }
            s if (BIT0..=BIT7W).contains(&s) && (s - BIT0) % 2 == 0 => {
                if clk_high {
                    let bit = 1u8 << ((s - BIT0) / 2);
                    self.line.byte = (self.line.byte & !bit) | if data_high { bit } else { 0 };
                    self.line.state += 1;
                }
            }
            s if (BIT0..BIT7W).contains(&s) => {
                if !clk_high {
                    self.line.state += 1;
                }
            }
            BIT7W => {
                if !clk_high {
                    let b = self.line.byte;
                    let take = if under_atn {
                        if self.line.primary == 0 {
                            self.line.primary = b;
                        } else if self.line.secondary == 0 {
                            self.line.secondary = b;
                        }
                        self.under_atn.push(b);
                        self.for_me(self.line.primary)
                    } else {
                        self.listened.push(b);
                        true
                    };
                    if !take {
                        self.line.state = DONE0;
                    } else {
                        let d = if under_atn { 5 } else { 50 };
                        self.line.timeout = now + self.cyc(d);
                        self.line.state = ACK;
                    }
                }
            }
            ACK if now >= self.line.timeout => {
                self.pull_data();
                self.line.state = PRE2;
            }
            _ => {} // DONE0: released until ATN rises.
        }
    }

    /// Clock-to events only.
    pub fn clocks(&self) -> Vec<(u64, IecLines)> {
        self.log.iter().filter_map(|e| if let Ev::Clock(c, l) = e { Some((*c, *l)) } else { None }).collect()
    }
}

impl IecDevice for ProbeListener {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn clock_to(&mut self, clk: u64, bus: IecLines) {
        if self.record {
            self.log.push(Ev::Clock(clk, bus));
        }
        let t = clk.max(self.line.now);
        let mut guard = 0;
        while let Some(due) = self.due() {
            if due >= t || guard > 4096 {
                break;
            }
            let at = due.max(self.line.now);
            self.settle(at);
            guard += 1;
        }
        self.line.bus_atn = bus.atn;
        self.line.bus_clk = bus.clk;
        self.line.bus_data = bus.data;
        self.settle(t);
    }

    fn outputs(&self) -> IecOut {
        let held = self.hold_data_until.is_some_and(|p| self.line.now < p);
        IecOut { clk: self.line.pull_clk, data: self.line.pull_data || held }
    }

    fn rebase(&mut self, clk: u64, bus: IecLines) {
        if self.record {
            self.log.push(Ev::Rebase(clk, bus));
        }
        self.line.now = clk;
        self.line.timeout = self.line.timeout.min(clk);
        self.line.bus_atn = bus.atn;
        self.line.bus_clk = bus.clk;
        self.line.bus_data = bus.data;
    }

    fn atn_edge(&mut self, clk: u64, level: bool) {
        if self.record {
            self.log.push(Ev::Edge(clk, level));
        }
    }

    fn units(&self) -> u16 {
        self.claims
    }

    fn set_cpu_hz(&mut self, hz: u32) {
        self.cpu_hz = hz;
        if self.record {
            self.log.push(Ev::Hz(hz));
        }
    }

    fn c64_reset(&mut self) {
        if self.record {
            self.log.push(Ev::Reset);
        }
    }

    fn checkpoint(&self) -> Option<serde_json::Value> {
        self.hooks.then(|| serde_json::to_value(&self.line).unwrap())
    }

    fn restore(&mut self, state: &serde_json::Value) -> Result<(), String> {
        if !self.hooks {
            return Err(format!("{}: carries no checkpoint state", self.name));
        }
        self.line = serde_json::from_value(state.clone()).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn clone_device(&self) -> Option<Box<dyn IecDevice>> {
        self.clonable.then(|| Box::new(self.clone()) as Box<dyn IecDevice>)
    }
}
