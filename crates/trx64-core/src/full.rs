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

/// $8000-$9FFF mapping (= memory-bus.ts MemConfigEntry.bank8, ts:62).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Bank8 {
    Ram,
    CartLo,
}

/// $A000-$BFFF mapping (= MemConfigEntry.bankA, ts:64).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BankA {
    Ram,
    Basic,
    CartHi,
}

/// $E000-$FFFF mapping (= MemConfigEntry.bankE, ts:68).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BankE {
    Ram,
    Kernal,
    CartHiUltimax,
}

/// One pre-built memconfig entry (= memory-bus.ts MemConfigEntry). The bank8/
/// bank_a/bank_e enums carry the cartridge windows (cart_lo / cart_hi /
/// cart_hi_ultimax) exactly like the TS string-enums; the `basic`/`kernal`/`io`/
/// `char_rom` booleans are KEPT (derived from the enums) so the no-cart read/write
/// path stays byte-identical to the pre-cart code.
#[derive(Clone, Copy)]
pub struct MemConfig {
    /// $8000-$9FFF: RAM or cart ROML.
    pub bank8: Bank8,
    /// $A000-$BFFF: RAM / BASIC / cart ROMH.
    pub bank_a: BankA,
    /// $E000-$FFFF: RAM / KERNAL / cart ROMH (ultimax).
    pub bank_e: BankE,
    /// $A000-$BFFF maps BASIC ROM (= bank_a == Basic).
    pub basic: bool,
    /// $E000-$FFFF maps KERNAL ROM (= bank_e == Kernal).
    pub kernal: bool,
    /// $D000-$DFFF maps I/O (= bankD == io).
    pub io: bool,
    /// $D000-$DFFF maps CHARGEN ROM (when !io) (= bankD == char).
    pub char_rom: bool,
    /// True when GAME=0 && EXROM=1 (ultimax board overlay). Drives the open-bus
    /// windows ($1000-$7FFF, $C000-$CFFF, the $A000/$E000 non-cart-hi fallthrough).
    pub ultimax: bool,
}

/// Build the 32-entry memconfig table exactly like memory-bus.ts
/// `buildMemConfigTable` (ts:987-1019). Index = LORAM|HIRAM<<1|CHAREN<<2|
/// EXROM<<3|GAME<<4. The cart windows (cart_lo / cart_hi / cart_hi_ultimax) are
/// set per ultimax / 16K-cart rules; the booleans are derived so the no-cart path
/// (EXROM=GAME=1 ⇒ idx 0x18-0x1f, never ultimax, never cart) is byte-identical to
/// the prior table.
pub fn build_memconfig_table() -> [MemConfig; 32] {
    let mut table = [MemConfig {
        bank8: Bank8::Ram,
        bank_a: BankA::Ram,
        bank_e: BankE::Ram,
        basic: false,
        kernal: false,
        io: false,
        char_rom: false,
        ultimax: false,
    }; 32];
    for (idx, entry) in table.iter_mut().enumerate() {
        let loram = idx & 0x01 != 0;
        let hiram = idx & 0x02 != 0;
        let charen = idx & 0x04 != 0;
        let exrom = idx & 0x08 != 0;
        let game = idx & 0x10 != 0;
        // ts:996 — Ultimax mode: GAME=0 AND EXROM=1.
        let ultimax = !game && exrom;

        // ts:998-1000 — bank8 (kept as two separate conditions verbatim with the
        // TS `if (ultimax) ...; else if (loram && hiram && !exrom) ...`).
        #[allow(clippy::if_same_then_else)]
        let bank8 = if ultimax {
            Bank8::CartLo
        } else if loram && hiram && !exrom {
            Bank8::CartLo
        } else {
            Bank8::Ram
        };

        // ts:1002-1005 — bankA.
        let bank_a = if ultimax {
            BankA::Ram // unmapped in Ultimax
        } else if loram && hiram && !exrom && !game {
            BankA::CartHi // 16K cart
        } else if loram && hiram {
            BankA::Basic
        } else {
            BankA::Ram
        };

        // ts:1007-1009 — bankD.
        let io = if ultimax {
            true // I/O always in Ultimax
        } else {
            (loram || hiram) && charen
        };
        let char_rom = !ultimax && (loram || hiram) && !charen;

        // ts:1011-1012 — bankE.
        let bank_e = if ultimax {
            BankE::CartHiUltimax
        } else if hiram {
            BankE::Kernal
        } else {
            BankE::Ram
        };

        *entry = MemConfig {
            bank8,
            bank_a,
            bank_e,
            basic: matches!(bank_a, BankA::Basic),
            kernal: matches!(bank_e, BankE::Kernal),
            io,
            char_rom,
            ultimax,
        };
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
    /// The attached cartridge mapper (= memory-bus.ts `cartridge`, ts:118), or
    /// None for the stock no-cart machine. Borrowed `&mut` because a $DE00-$DFFF
    /// IO write mutates the mapper's bank/control register. The mapper is consulted
    /// SYNCHRONOUSLY from read()/write() (a per-access bus hook, NOT a clocked
    /// device — no tick). When None the banking is exactly the pre-cart path.
    pub cartridge: Option<&'a mut Box<dyn crate::cart::CartMapper>>,
}

impl<'a> FullBus<'a> {
    /// Recompute the live memconfig from the $00/$01 latches + cartridge EXROM/GAME
    /// lines (= memory-bus.ts memPlaConfigChanged, ts:854-871, VERBATIM).
    #[inline]
    pub fn pla_config_changed(&mut self) {
        // ts:855 — port = (~dir | data) & 7 — input pins pulled HIGH.
        let port = ((!self.port_dir | self.port_data) & 0x07) as usize;
        let loram = port & 0x01;
        let hiram = (port >> 1) & 0x01;
        let charen = (port >> 2) & 0x01;
        // ts:858-862 — cartridge lines; no cart: EXROM=1, GAME=1 (released).
        let (exrom, game) = match self.cartridge.as_ref() {
            Some(c) => {
                let lines = c.get_lines();
                ((lines.exrom & 1) as usize, (lines.game & 1) as usize)
            }
            None => (1usize, 1usize),
        };
        // ts:869 — idx = LORAM | HIRAM<<1 | CHAREN<<2 | EXROM<<3 | GAME<<4.
        let idx = (loram | (hiram << 1) | (charen << 2) | (exrom << 3) | (game << 4)) & 0x1f;
        self.config = self.memconfig_table[idx];
    }

    /// ts:237-251 — getBankInfo: the banking-context struct passed to every
    /// cartridge read/write/peek. The read-only mappers only read
    /// cpu_port_direction/value (and ignore them for banking), so this is mostly
    /// carried for 1:1 fidelity.
    #[inline]
    fn get_bank_info(&self) -> crate::cart::BankInfo {
        let lines = self.cartridge.as_ref().map(|c| c.get_lines());
        crate::cart::BankInfo {
            cpu_port_direction: self.port_dir,
            cpu_port_value: self.port_data,
            basic_visible: self.config.basic,
            kernal_visible: self.config.kernal,
            io_visible: self.config.io,
            char_visible: self.config.char_rom,
            cartridge_attached: self.cartridge.is_some(),
            cartridge_exrom: lines.map(|l| l.exrom),
            cartridge_game: lines.map(|l| l.game),
            // GMOD2's IO1 read mixes the EEPROM DO bit with open-bus low bits; the
            // stock C64 float bus is 0xFF here (= open_bus()). Read-only mappers
            // ignore phi1.
            phi1: 0xff,
        }
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
        self.iec_catch_up_to(target);
        // READ path (and end-of-instruction): a full re-fold publishes the live
        // `cpu_port` the C64 reads at $DD00 bits 6/7 (= VICE iecbus_cpu_read_conf1
        // returning the freshly-folded cpu_port). The cpu_bus is unchanged here, so
        // this single fold against the current cpu_bus is exact.
        // VICE via1d1541.c store_prb / iec_drive_write(~byte): the drive's PB
        // output folds into the bus as `~pb_out` (iec.rs iec_drive_write inverts via
        // the `~data ^ cpu_bus` formula by receiving the already-inverted byte).
        let pb_out = self.drive.via1_pb_iec_output();
        self.iec.iec_drive_write((!pb_out) & 0xff, 0);
    }

    /// Catch the drive up to `target` and refresh `drv_data_8` from its live VIA1
    /// PB output WITHOUT folding the wired-AND bus. The WRITE path uses this: it
    /// must re-read the drive pull (the catch-up's `$1800` stores never propagate
    /// `drv_data_8` into the shared IEC core in TRX64), but the wired-AND fold is
    /// then performed exactly ONCE by `c64_store_dd00` against the NEW cpu_bus —
    /// matching VICE `iecbus_cpu_write_conf1` (execute_one → update_cpu_bus →
    /// ATN-edge → single drv_bus recompute → update_ports). The extra fold that
    /// `iec_push_flush_to` performs against the OLD pre-write cpu_bus is what
    /// transiently wedged the $04E2 BIT $DD00 / BVC handshake loop.
    #[inline]
    fn iec_catch_up_to(&mut self, target: u64) {
        // Feed the drive the current bus state so a `$1800` PB read during its
        // catch-up run sees the live C64-driven CLK/DATA/ATN lines. Also feed the
        // C64-side intent (cpu_bus) — constant across this catch-up — so a `$1800`
        // STORE inside the run re-folds the wired-AND and the drive sees its own
        // CLK/DATA pull on the next read (= via1d1541.c store_prb cross-domain sync).
        self.drive.iec_drv_port = self.iec.iecbus.drv_port;
        self.drive.iec_cpu_bus = self.iec.iecbus.cpu_bus;
        self.drive_c64_ref = self.drive.catch_up_to(target, self.drive_c64_ref);
        let pb_out = self.drive.via1_pb_iec_output();
        self.iec.drive_set_data_no_fold(pb_out);
    }

    /// I/O read dispatch ($D000-$DFFF, IO config). Mirrors memory-bus.ts read().
    #[inline]
    fn io_read(&mut self, addr: u16) -> u8 {
        match addr {
            0xd000..=0xd3ff => {
                // $D01E (sprite-sprite) / $D01F (sprite-background) collision
                // registers (mirrored every $40): compute the live collision
                // latches from the current frozen state, fire the collision IRQ on
                // the 0→nonzero edge, then read-clear (read_reg_mut). Every other
                // VIC register is an ordinary register read.
                match (addr as u8) & 0x3f {
                    0x1e | 0x1f => {
                        self.recompute_collisions();
                        self.vic.read_reg_mut(addr as u8)
                    }
                    _ => self.vic.read_reg(addr as u8),
                }
            }
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
                    // = iecbus_cpu_read_conf1(clk): returns the freshly-folded
                    // cpu_port (the drive catch-up was done by iec_push_flush).
                    let pins = self.iec.iecbus_callback_read(self.clk);
                    // iecReadPins indirection record (= emitC64Access read at $DD00).
                    self.read_side_effects.push((0xdd00, pins));
                    let pra = self.cia2.peek(0xdd00);
                    let ddra = self.cia2.peek(0xdd02);
                    (((pra | !ddra) & 0x3f) | pins) & 0xff
                } else {
                    self.cia2.read(addr, self.clk, self.cia_table)
                }
            }
            // $DE00-$DFFF — cart IO1/IO2 (ts:407-410). The cart is consulted ONLY
            // when I/O is visible (guaranteed: io_read is reached only via the io
            // branch). Some ⇒ the mapper byte; None (or no cart) ⇒ the open-bus
            // shadow. The read-only mappers (MagicDesk/Ocean) are write-only here
            // (no IO read) so they return None ⇒ shadow, byte-identical to no-cart.
            _ => {
                if (0xde00..=0xdfff).contains(&addr) {
                    if let Some(v) = self.cart_read(addr) {
                        return v;
                    }
                }
                self.io[(addr as usize) - 0xd000]
            }
        }
    }

    /// VIC bank base from CIA2 port-A bits 0-1 (= Machine::vic_bank_base):
    /// `bank = 3 - (PA & DDRA & 3); base = bank * $4000`. Used by the per-cycle
    /// VIC fetch view (tick/check_ba) + the static collision recompute.
    #[inline]
    pub(crate) fn vic_bank_base(&self) -> u16 {
        let pra = self.cia2.peek(0xdd00);
        let ddra = self.cia2.peek(0xdd02);
        let bank = ((pra & ddra & 0x03) ^ 0x03) as u16;
        bank.wrapping_mul(0x4000)
    }

    /// Recompute the $D01E/$D01F collision latches from the current frozen state
    /// and merge them into the VIC (firing the collision IRQ on the 0→nonzero
    /// edge). Called when the CPU reads $D01E/$D01F so a polled collision register
    /// reflects the rendered sprite/graphics overlap of the current frame.
    ///
    /// The masks are produced by the static pixel renderer (`render_collisions`),
    /// the same source the screenshot pipeline uses, so the collision bits are
    /// pixel-consistent with the displayed frame. `apply_collisions` ports the
    /// VICE edge-trigger (vicii-cycle.c:407-433) verbatim.
    fn recompute_collisions(&mut self) {
        // Colour RAM low nibbles live in the I/O shadow at $D800-$DBFF.
        let mut color_ram = [0u8; 0x0400];
        for (i, c) in color_ram.iter_mut().enumerate() {
            *c = self.io[0x0800 + i] & 0x0f;
        }
        // VIC bank base from CIA2 port-A bits 0-1 (= Machine::vic_bank_base).
        let pra = self.cia2.peek(0xdd00);
        let ddra = self.cia2.peek(0xdd02);
        let bank = ((pra & ddra & 0x03) ^ 0x03) as u16;
        let bank_base = bank.wrapping_mul(0x4000);

        let inp = crate::render::RenderInput {
            regs: &self.vic.regs,
            ram: self.ram,
            char_rom: self.char_rom,
            color_ram: &color_ram,
            bank_base,
        };
        let (ss, sb) = crate::render::render_collisions(&inp);
        self.vic.apply_collisions(ss, sb);
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
                        // VICE iecbus_cpu_write_conf1 order: execute_one(drive catch-up)
                        // to the write instant FIRST, then iec_update_cpu_bus(~PA), ATN
                        // edge → drive VIA1 CA1, recompute drv_bus[8], update ports — a
                        // SINGLE fold against the NEW cpu_bus. We catch the drive up and
                        // refresh drv_data_8 from its live PB (no fold here), then let
                        // `c64_store_dd00` perform that one authoritative fold. The
                        // write instant is maincpu_clk + 1 (x64sc write_offset=0).
                        self.iec_catch_up_to(self.clk + 1);
                        // = (*iecbus_callback_write)(~PA, clk) → iecbus_cpu_write_conf1
                        // for the single-1541 (Conf1): iec_update_cpu_bus → ATN-edge →
                        // per-type drv_bus[8] recompute → iec_update_ports. Returns the
                        // ATN edge(s) to deliver to the drive VIA1 (the inline VICE
                        // `viacore_signal(via1d1541, VIA_SIG_CA1, ...)`).
                        let atn_edges = self.iec.iecbus_callback_write((!new_out) & 0xff, self.clk + 1);
                        // ATN-edge → drive VIA1 CA1: the C64 driving ATN raises the
                        // drive's attention IRQ (DOS $FE67 → $E85B). VICE
                        // iecbus_cpu_write_conf1 signals VIA1 CA1 right after the
                        // ATN-edge detect, before the drv_bus recompute. We stamp it at
                        // the drive clock the push-flush just reached. The hardware
                        // ATN-acknowledge (drive auto-pulls DATA) is already folded by
                        // the recompute_drv_bus cpu_bus term inside the conf1 write.
                        for (_dnr, edge) in atn_edges {
                            if let crate::iec::AtnEdge::Via1Ca1 { sig } = edge {
                                let dclk = self.drive.drive_clk;
                                self.drive.atn_edge_to_via1_ca1(sig, dclk);
                            }
                            // Other AtnEdge variants (1581/2000/4000/CMDHD) are
                            // unreachable in the single-1541 shape (unit 8 = Drive1541).
                        }
                    }
                }
            }
            // $DE00-$DFFF — cart IO1/IO2 write (ts:572-581). A consumed write can
            // change EXROM/GAME (the bank/disable register), so re-run the PLA
            // reconfig. A non-consumed write (or no cart) falls to the io shadow
            // (already stored at the top of io_write) — byte-identical to no-cart.
            _ => {
                if (0xde00..=0xdfff).contains(&addr) && self.cart_write(addr, value) {
                    self.pla_config_changed();
                }
            }
        }
    }
}

impl<'a> FullBus<'a> {
    /// ts:105 — openBusProvider default `() => 0xff`. The VIC float-bus value the
    /// ultimax open windows read. (No phi1 source wired in this read-only tier.)
    #[inline]
    fn open_bus(&self) -> u8 {
        0xff
    }

    /// Consult the attached cartridge mapper's read window (ts:
    /// `this.cartridge?.read(normalized, this.getBankInfo())`). None ⇒ no cart or
    /// the mapper does not handle this address (falls through to RAM / open-bus).
    /// `&mut self` + `self.clk` for the writable flash tier (flash reads advance
    /// the command FSM / latch DQ status / catch the erase alarm up); the
    /// read-only mappers ignore both and do a pure array index.
    #[inline]
    fn cart_read(&mut self, addr: u16) -> Option<u8> {
        let bi = self.get_bank_info();
        let clk = self.clk;
        match self.cartridge.as_mut() {
            Some(c) => c.read(addr, &bi, clk),
            None => None,
        }
    }

    /// Drive a write through the cartridge mapper (ts:
    /// `this.cartridge?.write(normalized, byte, bankInfo)`). Returns whether the
    /// cart CONSUMED the write (true ⇒ does not fall through to RAM). `bank_info`
    /// is computed BEFORE the mutable borrow (it only needs the live lines); `clk`
    /// drives the flash erase-alarm schedule.
    #[inline]
    fn cart_write(&mut self, addr: u16, value: u8) -> bool {
        let bi = self.get_bank_info();
        let clk = self.clk;
        match self.cartridge.as_mut() {
            Some(c) => c.write(addr, value, &bi, clk),
            None => false,
        }
    }
}

impl<'a> Bus for FullBus<'a> {
    #[inline]
    fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000 => self.port_dir,
            0x0001 => self.cpu_port_data_read(),
            // $1000-$7FFF — ultimax open bus (board != MAX keeps $0000-$0FFF RAM).
            // No-cart: ultimax=false ⇒ RAM (byte-identical to the prior path).
            0x0002..=0x7fff => {
                if self.config.ultimax && addr >= 0x1000 {
                    self.open_bus()
                } else {
                    self.ram[addr as usize]
                }
            }
            // $8000-$9FFF — ROML when bank8==CartLo (8k/16k/ultimax); else RAM.
            0x8000..=0x9fff => {
                if matches!(self.config.bank8, Bank8::CartLo) {
                    if let Some(v) = self.cart_read(addr) {
                        return v;
                    }
                }
                self.ram[addr as usize]
            }
            // $A000-$BFFF — ts:374-388: cart_hi ⇒ ROMH; basic ⇒ BASIC; ultimax
            // (non-cart-hi) ⇒ open bus; else RAM.
            0xa000..=0xbfff => {
                if matches!(self.config.bank_a, BankA::CartHi) {
                    if let Some(v) = self.cart_read(addr) {
                        return v;
                    }
                    self.ram[addr as usize]
                } else if self.config.basic {
                    self.basic_rom[(addr as usize) - 0xa000]
                } else if self.config.ultimax {
                    self.open_bus()
                } else {
                    self.ram[addr as usize]
                }
            }
            // $C000-$CFFF — ts:390-401: ultimax ⇒ open bus; else RAM.
            0xc000..=0xcfff => {
                if self.config.ultimax {
                    self.open_bus()
                } else {
                    self.ram[addr as usize]
                }
            }
            0xd000..=0xdfff => {
                if self.config.io {
                    self.io_read(addr)
                } else if self.config.char_rom {
                    self.char_rom[(addr as usize) - 0xd000]
                } else {
                    self.ram[addr as usize]
                }
            }
            // $E000-$FFFF — ts:463-481: KERNAL; else cart_hi_ultimax ⇒ ROMH
            // (open bus if the mapper returns None); else RAM.
            0xe000..=0xffff => {
                if self.config.kernal {
                    self.kernal_rom[(addr as usize) - 0xe000]
                } else if matches!(self.config.bank_e, BankE::CartHiUltimax) {
                    if let Some(v) = self.cart_read(addr) {
                        return v;
                    }
                    self.open_bus()
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
            // $8000-$9FFF — ts:597-606: bank8==CartLo ⇒ the cart may consume
            // (flash); read-only mappers return false ⇒ fall to RAM. ROM is
            // read-only so the RAM byte beneath stays writable (the trace `old`
            // byte reads this RAM, not the cart ROM).
            0x8000..=0x9fff => {
                if matches!(self.config.bank8, Bank8::CartLo) && self.cart_write(addr, value) {
                    return;
                }
                self.ram[addr as usize] = value;
            }
            // $A000-$BFFF — ts:610-619: bank_a==CartHi ⇒ cart may consume; the
            // non-cart-hi ultimax open window drops; otherwise RAM.
            0xa000..=0xbfff => {
                if matches!(self.config.bank_a, BankA::CartHi) && self.cart_write(addr, value) {
                    return;
                }
                if !matches!(self.config.bank_a, BankA::CartHi) && self.config.ultimax {
                    return; // open_bus drop
                }
                self.ram[addr as usize] = value;
            }
            // $C000-$CFFF — ts:621-626: ultimax open window drops; else RAM.
            0xc000..=0xcfff => {
                if self.config.ultimax {
                    return; // open_bus drop
                }
                self.ram[addr as usize] = value;
            }
            // $E000-$FFFF — ts:628-635: bank_e==CartHiUltimax ⇒ cart may consume
            // (flash); otherwise RAM.
            0xe000..=0xffff => {
                if matches!(self.config.bank_e, BankE::CartHiUltimax)
                    && self.cart_write(addr, value)
                {
                    return;
                }
                self.ram[addr as usize] = value;
            }
            // $1000-$7FFF — ts:637-640: ultimax open window drops; else RAM.
            // $0000-$0FFF + everything else → RAM.
            _ => {
                if self.config.ultimax && addr >= 0x1000 {
                    return; // open_bus drop
                }
                self.ram[addr as usize] = value;
            }
        }
    }

    /// One VIC master cycle per CPU master cycle (= c64ViciiCycle hook); advance
    /// the shared master clock + both CIAs' clk in lockstep. The VIC reads its
    /// per-cycle fetches through a `VicMemView` over the bus's RAM / CHARGEN /
    /// colour-RAM ($D800 IO shadow) / live VIC bank (built inline so the borrow
    /// checker sees disjoint field borrows vs `&mut self.vic`).
    #[inline]
    fn tick(&mut self) {
        let vbank = self.vic_bank_base();
        let view = crate::vic::VicMemView {
            ram: self.ram,
            char_rom: Some(self.char_rom),
            color_ram: &self.io[0x0800..0x0c00],
            vbank,
        };
        self.vic.tick(&view);
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
        let vbank = self.vic_bank_base();
        let view = crate::vic::VicMemView {
            ram: self.ram,
            char_rom: Some(self.char_rom),
            color_ram: &self.io[0x0800..0x0c00],
            vbank,
        };
        let stolen = self.vic.steal_cycles(&view);
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
