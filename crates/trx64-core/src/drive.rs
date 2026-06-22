//! drive.rs — 1541 floppy drive: 6502 CPU + minimal bus (2 KB RAM + VIA stubs + ROM).
//!
//! Isolation gate (ADR-012): no IEC cross-machine wiring. The drive boots from its
//! DOS ROM, runs its idle/init loop with no external stimulus. VIA chips are
//! register-stub skeletons that return 0xFF on read and silently drop writes, which
//! is enough for the ROM init path to run without jamming.
//!
//! Memory map (1541 per VICE memiec.c / memiec.ts):
//!   $0000-$07FF  2 KB RAM (mirrored at $0800-$1FFF, $2000-$3FFF, $4000-$7FFF)
//!   $1800-$1BFF  VIA1 (open-bus stub: read=0xFF, write ignored)
//!   $1C00-$1FFF  VIA2 (open-bus stub)
//!   $8000-$BFFF  rom[addr & 0x7FFF] = zero (open bus, rom buffer [0x0000..0x4000])
//!   $C000-$FFFF  rom[addr & 0x7FFF] = DOS ROM bytes (rom buffer [0x4000..0x8000])
//!
//! ROM layout: the 16 KB dos1541 file is placed at rom[0x4000..0x8000].
//! Reset vector $FFFC/$FFFD = rom[0x7FFC]/rom[0x7FFD] = file offset 0x3FFC/0x3FFD.

use crate::{cpu::{Bus, Cpu6510}, NullSink, RomError};

/// Minimal 6522 VIA stub: register file (16 bytes), returns 0xFF on uninitialized
/// reads. The isolation gate does not need timer IRQ delivery.
#[derive(Clone)]
pub struct Via6522 {
    regs: [u8; 16],
}

impl Via6522 {
    fn new() -> Self {
        Self { regs: [0xFF; 16] }
    }
    #[inline]
    fn read(&self, addr: u16) -> u8 {
        self.regs[(addr & 0x0F) as usize]
    }
    #[inline]
    fn write(&mut self, addr: u16, val: u8) {
        self.regs[(addr & 0x0F) as usize] = val;
    }
}

/// Drive 6502 bus (implements cpu::Bus). Borrows from Drive1541 fields.
struct DriveBus<'a> {
    ram: &'a mut [u8; 0x800],
    rom: &'a [u8; 0x8000],
    via1: &'a mut Via6522,
    via2: &'a mut Via6522,
}

impl<'a> Bus for DriveBus<'a> {
    #[inline]
    fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7FFF => {
                // VIA1: $1800-$1BFF (mirror every $400)
                if (0x1800..=0x1BFF).contains(&addr) {
                    return self.via1.read(addr);
                }
                // VIA2: $1C00-$1FFF
                if (0x1C00..=0x1FFF).contains(&addr) {
                    return self.via2.read(addr);
                }
                // RAM mirrors: $0000-$07FF and all mirrors up to $7FFF
                self.ram[(addr & 0x07FF) as usize]
            }
            0x8000..=0xFFFF => {
                self.rom[(addr & 0x7FFF) as usize]
            }
        }
    }

    #[inline]
    fn write(&mut self, addr: u16, val: u8) {
        match addr {
            0x0000..=0x7FFF => {
                if (0x1800..=0x1BFF).contains(&addr) {
                    self.via1.write(addr, val);
                    return;
                }
                if (0x1C00..=0x1FFF).contains(&addr) {
                    self.via2.write(addr, val);
                    return;
                }
                // RAM mirrors — write to the base 2 KB
                self.ram[(addr & 0x07FF) as usize] = val;
            }
            0x8000..=0xFFFF => {
                // ROM write: silently ignored (open bus)
            }
        }
    }
}

/// 1541 drive emulator: cycle-exact 6502 + 2 KB RAM + VIA stubs + DOS ROM.
///
/// `Clone` is required so `Machine` (which contains `Drive1541`) remains cloneable
/// for Phase-2 COW forks.
#[derive(Clone)]
pub struct Drive1541 {
    pub cpu: Cpu6510,
    ram: Box<[u8; 0x800]>,
    rom: Box<[u8; 0x8000]>,
    via1: Via6522,
    via2: Via6522,
    /// Monotonic drive clock (mirrors cpu.clk after each run).
    pub drive_clk: u64,
    /// Last sampled PC for drive8-cpu deduplication (sampleDrivePc pattern).
    last_sample_pc: Option<u16>,
}

impl Drive1541 {
    pub fn new() -> Self {
        Self {
            cpu: Cpu6510::new(),
            ram: Box::new([0u8; 0x800]),
            rom: Box::new([0u8; 0x8000]),
            via1: Via6522::new(),
            via2: Via6522::new(),
            drive_clk: 0,
            last_sample_pc: None,
        }
    }

    /// Load the 16 KB 1541 DOS ROM from `rom_dir`.
    ///
    /// Tries `dos1541-325302-01+901229-05.bin` first, then the alias `1541.bin`.
    /// On success the file bytes land at `rom[0x4000..0x8000]`.
    /// On failure returns `RomError` — caller may choose to continue with zeroed ROM.
    pub fn load_rom(&mut self, rom_dir: &std::path::Path) -> Result<(), RomError> {
        let data = std::fs::read(rom_dir.join("dos1541-325302-01+901229-05.bin"))
            .or_else(|_| std::fs::read(rom_dir.join("1541.bin")))?;
        if data.len() != 0x4000 {
            return Err(RomError::BadSize(data.len(), 0x4000));
        }
        self.rom[0x4000..0x8000].copy_from_slice(&data);
        Ok(())
    }

    /// Reset the drive 6502: read reset vector from $FFFC/$FFFD (in ROM) and
    /// set PC. Sets I flag (IRQ disabled) to match real 1541 power-on.
    pub fn cold_reset(&mut self) {
        let lo = self.rom[0x7FFC] as u16;
        let hi = self.rom[0x7FFD] as u16;
        let pc = lo | (hi << 8);
        self.cpu.reset_to(pc);
        self.cpu.reg_p |= 0x04; // I flag
        self.drive_clk = 0;
        self.last_sample_pc = None;
    }

    /// Reset PC from the ROM vector (re-read). Returns the resolved PC.
    pub fn reset_pc(&self) -> u16 {
        let lo = self.rom[0x7FFC] as u16;
        let hi = self.rom[0x7FFD] as u16;
        lo | (hi << 8)
    }

    /// Run the drive 6502 for exactly `n` cycles (instruction-stepped).
    /// Uses `NullSink` — the sampling approach (not per-instruction firehose)
    /// is handled externally by `sample_pc_change`.
    pub fn run_cycles(&mut self, n: u64) {
        let mut obs = NullSink;
        let start = self.cpu.clk;
        let mut bus = DriveBus {
            ram: &mut self.ram,
            rom: &self.rom,
            via1: &mut self.via1,
            via2: &mut self.via2,
        };
        loop {
            if self.cpu.clk.wrapping_sub(start) >= n {
                break;
            }
            // Step one whole instruction.
            loop {
                self.cpu.execute_cycle(&mut bus, &mut obs);
                if self.cpu.is_at_boundary() {
                    break;
                }
            }
        }
        self.drive_clk = self.cpu.clk;
    }

    /// Sample the current drive PC for the drive8-cpu trace domain.
    ///
    /// Mirrors the TS `sampleDrivePc()` deduplication: returns `Some(...)` only
    /// when the PC has changed since the last call. This is called once per C64
    /// instruction boundary (not per drive instruction) — the "sampled" pattern
    /// described in integrated-session.ts:855 and ADR-015.
    ///
    /// Returns `(pc, a, x, y, sp, p, drive_clk)` on change, `None` if unchanged.
    pub fn sample_pc_change(&mut self) -> Option<(u16, u8, u8, u8, u8, u8, u64)> {
        let pc = self.cpu.reg_pc;
        if self.last_sample_pc == Some(pc) {
            return None;
        }
        self.last_sample_pc = Some(pc);
        Some((
            pc,
            self.cpu.reg_a,
            self.cpu.reg_x,
            self.cpu.reg_y,
            self.cpu.reg_sp,
            self.cpu.flags(),
            self.drive_clk,
        ))
    }
}

impl Default for Drive1541 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_bus_ram_mirror() {
        let mut d = Drive1541::new();
        // Write via base address, read via mirror
        {
            let mut bus = DriveBus {
                ram: &mut d.ram,
                rom: &d.rom,
                via1: &mut d.via1,
                via2: &mut d.via2,
            };
            bus.write(0x0010, 0xAB);
            assert_eq!(bus.read(0x0810), 0xAB, "$0810 should mirror $0010");
            assert_eq!(bus.read(0x2010), 0xAB, "$2010 should mirror $0010");
        }
    }

    #[test]
    fn drive_bus_via_stub() {
        let mut d = Drive1541::new();
        let mut bus = DriveBus {
            ram: &mut d.ram,
            rom: &d.rom,
            via1: &mut d.via1,
            via2: &mut d.via2,
        };
        // VIA1 at $1800 — write then read back via register stub
        bus.write(0x1800, 0x42);
        assert_eq!(bus.read(0x1800), 0x42);
        // VIA2 at $1C00
        bus.write(0x1C01, 0x55);
        assert_eq!(bus.read(0x1C01), 0x55);
    }

    #[test]
    fn drive_bus_rom_read() {
        let mut d = Drive1541::new();
        // Place a sentinel in the ROM region
        d.rom[0x4010] = 0xEA; // NOP at CPU $C010
        let mut bus = DriveBus {
            ram: &mut d.ram,
            rom: &d.rom,
            via1: &mut d.via1,
            via2: &mut d.via2,
        };
        assert_eq!(bus.read(0xC010), 0xEA);
    }

    #[test]
    fn sample_pc_change_deduplicates() {
        let mut d = Drive1541::new();
        d.cpu.reg_pc = 0xEA00;
        // First call always returns Some
        assert!(d.sample_pc_change().is_some());
        // Second call with same PC returns None
        assert!(d.sample_pc_change().is_none());
        // Change PC → Some again
        d.cpu.reg_pc = 0xEA10;
        assert!(d.sample_pc_change().is_some());
    }
}
