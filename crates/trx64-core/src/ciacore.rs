//! ciacore.rs — the one CIA core: a 1:1 port of VICE `core/ciacore.c` + `core/ciatimer.h`.
//!
//! Every CIA in the machine runs on this: the C64's CIA 1 and CIA 2 (with the glue of
//! `c64/c64cia1.c` / `c64/c64cia2.c`, [`crate::c64cia`]) and the 1581's CIA
//! (`drive/iec/cia1581d.c`, [`crate::drive1581`]). The board's side of a chip — what
//! its ports, SP/CNT and interrupt pin are wired to — is the [`CiaBackend`] the caller
//! hands every entry point, the function pointers of VICE's `cia_context_t`.
//!
//! **Model.** `CIA_MODEL_6526` (the "old" CIA, VICE's `model = 0`) and
//! `CIA_MODEL_6526A` (the "new" one) differ in the IFR delay line: the old part raises
//! its interrupt one cycle later and acknowledges differently (`cia_run_ifr_cycle`,
//! the ICR read and the ICR write below). The board picks it ([`CiaCore::set_model`]).
//!
//! **TOD.** Two forms, chosen at construction. [`TodKind::Bcd`] is ciacore's BCD
//! clock, ticked by the mains through its own alarm (`ciacore_inttod`). VICE builds
//! that alarm with `#define TODRANDOM`, which jitters each mains period by
//! `lib_unsigned_rand(0, 3)`; this port takes the `#else` branch of the same function
//! (`todticks++` / `todticks--`), which corrects the same drift deterministically — a
//! run must replay identically. [`TodKind::Event8520`] is the 1581's 8520 (Spec 872
//! D1b): a 24-bit binary counter clocked by edges on the TOD pin, registers `$8/$9/$A`
//! = bits 0-7 / 8-15 / 16-23, `$B` not connected, a write stops the counter until the
//! LSB, reading the MSB latches all three until the LSB, CRB7 routes writes to the
//! alarm. The stock 1581 holds that pin high (R5), so no TOD alarm is scheduled and the
//! counter moves only when software writes it.
//!
//! **Clocks.** Every entry point runs at `self.clk` (VICE `*clk_ptr`), which the board
//! sets to its live clock before each call. Alarm callbacks receive `rclk` = the
//! alarm's own clock (VICE passes `offset = clk - alarm_clk` and every callback
//! reconstructs exactly that). `write_offset` is VICE's: 1 by default
//! (`ciacore_setup_context`), 0 on x64sc (`c64cia1.c` / `c64cia2.c` setup).
//!
//! **The interrupt line.** `my_set_int` records each `(level, rclk)` in `irq_events`,
//! in order; the board replays them into its CPU's interrupt status
//! (`interrupt_set_irq` / `interrupt_set_nmi` with that `rclk`) before the CPU next
//! consults it. That is the sequence VICE's `cia_set_int_clk` produces, including an
//! assert and a release inside one instruction.
//!
//! **Alarms.** VICE puts a CIA's five alarms (idle, Timer A, Timer B, TOD, SDR) on the
//! CPU's alarm context. Here each chip keeps its own context with the same pending-array
//! semantics; nothing a CIA alarm does reaches another device, so the order between two
//! chips' alarms at one clock is not observable.
//!
//! **RMW.** `ciacore_store` re-stores `last_read` one cycle early when the CPU core
//! flags a read-modify-write. Neither CPU core here sets that flag (x64sc's
//! `6510dtvcore.c` never uses `RMW_FLAG`; it performs the dummy store itself as a bus
//! write, as does the drive core), so the store path has no RMW arm.
//!
//! **pre_read / pre_store.** The C64 glue hooks them to `vicii_handle_pending_alarms_*`;
//! TRX64's VIC is ticked every cycle and has no pending alarms, so there is nothing to
//! call.

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

pub const CIA_CR_START: u8 = 0x01;
pub const CIA_CR_PBON: u8 = 0x02;
pub const CIA_CR_OUTMODE_TOGGLE: u8 = 0x04;
pub const CIA_CR_RUNMODE: u8 = 0x08;
pub const CIA_CR_RUNMODE_ONE_SHOT: u8 = 0x08;
const CIA_CR_RUNMODE_CONTINUOUS: u8 = 0x00;
pub const CIA_CR_FORCE_LOAD: u8 = 0x10;
const CIA_CRA_INMODE: u8 = 0x20;
const CIA_CRA_INMODE_PHI2: u8 = 0x00;
pub const CIA_CRA_SPMODE: u8 = 0x40;
pub const CIA_CRA_SPMODE_OUT: u8 = 0x40;
const CIA_CRA_SPMODE_IN: u8 = 0x00;
pub const CIA_CRA_TODIN_50HZ: u8 = 0x80;
const CIA_CRB_INMODE: u8 = 0x60;
const CIA_CRB_INMODE_PHI2: u8 = 0x00;
pub const CIA_CRB_INMODE_TA: u8 = 0x40;
pub const CIA_CRB_ALARM: u8 = 0x80;
const CIA_CRB_ALARM_ALARM: u8 = 0x80;
const CIA_CRB_ALARM_TOD: u8 = 0x00;

pub const CIA_IM_SET: u32 = 0x80;
pub const CIA_IM_TA: u32 = 1;
pub const CIA_IM_TB: u32 = 2;
pub const CIA_IM_TOD: u32 = 4;
pub const CIA_IM_SDR: u32 = 8;
pub const CIA_IM_FLG: u32 = 16;
/// ciacore.c — the Timer B bug marker in `irqflags` (never visible in the ICR byte).
const CIA_IM_TBB: u32 = 0x100;

/// cia.h `CIA_MODEL_6526` — the "old" CIA.
pub const CIA_MODEL_6526: u32 = 0;
/// cia.h `CIA_MODEL_6526A` — the "new" CIA.
pub const CIA_MODEL_6526A: u32 = 1;

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

/// ciacore.c:624 CIA_MAX_IDLE_CYCLES.
const CIA_MAX_IDLE_CYCLES: u64 = 5000;

/// ciacore.c:2140-2141 CIA_DUMP_VER_MAJOR / _MINOR.
pub const CIA_DUMP_VER_MAJOR: u8 = 2;
pub const CIA_DUMP_VER_MINOR: u8 = 5;

/// VICE `CLOCK_MAX` — the "never fires" alarm clock.
pub const CLOCK_NEVER: u64 = u64::MAX;

// =============================================================================
// ciatimer.h / ciatimer.c — one timer
// =============================================================================

/// Size of the timer transition table (`ciat_table`).
pub const CIAT_TABLEN: usize = 2 << 13;

const CIAT_CR_MASK: u16 = 0x039;
const CIAT_CR_START: u16 = 0x001;
const CIAT_CR_ONESHOT: u16 = 0x008;
const CIAT_CR_FLOAD: u16 = 0x010;
const CIAT_PHI2IN: u16 = 0x020;
const CIAT_STEP: u16 = 0x004;
const CIAT_COUNT2: u16 = 0x002;
const CIAT_COUNT3: u16 = 0x040;
const CIAT_COUNT: u16 = 0x800;
const CIAT_LOAD1: u16 = 0x080;
const CIAT_ONESHOT0: u16 = 0x100;
const CIAT_ONESHOT: u16 = 0x1000;
const CIAT_LOAD: u16 = 0x200;
const CIAT_OUT: u16 = 0x400;

/// ciatimer.c `ciat_init_table` — a pure function of the index, built once.
pub(crate) fn shared_table() -> &'static [u16; CIAT_TABLEN] {
    static TABLE: std::sync::OnceLock<Box<[u16; CIAT_TABLEN]>> = std::sync::OnceLock::new();
    TABLE.get_or_init(build_table)
}

fn build_table() -> Box<[u16; CIAT_TABLEN]> {
    let mut t = Box::new([0u16; CIAT_TABLEN]);
    for (i, slot) in t.iter_mut().enumerate() {
        let i = i as u16;
        let mut tmp = i & (CIAT_CR_START | CIAT_CR_ONESHOT | CIAT_PHI2IN);
        if (i & CIAT_CR_START) != 0 && (i & CIAT_PHI2IN) != 0 {
            tmp |= CIAT_COUNT2;
        }
        if (i & CIAT_COUNT2) != 0 || ((i & CIAT_STEP) != 0 && (i & CIAT_CR_START) != 0) {
            tmp |= CIAT_COUNT3;
        }
        if (i & CIAT_COUNT3) != 0 {
            tmp |= CIAT_COUNT;
        }
        if (i & CIAT_CR_FLOAD) != 0 {
            tmp |= CIAT_LOAD1;
        }
        if (i & CIAT_LOAD1) != 0 {
            tmp |= CIAT_LOAD;
        }
        if (i & CIAT_CR_ONESHOT) != 0 {
            tmp |= CIAT_ONESHOT0;
        }
        if (i & CIAT_ONESHOT0) != 0 {
            tmp |= CIAT_ONESHOT;
        }
        *slot = tmp;
    }
    t
}

#[inline]
fn tab() -> &'static [u16; CIAT_TABLEN] {
    shared_table()
}

/// VICE `ciat_t`, without the alarm pointer (the chip's alarm context holds it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ciat {
    pub state: u16,
    pub latch: u16,
    pub cnt: u16,
    /// The timer's own clock, advanced lazily by `update`.
    pub clk: u64,
}

impl Default for Ciat {
    fn default() -> Self {
        Self { state: 0, latch: 0xffff, cnt: 0xffff, clk: 0 }
    }
}

impl Ciat {
    /// ciatimer.c `ciat_reset`.
    pub fn reset(&mut self, cclk: u64) {
        self.clk = cclk;
        self.cnt = 0xffff;
        self.latch = 0xffff;
        self.state = 0;
    }

    #[inline]
    fn warp_counting(t: u16) -> bool {
        (t & (CIAT_CR_START
            | CIAT_CR_FLOAD
            | CIAT_LOAD1
            | CIAT_PHI2IN
            | CIAT_COUNT2
            | CIAT_COUNT3
            | CIAT_COUNT
            | CIAT_LOAD))
            == (CIAT_CR_START | CIAT_PHI2IN | CIAT_COUNT2 | CIAT_COUNT3 | CIAT_COUNT)
            && Self::oneshot_settled(t)
    }

    #[inline]
    fn oneshot_settled(t: u16) -> bool {
        ((t & CIAT_CR_ONESHOT) != 0 && (t & CIAT_ONESHOT0) != 0 && (t & CIAT_ONESHOT) != 0)
            || ((t & CIAT_CR_ONESHOT) == 0 && (t & CIAT_ONESHOT0) == 0 && (t & CIAT_ONESHOT) == 0)
    }

    /// ciatimer.h `ciat_update` — advance to `cclk`; returns the underflows on the way.
    pub fn update(&mut self, cclk: u64) -> u32 {
        let tab = tab();
        let mut n: u32 = 0;
        let mut t = self.state;
        while self.clk < cclk {
            if Self::warp_counting(t) {
                if self.clk + self.cnt as u64 > cclk {
                    self.cnt = self.cnt.wrapping_sub((cclk - self.clk) as u16);
                    self.clk = cclk;
                } else if (t & (CIAT_CR_ONESHOT | CIAT_ONESHOT0)) != 0 {
                    self.clk += self.cnt as u64;
                    self.cnt = 0;
                } else {
                    self.clk += self.cnt as u64;
                    self.cnt = 0;
                    // ciatimer.h: `((uint16_t)(cclk - state->clk)) >= state->latch + 1`.
                    if ((cclk - self.clk) as u16) as u32 >= self.latch as u32 + 1 {
                        let m = (cclk - self.clk) / (self.latch as u64 + 1);
                        n = n.wrapping_add(m as u32);
                        self.clk += m * (self.latch as u64 + 1);
                    }
                }
            } else if (t & (CIAT_COUNT2 | CIAT_COUNT3 | CIAT_COUNT)) == 0
                && ((t & CIAT_CR_START) == 0 || (t & (CIAT_PHI2IN | CIAT_STEP)) == 0)
                && (t & (CIAT_CR_FLOAD | CIAT_LOAD1 | CIAT_LOAD)) == 0
                && Self::oneshot_settled(t)
            {
                self.clk = cclk;
            } else if t == (CIAT_COUNT | CIAT_OUT | CIAT_LOAD | CIAT_PHI2IN | CIAT_COUNT2 | CIAT_CR_START)
                && self.cnt == 1
                && self.latch == 1
            {
                let m = (cclk - self.clk) & !1;
                if m != 0 {
                    self.clk += m;
                    n = n.wrapping_add((m >> 1) as u32);
                } else {
                    t = tab[t as usize];
                    self.clk += 1;
                }
            } else {
                if self.cnt != 0 && (t & CIAT_COUNT3) != 0 {
                    self.cnt -= 1;
                }
                t = tab[t as usize];
                self.clk += 1;
            }
            if self.cnt == 0 && (t & CIAT_COUNT3) != 0 {
                t |= CIAT_LOAD | CIAT_OUT;
                n = n.wrapping_add(1);
            }
            if (t & CIAT_LOAD) != 0 {
                self.cnt = self.latch;
                t &= !CIAT_COUNT3;
            }
            if (t & CIAT_OUT) != 0 && (t & (CIAT_ONESHOT | CIAT_ONESHOT0)) != 0 {
                t &= !(CIAT_CR_START | CIAT_COUNT2);
            }
        }
        self.state = t;
        n
    }

    /// ciatimer.h `ciat_set_alarm`'s prediction: the clock of the next underflow, or
    /// `CLOCK_NEVER`. Pure — the caller stores it (`alarmclk`) and sets the alarm.
    pub fn predict_alarm(&self) -> u64 {
        let tab = tab();
        let mut aclk = self.clk;
        let mut cnt = self.cnt;
        let mut t = self.state;
        loop {
            if Self::warp_counting(t) {
                return aclk + cnt as u64;
            } else if (t & (CIAT_COUNT2 | CIAT_COUNT3 | CIAT_COUNT)) == 0
                && ((t & CIAT_CR_START) == 0 || (t & (CIAT_PHI2IN | CIAT_STEP)) == 0)
                && Self::oneshot_settled(t)
            {
                return CLOCK_NEVER;
            } else {
                if cnt != 0 && (t & CIAT_COUNT3) != 0 {
                    cnt -= 1;
                }
                t = tab[t as usize];
                aclk += 1;
            }
            if cnt == 0 && (t & CIAT_COUNT3) != 0 {
                return aclk;
            }
            if (t & CIAT_LOAD) != 0 {
                cnt = self.latch;
                t &= !CIAT_COUNT3;
            }
            if (t & CIAT_OUT) != 0 && (t & (CIAT_ONESHOT | CIAT_ONESHOT0)) != 0 {
                t &= !(CIAT_CR_START | CIAT_COUNT2);
            }
        }
    }

    #[inline]
    pub fn read_timer(&self) -> u16 {
        self.cnt
    }
    #[inline]
    pub fn is_underflow_clk(&self) -> bool {
        (self.state & CIAT_OUT) != 0
    }
    #[inline]
    pub fn is_running(&self) -> bool {
        (self.state & CIAT_CR_START) != 0
    }
}

// =============================================================================
// The chip's alarm context (alarm.c / alarm.h, as far as a CIA uses it)
// =============================================================================

/// The CIA's alarms (ciacore_init allocates them in this order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CiaAlarm {
    Idle,
    Ta,
    Tb,
    Tod,
    Sdr,
}

impl CiaAlarm {
    #[inline]
    fn idx(self) -> usize {
        self as usize
    }
}

/// alarm.h `alarm_context_t` over the five CIA alarms: `alarm_context_update_next_pending`
/// scans with `<=`, so of two alarms due at one clock the later array entry runs first.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AlarmCtx {
    pending: [(CiaAlarm, u64); 5],
    num: usize,
    pending_idx: [i32; 5],
    next_clk: u64,
    next_idx: i32,
}

impl AlarmCtx {
    fn new() -> Self {
        Self { pending: [(CiaAlarm::Idle, 0); 5], num: 0, pending_idx: [-1; 5], next_clk: CLOCK_NEVER, next_idx: -1 }
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

/// The alarm clocks a chip has set, in the five `ciacore_init` slots; `None` = unset.
/// What a checkpoint stores to bring the alarm context back exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CiaAlarms {
    pub idle: Option<u64>,
    pub ta: Option<u64>,
    pub tb: Option<u64>,
    pub tod: Option<u64>,
    pub sdr: Option<u64>,
}

// =============================================================================
// The board's side — the function pointers of cia_context_t
// =============================================================================

/// What a port hook sees of the chip: VICE's hooks read `c_cia[]`, `old_pa`, `old_pb`.
#[derive(Clone, Copy, Debug)]
pub struct CiaPins {
    pub c_cia: [u8; 16],
    /// `old_pa` / `old_pb` — the last composed outputs handed to `store_ciapa/pb`, BEFORE
    /// the call in progress updates them.
    pub old_pa: u8,
    pub old_pb: u8,
}

/// The board's hooks (`store_ciapa`, `read_ciapb`, …). Defaults are VICE's "nothing
/// connected": a read returns 0xff, a store goes nowhere.
pub trait CiaBackend {
    /// `store_ciapa(cia, clk, byte)` — the composed port-A output changed.
    fn store_pa(&mut self, _clk: u64, _byte: u8, _pins: &CiaPins) {}
    /// `store_ciapb(cia, clk, byte)` — the composed port-B output changed.
    fn store_pb(&mut self, _clk: u64, _byte: u8, _pins: &CiaPins) {}
    /// `read_ciapa(cia)`.
    fn read_pa(&mut self, _pins: &CiaPins) -> u8 {
        0xff
    }
    /// `read_ciapb(cia)` — before the PB6/PB7 timer outputs are folded in.
    fn read_pb(&mut self, _pins: &CiaPins) -> u8 {
        0xff
    }
    /// `pulse_ciapc(cia, rclk)` — /PC pulses on a port-B access.
    fn pulse_pc(&mut self, _rclk: u64, _pins: &CiaPins) {}
    /// `read_ciaicr(cia)`.
    fn read_icr(&mut self) {}
    /// `read_sdr(cia)` — may replace the SDR byte before it is read.
    fn read_sdr(&mut self, sdr: u8) -> u8 {
        sdr
    }
    /// `store_sdr(cia, byte)` — a byte finished shifting out.
    fn store_sdr(&mut self, _byte: u8) {}
    /// `set_sp(cia, rclk, bit)` — the SP output (optional in VICE).
    fn set_sp(&mut self, _rclk: u64, _bit: bool) {}
    /// `set_cnt(cia, rclk, bit)` — the CNT output (optional in VICE).
    fn set_cnt(&mut self, _rclk: u64, _bit: bool) {}
    /// `undump_ciapa` / `undump_ciapb` — a snapshot restore re-drives the ports.
    fn undump_pa(&mut self, _rclk: u64, _byte: u8, _pins: &CiaPins) {}
    fn undump_pb(&mut self, _rclk: u64, _byte: u8, _pins: &CiaPins) {}
    /// `do_reset_cia`.
    fn do_reset(&mut self) {}
}

/// Nothing on the pins: ports read back their own outputs. For a side-effect-free peek
/// and for the CPU-isolated exerciser bus.
pub struct NoPorts;
impl CiaBackend for NoPorts {
    fn read_pa(&mut self, p: &CiaPins) -> u8 {
        p.c_cia[CIA_PRA] | !p.c_cia[CIA_DDRA]
    }
    fn read_pb(&mut self, p: &CiaPins) -> u8 {
        p.c_cia[CIA_PRB] | !p.c_cia[CIA_DDRB]
    }
}

/// The 8520's TOD (module doc).
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

/// Which TOD the chip carries (module doc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TodKind {
    /// ciacore's BCD clock on the mains.
    Bcd,
    /// The 1581's 8520 event counter.
    Event8520(Tod8520),
}

// =============================================================================
// cia_context_t
// =============================================================================

/// One CIA (VICE `cia_context_t`).
#[derive(Clone)]
pub struct CiaCore {
    pub c_cia: [u8; 16],
    pub irqflags: u32,
    pub ack_irqflags: u32,
    pub new_irqflags: u32,
    pub irq_enabled: bool,
    pub rdi: u64,
    pub ifr_clock: u64,
    pub ifr_delay: u32,
    pub tat: u32,
    pub tbt: u32,
    pub sr_bits: u32,
    pub sdr_force_finish: bool,
    pub sdr_valid: bool,
    pub shifter: u16,
    pub sdr_delay: u32,
    pub old_pa: u8,
    pub old_pb: u8,
    pub ta: Ciat,
    pub tb: Ciat,
    /// `ta->alarmclk` / `tb->alarmclk` (ciatimer.h).
    pub ta_alarmclk: u64,
    pub tb_alarmclk: u64,
    pub last_read: u8,
    pub read_clk: u64,
    pub read_offset: u64,
    pub write_offset: u64,
    pub model: u32,
    pub sp_in_state: bool,
    pub cnt_in_state: bool,
    pub cnt_out_state: bool,
    pub enabled: bool,
    // ── BCD TOD (ciacore.c) ──
    pub todalarm: [u8; 4],
    pub todlatch: [u8; 4],
    pub todlatched: bool,
    pub todstopped: bool,
    pub todclk: u64,
    pub todticks: u64,
    pub todtickcounter: u8,
    pub power_freq: u32,
    pub ticks_per_sec: u32,
    pub power_tickcounter: u32,
    pub power_ticks: u64,
    pub tod_kind: TodKind,
    alarms: AlarmCtx,
    /// VICE `*clk_ptr` — the board's live clock at the current call (module doc).
    pub clk: u64,
    /// `cia_set_int_clk(value, clk)` calls, in order (module doc).
    pub irq_events: Vec<(bool, u64)>,
    /// `myname` — the snapshot module name.
    pub myname: String,
}

impl CiaCore {
    /// `lib_calloc` + `ciacore_setup_context` + `ciacore_init`, with ciacore's BCD TOD. A
    /// context that has not been reset yet.
    pub fn new(myname: &str) -> Self {
        Self::with_tod(myname, TodKind::Bcd)
    }

    /// The same chip with the 8520's event-counter TOD (the 1581).
    pub fn new_8520(myname: &str) -> Self {
        Self::with_tod(myname, TodKind::Event8520(Tod8520::default()))
    }

    fn with_tod(myname: &str, tod_kind: TodKind) -> Self {
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
            // ciat_init: cnt = latch = 0xffff, state 0 (calloc).
            ta: Ciat::default(),
            tb: Ciat::default(),
            ta_alarmclk: CLOCK_NEVER,
            tb_alarmclk: CLOCK_NEVER,
            // ciacore_setup_context.
            last_read: 0,
            read_clk: 0,
            read_offset: 0,
            write_offset: 1,
            model: CIA_MODEL_6526,
            // ciacore_init: "not internal state, so does not get reset on RESET".
            sp_in_state: true,
            cnt_in_state: true,
            cnt_out_state: false,
            enabled: false,
            todalarm: [0; 4],
            todlatch: [0; 4],
            todlatched: false,
            todstopped: false,
            todclk: 0,
            todticks: 0,
            todtickcounter: 0,
            power_freq: 0,
            ticks_per_sec: 0,
            power_tickcounter: 0,
            power_ticks: 0,
            tod_kind,
            alarms: AlarmCtx::new(),
            clk: 0,
            irq_events: Vec::new(),
            myname: myname.to_string(),
        }
    }

    /// `cia->model` (the `CIA1Model` / `CIA2Model` resource, `cia*_update_model`).
    pub fn set_model(&mut self, model: u32) {
        self.model = model;
    }

    /// `cia1_set_timing` / `cia2_set_timing` (c64cia1.c:518-524).
    pub fn set_timing(&mut self, ticks_per_sec: u32, power_freq: u32) {
        self.power_freq = power_freq;
        self.ticks_per_sec = ticks_per_sec;
        self.todticks = if power_freq != 0 { (ticks_per_sec / power_freq) as u64 } else { 0 };
        self.power_tickcounter = 0;
        self.power_ticks = 0;
    }

    // ── my_set_int (ciacore.c:166-177) ───────────────────────────────────────
    #[inline]
    fn my_set_int(&mut self, value: bool, rclk: u64) {
        self.irq_events.push((value, rclk));
        self.irq_enabled = value;
    }

    // ── ciatimer.h wrappers that touch the alarm context ──────────────────────
    fn ciat_set_alarm_ta(&mut self) {
        let t = self.ta.predict_alarm();
        self.ta_alarmclk = t;
        if t != CLOCK_NEVER {
            self.alarms.set(CiaAlarm::Ta, t);
        } else {
            self.alarms.unset(CiaAlarm::Ta);
        }
    }
    fn ciat_set_alarm_tb(&mut self) {
        let t = self.tb.predict_alarm();
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
    /// ciatimer.h `ciat_set_latchlo` (ends in `ciat_set_alarm`).
    fn ciat_set_latchlo(&mut self, is_a: bool, byte: u8) {
        let t = if is_a { &mut self.ta } else { &mut self.tb };
        t.latch = (t.latch & 0xff00) | byte as u16;
        if (t.state & CIAT_LOAD) != 0 {
            t.cnt = (t.cnt & 0xff00) | byte as u16;
        }
        self.ciat_set_alarm(is_a);
    }
    /// ciatimer.h `ciat_set_latchhi`.
    fn ciat_set_latchhi(&mut self, is_a: bool, byte: u8) {
        let t = if is_a { &mut self.ta } else { &mut self.tb };
        t.latch = (t.latch & 0xff) | ((byte as u16) << 8);
        if (t.state & CIAT_LOAD) != 0 || (t.state & CIAT_CR_START) == 0 {
            t.cnt = t.latch;
        }
        self.ciat_set_alarm(is_a);
    }
    /// ciatimer.h `ciat_set_ctrl`.
    fn ciat_set_ctrl(&mut self, is_a: bool, byte: u8) {
        let t = if is_a { &mut self.ta } else { &mut self.tb };
        t.state &= !CIAT_CR_MASK;
        t.state |= ((byte as u16) & CIAT_CR_MASK) ^ CIAT_PHI2IN;
        self.ciat_set_alarm(is_a);
    }
    #[inline]
    fn ciat_set_alarm(&mut self, is_a: bool) {
        if is_a {
            self.ciat_set_alarm_ta();
        } else {
            self.ciat_set_alarm_tb();
        }
    }

    // ── ciacore.c:224-231 check_ciatodalarm ───────────────────────────────────
    fn check_ciatodalarm(&mut self, rclk: u64) {
        let hit = match &self.tod_kind {
            TodKind::Bcd => self.todalarm == self.c_cia[CIA_TOD_TEN..=CIA_TOD_HR],
            TodKind::Event8520(t) => t.counter == t.alarm,
        };
        if hit {
            self.cia_set_irq_flag(rclk, CIA_IM_TOD);
        }
    }

    // ── ciacore.c:240-286 ─────────────────────────────────────────────────────
    fn cia_do_update_ta(&mut self, rclk: u64) {
        let n = self.ta.update(rclk);
        if n != 0 {
            self.cia_set_irq_flag(rclk, CIA_IM_TA);
            self.tat = (self.tat + n) & 1;
        }
    }

    fn cia_do_update_tb(&mut self, rclk: u64) {
        let n = self.tb.update(rclk);
        if n != 0 {
            self.cia_set_irq_flag(rclk, CIA_IM_TB);
            if self.model == CIA_MODEL_6526 && self.rdi == rclk.wrapping_sub(1) {
                // flag the timer B bug
                self.irqflags |= CIA_IM_TBB;
            } else {
                self.irqflags &= !CIA_IM_TBB;
            }
            self.tbt = (self.tbt + n) & 1;
        }
    }

    /// ciacore.c:278-286 — `ciat_single_step` sets STEP and re-predicts, and returns 0,
    /// so no flag is raised here.
    fn cia_do_step_tb(&mut self, _rclk: u64) {
        if (self.tb.state & CIAT_CR_START) != 0 {
            self.tb.state |= CIAT_STEP;
            self.ciat_set_alarm_tb();
        }
    }

    // ── ciacore.c:292-344 ─────────────────────────────────────────────────────
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
        if (self.c_cia[CIA_CRB] & (CIA_CRB_INMODE_TA | CIA_CR_START)) == (CIA_CRB_INMODE_TA | CIA_CR_START) {
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

    // ── ciacore.c:372-430 cia_run_ifr_cycle ───────────────────────────────────
    fn cia_run_ifr_cycle(&mut self) {
        let mut delay = self.ifr_delay;
        let rclk = self.ifr_clock;

        if self.model != CIA_MODEL_6526 {
            // new fast CIA
            if delay & CIA_IRQ_ACK0 != 0 {
                self.irqflags &= !self.ack_irqflags;
                self.ack_irqflags = 0;
            }
        } else if delay & CIA_IRQ_ACK0 != 0 {
            // old slow CIA
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

    // ── ciacore.c:449-516 cia_ifr_current ─────────────────────────────────────
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

    // ── ciacore.c:526-539 cia_ifr_catchup ─────────────────────────────────────
    fn cia_ifr_catchup(&mut self, rclk: u64) {
        if self.ifr_clock < rclk {
            while (self.ifr_delay != 0 || self.new_irqflags != 0 || self.ack_irqflags != 0) && self.ifr_clock < rclk {
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

    /// ciacore.c:601-609 `ciacore_disable`.
    pub fn disable(&mut self) {
        self.alarms.unset(CiaAlarm::Idle);
        self.alarms.unset(CiaAlarm::Ta);
        self.alarms.unset(CiaAlarm::Tb);
        self.alarms.unset(CiaAlarm::Tod);
        self.alarms.unset(CiaAlarm::Sdr);
        self.enabled = false;
    }

    // ── ciacore.c:626-677 ciacore_reset ───────────────────────────────────────
    pub fn reset<B: CiaBackend + ?Sized>(&mut self, b: &mut B) {
        let clk = self.clk;
        self.c_cia = [0; 16];
        self.rdi = 0;
        self.sr_bits = 0;
        self.read_clk = 0;

        self.ciat_reset_ta(clk);
        self.ciat_reset_tb(clk);

        self.sdr_valid = false;
        self.sdr_force_finish = false;
        self.sdr_delay = CIA_SDR_CNT0 | CIA_SDR_CNT1 | CIA_SDR_CNT2 | CIA_SDR_CNT3;

        self.sp_in_state = true;
        self.cnt_in_state = true;
        self.cnt_out_state = true;

        match &mut self.tod_kind {
            TodKind::Bcd => {
                self.todalarm = [0; 4];
                self.todlatched = false;
                self.todstopped = true;
                self.c_cia[CIA_TOD_HR] = 1; // the most common value
                self.todlatch.copy_from_slice(&self.c_cia[CIA_TOD_TEN..=CIA_TOD_HR]);
                self.todclk = clk + self.todticks;
                self.alarms.set(CiaAlarm::Tod, self.todclk);
                self.todtickcounter = 0;
            }
            TodKind::Event8520(t) => {
                // Cleared, stopped, alarm clear. No TOD alarm: the pin sees no edges.
                *t = Tod8520 { stopped: true, ..Tod8520::default() };
            }
        }

        self.irqflags = 0;
        self.ack_irqflags = 0;
        self.new_irqflags = 0;
        self.irq_enabled = false;

        self.ifr_clock = 0;
        self.ifr_delay = 0;

        self.my_set_int(false, clk);

        // these must be 0xff, or programs relying on the initial value may not work
        // correctly, see bug #1143
        self.old_pa = 0xff;
        self.old_pb = 0xff;

        b.do_reset();
        self.enabled = true;

        self.alarms.set(CiaAlarm::Idle, clk + CIA_MAX_IDLE_CYCLES);
        self.alarms.unset(CiaAlarm::Ta);
        self.alarms.unset(CiaAlarm::Tb);
        self.alarms.unset(CiaAlarm::Sdr);
    }

    #[inline]
    fn pins(&self) -> CiaPins {
        CiaPins { c_cia: self.c_cia, old_pa: self.old_pa, old_pb: self.old_pb }
    }

    // ── ciacore.c:682-727 strange_extra_sdr_flags ─────────────────────────────
    fn strange_extra_sdr_flags(&mut self, rclk: u64, byte: u8) {
        if (self.sr_bits > 1 && self.sr_bits < 15) || (self.sr_bits == 15 && self.sdr_delay & CIA_SDR_CNT2 == 0) {
            self.schedule_sdr_alarm(rclk, CIA_SDR_SET_SDR_IRQ2);
        }

        if byte & CIA_CRA_SPMODE_OUT == CIA_CRA_SPMODE_IN {
            // Switching from output to input
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
            // Switching from input to output
            if !self.cnt_out_state && self.sr_bits != 0 {
                self.shifter <<= 1;
            }
            if self.sdr_force_finish {
                self.schedule_sdr_alarm(rclk, CIA_SDR_SET_SDR_IRQ2);
                self.sdr_force_finish = false;
            }
        }
    }

    /// The PB6/PB7 timer outputs folded into a port-B byte (ciacore.c:745-768, shared by
    /// `ciacore_update_pb67` and the PRB read).
    fn fold_pb67<B: CiaBackend + ?Sized>(&mut self, b: &mut B, rclk: u64, mut byte: u8) -> u8 {
        if self.c_cia[CIA_CRA] & CIA_CR_PBON != 0 {
            self.cia_update_ta(b, rclk);
            byte &= 0xbf;
            let out =
                if self.c_cia[CIA_CRA] & CIA_CR_OUTMODE_TOGGLE != 0 { self.tat != 0 } else { self.ta.is_underflow_clk() };
            if out {
                byte |= 0x40; // PB6 for timer A
            }
        }
        if self.c_cia[CIA_CRB] & CIA_CR_PBON != 0 {
            self.cia_update_tb(b, rclk);
            byte &= 0x7f;
            let out =
                if self.c_cia[CIA_CRB] & CIA_CR_OUTMODE_TOGGLE != 0 { self.tbt != 0 } else { self.tb.is_underflow_clk() };
            if out {
                byte |= 0x80; // PB7 for timer B
            }
        }
        self.cia_ifr_catchup(rclk);
        self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
        byte
    }

    // ── ciacore.c:735-777 ciacore_update_pb67 ─────────────────────────────────
    fn update_pb67<B: CiaBackend + ?Sized>(&mut self, b: &mut B, rclk: u64) -> bool {
        let mut byte = self.c_cia[CIA_PRB] | !self.c_cia[CIA_DDRB];
        let mut current_called = false;
        if (self.c_cia[CIA_CRA] | self.c_cia[CIA_CRB]) & CIA_CR_PBON != 0 {
            byte = self.fold_pb67(b, rclk, byte);
            current_called = true;
        }
        if byte != self.old_pb {
            let pins = self.pins();
            b.store_pb(self.clk, byte, &pins);
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
            CiaAlarm::Tod => self.ciacore_inttod_entry(aclk),
            CiaAlarm::Idle => self.ciacore_idle(b, aclk),
        }
    }

    /// ciacore.c:207-213 run_pending_alarms — at a register access: alarms `< clk`.
    fn run_pending_alarms<B: CiaBackend + ?Sized>(&mut self, b: &mut B, clk: u64) {
        while clk > self.alarms.next_clk {
            self.dispatch_next(b);
        }
    }

    /// The CPU loop's alarm dispatch (x64sc `interrupt_delay`, `6510core.c` PROCESS_ALARMS):
    /// every alarm `<= clk`. `self.clk` is the dispatching clock (VICE `*clk_ptr`).
    #[inline]
    pub fn process_alarms<B: CiaBackend + ?Sized>(&mut self, b: &mut B, clk: u64) {
        if clk >= self.alarms.next_clk {
            self.clk = clk;
            while clk >= self.alarms.next_clk {
                self.dispatch_next(b);
            }
        }
    }

    /// The next clock an alarm is due (`CLOCK_NEVER` when none).
    #[inline]
    pub fn next_alarm_clk(&self) -> u64 {
        self.alarms.next_clk
    }

    // ── ciacore.c:779-1111 ciacore_store_internal ─────────────────────────────
    fn store_internal<B: CiaBackend + ?Sized>(&mut self, b: &mut B, addr: u16, mut byte: u8) {
        let addr = (addr & 0xf) as usize;
        // stores have a one-cycle offset if CLK++ happens before store
        let rclk = self.clk.wrapping_sub(self.write_offset);

        self.run_pending_alarms(b, rclk);

        match addr {
            CIA_PRA | CIA_DDRA => {
                self.c_cia[addr] = byte;
                byte = self.c_cia[CIA_PRA] | !self.c_cia[CIA_DDRA];
                if byte != self.old_pa {
                    let pins = self.pins();
                    b.store_pa(self.clk, byte, &pins);
                    self.old_pa = byte;
                }
            }
            CIA_PRB | CIA_DDRB => {
                self.c_cia[addr] = byte;
                self.update_pb67(b, rclk);
                if addr == CIA_PRB {
                    let pins = self.pins();
                    b.pulse_pc(rclk, &pins);
                }
            }
            CIA_TAL => {
                self.cia_update_ta(b, rclk);
                self.cia_ifr_catchup(rclk);
                self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                self.ciat_set_latchlo(true, byte);
            }
            CIA_TBL => {
                self.cia_update_tb(b, rclk);
                self.cia_ifr_catchup(rclk);
                self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                self.ciat_set_latchlo(false, byte);
            }
            CIA_TAH => {
                self.cia_update_ta(b, rclk);
                self.cia_ifr_catchup(rclk);
                self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                self.ciat_set_latchhi(true, byte);
            }
            CIA_TBH => {
                self.cia_update_tb(b, rclk);
                self.cia_ifr_catchup(rclk);
                self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                self.ciat_set_latchhi(false, byte);
            }
            CIA_TOD_TEN | CIA_TOD_SEC | CIA_TOD_MIN | CIA_TOD_HR => {
                if matches!(self.tod_kind, TodKind::Event8520(_)) {
                    self.store_tod_8520(rclk, addr, byte);
                } else {
                    self.store_tod_bcd(rclk, addr, byte);
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
                    // Both pending interrupts and currently active interrupts are never
                    // cancelled or cleared.
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
                    // Starting timer A
                    self.tat = 1;
                }

                // Is the serial I/O direction changing?
                if (byte ^ self.c_cia[CIA_CRA]) & CIA_CRA_SPMODE != 0 {
                    self.strange_extra_sdr_flags(rclk, byte);
                    self.sr_bits = 0;
                    self.sdr_valid = false;
                    self.sdr_delay &= !(ALL_SDR_TOGGLE_CNT | ALL_SDR_NOGGLE_CNT);
                    if !self.cnt_out_state {
                        self.cnt_out_state = true;
                        b.set_cnt(rclk, true);
                    }
                }

                self.ciat_set_ctrl(true, byte);

                self.c_cia[addr] = byte & 0xef; // remove strobe

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

                // bit 5 is set when single-stepping is set
                if byte & CIA_CRB_INMODE_TA != 0 {
                    // we count ta - so we enable that
                    self.ciat_set_alarm_ta();
                    self.ciat_set_ctrl(false, byte | 0x20);
                } else {
                    self.ciat_set_ctrl(false, byte);
                }

                self.c_cia[addr] = byte & 0xef; // remove strobe

                if !self.update_pb67(b, rclk) {
                    self.cia_ifr_catchup(rclk);
                    self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
                }
            }
            _ => self.c_cia[addr] = byte,
        }
    }

    /// The TOD store of ciacore.c:879-934 (BCD).
    fn store_tod_bcd(&mut self, rclk: u64, addr: usize, mut byte: u8) {
        if addr == CIA_TOD_HR {
            // force bits 6-5 = 0
            byte &= 0x9f;
            // Flip AM/PM on hour 12 — only when writing the time, not the alarm.
            if (byte & 0x1f) == 0x12 && (self.c_cia[CIA_CRB] & CIA_CRB_ALARM) == CIA_CRB_ALARM_TOD {
                byte ^= 0x80;
            }
        } else if addr == CIA_TOD_MIN || addr == CIA_TOD_SEC {
            byte &= 0x7f;
        } else if addr == CIA_TOD_TEN {
            byte &= 0x0f;
        }
        let changed;
        if self.c_cia[CIA_CRB] & CIA_CRB_ALARM_ALARM != 0 {
            // set alarm
            changed = self.todalarm[addr - CIA_TOD_TEN] != byte;
            self.todalarm[addr - CIA_TOD_TEN] = byte;
        } else {
            // set time
            if addr == CIA_TOD_TEN && self.todstopped {
                // the tickcounter is kept clear while the clock is not running and then
                // restarted by writing to the 10th seconds register
                self.todtickcounter = 0;
                self.todstopped = false;
            }
            if addr == CIA_TOD_HR {
                self.todstopped = true;
            }
            changed = self.c_cia[addr] != byte;
            if changed {
                self.c_cia[addr] = byte;
            }
        }
        if changed {
            self.check_ciatodalarm(rclk);
            self.cia_ifr_catchup(rclk);
            self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
        }
    }

    /// The 8520 TOD store (module doc): `$8/$9/$A` are counter bits 0-7/8-15/16-23, `$B`
    /// is not connected; CRB7 routes the write to the alarm.
    fn store_tod_8520(&mut self, rclk: u64, addr: usize, byte: u8) {
        if addr == CIA_TOD_HR {
            return;
        }
        let shift = 8 * (addr - CIA_TOD_TEN) as u32;
        let mask = !(0xffu32 << shift);
        let alarm_mode = self.c_cia[CIA_CRB] & CIA_CRB_ALARM_ALARM != 0;
        let changed = match &mut self.tod_kind {
            TodKind::Event8520(t) => {
                if alarm_mode {
                    let v = (t.alarm & mask) | ((byte as u32) << shift);
                    let c = v != t.alarm;
                    t.alarm = v;
                    c
                } else {
                    if addr == CIA_TOD_TEN {
                        t.stopped = false;
                    }
                    if addr == CIA_TOD_MIN {
                        t.stopped = true;
                    }
                    let v = (t.counter & mask) | ((byte as u32) << shift);
                    let c = v != t.counter;
                    t.counter = v;
                    c
                }
            }
            TodKind::Bcd => unreachable!(),
        };
        if changed {
            self.check_ciatodalarm(rclk);
            self.cia_ifr_catchup(rclk);
            self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
        }
    }

    /// ciacore.c:1113-1127 ciacore_store (the RMW arm is the CPU core's, module doc).
    #[inline]
    pub fn store<B: CiaBackend + ?Sized>(&mut self, b: &mut B, addr: u16, byte: u8) {
        self.store_internal(b, addr, byte);
    }

    // ── ciacore.c:1132-1347 ciacore_read ──────────────────────────────────────
    pub fn read<B: CiaBackend + ?Sized>(&mut self, b: &mut B, addr: u16) -> u8 {
        let addr = (addr & 0xf) as usize;
        self.read_clk = self.clk;
        self.read_offset = 0;
        let rclk = self.clk; // READ_OFFSET 0

        self.run_pending_alarms(b, rclk);

        match addr {
            CIA_PRA => {
                let pins = self.pins();
                self.last_read = b.read_pa(&pins);
                self.last_read
            }
            CIA_PRB => {
                let pins = self.pins();
                let mut byte = b.read_pb(&pins);
                b.pulse_pc(rclk, &pins);
                if (self.c_cia[CIA_CRA] | self.c_cia[CIA_CRB]) & CIA_CR_PBON != 0 {
                    byte = self.fold_pb67(b, rclk, byte);
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
                self.c_cia[CIA_SDR] = b.read_sdr(self.c_cia[CIA_SDR]);
                self.last_read = self.c_cia[CIA_SDR];
                self.last_read
            }
            CIA_ICR => {
                self.cia_update_ta(b, rclk);
                self.cia_update_tb(b, rclk);
                self.cia_ifr_catchup(rclk);
                self.cia_ifr_current(rclk, CIA_IFR_CURRENT);

                self.rdi = rclk;

                b.read_icr();

                self.ciat_set_alarm_ta();
                self.ciat_set_alarm_tb();

                if self.irqflags & CIA_IM_TBB != 0 {
                    // timer b bug
                    self.irqflags &= !(CIA_IM_TBB | CIA_IM_TB);
                }

                let result;
                if self.model != CIA_MODEL_6526 {
                    // new fast CIA
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
                    // ack_irqflags effectively isn't used for old CIAs
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

    /// The TOD read: BCD (ciacore.c:1228-1247) or the 8520 latch (module doc).
    fn tod_read(&mut self, addr: usize) -> u8 {
        match &mut self.tod_kind {
            TodKind::Bcd => {
                if !self.todlatched {
                    self.todlatch.copy_from_slice(&self.c_cia[CIA_TOD_TEN..=CIA_TOD_HR]);
                }
                if addr == CIA_TOD_TEN {
                    self.todlatched = false;
                }
                if addr == CIA_TOD_HR {
                    self.todlatched = true;
                }
                self.todlatch[addr - CIA_TOD_TEN]
            }
            TodKind::Event8520(t) => {
                if addr == CIA_TOD_HR {
                    // Not connected; nothing on the chip answers for it. Read as 0.
                    return 0;
                }
                if !t.latched {
                    t.latch = t.counter;
                }
                if addr == CIA_TOD_TEN {
                    t.latched = false;
                }
                if addr == CIA_TOD_MIN {
                    t.latched = true;
                }
                ((t.latch >> (8 * (addr - CIA_TOD_TEN))) & 0xff) as u8
            }
        }
    }

    /// One positive edge on the 8520's TOD pin (the stock 1581 board has none).
    pub fn tod_pin_edge(&mut self) {
        let clk = self.clk;
        let fire = match &mut self.tod_kind {
            TodKind::Event8520(t) if !t.stopped => {
                t.counter = (t.counter + 1) & 0x00ff_ffff;
                true
            }
            _ => false,
        };
        if fire {
            self.check_ciatodalarm(clk);
        }
    }

    /// The 8520's TOD state, if the chip has one.
    pub fn tod8520(&self) -> Option<&Tod8520> {
        match &self.tod_kind {
            TodKind::Event8520(t) => Some(t),
            TodKind::Bcd => None,
        }
    }

    /// ciacore.c:1351-1416 `ciacore_peek`, without touching the chip. VICE reads ports,
    /// timers and control registers through `ciacore_read` (alarms, timer catch-up, the
    /// IFR line); here that read runs on a copy, so the answer is the one the CPU would
    /// get at `self.clk` and nothing moves. TOD answers the running clock (not the read
    /// latch), SDR its byte, ICR the flags as they stand.
    pub fn peek_with<B: CiaBackend + ?Sized>(&self, b: &mut B, addr: u16) -> u8 {
        let a = (addr & 0xf) as usize;
        match a {
            CIA_TOD_TEN | CIA_TOD_SEC | CIA_TOD_MIN | CIA_TOD_HR => match &self.tod_kind {
                TodKind::Bcd => self.c_cia[a],
                TodKind::Event8520(t) => {
                    if a == CIA_TOD_HR {
                        0
                    } else {
                        ((t.counter >> (8 * (a - CIA_TOD_TEN))) & 0xff) as u8
                    }
                }
            },
            CIA_SDR => self.c_cia[CIA_SDR],
            CIA_ICR => self.irqflags as u8,
            _ => {
                let mut c = self.clone();
                c.irq_events.clear();
                c.read(b, addr)
            }
        }
    }

    /// [`Self::peek_with`] with nothing on the ports (they read back their outputs).
    pub fn peek(&self, addr: u16) -> u8 {
        self.peek_with(&mut NoPorts, addr)
    }

    /// The composed port outputs as the pins drive them (`old_pa` / `old_pb`).
    #[inline]
    pub fn pa_out(&self) -> u8 {
        self.old_pa
    }
    #[inline]
    pub fn pb_out(&self) -> u8 {
        self.old_pb
    }

    // ── ciacore.c:1421-1490 ciacore_intta ─────────────────────────────────────
    fn ciacore_intta<B: CiaBackend + ?Sized>(&mut self, b: &mut B, rclk: u64) {
        self.cia_do_update_ta(rclk); // may set CIA_IM_TA
        self.ciat_ack_alarm_ta();

        if self.c_cia[CIA_CRA] & (CIA_CRA_INMODE | CIA_CR_RUNMODE | CIA_CR_START)
            == (CIA_CRA_INMODE_PHI2 | CIA_CR_RUNMODE_CONTINUOUS | CIA_CR_START)
            // if we do not need alarm, no PB6, no shift register, and not timer B
            // counting timer A, then we can safely skip alarms...
            && ((self.c_cia[CIA_ICR] as u32 & CIA_IM_TA != 0 && self.irqflags & CIA_IM_SET == 0)
                || self.c_cia[CIA_CRA] & (CIA_CRA_SPMODE | CIA_CRA_INMODE) != 0
                || self.c_cia[CIA_CRB] & CIA_CRB_INMODE_TA != 0)
        {
            self.ciat_set_alarm_ta();
        }

        if self.c_cia[CIA_CRA] & CIA_CRA_SPMODE_OUT != 0 && (self.sr_bits != 0 || self.sdr_valid) {
            // ~1.5 cycle delay until CNT is toggled.
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

    // ── ciacore.c:1512-1550 ciacore_inttb ─────────────────────────────────────
    fn ciacore_inttb(&mut self, rclk: u64) {
        self.cia_do_update_tb(rclk);
        self.ciat_ack_alarm_tb();
        // running and continous, then next alarm
        if self.c_cia[CIA_CRB] & (CIA_CRB_INMODE | CIA_CR_RUNMODE | CIA_CR_START)
            == (CIA_CRB_INMODE_PHI2 | CIA_CR_RUNMODE_CONTINUOUS | CIA_CR_START)
            // if no interrupt flag we can safely skip alarms
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

    // ── ciacore.c:1574-1611 ciacore_async_interrupt / ciacore_set_flag ────────
    fn ciacore_async_interrupt(&mut self, flag: u32) {
        let rclk = self.clk;
        self.cia_set_irq_flag(rclk, flag);
        // Leave calling cia_ifr_current() to the idle (or another) alarm.
        let idleclk = self.alarms.clk_of(CiaAlarm::Idle);
        if idleclk > rclk + 1 {
            self.alarms.set(CiaAlarm::Idle, rclk + 1);
        }
    }

    /// ciacore.c `ciacore_set_flag` — a falling edge on FLAG at `self.clk`.
    pub fn set_flag(&mut self) {
        self.ciacore_async_interrupt(CIA_IM_FLG);
    }

    /// ciacore.c `ciacore_set_sdr` — a whole byte arrives in the shift register.
    pub fn set_sdr(&mut self, data: u8) {
        if self.c_cia[CIA_CRA] & CIA_CRA_SPMODE == CIA_CRA_SPMODE_IN {
            self.c_cia[CIA_SDR] = data;
            self.ciacore_async_interrupt(CIA_IM_SDR);
            self.alarms.unset(CiaAlarm::Sdr);
        }
    }

    /// ciacore.c `ciacore_set_cnt` — the CNT input.
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

    /// ciacore.c `ciacore_set_sp` — the SP input.
    pub fn set_sp(&mut self, data: bool) {
        self.sp_in_state = data;
    }

    // ── ciacore.c:1709-1713 schedule_sdr_alarm ────────────────────────────────
    fn schedule_sdr_alarm(&mut self, rclk: u64, feed: u32) {
        self.sdr_delay |= feed;
        self.alarms.set(CiaAlarm::Sdr, rclk);
    }

    // ── ciacore.c:1723-1823 ciacore_intsdr ────────────────────────────────────
    fn ciacore_intsdr<B: CiaBackend + ?Sized>(&mut self, b: &mut B, rclk: u64) {
        let mut feed = 0u32;

        if self.alarms.clk_of(CiaAlarm::Ta) == rclk {
            // INT TA scheduled at the same clock cycle - do that one first
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
            let odd = if self.sr_bits != 0 {
                self.sr_bits -= 1;
                self.sr_bits & 1 != 0
            } else {
                false
            };
            if odd {
                // Note: the bit that's just left of the byte
                let bit = (self.shifter >> 8) & 1 != 0;
                b.set_sp(rclk, bit);
                self.cnt_out_state = false;
                b.set_cnt(rclk, false);

                if self.sr_bits == 1 {
                    b.store_sdr(((self.shifter >> 8) & 0xff) as u8);
                    // IFR/IRQ requires 2 cycle delay
                    feed |= CIA_SDR_SET_SDR_IRQ2;
                    if self.sdr_valid {
                        self.shifter |= self.c_cia[CIA_SDR] as u16;
                        self.sdr_valid = false;
                        self.sr_bits = 17;
                    }
                }
            } else {
                // So either sr_bits was 0, or after decrementing it is even.
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
            // don't call it twice
            self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
        }
    }

    // ── ciacore.c:1842-1987 ciacore_inttod ────────────────────────────────────
    fn ciacore_inttod(&mut self, rclk: u64) {
        let clk = self.clk;
        if self.power_freq == 0 {
            // power frequency not initialized, or not present: check again in about
            // 1/10th second
            self.todclk = clk + 100_000;
            self.alarms.set(CiaAlarm::Tod, self.todclk);
            return;
        }

        // The time between power ticks is ticks_per_sec / power_freq; keep the tick as
        // close as possible to the wanted 50/60 Hz over a second. VICE's TODRANDOM branch
        // jitters this by lib_unsigned_rand(0, 3); this is the deterministic #else branch
        // of the same code (module doc).
        self.todticks = (self.ticks_per_sec / self.power_freq) as u64;
        let tclk = (self.power_tickcounter as u64 * self.ticks_per_sec as u64) / self.power_freq as u64;
        if self.power_ticks < tclk {
            self.todticks += 1;
        } else if self.power_ticks > tclk {
            self.todticks -= 1;
        }
        self.power_tickcounter += 1;
        if self.power_tickcounter >= self.power_freq {
            self.todticks = (self.ticks_per_sec as u64).wrapping_sub(self.power_ticks);
            self.power_tickcounter = 0;
            self.power_ticks = 0;
        } else {
            self.power_ticks += self.todticks;
        }

        self.todclk = clk + self.todticks;
        self.alarms.set(CiaAlarm::Tod, self.todclk);

        let mut update = false;
        if !self.todstopped {
            // The 3-bit ring counter goes 000, 001, 011, 111, 110, 100; it matches at
            // "4" for 50 Hz and "5" for 60 Hz (the middle bit of the match is CRA7).
            update = self.todtickcounter == if self.c_cia[CIA_CRA] & CIA_CRA_TODIN_50HZ != 0 { 4 } else { 5 };
            if update {
                self.todtickcounter = 0;
            } else {
                self.todtickcounter += 1;
                if self.todtickcounter > 5 {
                    self.todtickcounter = 0;
                }
            }
        }

        if update {
            // individual counters are 4 bit except for sh and mh which are 3 bits
            let mut ts = self.c_cia[CIA_TOD_TEN] & 0x0f;
            let mut sl = self.c_cia[CIA_TOD_SEC] & 0x0f;
            let mut sh = (self.c_cia[CIA_TOD_SEC] >> 4) & 0x07;
            let mut ml = self.c_cia[CIA_TOD_MIN] & 0x0f;
            let mut mh = (self.c_cia[CIA_TOD_MIN] >> 4) & 0x07;
            let mut hl = self.c_cia[CIA_TOD_HR] & 0x0f;
            let mut hh = (self.c_cia[CIA_TOD_HR] >> 4) & 0x01;
            let mut pm = self.c_cia[CIA_TOD_HR] & 0x80;

            ts = (ts + 1) & 0x0f;
            if ts == 10 {
                ts = 0;
                sl = (sl + 1) & 0x0f;
                if sl == 10 {
                    sl = 0;
                    sh = (sh + 1) & 0x07;
                    if sh == 6 {
                        sh = 0;
                        ml = (ml + 1) & 0x0f;
                        if ml == 10 {
                            ml = 0;
                            mh = (mh + 1) & 0x07;
                            if mh == 6 {
                                mh = 0;
                                // flip from 09:59:59 to 10:00:00 or 12:59:59 to 01:00:00
                                if (hh == 1 && hl == 2) || (hh == 0 && hl == 9) {
                                    hl = hh;
                                    hh ^= 1;
                                } else {
                                    hl = (hl + 1) & 0x0f;
                                    // toggle the am/pm flag when reaching 12
                                    if hh == 1 && hl == 2 {
                                        pm ^= 0x80;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            self.c_cia[CIA_TOD_TEN] = ts;
            self.c_cia[CIA_TOD_SEC] = sl | (sh << 4);
            self.c_cia[CIA_TOD_MIN] = ml | (mh << 4);
            self.c_cia[CIA_TOD_HR] = hl | (hh << 4) | pm;

            self.check_ciatodalarm(rclk);
        }
    }

    fn ciacore_inttod_entry(&mut self, rclk: u64) {
        self.ciacore_inttod(rclk);
        self.cia_ifr_catchup(rclk);
        if self.ifr_clock == rclk {
            // don't call it twice
            self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
        }
    }

    // ── ciacore.c:2010-2034 ciacore_idle ──────────────────────────────────────
    fn ciacore_idle<B: CiaBackend + ?Sized>(&mut self, b: &mut B, rclk: u64) {
        self.cia_update_ta(b, rclk);
        self.cia_update_tb(b, rclk);
        // Schedule another idle alarm far in the future, before running the ifr delay
        // line, in case that wants to schedule it sooner.
        self.alarms.set(CiaAlarm::Idle, rclk + CIA_MAX_IDLE_CYCLES);
        self.cia_ifr_catchup(rclk);
        if self.ifr_clock == rclk {
            // don't call it twice
            self.cia_ifr_current(rclk, CIA_IFR_CUR_NXT);
        }
    }

    // ── the alarm context, for a checkpoint ───────────────────────────────────

    /// The alarm clocks as set now.
    pub fn alarm_clocks(&self) -> CiaAlarms {
        let at = |a: CiaAlarm| {
            let i = self.alarms.pending_idx[a.idx()];
            (i >= 0).then(|| self.alarms.pending[i as usize].1)
        };
        CiaAlarms {
            idle: at(CiaAlarm::Idle),
            ta: at(CiaAlarm::Ta),
            tb: at(CiaAlarm::Tb),
            tod: at(CiaAlarm::Tod),
            sdr: at(CiaAlarm::Sdr),
        }
    }

    /// Put the alarm context back. The pending array is rebuilt in slot order, which
    /// changes which of two alarms due at the SAME clock runs first only if a checkpoint
    /// was taken between their arming — and a CIA's callbacks commute at one clock except
    /// for Timer A against SDR, which `ciacore_intsdr` orders itself.
    pub fn set_alarm_clocks(&mut self, a: &CiaAlarms) {
        self.alarms = AlarmCtx::new();
        for (kind, clk) in [
            (CiaAlarm::Idle, a.idle),
            (CiaAlarm::Ta, a.ta),
            (CiaAlarm::Tb, a.tb),
            (CiaAlarm::Tod, a.tod),
            (CiaAlarm::Sdr, a.sdr),
        ] {
            if let Some(c) = clk {
                self.alarms.set(kind, c);
            }
        }
    }

    // ── ciacore.c:2190-2312 ciacore_snapshot_write_module ─────────────────────
    /// Write the chip's module in VICE's 2.5 layout. For the 8520 the TOD fields carry
    /// the counter (TOD_TEN/SEC/MIN), the alarm (ALARM_*) and the read latch (TODL_*);
    /// `$B` fields and TOD_TICKS are 0 (no tick is scheduled).
    pub fn snapshot_write_module<B: CiaBackend + ?Sized>(&mut self, b: &mut B, s: &mut SnapshotT) {
        let rclk = self.clk;
        self.cia_update_ta(b, rclk);
        self.cia_update_tb(b, rclk);
        self.cia_ifr_catchup(rclk);
        self.cia_ifr_current(rclk, CIA_IFR_CURRENT);

        let mut m = s.module_create(&self.myname.clone(), CIA_DUMP_VER_MAJOR, CIA_DUMP_VER_MINOR);
        let w = |s: &mut SnapshotT, m: &mut SnapshotModule, v: u8| s.smw_b(m, v);
        let byte_of = |v: u32, i: u32| ((v >> (8 * i)) & 0xff) as u8;
        let (tod, alarm, latch, latched_stopped, tod_ticks) = match &self.tod_kind {
            TodKind::Bcd => (
                [self.c_cia[8], self.c_cia[9], self.c_cia[10], self.c_cia[11]],
                self.todalarm,
                self.todlatch,
                (self.todlatched as u8) | if self.todstopped { 2 } else { 0 },
                self.todclk.wrapping_sub(rclk),
            ),
            TodKind::Event8520(t) => (
                [byte_of(t.counter, 0), byte_of(t.counter, 1), byte_of(t.counter, 2), 0],
                [byte_of(t.alarm, 0), byte_of(t.alarm, 1), byte_of(t.alarm, 2), 0],
                [byte_of(t.latch, 0), byte_of(t.latch, 1), byte_of(t.latch, 2), 0],
                (t.latched as u8) | if t.stopped { 2 } else { 0 },
                0,
            ),
        };

        w(s, &mut m, self.c_cia[CIA_PRA]);
        w(s, &mut m, self.c_cia[CIA_PRB]);
        w(s, &mut m, self.c_cia[CIA_DDRA]);
        w(s, &mut m, self.c_cia[CIA_DDRB]);
        s.smw_w(&mut m, self.ta.read_timer());
        s.smw_w(&mut m, self.tb.read_timer());
        for v in tod {
            w(s, &mut m, v);
        }
        w(s, &mut m, self.c_cia[CIA_SDR]);
        w(s, &mut m, self.c_cia[CIA_ICR]);
        w(s, &mut m, self.c_cia[CIA_CRA]);
        w(s, &mut m, self.c_cia[CIA_CRB]);
        s.smw_w(&mut m, self.ta.latch);
        s.smw_w(&mut m, self.tb.latch);
        // ciacore_peek(CIA_ICR) = irqflags.
        w(s, &mut m, self.irqflags as u8);
        // Bits 2 & 3 are compatibility to snapshot format v1.0
        w(
            s,
            &mut m,
            (if self.tat != 0 { 0x40 } else { 0 })
                | (if self.tbt != 0 { 0x80 } else { 0 })
                | (if self.ta.is_underflow_clk() { 0x04 } else { 0 })
                | (if self.tb.is_underflow_clk() { 0x08 } else { 0 }),
        );
        w(s, &mut m, self.sr_bits as u8);
        for v in alarm {
            w(s, &mut m, v);
        }
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
        w(s, &mut m, latched_stopped);
        for v in latch {
            w(s, &mut m, v);
        }
        s.smw_clock(&mut m, tod_ticks);
        // ciat_save_snapshot (ver >= 0x100): the state word.
        s.smw_w(&mut m, self.ta.state);
        s.smw_w(&mut m, self.tb.state);
        w(s, &mut m, (self.shifter & 0xff) as u8);
        w(s, &mut m, self.sdr_valid as u8);
        w(s, &mut m, self.irq_enabled as u8);
        w(s, &mut m, self.todtickcounter);
        w(s, &mut m, (self.shifter >> 8) as u8); // SHIFTER_HI
        let sdr_pending = self.alarms.clk_of(CiaAlarm::Sdr);
        let sdr_alarm =
            if sdr_pending > 0 { (1u64.wrapping_add(sdr_pending).wrapping_sub(rclk) & 0xff) as u8 } else { 0 };
        w(s, &mut m, sdr_alarm); // SDR_ALARM
        w(
            s,
            &mut m,
            (if self.sp_in_state { 0x80 } else { 0 })
                | (if self.cnt_in_state { 0x40 } else { 0 })
                | (if self.sdr_force_finish { 0x20 } else { 0 }),
        ); // SP_CNT_IN
        s.smw_dw(&mut m, self.sdr_delay); // SDR_DELAY
        w(s, &mut m, if self.cnt_out_state { 0x40 } else { 0 }); // SP_CNT_OUT
        s.smw_dw(&mut m, self.ifr_delay); // IFR_DELAY
        // VICE writes these two with SMW_DB — `snapshot_module_write_double`, eight bytes
        // of a C double — and reads them back with SMR_B (one byte). Both halves of that
        // are ported as they are.
        s.smw_db(&mut m, self.ack_irqflags as f64); // ACK_IRQFLAGS
        s.smw_db(&mut m, self.new_irqflags as f64); // NEW_IRQFLAGS
        s.module_close(&m);
    }

    // ── ciacore.c:2314-2517 ciacore_snapshot_read_module ──────────────────────
    /// Read the module back. Returns whether the interrupt line was restored active
    /// (VICE `cia_restore_int`), for the board to hand its CPU.
    pub fn snapshot_read_module<B: CiaBackend + ?Sized>(&mut self, b: &mut B, s: &mut SnapshotT) -> Result<bool, String> {
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
        // The reset's own line event is not part of the restored state.
        self.irq_events.clear();

        // stop timers, just in case
        self.ciat_set_ctrl(true, 0);
        self.ciat_set_ctrl(false, 0);
        self.alarms.unset(CiaAlarm::Tod);
        self.alarms.unset(CiaAlarm::Sdr);

        self.c_cia[CIA_PRA] = rb!();
        self.c_cia[CIA_PRB] = rb!();
        self.c_cia[CIA_DDRA] = rb!();
        self.c_cia[CIA_DDRB] = rb!();
        let pa = self.c_cia[CIA_PRA] | !self.c_cia[CIA_DDRA];
        self.old_pa = pa ^ 0xff; // all bits change?
        let pins = self.pins();
        b.undump_pa(rclk, pa, &pins);
        self.old_pa = pa;
        let pb = self.c_cia[CIA_PRB] | !self.c_cia[CIA_DDRB];
        self.old_pb = pb ^ 0xff;
        let pins = self.pins();
        b.undump_pb(rclk, pb, &pins);
        self.old_pb = pb;

        let tac = rw!();
        let tbc = rw!();
        let tod = [rb!(), rb!(), rb!(), rb!()];
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
        let alarm = [rb!(), rb!(), rb!(), rb!()];
        let readicr = rb!();
        self.rdi = if readicr != 0 { self.clk.wrapping_add(128).wrapping_sub(readicr as u64) } else { 0 };
        let todl = rb!();
        let latch = [rb!(), rb!(), rb!(), rb!()];
        let tod_ticks = s.smr_clock().ok_or_else(|| format!("{name}: truncated"))?;
        let word = |v: [u8; 4]| v[0] as u32 | (v[1] as u32) << 8 | (v[2] as u32) << 16;
        match &mut self.tod_kind {
            TodKind::Bcd => {
                self.c_cia[CIA_TOD_TEN..=CIA_TOD_HR].copy_from_slice(&tod);
                self.todalarm = alarm;
                self.todlatched = todl & 1 != 0;
                self.todstopped = todl & 2 != 0;
                self.todlatch = latch;
                self.todclk = self.clk + tod_ticks;
                self.alarms.set(CiaAlarm::Tod, self.todclk);
            }
            TodKind::Event8520(t) => {
                t.counter = word(tod);
                t.alarm = word(alarm);
                t.latched = todl & 1 != 0;
                t.stopped = todl & 2 != 0;
                t.latch = word(latch);
            }
        }

        // ciat_load_snapshot for both timers.
        let ver = ((vmajor as u32) << 8) | vminor as u32;
        for (is_a, cnt, latch, cr) in [(true, tac, tal, self.c_cia[CIA_CRA]), (false, tbc, tbl, self.c_cia[CIA_CRB])] {
            let state = if ver >= 0x101 {
                rw!()
            } else {
                let mut st = cr as u16;
                if cr & CIA_CR_START != 0 {
                    st |= CIAT_COUNT2 | CIAT_COUNT3 | CIAT_COUNT;
                }
                if cr & CIA_CR_RUNMODE_ONE_SHOT != 0 {
                    st |= CIAT_ONESHOT0 | CIAT_ONESHOT;
                }
                st
            };
            let t = if is_a { &mut self.ta } else { &mut self.tb };
            t.clk = rclk;
            t.cnt = cnt;
            t.latch = latch;
            t.state = state;
            self.ciat_set_alarm(is_a);
        }

        let mut restored_irq = false;
        if vminor > 1 {
            self.shifter = rb!() as u16;
            self.sdr_valid = rb!() != 0;
            self.irq_enabled = rb!() != 0;
            restored_irq = self.irq_enabled;
            self.todtickcounter = rb!();
        }
        if vminor > 2 {
            self.shifter |= (rb!() as u16) << 8; // SHIFTER_HI
            let sdr_alarm = rb!(); // SDR_ALARM
            if sdr_alarm != 0 {
                self.alarms.set(CiaAlarm::Sdr, rclk + sdr_alarm as u64 - 1);
            }
            let spcnt = rb!(); // SP_CNT_IN
            self.sp_in_state = spcnt & 0x80 != 0;
            self.cnt_in_state = spcnt & 0x40 != 0;
            self.sdr_force_finish = spcnt & 0x20 != 0;
        }
        if vminor > 3 {
            self.sdr_delay = rdw!(); // SDR_DELAY
            let out = rb!(); // SP_CNT_OUT
            self.cnt_out_state = out & 0x40 != 0;
        }
        if vminor > 4 {
            self.ifr_delay = rdw!(); // IFR_DELAY
            self.ifr_clock = rclk + 1;
            self.ack_irqflags = rb!() as u32; // ACK_IRQFLAGS (SMR_B, module doc of write)
            self.new_irqflags = rb!() as u32; // NEW_IRQFLAGS
            self.cia_ifr_current(rclk, CIA_IFR_NEXT);
        }
        s.module_close(&m);
        Ok(restored_irq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Pb(Vec<u8>);
    impl CiaBackend for Pb {
        fn read_pa(&mut self, p: &CiaPins) -> u8 {
            p.c_cia[CIA_PRA] | !p.c_cia[CIA_DDRA]
        }
        fn read_pb(&mut self, p: &CiaPins) -> u8 {
            p.c_cia[CIA_PRB] | !p.c_cia[CIA_DDRB]
        }
        fn store_pb(&mut self, _clk: u64, byte: u8, _p: &CiaPins) {
            self.0.push(byte);
        }
    }

    fn fresh_8520() -> (CiaCore, Pb) {
        let mut c = CiaCore::new_8520("CIA1581D0");
        let mut b = Pb(vec![]);
        c.clk = 0;
        c.reset(&mut b);
        c.irq_events.clear();
        (c, b)
    }

    /// The FLAG pin: a falling ATN edge latches CIA_IM_FLG, and with FLG enabled in the
    /// mask the IRQ line goes active one cycle later (the old CIA's delay line).
    #[test]
    fn flag_raises_the_irq_through_the_delay_line() {
        let (mut c, mut b) = fresh_8520();
        c.clk = 10;
        c.store(&mut b, 0x0d, 0x80 | CIA_IM_FLG as u8);
        c.clk = 100;
        c.set_flag();
        assert!(c.irqflags & CIA_IM_FLG != 0, "FLG latched at the edge");
        c.process_alarms(&mut b, 120);
        assert!(c.irq_events.iter().any(|&(v, _)| v), "the line went active: {:?}", c.irq_events);
        let (_, at) = *c.irq_events.iter().rev().find(|e| e.0).unwrap();
        assert!((101..=103).contains(&at), "asserted just after the edge, at {at}");
        c.clk = 130;
        let icr = c.read(&mut b, 0x0d);
        assert_eq!(icr as u32 & (CIA_IM_FLG | CIA_IM_SET), CIA_IM_FLG | CIA_IM_SET, "ICR {icr:02x}");
        assert_eq!(c.irq_events.last(), Some(&(false, 130)));
    }

    /// Timer A one-shot underflow sets CIA_IM_TA at the predicted clock.
    #[test]
    fn timer_a_underflow_is_an_alarm() {
        let (mut c, mut b) = fresh_8520();
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
        let (mut c, mut b) = fresh_8520();
        c.clk = 5;
        c.store(&mut b, 0x0a, 0x12);
        c.store(&mut b, 0x09, 0x34);
        c.store(&mut b, 0x08, 0x00);
        assert!(!c.tod8520().unwrap().stopped, "writing the LSB starts it");
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
            fn store_sdr(&mut self, b: u8) {
                self.0.push(b);
            }
            fn set_cnt(&mut self, _: u64, bit: bool) {
                if !bit {
                    self.1 += 1;
                }
            }
        }
        let mut c = CiaCore::new_8520("CIA1581D0");
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
            c.process_alarms(&mut b, clk);
        }
        assert_eq!(b.0, vec![0xa5], "the byte went out once");
        assert_eq!(b.1, 8, "eight CNT pulses");
        assert!(c.irqflags & CIA_IM_SDR != 0, "SDR flag");
    }

    fn fresh_c64(model: u32) -> (CiaCore, NoPorts) {
        let mut c = CiaCore::new("CIA1");
        c.write_offset = 0;
        c.set_model(model);
        c.set_timing(985_248, 50);
        c.clk = 0;
        let mut b = NoPorts;
        c.reset(&mut b);
        c.irq_events.clear();
        (c, b)
    }

    /// Issue #3: writing the mask alone does not release a pending interrupt — IR and the
    /// line stay until the ICR is READ. Both models.
    #[test]
    fn a_mask_write_does_not_acknowledge() {
        for model in [CIA_MODEL_6526, CIA_MODEL_6526A] {
            let (mut c, mut b) = fresh_c64(model);
            c.clk = 10;
            c.store(&mut b, 0x0d, 0x81);
            c.store(&mut b, 0x04, 0x10);
            c.store(&mut b, 0x05, 0x00);
            c.store(&mut b, 0x0e, 0x19);
            for clk in 11..60 {
                c.process_alarms(&mut b, clk);
            }
            assert!(c.irq_enabled, "model {model}: the line is up");
            c.clk = 60;
            c.store(&mut b, 0x0d, 0x7f);
            for clk in 61..100 {
                c.process_alarms(&mut b, clk);
            }
            assert!(c.irq_enabled, "model {model}: a mask write leaves the line up");
            c.clk = 100;
            let icr = c.read(&mut b, 0x0d);
            assert_eq!(icr & 0x81, 0x81, "model {model}: IR and TA still set, ICR {icr:02x}");
            assert!(!c.irq_enabled, "model {model}: the read releases the line");
        }
    }

    /// The old 6526 raises its line one cycle after the 6526A.
    #[test]
    fn the_old_cia_raises_one_cycle_later() {
        let at = |model| {
            let (mut c, mut b) = fresh_c64(model);
            c.clk = 10;
            c.store(&mut b, 0x0d, 0x81);
            c.store(&mut b, 0x04, 0x10);
            c.store(&mut b, 0x05, 0x00);
            c.store(&mut b, 0x0e, 0x19);
            for clk in 11..60 {
                c.process_alarms(&mut b, clk);
            }
            c.irq_events.iter().find(|e| e.0).unwrap().1
        };
        assert_eq!(at(CIA_MODEL_6526), at(CIA_MODEL_6526A) + 1);
    }

    /// The BCD TOD comes up stopped at 1:00:00.0, starts on a tenths write, and ticks a
    /// tenth every five mains periods at 50 Hz.
    #[test]
    fn the_bcd_tod_ticks_on_the_mains() {
        let (mut c, mut b) = fresh_c64(CIA_MODEL_6526);
        assert_eq!(c.peek(0x0b), 0x01, "VICE resets HR to 1");
        c.clk = 1;
        c.store(&mut b, 0x0e, 0x80); // 50 Hz
        c.store(&mut b, 0x0b, 0x00);
        c.store(&mut b, 0x08, 0x00); // start
        let second = 985_248u64;
        for clk in 2..=second + 10 {
            c.process_alarms(&mut b, clk);
        }
        c.clk = second + 10;
        assert_eq!(c.read(&mut b, 0x0b), 0x00);
        assert_eq!(c.read(&mut b, 0x0a), 0x00);
        assert_eq!(c.read(&mut b, 0x09), 0x01, "one second");
        assert_eq!(c.read(&mut b, 0x08), 0x00);
    }
}
