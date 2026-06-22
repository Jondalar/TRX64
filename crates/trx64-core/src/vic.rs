//! vic.rs — cycle-exact VIC-II (6569 PAL) raster/badline/BA core.
//!
//! Ported from the VICE-derived TS spec (vic/vic-ii-vice.ts + vic/literal/
//! vicii-cycle.c port + vic/badline-fetch.ts). The VIC is CLOCK-DRIVEN: it is
//! ticked exactly once per CPU master cycle (the CPU is the clock master in this
//! runtime, cpu.rs `tick()`), so the raster counter, badline detection, BA-low
//! cycle-stealing window, and sprite DMA all advance off the CPU clock
//! regardless of what the CPU executes — exactly as a CPU-isolated VIC exerciser
//! (SEI; minimal loop) demands.
//!
//! Scope (Stage-1 isolation gate, ADR-012): raster timing, badlines, BA-low,
//! sprite DMA accounting, and the $D000-$D02E register file. This is the
//! cycle-exact timing skeleton at the center of the literal viciisc port. Pixel
//! draw-cycle / framebuffer generation is intentionally NOT ported here: none of
//! it surfaces on the `.c64retrace` VIC trace domain (the vic channel is a
//! RESERVED channel with NO live producer in the TS oracle — verified
//! empirically: a vic-domain trace over a full PAL frame yields zero records).
//! So the verifiable contract is: the VIC advances deterministically per cycle
//! and the trace stays byte-exact. Internal timing is unit-tested against the
//! VICE PAL constants below.
//!
//! Pure / sync / deterministic — no async, no rand, no time. Clone-able with the
//! Machine for Phase-2 COW forks.

// ── PAL 6569 timing constants (vicii-timing.c / vic-ii-vice.ts) ────────────────

/// PAL: 63 cycles per raster line (0..62).
pub const PAL_CYCLES_PER_LINE: u16 = 63;
/// PAL: 312 raster lines (0..311).
pub const PAL_SCREEN_HEIGHT: u16 = 312;
/// First line on which a badline can occur (vicii.first_dma_line = 0x30).
pub const FIRST_DMA_LINE: u16 = 0x30;
/// Last line on which a badline can occur (vicii.last_dma_line = 0xf7).
pub const LAST_DMA_LINE: u16 = 0xf7;

/// Number of hardware sprites.
pub const NUM_SPRITES: usize = 8;

// ── Register offsets ($D000 + n), masked to 6 bits in the $D000-$D3FF window ───

pub const R_CTRL1: u8 = 0x11; // $D011 — YSCROLL(0..2) RSEL DEN BMM ECM RST8
pub const R_RASTER: u8 = 0x12; // $D012 — raster compare low 8 bits
pub const R_SP_ENABLE: u8 = 0x15; // $D015 — sprite enable
pub const R_CTRL2: u8 = 0x16; // $D016 — XSCROLL CSEL MCM
pub const R_SP_Y_EXP: u8 = 0x17; // $D017 — sprite Y expand
pub const R_MEM_PTR: u8 = 0x18; // $D018 — screen / char base
pub const R_IRQ_STATUS: u8 = 0x19; // $D019 — IRQ latch (write 1 to ack)
pub const R_IRQ_MASK: u8 = 0x1a; // $D01A — IRQ enable mask

// IRQ source bits ($D019 / $D01A).
pub const IRQ_RASTER: u8 = 0x01;
pub const IRQ_SUMMARY: u8 = 0x80;

/// Cycle-exact VIC-II state. Field names follow the VICE-derived TS port.
#[derive(Clone)]
pub struct VicII {
    /// $D000-$D02E register file (47 regs; we store 0x40 for a flat index).
    pub regs: [u8; 0x40],
    /// Current raster line (0..311 PAL). VICE: vicii.raster_line.
    pub raster_line: u16,
    /// Cycle within the current line (0..62 PAL). VICE: vicii.raster_cycle.
    pub raster_cycle: u16,
    /// Latched raster IRQ compare line (9-bit: $D012 + RST8 from $D011 bit7).
    pub raster_irq_line: u16,
    /// Sticky allow-bad-lines flag — set once DEN seen on first_dma_line within
    /// a frame; gates badline detection for the rest of the frame. VICE:
    /// vicii.allow_bad_lines.
    pub allow_bad_lines: bool,
    /// Current line is a badline (matrix DMA). VICE: vicii.bad_line.
    pub bad_line: bool,
    /// BA line is LOW this cycle (VIC stealing the bus from the CPU). VICE:
    /// maincpu_ba_low_flags. True during the badline c-access window and during
    /// sprite-DMA p/s-access windows.
    pub ba_low: bool,
    /// Per-sprite DMA active flags (bit i = sprite i). VICE: vicii.sprite_dma.
    pub sprite_dma: u8,
    /// Latched IRQ output line state (level). VICE: irq asserted when
    /// (regs[$D019] & regs[$D01A] & 0x0f) != 0.
    pub irq_line: bool,
    /// Frame counter (advances each time raster_line wraps 311 -> 0). Diagnostic.
    pub frame: u64,
    /// VICE maincpu_ba_low_flags (VICII bit): set when the last per-cycle tick
    /// returned BA low; consumed by the CPU's check_ba_before_read stall before
    /// the next read, then cleared.
    pub ba_low_flag: bool,
    /// VICE vicii.raster_irq_triggered — edge-trigger latch so the raster IRQ
    /// fires once per match (raster_line == raster_irq_line), not every cycle.
    pub raster_irq_triggered: bool,
}

impl Default for VicII {
    fn default() -> Self {
        Self {
            regs: [0u8; 0x40],
            raster_line: 0,
            raster_cycle: 0,
            raster_irq_line: 0,
            allow_bad_lines: false,
            bad_line: false,
            ba_low: false,
            sprite_dma: 0,
            irq_line: false,
            frame: 0,
            ba_low_flag: false,
            raster_irq_triggered: false,
        }
    }
}

/// A register write the VIC observed this cycle, surfaced to the Observer so a
/// trace sink can (in principle) emit a VIC_REG_WRITE frame. `kind` mirrors the
/// TS VIC_KIND_CODE { raster:1, mode:2, irq:3, badline:4 }; the value is the
/// byte written (or, for kind=badline, 1/0). NOTE: the TS oracle's vic channel
/// has no live producer, so these are not emitted into the gate trace — the
/// hook exists for format completeness + future integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VicRegKind {
    Raster = 1,
    Mode = 2,
    Irq = 3,
    Badline = 4,
}

impl VicII {
    pub fn new() -> Self {
        Self::default()
    }

    /// Classify a $D000-$D02E register offset into a VIC trace kind, matching the
    /// TS producer's `kind` tagging. $D012/$D011 → raster, $D016/$D018 → mode,
    /// $D019/$D01A → irq. Returns None for registers the TS producer would not
    /// tag (sprite coords/colors etc.).
    pub fn reg_kind(offset: u8) -> Option<VicRegKind> {
        match offset {
            R_RASTER | R_CTRL1 => Some(VicRegKind::Raster),
            R_CTRL2 | R_MEM_PTR => Some(VicRegKind::Mode),
            R_IRQ_STATUS | R_IRQ_MASK => Some(VicRegKind::Irq),
            _ => None,
        }
    }

    /// Recompute the latched 9-bit raster IRQ compare line from $D011/$D012.
    #[inline]
    fn update_raster_irq_line(&mut self) {
        let lo = self.regs[R_RASTER as usize] as u16;
        let hi8 = ((self.regs[R_CTRL1 as usize] & 0x80) as u16) << 1; // bit7 -> bit8
        self.raster_irq_line = lo | hi8;
    }

    /// YSCROLL (D011 low 3 bits) — the badline match offset.
    #[inline]
    fn ysmooth(&self) -> u16 {
        (self.regs[R_CTRL1 as usize] & 0x07) as u16
    }

    /// DEN (display enable) — D011 bit 4.
    #[inline]
    fn den(&self) -> bool {
        self.regs[R_CTRL1 as usize] & 0x10 != 0
    }

    /// VICE badline predicate: (raster_line & 7)==ysmooth && allow_bad_lines &&
    /// first_dma_line <= line <= last_dma_line.
    #[inline]
    fn compute_badline(&self) -> bool {
        if !self.allow_bad_lines {
            return false;
        }
        if self.raster_line < FIRST_DMA_LINE || self.raster_line > LAST_DMA_LINE {
            return false;
        }
        (self.raster_line & 7) == self.ysmooth()
    }

    /// Store to a VIC register ($D000-$D3FF mirrors every 0x40). Mirrors VICE
    /// vicii_store side effects we need for cycle-exact timing:
    ///  - $D011: RST8 + DEN + YSCROLL recomputed; raster IRQ line relatched.
    ///  - $D012: raster IRQ line relatched.
    ///  - $D019: writing 1-bits ACKs (clears) those IRQ latch bits.
    ///  - $D01A: IRQ mask; recompute the output line.
    pub fn write_reg(&mut self, offset: u8, value: u8) {
        let off = (offset & 0x3f) as usize;
        match off as u8 {
            R_IRQ_STATUS => {
                // $D019: write-1-to-clear the latched IRQ source bits.
                let latch = self.regs[R_IRQ_STATUS as usize];
                let cleared = latch & !(value & 0x0f);
                self.regs[R_IRQ_STATUS as usize] = cleared;
                self.update_irq_line();
                return;
            }
            R_IRQ_MASK => {
                self.regs[R_IRQ_MASK as usize] = value & 0x0f;
                self.update_irq_line();
                return;
            }
            R_CTRL1 => {
                self.regs[R_CTRL1 as usize] = value;
                self.update_raster_irq_line();
                return;
            }
            R_RASTER => {
                self.regs[R_RASTER as usize] = value;
                self.update_raster_irq_line();
                return;
            }
            _ => {}
        }
        self.regs[off] = value;
    }

    /// Read a VIC register ($D000-$D3FF). Side-effecting reads:
    ///  - $D011: bit7 reflects live raster_line bit 8.
    ///  - $D012: live raster_line low 8 bits.
    ///  - $D019: latch | 0x70 (unused bits read 1) and summary bit.
    pub fn read_reg(&self, offset: u8) -> u8 {
        let off = offset & 0x3f;
        match off {
            R_CTRL1 => (self.regs[R_CTRL1 as usize] & 0x7f) | (((self.raster_line >> 8) as u8 & 1) << 7),
            R_RASTER => (self.raster_line & 0xff) as u8,
            R_IRQ_STATUS => {
                let latch = self.regs[R_IRQ_STATUS as usize] & 0x0f;
                let summary = if (latch & self.regs[R_IRQ_MASK as usize] & 0x0f) != 0 {
                    IRQ_SUMMARY
                } else {
                    0
                };
                latch | 0x70 | summary
            }
            R_IRQ_MASK => self.regs[R_IRQ_MASK as usize] | 0xf0,
            _ => self.regs[off as usize],
        }
    }

    /// Recompute the IRQ output line (level): asserted when any enabled latch bit
    /// is set. Updates the summary bit in $D019.
    fn update_irq_line(&mut self) {
        let active = self.regs[R_IRQ_STATUS as usize] & self.regs[R_IRQ_MASK as usize] & 0x0f;
        self.irq_line = active != 0;
        if self.irq_line {
            self.regs[R_IRQ_STATUS as usize] |= IRQ_SUMMARY;
        } else {
            self.regs[R_IRQ_STATUS as usize] &= !IRQ_SUMMARY;
        }
    }

    /// Latch a raster IRQ: set $D019 bit0, refresh the output line.
    fn raise_raster_irq(&mut self) {
        self.regs[R_IRQ_STATUS as usize] |= IRQ_RASTER;
        self.update_irq_line();
    }

    /// True while the VIC is stealing the bus on a badline c-access window.
    /// VICE: BA goes low 3 cycles before the first c-access and stays low for the
    /// 40-column matrix fetch. PAL badline BA-low window is cycles 12..54 of the
    /// line (vicii-fetch.c / Bauer "VIC Article"): BA low from cycle 12, c-access
    /// 15..54. We model the BA-low window as 12..=54 on a badline.
    #[inline]
    fn badline_ba_low(&self) -> bool {
        self.bad_line && (12..=54).contains(&self.raster_cycle)
    }

    /// Advance the VIC by exactly one master cycle (= VICE vicii_cycle()).
    /// Returns BA-low for this cycle (1 = VIC owns the bus). Order mirrors VICE
    /// vicii-cycle.c precisely:
    ///   1. raster_cycle++ (wrap at cycles_per_line) — the cycle ADVANCES FIRST.
    ///   2. at raster_cycle == 0 (= PAL_CYCLE(1)): start-of-line → raster_line++
    ///      (and frame wrap), allow_bad_lines reset at frame start.
    ///   3. allow_bad_lines set if DEN seen on first_dma_line (checked any cycle
    ///      while !allow_bad_lines).
    ///   4. check_badline() (only if allow_bad_lines).
    ///   5. edge-triggered raster IRQ when raster_line == raster_irq_line.
    ///   6. sprite-DMA turn-on at the sprite-dma-check cycles.
    ///   7. BA-low = bad_line && in the BaFetch window (raster_cycle 12..54) OR a
    ///      sprite-DMA fetch window.
    pub fn tick(&mut self) -> bool {
        // 1. Advance the raster cycle FIRST (VICE: next_vicii_cycle at top).
        self.raster_cycle += 1;
        let mut line_wrapped = false;
        if self.raster_cycle >= PAL_CYCLES_PER_LINE {
            self.raster_cycle = 0;
        }

        // 2. Start of line at raster_cycle == 0 (= VICII_PAL_CYCLE(1)): the raster
        //    line advances. VICE does end_of_line/start_of_line + raster_line++.
        if self.raster_cycle == 0 {
            self.raster_line += 1;
            if self.raster_line >= PAL_SCREEN_HEIGHT {
                self.raster_line = 0;
                self.frame += 1;
            }
            line_wrapped = true;
            // allow_bad_lines is cleared at frame start (raster_line 0).
            if self.raster_line == 0 {
                self.allow_bad_lines = false;
            }
        }
        let _ = line_wrapped;

        // 3. DEN seen on first_dma_line latches allow_bad_lines (sticky for the
        //    frame). VICE checks this every cycle while !allow_bad_lines.
        if self.raster_line == FIRST_DMA_LINE && !self.allow_bad_lines && self.den() {
            self.allow_bad_lines = true;
        }

        // 4. Badline condition (only meaningful while allow_bad_lines).
        self.bad_line = self.compute_badline();

        // 5. Edge-triggered raster compare IRQ.
        if self.raster_line == self.raster_irq_line {
            if !self.raster_irq_triggered {
                self.raise_raster_irq();
                self.raster_irq_triggered = true;
            }
        } else {
            self.raster_irq_triggered = false;
        }

        // 6. Sprite-DMA turn-on (VICE check_sprite_dma at cycles 55 & 56 PAL).
        if self.raster_cycle == 55 || self.raster_cycle == 56 {
            self.update_sprite_dma();
        }

        // 7. BA-low derivation for this cycle.
        self.ba_low = self.badline_ba_low() || self.sprite_ba_low();
        let ba = self.ba_low;
        if ba {
            self.ba_low_flag = true;
        }
        ba
    }

    /// VICE vicii_steal_cycles (vicii-cycle.c:628) / check_ba(): if BA is low,
    /// stall the CPU by ticking the VIC `do { tick(); } while (ba_low)` and return
    /// the number of stolen cycles. Clears the BA-low flag. A safety cap (64)
    /// mirrors the TS guard. Returns 0 if BA was not low (no stall).
    pub fn steal_cycles(&mut self) -> u32 {
        if !self.ba_low_flag {
            return 0;
        }
        let mut stolen: u32 = 0;
        // VICE order: clk advances BEFORE vicii_cycle. Each tick() here IS one
        // stolen cycle that also advances the VIC; loop while BA stays low.
        loop {
            let ba = self.tick();
            stolen += 1;
            if !ba || stolen > 64 {
                break;
            }
        }
        self.ba_low_flag = false;
        stolen
    }

    /// Turn on sprite DMA for enabled sprites whose Y matches the current line.
    fn update_sprite_dma(&mut self) {
        let enable = self.regs[R_SP_ENABLE as usize];
        for i in 0..NUM_SPRITES {
            let b = 1u8 << i;
            let y = self.regs[i * 2 + 1]; // $D001,$D003,... sprite Y
            if (enable & b) != 0 && y as u16 == (self.raster_line & 0xff) && (self.sprite_dma & b) == 0 {
                self.sprite_dma |= b;
            }
        }
        // Sprites whose DMA completed are turned off elsewhere in the full model;
        // for the isolated timing gate we keep DMA active while enabled+matching,
        // and clear it when the sprite is disabled.
        self.sprite_dma &= enable;
    }

    /// BA-low window for sprite DMA. VICE: a fixed 3-cycle pointer-fetch window
    /// (p-access, cycles 58..60 PAL) plus 2 cycles per active sprite (s-access).
    /// We model BA low during the sprite-fetch tail of the line (cycles 55..62)
    /// when any sprite DMA is active — sufficient for the deterministic timing
    /// the isolated gate observes.
    #[inline]
    fn sprite_ba_low(&self) -> bool {
        self.sprite_dma != 0 && (55..=62).contains(&self.raster_cycle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tick the VIC `n` cycles from a fresh state.
    fn tick_n(v: &mut VicII, n: usize) {
        for _ in 0..n {
            v.tick();
        }
    }

    #[test]
    fn raster_advances_63_cycles_per_line_pal() {
        let mut v = VicII::new();
        assert_eq!(v.raster_line, 0);
        assert_eq!(v.raster_cycle, 0);
        tick_n(&mut v, 63);
        assert_eq!(v.raster_line, 1, "one full line after 63 cycles");
        assert_eq!(v.raster_cycle, 0);
    }

    #[test]
    fn frame_wraps_at_312_lines() {
        let mut v = VicII::new();
        // 312 lines * 63 cycles = 19656 cycles per PAL frame.
        tick_n(&mut v, 63 * 312);
        assert_eq!(v.raster_line, 0, "raster wraps to 0 after 312 lines");
        assert_eq!(v.frame, 1);
        assert_eq!(v.raster_cycle, 0);
    }

    #[test]
    fn pal_frame_is_19656_cycles() {
        // Sanity: cycles/frame = 63 * 312.
        assert_eq!(PAL_CYCLES_PER_LINE as u32 * PAL_SCREEN_HEIGHT as u32, 19656);
    }

    #[test]
    fn no_badline_without_den() {
        let mut v = VicII::new();
        // DEN=0 (D011 default 0): allow_bad_lines never set, no badlines.
        // Advance to a line in the DMA range with matching ysmooth.
        tick_n(&mut v, 63 * (FIRST_DMA_LINE as usize)); // now at line 0x30, cyc 0
        // Trigger the start-of-line work by ticking one cycle.
        v.tick();
        assert!(!v.allow_bad_lines, "DEN=0 => allow_bad_lines stays false");
        assert!(!v.bad_line);
    }

    #[test]
    fn badline_on_first_dma_line_with_den_and_ysmooth_zero() {
        let mut v = VicII::new();
        // Set DEN=1, YSCROLL=0 (D011 = 0x10) BEFORE reaching first_dma_line.
        v.write_reg(R_CTRL1, 0x10);
        // Run to the start of line 0x30.
        tick_n(&mut v, 63 * (FIRST_DMA_LINE as usize));
        // One tick performs the start-of-line work for line 0x30.
        v.tick();
        assert!(v.allow_bad_lines, "DEN seen on first_dma_line sets allow_bad_lines");
        assert!(v.bad_line, "line 0x30, ysmooth 0, (0x30 & 7)==0 => badline");
    }

    #[test]
    fn badline_ba_low_window_present_on_badline() {
        let mut v = VicII::new();
        v.write_reg(R_CTRL1, 0x10); // DEN=1, YSCROLL=0
        // Tick until we are at a confirmed badline (line 0x30, ysmooth 0). Walk a
        // full line and record the post-increment raster_cycle for each BA-low.
        // First, advance into line 0x30.
        while !(v.raster_line == FIRST_DMA_LINE && v.raster_cycle == 0) {
            v.tick();
        }
        let mut ba_low_cycles = Vec::new();
        for _ in 0..PAL_CYCLES_PER_LINE {
            let ba = v.tick();
            if ba {
                ba_low_cycles.push(v.raster_cycle);
            }
        }
        assert!(ba_low_cycles.contains(&12), "BA low at cycle 12 on a badline");
        assert!(ba_low_cycles.contains(&54), "BA low through cycle 54");
        assert!(!ba_low_cycles.contains(&0), "BA high at the start of the line");
        assert!(!ba_low_cycles.contains(&11), "BA high just before the window");
        assert!(!ba_low_cycles.contains(&55), "BA high just after the matrix window");
    }

    #[test]
    fn no_ba_low_on_non_badline() {
        let mut v = VicII::new();
        // No DEN -> no badlines anywhere -> BA never low (no sprites either).
        let mut any_ba = false;
        for _ in 0..(63 * 64) {
            if v.tick() {
                any_ba = true;
            }
        }
        assert!(!any_ba, "no badlines + no sprites => BA stays high");
    }

    #[test]
    fn raster_irq_latches_on_match() {
        let mut v = VicII::new();
        // Enable raster IRQ, compare line = 5.
        v.write_reg(R_IRQ_MASK, 0x01);
        v.write_reg(R_RASTER, 5);
        assert_eq!(v.raster_irq_line, 5);
        // Run to start of line 5.
        tick_n(&mut v, 63 * 5);
        v.tick(); // start-of-line work for line 5
        assert!(v.regs[R_IRQ_STATUS as usize] & IRQ_RASTER != 0, "raster IRQ latched");
        assert!(v.irq_line, "IRQ line asserted (enabled + latched)");
        // $D019 read shows the summary bit.
        assert!(v.read_reg(R_IRQ_STATUS) & IRQ_SUMMARY != 0);
    }

    #[test]
    fn raster_irq_ack_clears_latch() {
        let mut v = VicII::new();
        v.write_reg(R_IRQ_MASK, 0x01);
        v.write_reg(R_RASTER, 5);
        tick_n(&mut v, 63 * 5);
        v.tick();
        assert!(v.irq_line);
        // ACK by writing bit0 to $D019.
        v.write_reg(R_IRQ_STATUS, 0x01);
        assert_eq!(v.regs[R_IRQ_STATUS as usize] & IRQ_RASTER, 0, "latch cleared");
        assert!(!v.irq_line, "IRQ line deasserted after ack");
    }

    #[test]
    fn d011_rst8_extends_raster_irq_line_to_9_bits() {
        let mut v = VicII::new();
        // RST8 (D011 bit7) + D012=0 => compare line 256.
        v.write_reg(R_CTRL1, 0x80);
        v.write_reg(R_RASTER, 0x00);
        assert_eq!(v.raster_irq_line, 256);
    }

    #[test]
    fn d012_read_reflects_live_raster() {
        let mut v = VicII::new();
        tick_n(&mut v, 63 * 10); // line 10
        assert_eq!(v.read_reg(R_RASTER), 10);
    }

    #[test]
    fn sprite_dma_turns_on_for_enabled_sprite_at_matching_y() {
        let mut v = VicII::new();
        // Enable sprite 0, set its Y to line 100.
        v.write_reg(R_SP_ENABLE, 0x01);
        v.write_reg(0x01, 100); // sprite 0 Y
        // Sprite DMA turns on at the sprite-dma-check cycles (55/56 PAL) of the
        // line whose number matches the sprite Y. Advance past those cycles.
        while !(v.raster_line == 100 && v.raster_cycle == 57) {
            v.tick();
        }
        assert_eq!(v.sprite_dma & 0x01, 0x01, "sprite 0 DMA on at its Y line");
    }

    #[test]
    fn vic_path_no_badline_matches_plain_path() {
        use crate::{Machine, NullSink};
        // DEN=0 program (no badlines): SEI; LDA #$00; STA $D011; JMP $0805
        // -> allow_bad_lines never set, BA never low, so the VIC path steals
        // nothing and the cycle count is identical to the plain CPU path.
        let prog = [0x78u8, 0xA9, 0x00, 0x8D, 0x11, 0xD0, 0x4C, 0x05, 0x08];
        for budget in [19656u64, 25000, 40000] {
            let mut a = Machine::new();
            a.poke(0x0800, &prog);
            a.set_pc(0x0800);
            let mut o = NullSink;
            a.run_for(budget, &mut o);

            let mut b = Machine::new();
            b.poke(0x0800, &prog);
            b.set_pc(0x0800);
            let mut o2 = NullSink;
            b.run_for_vic(budget, &mut o2);
            assert_eq!(a.clk, b.clk, "no-badline: vic path == plain path @budget {budget}");
        }
    }

    #[test]
    fn vic_path_badline_steals_cycles() {
        use crate::{Machine, NullSink};
        // DEN=1 program (badlines active): SEI; LDA #$1B; STA $D011; JMP $0805.
        // Once raster reaches the DMA range the VIC steals read cycles, so the
        // VIC path advances MORE master cycles per budget window than the plain
        // (badline-blind) path would for the same instruction count.
        let prog = [0x78u8, 0xA9, 0x1B, 0x8D, 0x11, 0xD0, 0x4C, 0x05, 0x08];
        // Compare cycle counts against the badline-blind plain path at the SAME
        // instruction count. The plain path never stalls; the VIC path steals
        // read cycles on badlines, so it accumulates MORE master cycles for the
        // same number of retired instructions.
        let instr = 4000u64;
        let mut a = Machine::new();
        a.poke(0x0800, &prog);
        a.set_pc(0x0800);
        let mut oa = NullSink;
        a.run_for_capped(u64::MAX, instr, &mut oa);

        let mut b = Machine::new();
        b.poke(0x0800, &prog);
        b.set_pc(0x0800);
        let mut ob = NullSink;
        b.run_for_vic_capped(u64::MAX, instr, &mut ob);

        assert!(
            b.clk > a.clk,
            "badline steal: vic path clk {} must exceed plain path clk {} for {instr} instrs",
            b.clk,
            a.clk
        );
    }

    #[test]
    fn reg_kind_classification_matches_ts() {
        assert_eq!(VicII::reg_kind(R_RASTER), Some(VicRegKind::Raster));
        assert_eq!(VicII::reg_kind(R_CTRL1), Some(VicRegKind::Raster));
        assert_eq!(VicII::reg_kind(R_CTRL2), Some(VicRegKind::Mode));
        assert_eq!(VicII::reg_kind(R_MEM_PTR), Some(VicRegKind::Mode));
        assert_eq!(VicII::reg_kind(R_IRQ_STATUS), Some(VicRegKind::Irq));
        assert_eq!(VicII::reg_kind(R_IRQ_MASK), Some(VicRegKind::Irq));
        assert_eq!(VicII::reg_kind(0x20), None); // border color: untagged
    }
}
