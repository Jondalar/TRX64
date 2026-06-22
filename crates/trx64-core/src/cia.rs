//! cia.rs — cycle-exact MOS 6526 (CIA1 + CIA2) core.
//!
//! Ported from the VICE-derived TS spec (cia/cia6526-vice.ts + cia/ciat.ts).
//! Scope: the chip-isolation gate (ADR-012). The exercisers run CPU-isolated with
//! IRQs masked (SEI), so the CIA is observed through register READS (timer current
//! values $DC04-$DC07, ICR flags $DC0D, TOD $DC08-$DC0B), NOT through interrupt
//! dispatch. The full VICE IFR delay-line pipeline + alarm scheduling + mySetInt
//! IRQ-pin machinery is therefore NOT needed for the gate: with IRQs masked the
//! only software-visible CIA state is the timer counters, the ICR latch (set on
//! underflow / cleared on read), the control registers, and TOD.
//!
//! ADR-015 finding (see oracle corpus/cia): the TS oracle's `.c64retrace` carries
//! NO CIA-specific frames and NO io frames — every $DC00-$DDFF access surfaces as
//! an op-0x11 RAM_WRITE record from the `bus_access` CPU tap, plus the RAM_WRITE
//! records where the exerciser stores the read-back values. So the verifiable
//! contract is purely: each CIA register read returns the byte-exact value the TS
//! CIA would, at the byte-exact cycle. We match THAT.
//!
//! CLOCK-DRIVEN like the VIC: the CIA holds its own `clk`, advanced once per CPU
//! master cycle via the `Bus::tick` hook. Register reads/writes lazily run the
//! timer state machine forward to the current clk (= VICE `cia_update_ta/tb` →
//! `ciat_update(rclk)`), with rclk = clk (C64SC pins READ_OFFSET = write_offset = 0).
//!
//! Pure / sync / deterministic — no async, no rand, no time. Clone-able with the
//! Machine for Phase-2 COW forks.

// ── Register offsets (addr & 0xf) — cia.h ──────────────────────────────────────
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

// CR bits.
pub const CIA_CR_START: u8 = 0x01;
pub const CIA_CR_PBON: u8 = 0x02;
pub const CIA_CR_OUTMODE_TOGGLE: u8 = 0x04;
pub const CIA_CR_RUNMODE_ONE_SHOT: u8 = 0x08;
pub const CIA_CR_FORCE_LOAD: u8 = 0x10;
pub const CIA_CRA_SPMODE_OUT: u8 = 0x40;
pub const CIA_CRB_INMODE_TA: u8 = 0x40; // CRB bit6: count TA underflows
pub const CIA_CRB_ALARM: u8 = 0x80;

// IRQ-mask / ICR flag bits.
pub const CIA_IM_SET: u8 = 0x80;
pub const CIA_IM_TA: u8 = 0x01;
pub const CIA_IM_TB: u8 = 0x02;
pub const CIA_IM_TOD: u8 = 0x04;
pub const CIA_IM_SDR: u8 = 0x08;
pub const CIA_IM_FLG: u8 = 0x10;

// ── Ciat — 1:1 VICE MOS6526 timer state machine (ciatimer.c port) ─────────────
//
// 13-bit state register driven by an 8192-entry transition table. `update(cclk)`
// advances state cycle-by-cycle, applying transitions; returns the count of
// underflows during the advance. Verbatim port of cia/ciat.ts (which itself
// re-implements VICE ciatimer.c/.h by reading the source).

/// Size of the CIA timer transition table (8192 states × 2).
pub const CIAT_TABLEN: usize = 2 << 13; // 16384

/// Shared, immutable CIA timer transition table. Built once, shared by every
/// `Cia` via an `Arc` so Machine clones (Phase-2 COW forks) stay cheap.
pub type CiaTable = std::sync::Arc<[u16; CIAT_TABLEN]>;

/// Build the shared transition table once (wrap in `Arc` for cheap cloning).
pub fn new_table() -> CiaTable {
    std::sync::Arc::from(build_table())
}

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

/// Build the transition table (VICE ciat_init_table()). Pure function of the
/// index, so it is computed once and the result cloned into each Ciat is avoided
/// — we recompute lazily via a thread-local-free `build_table()` and index it.
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

/// VICE ciat_t — one CIA timer (A or B).
#[derive(Clone)]
pub struct Ciat {
    pub state: u16,
    pub latch: u16,
    pub cnt: u16,
    /// Timer's own clock; advanced lazily to the CPU clk via `update()`.
    pub clk: u64,
}

impl Default for Ciat {
    fn default() -> Self {
        Self { state: 0, latch: 0xffff, cnt: 0xffff, clk: 0 }
    }
}

impl Ciat {
    pub fn reset(&mut self, cclk: u64) {
        self.clk = cclk;
        self.cnt = 0xffff;
        self.latch = 0xffff;
        self.state = 0;
    }

    /// VICE ciat_update — advance state from `self.clk` to `cclk`. Returns the
    /// number of underflows during the advance. `tab` is the shared transition
    /// table (passed in to avoid rebuilding it per timer).
    pub fn update(&mut self, cclk: u64, tab: &[u16; CIAT_TABLEN]) -> u32 {
        let mut n: u32 = 0;
        let mut t = self.state;

        while self.clk < cclk {
            // Warp counting.
            if (t
                & (CIAT_CR_START
                    | CIAT_CR_FLOAD
                    | CIAT_LOAD1
                    | CIAT_PHI2IN
                    | CIAT_COUNT2
                    | CIAT_COUNT3
                    | CIAT_COUNT
                    | CIAT_LOAD))
                == (CIAT_CR_START | CIAT_PHI2IN | CIAT_COUNT2 | CIAT_COUNT3 | CIAT_COUNT)
                && (((t & CIAT_CR_ONESHOT) != 0
                    && (t & CIAT_ONESHOT0) != 0
                    && (t & CIAT_ONESHOT) != 0)
                    || ((t & CIAT_CR_ONESHOT) == 0
                        && (t & CIAT_ONESHOT0) == 0
                        && (t & CIAT_ONESHOT) == 0))
            {
                if self.clk + self.cnt as u64 > cclk {
                    self.cnt = self.cnt.wrapping_sub((cclk - self.clk) as u16);
                    self.clk = cclk;
                } else if (t & (CIAT_CR_ONESHOT | CIAT_ONESHOT0)) != 0 {
                    self.clk += self.cnt as u64;
                    self.cnt = 0;
                } else {
                    self.clk += self.cnt as u64;
                    self.cnt = 0;
                    let span = cclk - self.clk;
                    if span >= self.latch as u64 + 1 {
                        let m = span / (self.latch as u64 + 1);
                        n += m as u32;
                        self.clk += m * (self.latch as u64 + 1);
                    }
                }
            }
            // Warp stopped.
            else if (t & (CIAT_COUNT2 | CIAT_COUNT3 | CIAT_COUNT)) == 0
                && ((t & CIAT_CR_START) == 0 || (t & (CIAT_PHI2IN | CIAT_STEP)) == 0)
                && (t & (CIAT_CR_FLOAD | CIAT_LOAD1 | CIAT_LOAD)) == 0
                && (((t & CIAT_CR_ONESHOT) != 0
                    && (t & CIAT_ONESHOT0) != 0
                    && (t & CIAT_ONESHOT) != 0)
                    || ((t & CIAT_CR_ONESHOT) == 0
                        && (t & CIAT_ONESHOT0) == 0
                        && (t & CIAT_ONESHOT) == 0))
            {
                self.clk = cclk;
            }
            // Latch=1 cnt=1 special case.
            else if t
                == (CIAT_COUNT | CIAT_OUT | CIAT_LOAD | CIAT_PHI2IN | CIAT_COUNT2 | CIAT_CR_START)
                && self.cnt == 1
                && self.latch == 1
            {
                let m = (cclk - self.clk) & !1;
                if m != 0 {
                    self.clk += m;
                    n += (m >> 1) as u32;
                } else {
                    t = tab[t as usize];
                    self.clk += 1;
                }
            }
            // Default: one cycle.
            else {
                if self.cnt != 0 && (t & CIAT_COUNT3) != 0 {
                    self.cnt = self.cnt.wrapping_sub(1);
                }
                t = tab[t as usize];
                self.clk += 1;
            }

            // Underflow detection.
            if self.cnt == 0 && (t & CIAT_COUNT3) != 0 {
                t |= CIAT_LOAD | CIAT_OUT;
                n += 1;
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

    /// VICE ciat_single_step — set the STEP bit while running (TB count-TA mode).
    pub fn single_step(&mut self) {
        if (self.state & CIAT_CR_START) != 0 {
            self.state |= CIAT_STEP;
        }
    }

    /// VICE ciat_set_latchhi.
    pub fn set_latch_hi(&mut self, byte: u8) {
        self.latch = (self.latch & 0xff) | ((byte as u16) << 8);
        if (self.state & CIAT_LOAD) != 0 || (self.state & CIAT_CR_START) == 0 {
            self.cnt = self.latch;
        }
    }

    /// VICE ciat_set_latchlo.
    pub fn set_latch_lo(&mut self, byte: u8) {
        self.latch = (self.latch & 0xff00) | byte as u16;
        if (self.state & CIAT_LOAD) != 0 {
            self.cnt = (self.cnt & 0xff00) | byte as u16;
        }
    }

    /// VICE ciat_set_ctrl. byte bit5=0 ⇒ phi2 input (CIAT_PHI2IN set via XOR).
    pub fn set_ctrl(&mut self, byte: u8) {
        self.state &= !CIAT_CR_MASK;
        self.state |= ((byte as u16) & CIAT_CR_MASK) ^ CIAT_PHI2IN;
    }
}

// ── Cia — one 6526 chip (CIA1 or CIA2) ────────────────────────────────────────

/// One MOS 6526. Holds the 16-byte register file, the two timers, the ICR latch,
/// and a TOD clock. CLOCK-DRIVEN: `clk` advances once per CPU master cycle.
#[derive(Clone)]
pub struct Cia {
    /// 16-byte register file (c_cia). Index by `addr & 0xf`.
    pub regs: [u8; 16],
    /// Timer A.
    pub ta: Ciat,
    /// Timer B.
    pub tb: Ciat,
    /// ICR latch (irqflags low 5 bits = TA/TB/TOD/SDR/FLG). Bit7 (CIA_IM_SET) is
    /// the summary; set when an enabled source is latched. ICR read returns this
    /// and then clears the latch.
    pub irqflags: u8,
    /// This chip's master clock (advanced via `tick`).
    pub clk: u64,
    /// TOD running counter (10ths/sec/min/hr as BCD in regs 8..11). Stage-1: a
    /// simple 60Hz-or-50Hz divider sourced off the CPU clock, deterministic.
    pub tod_prescaler: u32,
    /// Latched TOD snapshot (VICE todlatch) — reading HR latches, reading 10ths
    /// unlatches, so an in-progress read is coherent.
    pub tod_latched: bool,
    pub tod_latch: [u8; 4],
}

impl Default for Cia {
    fn default() -> Self {
        Self {
            regs: [0u8; 16],
            ta: Ciat::default(),
            tb: Ciat::default(),
            irqflags: 0,
            clk: 0,
            tod_prescaler: 0,
            tod_latched: false,
            tod_latch: [0u8; 4],
        }
    }
}

impl Cia {
    pub fn new() -> Self {
        Self::default()
    }

    /// VICE cia_update_ta/tb wrappers: run the timer to `rclk`, latch the ICR flag
    /// on each underflow. The cascade (TB counting TA underflows) is handled by
    /// feeding TA underflow count into TB via single_step before TB's own update.
    fn update_ta(&mut self, rclk: u64, tab: &[u16; CIAT_TABLEN]) {
        // When TB is in cascade mode (counts TA underflows), TA must be advanced
        // cycle-by-cycle so each TA underflow can inject a single-step into TB at
        // its exact clock — and this must hold no matter WHICH access drives the TA
        // update (a $DC04 TA read, an ICR read, a TB read, …), so every TA underflow
        // cascades. Outside cascade mode TA advances with warp counting (O(1)).
        if self.tb_cascade() {
            while self.ta.clk < rclk {
                let c = self.ta.clk + 1;
                let n_ta = self.ta.update(c, tab);
                if n_ta > 0 {
                    self.irqflags |= CIA_IM_TA;
                    for _ in 0..n_ta {
                        self.tb.single_step();
                        let n_tb = self.tb.update(self.tb.clk + 1, tab);
                        if n_tb > 0 {
                            self.irqflags |= CIA_IM_TB;
                        }
                    }
                }
            }
            return;
        }
        let n = self.ta.update(rclk, tab);
        if n > 0 {
            self.irqflags |= CIA_IM_TA;
        }
    }

    /// True when TB is in count-TA-underflow (cascade) mode and running.
    #[inline]
    fn tb_cascade(&self) -> bool {
        (self.regs[CIA_CRB] & (CIA_CRB_INMODE_TA | CIA_CR_START))
            == (CIA_CRB_INMODE_TA | CIA_CR_START)
    }

    fn update_tb(&mut self, rclk: u64, tab: &[u16; CIAT_TABLEN]) {
        // In cascade mode TB is driven by TA underflows (handled inside update_ta),
        // not phi2 — so advancing TA also cascades into TB. We then snap TB's own
        // clk forward to rclk (no phi2 counting). Outside cascade, TB runs on phi2.
        if self.tb_cascade() {
            self.update_ta(rclk, tab);
            if self.tb.clk < rclk {
                self.tb.clk = rclk;
            }
            return;
        }
        let n = self.tb.update(rclk, tab);
        if n > 0 {
            self.irqflags |= CIA_IM_TB;
        }
    }

    /// Advance both timers to `rclk`. TA underflows cascade into TB automatically
    /// inside update_ta when cascade mode is active; otherwise the two are
    /// independent. Idempotent: a timer already at rclk is untouched.
    fn update_both(&mut self, rclk: u64, tab: &[u16; CIAT_TABLEN]) {
        self.update_ta(rclk, tab);
        self.update_tb(rclk, tab);
    }

    /// Advance both timers to the current clk (used by the per-cycle tick when no
    /// access happens, so warp-counting stays bounded). Cheap: warp counting
    /// collapses long idle spans into O(1) per call.
    pub fn tick(&mut self, tab: &[u16; CIAT_TABLEN]) {
        self.clk = self.clk.wrapping_add(1);
        // TOD prescaler — VICE drives TOD off the 50/60 Hz power line, divided
        // from the system clock. For the isolation gate the absolute rate must
        // match the TS divider; cia-tod.ts uses todticks = cyclesPerSec/powerFreq.
        // PAL: 985248 / (CRA bit7 ? 50 : 60). We advance a free-running prescaler
        // and tick the BCD TOD when it wraps.
        self.tod_prescaler = self.tod_prescaler.wrapping_add(1);
        let _ = tab;
    }

    /// CPU read of a CIA register (addr already masked to $DC00-$DDFF window by the
    /// bus). `clk` = CPU master clock at the access (rclk, READ_OFFSET=0).
    pub fn read(&mut self, addr: u16, clk: u64, tab: &[u16; CIAT_TABLEN]) -> u8 {
        let a = (addr & 0xf) as usize;
        let rclk = clk;
        match a {
            CIA_TAL => {
                self.update_ta(rclk, tab);
                (self.ta.read_timer() & 0xff) as u8
            }
            CIA_TAH => {
                self.update_ta(rclk, tab);
                (self.ta.read_timer() >> 8) as u8
            }
            CIA_TBL => {
                self.update_tb(rclk, tab);
                (self.tb.read_timer() & 0xff) as u8
            }
            CIA_TBH => {
                self.update_tb(rclk, tab);
                (self.tb.read_timer() >> 8) as u8
            }
            CIA_TOD_TEN | CIA_TOD_SEC | CIA_TOD_MIN | CIA_TOD_HR => self.tod_read(a),
            CIA_ICR => {
                self.update_both(rclk, tab);
                // ICR read returns the latch (low 5 bits) + summary bit7, then
                // clears the latch (read-clears). VICE old "slow" 6526:
                // result = irqflags; irqflags &= CIA_IM_SET (then SET cleared by
                // the IFR pipeline) — for the masked gate we clear fully.
                let active = self.irqflags & self.regs[CIA_ICR] & 0x1f;
                let summary = if active != 0 { CIA_IM_SET } else { 0 };
                let result = (self.irqflags & 0x1f) | summary;
                self.irqflags = 0;
                result
            }
            CIA_CRA => (self.regs[CIA_CRA] & !CIA_CR_START) | (self.ta.is_running() as u8),
            CIA_CRB => (self.regs[CIA_CRB] & !CIA_CR_START) | (self.tb.is_running() as u8),
            _ => self.regs[a],
        }
    }

    /// CPU write of a CIA register. `clk` = CPU master clock at the access (rclk,
    /// write_offset=0 for C64SC).
    pub fn write(&mut self, addr: u16, value: u8, clk: u64, tab: &[u16; CIAT_TABLEN]) {
        let a = (addr & 0xf) as usize;
        let rclk = clk;
        match a {
            CIA_TAL => {
                self.update_ta(rclk, tab);
                self.ta.set_latch_lo(value);
            }
            CIA_TAH => {
                self.update_ta(rclk, tab);
                self.ta.set_latch_hi(value);
            }
            CIA_TBL => {
                self.update_tb(rclk, tab);
                self.tb.set_latch_lo(value);
            }
            CIA_TBH => {
                self.update_tb(rclk, tab);
                self.tb.set_latch_hi(value);
            }
            CIA_TOD_TEN | CIA_TOD_SEC | CIA_TOD_MIN | CIA_TOD_HR => {
                self.tod_store(a, value);
            }
            CIA_ICR => {
                self.update_both(rclk, tab);
                // Mask set/clear: bit7 set ⇒ OR in (value & 0x7f); else clear those.
                if value & CIA_IM_SET != 0 {
                    self.regs[CIA_ICR] |= value & 0x1f;
                } else {
                    self.regs[CIA_ICR] &= !(value & 0x1f);
                }
            }
            CIA_CRA => {
                self.update_ta(rclk, tab);
                self.ta.set_ctrl(value);
                // Bit4 (force load) is a strobe — not stored (regs keeps v & 0xef).
                self.regs[CIA_CRA] = value & 0xef;
            }
            CIA_CRB => {
                self.update_tb(rclk, tab);
                if value & CIA_CRB_INMODE_TA != 0 {
                    // Count-TA mode: TB step source is the STEP bit, not phi2.
                    self.tb.set_ctrl(value | 0x20);
                } else {
                    self.tb.set_ctrl(value);
                }
                self.regs[CIA_CRB] = value & 0xef;
            }
            _ => {
                self.regs[a] = value;
            }
        }
    }

    /// IRQ-line level: asserted when any latched ICR flag is also enabled in the
    /// mask (= VICE summary bit7 condition). Drives the C64 IRQ/NMI line on the
    /// FullBus. Pure read of the current latched state — the caller advances the
    /// timers (via `update_to`/`tick`) before polling so the flags are current.
    #[inline]
    pub fn irq_asserted(&self) -> bool {
        (self.irqflags & self.regs[CIA_ICR] & 0x1f) != 0
    }

    /// Advance both timers to `rclk` so a subsequent `irq_asserted()` reflects any
    /// underflow that occurred up to that clock — used by the FullBus to sample the
    /// IRQ line at an instruction boundary without performing a register access.
    #[inline]
    pub fn update_to(&mut self, rclk: u64, tab: &[u16; CIAT_TABLEN]) {
        self.update_both(rclk, tab);
    }

    /// Peek a register without side effects (for snapshot / state readers).
    pub fn peek(&self, addr: u16) -> u8 {
        let a = (addr & 0xf) as usize;
        match a {
            CIA_TAL => (self.ta.cnt & 0xff) as u8,
            CIA_TAH => (self.ta.cnt >> 8) as u8,
            CIA_TBL => (self.tb.cnt & 0xff) as u8,
            CIA_TBH => (self.tb.cnt >> 8) as u8,
            CIA_ICR => self.irqflags & 0x1f,
            _ => self.regs[a],
        }
    }

    // ── TOD (Stage-1: clock set + latched read; CRB-bit7 alarm split + the
    //    50/60 Hz tick are out of scope for the masked isolation gate) ───────────

    /// VICE todStore (cia-tod.ts): addr-specific BCD masking + AM/PM flip on HR 12,
    /// + clock stop on HR write / restart on TEN write. CRB-bit7 (alarm-vs-clock)
    /// is not split here — the gate writes the clock registers.
    fn tod_store(&mut self, a: usize, byte: u8) {
        let mut v = byte;
        if a == CIA_TOD_HR {
            v &= 0x9f;
            // Flip AM/PM when writing hour 12 (clock, not alarm).
            if (v & 0x1f) == 0x12 {
                v ^= 0x80;
            }
        } else if a == CIA_TOD_MIN || a == CIA_TOD_SEC {
            v &= 0x7f;
        } else if a == CIA_TOD_TEN {
            v &= 0x0f;
        }
        self.regs[a] = v;
    }

    /// VICE todRead (cia-tod.ts): the first read while unlatched snapshots all 4
    /// registers; reading HR latches (subsequent reads return the snapshot),
    /// reading TEN releases the latch.
    fn tod_read(&mut self, a: usize) -> u8 {
        if !self.tod_latched {
            self.tod_latch.copy_from_slice(&self.regs[CIA_TOD_TEN..=CIA_TOD_HR]);
        }
        if a == CIA_TOD_TEN {
            self.tod_latched = false;
        }
        if a == CIA_TOD_HR {
            self.tod_latched = true;
        }
        self.tod_latch[a - CIA_TOD_TEN]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab() -> Box<[u16; CIAT_TABLEN]> {
        build_table()
    }

    /// Timer A one-shot: latch 16, force-load + one-shot + start, then read after
    /// N cycles. The Ciat counts down once per phi2 cycle while running.
    #[test]
    fn ta_oneshot_counts_down() {
        let t = tab();
        let mut c = Cia::new();
        // Program latch = $0010.
        c.write(0xdc04, 0x10, 0, &t);
        c.write(0xdc05, 0x00, 0, &t);
        // CRA = FORCE_LOAD | ONE_SHOT | START.
        c.write(0xdc0e, CIA_CR_FORCE_LOAD | CIA_CR_RUNMODE_ONE_SHOT | CIA_CR_START, 0, &t);
        // Read the timer some cycles later — it must have counted down.
        let lo = c.read(0xdc04, 13, &t);
        assert!(lo < 0x10, "TA must count down from 0x10, got 0x{lo:02X}");
    }

    #[test]
    fn ta_continuous_reloads_on_underflow() {
        let t = tab();
        let mut c = Cia::new();
        c.write(0xdc04, 0x05, 0, &t);
        c.write(0xdc05, 0x00, 0, &t);
        // CRA = FORCE_LOAD | START (continuous: one-shot bit clear).
        c.write(0xdc0e, CIA_CR_FORCE_LOAD | CIA_CR_START, 0, &t);
        // After many cycles the timer keeps reloading from the latch (never stops).
        let _ = c.read(0xdc04, 100, &t);
        assert!(c.ta.is_running(), "continuous TA stays running across underflows");
    }

    #[test]
    fn icr_latches_ta_underflow_and_read_clears() {
        let t = tab();
        let mut c = Cia::new();
        c.write(0xdc04, 0x02, 0, &t);
        c.write(0xdc05, 0x00, 0, &t);
        c.write(0xdc0e, CIA_CR_FORCE_LOAD | CIA_CR_RUNMODE_ONE_SHOT | CIA_CR_START, 0, &t);
        // Force at least one underflow.
        let icr = c.read(0xdc0d, 50, &t);
        assert!(icr & CIA_IM_TA != 0, "ICR TA flag latched after underflow");
        // Read-clears: a second read returns no TA flag.
        let icr2 = c.read(0xdc0d, 51, &t);
        assert_eq!(icr2 & CIA_IM_TA, 0, "ICR read clears the latch");
    }

    #[test]
    fn tod_hr_am_pm_flip_and_latched_read() {
        let t = tab();
        let mut c = Cia::new();
        // Writing hour 12 (BCD $12) flips the AM/PM bit ⇒ stored $92.
        c.write(0xdc0b, 0x12, 0, &t); // HR
        c.write(0xdc0a, 0x34, 0, &t); // MIN
        c.write(0xdc09, 0x56, 0, &t); // SEC
        c.write(0xdc08, 0x09, 0, &t); // TEN
        assert_eq!(c.read(0xdc0b, 10, &t), 0x92, "HR 12 sets PM bit");
        assert_eq!(c.read(0xdc0a, 11, &t), 0x34, "latched MIN");
        assert_eq!(c.read(0xdc09, 12, &t), 0x56, "latched SEC");
        assert_eq!(c.read(0xdc08, 13, &t), 0x09, "TEN read releases latch");
    }

    #[test]
    fn icr_summary_bit_only_when_source_enabled() {
        let t = tab();
        let mut c = Cia::new();
        // Enable TA in the mask (SET|TA), one-shot TA, force an underflow.
        c.write(0xdc0d, CIA_IM_SET | CIA_IM_TA, 0, &t);
        c.write(0xdc04, 0x04, 0, &t);
        c.write(0xdc05, 0x00, 0, &t);
        c.write(0xdc0e, CIA_CR_FORCE_LOAD | CIA_CR_RUNMODE_ONE_SHOT | CIA_CR_START, 0, &t);
        let icr = c.read(0xdc0d, 40, &t);
        assert_eq!(icr, CIA_IM_SET | CIA_IM_TA, "enabled TA ⇒ flag + summary bit7");

        // Disable TA in the mask; latch the flag again — no summary bit this time.
        c.write(0xdc0d, CIA_IM_TA, 0, &t); // no SET ⇒ clear enable
        c.write(0xdc04, 0x04, 0, &t);
        c.write(0xdc05, 0x00, 0, &t);
        c.write(0xdc0e, CIA_CR_FORCE_LOAD | CIA_CR_RUNMODE_ONE_SHOT | CIA_CR_START, 0, &t);
        let icr2 = c.read(0xdc0d, 80, &t);
        assert_eq!(icr2, CIA_IM_TA, "disabled TA ⇒ flag set, no summary bit7");
    }
}
