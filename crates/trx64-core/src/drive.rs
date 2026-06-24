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

use crate::{
    drive_6510core::{
        drive_6510core_execute, DriveCore6510, DriveCore6510Bus, IntStatus, IK_RESET,
    },
    gcr::GcrImage,
    rotation::Rotation,
    viacore::{self, Via2Irq, Via2dBackend, ViaContext},
    RomError,
};

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

/// VICE per-VIA-instance `write_offset` (Spec 612 PL-6; viacore.c:529 / via2d.c
/// viacore_setup_context). A VIA STORE on the drive sees `rclk = clk_ptr - 1`:
/// the 6510 core does the store cycle's `CLK_ADD` BEFORE the store, so the
/// register/timer/IFR/IRQ logic must subtract that one cycle to land on the
/// pre-increment rclk VICE writes at. READS keep `read_offset = 0`. The rotation
/// effects (rotate_disk / set_ca2 / store_prb) instead read the FULL `clk_ptr`
/// (rotation.c reads `clk_ptr.value` directly), so only the viacore register
/// path gets the `-1`. Both drive VIAs use `write_offset = 1`.
const VIA_WRITE_OFFSET: u64 = 1;

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
        Self {
            pra_pin: 0xff,
            prb_pin: 0xff,
        }
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
    /// The T1 `t1zero` schedule (store re-arm `rclk+1+tal`, free-run reschedule
    /// `t1zero += full_cycle`) is a faithful port of viacore.c and is byte-exact
    /// against the golden for the full drive-boot-deep watchdog cadence. The
    /// drive-boot-deep KNOWN-RED that historically surfaced here (the 3rd watchdog
    /// T1 IRQ firing 2 drive cycles early) was NOT a timer bug at all — the timer
    /// schedule was already correct. It was a drive-6502 IRQ-dispatch latency gap:
    /// VICE's `interrupt_check_irq_delay` (drivecpu.c) delays the IRQ one extra
    /// cycle after a taken-no-page-cross branch (OPINFO_DELAYS_INTERRUPT) and
    /// defers it a full instruction after an I-clearing opcode (OPINFO_ENABLES_IRQ
    /// → IK_IRQPEND). Both are now modelled in `cpu.rs`, which restored byte-exact
    /// watchdog-IRQ entry cycles and let the early-firing diagnosis fall away.
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
    /// VIA2 — the 1:1-ported viacore `ViaContext` (viacore.rs). Its disk-controller
    /// hooks dispatch through a `Via2dBackend` built on the fly from `rotation` +
    /// `via2_irq` + `pending_set_overflow`.
    via2: &'a mut ViaContext,
    /// VIA2 IRQ-line mirror (the viacore `set_int` sink — see `viacore::Via2Irq`).
    via2_irq: &'a mut Via2Irq,
    /// Live IEC bus state at the VIA1 PB inputs (= iecbus.drv_port). Read by a
    /// `$1800` PB access so the drive's idle loop sees the C64-driven CLK/DATA/ATN.
    /// MUTATED by a `$1800` store: the drive re-folds the wired-AND against the
    /// fixed `cpu_bus` so its own pull on CLK/DATA is visible to its next read.
    drv_port: u8,
    /// C64-side IEC intent (= iecbus.cpu_bus), constant across this catch-up run.
    /// Used to re-fold `drv_port` after each `$1800` store (= via1d1541 store_prb).
    cpu_bus: u8,
    /// Live drive-CPU clock pointer (= VICE `via_context->clk_ptr`, Spec 612). The
    /// verbatim drive 6510 core advances `DriveCore6510.clk` between bus accesses
    /// via CLK_ADD; the VIA `rclk` for a register read/write and a timer-alarm
    /// catch-up must be that exact live clock at the access instant, NOT a stale
    /// snapshot. We thread it as a raw `*const u64` to `core.clk` — disjoint from
    /// the bus's borrowed RAM/ROM/VIA/rotation fields, read-only, single-threaded
    /// (the core invokes the bus synchronously), so there is no aliasing hazard.
    /// This is the literal `clk_ptr` indirection VICE keeps per VIA instance.
    /// It is `*mut` because the `cpu_reset` hook writes `*clk_ptr = 6` (VICE
    /// drivecpu.c:165 `drv->clk_ptr->value = 6`) — the 6-cycle reset sequence —
    /// exactly as VICE mutates the shared drive clock from the reset dispatch.
    clk_ptr: *mut u64,
    /// The rotating GCR disk model. `image == None` ⇒ no disk; the VIA2 read_pra/
    /// read_prb hooks then return 0xff (= the old static "no rotating disk"
    /// defaults). When a D64 is mounted this drives PRA (GCR_read), PRB bit7
    /// (SYNC), the stepper/motor/speed-zone from store_prb, and the byte-ready
    /// (SO) handshake consumed by the drive CPU's V flag.
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
    /// Live drive clock at the current access (= `*clk_ptr`). See `clk_ptr`.
    #[inline]
    fn clk(&self) -> u64 {
        // SAFETY: `clk_ptr` points at `Drive1541.core.clk`, a field disjoint from
        // every field this bus borrows. The read is synchronous inside a bus call
        // the core itself invoked, single-threaded, and never aliases a live `&mut`
        // to that same u64 at the instant of the read.
        unsafe { *self.clk_ptr }
    }

    /// Write the live drive clock (= VICE `drv->clk_ptr->value = n`). Used ONLY by
    /// the `cpu_reset` hook to seed the 6-cycle reset sequence. See `clk_ptr`.
    #[inline]
    fn set_clk(&mut self, v: u64) {
        // SAFETY: same disjoint-field reasoning as `clk`. The write happens inside
        // `cpu_reset` (the DO_INTERRUPT IK_RESET dispatch), at which instant the
        // core is not concurrently writing `core.clk` (it is between CLK_ADD steps).
        unsafe { *self.clk_ptr = v };
    }

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
        Via2Ports {
            pra_pin: 0xff,
            prb_pin: self.via1_iec_tmp(),
        }
    }

    /// Build the VIA2 disk-controller backend (via2d.ts) from the bus's borrowed
    /// rotation / IRQ-mirror / set-overflow fields, run `f` (a viacore entry), and
    /// flush the backend's `pending_set_overflow` back. `self.via2.clk` is synced
    /// to the live drive clock first — this IS VICE's `clk_ptr->value` indirection.
    /// `has_image` mirrors the TS `if (!drv) return` guard (no disk ⇒ hooks skip).
    #[inline]
    fn via2_with_backend<R>(
        &mut self,
        f: impl FnOnce(&mut ViaContext, &mut Via2dBackend) -> R,
    ) -> R {
        self.via2.clk = self.clk();
        let has_image = self.rotation.image.is_some();
        let mut backend = Via2dBackend {
            drive: self.rotation,
            number: 0,
            irq: self.via2_irq,
            pending_set_overflow: false,
            has_image,
        };
        let r = f(self.via2, &mut backend);
        if backend.pending_set_overflow {
            self.pending_set_overflow = true;
        }
        r
    }

    /// Dispatch any VIA2 alarms due at/before `clk` (= viacore run_pending_alarms,
    /// the PROCESS_ALARMS path). The alarm callbacks update IFR + the IRQ mirror.
    #[inline]
    fn via2_run_alarms(&mut self, clk: u64) {
        self.via2.clk = clk;
        self.via2_with_backend(|ctx, b| viacore::run_pending_alarms(ctx, b, clk, 0));
    }

    /// VIA2 register store (= viacore_store via the via2d backend). The viacore
    /// applies its own `write_offset` (= 1) so rclk = clk - 1; the rotation
    /// side-effects (store_prb / store_pcr) read the FULL clk via `ctx.clk`.
    #[inline]
    fn via2_store(&mut self, addr: u16, val: u8) {
        self.via2_with_backend(|ctx, b| viacore::viacore_store(ctx, b, addr, val));
    }

    /// VIA2 register read (= viacore_read via the via2d backend).
    #[inline]
    fn via2_read(&mut self, addr: u16) -> u8 {
        self.via2_with_backend(|ctx, b| viacore::viacore_read(ctx, b, addr))
    }
}

impl<'a> DriveCore6510Bus for DriveBus<'a> {
    /// PROCESS_ALARMS hook (6510core.c:139-146). VICE dispatches the VIA timer
    /// alarms up to `clk` here; the alarm callback raises the IFR and stamps the
    /// IRQ line. We run BOTH VIA alarm sets up to `clk` so an IFR underflow latches
    /// at the exact cycle it occurs (the per-VIA `irq_stamp` is the precise
    /// underflow rclk). The combined line is re-sampled into the core's IntStatus
    /// by the run loop at each instruction boundary (= where the drive 6510 core
    /// consults it). `clk` is the live `core.clk` the core passes in.
    #[inline]
    fn process_alarms(&mut self, clk: u64) {
        self.via1.run_alarms(clk);
        self.via2_run_alarms(clk);
    }

    /// drivecpu_rotate (drivecpu.c:423-433): advance the rotating GCR head to the
    /// live drive clock. Called by the core at the BVC/BVS/PHP opcodes and by
    /// LOCAL_SET_OVERFLOW(0) (CLV / ADC/SBC/ARR decimal-V-clear) — exactly where
    /// VICE consults the byte-ready handshake, NOT per cycle.
    #[inline]
    fn rotate(&mut self) {
        if self.rotation.image.is_some() {
            let clk = self.clk();
            self.rotation.rotate_disk(clk);
        }
    }

    /// drivecpu_byte_ready (drivecpu.c:423-433): the GCR byte-ready rising-edge
    /// flag the core folds into the V flag (SET_OVERFLOW) at BVC/BVS/PHP. Non-zero
    /// `byte_ready_edge` ⇒ a fresh byte latched since the last consult.
    #[inline]
    fn byte_ready(&mut self) -> bool {
        self.rotation.byte_ready_edge != 0
    }

    /// drivecpu_byte_ready_egde_clear (sic, drivecpu.c:423-433): clear the
    /// byte-ready rising-edge flag once consumed.
    #[inline]
    fn byte_ready_edge_clear(&mut self) {
        self.rotation.byte_ready_edge = 0;
    }

    /// cpu_reset (drivecpu.c:165-184): the drive 6502 hardware-reset sequence. VICE
    /// sets `drv->clk_ptr->value = 6` (the ~6 cycles the chip burns before the
    /// first opcode fetch) — we mutate the shared drive clock through `clk_ptr` to
    /// the same effect. The DO_INTERRUPT IK_RESET path that called us then pulls
    /// the reset vector ($FFFC/$FFFD) and JUMPs there, so the reset and the first
    /// opcode (SEI) are atomic within one execute call (first sampled record
    /// $EAA1@8, the atomic reset+SEI). The VIAs are reset by `cold_reset` (= VICE
    /// drive_reset → viacore_reset); a disk, if any, is dropped there too, so no
    /// rotation_reset is needed here for the boot path.
    #[inline]
    fn cpu_reset(&mut self) {
        self.set_clk(DRIVE_RESET_CYCLES);
    }

    #[inline]
    fn read(&mut self, addr: u16) -> u8 {
        let clk = self.clk();
        match addr {
            0x0000..=0x7FFF => {
                // VIA1: $1800-$1BFF (mirror every $400) — real 6522. PB carries
                // the IEC bus lines via the read_prb `tmp` (supplied as prb_pin);
                // the IFR/IER reads follow 6522 semantics so the drive IRQ handler
                // ($FE6C LDA $180D) sees the real CA1/timer flags, not a stale
                // last-written byte.
                if (0x1800..=0x1BFF).contains(&addr) {
                    let ports = self.via1_ports();
                    self.via1.run_alarms(clk);
                    return self.via1.read(addr, clk, ports);
                }
                // VIA2: $1C00-$1FFF — the 1:1-ported viacore (viacore.rs). PRA
                // ($1C01) / PRB ($1C00) reads sample the rotating disk through the
                // via2d read_pra/read_prb hooks (GCR_read / sync | wps | 0x6f) and
                // clear byte_ready_level inside the backend. viacore_read dispatches
                // any due alarms itself (rclk = clk) for PRB/timer/IFR regs.
                if (0x1C00..=0x1FFF).contains(&addr) {
                    return self.via2_read(addr);
                }
                // RAM mirrors: $0000-$07FF and all mirrors up to $7FFF
                self.ram[(addr & 0x07FF) as usize]
            }
            0x8000..=0xFFFF => self.rom[(addr & 0x7FFF) as usize],
        }
    }

    #[inline]
    fn write(&mut self, addr: u16, val: u8) {
        let clk = self.clk();
        // VIA register STORE rclk = clk_ptr - write_offset (= clk - 1, Spec 612):
        // the 6510 core's store-cycle CLK_ADD ran before the store, so the viacore
        // register/timer/IFR/IRQ logic lands one cycle earlier than the live clk.
        // The rotation side-effects (store_prb / store_pcr) keep the FULL `clk`
        // (rotation.c reads clk_ptr directly), so they read `self.clk()` themselves.
        let wclk = clk.wrapping_sub(VIA_WRITE_OFFSET);
        match addr {
            0x0000..=0x7FFF => {
                if (0x1800..=0x1BFF).contains(&addr) {
                    // Composed VIA1 PB output before the store (= VICE p_oldpb).
                    let reg = addr & 0x0f;
                    let old_pb = (self.via1.regs[0] | !self.via1.regs[2]) & 0xff;
                    self.via1.run_alarms(wclk);
                    self.via1.write(addr, val, wclk);
                    // via1d1541.c store_prb: a PB/DDRB write that CHANGES the drive's
                    // composed IEC output re-folds the wired-AND bus against the fixed
                    // C64 cpu_bus, so the drive's NEXT `$1800` read sees its own
                    // CLK/DATA pull. This is the cross-domain fix: without it the drive
                    // samples a STALE drv_port snapshot across a multi-instruction
                    // catch-up run and misreads its own / the C64's line at the byte
                    // handshake ($E95C isr01: `and #datin / bne frmerx`), falsely
                    // aborting the directory talk-send at byte 11. VICE gates store_prb
                    // on `byte != p_oldpb` (via1d1541.c:219) — so an ORB write with the
                    // bit as an INPUT (DDRB=0, output unchanged) does NOT re-fold.
                    if reg == 0 || reg == 2 {
                        let new_pb = (self.via1.regs[0] | !self.via1.regs[2]) & 0xff;
                        if new_pb != old_pb {
                            self.drv_port = crate::iec::fold_drv_port(self.cpu_bus, new_pb);
                        }
                    }
                    return;
                }
                if (0x1C00..=0x1FFF).contains(&addr) {
                    // viacore_store applies its own write_offset (= 1) so rclk =
                    // ctx.clk - 1 for the register/timer/IFR/IRQ logic, while the
                    // store_prb/store_pcr rotation hooks read the FULL ctx.clk —
                    // exactly the Spec 612 split. The stepper/motor/speed-zone/
                    // byte-ready side-effects run inside the via2d backend hooks.
                    self.via2_store(addr, val);
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
    /// The drive's DEDICATED verbatim 6502 core (drive_6510core.rs — the 1:1 port
    /// of VICE's 6510core.c DRIVE_CPU build). Replaces the shared C64 `Cpu6510`:
    /// the rotate / byte-ready / SET_OVERFLOW hooks are woven INTO the opcodes at
    /// the exact cycle, so the drive CPU is cycle-identical to VICE.
    pub core: DriveCore6510,
    /// Interrupt status mirror the verbatim core dispatches against (irq_clk /
    /// global_pending_int / IK_*). The combined VIA1∨VIA2 IRQ line is fed in via
    /// `int.set_irq` at each instruction boundary.
    pub int: IntStatus,
    ram: Box<[u8; 0x800]>,
    rom: Box<[u8; 0x8000]>,
    via1: Via6522,
    /// VIA2 — the 1:1-ported viacore `ViaContext` (viacore.rs). Replaces the
    /// distilled `Via6522` for VIA2: the disk-controller hooks (stepper/motor/
    /// SYNC/byte-ready) run through `Via2dBackend` exactly as via2d.ts does.
    via2: ViaContext,
    /// VIA2 IRQ-line mirror (see `viacore::Via2Irq`): the viacore `set_int` hook
    /// records the line level + rclk here; the run loop replays it into
    /// `int.set_irq(1, ..)` at the instruction boundary.
    via2_irq: Via2Irq,
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
    /// C64-side IEC intent (= VICE iecbus.cpu_bus: bit4=ATN, bit6=CLK, bit7=DATA),
    /// constant across a single drive catch-up run (the C64 only mutates it on a
    /// $DD00 write, which push-flushes the drive first). Refreshed by the FullBus
    /// push-flush alongside `iec_drv_port`. A `$1800` store inside the catch-up run
    /// re-folds the wired-AND bus against THIS fixed `cpu_bus` so the drive's next
    /// `$1800` read reflects its own pull (= via1d1541.c store_prb). Power-on 0xff
    /// (all released).
    pub iec_cpu_bus: u8,
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

/// Build a powered-on VIA2 `ViaContext` (via2d.ts:625-696 via2d_setup_context +
/// via2d.ts:612-618 via2d_init). Seeds the calloc-zero struct, runs
/// `viacore_setup_context` (power-on register latches, write_offset=1, external
/// cb1/cb2 high), then `viacore_init` (the 5 timer alarms). Sets `int_num = 1`
/// (the drive VIA2 is interrupt source 1; VIA1 is 0) and the VICE names. The
/// VIA2 is then cold-reset by `cold_reset()` via `viacore_reset`.
fn new_via2_ctx() -> ViaContext {
    let mut via = ViaContext::new();
    // via2d.ts:709-710 — myname / my_module_name (drive unit 8 → number 0).
    via.myname = Some("Drive0Via2".to_string());
    via.my_module_name = Some("VIA2D0".to_string());
    viacore::viacore_setup_context(&mut via);
    // via2d.ts:718 — via->irq_line = IK_IRQ = 2.
    via.irq_line = 2;
    // via2d.ts:729 — via->int_num. The drive wires VIA2 to IntStatus source 1.
    via.int_num = 1;
    viacore::viacore_init(&mut via);
    via
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
            core: DriveCore6510::new(),
            int: IntStatus::new(),
            ram: Box::new([0u8; 0x800]),
            rom: Box::new([0u8; 0x8000]),
            via1: Via6522::new(),
            via2: new_via2_ctx(),
            via2_irq: Via2Irq::new(),
            drive_clk: 0,
            last_sample_pc: None,
            sync_accum: 0,
            stop_clk: 0,
            reset_pending: true,
            iec_drv_port: 0x85,
            iec_cpu_bus: 0xff,
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

    /// Cold-reset the drive 6502 (VICE drivecpu_reset, drivecpu.c:193-211). Unlike
    /// the old shared-CPU path, the reset is NOT applied by pre-loading PC here:
    /// the verbatim core dispatches it through its IK_RESET path on the FIRST
    /// `drive_6510core_execute` call (the prologue sees `global_pending_int &
    /// IK_RESET`, runs `cpu_reset` → clk=6, then `load_addr($FFFC)` + JUMP). That
    /// reset and the first opcode (SEI) are atomic within one execute call, so the
    /// first sampled record is $EAA1@8 (not a spurious $EAA0@6) — exactly VICE.
    pub fn cold_reset(&mut self) {
        // Power-on register state (drivecpu cpu_regs init `{pc,ac,xr,yr,sp,flags=0}`,
        // sp=0). The drive 6502 powers on with SP=0; the IK_RESET dispatch does NOT
        // push (unlike an IRQ), so SP stays 0 through boot until the ROM's own TXS.
        self.core = DriveCore6510::new();
        // VICE drivecpu_reset: clk = 0, stop_clk = 0, last_clk = maincpu_clk (= 0 at
        // cold boot). The +6 reset-sequence cost is applied by the IK_RESET dispatch
        // (cpu_reset → clk=6) on the first run cycle, NOT here.
        self.core.clk = 0;
        // Reset the interrupt status to power-on (CLOCK_MAX sentinels, no pending)
        // and arm IK_RESET so the core's first execute dispatches the hardware reset
        // (= VICE interrupt_cpu_status_reset + interrupt_trigger_reset, the latter
        // setting `global_pending_int |= IK_RESET` — vice1541-facade.ts:659).
        self.int = IntStatus::new();
        self.int.global_pending_int |= IK_RESET;
        self.drive_clk = 0;
        self.stop_clk = 0;
        self.sync_accum = 0;
        self.reset_pending = true;
        self.last_sample_pc = None;
        self.iec_drv_port = 0x85;
        self.iec_cpu_bus = 0xff;
        // VICE viacore_reset (viacore.c:378-439) for both VIAs: clear port/ddr
        // and control regs, latch timers to power-on, clear IFR/IER. VIA1's PB/
        // DDRB start at 0 (all inputs, ORB latch 0) so the IEC read_prb formula
        // sees the right DDRB before the ROM programs $1802; VIA2's PCR → 0 so the
        // boot $1C0C read returns 0x00 as VICE does. Anchored at reset clock (0).
        self.via1.reset(0);
        // VIA2: re-seed a fresh power-on ViaContext, then viacore_reset at clk 0.
        // A fresh ctx clears any leftover alarm schedule / IFR / latches; the
        // viacore_reset then re-latches the timers and clears IFR/IER exactly as
        // VICE drive_reset → viacore_reset does (the via2d `reset` hook sets the
        // LED; no behavioural impact here). The IRQ mirror is cleared too.
        self.via2 = new_via2_ctx();
        self.via2_irq = Via2Irq::new();
        {
            self.via2.clk = 0;
            let mut backend = Via2dBackend {
                drive: &mut self.rotation,
                number: 0,
                irq: &mut self.via2_irq,
                pending_set_overflow: false,
                has_image: false,
            };
            viacore::viacore_reset(&mut self.via2, &mut backend);
        }
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

    /// Feed the combined VIA1∨VIA2 IRQ line into the verbatim core's `IntStatus`
    /// at the precise per-source rclk, mirroring VICE's `update_myviairq_rclk →
    /// set_int → interrupt_set_irq(int_status, int_num, level, rclk)` for each VIA.
    /// VIA1 is int_num 0, VIA2 is int_num 1 (both wired into the single drive CPU
    /// IRQ pin). `IntStatus::set_irq` stamps `irq_clk` only on the `nirq` 0→1 edge
    /// (first source) and arms the IK_IRQPEND tail (`irq_pending_clk = rclk + 3`)
    /// on the final deassert — exactly VICE.
    ///
    /// The rclk passed per source: on an ASSERT, the VIA's `irq_stamp` (the precise
    /// underflow / CA1-edge rclk its own `update_irq` recorded); on a DEASSERT, the
    /// live drive clock `now` (VICE clears the flag at the access rclk, which the
    /// boundary clock equals or just trails — and `irq_stamp` is the inactive
    /// `u64::MAX` sentinel there, which would overflow `rclk + 3`).
    #[inline]
    fn refresh_irq_line(int: &mut IntStatus, via1: &Via6522, via2_irq: &Via2Irq, now: u64) {
        let s1 = if via1.irq_active { via1.irq_stamp } else { now };
        let s2 = if via2_irq.active { via2_irq.stamp } else { now };
        int.set_irq(0, via1.irq_active, s1);
        int.set_irq(1, via2_irq.active, s2);
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

    /// DIAGNOSTIC: snapshot the drive VIA1 IRQ/CA1 state for the ATN-IRQ probe.
    /// Returns (ifr, ier, pcr, irq_active, irq_stamp).
    #[doc(hidden)]
    pub fn via1_irq_debug(&self) -> (u8, u8, u8, bool, u64) {
        (
            self.via1.ifr,
            self.via1.ier,
            self.via1.regs[VIA_PCR],
            self.via1.irq_active,
            self.via1.irq_stamp,
        )
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
    /// whole instructions while `core.clk < stop_clk` (drivecpu.c:393). The first run
    /// also consumes the 6-cycle reset sequence — but, unlike the old shared-CPU path,
    /// that is now dispatched by the verbatim core's IK_RESET path (cpu_reset → clk=6
    /// + JMP $FFFC), folded into the first execute call exactly like drivecpu.c.
    pub fn run_cycles(&mut self, n: u64) {
        // Advance the drive-clock target for this slice of main-CPU time.
        self.advance_stop_clk(n);
        // Disjoint split-borrow of `self`: `core`/`int`/`reset_pending` go to the
        // verbatim execute call; the rest (RAM/ROM/VIA/rotation/IEC) to the bus.
        let core = &mut self.core;
        let int = &mut self.int;
        let reset_pending = &mut self.reset_pending;
        // The bus reads the live drive clock through `clk_ptr` (= VICE clk_ptr,
        // Spec 612): the verbatim core advances `core.clk` via CLK_ADD between bus
        // accesses, so the VIA rclk for a register read/write or a timer-alarm
        // catch-up must be that exact clock at the access instant. `clk_ptr` is also
        // written by the `cpu_reset` hook (`*clk_ptr = 6`).
        let clk_ptr: *mut u64 = &mut core.clk;
        let mut bus = DriveBus {
            ram: &mut self.ram,
            rom: &self.rom,
            via1: &mut self.via1,
            via2: &mut self.via2,
            via2_irq: &mut self.via2_irq,
            drv_port: self.iec_drv_port,
            cpu_bus: self.iec_cpu_bus,
            clk_ptr,
            rotation: &mut self.rotation,
            pending_set_overflow: false,
        };
        // Run whole instructions while the drive clock is behind the stop target
        // (VICE drivecpu.c:393 — `while (*clk_ptr < stop_clk)`). Once `reset_pending`
        // is armed the first execute always runs even when `stop_clk` is still small
        // — VICE's 6510 core dispatches IK_RESET (and the first opcode, SEI) in the
        // SAME execute call, so the atomic reset+SEI lands regardless of stop_clk.
        while *reset_pending || core.clk < self.stop_clk {
            *reset_pending = false;
            // Sample the combined VIA1∨VIA2 IRQ line into the core's IntStatus at
            // the instruction boundary, BEFORE the execute call's prologue dispatch
            // (= VICE: the VIA alarm `set_int` has already stamped int_status by the
            // time DO_INTERRUPT's interrupt_check_irq_delay reads it). The VIAs'
            // alarms were brought up to `core.clk` by the prior step's PROCESS_ALARMS
            // and by every bus access.
            bus.via1.run_alarms(core.clk);
            bus.via2_run_alarms(core.clk);
            Self::refresh_irq_line(int, bus.via1, bus.via2_irq, core.clk);

            // One whole drive instruction (or one interrupt/reset dispatch) on the
            // verbatim core. The rotate/byte-ready/SET_OVERFLOW hooks are woven into
            // the opcodes (BVC/BVS/PHP/CLV), so the SO handshake is exact.
            drive_6510core_execute(core, &mut bus, int);

            // drive_cpu_set_overflow flush: a VIA2 store side-effect (set_ca2 on the
            // PCR CA2 low→high edge / store_prb motor-off) latched a byte-ready→V
            // request during this instruction. VICE pushes it straight into the
            // drive CPU's P register from the store; fold it in at the instruction
            // boundary (the store completed within this execute call).
            if bus.pending_set_overflow {
                bus.pending_set_overflow = false;
                core.reg_p |= 0x40; // P_OVERFLOW
            }
        }
        self.drive_clk = core.clk;
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
        let pc = self.core.reg_pc;
        if self.last_sample_pc == Some(pc) {
            return None;
        }
        self.last_sample_pc = Some(pc);
        Some((
            pc,
            self.core.reg_a,
            self.core.reg_x,
            self.core.reg_y,
            self.core.reg_sp,
            self.core.status(), // composite P (= LOCAL_STATUS, flag_n/flag_z folded in)
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
        let mut clk: u64 = 0;
        // Write via base address, read via mirror
        {
            let mut bus = DriveBus {
                ram: &mut d.ram,
                rom: &d.rom,
                via1: &mut d.via1,
                via2: &mut d.via2,
                via2_irq: &mut d.via2_irq,
                drv_port: 0x85,
                cpu_bus: 0xff,
                clk_ptr: &mut clk,
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
        let mut clk: u64 = 0;
        let mut bus = DriveBus {
            ram: &mut d.ram,
            rom: &d.rom,
            via1: &mut d.via1,
            via2: &mut d.via2,
            via2_irq: &mut d.via2_irq,
            drv_port: 0x85,
            cpu_bus: 0xff,
            clk_ptr: &mut clk,
            rotation: &mut d.rotation,
            pending_set_overflow: false,
        };
        // VIA1 PB ($1800) read = 6522 PRB read with prb_pin = IEC tmp:
        //   byte = (tmp & ~DDRB) | (PRB & DDRB), tmp = (drv_port ^ 0x85)|0x1a.
        // With DDRB=0 (reset) the read returns tmp = (0x85^0x85)|0x1a = 0x1a.
        bus.write(0x1800, 0x42); // sets ORB latch (no effect with DDRB=0)
        assert_eq!(
            bus.read(0x1800),
            0x1a,
            "$1800 PB read = IEC tmp with DDRB=0"
        );
        // Drive all bits as outputs → read returns the ORB latch verbatim.
        bus.write(0x1802, 0xff); // DDRB = all outputs
        assert_eq!(
            bus.read(0x1800),
            0x42,
            "$1800 PB read = ORB latch when DDRB=$FF"
        );
        // VIA1 PRA ($1801): with DDRA=0xFF it reads back the stored ORA latch.
        bus.write(0x1803, 0xff); // DDRA = all outputs
        bus.write(0x1801, 0x33);
        assert_eq!(
            bus.read(0x1801),
            0x33,
            "$1801 PRA reads ORA latch with DDRA=$FF"
        );
    }

    #[test]
    fn drive_bus_via2_pcr_readback() {
        // VIA2 PCR ($1C0C) is a real 6522 register (viacore.rs): it reads back the
        // stored value, NOT the old 0xFF stub. After power-on PCR = 0x00 (the byte
        // the boot init at $F263 LDA $1C0C expects — fixes boot-basic-ready +2).
        let mut d = Drive1541::new();
        let mut clk: u64 = 0;
        let mut bus = DriveBus {
            ram: &mut d.ram,
            rom: &d.rom,
            via1: &mut d.via1,
            via2: &mut d.via2,
            via2_irq: &mut d.via2_irq,
            drv_port: 0x85,
            cpu_bus: 0xff,
            clk_ptr: &mut clk,
            rotation: &mut d.rotation,
            pending_set_overflow: false,
        };
        assert_eq!(
            bus.read(0x1C0C),
            0x00,
            "$1C0C PCR reads 0x00 after power-on"
        );
        bus.write(0x1C0C, 0xEE);
        assert_eq!(
            bus.read(0x1C0C),
            0xEE,
            "$1C0C PCR reads back the stored value"
        );
    }

    #[test]
    fn drive_via2_t1_underflow_raises_irq() {
        // Program VIA2 T1 (latch $0010) free-run + enable the T1 IRQ, then run the
        // timer past the underflow and assert the IRQ line goes active with the IFR
        // T1 bit set — the mechanism behind the periodic drive IRQ. Exercises the
        // 1:1-ported viacore (ViaContext) + a no-disk Via2dBackend driving the IRQ
        // mirror. The store offset (write_offset=1) makes rclk = clk - 1.
        use crate::viacore::{
            self as vc, Via2Irq, Via2dBackend, ViaContext, VIA_ACR_T1_FREE_RUN, VIA_IM_T1,
        };
        let mut ctx = new_via2_ctx();
        let mut irq = Via2Irq::new();
        let mut rot = Rotation::new();
        // Power-on viacore_reset at clk 0.
        ctx.clk = 0;
        {
            let mut b = Via2dBackend {
                drive: &mut rot,
                number: 0,
                irq: &mut irq,
                pending_set_overflow: false,
                has_image: false,
            };
            vc::viacore_reset(&mut ctx, &mut b);
        }
        // Helper: store / read at clk through a fresh no-disk backend.
        let store =
            |ctx: &mut ViaContext, irq: &mut Via2Irq, rot: &mut Rotation, addr, val, clk| {
                ctx.clk = clk;
                let mut b = Via2dBackend {
                    drive: rot,
                    number: 0,
                    irq,
                    pending_set_overflow: false,
                    has_image: false,
                };
                vc::viacore_store(ctx, &mut b, addr, val);
            };
        store(
            &mut ctx,
            &mut irq,
            &mut rot,
            0x1C0B,
            VIA_ACR_T1_FREE_RUN,
            10,
        ); // ACR: T1 free-run
        store(&mut ctx, &mut irq, &mut rot, 0x1C06, 0x10, 11); // T1LL = 0x10
        store(&mut ctx, &mut irq, &mut rot, 0x1C07, 0x00, 12); // T1LH = 0x00
        store(&mut ctx, &mut irq, &mut rot, 0x1C0E, 0xC0, 13); // IER: enable T1
        store(&mut ctx, &mut irq, &mut rot, 0x1C05, 0x00, 14); // T1CH write starts the timer
        assert!(!irq.active, "no IRQ before underflow");
        // Dispatch alarms past t1zero.
        {
            ctx.clk = 14 + 0x10 + 4;
            let mut b = Via2dBackend {
                drive: &mut rot,
                number: 0,
                irq: &mut irq,
                pending_set_overflow: false,
                has_image: false,
            };
            vc::run_pending_alarms(&mut ctx, &mut b, 14 + 0x10 + 4, 0);
        }
        assert!(irq.active, "T1 underflow asserts the IRQ line");
        assert_ne!(ctx.ifr & VIA_IM_T1, 0, "IFR T1 flag set");
        // Reading T1CL ($1C04) clears the T1 flag and drops the line.
        {
            ctx.clk = 14 + 0x10 + 5;
            let mut b = Via2dBackend {
                drive: &mut rot,
                number: 0,
                irq: &mut irq,
                pending_set_overflow: false,
                has_image: false,
            };
            let _ = vc::viacore_read(&mut ctx, &mut b, 0x1C04);
        }
        assert_eq!(ctx.ifr & VIA_IM_T1, 0, "reading T1CL clears the T1 flag");
        assert!(!irq.active, "IRQ line drops once IFR T1 cleared");
    }

    #[test]
    fn drive_bus_rom_read() {
        let mut d = Drive1541::new();
        // Place a sentinel in the ROM region
        d.rom[0x4010] = 0xEA; // NOP at CPU $C010
        let mut clk: u64 = 0;
        let mut bus = DriveBus {
            ram: &mut d.ram,
            rom: &d.rom,
            via1: &mut d.via1,
            via2: &mut d.via2,
            via2_irq: &mut d.via2_irq,
            drv_port: 0x85,
            cpu_bus: 0xff,
            clk_ptr: &mut clk,
            rotation: &mut d.rotation,
            pending_set_overflow: false,
        };
        assert_eq!(bus.read(0xC010), 0xEA);
    }

    #[test]
    fn sample_pc_change_deduplicates() {
        let mut d = Drive1541::new();
        d.core.reg_pc = 0xEA00;
        // First call always returns Some
        assert!(d.sample_pc_change().is_some());
        // Second call with same PC returns None
        assert!(d.sample_pc_change().is_none());
        // Change PC → Some again
        d.core.reg_pc = 0xEA10;
        assert!(d.sample_pc_change().is_some());
    }
}
