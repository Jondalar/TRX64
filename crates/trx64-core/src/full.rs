//! full.rs — the assembled full-C64 memory bus (FullBus).
//!
//! Composition (ADR-010/012/021): the cycle-exact CPU (cpu.rs), VIC-II (vic.rs),
//! CIA1/CIA2 (cia.rs) and 1541 drive (drive.rs) — each byte-exact in isolation —
//! wired into the real machine through a single `Bus` impl that reproduces the TS
//! oracle's `HeadlessMemoryBus` (memory-bus.ts) + `integrated-session.ts` run loop:
//!
//!   * PLA banking via the $00/$01 CPU port (LORAM/HIRAM/CHAREN; GAME/EXROM=1, no
//!     cart). 32-entry memconfig table (= buildMemConfigTable), selected each
//!     $00/$01 write. RAM $0000-$9FFF, BASIC $A000-$BFFF, IO/char $D000-$DFFF,
//!     KERNAL $E000-$FFFF — per the live config.
//!   * IO routing: VIC $D000-$D3FF, SID $D400-$D7FF (register shadow), colorRAM
//!     $D800-$DBFF (low nibble + $F0 open-bus high nibble), CIA1 $DC00-$DCFF,
//!     CIA2 $DD00-$DDFF.
//!   * ROMs in SEPARATE arrays; the RAM under the ROM/IO windows keeps its
//!     power-on DRAM-fill (writes-through-ROM + the trace `old`/oldValue byte read
//!     RAM underneath, never ROM — memory-bus.ts gotcha).
//!   * VIC ticked per CPU master cycle (Bus::tick + BA-low stealing, vic.rs).
//!   * Both CIAs share the CPU master clock; their timer state machines advance
//!     lazily to `clk` on each register access (rclk = clk, C64SC offsets = 0).
//!   * Cross-chip IRQ: CIA1 ∨ VIC → CPU IRQ line; CIA2 → CPU NMI line, sampled by
//!     the CPU at the instruction boundary (cpu.rs interrupt pipeline).

use crate::cia::{Cia, CIAT_TABLEN};
use crate::cpu::Bus;
use crate::vic::VicII;

/// One pre-built memconfig entry (= memory-bus.ts MemConfigEntry, no-cart slice).
#[derive(Clone, Copy)]
pub struct MemConfig {
    /// $A000-$BFFF maps BASIC ROM.
    pub basic: bool,
    /// $E000-$FFFF maps KERNAL ROM.
    pub kernal: bool,
    /// $D000-$DFFF maps I/O.
    pub io: bool,
    /// $D000-$DFFF maps CHARGEN ROM (when !io).
    pub char_rom: bool,
}

/// Build the 32-entry memconfig table exactly like memory-bus.ts
/// `buildMemConfigTable` for the stock no-cart C64 (GAME=EXROM=1 ⇒ never ultimax
/// for the configs the boot uses). Index = LORAM|HIRAM<<1|CHAREN<<2|EXROM<<3|GAME<<4.
pub fn build_memconfig_table() -> [MemConfig; 32] {
    let mut table = [MemConfig { basic: false, kernal: false, io: false, char_rom: false }; 32];
    for (idx, entry) in table.iter_mut().enumerate() {
        let loram = idx & 0x01 != 0;
        let hiram = idx & 0x02 != 0;
        let charen = idx & 0x04 != 0;
        let exrom = idx & 0x08 != 0;
        let game = idx & 0x10 != 0;
        let ultimax = !game && exrom;

        // bankA — BASIC visible only when LORAM && HIRAM (no cart).
        let basic = !ultimax && loram && hiram;
        // bankE — KERNAL visible when HIRAM (no cart).
        let kernal = !ultimax && hiram;
        // bankD — IO when (LORAM||HIRAM) && CHAREN; CHAR when (LORAM||HIRAM) && !CHAREN.
        let io = ultimax || ((loram || hiram) && charen);
        let char_rom = !ultimax && (loram || hiram) && !charen;
        *entry = MemConfig { basic, kernal, io, char_rom };
    }
    table
}

/// The assembled full-C64 bus, borrowing every subsystem from the `Machine`.
/// One per run (built in `run_for_full*`), so the borrows are scoped to the loop.
pub struct FullBus<'a> {
    pub ram: &'a mut [u8; 0x10000],
    pub basic_rom: &'a [u8; 0x2000],
    pub kernal_rom: &'a [u8; 0x2000],
    pub char_rom: &'a [u8; 0x1000],
    /// IO register shadow ($D000-$DFFF) for open-bus reads of unclaimed regs.
    pub io: &'a mut [u8; 0x1000],
    pub vic: &'a mut VicII,
    pub cia1: &'a mut Cia,
    pub cia2: &'a mut Cia,
    pub cia_table: &'a [u16; CIAT_TABLEN],
    /// 25-byte SID register shadow ($D400-$D418) — write-only store for parity.
    pub sid_regs: &'a mut [u8; 32],
    /// Live memconfig (selected by $00/$01 writes).
    pub config: MemConfig,
    pub memconfig_table: &'a [MemConfig; 32],
    /// CPU-port latches.
    pub port_dir: u8,
    pub port_data: u8,
    /// Master clock, advanced by `tick` (shared with CPU + CIAs).
    pub clk: u64,
}

impl<'a> FullBus<'a> {
    /// Recompute the live memconfig from the $00/$01 latches (= memPlaConfigChanged).
    #[inline]
    fn pla_config_changed(&mut self) {
        // port = (~dir | data) & 7 — input pins pulled HIGH.
        let port = (!self.port_dir | self.port_data) & 0x07;
        // No cart: EXROM=1 (bit3), GAME=1 (bit4).
        let idx = (port | 0x18) as usize & 0x1f;
        self.config = self.memconfig_table[idx];
    }

    /// VICE computeCpuPortDataRead simplified for the no-datasette stock C64
    /// (capacitor bits 6,7 → 0 in input mode; the boot ROM never relies on decay).
    #[inline]
    fn cpu_port_data_read(&self) -> u8 {
        let dir = self.port_dir;
        let data = self.port_data;
        let pullup = 0x17u8;
        let data_out = data & dir;
        let mut retval = (data | !dir) & (data_out | pullup);
        // Bit 5 (CASS_MOTOR): cleared in input mode (no datasette pullup).
        if dir & 0x20 == 0 {
            retval &= 0xdf;
        }
        // Bits 6,7: input mode → 0 (capacitor decayed / never charged at boot).
        if dir & 0x40 == 0 {
            retval &= !0x40;
        }
        if dir & 0x80 == 0 {
            retval &= !0x80;
        }
        retval
    }

    /// I/O read dispatch ($D000-$DFFF, IO config). Mirrors memory-bus.ts read().
    #[inline]
    fn io_read(&mut self, addr: u16) -> u8 {
        match addr {
            0xd000..=0xd3ff => self.vic.read_reg(addr as u8),
            0xd400..=0xd7ff => {
                // SID: 32-byte register mirror every $20. Reads are mostly open
                // bus / write-only; return the shadow (regs 0x19/0x1a/0x1b/0x1c
                // would be live on real HW — out of scope, shadow suffices).
                self.sid_regs[(addr as usize - 0xd400) & 0x1f]
            }
            0xd800..=0xdbff => {
                // Color RAM: low nibble stored in `io` shadow, high nibble open bus ($F0).
                let v = self.io[(addr as usize) - 0xd000];
                (v & 0x0f) | 0xf0
            }
            0xdc00..=0xdcff => self.cia1.read(addr, self.clk, self.cia_table),
            0xdd00..=0xddff => self.cia2.read(addr, self.clk, self.cia_table),
            // $DE00-$DFFF (cart IO, no cart) → open-bus shadow.
            _ => self.io[(addr as usize) - 0xd000],
        }
    }

    /// I/O write dispatch ($D000-$DFFF, IO config).
    #[inline]
    fn io_write(&mut self, addr: u16, value: u8) {
        // Keep the open-bus shadow for unclaimed-register reads.
        self.io[(addr as usize) - 0xd000] = value;
        match addr {
            0xd000..=0xd3ff => self.vic.write_reg(addr as u8, value),
            0xd400..=0xd7ff => {
                self.sid_regs[(addr as usize - 0xd400) & 0x1f] = value;
            }
            0xd800..=0xdbff => { /* color RAM: shadow already stored above */ }
            0xdc00..=0xdcff => self.cia1.write(addr, value, self.clk, self.cia_table),
            0xdd00..=0xddff => self.cia2.write(addr, value, self.clk, self.cia_table),
            _ => { /* cart IO (none) */ }
        }
    }
}

impl<'a> Bus for FullBus<'a> {
    #[inline]
    fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000 => self.port_dir,
            0x0001 => self.cpu_port_data_read(),
            0x0002..=0x9fff => self.ram[addr as usize],
            0xa000..=0xbfff => {
                if self.config.basic {
                    self.basic_rom[(addr as usize) - 0xa000]
                } else {
                    self.ram[addr as usize]
                }
            }
            0xc000..=0xcfff => self.ram[addr as usize],
            0xd000..=0xdfff => {
                if self.config.io {
                    self.io_read(addr)
                } else if self.config.char_rom {
                    self.char_rom[(addr as usize) - 0xd000]
                } else {
                    self.ram[addr as usize]
                }
            }
            0xe000..=0xffff => {
                if self.config.kernal {
                    self.kernal_rom[(addr as usize) - 0xe000]
                } else {
                    self.ram[addr as usize]
                }
            }
        }
    }

    #[inline]
    fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0x0000 => {
                self.port_dir = value;
                self.ram[0] = value;
                self.pla_config_changed();
            }
            0x0001 => {
                self.port_data = value;
                self.ram[1] = value;
                self.pla_config_changed();
            }
            0xd000..=0xdfff => {
                if self.config.io {
                    self.io_write(addr, value);
                } else {
                    // char-ROM / RAM config: write lands in RAM underneath.
                    self.ram[addr as usize] = value;
                }
            }
            // ROM windows + all RAM: writes always land in the RAM underneath
            // (ROM is read-only; the RAM byte beneath stays writable — the trace
            // `old` byte for $A000-$BFFF reads this RAM, not BASIC ROM).
            _ => self.ram[addr as usize] = value,
        }
    }

    /// One VIC master cycle per CPU master cycle (= c64ViciiCycle hook); advance
    /// the shared master clock + both CIAs' clk in lockstep.
    #[inline]
    fn tick(&mut self) {
        self.vic.tick();
        self.clk = self.clk.wrapping_add(1);
        self.cia1.clk = self.clk;
        self.cia2.clk = self.clk;
        self.cia1.tick(self.cia_table);
        self.cia2.tick(self.cia_table);
    }

    /// VICE check_ba(): stall the CPU read while VIC BA is low (badline / sprite
    /// DMA), stealing cycles + advancing the VIC. Returns the stolen-cycle count.
    /// The stolen cycles also advance the shared clk + CIAs (folded in here so the
    /// CIA timers stay phase-aligned with the stretched CPU read).
    #[inline]
    fn check_ba_before_read(&mut self) -> u32 {
        let stolen = self.vic.steal_cycles();
        if stolen != 0 {
            self.clk = self.clk.wrapping_add(stolen as u64);
            self.cia1.clk = self.clk;
            self.cia2.clk = self.clk;
        }
        stolen
    }
}
