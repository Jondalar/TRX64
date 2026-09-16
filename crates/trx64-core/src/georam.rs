//! Spec 853 — the GeoRAM / BBGRAM banked memory expansion.
//!
//! 1:1 PORT of `vice/src/c64/cart/georam.c`.
//!
//! The opposite of the REU in every way that costs work: no DMA, no interrupt, no hold.
//! VICE's own summary is the whole design — "The GeoRAM is a banked memory system"
//! (georam.c:51). A 256-byte window at `$DE00-$DEFF`, two write-only registers at
//! `$DFFE`/`$DFFF`, and the CPU moves every byte itself.
//!
//! The battery-backed variant (BBG), where retaining the contents IS the hardware's
//! behaviour, is deliberately out of scope (853 §6, D9).

use crate::expansion::{Access, ExpansionDevice};

/// The window lives in IO1; the two registers at the top of IO2, mirrored down to
/// `$DF80` (`georam_io2_device`, georam.c:159-173).
const IO2_REGS_BASE: u16 = 0xDF80;

/// A GeoRAM: `size_kb` KiB of RAM behind a 256-byte window.
pub struct GeoRam {
    ram: Vec<u8>,
    size_kb: u32,
    /// `georam[0]` — the window, 0..63.
    window: u8,
    /// `georam[1]` — the bank.
    bank: u8,
}

impl Clone for GeoRam {
    fn clone(&self) -> Self {
        GeoRam { ram: self.ram.clone(), size_kb: self.size_kb, window: self.window, bank: self.bank }
    }
}

impl GeoRam {
    /// `size_kb` must be a multiple of 16 — the bank register counts 16 KiB blocks, and
    /// VICE's wrap (`size_kb / 16`) divides by exactly that.
    pub fn new(size_kb: u32) -> Option<Self> {
        if size_kb == 0 || size_kb % 16 != 0 {
            return None;
        }
        Some(GeoRam {
            ram: vec![0; (size_kb as usize) << 10],
            size_kb,
            window: 0,
            bank: 0,
        })
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

    pub fn window(&self) -> u8 {
        self.window
    }

    pub fn bank(&self) -> u8 {
        self.bank
    }

    /// georam.c:191-198 — `georam_ram[(bank * 16384) + (window * 256) + addr]`.
    fn offset(&self, addr: u16) -> usize {
        (self.bank as usize) * 16384 + (self.window as usize) * 256 + (addr & 0xFF) as usize
    }

    /// georam.c:213-229. The wrap is repeated subtraction, not a mask — with a size that
    /// is not a power of two a mask would land somewhere else entirely.
    fn store_reg(&mut self, addr: u16, byte: u8) {
        let mut byte = byte;
        if addr & 1 == 1 {
            let banks = (self.size_kb / 16) as u8;
            while byte > banks - 1 {
                byte -= banks;
            }
            self.bank = byte;
        } else {
            while byte > 63 {
                byte -= 64;
            }
            self.window = byte;
        }
    }

    pub fn status(&self) -> GeoRamStatus {
        GeoRamStatus { size_kb: self.size_kb, bank: self.bank, window: self.window }
    }
}

/// What the monitor reports (`georam_dump`, georam.c:229-233).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GeoRamStatus {
    pub size_kb: u32,
    pub bank: u8,
    pub window: u8,
}

impl ExpansionDevice for GeoRam {
    fn read(&mut self, a: Access, _cart: Option<u8>) -> Option<u8> {
        match a.addr & 0xFF00 {
            // IO1: the window. Always valid.
            0xDE00 => Some(self.ram[self.offset(a.addr)]),
            // IO2: the registers are WRITE ONLY — a read is never valid.
            _ => None,
        }
    }

    fn peek(&self, addr: u16, _cart: Option<u8>) -> Option<u8> {
        match addr & 0xFF00 {
            0xDE00 => Some(self.ram[self.offset(addr)]),
            // georam.c:200-206 — the peek door does show the two registers.
            _ if addr >= IO2_REGS_BASE => {
                if addr & 0xFFFE == 0xDFFE {
                    Some(if addr & 1 == 1 { self.bank } else { self.window })
                } else {
                    Some(0)
                }
            }
            _ => None,
        }
    }

    fn write(&mut self, a: Access, value: u8) {
        match a.addr & 0xFF00 {
            0xDE00 => {
                let off = self.offset(a.addr);
                self.ram[off] = value;
            }
            _ if a.addr >= IO2_REGS_BASE => self.store_reg(a.addr, value),
            _ => {}
        }
    }

    fn clone_device(&self) -> Option<Box<dyn ExpansionDevice>> {
        Some(Box::new(self.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expansion::AccessKind;

    fn a(addr: u16) -> Access {
        Access { addr, clk: 0, kind: AccessKind::Cpu, stalled: 0, stalled_on_bus: 0 }
    }

    #[test]
    fn the_window_reads_and_writes() {
        let mut g = GeoRam::new(512).unwrap();
        g.write(a(0xDE00), 0x11);
        g.write(a(0xDEFF), 0x22);
        assert_eq!(g.read(a(0xDE00), None), Some(0x11));
        assert_eq!(g.read(a(0xDEFF), None), Some(0x22));
        assert_eq!(g.ram[0], 0x11);
        assert_eq!(g.ram[255], 0x22);
    }

    #[test]
    fn the_registers_are_write_only() {
        let mut g = GeoRam::new(512).unwrap();
        assert_eq!(g.read(a(0xDFFE), None), None);
        assert_eq!(g.read(a(0xDFFF), None), None);
    }

    #[test]
    fn bank_and_window_place_the_address() {
        let mut g = GeoRam::new(512).unwrap();
        g.write(a(0xDFFF), 3); // bank 3
        g.write(a(0xDFFE), 2); // window 2
        g.write(a(0xDE10), 0x5A);
        assert_eq!(g.bank, 3);
        assert_eq!(g.window, 2);
        assert_eq!(g.ram[3 * 16384 + 2 * 256 + 0x10], 0x5A);
    }

    #[test]
    fn the_window_wraps_at_64_by_subtraction() {
        let mut g = GeoRam::new(512).unwrap();
        g.write(a(0xDFFE), 65);
        assert_eq!(g.window, 1);
    }

    #[test]
    fn the_bank_wraps_at_the_fitted_size() {
        // 512 KiB = 32 banks of 16 KiB.
        let mut g = GeoRam::new(512).unwrap();
        g.write(a(0xDFFF), 33);
        assert_eq!(g.bank, 1);
    }

    #[test]
    fn a_size_that_is_not_whole_banks_is_refused() {
        assert!(GeoRam::new(0).is_none());
        assert!(GeoRam::new(24).is_none());
        assert!(GeoRam::new(512).is_some());
    }

    #[test]
    fn nothing_it_does_drives_a_line() {
        let g = GeoRam::new(512).unwrap();
        let l = g.lines();
        assert!(!l.irq && !l.nmi && !l.hold);
        assert!(g.snoop_addresses().is_empty());
    }
}
