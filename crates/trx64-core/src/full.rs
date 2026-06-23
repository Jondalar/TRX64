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
use crate::sid::Sid6581;
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
    /// 32-byte SID register shadow ($D400-$D41F) — write-only store for parity.
    pub sid_regs: &'a mut [u8; 32],
    /// SID 6581 oscillator + envelope state machine (for $D41B osc3 / $D41C env3).
    pub sid: &'a mut Sid6581,
    /// Live memconfig (selected by $00/$01 writes).
    pub config: MemConfig,
    pub memconfig_table: &'a [MemConfig; 32],
    /// CPU-port latches.
    pub port_dir: u8,
    pub port_data: u8,
    /// Master clock, advanced by `tick` (shared with CPU + CIAs).
    pub clk: u64,
    /// Last CIA2 port-A OUTPUT byte pushed to $DD00 (= IEC/VIC-bank). A write to
    /// $DD00/$DD02 that changes the composed output re-pushes it as a $DD00 bus
    /// write (TS `iecWrite`). Seeded from the live CIA2 PA output so the FIRST
    /// change is detected correctly.
    pub cia2_pa_out: u8,
    /// Side-effect writes queued by the immediately-preceding `write()`
    /// (`(addr, value, old)`), drained by the CPU `store` into the trace.
    pub side_effects: Vec<(u16, u8, u8)>,
    /// Side-effect reads queued by the immediately-preceding `read()`
    /// (`(addr, value)`), drained by the CPU `load_read` into the trace. Carries
    /// the IEC `iecReadPins` indirection record on a CIA2 PA ($DD00) read.
    pub read_side_effects: Vec<(u16, u8)>,
    /// The 1541 drive — borrowed so a $DD00 access can push-flush it to the exact
    /// C64 clock before sampling/applying the IEC lines (cross-domain sync).
    pub drive: &'a mut crate::drive::Drive1541,
    /// IEC wired-AND core (C64 CIA2 PA ↔ drive VIA1 PB), borrowed from the Machine.
    pub iec: &'a mut crate::iec::IecCore,
    /// Keyboard matrix (CIA1 PA column drive ↔ PB row read). Read on a $DC01
    /// access to inject queued `session/type` key presses.
    pub keyboard: &'a crate::keyboard::KeyboardMatrix,
    /// Monotonic C64-clock the drive has been advanced up to (push-flush reference).
    pub drive_c64_ref: u64,
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

    /// Push-flush the drive to the current C64 `clk`, then refresh the IEC core's
    /// drive-side contribution from the drive's live VIA1 PB output and re-fold the
    /// bus. Mirrors VICE `drive_cpu_execute_{one,all}(clock)` followed by the
    /// drv_data[8]/drv_bus[8]/cpu_port recompute that the kernel overlay performs at
    /// each $DD00 read/write instant.
    #[inline]
    fn iec_push_flush(&mut self) {
        self.iec_push_flush_to(self.clk);
    }

    /// Push-flush the drive to an explicit C64-clock target. VICE passes
    /// `maincpu_clk` on a $DD00 READ (iecbus_cpu_read_conf1) but
    /// `maincpu_clk + !write_offset` = `maincpu_clk + 1` on a $DD00 WRITE
    /// (iecbus_cpu_write_conf1, x64sc write_offset=0 — c64cia2.c:162). The extra
    /// cycle on writes shifts the drive's sampling instant by one C64 cycle, which
    /// the IEC handshake timing depends on.
    #[inline]
    fn iec_push_flush_to(&mut self, target: u64) {
        // Feed the drive the current bus state so a `$1800` PB read during its
        // catch-up run sees the live C64-driven CLK/DATA/ATN lines.
        self.drive.iec_drv_port = self.iec.drv_port;
        self.drive_c64_ref = self.drive.catch_up_to(target, self.drive_c64_ref);
        let pb_out = self.drive.via1_pb_iec_output();
        self.iec.drive_store_pb(pb_out);
    }

    /// I/O read dispatch ($D000-$DFFF, IO config). Mirrors memory-bus.ts read().
    #[inline]
    fn io_read(&mut self, addr: u16) -> u8 {
        match addr {
            0xd000..=0xd3ff => self.vic.read_reg(addr as u8),
            0xd400..=0xd7ff => {
                // SID: 32-byte register mirror every $20.
                // $D419/$D41A (POT X/Y) → 0x80 (unconnected default).
                // $D41B (OSC3) → voice-3 oscillator output MSB (live computed).
                // $D41C (ENV3) → voice-3 envelope value (live computed).
                // All other registers: write-only shadow byte (B-level round-trip).
                let reg = (addr as usize - 0xd400) & 0x1f;
                self.sid.read(reg, self.sid_regs)
            }
            0xd800..=0xdbff => {
                // Color RAM: low nibble stored in `io` shadow, high nibble open bus ($F0).
                let v = self.io[(addr as usize) - 0xd000];
                (v & 0x0f) | 0xf0
            }
            0xdc00..=0xdcff => {
                // CIA1 PB ($DC01) carries the keyboard ROW lines. VICE
                // c64cia1.c:425-431 read_ciapb: byte = (val & (PRB|~DDRB)) |
                // (DDRB & PRB), then ANDed with joystick-port-1 (none here).
                // `val` = keyboard row pull-down for the PA column drive
                // (paOut = PRA|~DDRA). KERNAL programs DDRB=0 so this collapses
                // to `val`, but we compute the full formula for fidelity.
                if (addr & 0xf) == crate::cia::CIA_PRB as u16 {
                    let pra = self.cia1.peek(0xdc00);
                    let ddra = self.cia1.peek(0xdc02);
                    let prb = self.cia1.peek(0xdc01);
                    let ddrb = self.cia1.peek(0xdc03);
                    let pa_out = (pra | !ddra) & 0xff;
                    let val = self.keyboard.read_rows_for_pa(self.clk, pa_out);
                    let val_out_hi = ddrb & prb;
                    (val & ((prb | !ddrb) & 0xff)) | val_out_hi
                } else {
                    self.cia1.read(addr, self.clk, self.cia_table)
                }
            }
            0xdd00..=0xddff => {
                // CIA2 register 0 = port A ($DD00) carries the IEC input lines on
                // bits 6/7. VICE read_ciapa: value = ((PRA|~DDRA)&0x3f) |
                // iecbus_callback_read(clk). The callback push-flushes the drive,
                // re-folds the wired-AND bus, and returns the cached cpu_port —
                // and (via iecReadPins → c64Read($DD00) → emitC64Access) emits an
                // EXTRA bus-access read record of cpu_port BEFORE the CPU's own
                // load record. We reproduce both: the indirection record (queued as
                // a read side-effect) and the composed PA byte.
                if (addr & 0xf) == crate::cia::CIA_PRA as u16 {
                    self.iec_push_flush();
                    let pins = self.iec.cpu_port;
                    // iecReadPins indirection record (= emitC64Access read at $DD00).
                    self.read_side_effects.push((0xdd00, pins));
                    let pra = self.cia2.peek(0xdd00);
                    let ddra = self.cia2.peek(0xdd02);
                    (((pra | !ddra) & 0x3f) | pins) & 0xff
                } else {
                    self.cia2.read(addr, self.clk, self.cia_table)
                }
            }
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
                let reg = (addr as usize - 0xd400) & 0x1f;
                self.sid_regs[reg] = value;
                self.sid.write(reg, value, self.sid_regs);
            }
            0xd800..=0xdbff => { /* color RAM: shadow already stored above */ }
            0xdc00..=0xdcff => self.cia1.write(addr, value, self.clk, self.cia_table),
            0xdd00..=0xddff => {
                self.cia2.write(addr, value, self.clk, self.cia_table);
                // CIA2 port-A output drives the IEC bus + VIC bank. A $DD00 (PRA)
                // or $DD02 (DDRA) write that changes the composed output re-pushes
                // it to $DD00 (= TS iecWrite → c64Write($DD00, or)). The push is
                // recorded BEFORE the originating store's own trace record.
                let reg = (addr & 0xf) as usize;
                if reg == crate::cia::CIA_PRA || reg == crate::cia::CIA_DDRA {
                    let new_out = self.cia2.pa_output();
                    if new_out != self.cia2_pa_out {
                        // The $DD00 IO shadow becomes the new output; `old` = prior
                        // shadow at $DD00 (the trace old byte for an IO write is
                        // omitted anyway — hasOld=0 for $D000-$DFFF — so 0 is fine).
                        let old = self.io[0xdd00 - 0xd000];
                        self.io[0xdd00 - 0xd000] = new_out;
                        self.cia2_pa_out = new_out;
                        self.side_effects.push((0xdd00, new_out, old));
                        // IEC: drive the wired-AND bus from the new CIA2 PA output.
                        // VICE iecbus_cpu_write_conf1 order: push-flush drive to the
                        // write instant FIRST, then iec_update_cpu_bus(~PA), ATN edge
                        // → drive VIA1 CA1, recompute drv_bus[8], update ports. The
                        // write instant is maincpu_clk + 1 (x64sc write_offset=0).
                        self.iec_push_flush_to(self.clk + 1);
                        let atn_edge = self.iec.c64_store_dd00((!new_out) & 0xff);
                        // ATN-edge → drive VIA1 CA1: the C64 driving ATN raises the
                        // drive's attention IRQ (DOS $FE67 → $E85B). VICE
                        // iecbus_cpu_write_conf1 signals VIA1 CA1 right after the
                        // ATN-edge detect, before the drv_bus recompute. We stamp it at
                        // the drive clock the push-flush just reached. The hardware
                        // ATN-acknowledge (drive auto-pulls DATA) is already folded by
                        // recompute_drv_bus's cpu_bus term inside c64_store_dd00.
                        if let Some(atn_high) = atn_edge {
                            let dclk = self.drive.drive_clk;
                            self.drive.atn_edge_to_via1_ca1(atn_high, dclk);
                        }
                    }
                }
            }
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

    #[inline]
    fn take_side_effect_writes(&mut self, out: &mut Vec<(u16, u8, u8)>) {
        if !self.side_effects.is_empty() {
            out.append(&mut self.side_effects);
        }
    }

    #[inline]
    fn take_side_effect_reads(&mut self, out: &mut Vec<(u16, u8)>) {
        if !self.read_side_effects.is_empty() {
            out.append(&mut self.read_side_effects);
        }
    }
}
