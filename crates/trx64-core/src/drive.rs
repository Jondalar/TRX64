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

use crate::{cpu::{Bus, Cpu6510}, gcr::GcrImage, rotation::Rotation, NullSink, Observer, RomError};

/// Disk image kind — D64 (standard 1541 format) or G64 (GCR nibble dump).
#[derive(Clone, Debug)]
pub enum DiskKind {
    D64,
    G64,
}

/// In-memory disk image attached to a drive. The GCR read path is out of scope
/// (ADR-012 isolation gate); this struct only stores the raw bytes for media
/// mount / persist / SHA256 parity.
#[derive(Clone)]
pub struct DiskImage {
    pub kind: DiskKind,
    pub bytes: Vec<u8>,
    pub backing_path: Option<String>,
    pub read_only: bool,
}

// ── 6522 VIA register indices (via.h:35-55) ─────────────────────────────────
const VIA_PRB: usize = 0;
const VIA_PRA: usize = 1;
const VIA_DDRB: usize = 2;
const VIA_DDRA: usize = 3;
const VIA_T1CL: usize = 4;
const VIA_T1CH: usize = 5;
const VIA_T1LL: usize = 6;
const VIA_T1LH: usize = 7;
const VIA_T2CL: usize = 8;
const VIA_T2LL: usize = 8;
const VIA_T2CH: usize = 9;
const VIA_T2LH: usize = 9;
const VIA_SR: usize = 10;
const VIA_ACR: usize = 11;
const VIA_PCR: usize = 12;
const VIA_IFR: usize = 13;
const VIA_IER: usize = 14;
const VIA_PRA_NHS: usize = 15;

// ── IFR / IER interrupt-mask bits (via.h:59-66) ─────────────────────────────
const VIA_IM_IRQ: u8 = 0x80;
const VIA_IM_T1: u8 = 0x40;
const VIA_IM_T2: u8 = 0x20;

// ── ACR bits (via.h:68-93) ──────────────────────────────────────────────────
const VIA_ACR_T1_FREE_RUN: u8 = 0x40;
const VIA_ACR_T1_PB7_USED: u8 = 0x80;
const VIA_ACR_T2_COUNTPB6: u8 = 0x20;

/// VICE viacore.c:216 `FULL_CYCLE_2` — the 2-cycle reload overhead of T1.
const FULL_CYCLE_2: u64 = 2;

/// Real 6522 VIA timer core (port of vice/src/core/viacore.c, scoped to the
/// 1541 drive's VIA2 needs: T1 free-run/one-shot, T2 timer, IFR/IER, computed
/// timer reads, ACR/PCR storage, and the IRQ line `(ifr & ier & 0x7f) != 0`).
///
/// VICE drives the timers off an alarm context; this port instead advances
/// them lazily/eagerly via [`Via6522::run_alarms`] keyed on the drive clock,
/// which is deterministic and produces the same IFR-raise instants. The CB/CA
/// handshake, shift register and PB7 toggle paths are not needed for the disk
/// controller's idle/IRQ behaviour and are omitted (PB7 toggle bookkeeping is
/// kept minimal). Port reads ($1C00 PRB / $1C01 PRA) are supplied by the
/// caller through [`Via2Ports`] so the GCR/sync/wps bits stay in the drive.
#[derive(Clone)]
pub struct Via6522 {
    /// 16-byte register file (PRA/PRB/DDR*/timer latches/SR/ACR/PCR/IFR/IER).
    regs: [u8; 16],
    /// T1 latch value (`tal` = T1LL | T1LH<<8). viacore.c `ctx.tal`.
    tal: u16,
    /// Absolute clock of the next T1 underflow ("zero") — viacore.c `t1zero`.
    t1zero: u64,
    /// Absolute clock T1 reloads from latch — viacore.c `t1reload`.
    t1reload: u64,
    /// T1 currently counting (a T1CH write armed it). 0 once a one-shot expired.
    t1_active: bool,
    /// PB7 toggle output state bit (0x80/0x00) — viacore.c `t1_pb7`.
    t1_pb7: u8,
    /// T2 low/high counter bytes — viacore.c `t2cl` / `t2ch`.
    t2cl: u8,
    t2ch: u8,
    /// Absolute clock of the next T2 underflow — viacore.c `t2zero`.
    t2zero: u64,
    /// T2 reached the xx00 boundary (the 256-step high decrement window).
    t2xx00: bool,
    /// T2 IRQ permitted (latched by a T2CH write) — viacore.c `t2_irq_allowed`.
    t2_irq_allowed: bool,
    /// IFR / IER (held outside the register file like viacore.c `ifr`/`ier`).
    ifr: u8,
    ier: u8,
    /// Clock the IRQ line `(ifr&ier&0x7f)` last went active (= the rclk VICE
    /// stamps into the CPU via update_myviairq_rclk). `u64::MAX` = inactive.
    pub irq_stamp: u64,
    /// Whether the IRQ line is currently asserted.
    pub irq_active: bool,
}

/// Port-input snapshot the disk controller supplies for a VIA2 PRA/PRB read.
/// VICE's via2d read_pra/read_prb compute these from the rotating GCR stream;
/// until the GCR path lands they are static "no disk / no sync" defaults that
/// reproduce the drive idle/IRQ instruction stream byte-exact.
#[derive(Clone, Copy)]
pub struct Via2Ports {
    /// PRA pin input (GCR read byte). 0xFF with no rotating disk.
    pub pra_pin: u8,
    /// PRB pin input = `sync_found | wps | 0x6f` (via2d.c:486-512). With no
    /// sync and writeable media this is `0 | 0x10 | 0x6f = 0x7f`.
    pub prb_pin: u8,
}

impl Default for Via2Ports {
    fn default() -> Self {
        // No disk in the drive (boot-basic-ready runs with an empty drive). VICE:
        //   rotation_sync_found → 0x80 when read_write_mode==0 (read mode, the
        //     power-on default) regardless of media (rotation.ts:895-901);
        //   drive_writeprotect_sense → 0x10 with no image (drive.ts:1689-1691);
        //   read_pra GCR_read → 0 with no image, but DDRA=0 so PRA reads the pin
        //     which the ROM treats as the floating data bus.
        // PRB pin = sync(0x80) | wps(0x10) | 0x6f = 0xFF.
        Self { pra_pin: 0xff, prb_pin: 0xff }
    }
}

impl Via6522 {
    /// Power-on state (viacore_setup_context viacore.c:1843-1854).
    fn new() -> Self {
        let mut regs = [0u8; 16];
        regs[VIA_T1CL] = 0xff;
        regs[VIA_T1LL] = 0xff;
        regs[VIA_T1CH] = 223;
        regs[VIA_T1LH] = 223;
        regs[VIA_T2CL] = 0xff;
        regs[VIA_T2CH] = 0xff;
        Self {
            regs,
            tal: 0xffff,
            t1zero: 0,
            t1reload: 0,
            t1_active: false,
            t1_pb7: 0x80,
            t2cl: 0xff,
            t2ch: 0xff,
            t2zero: 0,
            t2xx00: false,
            t2_irq_allowed: false,
            ifr: 0,
            ier: 0,
            irq_stamp: u64::MAX,
            irq_active: false,
        }
    }

    /// viacore_reset (viacore.c:378-439): clear port/ddr + control regs, latch
    /// the timers to power-on, clear IFR/IER. Seeds the reload anchors at `clk`.
    fn reset(&mut self, clk: u64) {
        for i in 0..4 {
            self.regs[i] = 0;
        }
        for i in 11..16 {
            self.regs[i] = 0;
        }
        self.tal = 0xffff;
        self.t2cl = 0xff;
        self.t2ch = 0xff;
        self.t1reload = clk;
        self.t2zero = clk;
        self.ier = 0;
        self.ifr = 0;
        self.t1_pb7 = 0x80;
        self.t1_active = false;
        self.t2_irq_allowed = false;
        self.t2xx00 = false;
        self.irq_stamp = u64::MAX;
        self.irq_active = false;
    }

    /// viacore_t1 (viacore.c:265-284): the value the T1 counter would read at
    /// `rclk` given the latch + reload anchor.
    #[inline]
    fn t1_value(&self, rclk: u64) -> u16 {
        if rclk < self.t1reload {
            (self.t1reload.wrapping_sub(rclk).wrapping_sub(FULL_CYCLE_2)) as u16
        } else {
            let full_cycle = self.tal as u64 + FULL_CYCLE_2;
            let past = rclk - self.t1reload;
            let partial = past % full_cycle;
            (self.tal as u64).wrapping_sub(partial) as u16
        }
    }

    /// viacore_t2 (viacore.c:311-331): T2 counter value at `rclk`.
    #[inline]
    fn t2_value(&self, rclk: u64) -> u16 {
        if self.regs[VIA_ACR] & VIA_ACR_T2_COUNTPB6 != 0 {
            ((self.t2ch as u16) << 8) | self.t2cl as u16
        } else {
            let mut t2 = (self.t2zero.wrapping_sub(rclk)) as u16;
            if self.t2xx00 {
                t2 = ((self.t2ch as u16) << 8) | (t2 & 0xff);
            }
            t2
        }
    }

    /// update_via_t1_latch (viacore.c:340-361): roll `t1reload` forward past
    /// `rclk` and refresh `tal` from the latch registers.
    #[inline]
    fn update_t1_latch(&mut self, rclk: u64) {
        if rclk >= self.t1reload {
            let full_cycle = self.tal as u64 + FULL_CYCLE_2;
            let past = rclk - self.t1reload;
            let nuf = 1 + past / full_cycle;
            self.t1reload += nuf * full_cycle;
        }
        self.tal = (self.regs[VIA_T1LL] as u16) | ((self.regs[VIA_T1LH] as u16) << 8);
    }

    /// Recompute the IRQ line `(ifr & ier & 0x7f)` and stamp the active clock.
    /// Mirrors viacore.c update_myviairq_rclk → interrupt_set_irq(rclk): the
    /// `rclk` becomes the CPU's irq_clk on a fresh rising edge.
    #[inline]
    fn update_irq(&mut self, rclk: u64) {
        let active = (self.ifr & self.ier & 0x7f) != 0;
        if active {
            if !self.irq_active {
                self.irq_active = true;
                self.irq_stamp = rclk;
            }
        } else if self.irq_active {
            self.irq_active = false;
            self.irq_stamp = u64::MAX;
        }
    }

    /// viacore_signal(VIA_SIG_CA1, edge) (viacore.c:441-490): the external CA1
    /// input edge. On the 1541 VIA1, CA1 is tied to the IEC ATN line so the C64
    /// driving ATN raises the drive's attention IRQ. Raise IFR CA1 (0x02) only when
    /// the edge polarity matches the PCR CA1-control bit (PCR bit0): VICE
    /// `if ((edge?1:0) == (PCR & VIA_PCR_CA1_CONTROL))`. `edge` is 1 = rising,
    /// 0 = falling. The CA2-toggle and latching sub-paths are not used by VIA1 here.
    #[inline]
    fn signal_ca1(&mut self, edge: u8, rclk: u64) {
        let pcr_ctrl = self.regs[VIA_PCR] & 0x01;
        if (if edge != 0 { 1 } else { 0 }) == pcr_ctrl {
            self.ifr |= 0x02; // VIA_IM_CA1
            self.update_irq(rclk);
        }
    }

    /// Run any timer underflows that are due at or before `clk` (= VICE alarm
    /// dispatch for the T1-zero / T2-zero alarms). Raises IFR bits and stamps
    /// the IRQ line at the precise underflow rclk (`zero + 1`). Must be invoked
    /// before each VIA register access and before each drive-CPU IRQ sample so
    /// the IFR/IRQ state is current as of `clk`.
    ///
    /// KNOWN RESIDUAL DIVERGENCE (drive-boot-deep, ~drive cycle 1.05M): this
    /// model is byte-exact for the first TWO watchdog T1 IRQs but the THIRD T1
    /// underflow lands 2 drive cycles early (candidate t1zero 1048795 vs golden
    /// 1048797 — surfacing as `$FE68` IRQ entry @1048808 vs @1048810). The
    /// $F99F watchdog `STA $1C05` re-arm stores at byte-identical instruction
    /// boundaries to the golden (verified: $F99F→$F9A2 cycles match), and the
    /// re-arm `t1zero = store_clk + 1 + tal` is correct for the boot arm and the
    /// first re-arm, but the golden's third underflow is +2 — i.e. golden's
    /// inter-re-arm t1zero spacing is 15000 while the matching instruction
    /// period is 14998. The +2 is exactly FULL_CYCLE_2 and almost certainly
    /// reflects a VICE free-run reload phase the eager `t1zero += full_cycle`
    /// below does not reproduce when a free-run underflow falls between two
    /// watchdog re-arms. Resolving it needs a VICE cross-check of the exact
    /// t1reload anchor at the re-arm; the spec port alone is insufficient.
    fn run_alarms(&mut self, clk: u64) {
        // T1 zero (viacore_t1_zero_alarm viacore.c:1306-1342).
        while self.t1_active && clk >= self.t1zero {
            let rclk = self.t1zero; // alarm fires at the scheduled zero clock
            if self.regs[VIA_ACR] & VIA_ACR_T1_FREE_RUN == 0 {
                // one-shot: stop after this underflow
                self.t1_active = false;
            } else {
                let full_cycle = self.tal as u64 + FULL_CYCLE_2;
                self.t1zero += full_cycle;
            }
            self.t1_pb7 ^= 0x80;
            self.ifr |= VIA_IM_T1;
            self.update_irq(rclk + 1);
        }
        // T2 zero (viacore_t2_zero_alarm viacore.c:1554-1586). Only the 16-bit
        // timer mode (ACR T2_COUNTPB6 clear) is modelled — the drive uses T2 as
        // a plain down-counter; the SR-controlled/free-running shift paths are
        // out of scope for the disk-controller idle/IRQ behaviour.
        if self.regs[VIA_ACR] & VIA_ACR_T2_COUNTPB6 == 0 {
            while self.t2xx00 && clk >= self.t2zero {
                let rclk = self.t2zero;
                // low underflow decrements high; IRQ on the high wrap if allowed.
                self.t2ch = self.t2ch.wrapping_sub(1);
                if self.t2ch == 0xff && self.t2_irq_allowed {
                    self.ifr |= VIA_IM_T2;
                    self.update_irq(rclk);
                    self.t2_irq_allowed = false;
                }
                // schedule the next xx00 boundary 256 cycles on (16-bit mode).
                if self.t2ch != 0xff {
                    self.t2zero += 256;
                } else {
                    self.t2xx00 = false;
                }
            }
        }
    }

    /// viacore_read (viacore.c:1032-1214) scoped to VIA2. `clk` is the access
    /// rclk; `ports` supplies the PRA/PRB pin inputs. Caller MUST `run_alarms`
    /// up to `clk` first.
    #[inline]
    fn read(&mut self, addr: u16, clk: u64, ports: Via2Ports) -> u8 {
        let a = (addr & 0x0f) as usize;
        match a {
            VIA_PRA | VIA_PRA_NHS => {
                self.ifr &= !0x02; // clear CA1
                if self.regs[VIA_PCR] & 0x0a != 0x02 {
                    self.ifr &= !0x01; // clear CA2 unless independent-input
                }
                self.update_irq(clk);
                let ddra = self.regs[VIA_DDRA];
                ((ports.pra_pin & !ddra) | (self.regs[VIA_PRA] & ddra)) & 0xff
            }
            VIA_PRB => {
                self.ifr &= !0x10; // clear CB1
                if self.regs[VIA_PCR] & 0xa0 != 0x20 {
                    self.ifr &= !0x08; // clear CB2
                }
                self.update_irq(clk);
                let ddrb = self.regs[VIA_DDRB];
                let mut byte = ((ports.prb_pin & !ddrb) | (self.regs[VIA_PRB] & ddrb)) & 0xff;
                if self.regs[VIA_ACR] & VIA_ACR_T1_PB7_USED != 0 {
                    byte = (byte & 0x7f) | self.t1_pb7;
                }
                byte
            }
            VIA_T1CL => {
                self.ifr &= !VIA_IM_T1;
                self.update_irq(clk);
                (self.t1_value(clk) & 0xff) as u8
            }
            VIA_T1CH => ((self.t1_value(clk) >> 8) & 0xff) as u8,
            VIA_T2CL => {
                self.ifr &= !VIA_IM_T2;
                self.update_irq(clk);
                (self.t2_value(clk) & 0xff) as u8
            }
            VIA_T2CH => ((self.t2_value(clk) >> 8) & 0xff) as u8,
            VIA_IFR => {
                let mut t = self.ifr;
                if self.ifr & self.ier != 0 {
                    t |= 0x80;
                } else {
                    t &= !0x80;
                }
                t
            }
            VIA_IER => self.ier | 0x80,
            // PRB/T1/T2/IFR/IER handled; everything else returns the raw reg
            // (SR/ACR/PCR/DDR*/T1L*). PCR ($1C0C) reads its stored value — this
            // is the byte the boot init expects (0x00 after reset).
            _ => self.regs[a],
        }
    }

    /// viacore_store (viacore.c:637-1024) scoped to VIA2. Caller MUST
    /// `run_alarms` up to `clk` first.
    #[inline]
    fn write(&mut self, addr: u16, val: u8, clk: u64) {
        let a = (addr & 0x0f) as usize;
        let v = val;
        match a {
            VIA_PRA => {
                self.ifr &= !0x02;
                if self.regs[VIA_PCR] & 0x0a != 0x02 {
                    self.ifr &= !0x01;
                }
                self.update_irq(clk);
                self.regs[VIA_PRA_NHS] = v;
                self.regs[VIA_PRA] = v;
            }
            VIA_PRA_NHS => {
                self.regs[VIA_PRA_NHS] = v;
                self.regs[VIA_PRA] = v;
            }
            VIA_DDRA => self.regs[VIA_DDRA] = v,
            VIA_PRB => {
                self.ifr &= !0x10;
                if self.regs[VIA_PCR] & 0xa0 != 0x20 {
                    self.ifr &= !0x08;
                }
                self.update_irq(clk);
                self.regs[VIA_PRB] = v;
            }
            VIA_DDRB => self.regs[VIA_DDRB] = v,
            VIA_SR => self.regs[VIA_SR] = v,
            VIA_T1CL | VIA_T1LL => {
                self.regs[VIA_T1LL] = v;
                self.update_t1_latch(clk);
            }
            VIA_T1CH => {
                self.regs[VIA_T1LH] = v;
                self.update_t1_latch(clk);
                // Start T1: reload anchors at clk (viacore.c:650-655).
                self.t1reload = clk + 1 + self.tal as u64 + FULL_CYCLE_2;
                self.t1zero = clk + 1 + self.tal as u64;
                self.t1_active = true;
                self.t1_pb7 = 0;
                self.ifr &= !VIA_IM_T1;
                self.update_irq(clk);
            }
            VIA_T1LH => {
                self.regs[VIA_T1LH] = v;
                self.update_t1_latch(clk);
                self.ifr &= !VIA_IM_T1;
                self.update_irq(clk);
            }
            VIA_T2LL => self.regs[VIA_T2LL] = v,
            VIA_T2CH => {
                self.regs[VIA_T2LH] = v;
                self.t2cl = self.regs[VIA_T2LL];
                self.t2ch = v;
                if self.regs[VIA_ACR] & VIA_ACR_T2_COUNTPB6 == 0 {
                    // schedule_t2_zero_alarm (viacore.c:557-566) at clk+1.
                    self.t2zero = (clk + 1) + self.t2cl as u64;
                    self.t2xx00 = true;
                }
                self.ifr &= !VIA_IM_T2;
                self.update_irq(clk);
                self.t2_irq_allowed = true;
            }
            VIA_IFR => {
                self.ifr &= !v;
                self.update_irq(clk);
            }
            VIA_IER => {
                if v & VIA_IM_IRQ != 0 {
                    self.ier |= v & 0x7f;
                } else {
                    self.ier &= !v;
                }
                self.update_irq(clk);
            }
            VIA_ACR => {
                // PB7-toggle enable rising edge re-arms the PB7 latch (viacore.c:857).
                let old = self.regs[VIA_ACR];
                if (old ^ v) & VIA_ACR_T1_PB7_USED != 0 && v & VIA_ACR_T1_PB7_USED != 0 {
                    self.t1_pb7 = 0x80;
                }
                // T2 mode change (viacore.c:889-925) — the drive keeps T2 in
                // timer mode (COUNTPB6 clear), so only the PB6-count transition
                // would need the freeze path; not exercised by the disk
                // controller, so the stored value is sufficient here.
                self.regs[VIA_ACR] = v;
            }
            VIA_PCR => self.regs[VIA_PCR] = v,
            _ => self.regs[a] = v,
        }
    }
}

/// Drive 6502 bus (implements cpu::Bus). Borrows from Drive1541 fields.
struct DriveBus<'a> {
    ram: &'a mut [u8; 0x800],
    rom: &'a [u8; 0x8000],
    via1: &'a mut Via6522,
    via2: &'a mut Via6522,
    /// Live IEC bus state at the VIA1 PB inputs (= iecbus.drv_port). Read by a
    /// `$1800` PB access so the drive's idle loop sees the C64-driven CLK/DATA/ATN.
    drv_port: u8,
    /// Drive-CPU clock at the current bus access (= rclk for the VIA2 timer).
    /// Kept in step with `cpu.clk` by the run loop before each cycle.
    clk: u64,
    /// Disk-controller port inputs supplied to VIA2 PRA/PRB reads (fallback when
    /// no disk is mounted — the static "no rotating disk" defaults).
    via2_ports: Via2Ports,
    /// The rotating GCR disk model. `image == None` ⇒ no disk; VIA2 falls back to
    /// the static `via2_ports`. When a D64 is mounted this drives PRA (GCR_read),
    /// PRB bit7 (SYNC), the stepper/motor/speed-zone from store_prb, and the
    /// byte-ready (SO) handshake consumed by the drive CPU's V flag.
    rotation: &'a mut Rotation,
    /// Pending `drive_cpu_set_overflow` request raised by a VIA2 store side-effect
    /// (set_ca2 on the PCR CA2 edge / store_prb on the motor edge — via2d.c
    /// set_ca2 → drive_cpu_set_overflow, store_prb motor branch). VICE delivers the
    /// byte-ready→V flush straight into the drive CPU's P register from the store;
    /// the bus borrow can't touch `cpu`, so we latch it here and `step_instruction`
    /// folds it into `reg_p` after the store cycle completes. `true` ⇒ set V.
    pending_set_overflow: bool,
}

impl<'a> DriveBus<'a> {
    /// VIA1 PB pin input = via1d1541.c read_prb IEC `tmp`:
    ///   tmp = (drv_port ^ 0x85) | 0x1a | driveid   (unit 8 → driveid 0)
    /// Fed to the generic 6522 PRB read as `prb_pin`, which then applies
    ///   byte = (tmp & ~DDRB) | (PRB & DDRB)
    /// — identical to VICE. Output bits (DDRB=1) read the ORB latch; input bits
    /// (DDRB=0) read the IEC bus.
    #[inline]
    fn via1_iec_tmp(&self) -> u8 {
        ((self.drv_port ^ 0x85) | 0x1a) & 0xff // driveid 0 for unit 8
    }
    /// Port inputs presented to the IEC VIA1: PB carries the IEC bus `tmp`,
    /// PA floats high (the drive does not read a meaningful PA on VIA1).
    #[inline]
    fn via1_ports(&self) -> Via2Ports {
        Via2Ports { pra_pin: 0xff, prb_pin: self.via1_iec_tmp() }
    }

    /// The drive 6502's IRQ pin is the wired-OR of the VIA1 and VIA2 IRQ lines
    /// (both 6522 IRQ outputs share the single CPU IRQ pin). VICE routes both VIA
    /// `set_int` calls into the same `int_status`, so the CPU samples their OR.
    /// Returns `(active, stamp)`: active iff either VIA asserts; the stamp is the
    /// earliest active edge (the instant the combined line first rose).
    #[inline]
    fn combined_irq(&self) -> (bool, u64) {
        let active = self.via1.irq_active || self.via2.irq_active;
        if !active {
            return (false, u64::MAX);
        }
        let s1 = if self.via1.irq_active { self.via1.irq_stamp } else { u64::MAX };
        let s2 = if self.via2.irq_active { self.via2.irq_stamp } else { u64::MAX };
        (true, s1.min(s2))
    }

    /// VIA2 port inputs for a PRA/PRB read. When a disk is mounted the rotating
    /// model is advanced to `clk` first (via2d read_pra → rotation_byte_read /
    /// read_prb → rotation_rotate_disk) and supplies PRA = GCR_read, PRB =
    /// (sync | wps | 0x6f). With no disk, the static defaults.
    #[inline]
    fn via2_ports_live(&mut self, for_pra: bool) -> Via2Ports {
        if self.rotation.image.is_some() {
            if for_pra {
                self.rotation.byte_read(self.clk);
            } else {
                self.rotation.rotate_disk(self.clk);
            }
            Via2Ports { pra_pin: self.rotation.pra_pin(), prb_pin: self.rotation.prb_pin() }
        } else {
            self.via2_ports
        }
    }

    /// via2d store_prb side-effects (via2d.c:382-487): LED (PB3), stepper (PB0-1
    /// gated by motor PB2), motor-on (PB2 → byte_ready_active), speed-zone
    /// (PB5-6). Applied AFTER the generic 6522 latches the ORB, using the new
    /// composed output byte. `old_pb` is the prior PB output (for edge detects).
    fn via2_store_prb_effects(&mut self, old_pb: u8, new_pb: u8) {
        if self.rotation.image.is_none() {
            return;
        }
        let r = &mut self.rotation;
        // Speed zone (density) PB5-6 — only on change.
        if (old_pb ^ new_pb) & 0x60 != 0 {
            r.speed_zone_set(((new_pb >> 5) & 0x3) as usize);
        }
        // Stepper PB0-1, gated by motor PB2 (via2d.c:422-443).
        if new_pb & 0x04 != 0 {
            let track_number = r.current_half_track.wrapping_sub(2);
            let new_stepper = (new_pb & 3) as i32;
            let old_stepper = (track_number & 3) as i32;
            let mut step_count = (new_stepper - old_stepper) & 3;
            if step_count == 3 {
                step_count = -1;
            }
            if step_count == 1 || step_count == -1 {
                r.move_head(step_count);
            }
        }
        // Motor on/off edge (via2d.c:325-352): mirror PB2 into byte_ready_active.
        // On motor-off, flush a pending byte-ready edge into V (drive_cpu_set_overflow,
        // via2d.c:343-348); on motor-on, re-anchor the rotation clock.
        let was_motor = r.byte_ready_active & crate::rotation::BRA_MOTOR_ON;
        let now_motor = new_pb & crate::rotation::BRA_MOTOR_ON;
        if was_motor != now_motor {
            r.byte_ready_active =
                (r.byte_ready_active & !crate::rotation::BRA_MOTOR_ON) | now_motor;
            if now_motor != 0 {
                r.begins(self.clk);
            } else if r.byte_ready_edge != 0 {
                self.pending_set_overflow = true;
                r.byte_ready_edge = 0;
            }
        }
        // VICE via2d.c:354 — byte_ready_level cleared last on a PB store.
        self.rotation.byte_ready_level = 0;
    }

    /// Composed VIA2 PB output (`ORB | ~DDRB`) — the value the motor/stepper/LED/
    /// speed-zone pins see (output bits = ORB, input bits float high).
    #[inline]
    fn via2_pb_output(&self) -> u8 {
        let orb = self.via2.regs[0];
        let ddrb = self.via2.regs[2];
        (orb | !ddrb) & 0xff
    }

    /// VIA2 $1C0C (PCR) store side-effects, in VICE dispatch order. viacore_store
    /// (viacore.c:786) on a PCR write first recomputes `ca2_out_state` from the new
    /// PCR CA2 mode and calls `set_ca2` (via2d.c:72-93), THEN runs `store_pcr` →
    /// `via2d_update_pcr` (via2d.c:165-178). Both touch the byte-ready-enable bit of
    /// `byte_ready_active`; the set_ca2 call additionally flushes a pending
    /// byte-ready edge into the drive CPU's overflow flag on the CA2 low→high edge —
    /// the $F556 read-loop handshake. `for_pcr` carries the raw stored PCR byte.
    fn via2_store_pcr_effects(&mut self, pcrval: u8) {
        if self.rotation.image.is_none() {
            return;
        }
        // ── set_ca2 (via2d.c:72-93), dispatched by viacore from the PCR store ──
        // VICE viacore.c:786 derives ca2_out_state from the new PCR CA2 mode:
        //   (pcr & 0x0e) == 0x0c (LOW_OUTPUT)  → 0
        //   (pcr & 0x0e) == 0x0e (HIGH_OUTPUT) → 1
        //   else (input / handshake / pulse)   → 1
        let ca2_low = (pcrval & 0x0e) == 0x0c;
        let new_ca2: u8 = if ca2_low { 0 } else { 1 };
        let curr = (self.rotation.byte_ready_active >> 1) & 1;
        if new_ca2 != curr {
            // set_ca2: rotate, latch the new byte-ready-active bit, and on the
            // low→high re-enable flush any pending byte-ready edge into V.
            self.rotation.rotate_disk(self.clk);
            self.rotation.byte_ready_active =
                (self.rotation.byte_ready_active & !crate::rotation::BRA_BYTE_READY)
                    | (new_ca2 << 1);
            if self.rotation.byte_ready_edge != 0 {
                // drive_cpu_set_overflow(dc): set the drive 6502 V flag. The bus
                // borrow can't reach `cpu`; latch the request for step_instruction.
                self.pending_set_overflow = true;
                self.rotation.byte_ready_edge = 0;
            }
        }
        // ── via2d_update_pcr (via2d.c:165-178), via store_pcr after set_ca2 ──
        let r = &mut self.rotation;
        r.rotate_disk(self.clk);
        r.read_write_mode = pcrval & 0x20 != 0;
        // PCR bit1 → BRA_BYTE_READY in byte_ready_active (matches the set_ca2 latch
        // above for the LOW/HIGH output modes the DOS uses).
        let pcr_br = (pcrval & crate::rotation::BRA_BYTE_READY) != 0;
        if pcr_br {
            r.byte_ready_active |= crate::rotation::BRA_BYTE_READY;
        } else {
            r.byte_ready_active &= !crate::rotation::BRA_BYTE_READY;
        }
    }
}

impl<'a> Bus for DriveBus<'a> {
    /// One drive master-cycle: advance the bus clock in lockstep with the CPU
    /// (= FullBus::tick, full.rs:327) and run the VIA2 timer alarms so an IFR
    /// underflow latches at the exact cycle it occurs. The drive CPU samples the
    /// resulting IRQ line at its next instruction boundary.
    #[inline]
    fn tick(&mut self) {
        self.clk = self.clk.wrapping_add(1);
        self.via1.run_alarms(self.clk);
        self.via2.run_alarms(self.clk);
        // NOTE: the rotating disk is advanced lazily — at VIA2 port accesses
        // (read_pra/read_prb → rotation_byte_read/rotate_disk) and at the
        // BVS/BVC/PHP/CLV opcodes (the byte-ready/SO handshake) — exactly where
        // VICE's drive 6510 core consults it, NOT per cycle. Advancing per cycle
        // would over-run the head between the BVC edge and the LDA $1C01 that
        // reads the latched byte.
    }

    #[inline]
    fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7FFF => {
                // VIA1: $1800-$1BFF (mirror every $400) — real 6522. PB carries
                // the IEC bus lines via the read_prb `tmp` (supplied as prb_pin);
                // the IFR/IER reads follow 6522 semantics so the drive IRQ handler
                // ($FE6C LDA $180D) sees the real CA1/timer flags, not a stale
                // last-written byte.
                if (0x1800..=0x1BFF).contains(&addr) {
                    let ports = self.via1_ports();
                    self.via1.run_alarms(self.clk);
                    return self.via1.read(addr, self.clk, ports);
                }
                // VIA2: $1C00-$1FFF — real 6522 timer/IFR/IER/PCR model. PRA
                // ($1C01) / PRB ($1C00) port reads sample the rotating disk: PRA
                // = GCR_read, PRB bit7 = SYNC. A PRA read advances the model and
                // clears byte_ready_level (via2d read_pra/read_prb).
                if (0x1C00..=0x1FFF).contains(&addr) {
                    self.via2.run_alarms(self.clk);
                    let reg = addr & 0x0f;
                    let ports = if reg == 1 || reg == 15 {
                        let p = self.via2_ports_live(true);
                        self.rotation.byte_ready_level = 0;
                        p
                    } else if reg == 0 {
                        let p = self.via2_ports_live(false);
                        self.rotation.byte_ready_level = 0;
                        p
                    } else {
                        self.via2_ports
                    };
                    return self.via2.read(addr, self.clk, ports);
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
                    self.via1.run_alarms(self.clk);
                    self.via1.write(addr, val, self.clk);
                    return;
                }
                if (0x1C00..=0x1FFF).contains(&addr) {
                    self.via2.run_alarms(self.clk);
                    let reg = addr & 0x0f;
                    // Prior PB output (ORB|~DDRB) for the stepper/motor edge detect.
                    let old_pb = self.via2_pb_output();
                    self.via2.write(addr, val, self.clk);
                    // store_prb side-effects: stepper / motor / LED / speed-zone.
                    if reg == 0 {
                        let new_pb = self.via2_pb_output();
                        self.via2_store_prb_effects(old_pb, new_pb);
                    } else if reg == 0x0c {
                        // store_pcr → via2d_update_pcr (read/write mode + byte-ready).
                        self.via2_store_pcr_effects(val);
                    }
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
    /// VICE drive-sync fixed-point accumulator (drivecpu.c:383-390 `cycle_accum`).
    /// Low 16 fractional bits of accumulated `sync_factor * c64_cycles`; the carry
    /// out of bit 16 is the integer number of drive cycles to advance `stop_clk`.
    sync_accum: u32,
    /// Absolute drive clock the CPU may run up to (VICE `cpu->stop_clk`). The drive
    /// 6502 executes whole instructions while `cpu.clk < stop_clk`.
    stop_clk: u64,
    /// Effective IEC bus state the drive reads at its VIA1 PB inputs (= VICE
    /// iecbus.drv_port: bit0=DATA_IN, bit2=CLK_IN, bit7=ATN). Refreshed by the
    /// FullBus push-flush before the drive runs, so a `read $1800` reflects the
    /// live C64-driven IEC lines. Power-on 0x85 (all released).
    pub iec_drv_port: u8,
    /// Pending 6502 hardware-reset sequence. VICE fires `cpu_reset` (drivecpu.c:165)
    /// from the 6510 core's IK_RESET dispatch on the FIRST execute round, which sets
    /// `clk_ptr = 6` (the ~6-cycle reset sequence the chip consumes before the first
    /// opcode fetch). We model that lazily, on the first cycle the drive runs, so the
    /// shared `Cpu6510::reset_to()` stays untouched (C64 CPU/VIC/CIA gates unaffected).
    reset_pending: bool,
    /// Attached disk image (None = no disk in drive).
    pub disk: Option<DiskImage>,
    /// The rotating GCR disk model (head position, bit-stream, byte-ready). Holds
    /// the per-track GCR bitstream for a mounted D64 (`rotation.image`).
    pub rotation: Rotation,
}

/// PAL drive sync factor (VICE drivesync.c:53-62 `drive_set_machine_parameter`):
///   sync_factor = floor(65536 * 1_000_000 / cycles_per_sec)
/// with the C64 PAL clock cycles_per_sec = 985_248 (vice1541-facade.ts:319). The
/// 1541's `clock_frequency` is 1, so `drv.cpud.sync_factor` = sync_factor * 1.
/// floor(65536 * 1e6 / 985248) = 66517.
const DRIVE_SYNC_FACTOR_PAL: u32 = 66517;

/// 6502 hardware-reset sequence cost the drive consumes before the first opcode
/// fetch (VICE drivecpu.c:165-184 `cpu_reset` → `drv->clk_ptr = 6`).
const DRIVE_RESET_CYCLES: u64 = 6;

/// C64 main-CPU reset-sequence cycles the drive's catch-up clock observes BEFORE the
/// first traced C64 instruction.
///
/// In the TS oracle the drive catches up to `c64Cpu.cycles`, whose origin includes
/// the cycles the C64's own power-on reset consumed reading the $FFFC/$FFFD vector
/// (cpu65xx-vice.ts:531-538). TRX64's shared `Cpu6510::reset_to()` injects PC
/// directly and starts `clk` at 0, so its main-clock origin sits one cycle earlier
/// than TS's. The drive's catch-up targets are therefore uniformly 1 lower than the
/// golden's. We must NOT shift `reset_to()` (it would move the byte-exact C64
/// CPU/VIC/CIA gate cycle stamps), so the drive instead seeds its sync accumulator
/// with this offset at cold reset — a drive-boot-local correction.
const C64_RESET_DRIVE_OFFSET: u64 = 1;

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
            sync_accum: 0,
            stop_clk: 0,
            reset_pending: true,
            iec_drv_port: 0x85,
            disk: None,
            rotation: Rotation::new(),
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
        // The drive 6502 powers on with SP=0 (drivecpu.ts:459 — cpu_regs init
        // `{ pc:0, ac:0, xr:0, yr:0, sp:0, flags:0 }`), and the VICE drive reset
        // dispatch (drive_6510core.ts:2105-2113) does NOT push (unlike an IRQ), so SP
        // stays 0 through boot until the ROM's own TXS. The shared `reset_to()` seeds
        // SP=$FF for the C64; override it here so the drive matches VICE byte-exact.
        self.cpu.reg_sp = 0;
        // VICE drivecpu_reset (drivecpu.c:193-211): clk = 0, stop_clk = 0,
        // last_clk = maincpu_clk (= 0 at cold boot). The +6 reset-sequence cost is
        // applied lazily by `step_instruction` on the first run cycle, matching the
        // IK_RESET dispatch order in VICE's 6510 core (drivecpu.c:165 cpu_reset).
        self.cpu.clk = 0;
        self.drive_clk = 0;
        self.stop_clk = 0;
        self.sync_accum = 0;
        self.reset_pending = true;
        self.last_sample_pc = None;
        self.iec_drv_port = 0x85;
        // VICE viacore_reset (viacore.c:378-439) for both VIAs: clear port/ddr
        // and control regs, latch timers to power-on, clear IFR/IER. VIA1's PB/
        // DDRB start at 0 (all inputs, ORB latch 0) so the IEC read_prb formula
        // sees the right DDRB before the ROM programs $1802; VIA2's PCR → 0 so the
        // boot $1C0C read returns 0x00 as VICE does. Anchored at reset clock (0).
        self.via1.reset(0);
        self.via2.reset(0);
        // Seed the sync accumulator with the C64 power-on reset cycles the drive's
        // catch-up clock observes in TS (see C64_RESET_DRIVE_OFFSET). This shifts the
        // whole drive_clk schedule into phase with the golden without touching the
        // shared C64 reset path.
        self.advance_stop_clk(C64_RESET_DRIVE_OFFSET);
        // A real 1541 loses its disk on power cycle. Don't preserve disk across reset.
        self.disk = None;
        self.rotation = Rotation::new();
    }

    /// Advance the drive's `stop_clk` target by `c64_cycles` of main-CPU time,
    /// applying the VICE PAL sync factor (drivecpu.c:383-390). The integer carry out
    /// of the 16-bit fixed-point accumulator is the number of drive cycles to add.
    #[inline]
    fn advance_stop_clk(&mut self, c64_cycles: u64) {
        // VICE processes in 10000-cycle chunks to bound `sync_factor * tcycles`
        // inside 32 bits; mirror that so the carry math is bit-identical.
        let mut remaining = c64_cycles;
        while remaining != 0 {
            let tcycles = remaining.min(10000) as u32;
            remaining -= tcycles as u64;
            self.sync_accum = self
                .sync_accum
                .wrapping_add(DRIVE_SYNC_FACTOR_PAL.wrapping_mul(tcycles));
            self.stop_clk = self.stop_clk.wrapping_add((self.sync_accum >> 16) as u64);
            self.sync_accum &= 0xFFFF;
        }
    }

    /// Execute one whole drive 6502 instruction over an already-borrowed bus,
    /// folding in the pending hardware reset cost on the very first call.
    ///
    /// VICE's drive 6510 core runs ONE opcode per `drive_6510core_execute` call, and
    /// the IK_RESET dispatch (drivecpu.c:165 `cpu_reset` → clk=6, JUMP $FFFC) happens
    /// in the SAME call body as the opcode fetch+execute (drive_6510core.ts:1672-1733).
    /// So the reset sequence and the first instruction (SEI) are atomic: the drive
    /// goes 0 → reset(clk=6) → SEI(clk=8, PC=$EAA1) without ever stopping at $EAA0.
    /// Modelling them as one step is what makes the first sampled record $EAA1@8
    /// instead of a spurious $EAA0@6.
    ///
    /// Free function (not `&mut self`) so the caller can keep `DriveBus` borrowed
    /// from `self.ram/rom/via*` while we mutate `self.cpu` — the two are disjoint.
    #[inline]
    fn step_instruction<O: Observer>(
        cpu: &mut Cpu6510,
        reset_pending: &mut bool,
        bus: &mut DriveBus,
        obs: &mut O,
    ) {
        if *reset_pending {
            *reset_pending = false;
            // cpu_reset: the 6502 reset sequence consumes 6 cycles before the first
            // opcode of the same execute call runs.
            cpu.clk = DRIVE_RESET_CYCLES;
        }
        // Keep the bus clock aligned with the CPU before the instruction begins
        // (the reset fold-in above may have jumped cpu.clk). Run any VIA2 alarms
        // already due as of this clock, then present the IRQ line to the CPU so
        // it is sampled at the opcode-fetch boundary (= VICE PROCESS_ALARMS then
        // interrupt_check_irq_delay at the fetch — drive_6510core.ts:1660/1682).
        bus.clk = cpu.clk;
        bus.via1.run_alarms(bus.clk);
        bus.via2.run_alarms(bus.clk);
        let (irq, stamp) = bus.combined_irq();
        cpu.set_irq_line_at(irq, stamp);
        // Byte-ready (SO) → drive 6502 V flag. VICE's drive 6510 core consults the
        // GCR byte-ready edge at the BVS/BVC/PHP opcodes (drive_6510core.ts:1753 /
        // 1839 / case 0x08): `rotate; if (byte_ready_edge) { clear edge;
        // SET_OVERFLOW(1) }` — this is the $F556 `BVC *` read-loop handshake. We
        // reproduce it at the opcode-fetch boundary for exactly those opcodes,
        // keeping the shared cpu.rs opcode table untouched (C64 gates unaffected).
        if bus.rotation.image.is_some() {
            // Peek the opcode without side effects (PC is always in ROM/RAM during
            // drive execution, never IO).
            let pc = cpu.reg_pc;
            let op = if pc >= 0x8000 {
                bus.rom[(pc & 0x7FFF) as usize]
            } else {
                bus.ram[(pc & 0x07FF) as usize]
            };
            match op {
                // BVC / BVS / PHP: rotate + byte-ready edge → V flag.
                0x50 | 0x70 | 0x08 => {
                    bus.rotation.rotate_disk(bus.clk);
                    if bus.rotation.byte_ready_edge != 0 {
                        bus.rotation.byte_ready_edge = 0;
                        cpu.reg_p |= 0x40; // P_OVERFLOW
                    }
                }
                // CLV: LOCAL_SET_OVERFLOW(0) → rotate + clear the byte-ready edge
                // (drive_6510core.ts:486-493) so the next byte re-arms cleanly.
                0xB8 => {
                    bus.rotation.rotate_disk(bus.clk);
                    bus.rotation.byte_ready_edge = 0;
                }
                _ => {}
            }
        }
        loop {
            cpu.execute_cycle(bus, obs);
            // drive_cpu_set_overflow flush: a VIA2 store side-effect (set_ca2 on the
            // PCR CA2 low→high edge / store_prb motor-off) latched a byte-ready→V
            // request this cycle. VICE pushes it straight into the drive CPU's P
            // register from the store; fold it in here at the same cycle boundary.
            if bus.pending_set_overflow {
                bus.pending_set_overflow = false;
                cpu.reg_p |= 0x40; // P_OVERFLOW
            }
            if cpu.is_at_boundary() {
                // VICE folds the IRQ/NMI entry into the same execute call as the
                // first handler opcode (drive_6510core.ts:1682 DO_INTERRUPT then
                // fetch). So if this boundary is a freshly-dispatched interrupt,
                // do NOT stop — refresh the line and run the first handler
                // instruction in the same step, leaving PC past the bare vector.
                if cpu.interrupt_just_dispatched() {
                    let (irq, stamp) = bus.combined_irq();
                    cpu.set_irq_line_at(irq, stamp);
                    continue;
                }
                break;
            }
            // Mid-instruction: a VIA1/VIA2 register access or the per-cycle tick may
            // have raised/cleared the IFR. Refresh the IRQ line so a multi-cycle
            // opcode still has the right line state for the next boundary.
            let (irq, stamp) = bus.combined_irq();
            cpu.set_irq_line_at(irq, stamp);
        }
    }

    /// Composed VIA1 PB output byte driving the IEC bus (= viacore VIA_PRB store
    /// `out = ORB | ~DDRB`). Output bits (DDRB=1) carry the ORB latch; input bits
    /// (DDRB=0) float HIGH. The IEC core inverts this to `drv_data[8]`. PB1=DATA_OUT,
    /// PB3=CLK_OUT, PB4=ATN_ACK (active-low after the 7406 / wired-AND inversion).
    #[inline]
    pub fn via1_pb_iec_output(&self) -> u8 {
        let orb = self.via1.regs[0];
        let ddrb = self.via1.regs[2];
        (orb | !ddrb) & 0xff
    }

    /// Deliver an IEC ATN-line edge to the drive's VIA1 CA1 input (= VICE
    /// iecbus.c:440-446: `viacore_signal(via1d1541, VIA_SIG_CA1,
    /// iec_old_atn ? 0 : VIA_SIG_RISE)`, where `iec_old_atn = cpu_bus & 0x10` is the
    /// NEW ATN line state). The C64 asserting ATN drives the drive's attention IRQ
    /// (DOS $FE67 → $E85B handler) via VIA1 CA1. `atn_high` is the new ATN line level
    /// (the `Some(..)` returned by `IecCore::c64_store_dd00`): VICE signals a CA1
    /// RISE when ATN is now LOW (`atn_high == false`), a FALL when ATN is now HIGH.
    /// `clk` is the drive clock the edge is stamped at (the push-flush target).
    #[inline]
    pub fn atn_edge_to_via1_ca1(&mut self, atn_high: bool, clk: u64) {
        let edge: u8 = if atn_high { 0 } else { 1 }; // FALL when high, RISE when low
        self.via1.run_alarms(clk);
        self.via1.signal_ca1(edge, clk);
    }

    /// Reset PC from the ROM vector (re-read). Returns the resolved PC.
    pub fn reset_pc(&self) -> u16 {
        let lo = self.rom[0x7FFC] as u16;
        let hi = self.rom[0x7FFD] as u16;
        lo | (hi << 8)
    }

    /// Advance the drive by `n` cycles of C64 main-CPU time (VICE
    /// `drivecpu_execute` shape, drivecpu.c:353-445).
    ///
    /// The drive 1541 runs at ~1 MHz while the C64 PAL clock is 985_248 Hz, so VICE
    /// scales main-CPU cycles into drive cycles through the fixed-point `sync_factor`
    /// accumulator (`advance_stop_clk`) rather than 1:1. The drive 6502 then executes
    /// whole instructions while `cpu.clk < stop_clk`. The first run also consumes the
    /// 6-cycle reset sequence (folded into `step_instruction`).
    ///
    /// Uses `NullSink` — the sampling approach (not per-instruction firehose) is
    /// handled externally by `sample_pc_change`.
    pub fn run_cycles(&mut self, n: u64) {
        // Advance the drive-clock target for this slice of main-CPU time.
        self.advance_stop_clk(n);
        let mut obs = NullSink;
        // Disk-controller port inputs for VIA2 PRA/PRB when NO disk is mounted —
        // the static "no rotating disk" defaults (sync=0, writeable, GCR floats
        // high) which reproduce the idle/IRQ stream. With a disk the rotating
        // model supplies these instead.
        let via2_ports = Via2Ports::default();
        let mut bus = DriveBus {
            ram: &mut self.ram,
            rom: &self.rom,
            via1: &mut self.via1,
            via2: &mut self.via2,
            drv_port: self.iec_drv_port,
            clk: self.cpu.clk,
            via2_ports,
            rotation: &mut self.rotation,
            pending_set_overflow: false,
        };
        // Run whole instructions while the drive clock is behind the stop target
        // (VICE drivecpu.c:393 — `while (*clk_ptr < stop_clk)`). The reset sequence
        // is folded into the first instruction (see step_drive_instruction): VICE's
        // 6510 core dispatches IK_RESET and the first opcode in the SAME execute call,
        // so once `reset_pending` is armed the first `step_drive_instruction` always
        // runs even when `stop_clk` is still small — matching VICE's atomic reset+SEI.
        while self.reset_pending || self.cpu.clk < self.stop_clk {
            Self::step_instruction(&mut self.cpu, &mut self.reset_pending, &mut bus, &mut obs);
        }
        self.drive_clk = self.cpu.clk;
    }

    /// Advance the drive to an ABSOLUTE C64-clock target (VICE
    /// drive_cpu_execute_one/all at the $DD00 read/write instant). `c64_ref` is the
    /// C64 clock the drive was last advanced up to; returns the new reference (=
    /// `c64_clk`). A monotonic no-op when `c64_clk <= c64_ref`.
    #[inline]
    pub fn catch_up_to(&mut self, c64_clk: u64, c64_ref: u64) -> u64 {
        if c64_clk > c64_ref {
            self.run_cycles(c64_clk - c64_ref);
        }
        c64_clk
    }

    /// Attach a disk image to this drive (replaces any existing disk). For a D64
    /// the raw bytes are encoded to the per-track GCR bitstream and handed to the
    /// rotating-disk model, parking the head at track 18. G64 GCR-stream mounting
    /// is not yet wired (the rotating model only reads the simple/D64 path), so a
    /// G64 attaches the raw image for media parity but leaves the GCR path empty.
    pub fn attach_disk(&mut self, image: DiskImage) {
        if matches!(image.kind, DiskKind::D64) {
            let gcr = GcrImage::from_d64(&image.bytes);
            self.rotation.attach(gcr, self.drive_clk);
        }
        self.disk = Some(image);
    }

    /// Detach (eject) the disk from this drive.
    pub fn detach_disk(&mut self) {
        self.disk = None;
        self.rotation.detach();
    }

    /// Get a reference to the currently attached disk image, if any.
    pub fn get_attached_disk(&self) -> Option<&DiskImage> {
        self.disk.as_ref()
    }

    /// Read a byte of the drive's 2 KB RAM (mirrored every $800). Used to inspect
    /// the DOS job queue / sector buffers (the decoded sector at $0300) for the
    /// disk-read gate. No side effects.
    #[inline]
    pub fn drive_ram_read(&self, addr: u16) -> u8 {
        self.ram[(addr & 0x07FF) as usize]
    }

    /// Write a byte of the drive's 2 KB RAM (mirrored every $800). Used to poke
    /// the DOS job queue directly ($00=$80 READ, $06/$07 = track/sector) to drive
    /// a sector read without the full IEC command handshake.
    #[inline]
    pub fn drive_ram_write(&mut self, addr: u16, val: u8) {
        self.ram[(addr & 0x07FF) as usize] = val;
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
                drv_port: 0x85,
                clk: 0,
                via2_ports: Via2Ports::default(),
                rotation: &mut d.rotation,
                pending_set_overflow: false,
            };
            bus.write(0x0010, 0xAB);
            assert_eq!(bus.read(0x0810), 0xAB, "$0810 should mirror $0010");
            assert_eq!(bus.read(0x2010), 0xAB, "$2010 should mirror $0010");
        }
    }

    #[test]
    fn drive_bus_via1_iec_pb() {
        let mut d = Drive1541::new();
        d.via1.reset(0); // DDRA=DDRB=0 (all inputs), ORB/ORA=0
        let mut bus = DriveBus {
            ram: &mut d.ram,
            rom: &d.rom,
            via1: &mut d.via1,
            via2: &mut d.via2,
            drv_port: 0x85,
            clk: 0,
            via2_ports: Via2Ports::default(),
            rotation: &mut d.rotation,
            pending_set_overflow: false,
        };
        // VIA1 PB ($1800) read = 6522 PRB read with prb_pin = IEC tmp:
        //   byte = (tmp & ~DDRB) | (PRB & DDRB), tmp = (drv_port ^ 0x85)|0x1a.
        // With DDRB=0 (reset) the read returns tmp = (0x85^0x85)|0x1a = 0x1a.
        bus.write(0x1800, 0x42); // sets ORB latch (no effect with DDRB=0)
        assert_eq!(bus.read(0x1800), 0x1a, "$1800 PB read = IEC tmp with DDRB=0");
        // Drive all bits as outputs → read returns the ORB latch verbatim.
        bus.write(0x1802, 0xff); // DDRB = all outputs
        assert_eq!(bus.read(0x1800), 0x42, "$1800 PB read = ORB latch when DDRB=$FF");
        // VIA1 PRA ($1801): with DDRA=0xFF it reads back the stored ORA latch.
        bus.write(0x1803, 0xff); // DDRA = all outputs
        bus.write(0x1801, 0x33);
        assert_eq!(bus.read(0x1801), 0x33, "$1801 PRA reads ORA latch with DDRA=$FF");
    }

    #[test]
    fn drive_bus_via2_pcr_readback() {
        // VIA2 PCR ($1C0C) is a real 6522 register: it reads back the stored
        // value, NOT the old 0xFF stub. After reset PCR = 0x00 (the byte the
        // boot init at $F263 LDA $1C0C expects — fixes boot-basic-ready +2).
        let mut d = Drive1541::new();
        d.via2.reset(0);
        let mut bus = DriveBus {
            ram: &mut d.ram,
            rom: &d.rom,
            via1: &mut d.via1,
            via2: &mut d.via2,
            drv_port: 0x85,
            clk: 0,
            via2_ports: Via2Ports::default(),
            rotation: &mut d.rotation,
            pending_set_overflow: false,
        };
        assert_eq!(bus.read(0x1C0C), 0x00, "$1C0C PCR reads 0x00 after reset");
        bus.write(0x1C0C, 0xEE);
        assert_eq!(bus.read(0x1C0C), 0xEE, "$1C0C PCR reads back the stored value");
    }

    #[test]
    fn drive_via2_t1_underflow_raises_irq() {
        // Program VIA2 T1 (latch $0010) free-run + enable the T1 IRQ, then run
        // the timer past the underflow and assert the IRQ line goes active with
        // the IFR T1 bit set — the mechanism behind the periodic drive IRQ.
        let mut via = Via6522::new();
        via.reset(0);
        via.write(0x1C0B, VIA_ACR_T1_FREE_RUN, 10); // ACR: T1 free-run
        via.write(0x1C06, 0x10, 11); // T1LL = 0x10
        via.write(0x1C07, 0x00, 12); // T1LH = 0x00
        via.write(0x1C0E, 0xC0, 13); // IER: enable T1 (bit7 set + T1 bit)
        via.write(0x1C05, 0x00, 14); // T1CH write starts the timer (tal=0x0010)
        assert!(!via.irq_active, "no IRQ before underflow");
        via.run_alarms(14 + 0x10 + 4); // past t1zero
        assert!(via.irq_active, "T1 underflow asserts the IRQ line");
        assert_ne!(via.ifr & VIA_IM_T1, 0, "IFR T1 flag set");
        // Reading T1CL ($1C04) clears the T1 flag and drops the line.
        via.run_alarms(14 + 0x10 + 5);
        let _ = via.read(0x1C04, 14 + 0x10 + 5, Via2Ports::default());
        assert_eq!(via.ifr & VIA_IM_T1, 0, "reading T1CL clears the T1 flag");
        assert!(!via.irq_active, "IRQ line drops once IFR T1 cleared");
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
            drv_port: 0x85,
            clk: 0,
            via2_ports: Via2Ports::default(),
            rotation: &mut d.rotation,
            pending_set_overflow: false,
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
