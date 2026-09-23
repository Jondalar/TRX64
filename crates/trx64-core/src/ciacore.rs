//! ciacore.rs — the drive CIA (Spec 872 D1b): a 1:1 port of VICE `core/ciacore.c`.
//!
//! The C64's two CIAs (`cia.rs`) are a distilled 6526: timers, ICR latch and TOD, no
//! FLAG input, no serial shift register, no port hooks, and a latched-level IRQ test.
//! The 1581 takes ATN on its CIA's FLAG pin and runs its whole ATN handling from the
//! FLAG interrupt, and its serial port is the fast-serial path — so the drive's CIA is
//! the full ciacore: the IFR delay line (`ifr_delay`), the SDR delay line
//! (`sdr_delay`), FLAG / SDR / CNT / SP inputs, the port read/store hooks and the IRQ
//! line as `cia_set_int_clk` drives it. It shares only the timer (`cia::Ciat`, the
//! same `ciatimer.h` port) with the C64's CIAs; those two stay byte-identical because
//! nothing here is reachable from them.
//!
//! **The TOD is an 8520 event counter**, not ciacore's BCD clock (Spec 872 §D1b): a
//! 24-bit binary counter clocked by positive edges on the TOD pin, registers `$8/$9/$A`
//! = bits 0-7 / 8-15 / 16-23, `$B` not connected. A write stops the counter until the
//! LSB is written; reading the MSB latches all three until the LSB is read; CRB7 routes
//! writes to the alarm. On the stock 1581 board (WD1772, J1 open) the pin is held high
//! by R5 and never sees an edge, so no TOD alarm is scheduled and the counter moves
//! only when software writes it. VICE's 50 Hz BCD tick is not ported.
//!
//! **Clocks.** Every entry point runs at `self.clk` (VICE `*clk_ptr`), which the bus
//! sets to the live drive clock before each call — the same indirection the drive's
//! VIAs use. Alarm callbacks always receive `rclk` = the alarm's own clock (VICE passes
//! `offset = cpu_clk - alarm_clk` and every callback reconstructs exactly that).
//!
//! **The IRQ line.** `my_set_int` records each `(level, rclk)` in `irq_events`, in
//! order. The board replays them into the drive CPU's `IntStatus` at the instruction
//! boundary — where the 6502 core consults it — so the sequence `interrupt_set_irq`
//! sees is VICE's, including an assert and release inside one instruction.
//!
//! **RMW.** VICE's `ciacore_store` re-stores `last_read` one cycle early when the CPU
//! core flags a read-modify-write. TRX64's drive 6510 core performs that dummy store
//! itself as a bus write (it never sets a flag), so the store path here has no RMW arm.

use crate::cia::{shared_table, Ciat, CIAT_TABLEN, CLOCK_NEVER};
use crate::vice_snapshot_stream::{SnapshotModule, SnapshotT};

// ── cia.h register offsets and control bits ──────────────────────────────────────
pub const CIA_PRA: usize = 0;
pub const CIA_PRB: usize = 1;
pub const CIA_DDRA: usize = 2;
pub const CIA_DDRB: usize = 3;
pub const CIA_TAL: usize = 4;
pub const CIA_TAH: usize = 5;
pub const CIA_TBL: usize = 6;
pub const CIA_TBH: usize = 7;
pub const CIA_TOD_TEN: usize = 8;
pub const CIA_TOD_SEC: usize = 9;
pub const CIA_TOD_MIN: usize = 10;
pub const CIA_TOD_HR: usize = 11;
pub const CIA_SDR: usize = 12;
pub const CIA_ICR: usize = 13;
pub const CIA_CRA: usize = 14;
pub const CIA_CRB: usize = 15;

const CIA_CR_START: u8 = 0x01;
const CIA_CR_PBON: u8 = 0x02;
const CIA_CR_OUTMODE_TOGGLE: u8 = 0x04;
const CIA_CR_RUNMODE: u8 = 0x08;
const CIA_CR_RUNMODE_CONTINUOUS: u8 = 0x00;
const CIA_CRA_INMODE: u8 = 0x20;
const CIA_CRA_INMODE_PHI2: u8 = 0x00;
pub const CIA_CRA_SPMODE: u8 = 0x40;
pub const CIA_CRA_SPMODE_OUT: u8 = 0x40;
const CIA_CRA_SPMODE_IN: u8 = 0x00;
const CIA_CRB_INMODE: u8 = 0x60;
const CIA_CRB_INMODE_PHI2: u8 = 0x00;
const CIA_CRB_INMODE_TA: u8 = 0x40;
const CIA_CRB_ALARM_ALARM: u8 = 0x80;

pub const CIA_IM_SET: u32 = 0x80;
pub const CIA_IM_TA: u32 = 1;
pub const CIA_IM_TB: u32 = 2;
pub const CIA_IM_TOD: u32 = 4;
pub const CIA_IM_SDR: u32 = 8;
pub const CIA_IM_FLG: u32 = 16;
const CIA_IM_TBB: u32 = 0x100;

/// `cia_context->model` — CIA_MODEL_6526 (0), the "old" CIA. `cia1581d.c` never sets
/// it (`ciacore_setup_context` writes 0), so VICE runs the drive CIA as an old 6526.
const CIA_MODEL_6526: u32 = 0;

// ── sdr_delay bits (ciacore.c:59-99) ─────────────────────────────────────────────
const CIA_SDR_TOGGLE_CNT2: u32 = 0x0001;
const CIA_SDR_TOGGLE_CNT1: u32 = 0x0002;
const CIA_SDR_TOGGLE_CNT0: u32 = 0x0004;
const CIA_SDR_TOGGLE_CNT_1: u32 = 0x0008;
const CIA_SDR_NOGGLE_CNT2: u32 = 0x0010;
const CIA_SDR_NOGGLE_CNT1: u32 = 0x0020;
const CIA_SDR_NOGGLE_CNT0: u32 = 0x0040;
const CIA_SDR_NOGGLE_CNT_1: u32 = 0x0080;
const CIA_SDR_SET_SDR_IRQ3: u32 = 0x0100;
const CIA_SDR_SET_SDR_IRQ2: u32 = 0x0200;
const CIA_SDR_SET_SDR_IRQ1: u32 = 0x0400;
const CIA_SDR_SET_SDR_IRQ0: u32 = 0x0800;
const CIA_SDR_CNT0: u32 = 0x1000;
const CIA_SDR_CNT1: u32 = 0x2000;
const CIA_SDR_CNT2: u32 = 0x4000;
const CIA_SDR_CNT3: u32 = 0x8000;
const CIA_SDR_SET3: u32 = 0x0001_0000;
const CIA_SDR_SET2: u32 = 0x0002_0000;
const CIA_SDR_SET1: u32 = 0x0004_0000;
const CIA_SDR_SET0: u32 = 0x0008_0000;
const CIA_SDR_LEFTMOST: u32 = 0x0010_0000;

const CIA_SDR_CLEAR: u32 =
    CIA_SDR_NOGGLE_CNT2 | CIA_SDR_SET_SDR_IRQ3 | CIA_SDR_CNT0 | CIA_SDR_SET3 | CIA_SDR_LEFTMOST;
const CIA_SDR_ACTIVE: u32 = CIA_SDR_TOGGLE_CNT2
    | CIA_SDR_TOGGLE_CNT1
    | CIA_SDR_TOGGLE_CNT0
    | CIA_SDR_TOGGLE_CNT_1
    | CIA_SDR_NOGGLE_CNT2
    | CIA_SDR_NOGGLE_CNT1
    | CIA_SDR_NOGGLE_CNT0
    | CIA_SDR_NOGGLE_CNT_1
    | CIA_SDR_SET_SDR_IRQ3
    | CIA_SDR_SET_SDR_IRQ2
    | CIA_SDR_SET_SDR_IRQ1
    | CIA_SDR_SET_SDR_IRQ0
    | CIA_SDR_SET3
    | CIA_SDR_SET2
    | CIA_SDR_SET1
    | CIA_SDR_SET0;
const ALL_SDR_CNT: u32 = CIA_SDR_CNT0 | CIA_SDR_CNT1 | CIA_SDR_CNT2 | CIA_SDR_CNT3;
const ALL_SDR_TOGGLE_CNT: u32 =
    CIA_SDR_TOGGLE_CNT2 | CIA_SDR_TOGGLE_CNT1 | CIA_SDR_TOGGLE_CNT0 | CIA_SDR_TOGGLE_CNT_1;
const ALL_SDR_NOGGLE_CNT: u32 =
    CIA_SDR_NOGGLE_CNT2 | CIA_SDR_NOGGLE_CNT1 | CIA_SDR_NOGGLE_CNT0 | CIA_SDR_NOGGLE_CNT_1;

// ── ifr_delay bits (ciacore.c:101-153) ───────────────────────────────────────────
const CIA_IRQ_ACK1: u32 = 0x0001;
const CIA_IRQ_ACK0: u32 = 0x0002;
const CIA_IRQ_ACK_1: u32 = 0x0004;
const CIA_IRQ_ACK_2: u32 = 0x0008;
const CIA_IRQ_D7SET1: u32 = 0x0010;
const CIA_IRQ_D7SET0: u32 = 0x0020;
const CIA_IRQ_D7SET_1: u32 = 0x0040;
const CIA_IRQ_RAISE1: u32 = 0x0100;
const CIA_IRQ_RAISE0: u32 = 0x0200;
const CIA_IRQ_RAISE_1: u32 = 0x0400;
const CIA_IRQ_READ0: u32 = 0x1000;
const CIA_IRQ_READ1: u32 = 0x2000;
const CIA_IRQ_READ2: u32 = 0x4000;
const CIA_IRQ_CLEAR: u32 = CIA_IRQ_ACK_2 | CIA_IRQ_D7SET_1 | CIA_IRQ_RAISE_1 | CIA_IRQ_READ2;

const CIA_IFR_CURRENT: u32 = 0x01;
const CIA_IFR_NEXT: u32 = 0x02;
const CIA_IFR_CUR_NXT: u32 = 0x03;

/// ciacore.c:614 CIA_MAX_IDLE_CYCLES.
const CIA_MAX_IDLE_CYCLES: u64 = 5000;

/// ciacore.c:2037-2038 CIA_DUMP_VER_MAJOR / _MINOR.
const CIA_DUMP_VER_MAJOR: u8 = 2;
const CIA_DUMP_VER_MINOR: u8 = 5;

// =============================================================================
// The drive CPU's alarm context, as far as the CIA uses it (alarm.c / alarm.h)
// =============================================================================

/// The CIA's alarms (ciacore_init). The TOD alarm is not allocated: the 8520's TOD pin
/// has no edges on the stock board (module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CiaAlarm {
    Idle,
    Ta,
    Tb,
    Sdr,
}

impl CiaAlarm {
    fn idx(self) -> usize {
        self as usize
    }
}

/// alarm.h `alarm_context_t`, the four alarms the CIA registers. Same pending-array
/// semantics as viacore's port: `alarm_context_update_next_pending` scans with `<=`,
/// so of two alarms due at one clock the later array entry runs first.
#[derive(Clone, Debug)]
struct AlarmCtx {
    pending: [(CiaAlarm, u64); 4],
    num: usize,
    pending_idx: [i32; 4],
    next_clk: u64,
    next_idx: i32,
}

impl AlarmCtx {
    fn new() -> Self {
        Self {
            pending: [(CiaAlarm::Idle, 0); 4],
            num: 0,
            pending_idx: [-1; 4],
            next_clk: CLOCK_NEVER,
            next_idx: -1,
        }
    }

    fn update_next_pending(&mut self) {
        let mut nclk = CLOCK_NEVER;
        let mut nidx = self.next_idx;
        for i in 0..self.num {
            if self.pending[i].1 <= nclk {
                nclk = self.pending[i].1;
                nidx = i as i32;
            }
        }
        self.next_clk = nclk;
        self.next_idx = nidx;
    }

    fn set(&mut self, a: CiaAlarm, clk: u64) {
        let idx = self.pending_idx[a.idx()];
        if idx < 0 {
            let n = self.num;
            self.pending[n] = (a, clk);
            self.num += 1;
            if clk < self.next_clk {
                self.next_clk = clk;
                self.next_idx = n as i32;
            }
            self.pending_idx[a.idx()] = n as i32;
        } else {
            let idx = idx as usize;
            self.pending[idx].1 = clk;
            if self.next_clk > clk || idx as i32 == self.next_idx {
                self.update_next_pending();
            }
        }
    }

    fn unset(&mut self, a: CiaAlarm) {
        let idx = self.pending_idx[a.idx()];
        if idx < 0 {
            return;
        }
        let idx = idx as usize;
        if self.num > 1 {
            self.num -= 1;
            let last = self.num;
            if last != idx {
                let moved = self.pending[last];
                self.pending[idx] = moved;
                self.pending_idx[moved.0.idx()] = idx as i32;
            }
            if self.next_idx == idx as i32 {
                self.update_next_pending();
            } else if self.next_idx == last as i32 {
                self.next_idx = idx as i32;
            }
        } else {
            self.num = 0;
            self.next_clk = CLOCK_NEVER;
            self.next_idx = -1;
        }
        self.pending_idx[a.idx()] = -1;
    }

    /// ciacore.c `alarm_clk`: the clock the alarm is due, or 0 when not set.
    fn clk_of(&self, a: CiaAlarm) -> u64 {
        let idx = self.pending_idx[a.idx()];
        if idx >= 0 {
            self.pending[idx as usize].1
        } else {
            0
        }
    }
}

// =============================================================================
// Port hooks — the function pointers of cia_context_t (cia1581d.c installs them)
// =============================================================================

/// The board's side of the CIA: what the ports and the serial lines are wired to.
pub trait CiaBackend {
    /// `store_ciapa(cia, clk, byte)` — the composed port-A output changed.
    fn store_pa(&mut self, _clk: u64, _byte: u8) {}
    /// `store_ciapb(cia, clk, byte)` — the composed port-B output changed. `old_pb` is
    /// the previous composed byte (cia1581d.c compares against it).
    fn store_pb(&mut self, _clk: u64, _byte: u8, _old_pb: u8) {}
    /// `read_ciapa(cia)` — what a `$x0` read returns (the hook composes DDR itself).
    fn read_pa(&mut self, c_cia: &[u8; 16]) -> u8;
    /// `read_ciapb(cia)` — what a `$x1` read returns before the PB6/PB7 timer outputs.
    fn read_pb(&mut self, c_cia: &[u8; 16]) -> u8;
    /// `store_sdr(cia, byte)` — a byte finished shifting out.
    fn store_sdr(&mut self, _byte: u8) {}
    /// `set_sp(cia, rclk, bit)` — the SP output.
    fn set_sp(&mut self, _rclk: u64, _bit: bool) {}
    /// `set_cnt(cia, rclk, bit)` — the CNT output.
    fn set_cnt(&mut self, _rclk: u64, _bit: bool) {}
    /// `undump_ciapa` / `undump_ciapb` — a snapshot restore re-drives the ports.
    fn undump_pa(&mut self, _rclk: u64, _byte: u8) {}
    fn undump_pb(&mut self, _rclk: u64, _byte: u8) {}
    /// `do_reset_cia`.
    fn do_reset(&mut self) {}
}

/// The 8520's TOD: a 24-bit binary event counter (module doc).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tod8520 {
    pub counter: u32,
    pub alarm: u32,
    /// The three bytes as latched by a read of the MSB.
    pub latch: u32,
    pub latched: bool,
    /// A write of the MSB stops the counter; a write of the LSB starts it again.
    pub stopped: bool,
}

// =============================================================================
// cia_context_t
// =============================================================================

/// One drive CIA (VICE `cia_context_t`).
#[derive(Clone)]
pub struct CiaCore {
    pub c_cia: [u8; 16],
    pub irqflags: u32,
    ack_irqflags: u32,
    new_irqflags: u32,
    pub irq_enabled: bool,
    rdi: u64,
    ifr_clock: u64,
    ifr_delay: u32,
    tat: u32,
    tbt: u32,
    sr_bits: u32,
    sdr_force_finish: bool,
    sdr_valid: bool,
    shifter: u16,
    sdr_delay: u32,
    old_pa: u8,
    old_pb: u8,
    pub ta: Ciat,
    pub tb: Ciat,
    /// `ta->alarmclk` / `tb->alarmclk` (ciatimer.h).
    ta_alarmclk: u64,
    tb_alarmclk: u64,
    last_read: u8,
    write_offset: u64,
    model: u32,
    sp_in_state: bool,
    cnt_in_state: bool,
    cnt_out_state: bool,
    alarms: AlarmCtx,
    /// VICE `*clk_ptr` — the live drive clock at the current call (module doc).
    pub clk: u64,
    pub tod: Tod8520,
    /// `cia_set_int_clk(value, clk)` calls, in order (module doc).
    pub irq_events: Vec<(bool, u64)>,
    /// `myname` — the snapshot module name (`CIA1581D<n>`).
    pub myname: String,
}

impl CiaCore {
    /// `lib_calloc` + `ciacore_setup_context` + `ciacore_init`: a context that has not
    /// been reset yet (the board resets it at the drive's reset).
    pub fn new(myname: &str) -> Self {
        Self {
            c_cia: [0; 16],
            irqflags: 0,
            ack_irqflags: 0,
            new_irqflags: 0,
            irq_enabled: false,
            rdi: 0,
            ifr_clock: 0,
            ifr_delay: 0,
            tat: 0,
            tbt: 0,
            sr_bits: 0,
            sdr_force_finish: false,
            sdr_valid: false,
            shifter: 0,
            sdr_delay: 0,
            old_pa: 0,
            old_pb: 0,
            ta: Ciat { state: 0, latch: 0xffff, cnt: 0xffff, clk: 0 },
            tb: Ciat { state: 0, latch: 0xffff, cnt: 0xffff, clk: 0 },
            ta_alarmclk: CLOCK_NEVER,
            tb_alarmclk: CLOCK_NEVER,
            last_read: 0,
            write_offset: 1,
            model: CIA_MODEL_6526,
            // ciacore_init: "not internal state, so does not get reset on RESET".
            sp_in_state: true,
            cnt_in_state: true,
            cnt_out_state: false,
            alarms: AlarmCtx::new(),
            clk: 0,
            tod: Tod8520::default(),
            irq_events: Vec::new(),
            myname: myname.to_string(),
        }
    }

    #[inline]
    fn tab() -> &'static [u16; CIAT_TABLEN] {
        shared_table()
    }

    // ── my_set_int (ciacore.c:163-176) ───────────────────────────────────────
    fn my_set_int(&mut self, value: bool, rclk: u64) {
        self.irq_events.push((value, rclk));
        self.irq_enabled = value;
    }

    // ── ciatimer.h wrappers that touch the alarm context ──────────────────────
    fn ciat_set_alarm_ta(&mut self) {
        let t = self.ta.set_alarm(Self::tab());
        self.ta_alarmclk = t;
        if t != CLOCK_NEVER {
            self.alarms.set(CiaAlarm::Ta, t);
        } else {
            self.alarms.unset(CiaAlarm::Ta);
        }
    }
    fn ciat_set_alarm_tb(&mut self) {
        let t = self.tb.set_alarm(Self::tab());
        self.tb_alarmclk = t;
        if t != CLOCK_NEVER {
            self.alarms.set(CiaAlarm::Tb, t);
        } else {
            self.alarms.unset(CiaAlarm::Tb);
        }
    }
    fn ciat_ack_alarm_ta(&mut self) {
        self.alarms.unset(CiaAlarm::Ta);
        self.ta_alarmclk = CLOCK_NEVER;
    }
    fn ciat_ack_alarm_tb(&mut self) {
        self.alarms.unset(CiaAlarm::Tb);
        self.tb_alarmclk = CLOCK_NEVER;
    }
    fn ciat_reset_ta(&mut self, cclk: u64) {
        self.ta.reset(cclk);
        self.ta_alarmclk = CLOCK_NEVER;
        self.alarms.unset(CiaAlarm::Ta);
    }
    fn ciat_reset_tb(&mut self, cclk: u64) {
        self.tb.reset(cclk);
        self.tb_alarmclk = CLOCK_NEVER;
        self.alarms.unset(CiaAlarm::Tb);
    }

    // ── 8520 TOD alarm compare (replaces check_ciatodalarm) ──────────────────
    fn check_tod_alarm(&mut self, rclk: u64) {
        if self.tod.counter == self.tod.alarm {
            self.cia_set_irq_flag(rclk, CIA_IM_TOD);
        }
    }

    // ── ciacore.c:231-273 ─────────────────────────────────────────────────────
    fn cia_do_update_ta(&mut self, rclk: u64) {
        let n = self.ta.update(rclk, Self::tab());
        if n != 0 {
            self.cia_set_irq_flag(rclk, CIA_IM_TA);
            self.tat = (self.tat + n) & 1;
        }
    }

    fn cia_do_update_tb(&mut self, rclk: u64) {
        let n = self.tb.update(rclk, Self::tab());
        if n != 0 {
            self.cia_set_irq_flag(rclk, CIA_IM_TB);
            if self.model == CIA_MODEL_6526 && self.rdi == rclk.wrapping_sub(1) {
                self.irqflags |= CIA_IM_TBB;
            } else {
                self.irqflags &= !CIA_IM_TBB;
            }
            self.tbt = (self.tbt + n) & 1;
        }
    }

    /// ciacore.c:275-283 — `ciat_single_step` always returns 0, so no flag is raised.
    fn cia_do_step_tb(&mut self, _rclk: u64) {
        if self.tb.is_running() {
            self.tb.single_step();
            self.ciat_set_alarm_tb();
        }
    }

    // ── ciacore.c:289-344 ─────────────────────────────────────────────────────
    fn cia_update_ta<B: CiaBackend + ?Sized>(&mut self, b: &mut B, rclk: u64) {
        let mut last_tmp = 0u64;
        let mut tmp = self.ta_alarmclk;
        while tmp <= rclk {
            self.ciacore_intta(b, tmp);
            last_tmp = tmp;
            tmp = self.ta_alarmclk;
        }
        if last_tmp != rclk {
            self.cia_do_update_ta(rclk);
        }
    }

    fn cia_update_tb<B: CiaBackend + ?Sized>(&mut self, b: &mut B, rclk: u64) {
        if (self.c_cia[CIA_CRB] & (CIA_CRB_INMODE_TA | CIA_CR_START))
            == (CIA_CRB_INMODE_TA | CIA_CR_START)
        {
            self.cia_update_ta(b, rclk);
        }
        let mut last_tmp = 0u64;
        let mut tmp = self.tb_alarmclk;
        while tmp <= rclk {
            self.ciacore_inttb(tmp);
            last_tmp = tmp;
            tmp = self.tb_alarmclk;
        }
        if last_tmp != rclk {
            self.cia_do_update_tb(rclk);
        }
    }

    // ── ciacore.c:377-440 cia_run_ifr_cycle ───────────────────────────────────
    fn cia_run_ifr_cycle(&mut self) {
        let mut delay = self.ifr_delay;
        let rclk = self.ifr_clock;

        if self.model != CIA_MODEL_6526 {
            if delay & CIA_IRQ_ACK0 != 0 {
                self.irqflags &= !self.ack_irqflags;
                self.ack_irqflags = 0;
            }
        } else if delay & CIA_IRQ_ACK0 != 0 {
            self.irqflags &= !self.ack_irqflags;
            self.irqflags &= !CIA_IM_SET;
            self.ack_irqflags = 0;
        }

        if self.new_irqflags & (self.c_cia[CIA_ICR] as u32) & 0x1f != 0 {
            if self.model != CIA_MODEL_6526 {
                if self.rdi.wrapping_add(1) == rclk {
                    delay |= CIA_IRQ_RAISE1;
                    delay |= CIA_IRQ_D7SET1;
                } else {
                    delay |= CIA_IRQ_RAISE0;
                    delay |= CIA_IRQ_D7SET0;
                }
            } else {
                delay |= CIA_IRQ_RAISE1;
                delay |= CIA_IRQ_D7SET1;
            }
        }

        if delay & CIA_IRQ_D7SET0 != 0 {
            self.irqflags |= CIA_IM_SET;
        }
        if delay & CIA_IRQ_RAISE0 != 0 {
            self.my_set_int(true, rclk);
        }

        self.new_irqflags = 0;

        delay <<= 1;
        delay &= !CIA_IRQ_CLEAR;
        self.ifr_delay = delay;
        self.ifr_clock = self.ifr_clock.wrapping_add(1);
    }

    // ── ciacore.c:460-523 cia_ifr_current ─────────────────────────────────────
    fn cia_ifr_current(&mut self, rclk: u64, what: u32) {
        if self.ta_alarmclk != rclk && self.tb_alarmclk != rclk {
            if what & CIA_IFR_CURRENT != 0 {
                self.cia_run_ifr_cycle();
            }
            if what & CIA_IFR_NEXT != 0 {
                let delay = self.ifr_delay;
                if delay & CIA_IRQ_RAISE0 != 0 {
                    // USE_IRQ_RAISE0_SHORTCUT
                    self.my_set_int(true, rclk + 1);
                } else if delay & CIA_IRQ_RAISE1 != 0 {
                    self.alarms.set(CiaAlarm::Idle, rclk + 1);
                }
            }
        }
    }

    // ── ciacore.c:533-547 cia_ifr_catchup ─────────────────────────────────────
    fn cia_ifr_catchup(&mut self, rclk: u64) {
        if self.ifr_clock < rclk {
            while (self.ifr_delay != 0 || self.new_irqflags != 0 || self.ack_irqflags != 0)
                && self.ifr_clock < rclk
            {
                self.cia_run_ifr_cycle();
            }
            self.ifr_clock = rclk;
        }
    }

    // ── ciacore.c:590-598 cia_set_irq_flag ────────────────────────────────────
    fn cia_set_irq_flag(&mut self, rclk: u64, bits: u32) {
        self.cia_ifr_catchup(rclk);
        self.irqflags |= bits;
        self.new_irqflags |= bits;
        self.ack_irqflags &= !bits;
    }

    // ── ciacore.c:616-670 ciacore_reset ───────────────────────────────────────
    pub fn reset<B: CiaBackend + ?Sized>(&mut self, b: &mut B) {
        let clk = self.clk;
        self.c_cia = [0; 16];
        self.rdi = 0;
        self.sr_bits = 0;

        self.ciat_reset_ta(clk);
        self.ciat_reset_tb(clk);

        self.sdr_valid = false;
        self.sdr_force_finish = false;
        self.sdr_delay = CIA_SDR_CNT0 | CIA_SDR_CNT1 | CIA_SDR_CNT2 | CIA_SDR_CNT3;

        self.sp_in_state = true;
        self.cnt_in_state = true;
        self.cnt_out_state = true;

        // The 8520 TOD (module doc): cleared, stopped, alarm clear. No TOD alarm is
        // scheduled — the pin sees no edges on the stock board.
        self.tod = Tod8520 { stopped: true, ..Tod8520::default() };

        self.irqflags = 0;
        self.ack_irqflags = 0;
        self.new_irqflags = 0;
        self.irq_enabled = false;

        self.ifr_clock = 0;
        self.ifr_delay = 0;

        self.my_set_int(false, clk);

        self.old_pa = 0xff;
        self.old_pb = 0xff;

        b.do_reset();

        self.alarms.set(CiaAlarm::Idle, clk + CIA_MAX_IDLE_CYCLES);
        self.alarms.unset(CiaAlarm::Ta);
        self.alarms.unset(CiaAlarm::Tb);
        self.alarms.unset(CiaAlarm::Sdr);
    }

    // ── ciacore.c:675-734 strange_extra_sdr_flags ─────────────────────────────
    fn strange_extra_sdr_flags<B: CiaBackend + ?Sized>(&mut self, _b: &mut B, rclk: u64, byte: u8) {
        if (self.sr_bits > 1 && self.sr_bits < 15)
            || (self.sr_bits == 15 && self.sdr_delay & CIA_SDR_CNT2 == 0)
        {
            self.schedule_sdr_alarm(rclk, CIA_SDR_SET_SDR_IRQ2);
        }

        if byte & CIA_CRA_SPMODE_OUT == CIA_CRA_SPMODE_IN {
            let sdr_delay = self.sdr_delay;
            let cnt_delay = sdr_delay;
            let cnt_wanted = CIA_SDR_CNT1 | CIA_SDR_CNT2;
            let mut force_finish = (cnt_delay & cnt_wanted) != cnt_wanted;
            if !force_finish
                && self.sr_bits != 2
                && sdr_delay & CIA_SDR_TOGGLE_CNT2 == 0
                && sdr_delay & CIA_SDR_TOGGLE_CNT1 == 0
                && sdr_delay & CIA_SDR_TOGGLE_CNT0 != 0
            {
                force_finish = true;
            }
            self.sdr_force_finish = force_finish;
        } else {
            if !self.cnt_out_state && self.sr_bits != 0 {
                self.shifter <<= 1;
            }
            if self.sdr_force_finish {
                self.schedule_sdr_alarm(rclk, CIA_SDR_SET_SDR_IRQ2);
                self.sdr_force_finish = false;
            }
        }
    }

    // ── ciacore.c:742-784 ciacore_update_pb67 ─────────────────────────────────
    fn update_pb67<B: CiaBackend + ?Sized>(&mut self, b: &mut B, rclk: u64) -> bool {
        let mut byte = self.c_cia[CIA_PRB] | !self.c_cia[CIA_DDRB];
        let mut current_called = false;

        if (self.c_cia[CIA_CRA] | self.c_cia[CIA_CRB]) & CIA_CR_PBON != 0 {
            if self.c_cia[CIA_CRA] & CIA_CR_PBON != 0 {
                self.cia_update_ta(b, rclk);
                byte &= 0xbf;
                let out = if self.c_cia[CIA_CRA] & CIA_CR_OUTMODE_TOGGLE != 0 {
                    self.tat != 0
                } else {
                    self.ta.is_underflow_clk()
                };
                if out {
                    byte |= 0x40;
                }
            }
            if self.c_cia[CIA_CRB] & CIA_CR_PBON != 0 {
                self.cia_update_tb(b, rclk);
                byte &= 0x7f;
                let out = if self.c_cia[CIA_CRB] & CIA_CR_OUTMODE_TOGGLE != 0 {
                    self.tbt != 0
                } else {
                    self.tb.is_underflow_clk()
                };
                if out {
                    byte |= 0x80;
                }
            }
            self.cia_ifr_catchup(rclk);
            self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
            current_called = true;
        }

        if byte != self.old_pb {
            let old = self.old_pb;
            b.store_pb(self.clk, byte, old);
            self.old_pb = byte;
        }
        current_called
    }

    // ── alarm dispatch ────────────────────────────────────────────────────────

    /// alarm_context_dispatch: fire the next pending alarm (its callback sees its own
    /// clock as `rclk`).
    fn dispatch_next<B: CiaBackend + ?Sized>(&mut self, b: &mut B) {
        let idx = self.alarms.next_idx;
        if idx < 0 {
            return;
        }
        let (a, aclk) = self.alarms.pending[idx as usize];
        match a {
            CiaAlarm::Ta => self.ciacore_intta_entry(b, aclk),
            CiaAlarm::Tb => self.ciacore_inttb_entry(aclk),
            CiaAlarm::Sdr => self.ciacore_intsdr_entry(b, aclk),
            CiaAlarm::Idle => self.ciacore_idle(b, aclk),
        }
    }

    /// ciacore.c:213-219 run_pending_alarms — at a register access: alarms `< clk`.
    fn run_pending_alarms<B: CiaBackend + ?Sized>(&mut self, b: &mut B, clk: u64) {
        let mut guard = 0u32;
        while clk > self.alarms.next_clk {
            self.dispatch_next(b);
            guard += 1;
            if guard > 1_000_000 {
                break;
            }
        }
    }

    /// 6510core.c:139-143 PROCESS_ALARMS — the CPU loop: every alarm `<= clk`.
    pub fn process_alarms<B: CiaBackend + ?Sized>(&mut self, b: &mut B, clk: u64) {
        let mut guard = 0u32;
        while clk >= self.alarms.next_clk {
            self.dispatch_next(b);
            guard += 1;
            if guard > 1_000_000 {
                break;
            }
        }
    }

    /// The next clock an alarm is due (`CLOCK_NEVER` when none).
    pub fn next_alarm_clk(&self) -> u64 {
        self.alarms.next_clk
    }

    // ── ciacore.c:786-1082 ciacore_store_internal ─────────────────────────────
    fn store_internal<B: CiaBackend + ?Sized>(&mut self, b: &mut B, addr: u16, mut byte: u8) {
        let addr = (addr & 0xf) as usize;
        let rclk = self.clk.wrapping_sub(self.write_offset);

        self.run_pending_alarms(b, rclk);

        match addr {
            CIA_PRA | CIA_DDRA => {
                self.c_cia[addr] = byte;
                byte = self.c_cia[CIA_PRA] | !self.c_cia[CIA_DDRA];
                if byte != self.old_pa {
                    b.store_pa(self.clk, byte);
                    self.old_pa = byte;
                }
            }
            CIA_PRB | CIA_DDRB => {
                self.c_cia[addr] = byte;
                self.update_pb67(b, rclk);
                // pulse_ciapc: nothing on the 1581.
            }
            CIA_TAL => {
                self.cia_update_ta(b, rclk);
                self.cia_ifr_catchup(rclk);
                self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                self.ta.set_latch_lo(byte);
                self.ciat_set_alarm_ta();
            }
            CIA_TBL => {
                self.cia_update_tb(b, rclk);
                self.cia_ifr_catchup(rclk);
                self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                self.tb.set_latch_lo(byte);
                self.ciat_set_alarm_tb();
            }
            CIA_TAH => {
                self.cia_update_ta(b, rclk);
                self.cia_ifr_catchup(rclk);
                self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                self.ta.set_latch_hi(byte);
                self.ciat_set_alarm_ta();
            }
            CIA_TBH => {
                self.cia_update_tb(b, rclk);
                self.cia_ifr_catchup(rclk);
                self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                self.tb.set_latch_hi(byte);
                self.ciat_set_alarm_tb();
            }
            CIA_TOD_TEN | CIA_TOD_SEC | CIA_TOD_MIN | CIA_TOD_HR => {
                // 8520 (module doc): $8/$9/$A are counter bits 0-7/8-15/16-23, $B is
                // not connected. CRB7 routes the write to the alarm.
                if addr == CIA_TOD_HR {
                    return;
                }
                let shift = 8 * (addr - CIA_TOD_TEN) as u32;
                let mask = !(0xffu32 << shift);
                let changed;
                if self.c_cia[CIA_CRB] & CIA_CRB_ALARM_ALARM != 0 {
                    let v = (self.tod.alarm & mask) | ((byte as u32) << shift);
                    changed = v != self.tod.alarm;
                    self.tod.alarm = v;
                } else {
                    if addr == CIA_TOD_TEN {
                        self.tod.stopped = false;
                    }
                    if addr == CIA_TOD_MIN {
                        self.tod.stopped = true;
                    }
                    let v = (self.tod.counter & mask) | ((byte as u32) << shift);
                    changed = v != self.tod.counter;
                    self.tod.counter = v;
                }
                if changed {
                    self.check_tod_alarm(rclk);
                    self.cia_ifr_catchup(rclk);
                    self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                }
            }
            CIA_SDR => {
                if self.c_cia[CIA_CRA] & CIA_CRA_SPMODE == CIA_CRA_SPMODE_OUT {
                    self.schedule_sdr_alarm(rclk, CIA_SDR_SET1);
                }
                self.c_cia[addr] = byte;
            }
            CIA_ICR => {
                self.cia_update_ta(b, rclk);
                self.cia_update_tb(b, rclk);
                self.cia_ifr_catchup(rclk);
                self.cia_ifr_current(rclk, CIA_IFR_CURRENT);

                if byte as u32 & CIA_IM_SET != 0 {
                    self.c_cia[CIA_ICR] |= byte & 0x7f;
                } else {
                    self.c_cia[CIA_ICR] &= !(byte & 0x7f);
                }

                if self.irqflags & (self.c_cia[CIA_ICR] as u32) & 0x7f != 0 {
                    if !self.irq_enabled {
                        if self.model != CIA_MODEL_6526 {
                            if self.ifr_delay & CIA_IRQ_READ1 == 0 {
                                self.ifr_delay |= CIA_IRQ_RAISE0;
                                self.ifr_delay |= CIA_IRQ_D7SET0;
                            }
                        } else {
                            self.ifr_delay |= CIA_IRQ_RAISE1;
                            self.ifr_delay |= CIA_IRQ_D7SET1;
                        }
                    }
                } else if self.model == CIA_MODEL_6526 && self.ifr_delay & CIA_IRQ_ACK_1 != 0 {
                    self.ifr_delay &= !CIA_IRQ_RAISE0;
                    self.ifr_delay &= !CIA_IRQ_D7SET0;
                }

                if self.c_cia[CIA_ICR] as u32 & CIA_IM_TA != 0 {
                    self.ciat_set_alarm_ta();
                }
                if self.c_cia[CIA_ICR] as u32 & CIA_IM_TB != 0 {
                    self.ciat_set_alarm_tb();
                }

                self.cia_ifr_current(rclk, CIA_IFR_NEXT);
            }
            CIA_CRA => {
                self.cia_update_ta(b, rclk);

                if byte & CIA_CR_START != 0 && self.c_cia[CIA_CRA] & CIA_CR_START == 0 {
                    self.tat = 1;
                }

                if (byte ^ self.c_cia[CIA_CRA]) & CIA_CRA_SPMODE != 0 {
                    self.strange_extra_sdr_flags(b, rclk, byte);
                    self.sr_bits = 0;
                    self.sdr_valid = false;
                    self.sdr_delay &= !(ALL_SDR_TOGGLE_CNT | ALL_SDR_NOGGLE_CNT);
                    if !self.cnt_out_state {
                        self.cnt_out_state = true;
                        b.set_cnt(rclk, true);
                    }
                }

                self.ta.set_ctrl(byte);
                self.ciat_set_alarm_ta();

                self.c_cia[addr] = byte & 0xef;

                if !self.update_pb67(b, rclk) {
                    self.cia_ifr_catchup(rclk);
                    self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                }
            }
            CIA_CRB => {
                if byte & 1 != 0 && self.c_cia[CIA_CRB] & CIA_CR_START == 0 {
                    self.tbt = 1;
                }
                self.cia_update_ta(b, rclk);
                self.cia_update_tb(b, rclk);

                if byte & CIA_CRB_INMODE_TA != 0 {
                    self.ciat_set_alarm_ta();
                    self.tb.set_ctrl(byte | 0x20);
                    self.ciat_set_alarm_tb();
                } else {
                    self.tb.set_ctrl(byte);
                    self.ciat_set_alarm_tb();
                }

                self.c_cia[addr] = byte & 0xef;

                if !self.update_pb67(b, rclk) {
                    self.cia_ifr_catchup(rclk);
                    self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                }
            }
            _ => self.c_cia[addr] = byte,
        }
    }

    /// ciacore.c:1084-1097 ciacore_store (the RMW arm is the CPU core's, module doc).
    pub fn store<B: CiaBackend + ?Sized>(&mut self, b: &mut B, addr: u16, byte: u8) {
        self.store_internal(b, addr, byte);
    }

    // ── ciacore.c:1101-1345 ciacore_read ──────────────────────────────────────
    pub fn read<B: CiaBackend + ?Sized>(&mut self, b: &mut B, addr: u16) -> u8 {
        let addr = (addr & 0xf) as usize;
        let rclk = self.clk;

        self.run_pending_alarms(b, rclk);

        match addr {
            CIA_PRA => {
                self.last_read = b.read_pa(&self.c_cia);
                self.last_read
            }
            CIA_PRB => {
                let mut byte = b.read_pb(&self.c_cia);
                if (self.c_cia[CIA_CRA] | self.c_cia[CIA_CRB]) & CIA_CR_PBON != 0 {
                    if self.c_cia[CIA_CRA] & CIA_CR_PBON != 0 {
                        self.cia_update_ta(b, rclk);
                        byte &= 0xbf;
                        let out = if self.c_cia[CIA_CRA] & CIA_CR_OUTMODE_TOGGLE != 0 {
                            self.tat != 0
                        } else {
                            self.ta.is_underflow_clk()
                        };
                        if out {
                            byte |= 0x40;
                        }
                    }
                    if self.c_cia[CIA_CRB] & CIA_CR_PBON != 0 {
                        self.cia_update_tb(b, rclk);
                        byte &= 0x7f;
                        let out = if self.c_cia[CIA_CRB] & CIA_CR_OUTMODE_TOGGLE != 0 {
                            self.tbt != 0
                        } else {
                            self.tb.is_underflow_clk()
                        };
                        if out {
                            byte |= 0x80;
                        }
                    }
                    self.cia_ifr_catchup(rclk);
                    self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                }
                self.last_read = byte;
                byte
            }
            CIA_TAL | CIA_TAH => {
                self.cia_update_ta(b, rclk);
                self.cia_ifr_catchup(rclk);
                self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                let t = self.ta.read_timer();
                self.last_read = if addr == CIA_TAL { (t & 0xff) as u8 } else { (t >> 8) as u8 };
                self.last_read
            }
            CIA_TBL | CIA_TBH => {
                self.cia_update_tb(b, rclk);
                self.cia_ifr_catchup(rclk);
                self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                let t = self.tb.read_timer();
                self.last_read = if addr == CIA_TBL { (t & 0xff) as u8 } else { (t >> 8) as u8 };
                self.last_read
            }
            CIA_TOD_TEN | CIA_TOD_SEC | CIA_TOD_MIN | CIA_TOD_HR => {
                self.last_read = self.tod_read(addr);
                self.last_read
            }
            CIA_SDR => {
                self.last_read = self.c_cia[CIA_SDR];
                self.last_read
            }
            CIA_ICR => {
                self.cia_update_ta(b, rclk);
                self.cia_update_tb(b, rclk);
                self.cia_ifr_catchup(rclk);
                self.cia_ifr_current(rclk, CIA_IFR_CURRENT);

                self.rdi = rclk;
                // read_ciaicr: nothing on the 1581.

                self.ciat_set_alarm_ta();
                self.ciat_set_alarm_tb();

                if self.irqflags & CIA_IM_TBB != 0 {
                    self.irqflags &= !(CIA_IM_TBB | CIA_IM_TB);
                }

                let result;
                if self.model != CIA_MODEL_6526 {
                    if self.ifr_delay & CIA_IRQ_RAISE0 != 0 && self.irqflags & 0x1f != 0 {
                        self.irqflags |= CIA_IM_SET;
                    }
                    if self.irqflags & 0x9f != 0 {
                        self.ack_irqflags |= (self.irqflags & 0x9f) | 0x80;
                    }
                    self.ifr_delay |= CIA_IRQ_ACK1;
                    self.ifr_delay &= !CIA_IRQ_RAISE0;
                    self.ifr_delay &= !CIA_IRQ_D7SET0;
                    result = self.irqflags;
                } else {
                    self.ifr_delay |= CIA_IRQ_ACK1;
                    self.ifr_delay &= !CIA_IRQ_RAISE0;
                    result = self.irqflags;
                    self.irqflags &= CIA_IM_SET;
                    self.new_irqflags = 0;
                }

                self.ifr_delay |= CIA_IRQ_READ0;

                self.my_set_int(false, rclk);

                self.cia_ifr_current(rclk, CIA_IFR_NEXT);

                self.last_read = result as u8;
                self.last_read
            }
            CIA_CRA => {
                self.cia_update_ta(b, rclk);
                self.cia_ifr_catchup(rclk);
                self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                self.last_read = (self.c_cia[CIA_CRA] & !CIA_CR_START) | self.ta.is_running() as u8;
                self.last_read
            }
            CIA_CRB => {
                self.cia_update_tb(b, rclk);
                self.cia_ifr_catchup(rclk);
                self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                self.last_read = (self.c_cia[CIA_CRB] & !CIA_CR_START) | self.tb.is_running() as u8;
                self.last_read
            }
            _ => {
                self.last_read = self.c_cia[addr];
                self.last_read
            }
        }
    }

    /// The 8520 TOD read (module doc): the MSB latches, the LSB releases.
    fn tod_read(&mut self, addr: usize) -> u8 {
        if addr == CIA_TOD_HR {
            // Not connected; nothing on the chip answers for it and no document gives
            // a value. Read as 0.
            return 0;
        }
        if !self.tod.latched {
            self.tod.latch = self.tod.counter;
        }
        if addr == CIA_TOD_TEN {
            self.tod.latched = false;
        }
        if addr == CIA_TOD_MIN {
            self.tod.latched = true;
        }
        ((self.tod.latch >> (8 * (addr - CIA_TOD_TEN))) & 0xff) as u8
    }

    /// One positive edge on the TOD pin. The stock board has none (module doc); kept so
    /// the counter's behaviour is stated once, where the registers are.
    pub fn tod_pin_edge(&mut self) {
        if !self.tod.stopped {
            self.tod.counter = (self.tod.counter + 1) & 0x00ff_ffff;
            let clk = self.clk;
            self.check_tod_alarm(clk);
        }
    }

    /// Side-effect-free read of a register, for the monitor and hosts. Unlike VICE's
    /// `ciacore_peek` (which reads through the ports and timers), this touches nothing:
    /// the timers answer as of their last catch-up and the ports from their latches.
    pub fn peek(&self, addr: u16) -> u8 {
        let addr = (addr & 0xf) as usize;
        match addr {
            CIA_TAL => (self.ta.cnt & 0xff) as u8,
            CIA_TAH => (self.ta.cnt >> 8) as u8,
            CIA_TBL => (self.tb.cnt & 0xff) as u8,
            CIA_TBH => (self.tb.cnt >> 8) as u8,
            CIA_TOD_TEN | CIA_TOD_SEC | CIA_TOD_MIN => {
                ((self.tod.counter >> (8 * (addr - CIA_TOD_TEN))) & 0xff) as u8
            }
            CIA_TOD_HR => 0,
            CIA_ICR => self.irqflags as u8,
            CIA_CRA => (self.c_cia[CIA_CRA] & !CIA_CR_START) | self.ta.is_running() as u8,
            CIA_CRB => (self.c_cia[CIA_CRB] & !CIA_CR_START) | self.tb.is_running() as u8,
            _ => self.c_cia[addr],
        }
    }

    /// The composed port outputs `PRx | ~DDRx`, as the pins drive them.
    pub fn pa_out(&self) -> u8 {
        self.c_cia[CIA_PRA] | !self.c_cia[CIA_DDRA]
    }
    pub fn pb_out(&self) -> u8 {
        self.old_pb
    }

    // ── ciacore.c:1349-1418 ciacore_intta ─────────────────────────────────────
    fn ciacore_intta<B: CiaBackend + ?Sized>(&mut self, b: &mut B, rclk: u64) {
        self.cia_do_update_ta(rclk);
        self.ciat_ack_alarm_ta();

        if self.c_cia[CIA_CRA] & (CIA_CRA_INMODE | CIA_CR_RUNMODE | CIA_CR_START)
            == (CIA_CRA_INMODE_PHI2 | CIA_CR_RUNMODE_CONTINUOUS | CIA_CR_START)
            && ((self.c_cia[CIA_ICR] as u32 & CIA_IM_TA != 0 && self.irqflags & CIA_IM_SET == 0)
                || self.c_cia[CIA_CRA] & (CIA_CRA_SPMODE | CIA_CRA_INMODE) != 0
                || self.c_cia[CIA_CRB] & CIA_CRB_INMODE_TA != 0)
        {
            self.ciat_set_alarm_ta();
        }

        if self.c_cia[CIA_CRA] & CIA_CRA_SPMODE_OUT != 0 && (self.sr_bits != 0 || self.sdr_valid) {
            let mut event = CIA_SDR_TOGGLE_CNT1;
            if self.sdr_delay & (CIA_SDR_TOGGLE_CNT0 | CIA_SDR_NOGGLE_CNT0) != 0 {
                event = CIA_SDR_NOGGLE_CNT1;
            }
            self.schedule_sdr_alarm(rclk, event);
        }

        if self.c_cia[CIA_CRB] & (CIA_CRB_INMODE_TA | CIA_CR_START) == (CIA_CRB_INMODE_TA | CIA_CR_START) {
            self.cia_update_tb(b, rclk);
            self.cia_do_step_tb(rclk);
        }
    }

    fn ciacore_intta_entry<B: CiaBackend + ?Sized>(&mut self, b: &mut B, rclk: u64) {
        self.ciacore_intta(b, rclk);
        self.cia_ifr_catchup(rclk);
        self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
    }

    // ── ciacore.c:1440-1476 ciacore_inttb ─────────────────────────────────────
    fn ciacore_inttb(&mut self, rclk: u64) {
        self.cia_do_update_tb(rclk);
        self.ciat_ack_alarm_tb();
        if self.c_cia[CIA_CRB] & (CIA_CRB_INMODE | CIA_CR_RUNMODE | CIA_CR_START)
            == (CIA_CRB_INMODE_PHI2 | CIA_CR_RUNMODE_CONTINUOUS | CIA_CR_START)
            && self.c_cia[CIA_ICR] as u32 & CIA_IM_TB != 0
        {
            self.ciat_set_alarm_tb();
        }
    }

    fn ciacore_inttb_entry(&mut self, rclk: u64) {
        self.ciacore_inttb(rclk);
        self.cia_ifr_catchup(rclk);
        self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
    }

    // ── ciacore.c:1500-1528 ciacore_async_interrupt / ciacore_set_flag ────────
    fn ciacore_async_interrupt(&mut self, flag: u32) {
        let rclk = self.clk;
        self.cia_set_irq_flag(rclk, flag);
        let idleclk = self.alarms.clk_of(CiaAlarm::Idle);
        if idleclk > rclk + 1 {
            self.alarms.set(CiaAlarm::Idle, rclk + 1);
        }
    }

    /// ciacore.c:1521 `ciacore_set_flag` — a falling edge on the FLAG pin (the 1581's
    /// ATN, iecbus.c:250-252). `self.clk` must be the drive clock of the edge.
    pub fn set_flag(&mut self) {
        self.ciacore_async_interrupt(CIA_IM_FLG);
    }

    /// ciacore.c:1531-1550 `ciacore_set_sdr` — a whole byte arrives in the shift
    /// register at once.
    pub fn set_sdr(&mut self, data: u8) {
        if self.c_cia[CIA_CRA] & CIA_CRA_SPMODE == CIA_CRA_SPMODE_IN {
            self.c_cia[CIA_SDR] = data;
            self.ciacore_async_interrupt(CIA_IM_SDR);
            self.alarms.unset(CiaAlarm::Sdr);
        }
    }

    /// ciacore.c:1552-1602 `ciacore_set_cnt` — the CNT input.
    pub fn set_cnt(&mut self, data: bool) {
        if data != self.cnt_in_state {
            if self.c_cia[CIA_CRA] & CIA_CRA_SPMODE == CIA_CRA_SPMODE_IN {
                if !data && self.sr_bits == 0 {
                    self.sr_bits = 16;
                }
                self.sr_bits = self.sr_bits.wrapping_sub(1);
                if data {
                    self.shifter <<= 1;
                    self.shifter |= self.sp_in_state as u16;
                    if self.sr_bits == 0 {
                        let v = (self.shifter & 0xff) as u8;
                        self.set_sdr(v);
                    }
                }
            }
            self.cnt_in_state = data;
        }
    }

    /// ciacore.c:1604-1607 `ciacore_set_sp` — the SP input.
    pub fn set_sp(&mut self, data: bool) {
        self.sp_in_state = data;
    }

    // ── ciacore.c:1615-1620 schedule_sdr_alarm ────────────────────────────────
    fn schedule_sdr_alarm(&mut self, rclk: u64, feed: u32) {
        self.sdr_delay |= feed;
        self.alarms.set(CiaAlarm::Sdr, rclk);
    }

    // ── ciacore.c:1631-1738 ciacore_intsdr ────────────────────────────────────
    fn ciacore_intsdr<B: CiaBackend + ?Sized>(&mut self, b: &mut B, rclk: u64) {
        let mut feed = 0u32;

        if self.alarms.clk_of(CiaAlarm::Ta) == rclk {
            self.ciacore_intta(b, rclk);
        }

        if self.sdr_delay & CIA_SDR_SET0 != 0 {
            if self.sr_bits == 0 {
                self.sr_bits = 16;
                self.shifter = (self.c_cia[CIA_SDR] as u16) << 1;
            } else if self.sr_bits == 1 {
                self.shifter |= self.c_cia[CIA_SDR] as u16;
                self.sr_bits = 17;
            } else {
                self.sdr_valid = true;
            }
        }

        if self.sdr_delay & CIA_SDR_TOGGLE_CNT0 != 0 {
            let dec = if self.sr_bits != 0 {
                self.sr_bits -= 1;
                self.sr_bits & 1 != 0
            } else {
                false
            };
            if dec {
                let bit = (self.shifter >> 8) & 1 != 0;
                b.set_sp(rclk, bit);
                self.cnt_out_state = false;
                b.set_cnt(rclk, false);

                if self.sr_bits == 1 {
                    b.store_sdr(((self.shifter >> 8) & 0xff) as u8);
                    feed |= CIA_SDR_SET_SDR_IRQ2;
                    if self.sdr_valid {
                        self.shifter |= self.c_cia[CIA_SDR] as u16;
                        self.sdr_valid = false;
                        self.sr_bits = 17;
                    }
                }
            } else {
                self.shifter <<= 1;
                self.cnt_out_state = true;
                b.set_cnt(rclk, true);
            }
        }

        if self.sdr_delay & CIA_SDR_SET_SDR_IRQ0 != 0 {
            self.cia_set_irq_flag(rclk, CIA_IM_SDR);
        }

        self.sdr_delay |= feed;
        self.sdr_delay <<= 1;
        self.sdr_delay &= !CIA_SDR_CLEAR;

        if self.cnt_out_state {
            self.sdr_delay |= CIA_SDR_CNT0;
        }

        let mut active = self.sdr_delay & CIA_SDR_ACTIVE != 0;
        if !active {
            let all_cnt = self.sdr_delay & ALL_SDR_CNT;
            if all_cnt != 0 && all_cnt != ALL_SDR_CNT {
                active = true;
            }
        }
        if active {
            self.alarms.set(CiaAlarm::Sdr, rclk + 1);
        } else {
            self.alarms.unset(CiaAlarm::Sdr);
        }
    }

    fn ciacore_intsdr_entry<B: CiaBackend + ?Sized>(&mut self, b: &mut B, rclk: u64) {
        self.ciacore_intsdr(b, rclk);
        self.cia_ifr_catchup(rclk);
        if self.ifr_clock == rclk {
            self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
        }
    }

    // ── ciacore.c:1921-1945 ciacore_idle ──────────────────────────────────────
    fn ciacore_idle<B: CiaBackend + ?Sized>(&mut self, b: &mut B, rclk: u64) {
        self.cia_update_ta(b, rclk);
        self.cia_update_tb(b, rclk);
        self.alarms.set(CiaAlarm::Idle, rclk + CIA_MAX_IDLE_CYCLES);
        self.cia_ifr_catchup(rclk);
        if self.ifr_clock == rclk {
            self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
        }
    }

    /// Bring both timers and the IFR delay line up to `self.clk`, as
    /// `ciacore_snapshot_write_module` does before it reads them.
    fn settle_for_snapshot<B: CiaBackend + ?Sized>(&mut self, b: &mut B) -> u64 {
        let rclk = self.clk;
        self.cia_update_ta(b, rclk);
        self.cia_update_tb(b, rclk);
        self.cia_ifr_catchup(rclk);
        self.cia_ifr_current(rclk, CIA_IFR_CURRENT);
        rclk
    }

    // ── ciacore.c:2077-2211 ciacore_snapshot_write_module ─────────────────────
    /// Write the `CIA1581D<n>` module (2.5). The 8520 TOD rides in the TOD fields: the
    /// counter bytes in TOD_TEN/SEC/MIN, the alarm in ALARM_TEN/SEC/MIN, the read latch
    /// in TODL_*; `$B` fields are 0 and TOD_TICKS is 0 (no tick is scheduled).
    pub fn snapshot_write_module<B: CiaBackend + ?Sized>(&mut self, b: &mut B, s: &mut SnapshotT) {
        let rclk = self.settle_for_snapshot(b);
        let mut m = s.module_create(&self.myname.clone(), CIA_DUMP_VER_MAJOR, CIA_DUMP_VER_MINOR);
        let w = |s: &mut SnapshotT, m: &mut SnapshotModule, v: u8| s.smw_b(m, v);
        let byte_of = |v: u32, i: u32| ((v >> (8 * i)) & 0xff) as u8;

        w(s, &mut m, self.c_cia[CIA_PRA]);
        w(s, &mut m, self.c_cia[CIA_PRB]);
        w(s, &mut m, self.c_cia[CIA_DDRA]);
        w(s, &mut m, self.c_cia[CIA_DDRB]);
        s.smw_w(&mut m, self.ta.read_timer());
        s.smw_w(&mut m, self.tb.read_timer());
        w(s, &mut m, byte_of(self.tod.counter, 0));
        w(s, &mut m, byte_of(self.tod.counter, 1));
        w(s, &mut m, byte_of(self.tod.counter, 2));
        w(s, &mut m, 0);
        w(s, &mut m, self.c_cia[CIA_SDR]);
        w(s, &mut m, self.c_cia[CIA_ICR]);
        w(s, &mut m, self.c_cia[CIA_CRA]);
        w(s, &mut m, self.c_cia[CIA_CRB]);
        s.smw_w(&mut m, self.ta.latch);
        s.smw_w(&mut m, self.tb.latch);
        // ciacore_peek(CIA_ICR) = irqflags.
        w(s, &mut m, self.irqflags as u8);
        w(
            s,
            &mut m,
            (if self.tat != 0 { 0x40 } else { 0 })
                | (if self.tbt != 0 { 0x80 } else { 0 })
                | (if self.ta.is_underflow_clk() { 0x04 } else { 0 })
                | (if self.tb.is_underflow_clk() { 0x08 } else { 0 }),
        );
        w(s, &mut m, self.sr_bits as u8);
        w(s, &mut m, byte_of(self.tod.alarm, 0));
        w(s, &mut m, byte_of(self.tod.alarm, 1));
        w(s, &mut m, byte_of(self.tod.alarm, 2));
        w(s, &mut m, 0);
        let readicr = if self.rdi != 0 {
            if rclk.wrapping_sub(self.rdi) > 120 {
                0
            } else {
                (rclk.wrapping_add(128).wrapping_sub(self.rdi) & 0xff) as u8
            }
        } else {
            0
        };
        w(s, &mut m, readicr);
        w(s, &mut m, (self.tod.latched as u8) | if self.tod.stopped { 2 } else { 0 });
        w(s, &mut m, byte_of(self.tod.latch, 0));
        w(s, &mut m, byte_of(self.tod.latch, 1));
        w(s, &mut m, byte_of(self.tod.latch, 2));
        w(s, &mut m, 0);
        s.smw_clock(&mut m, 0); // TOD_TICKS — no tick scheduled
        // ciat_save_snapshot (ver >= 0x100): the state word.
        s.smw_w(&mut m, self.ta.state);
        s.smw_w(&mut m, self.tb.state);
        w(s, &mut m, (self.shifter & 0xff) as u8);
        w(s, &mut m, self.sdr_valid as u8);
        w(s, &mut m, self.irq_enabled as u8);
        w(s, &mut m, 0); // todtickcounter — no mains divider on an 8520
        w(s, &mut m, (self.shifter >> 8) as u8);
        let sdr_pending = self.alarms.clk_of(CiaAlarm::Sdr);
        let sdr_alarm = if sdr_pending > 0 {
            (1u64.wrapping_add(sdr_pending).wrapping_sub(rclk) & 0xff) as u8
        } else {
            0
        };
        w(s, &mut m, sdr_alarm);
        w(
            s,
            &mut m,
            (if self.sp_in_state { 0x80 } else { 0 })
                | (if self.cnt_in_state { 0x40 } else { 0 })
                | (if self.sdr_force_finish { 0x20 } else { 0 }),
        );
        s.smw_dw(&mut m, self.sdr_delay);
        w(s, &mut m, if self.cnt_out_state { 0x40 } else { 0 });
        s.smw_dw(&mut m, self.ifr_delay);
        w(s, &mut m, self.ack_irqflags as u8);
        w(s, &mut m, self.new_irqflags as u8);
        s.module_close(&m);
    }

    // ── ciacore.c:2213-2415 ciacore_snapshot_read_module ──────────────────────
    /// Read the module back. Returns whether the IRQ line was restored active (VICE
    /// `cia_restore_int`), for the board to hand the drive CPU.
    pub fn snapshot_read_module<B: CiaBackend + ?Sized>(
        &mut self,
        b: &mut B,
        s: &mut SnapshotT,
    ) -> Result<bool, String> {
        let name = self.myname.clone();
        let (m, vmajor, vminor) = s.module_open(&name).ok_or_else(|| format!("{name}: module missing"))?;
        if vmajor != CIA_DUMP_VER_MAJOR {
            return Err(format!("{name}: module version {vmajor}.{vminor}, this reader knows {CIA_DUMP_VER_MAJOR}.x"));
        }
        macro_rules! rb {
            () => {
                s.smr_b().ok_or_else(|| format!("{name}: truncated"))?
            };
        }
        macro_rules! rw {
            () => {
                s.smr_w().ok_or_else(|| format!("{name}: truncated"))?
            };
        }
        macro_rules! rdw {
            () => {
                s.smr_dw().ok_or_else(|| format!("{name}: truncated"))?
            };
        }
        let rclk = self.clk;

        self.reset(b);
        // Discard the reset's own line event: the restored level comes from the module.
        self.irq_events.clear();
        // stop timers, just in case
        self.ta.set_ctrl(0);
        self.ciat_set_alarm_ta();
        self.tb.set_ctrl(0);
        self.ciat_set_alarm_tb();
        self.alarms.unset(CiaAlarm::Sdr);

        self.c_cia[CIA_PRA] = rb!();
        self.c_cia[CIA_PRB] = rb!();
        self.c_cia[CIA_DDRA] = rb!();
        self.c_cia[CIA_DDRB] = rb!();
        let pa = self.c_cia[CIA_PRA] | !self.c_cia[CIA_DDRA];
        self.old_pa = pa ^ 0xff;
        b.undump_pa(rclk, pa);
        self.old_pa = pa;
        let pb = self.c_cia[CIA_PRB] | !self.c_cia[CIA_DDRB];
        self.old_pb = pb ^ 0xff;
        b.undump_pb(rclk, pb);
        self.old_pb = pb;

        let tac = rw!();
        let tbc = rw!();
        let t0 = rb!() as u32;
        let t1 = rb!() as u32;
        let t2 = rb!() as u32;
        let _t3 = rb!();
        self.tod.counter = t0 | (t1 << 8) | (t2 << 16);
        self.c_cia[CIA_SDR] = rb!();
        self.c_cia[CIA_ICR] = rb!();
        self.c_cia[CIA_CRA] = rb!();
        self.c_cia[CIA_CRB] = rb!();
        let tal = rw!();
        let tbl = rw!();
        self.irqflags = rb!() as u32;
        let pbstate = rb!();
        self.tat = (pbstate & 0x40 != 0) as u32;
        self.tbt = (pbstate & 0x80 != 0) as u32;
        self.sr_bits = rb!() as u32;
        let a0 = rb!() as u32;
        let a1 = rb!() as u32;
        let a2 = rb!() as u32;
        let _a3 = rb!();
        self.tod.alarm = a0 | (a1 << 8) | (a2 << 16);
        let readicr = rb!();
        self.rdi = if readicr != 0 { self.clk.wrapping_add(128).wrapping_sub(readicr as u64) } else { 0 };
        let todl = rb!();
        self.tod.latched = todl & 1 != 0;
        self.tod.stopped = todl & 2 != 0;
        let l0 = rb!() as u32;
        let l1 = rb!() as u32;
        let l2 = rb!() as u32;
        let _l3 = rb!();
        self.tod.latch = l0 | (l1 << 8) | (l2 << 16);
        let _tod_ticks = s.smr_clock().ok_or_else(|| format!("{name}: truncated"))?;

        // ciat_load_snapshot for both timers.
        let ver = ((vmajor as u32) << 8) | vminor as u32;
        for (is_a, cnt, latch, cr) in [(true, tac, tal, self.c_cia[CIA_CRA]), (false, tbc, tbl, self.c_cia[CIA_CRB])] {
            let t = if is_a { &mut self.ta } else { &mut self.tb };
            t.clk = rclk;
            t.cnt = cnt;
            t.latch = latch;
            if ver >= 0x101 {
                t.state = rw!();
            } else {
                t.state = cr as u16;
                if cr & CIA_CR_START != 0 {
                    t.state |= 0x002 | 0x040 | 0x800;
                }
                if cr & 0x08 != 0 {
                    t.state |= 0x100 | 0x1000;
                }
            }
            if is_a {
                self.ciat_set_alarm_ta();
            } else {
                self.ciat_set_alarm_tb();
            }
        }

        let mut restored_irq = false;
        if vminor > 1 {
            self.shifter = rb!() as u16;
            self.sdr_valid = rb!() != 0;
            self.irq_enabled = rb!() != 0;
            restored_irq = self.irq_enabled;
            let _todtickcounter = rb!();
        }
        if vminor > 2 {
            let hi = rb!();
            self.shifter |= (hi as u16) << 8;
            let sdr_alarm = rb!();
            if sdr_alarm != 0 {
                self.alarms.set(CiaAlarm::Sdr, rclk + sdr_alarm as u64 - 1);
            }
            let spcnt = rb!();
            self.sp_in_state = spcnt & 0x80 != 0;
            self.cnt_in_state = spcnt & 0x40 != 0;
            self.sdr_force_finish = spcnt & 0x20 != 0;
        }
        if vminor > 3 {
            self.sdr_delay = rdw!();
            let out = rb!();
            self.cnt_out_state = out & 0x40 != 0;
        }
        if vminor > 4 {
            self.ifr_delay = rdw!();
            self.ifr_clock = rclk + 1;
            self.ack_irqflags = rb!() as u32;
            self.new_irqflags = rb!() as u32;
            self.cia_ifr_current(rclk, CIA_IFR_NEXT);
        }
        s.module_close(&m);
        Ok(restored_irq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoPorts {
        stored_pb: Vec<u8>,
    }
    impl CiaBackend for NoPorts {
        fn read_pa(&mut self, c: &[u8; 16]) -> u8 {
            c[CIA_PRA] | !c[CIA_DDRA]
        }
        fn read_pb(&mut self, c: &[u8; 16]) -> u8 {
            c[CIA_PRB] | !c[CIA_DDRB]
        }
        fn store_pb(&mut self, _clk: u64, byte: u8, _old: u8) {
            self.stored_pb.push(byte);
        }
    }

    fn fresh() -> (CiaCore, NoPorts) {
        let mut c = CiaCore::new("CIA1581D0");
        let mut b = NoPorts { stored_pb: vec![] };
        c.clk = 0;
        c.reset(&mut b);
        c.irq_events.clear();
        (c, b)
    }

    /// The FLAG pin: a falling ATN edge latches CIA_IM_FLG, and with FLG enabled in the
    /// mask the IRQ line goes active one cycle later (the old CIA's delay line).
    #[test]
    fn flag_raises_the_irq_through_the_delay_line() {
        let (mut c, mut b) = fresh();
        c.clk = 10;
        c.store(&mut b, 0x0d, 0x80 | CIA_IM_FLG as u8);
        c.clk = 100;
        c.set_flag();
        assert!(c.irqflags & CIA_IM_FLG != 0, "FLG latched at the edge");
        c.process_alarms(&mut b, 120);
        assert!(c.irq_events.iter().any(|&(v, _)| v), "the line went active: {:?}", c.irq_events);
        let (_, at) = *c.irq_events.iter().rev().find(|e| e.0).unwrap();
        assert!((101..=103).contains(&at), "asserted just after the edge, at {at}");
        // Reading the ICR returns FLG with the summary bit and releases the line.
        c.clk = 130;
        let icr = c.read(&mut b, 0x0d);
        assert_eq!(icr as u32 & (CIA_IM_FLG | CIA_IM_SET), CIA_IM_FLG | CIA_IM_SET, "ICR {icr:02x}");
        assert_eq!(c.irq_events.last(), Some(&(false, 130)));
    }

    /// Timer A one-shot underflow sets CIA_IM_TA at the predicted clock.
    #[test]
    fn timer_a_underflow_is_an_alarm() {
        let (mut c, mut b) = fresh();
        c.clk = 1;
        c.store(&mut b, 0x0d, 0x81);
        c.clk = 2;
        c.store(&mut b, 0x04, 0x20);
        c.clk = 3;
        c.store(&mut b, 0x05, 0x00);
        c.clk = 4;
        c.store(&mut b, 0x0e, 0x19); // force load, one-shot, start
        let due = c.next_alarm_clk();
        assert!(due > 4 && due < 60, "TA alarm scheduled at {due}");
        c.process_alarms(&mut b, 80);
        assert!(c.irqflags & CIA_IM_TA != 0);
        assert!(c.irq_events.iter().any(|&(v, _)| v), "IRQ raised");
    }

    /// The 8520 TOD: a write of $8 reads back from $8, nothing ticks, $B reads 0.
    #[test]
    fn the_8520_tod_is_a_counter_that_does_not_move_without_edges() {
        let (mut c, mut b) = fresh();
        c.clk = 5;
        c.store(&mut b, 0x0a, 0x12);
        c.store(&mut b, 0x09, 0x34);
        c.store(&mut b, 0x08, 0x00);
        assert!(!c.tod.stopped, "writing the LSB starts it");
        c.clk = 1_000_000;
        c.process_alarms(&mut b, 1_000_000);
        assert_eq!(c.read(&mut b, 0x0a), 0x12, "MSB (latches)");
        assert_eq!(c.read(&mut b, 0x09), 0x34);
        assert_eq!(c.read(&mut b, 0x08), 0x00, "LSB (releases)");
        assert_eq!(c.read(&mut b, 0x0b), 0x00, "$B is not connected");
        c.tod_pin_edge();
        assert_eq!(c.read(&mut b, 0x08), 0x01, "an edge counts");
    }

    /// Serial out: a byte written to SDR with CRA in output mode shifts out on Timer A
    /// underflows and is handed to `store_sdr`, then raises CIA_IM_SDR.
    #[test]
    fn the_shift_register_shifts_a_byte_out() {
        struct Sink(Vec<u8>, u32);
        impl CiaBackend for Sink {
            fn read_pa(&mut self, _: &[u8; 16]) -> u8 {
                0xff
            }
            fn read_pb(&mut self, _: &[u8; 16]) -> u8 {
                0xff
            }
            fn store_sdr(&mut self, b: u8) {
                self.0.push(b);
            }
            fn set_cnt(&mut self, _: u64, bit: bool) {
                if !bit {
                    self.1 += 1;
                }
            }
        }
        let mut c = CiaCore::new("CIA1581D0");
        let mut b = Sink(vec![], 0);
        c.reset(&mut b);
        c.clk = 1;
        c.store(&mut b, 0x04, 0x03);
        c.clk = 2;
        c.store(&mut b, 0x05, 0x00);
        c.clk = 3;
        c.store(&mut b, 0x0e, 0x51); // SP out, force load, continuous, start
        c.clk = 4;
        c.store(&mut b, 0x0c, 0xa5);
        for clk in 5..400 {
            c.clk = clk;
            c.process_alarms(&mut b, clk);
        }
        assert_eq!(b.0, vec![0xa5], "the byte went out once");
        assert_eq!(b.1, 8, "eight CNT pulses");
        assert!(c.irqflags & CIA_IM_SDR != 0, "SDR flag");
    }
}
