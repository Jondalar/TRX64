//! vic.rs — VERBATIM per-cycle (viciisc) VIC-II (6569 PAL) timing core.
//!
//! This is a 1:1 port of VICE's per-cycle "SC" VIC-II — the model that pairs
//! with the verbatim x64sc 6510 SC core (`c64_6510core.rs`). It replaces the
//! earlier hardcoded-window approximation (BA `(12..=54)`, sprite check at
//! `cycle==55||56`, a prefetch-blind steal loop) with the actual cycle-table-
//! driven BA/AEC/DMA model.
//!
//! SOURCE OF TRUTH (ported VERBATIM — every fn cites the line):
//!   - vice/src/viciisc/vicii-cycle.c       — the per-cycle engine (`vicii_cycle`,
//!     `check_badline`, the sprite-DMA lifecycle, the Phi1/Phi2 split, the BA
//!     logic + `prefetch_cycles`, and `vicii_steal_cycles`).
//!   - vice/src/viciisc/vicii-chip-model.c  — `cycle_tab_pal` + `vicii_chip_model_set`
//!     (the BA/fetch grid compiled into `cycle_table[]`).
//!   - vice/src/viciisc/vicii-chip-model.h  — the cycle-flag bit layout + accessors.
//!   - vice/src/viciisc/vicii-mem.c         — `vicii_store` / `vicii_read` register
//!     R/W side effects (`d011_store` → ysmooth/raster-irq-line, `d019_store` ACK,
//!     `d01a_store` mask, `d017_store` sprite-crunch).
//!   - vice/src/viciisc/vicii-irq.c         — `vicii_irq_set_line` / `_raster_set` /
//!     `_raster_trigger` (the raster-IRQ edge-trigger, table-positioned so the
//!     line-0 +1 and the $D011/$D012 RMW edge come out of the cycle grid for free).
//!   - vice/src/c64/c64cpusc.c              — `CLK_INC()` (the per-CLK `vicii_cycle`
//!     hook) + `FETCH_OPCODE`/`check_ba` ordering.
//!   - vice/src/mainc64cpu.c                — `check_ba` (194-208) / `maincpu_steal_cycles`
//!     (112-192) — the steal that produces the CPU stall.
//!
//! Cross-ref (the TS oracle the gates validate against — the Rust reproduces its
//! per-cycle behavior 1:1): C64ReverseEngineeringMCP/src/runtime/headless/vic/
//! literal/{vicii-cycle.ts, vicii-chip-model.ts, vicii-mem.ts, vicii-irq.ts}.
//!
//! KEY MODEL DIFFERENCE vs the old approximation (the $E4DD fix): BA-low is NOT a
//! hardcoded window. It is `bad_line && cycle_is_fetch_ba(cycle_flags)` OR a sprite
//! BA mask, read PER CYCLE from `cycle_table[raster_cycle]`. The steal is the exact
//! VICE `vicii_steal_cycles` do-while (`clk++; ba = vicii_cycle()`) — the CPU is
//! stalled only while BA stays low, and the WRITE cycle of a store is never
//! check_ba'd (STORE has no check_ba in the SC core), so the badline-stalled
//! `STA ($F3),Y` cursor store at $E4DD costs 48 cycles, matching VICE/TS.
//!
//! `raster_line` / `raster_cycle` are derived FROM the clk-driven engine (the
//! `vicii_cycle` raster_cycle++ + the start-of-line/frame handling), exactly as
//! VICE — NOT parallel counters.
//!
//! Pixel draw / framebuffer generation is intentionally NOT here (that is the
//! separate per-cycle pixel-pipeline renderer step). This module exposes the
//! per-cycle state (display/idle, c/g-access selection, sprite data, the register
//! file) so `render.rs`'s existing static draw stays pixel-identical; render.rs
//! reads `vic.regs` + the derived `raster_line`/`raster_cycle`, which this port
//! keeps byte-for-byte compatible.
//!
//! Pure / sync / deterministic — no async, no rand, no time. Clone-able with the
//! Machine for Phase-2 COW forks.

#![allow(clippy::manual_range_contains)]
// The register-store/read match arms mirror VICE's `vicii_store`/`vicii_read`
// switch labels (one case per $D0xx offset) verbatim; clippy's range rewrite
// would obscure the 1:1 correspondence. Kept as explicit OR patterns.
#![allow(clippy::manual_range_patterns)]
// `SprPtr(0)` / `SprDma0(0)` etc. in the cycle table carry an explicit sprite
// number (`|0`) to mirror VICE's cycle_tab_pal row notation 1:1 for review; the
// `|0` is an identity op clippy flags but it is load-bearing for readability.
#![allow(clippy::identity_op)]
// Several cycle-flag accessors + Phi1-fetch-grid helpers are part of the verbatim
// 1:1 port surface (they classify the fetch type for the deferred pixel pipeline).
// They are not yet read by the timing/BA model, so allow dead_code on this module
// to preserve the full VICE accessor set for the later renderer step.
#![allow(dead_code)]

// ── PAL 6569 timing constants (vicii-timing.c / vicii-chip-model.c) ────────────

/// PAL: 63 cycles per raster line. (chip_model_mos6569r3.cycles_per_line)
pub const PAL_CYCLES_PER_LINE: u16 = 63;
/// PAL: 312 raster lines. (chip_model_mos6569r3.num_raster_lines = screen_height)
pub const PAL_SCREEN_HEIGHT: u16 = 312;
/// First line on which a badline can occur (viciitypes.h VICII_FIRST_DMA_LINE).
pub const FIRST_DMA_LINE: u16 = 0x30;
/// Last line on which a badline can occur (viciitypes.h VICII_LAST_DMA_LINE).
pub const LAST_DMA_LINE: u16 = 0xf7;

/// vicii-cycle.c VICII_PAL_CYCLE(c) = c-1 (PAL cycle 1..63 → table index 0..62).
#[inline]
const fn pal_cycle(c: u16) -> u16 {
    c - 1
}

/// Number of hardware sprites (viciitypes.h VICII_NUM_SPRITES).
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

// IRQ source bits ($D019 / $D01A) — vicii-irq.c.
pub const IRQ_RASTER: u8 = 0x01;
pub const IRQ_SBCOLL: u8 = 0x02;
pub const IRQ_SSCOLL: u8 = 0x04;
pub const IRQ_LIGHTPEN: u8 = 0x08;
pub const IRQ_SUMMARY: u8 = 0x80;

// =============================================================================
// SECTION — cycle-flag bit layout (vicii-chip-model.h).
// The compiled `cycle_table[]` entry is a u32 with this layout; the
// `cycle_is_*` accessors read it. PORT OF: vicii-chip-model.h:33-218.
// =============================================================================

// 31-29 Border.
const CHECK_BRD_L: u32 = 0x8000_0000;
const CHECK_BRD_R: u32 = 0x4000_0000;
const CHECK_BRD_CSEL: u32 = 0x2000_0000;
// 28-25 Sprites.
const CHECK_SPR_EXP_M: u32 = 0x1000_0000;
const CHECK_SPR_M: u32 = 0x0e00_0000;
const CHECK_SPR_DMA: u32 = 0x0200_0000;
const CHECK_SPR_DISP: u32 = 0x0400_0000;
const UPDATE_MCBASE: u32 = 0x0600_0000;
const CHECK_SPR_CRUNCH: u32 = 0x0800_0000;
// 24-23 VcRc.
const UPDATE_VC_M: u32 = 0x0100_0000;
const UPDATE_RC_M: u32 = 0x0080_0000;
// 22 Visible.
const VISIBLE_M: u32 = 0x0040_0000;
// 21-16 XPos/8.
const XPOS_M: u32 = 0x003f_0000;
const XPOS_B: u32 = 16;
// 15 May FetchC.
const PHI2_FETCH_C_M: u32 = 0x0000_8000;
// 14-12 Phi1 Fetch sprite num.
const PHI1_SPR_NUM_M: u32 = 0x0000_7000;
const PHI1_SPR_NUM_B: u32 = 12;
// 11-9 Phi1 Fetch.
const PHI1_TYPE_M: u32 = 0x0000_0e00;
const PHI1_IDLE: u32 = 0x0000_0000;
const PHI1_REFRESH: u32 = 0x0000_0200;
const PHI1_FETCH_G: u32 = 0x0000_0400;
const PHI1_SPR_PTR: u32 = 0x0000_0600;
const PHI1_SPR_DMA1: u32 = 0x0000_0800;
// 8-0 Check BA flags.
const FETCH_BA_M: u32 = 0x0000_0100;
const SPRITE_BA_MASK_M: u32 = 0x0000_00ff;
const SPRITE_BA_MASK_B: u32 = 0;

// PORT OF: vicii-chip-model.h:119-218 (cycle_is_* accessors).
#[inline]
fn cycle_get_sprite_ba_mask(flags: u32) -> u8 {
    ((flags & SPRITE_BA_MASK_M) >> SPRITE_BA_MASK_B) as u8
}
#[inline]
fn cycle_is_fetch_ba(flags: u32) -> bool {
    flags & FETCH_BA_M != 0
}
#[inline]
fn cycle_is_sprite_ptr_dma0(flags: u32) -> bool {
    flags & PHI1_TYPE_M == PHI1_SPR_PTR
}
#[inline]
fn cycle_is_sprite_dma1_dma2(flags: u32) -> bool {
    flags & PHI1_TYPE_M == PHI1_SPR_DMA1
}
#[inline]
fn cycle_get_sprite_num(flags: u32) -> usize {
    ((flags & PHI1_SPR_NUM_M) >> PHI1_SPR_NUM_B) as usize
}
#[inline]
fn cycle_is_refresh(flags: u32) -> bool {
    flags & PHI1_TYPE_M == PHI1_REFRESH
}
#[inline]
fn cycle_is_fetch_g(flags: u32) -> bool {
    flags & PHI1_TYPE_M == PHI1_FETCH_G
}
#[inline]
fn cycle_may_fetch_c(flags: u32) -> bool {
    flags & PHI2_FETCH_C_M != 0
}
#[inline]
fn cycle_is_update_vc(flags: u32) -> bool {
    flags & UPDATE_VC_M != 0
}
#[inline]
fn cycle_is_update_rc(flags: u32) -> bool {
    flags & UPDATE_RC_M != 0
}
#[inline]
fn cycle_is_check_spr_crunch(flags: u32) -> bool {
    flags & CHECK_SPR_M == CHECK_SPR_CRUNCH
}
#[inline]
fn cycle_is_update_mcbase(flags: u32) -> bool {
    flags & CHECK_SPR_M == UPDATE_MCBASE
}
#[inline]
fn cycle_is_check_spr_exp(flags: u32) -> bool {
    flags & CHECK_SPR_EXP_M != 0
}
#[inline]
fn cycle_is_check_spr_dma(flags: u32) -> bool {
    flags & CHECK_SPR_M == CHECK_SPR_DMA
}
#[inline]
fn cycle_is_check_spr_disp(flags: u32) -> bool {
    flags & CHECK_SPR_M == CHECK_SPR_DISP
}
#[inline]
fn cycle_is_check_border_l(flags: u32, csel: bool) -> bool {
    if flags & CHECK_BRD_L != 0 {
        if flags & CHECK_BRD_CSEL != 0 {
            csel
        } else {
            !csel
        }
    } else {
        false
    }
}
#[inline]
fn cycle_is_check_border_r(flags: u32, csel: bool) -> bool {
    if flags & CHECK_BRD_R != 0 {
        if flags & CHECK_BRD_CSEL != 0 {
            csel
        } else {
            !csel
        }
    } else {
        false
    }
}

// =============================================================================
// SECTION — the PAL cycle table (vicii-chip-model.c cycle_tab_pal).
//
// PORT OF: vicii-chip-model.c:111-238 (cycle_tab_pal) + :579-811
// (vicii_chip_model_set, which folds the Phi1+Phi2 rows of each PAL cycle into
// one `cycle_table[cycle-1]` u32 entry). We precompute the SAME 63-entry table
// at compile time below (cycle index 0..62 = VICII_PAL_CYCLE(1..63)).
//
// Each source row is { cycle, xpos, visible, fetch, ba, flags }. The encoder
// (chip-model.c:730-808) merges the two phases:
//   entry |= (ba_phi1 & BaSpr_M) << SPRITE_BA_MASK_B
//   entry |= (ba_phi1 & BaFetch) ? FETCH_BA_M : 0
//   Phi1 fetch type → PHI1_*; FetchC (Phi2) → PHI2_FETCH_C_M | VISIBLE_M
//   xpos from Phi1; UpdateVc/Rc, ChkSpr*, ChkBrd* OR'd from both phases' flags.
// =============================================================================

/// Source-table row constants (vicii-chip-model.c #defines, lines 53-97).
mod tab {
    // Fetch field (FetchType_M | FetchSprNum_M).
    pub const NONE: u16 = 0;
    pub const SPR_PTR: u16 = 0x100; // | sprite num
    pub const SPR_DMA0: u16 = 0x200;
    pub const SPR_DMA1: u16 = 0x300;
    pub const SPR_DMA2: u16 = 0x400;
    pub const REFRESH: u16 = 0x500;
    pub const FETCH_G: u16 = 0x600;
    pub const FETCH_C: u16 = 0x700;
    pub const IDLE: u16 = 0x800;
    pub const FETCH_TYPE_M: u16 = 0xf00;
    pub const FETCH_SPR_NUM_M: u16 = 0x007;

    // BA field.
    pub const BA_FETCH: u16 = 0x100;
    pub const BA_SPR_M: u16 = 0xff;

    // Flags.
    pub const UPDATE_MCBASE: u16 = 0x001;
    pub const CHK_SPR_EXP: u16 = 0x002;
    pub const CHK_SPR_DMA: u16 = 0x004;
    pub const CHK_SPR_DISP: u16 = 0x008;
    pub const CHK_SPR_CRUNCH: u16 = 0x010;
    pub const CHK_BRD_L1: u16 = 0x020;
    pub const CHK_BRD_L0: u16 = 0x040;
    pub const CHK_BRD_R0: u16 = 0x080;
    pub const CHK_BRD_R1: u16 = 0x100;
    pub const UPDATE_VC: u16 = 0x200;
    pub const UPDATE_RC: u16 = 0x400;

    #[inline]
    pub const fn spr1(x: u16) -> u16 {
        1 << x
    }
    #[inline]
    pub const fn spr2(x: u16, y: u16) -> u16 {
        (1 << x) | (1 << y)
    }
    #[inline]
    pub const fn spr3(x: u16, y: u16, z: u16) -> u16 {
        (1 << x) | (1 << y) | (1 << z)
    }
}

/// One row of cycle_tab_pal: (xpos, fetch, ba, flags). The `cycle`/`visible`
/// columns are not needed by the timing/BA model (visible/xpos feed the pixel
/// renderer, deferred). Pairs of rows (Phi1, Phi2) per PAL cycle.
struct Row {
    fetch: u16,
    ba: u16,
    flags: u16,
}
const fn r(fetch: u16, ba: u16, flags: u16) -> Row {
    Row { fetch, ba, flags }
}

/// PORT OF: vicii-chip-model.c:111-238 cycle_tab_pal (PAL, 63 cycles × 2 phases).
/// Index 2*(cycle-1)+phi. Only the (fetch, ba, flags) columns are carried.
#[rustfmt::skip]
const CYCLE_TAB_PAL: [Row; 126] = {
    use tab::*;
    [
        // Phi1(1),  Phi2(1)
        r(SPR_PTR|3,  spr2(3,4),    NONE),               r(SPR_DMA0|3, spr2(3,4),    NONE),
        // Phi1(2),  Phi2(2)
        r(SPR_DMA1|3, spr3(3,4,5),  NONE),               r(SPR_DMA2|3, spr3(3,4,5),  NONE),
        // Phi1(3),  Phi2(3)
        r(SPR_PTR|4,  spr2(4,5),    NONE),               r(SPR_DMA0|4, spr2(4,5),    NONE),
        // Phi1(4),  Phi2(4)
        r(SPR_DMA1|4, spr3(4,5,6),  NONE),               r(SPR_DMA2|4, spr3(4,5,6),  NONE),
        // Phi1(5),  Phi2(5)
        r(SPR_PTR|5,  spr2(5,6),    NONE),               r(SPR_DMA0|5, spr2(5,6),    NONE),
        // Phi1(6),  Phi2(6)
        r(SPR_DMA1|5, spr3(5,6,7),  NONE),               r(SPR_DMA2|5, spr3(5,6,7),  NONE),
        // Phi1(7),  Phi2(7)
        r(SPR_PTR|6,  spr2(6,7),    NONE),               r(SPR_DMA0|6, spr2(6,7),    NONE),
        // Phi1(8),  Phi2(8)
        r(SPR_DMA1|6, spr2(6,7),    NONE),               r(SPR_DMA2|6, spr2(6,7),    NONE),
        // Phi1(9),  Phi2(9)
        r(SPR_PTR|7,  spr1(7),      NONE),               r(SPR_DMA0|7, spr1(7),      NONE),
        // Phi1(10), Phi2(10)
        r(SPR_DMA1|7, spr1(7),      NONE),               r(SPR_DMA2|7, spr1(7),      NONE),
        // Phi1(11), Phi2(11)
        r(REFRESH,    NONE,         NONE),               r(NONE,       NONE,         NONE),
        // Phi1(12), Phi2(12)
        r(REFRESH,    BA_FETCH,     NONE),               r(NONE,       BA_FETCH,     NONE),
        // Phi1(13), Phi2(13)
        r(REFRESH,    BA_FETCH,     NONE),               r(NONE,       BA_FETCH,     NONE),
        // Phi1(14), Phi2(14)
        r(REFRESH,    BA_FETCH,     NONE),               r(NONE,       BA_FETCH,     UPDATE_VC),
        // Phi1(15), Phi2(15)
        r(REFRESH,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     CHK_SPR_CRUNCH),
        // Phi1(16), Phi2(16)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     UPDATE_MCBASE),
        // Phi1(17), Phi2(17)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     CHK_BRD_L1),
        // Phi1(18), Phi2(18)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     CHK_BRD_L0),
        // Phi1(19), Phi2(19)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(20), Phi2(20)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(21), Phi2(21)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(22), Phi2(22)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(23), Phi2(23)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(24), Phi2(24)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(25), Phi2(25)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(26), Phi2(26)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(27), Phi2(27)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(28), Phi2(28)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(29), Phi2(29)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(30), Phi2(30)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(31), Phi2(31)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(32), Phi2(32)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(33), Phi2(33)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(34), Phi2(34)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(35), Phi2(35)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(36), Phi2(36)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(37), Phi2(37)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(38), Phi2(38)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(39), Phi2(39)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(40), Phi2(40)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(41), Phi2(41)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(42), Phi2(42)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(43), Phi2(43)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(44), Phi2(44)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(45), Phi2(45)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(46), Phi2(46)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(47), Phi2(47)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(48), Phi2(48)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(49), Phi2(49)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(50), Phi2(50)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(51), Phi2(51)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(52), Phi2(52)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(53), Phi2(53)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(54), Phi2(54)
        r(FETCH_G,    BA_FETCH,     NONE),               r(FETCH_C,    BA_FETCH,     NONE),
        // Phi1(55), Phi2(55)
        r(FETCH_G,    spr1(0),      CHK_SPR_DMA),        r(NONE,       spr1(0),      NONE),
        // Phi1(56), Phi2(56)
        r(IDLE,       spr1(0),      CHK_SPR_DMA),        r(NONE,       spr1(0),      CHK_BRD_R0|CHK_SPR_EXP),
        // Phi1(57), Phi2(57)
        r(IDLE,       spr2(0,1),    NONE),               r(NONE,       spr2(0,1),    CHK_BRD_R1),
        // Phi1(58), Phi2(58)
        r(SPR_PTR|0,  spr2(0,1),    CHK_SPR_DISP),       r(SPR_DMA0|0, spr2(0,1),    UPDATE_RC),
        // Phi1(59), Phi2(59)
        r(SPR_DMA1|0, spr3(0,1,2),  NONE),               r(SPR_DMA2|0, spr3(0,1,2),  NONE),
        // Phi1(60), Phi2(60)
        r(SPR_PTR|1,  spr2(1,2),    NONE),               r(SPR_DMA0|1, spr2(1,2),    NONE),
        // Phi1(61), Phi2(61)
        r(SPR_DMA1|1, spr3(1,2,3),  NONE),               r(SPR_DMA2|1, spr3(1,2,3),  NONE),
        // Phi1(62), Phi2(62)
        r(SPR_PTR|2,  spr2(2,3),    NONE),               r(SPR_DMA0|2, spr2(2,3),    NONE),
        // Phi1(63), Phi2(63)
        r(SPR_DMA1|2, spr3(2,3,4),  NONE),               r(SPR_DMA2|2, spr3(2,3,4),  NONE),
    ]
};

/// Build the compiled 63-entry `cycle_table[]` from `CYCLE_TAB_PAL`.
/// PORT OF: vicii-chip-model.c:729-809 (the per-cycle Phi1+Phi2 fold).
fn build_cycle_table() -> [u32; PAL_CYCLES_PER_LINE as usize] {
    let mut table = [0u32; PAL_CYCLES_PER_LINE as usize];
    for cyc in 0..PAL_CYCLES_PER_LINE as usize {
        let phi1 = &CYCLE_TAB_PAL[cyc * 2]; // Phi1 row
        let phi2 = &CYCLE_TAB_PAL[cyc * 2 + 1]; // Phi2 row
        let f = (phi1.flags | phi2.flags) as u32;

        let mut entry: u32 = 0;

        // chip-model.c:735-736 — BA from Phi1.
        entry |= ((phi1.ba & tab::BA_SPR_M) as u32) << SPRITE_BA_MASK_B;
        entry |= if phi1.ba & tab::BA_FETCH != 0 { FETCH_BA_M } else { 0 };

        // chip-model.c:738-760 — Phi1 fetch type → PHI1_* + sprite num.
        match phi1.fetch & tab::FETCH_TYPE_M {
            tab::SPR_PTR => {
                entry |= PHI1_SPR_PTR;
                entry |= ((phi1.fetch & tab::FETCH_SPR_NUM_M) as u32) << PHI1_SPR_NUM_B;
            }
            tab::SPR_DMA1 => {
                entry |= PHI1_SPR_DMA1;
                entry |= ((phi1.fetch & tab::FETCH_SPR_NUM_M) as u32) << PHI1_SPR_NUM_B;
            }
            tab::REFRESH => entry |= PHI1_REFRESH,
            tab::FETCH_G => entry |= PHI1_FETCH_G,
            _ => entry |= PHI1_IDLE,
        }

        // chip-model.c:761-765 — FetchC (Phi2) → PHI2_FETCH_C_M | VISIBLE_M.
        if phi2.fetch & tab::FETCH_TYPE_M == tab::FETCH_C {
            entry |= PHI2_FETCH_C_M;
            entry |= VISIBLE_M;
        }

        // chip-model.c:767 — xpos (Phi1). Not used by the timing model; left 0.
        let _ = XPOS_M;
        let _ = XPOS_B;

        // chip-model.c:769-792 — VC/RC + sprite flags.
        if f & tab::UPDATE_VC as u32 != 0 {
            entry |= UPDATE_VC_M;
        }
        if f & tab::UPDATE_RC as u32 != 0 {
            entry |= UPDATE_RC_M;
        }
        if f & tab::CHK_SPR_EXP as u32 != 0 {
            entry |= CHECK_SPR_EXP_M;
        }
        if f & tab::CHK_SPR_DISP as u32 != 0 {
            entry |= CHECK_SPR_DISP;
        }
        if f & tab::CHK_SPR_DMA as u32 != 0 {
            entry |= CHECK_SPR_DMA;
        }
        if f & tab::UPDATE_MCBASE as u32 != 0 {
            entry |= UPDATE_MCBASE;
        }
        if f & tab::CHK_SPR_CRUNCH as u32 != 0 {
            entry |= CHECK_SPR_CRUNCH;
        }

        // chip-model.c:794-806 — border flags.
        if f & tab::CHK_BRD_L0 as u32 != 0 {
            entry |= CHECK_BRD_L;
        }
        if f & tab::CHK_BRD_L1 as u32 != 0 {
            entry |= CHECK_BRD_L | CHECK_BRD_CSEL;
        }
        if f & tab::CHK_BRD_R0 as u32 != 0 {
            entry |= CHECK_BRD_R;
        }
        if f & tab::CHK_BRD_R1 as u32 != 0 {
            entry |= CHECK_BRD_R | CHECK_BRD_CSEL;
        }

        table[cyc] = entry;
    }
    table
}

// =============================================================================
// SECTION — per-sprite state (viciitypes.h vicii_sprite_s).
// =============================================================================

/// PORT OF: viciitypes.h:77-92 vicii_sprite_s — the per-sprite DMA state.
#[derive(Clone, Copy, Default)]
pub struct Sprite {
    /// 24-bit shift data (uint32_t). Pixel renderer reads it; timing only stores.
    pub data: u32,
    /// 6-bit MC counter.
    pub mc: u8,
    /// 6-bit MCBASE counter.
    pub mcbase: u8,
    /// 8-bit sprite pointer.
    pub pointer: u8,
    /// Y-expansion flip-flop.
    pub exp_flop: u8,
    /// X coordinate (9-bit; renderer).
    pub x: u16,
}

/// A register write the VIC observed this cycle, surfaced to the Observer so a
/// trace sink can (in principle) emit a VIC_REG_WRITE frame. Mirrors the TS
/// VIC_KIND_CODE { raster:1, mode:2, irq:3, badline:4 }. NOTE: the TS oracle's
/// vic channel has no live producer, so these are not emitted into the gate
/// trace — the hook exists for format completeness + future integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VicRegKind {
    Raster = 1,
    Mode = 2,
    Irq = 3,
    Badline = 4,
}

// =============================================================================
// SECTION — the VIC-II state (viciitypes.h struct vicii_s subset).
// =============================================================================

/// VERBATIM per-cycle VIC-II state. Field names follow VICE's `vicii` struct
/// (viciitypes.h). The public fields used by render.rs / vsf.rs / the daemon
/// (`regs`, `raster_line`, `raster_cycle`, `raster_irq_line`, `allow_bad_lines`,
/// `bad_line`, `irq_line`, `frame`) keep the exact names + types the old
/// approximation exposed, so callers are unchanged.
#[derive(Clone)]
pub struct VicII {
    /// $D000-$D03F register file (vicii.regs[0x40]).
    pub regs: [u8; 0x40],

    /// Cycle # within the current line (vicii.raster_cycle), 0..62 PAL.
    pub raster_cycle: u16,
    /// Compiled cycle-flags for the current cycle (vicii.cycle_flags).
    pub cycle_flags: u32,
    /// Current line (vicii.raster_line), 0..311 PAL.
    pub raster_line: u16,
    /// Start-of-frame latch (vicii.start_of_frame).
    pub start_of_frame: bool,

    /// IRQ latch register (vicii.irq_status) — bits 0..3 + bit7 summary.
    pub irq_status: u8,
    /// Raster compare line (vicii.raster_irq_line) — 9-bit.
    pub raster_irq_line: u16,
    /// Raster-compare edge-detect latch (vicii.raster_irq_triggered).
    pub raster_irq_triggered: bool,

    /// Parsed YSCROLL (vicii.ysmooth) — D011 low 3 bits.
    pub ysmooth: u8,
    /// Sticky allow-bad-lines (vicii.allow_bad_lines).
    pub allow_bad_lines: bool,
    /// Current line is a badline (vicii.bad_line).
    pub bad_line: bool,

    /// Idle state (vicii.idle_state).
    pub idle_state: bool,
    /// VCBASE (vicii.vcbase).
    pub vcbase: u16,
    /// VC (vicii.vc).
    pub vc: u16,
    /// RC (vicii.rc).
    pub rc: u8,
    /// VMLI (vicii.vmli).
    pub vmli: u16,

    /// reg11 delayed-by-one-cycle copy (vicii.reg11_delay).
    pub reg11_delay: u8,
    /// Prefetch-cycle countdown (vicii.prefetch_cycles).
    pub prefetch_cycles: u8,

    /// Sprite-display mask (vicii.sprite_display_bits).
    pub sprite_display_bits: u8,
    /// Sprite-DMA mask (vicii.sprite_dma).
    pub sprite_dma: u8,
    /// Per-sprite DMA state (vicii.sprite[8]).
    pub sprite: [Sprite; NUM_SPRITES],

    /// Collision registers (vicii.sprite_*_collisions) + clear flag.
    pub sprite_sprite_collisions: u8,
    pub sprite_background_collisions: u8,
    pub clear_collisions: u8,

    /// Border flags (vicii.vborder / set_vborder / main_border).
    pub vborder: bool,
    pub set_vborder: bool,
    pub main_border: bool,

    /// DRAM refresh counter (vicii.refresh_counter).
    pub refresh_counter: u8,

    /// Last value read by VIC during Phi1 (vicii.last_read_phi1).
    pub last_read_phi1: u8,
    /// Last value on the internal VIC bus during Phi2 (vicii.last_bus_phi2).
    pub last_bus_phi2: u8,

    /// Frame counter (diagnostic; advances at start_of_frame). Not a VICE field.
    pub frame: u64,

    /// Latched IRQ output line level (vicii.irq_status & regs[0x1a] != 0). The
    /// full-machine run loop samples this into the CPU IRQ source each boundary.
    pub irq_line: bool,

    /// VICE `maincpu_ba_low_flags` (VICII bit): the BA-low state the LAST
    /// `vicii_cycle()` returned, consumed by the next `check_ba` / `steal_cycles`.
    pub ba_low_flag: bool,

    /// Compiled PAL cycle table (vicii.cycle_table). Built at `new()`.
    cycle_table: [u32; PAL_CYCLES_PER_LINE as usize],
    /// cycles_per_line (vicii.cycles_per_line) — 63 PAL.
    cycles_per_line: u16,
    /// screen_height (vicii.screen_height) — 312 PAL.
    screen_height: u16,
}

impl Default for VicII {
    fn default() -> Self {
        Self::new()
    }
}

impl VicII {
    /// PORT OF: vicii_init (vicii.c) + vicii_reset (viciisc/vicii.c:77-133) +
    /// vicii.c:240 sprite exp_flop=1 + the chip-model build. Power-on defaults.
    pub fn new() -> Self {
        let mut v = VicII {
            regs: [0u8; 0x40],
            raster_cycle: 0,
            cycle_flags: 0,
            raster_line: 0,
            start_of_frame: false,
            irq_status: 0,
            raster_irq_line: 0,
            raster_irq_triggered: false,
            ysmooth: 0,
            allow_bad_lines: false,
            bad_line: false,
            idle_state: false,
            vcbase: 0,
            vc: 0,
            rc: 0,
            vmli: 0,
            reg11_delay: 0,
            prefetch_cycles: 0,
            sprite_display_bits: 0,
            sprite_dma: 0,
            sprite: [Sprite::default(); NUM_SPRITES],
            sprite_sprite_collisions: 0,
            sprite_background_collisions: 0,
            clear_collisions: 0,
            vborder: true,
            set_vborder: true,
            main_border: true,
            refresh_counter: 0xff,
            last_read_phi1: 0,
            last_bus_phi2: 0xff,
            frame: 0,
            irq_line: false,
            ba_low_flag: false,
            cycle_table: build_cycle_table(),
            cycles_per_line: PAL_CYCLES_PER_LINE,
            screen_height: PAL_SCREEN_HEIGHT,
        };
        // vicii.c:240 — Y-expansion flip-flops init to 1.
        for s in v.sprite.iter_mut() {
            s.exp_flop = 1;
        }
        v
    }

    /// Classify a $D000-$D02E register offset into a VIC trace kind, matching the
    /// TS producer's `kind` tagging. $D012/$D011 → raster, $D016/$D018 → mode,
    /// $D019/$D01A → irq. Returns None otherwise.
    pub fn reg_kind(offset: u8) -> Option<VicRegKind> {
        match offset {
            R_RASTER | R_CTRL1 => Some(VicRegKind::Raster),
            R_CTRL2 | R_MEM_PTR => Some(VicRegKind::Mode),
            R_IRQ_STATUS | R_IRQ_MASK => Some(VicRegKind::Irq),
            _ => None,
        }
    }

    // =========================================================================
    // SECTION — IRQ (PORT OF: viciisc/vicii-irq.c).
    // =========================================================================

    /// PORT OF: vicii-irq.c:36 vicii_irq_set_line. Recompute irq_status bit7 +
    /// the output line level (irq_line) from irq_status & regs[0x1a]. The
    /// `maincpu_set_irq` host call is replaced by latching `irq_line`, which the
    /// full-machine run loop samples into the CPU's INT_SRC_VIC each boundary.
    fn vicii_irq_set_line(&mut self) {
        if self.irq_status & self.regs[R_IRQ_MASK as usize] != 0 {
            self.irq_status |= 0x80;
            self.irq_line = true;
        } else {
            self.irq_status &= 0x7f;
            self.irq_line = false;
        }
    }

    /// PORT OF: vicii-irq.c:58 vicii_irq_raster_set (clk arg dropped — the line
    /// level is sampled at the boundary, not stamped at a sub-cycle clk).
    fn vicii_irq_raster_set(&mut self) {
        self.irq_status |= IRQ_RASTER;
        self.vicii_irq_set_line();
    }

    /// PORT OF: vicii-irq.c:70 vicii_irq_sbcoll_set.
    fn vicii_irq_sbcoll_set(&mut self) {
        self.irq_status |= IRQ_SBCOLL;
        self.vicii_irq_set_line();
    }
    /// PORT OF: vicii-irq.c:82 vicii_irq_sscoll_set.
    fn vicii_irq_sscoll_set(&mut self) {
        self.irq_status |= IRQ_SSCOLL;
        self.vicii_irq_set_line();
    }

    /// PORT OF: vicii-irq.c:116 vicii_irq_raster_trigger — fire on the
    /// non-match→match edge if the raster latch is not already set.
    fn vicii_irq_raster_trigger(&mut self) {
        if self.irq_status & IRQ_RASTER == 0 {
            self.vicii_irq_raster_set();
        }
    }

    // =========================================================================
    // SECTION — the per-cycle helpers (PORT OF: viciisc/vicii-cycle.c).
    // =========================================================================

    /// PORT OF: vicii-cycle.c:51 check_badline.
    #[inline]
    fn check_badline(&mut self) {
        if (self.raster_line & 7) == self.ysmooth as u16 {
            self.bad_line = true;
            self.idle_state = false;
        } else {
            self.bad_line = false;
        }
    }

    /// PORT OF: vicii-cycle.c:62 check_sprite_display.
    #[inline]
    fn check_sprite_display(&mut self) {
        let enable = self.regs[R_SP_ENABLE as usize];
        let mut b = 1u8;
        for i in 0..NUM_SPRITES {
            let y = self.regs[i * 2 + 1];
            self.sprite[i].mc = self.sprite[i].mcbase;
            if self.sprite_dma & b != 0 {
                if (enable & b != 0) && (y as u16 == (self.raster_line & 0xff)) {
                    self.sprite_display_bits |= b;
                }
            } else {
                self.sprite_display_bits &= !b;
            }
            b <<= 1;
        }
    }

    /// PORT OF: vicii-cycle.c:81 sprite_mcbase_update — memptr==63 turn-off.
    #[inline]
    fn sprite_mcbase_update(&mut self) {
        for i in 0..NUM_SPRITES {
            if self.sprite[i].exp_flop != 0 {
                self.sprite[i].mcbase = self.sprite[i].mc;
                if self.sprite[i].mcbase == 63 {
                    self.sprite_dma &= !(1u8 << i);
                }
            }
        }
    }

    /// PORT OF: vicii-cycle.c:95 check_exp — toggle the Y-expansion flip-flop.
    #[inline]
    fn check_exp(&mut self) {
        let y_exp = self.regs[R_SP_Y_EXP as usize];
        let mut b = 1u8;
        for i in 0..NUM_SPRITES {
            if (self.sprite_dma & b != 0) && (y_exp & b != 0) {
                self.sprite[i].exp_flop ^= 1;
            }
            b <<= 1;
        }
    }

    /// PORT OF: vicii-cycle.c:108 turn_sprite_dma_on.
    #[inline]
    fn turn_sprite_dma_on(&mut self, i: usize) {
        self.sprite_dma |= 1u8 << i;
        self.sprite[i].mcbase = 0;
        self.sprite[i].exp_flop = 1;
    }

    /// PORT OF: vicii-cycle.c:115 check_sprite_dma.
    #[inline]
    fn check_sprite_dma(&mut self) {
        let enable = self.regs[R_SP_ENABLE as usize];
        let mut b = 1u8;
        for i in 0..NUM_SPRITES {
            let y = self.regs[i * 2 + 1];
            if (enable & b != 0)
                && (y as u16 == (self.raster_line & 0xff))
                && (self.sprite_dma & b == 0)
            {
                self.turn_sprite_dma_on(i);
            }
            b <<= 1;
        }
    }

    /// PORT OF: vicii-cycle.c:202 vicii_cycle_start_of_frame.
    #[inline]
    fn vicii_cycle_start_of_frame(&mut self) {
        self.start_of_frame = false;
        self.raster_line = 0;
        self.refresh_counter = 0xff;
        self.allow_bad_lines = false;
        self.vcbase = 0;
        self.vc = 0;
        self.frame = self.frame.wrapping_add(1);
        // light_pen.triggered = 0 / retrigger — deferred (light pen not modeled).
    }

    /// PORT OF: vicii-cycle.c:220 vicii_cycle_end_of_line.
    #[inline]
    fn vicii_cycle_end_of_line(&mut self) {
        // vicii_raster_draw_handler() — frame-buffer flush; deferred (render.rs
        // draws statically). The start_of_frame latch is the only timing effect.
        if self.raster_line == self.screen_height - 1 {
            self.start_of_frame = true;
        }
    }

    /// PORT OF: vicii-cycle.c:228 vicii_cycle_start_of_line.
    #[inline]
    fn vicii_cycle_start_of_line(&mut self) {
        if (self.raster_line == FIRST_DMA_LINE)
            && !self.allow_bad_lines
            && (self.regs[R_CTRL1 as usize] & 0x10 != 0)
        {
            self.allow_bad_lines = true;
        }
        if self.raster_line == LAST_DMA_LINE {
            self.allow_bad_lines = false;
        }
        self.bad_line = false;
    }

    /// PORT OF: vicii-cycle.c:244 next_vicii_cycle — raster_cycle++ with wrap.
    #[inline]
    fn next_vicii_cycle(&mut self) {
        self.raster_cycle += 1;
        if self.raster_cycle == self.cycles_per_line {
            self.raster_cycle = 0;
        }
    }

    // =========================================================================
    // SECTION — the cycle engine (PORT OF: vicii-cycle.c:374 vicii_cycle).
    // =========================================================================

    /// PORT OF: vicii-cycle.c:374 `int vicii_cycle(void)`. Advance the VIC by
    /// exactly one master cycle; return BA-low for this cycle (true = VIC owns
    /// the bus). This is `tick()` — the per-CLK hook the SC CPU calls from
    /// CLK_INC (c64cpusc.c:47-51). Control flow + ordering mirror the C 1:1.
    ///
    /// (The Phi1/Phi2 graphics+sprite FETCHES that VICE performs here — the
    /// vicii_fetch_* calls and the pixel `vicii_draw_cycle` — are the separate
    /// renderer step and are intentionally omitted; their internal-counter side
    /// effects that the TIMING depends on (vc/rc/vmli/idle, sprite mc) ARE
    /// reproduced inline below. Memory-content fetches feed only the pixel
    /// pipeline, which render.rs handles statically.)
    pub fn tick(&mut self) -> bool {
        let mut ba_low = false;

        // vicii-cycle.c:383 vicii_fetch_sprites — Phi2 sprite data fetch for the
        // PREVIOUS cycle's flags. The memory fetch feeds only the pixel pipeline
        // (deferred), but the per-sprite `mc` counter advance it performs is
        // TIMING-RELEVANT: `mc` drives `mcbase==63` (sprite_mcbase_update), which
        // turns sprite DMA off and so ends the sprite BA-low window. PORT OF:
        // vicii-fetch.c:309 vicii_fetch_sprites (the mc++ in sprite_dma_cycle_0 /
        // sprite_dma_cycle_2; the fetch itself is skipped).
        self.vicii_fetch_sprites_mc(self.cycle_flags);

        // ── End of Phi2 ──

        // vicii-cycle.c:392 — Next cycle + load cycle_flags.
        self.next_vicii_cycle();
        self.cycle_flags = self.cycle_table[self.raster_cycle as usize];

        // ── Start of Phi1 ──
        // vicii-cycle.c:402 cycle_phi1_fetch — sets last_read_phi1 (renderer); the
        // idle/graphics distinction is via idle_state (tracked below). The Phi1
        // sprite-DMA-1 mc advance (vicii_fetch_sprite_dma_1) is timing-relevant
        // (same `mc`→mcbase→turn-off chain), reproduced here sans fetch.
        self.cycle_phi1_fetch_mc(self.cycle_flags);

        // vicii-cycle.c:405 check_hborder.
        self.check_hborder();

        // vicii-cycle.c:411 vicii_draw_cycle — pixel pipeline (deferred).

        // vicii-cycle.c:414-425 — collision-register clear initiated by $d01e/$d01f.
        match self.clear_collisions {
            0x1e => {
                self.sprite_sprite_collisions = 0;
                self.clear_collisions = 0;
            }
            0x1f => {
                self.sprite_background_collisions = 0;
                self.clear_collisions = 0;
            }
            _ => {}
        }
        // vicii-cycle.c:427-433 — collision IRQs (collisions never set at this
        // timing level, but the gate is reproduced for structure).
        let can_sprite_sprite = self.sprite_sprite_collisions == 0;
        let can_sprite_background = self.sprite_background_collisions == 0;
        if can_sprite_sprite && self.sprite_sprite_collisions != 0 {
            self.vicii_irq_sscoll_set();
        }
        if can_sprite_background && self.sprite_background_collisions != 0 {
            self.vicii_irq_sbcoll_set();
        }

        // ── End of Phi1 / Start of Phi2 ──

        // vicii-cycle.c:448-451 — end-of-line / start-of-line at PAL cycle 1.
        if self.raster_cycle == pal_cycle(1) {
            self.vicii_cycle_end_of_line();
            self.vicii_cycle_start_of_line();
        }

        // vicii-cycle.c:453-461 — start-of-frame (cycle 2) or raster_line++ (cycle 1).
        if self.start_of_frame {
            if self.raster_cycle == pal_cycle(2) {
                self.vicii_cycle_start_of_frame();
            }
        } else if self.raster_cycle == pal_cycle(1) {
            self.raster_line += 1;
        }

        // vicii-cycle.c:467-474 — raster-compare IRQ edge.
        if self.raster_line == self.raster_irq_line {
            if !self.raster_irq_triggered {
                self.vicii_irq_raster_trigger();
                self.raster_irq_triggered = true;
            }
        } else {
            self.raster_irq_triggered = false;
        }

        // vicii-cycle.c:477-482 — vertical border flags.
        self.check_vborder_top(self.raster_line);
        self.check_vborder_bottom(self.raster_line);
        if self.raster_cycle == pal_cycle(1) {
            self.vborder = self.set_vborder;
        }

        // ── Sprite logic ──
        // vicii-cycle.c:492 sprite_mcbase_update (cycle 16).
        if cycle_is_update_mcbase(self.cycle_flags) {
            self.sprite_mcbase_update();
        }
        // vicii-cycle.c:499 check_sprite_dma (cycles 55 & 56).
        if cycle_is_check_spr_dma(self.cycle_flags) {
            self.check_sprite_dma();
        }
        // vicii-cycle.c:505 check_exp (cycle 56).
        if cycle_is_check_spr_exp(self.cycle_flags) {
            self.check_exp();
        }
        // vicii-cycle.c:511 check_sprite_display (cycle 58).
        if cycle_is_check_spr_disp(self.cycle_flags) {
            self.check_sprite_display();
        }

        // ── Graphics logic ──
        // vicii-cycle.c:524-526 — DEN on first DMA line.
        if (self.raster_line == FIRST_DMA_LINE) && !self.allow_bad_lines {
            self.allow_bad_lines = self.regs[R_CTRL1 as usize] & 0x10 != 0;
        }
        // vicii-cycle.c:529-531 — badline check.
        if self.allow_bad_lines {
            self.check_badline();
        }
        // vicii-cycle.c:534-538 VSP bug — disabled (vsp_bug_enabled=0), no effect.

        // vicii-cycle.c:543-549 — Update VC (cycle 14).
        if cycle_is_update_vc(self.cycle_flags) {
            self.vc = self.vcbase;
            self.vmli = 0;
            if self.bad_line {
                self.rc = 0;
            }
        }
        // vicii-cycle.c:553-564 — Update RC (cycle 58).
        if cycle_is_update_rc(self.cycle_flags) {
            if self.rc == 7 {
                self.idle_state = true;
                self.vcbase = self.vc;
            }
            if !self.idle_state || self.bad_line {
                self.rc = (self.rc + 1) & 0x7;
                self.idle_state = false;
            }
        }

        // ── BA logic ── (vicii-cycle.c:572-591)
        // Matrix fetch BA.
        if self.bad_line && cycle_is_fetch_ba(self.cycle_flags) {
            ba_low = true;
        }
        // Sprite Phi2 fetch BA.
        if self.sprite_dma & cycle_get_sprite_ba_mask(self.cycle_flags) != 0 {
            ba_low = true;
        }
        // Prefetch-cycle countdown (gates Phi2 accesses; does NOT gate ba_low).
        if ba_low {
            if self.prefetch_cycles != 0 {
                self.prefetch_cycles -= 1;
            }
        } else {
            self.prefetch_cycles = 3 + 1;
        }

        // vicii-cycle.c:595-602 — Matrix fetch (renderer; vbuf/cbuf, deferred).
        // The vc/vmli advances that affect timing already ran above.

        // vicii-cycle.c:605 — clear internal Phi2 bus.
        self.last_bus_phi2 = 0xff;
        // vicii-cycle.c:608 — delay video mode by one cycle.
        self.reg11_delay = self.regs[R_CTRL1 as usize];

        // Latch BA-low for the next check_ba (= CLK_INC: maincpu_ba_low_flags
        // |= vicii_cycle()). Set-only on true (CLK_INC clears the VICII bit
        // first via `&= ~MAINCPU_BA_LOW_VICII`, then OR's this).
        self.ba_low_flag = ba_low;
        ba_low
    }

    /// PORT OF: vicii-cycle.c:165 check_vborder_top.
    #[inline]
    fn check_vborder_top(&mut self, line: u16) {
        let rsel = self.regs[R_CTRL1 as usize] & 0x08;
        let start = if rsel != 0 { 0x33 } else { 0x37 };
        if (line == start) && (self.regs[R_CTRL1 as usize] & 0x10 != 0) {
            self.vborder = false;
            self.set_vborder = false;
        }
    }
    /// PORT OF: vicii-cycle.c:175 check_vborder_bottom.
    #[inline]
    fn check_vborder_bottom(&mut self, line: u16) {
        let rsel = self.regs[R_CTRL1 as usize] & 0x08;
        let stop = if rsel != 0 { 0xfb } else { 0xf7 };
        if line == stop {
            self.set_vborder = true;
        }
    }
    /// PORT OF: vicii-fetch.c:309 vicii_fetch_sprites — the Phi2 sprite-DMA `mc`
    /// counter advance ONLY (the memory fetch into `sprite[i].data` is the pixel
    /// pipeline, deferred). `sprite_dma_cycle_0` (on a SprPtr/DMA0 cycle) and
    /// `sprite_dma_cycle_2` (on a SprDma1/DMA2 cycle) each do `mc++; mc &= 0x3f`
    /// when `check_sprite_dma(i)`. This `mc` is what `sprite_mcbase_update` copies
    /// to mcbase and tests against 63 to end the DMA — so it MUST advance for the
    /// sprite BA window to close at the right cycle.
    #[inline]
    fn vicii_fetch_sprites_mc(&mut self, cycle_flags: u32) {
        if cycle_is_sprite_ptr_dma0(cycle_flags) {
            // sprite_dma_cycle_0 (vicii-fetch.c:110).
            let i = cycle_get_sprite_num(cycle_flags);
            if self.sprite_dma & (1 << i) != 0 {
                self.sprite[i].mc = (self.sprite[i].mc + 1) & 0x3f;
            }
        }
        if cycle_is_sprite_dma1_dma2(cycle_flags) {
            // sprite_dma_cycle_2 (vicii-fetch.c:133).
            let i = cycle_get_sprite_num(cycle_flags);
            if self.sprite_dma & (1 << i) != 0 {
                self.sprite[i].mc = (self.sprite[i].mc + 1) & 0x3f;
            }
        }
    }

    /// PORT OF: vicii-cycle.c:130 cycle_phi1_fetch — the Phi1 `mc` / `vc` / `vmli`
    /// counter advances ONLY (the actual byte fetches feed the pixel pipeline,
    /// deferred). FetchG advances vmli + vc (vicii_fetch_graphics); the SprDma1
    /// Phi1 cycle advances sprite `mc` (vicii_fetch_sprite_dma_1). These counters
    /// gate idle_state, the graphics fetch addressing, and the sprite turn-off.
    #[inline]
    fn cycle_phi1_fetch_mc(&mut self, cycle_flags: u32) {
        if cycle_is_fetch_g(cycle_flags) {
            if !self.idle_state {
                // vicii_fetch_graphics (vicii-fetch.c:234): vmli++; vc = (vc+1)&0x3ff.
                self.vmli += 1;
                self.vc = (self.vc + 1) & 0x3ff;
            }
            // idle path (vicii_fetch_idle_gfx) advances no counters.
            return;
        }
        if cycle_is_sprite_dma1_dma2(cycle_flags) {
            // vicii_fetch_sprite_dma_1 (vicii-fetch.c:282): mc++ when DMA active.
            let i = cycle_get_sprite_num(cycle_flags);
            if self.sprite_dma & (1 << i) != 0 {
                self.sprite[i].mc = (self.sprite[i].mc + 1) & 0x3f;
            }
        }
    }

    /// PORT OF: vicii-cycle.c:184 check_hborder.
    #[inline]
    fn check_hborder(&mut self) {
        let csel = self.regs[R_CTRL2 as usize] & 0x08 != 0;
        if cycle_is_check_border_l(self.cycle_flags, csel) {
            self.check_vborder_bottom(self.raster_line);
            self.vborder = self.set_vborder;
            if !self.vborder {
                self.main_border = false;
            }
        }
        if cycle_is_check_border_r(self.cycle_flags, csel) {
            self.main_border = true;
        }
    }

    // =========================================================================
    // SECTION — the BA cycle-steal (PORT OF: mainc64cpu.c:194-208 check_ba /
    // viciisc/vicii-cycle.c:628 vicii_steal_cycles).
    // =========================================================================

    /// PORT OF: vicii-cycle.c:628 vicii_steal_cycles, invoked by mainc64cpu.c:194
    /// check_ba when `maincpu_ba_low_flags & MAINCPU_BA_LOW_VICII` is set:
    /// ```c
    /// do { maincpu_clk++; ba_low = vicii_cycle(); } while (ba_low);
    /// ```
    /// Each iteration is ONE stolen master cycle: `clk++` then `vicii_cycle()`
    /// (= `tick()`), looping while BA stays low. Returns the stolen count; the
    /// caller (FullBus::check_ba_before_read) advances the shared clk + CIAs by
    /// it. Clears the VICII BA-low flag (mainc64cpu.c:119). Returns 0 when BA was
    /// not low (no steal). This is the EXACT model — no hardcoded window, no
    /// prefetch-blind over-steal: BA-low is re-sampled from the cycle table each
    /// `tick()`, so the loop ends precisely when `cycle_is_fetch_ba` (or the
    /// sprite BA mask) goes false — which is why the badline-stalled
    /// `STA ($F3),Y` at $E4DD costs 48, not 49.
    pub fn steal_cycles(&mut self) -> u32 {
        if !self.ba_low_flag {
            return 0;
        }
        let mut stolen: u32 = 0;
        loop {
            // VICE order: maincpu_clk++ (the caller folds `stolen` into clk) THEN
            // vicii_cycle(). Each tick() is one stolen cycle that re-samples BA.
            let ba = self.tick();
            stolen += 1;
            if !ba {
                break;
            }
            // Safety cap (a badline window is at most 43 cycles; sprite DMA tail
            // bounded). Mirrors the TS guard. Never reached in correct operation.
            if stolen > 64 {
                break;
            }
        }
        self.ba_low_flag = false;
        stolen
    }

    // =========================================================================
    // SECTION — register R/W (PORT OF: viciisc/vicii-mem.c).
    // =========================================================================

    /// PORT OF: vicii-mem.c:132 update_raster_line — relatch the 9-bit compare.
    #[inline]
    fn update_raster_line(&mut self) {
        let mut new_line = self.regs[R_RASTER as usize] as u16;
        new_line |= ((self.regs[R_CTRL1 as usize] & 0x80) as u16) << 1;
        self.raster_irq_line = new_line;
    }

    /// PORT OF: vicii-mem.c:334 vicii_store. `addr` is the $D000-offset (masked to
    /// 6 bits). Reproduces the register side effects timing depends on.
    pub fn write_reg(&mut self, offset: u8, value: u8) {
        let addr = offset & 0x3f;
        // vicii-mem.c:339 — last_bus_phi2 = value.
        self.last_bus_phi2 = value;

        match addr {
            // store_sprite_x_position_lsb (vicii-mem.c:92).
            0x00 | 0x02 | 0x04 | 0x06 | 0x08 | 0x0a | 0x0c | 0x0e => {
                if value != self.regs[addr as usize] {
                    self.regs[addr as usize] = value;
                    let n = (addr >> 1) as usize;
                    let msb = if self.regs[0x10] & (1 << n) != 0 { 0x100 } else { 0 };
                    self.sprite[n].x = value as u16 | msb;
                }
            }
            // store_sprite_y_position (vicii-mem.c:108).
            0x01 | 0x03 | 0x05 | 0x07 | 0x09 | 0x0b | 0x0d | 0x0f => {
                self.regs[addr as usize] = value;
            }
            // store_sprite_x_position_msb (vicii-mem.c:113).
            0x10 => {
                if value != self.regs[0x10] {
                    self.regs[0x10] = value;
                    let mut b = 0x01u8;
                    for i in 0..8 {
                        let msb = if value & b != 0 { 0x100 } else { 0 };
                        self.sprite[i].x = self.regs[2 * i] as u16 | msb;
                        b <<= 1;
                    }
                }
            }
            // d011_store (vicii-mem.c:145).
            0x11 => {
                self.ysmooth = value & 0x7;
                self.regs[0x11] = value;
                self.update_raster_line();
            }
            // d012_store (vicii-mem.c:158).
            0x12 => {
                if value != self.regs[0x12] {
                    self.regs[0x12] = value;
                    self.update_raster_line();
                }
            }
            // $D013/$D014 light pen — no-op store (vicii-mem.c:255 break).
            0x13 | 0x14 => {}
            // d015_store (vicii-mem.c:171).
            0x15 => self.regs[0x15] = value,
            // d016_store (vicii-mem.c:176).
            0x16 => self.regs[0x16] = value,
            // d017_store (vicii-mem.c:183) — sprite Y-expand + crunch.
            0x17 => self.d017_store(value),
            // d018_store (vicii-mem.c:216).
            0x18 => {
                if self.regs[0x18] != value {
                    self.regs[0x18] = value;
                }
            }
            // d019_store (vicii-mem.c:227) — 1-to-clear ACK.
            0x19 => {
                self.irq_status &= !((value & 0xf) | 0x80);
                self.vicii_irq_set_line();
            }
            // d01a_store (vicii-mem.c:235) — mask (low nibble).
            0x1a => {
                self.regs[0x1a] = value & 0xf;
                self.vicii_irq_set_line();
            }
            // d01b/d01c/d01d (vicii-mem.c:244/251/258).
            0x1b => self.regs[0x1b] = value,
            0x1c => self.regs[0x1c] = value,
            0x1d => self.regs[0x1d] = value,
            // collision_store (vicii-mem.c:265) — read-only.
            0x1e | 0x1f => {}
            // d020/d021 (vicii-mem.c:277/286) — colors, 4-bit.
            0x20 | 0x21 => self.regs[addr as usize] = value & 0x0f,
            // ext_background_store (vicii-mem.c:295).
            0x22 | 0x23 | 0x24 => self.regs[addr as usize] = value & 0x0f,
            // d025/d026 (vicii-mem.c:305/314).
            0x25 | 0x26 => self.regs[addr as usize] = value & 0x0f,
            // sprite_color_store (vicii-mem.c:323).
            0x27 | 0x28 | 0x29 | 0x2a | 0x2b | 0x2c | 0x2d | 0x2e => {
                self.regs[addr as usize] = value & 0x0f
            }
            // default — unused (vicii-mem.c:333 `/* unused */ break`). VICE does
            // NOT write the reg file here; addr is already masked to 0x3f so all
            // offsets above $2E are unused (no $D02F-$D03F effect at non-DTV).
            _ => {}
        }
    }

    /// PORT OF: vicii-mem.c:183 d017_store — sprite Y-expand + the sprite-crunch.
    fn d017_store(&mut self, value: u8) {
        if value == self.regs[0x17] {
            return;
        }
        let mut b = 0x01u8;
        for i in 0..NUM_SPRITES {
            if (value & b == 0) && self.sprite[i].exp_flop == 0 {
                // sprite crunch.
                if cycle_is_check_spr_crunch(self.cycle_flags) {
                    let mc = self.sprite[i].mc;
                    let mcbase = self.sprite[i].mcbase;
                    self.sprite[i].mc = (0x2a & (mcbase & mc)) | (0x15 & (mcbase | mc));
                }
                self.sprite[i].exp_flop = 1;
            }
            b <<= 1;
        }
        self.regs[0x17] = value;
    }

    /// PORT OF: vicii-mem.c:492 read_raster_y.
    #[inline]
    fn read_raster_y(&self) -> u16 {
        self.raster_line
    }

    /// PORT OF: vicii-mem.c:562 vicii_read. `offset` is the $D000-offset.
    pub fn read_reg(&self, offset: u8) -> u8 {
        let addr = offset & 0x3f;
        match addr {
            // Sprite X LSB / Y — straight reg read (vicii-mem.c:566-575).
            0x00 | 0x02 | 0x04 | 0x06 | 0x08 | 0x0a | 0x0c | 0x0e
            | 0x01 | 0x03 | 0x05 | 0x07 | 0x09 | 0x0b | 0x0d | 0x0f
            | 0x10 => self.regs[addr as usize],
            // d01112_read (vicii-mem.c:501).
            0x11 => (self.regs[0x11] & 0x7f) | (((self.read_raster_y() & 0x100) >> 1) as u8),
            0x12 => (self.read_raster_y() & 0xff) as u8,
            // $D013/$D014 light pen — not modeled, returns 0.
            0x13 | 0x14 => 0,
            0x15 => self.regs[0x15],
            0x16 => self.regs[0x16] | 0xc0,
            0x17 => self.regs[0x17],
            0x18 => self.regs[0x18] | 0x01,
            // d019_read (vicii-mem.c:515) — irq_status | 0x70.
            0x19 => self.irq_status | 0x70,
            0x1a => self.regs[0x1a] | 0xf0,
            0x1b => self.regs[0x1b],
            0x1c => self.regs[0x1c],
            0x1d => self.regs[0x1d],
            // d01e_read / d01f_read (vicii-mem.c:520/537). The read-to-clear has
            // side effects (clear_collisions + reg reset) handled in read_reg_mut.
            // For the const read we return the latched collision value | open bits.
            0x1e => self.sprite_sprite_collisions,
            0x1f => self.sprite_background_collisions,
            0x20 => self.regs[0x20] | 0xf0,
            0x21 | 0x22 | 0x23 | 0x24 => self.regs[addr as usize] | 0xf0,
            0x25 | 0x26 => self.regs[addr as usize] | 0xf0,
            0x27 | 0x28 | 0x29 | 0x2a | 0x2b | 0x2c | 0x2d | 0x2e => {
                self.regs[addr as usize] | 0xf0
            }
            _ => 0xff,
        }
    }

    /// PORT OF: vicii-mem.c:520/537 — the read-to-clear collision reads (the
    /// side-effecting variant the live bus must use for $D01E/$D01F). For all
    /// other registers it is identical to `read_reg`. `last_bus_phi2` is updated
    /// (vicii-mem.c:586). Returns the value.
    pub fn read_reg_mut(&mut self, offset: u8) -> u8 {
        let addr = offset & 0x3f;
        let value = match addr {
            0x1e => {
                // d01e_read: latch collisions into reg, schedule clear, return.
                self.regs[0x1e] = self.sprite_sprite_collisions;
                self.clear_collisions = 0x1e;
                // also clears the sscoll IRQ source (VICE vicii_irq_sscoll_clear is
                // NOT called in the SC d01e_read; clear is purely via clear_collisions
                // on the next cycle). Mirror vicii-mem.c:520 exactly.
                self.regs[0x1e]
            }
            0x1f => {
                self.regs[0x1f] = self.sprite_background_collisions;
                self.clear_collisions = 0x1f;
                self.regs[0x1f]
            }
            _ => return self.read_reg(offset),
        };
        self.last_bus_phi2 = value;
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick_n(v: &mut VicII, n: usize) {
        for _ in 0..n {
            v.tick();
        }
    }

    #[test]
    fn cycle_table_ba_window_matches_vice_pal() {
        // VICE cycle_tab_pal: BaFetch set on PAL cycles 12..54 (table index
        // 11..53) at the matrix-fetch BA, and the sprite BA masks elsewhere.
        let t = build_cycle_table();
        // Cycle index 11 (PAL cycle 12) is the first BaFetch.
        assert!(cycle_is_fetch_ba(t[11]), "PAL cycle 12 = first BaFetch");
        assert!(cycle_is_fetch_ba(t[53]), "PAL cycle 54 = last BaFetch");
        assert!(!cycle_is_fetch_ba(t[10]), "PAL cycle 11 (refresh) = no BaFetch");
        assert!(!cycle_is_fetch_ba(t[54]), "PAL cycle 55 = sprite BA, no BaFetch");
        // FetchC (Phi2 c-access) on cycles 15..54 (index 14..53).
        assert!(cycle_may_fetch_c(t[14]), "PAL cycle 15 = first c-access");
        assert!(cycle_may_fetch_c(t[53]), "PAL cycle 54 = last c-access");
        assert!(!cycle_may_fetch_c(t[13]), "PAL cycle 14 = no c-access yet");
    }

    #[test]
    fn cycle_table_flags_positions() {
        let t = build_cycle_table();
        // UpdateVc on PAL cycle 14 (index 13).
        assert!(cycle_is_update_vc(t[13]));
        // UpdateRc + ChkSprDisp on PAL cycle 58 (index 57).
        assert!(cycle_is_update_rc(t[57]));
        assert!(cycle_is_check_spr_disp(t[57]));
        // ChkSprDma on cycles 55 & 56 (index 54, 55).
        assert!(cycle_is_check_spr_dma(t[54]));
        assert!(cycle_is_check_spr_dma(t[55]));
        // ChkSprExp + UpdateMcBase positions.
        assert!(cycle_is_check_spr_exp(t[55]));
        assert!(cycle_is_update_mcbase(t[15]));
        // ChkSprCrunch on cycle 15 (index 14).
        assert!(cycle_is_check_spr_crunch(t[14]));
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
    fn frame_is_periodic_19656_cycles() {
        // VICE's start-of-frame is table-positioned: end_of_line latches
        // start_of_frame at line 311/cycle 0, and vicii_cycle_start_of_frame
        // resets raster_line=0 at the next cycle (pal_cycle 2). So the PHASE of
        // when raster_line first reads 0 is offset by the start-of-frame latch,
        // but the PERIOD is exactly 19656 cycles. Assert true periodicity: the
        // (raster_line, raster_cycle) pair after N cycles equals the pair after
        // N+19656 cycles, and `frame` advances by exactly 1 per 19656 cycles.
        let mut a = VicII::new();
        let mut b = VicII::new();
        tick_n(&mut a, 5000); // arbitrary phase
        tick_n(&mut b, 5000 + 19656); // one full PAL frame later
        assert_eq!(a.raster_line, b.raster_line, "raster_line periodic at 19656");
        assert_eq!(a.raster_cycle, b.raster_cycle, "raster_cycle periodic at 19656");
        assert_eq!(b.frame, a.frame + 1, "exactly one frame elapsed per 19656 cycles");
    }

    #[test]
    fn frame_counter_advances_once_per_frame() {
        // start_of_frame fires (frame++) once per 19656-cycle frame. From a cold
        // start at line0/cycle0, the FIRST wrap (vicii_cycle_start_of_frame, which
        // runs at the table-positioned start-of-frame cycle) lands at clk 19657 —
        // 1 cycle past nominal, the cold-start phase. Period is exactly 19656.
        let mut v = VicII::new();
        tick_n(&mut v, 19656);
        assert_eq!(v.frame, 0, "frame not yet wrapped at exactly 19656 (cold phase)");
        v.tick();
        assert_eq!(v.frame, 1, "frame wraps at clk 19657 from cold start");
    }

    #[test]
    fn pal_frame_is_19656_cycles() {
        assert_eq!(PAL_CYCLES_PER_LINE as u32 * PAL_SCREEN_HEIGHT as u32, 19656);
    }

    #[test]
    fn no_badline_without_den() {
        let mut v = VicII::new();
        tick_n(&mut v, 63 * (FIRST_DMA_LINE as usize));
        v.tick();
        assert!(!v.allow_bad_lines, "DEN=0 => allow_bad_lines stays false");
        assert!(!v.bad_line);
    }

    #[test]
    fn badline_on_first_dma_line_with_den_and_ysmooth_zero() {
        let mut v = VicII::new();
        v.write_reg(R_CTRL1, 0x10); // DEN=1, YSCROLL=0
        // Run into line 0x30, cycle 0 (where start-of-line + DEN check runs).
        while !(v.raster_line == FIRST_DMA_LINE && v.raster_cycle == 0) {
            v.tick();
        }
        // Walk one more cycle so the badline check (post-start-of-line) has run.
        v.tick();
        assert!(v.allow_bad_lines, "DEN seen on first_dma_line sets allow_bad_lines");
        assert!(v.bad_line, "line 0x30, ysmooth 0 => badline");
    }

    #[test]
    fn ba_low_window_present_on_badline() {
        let mut v = VicII::new();
        v.write_reg(R_CTRL1, 0x10); // DEN=1, YSCROLL=0
        while !(v.raster_line == FIRST_DMA_LINE && v.raster_cycle == 0) {
            v.tick();
        }
        let mut ba_cycles = Vec::new();
        for _ in 0..PAL_CYCLES_PER_LINE {
            let ba = v.tick();
            if ba {
                ba_cycles.push(v.raster_cycle);
            }
        }
        // On a badline BA is low for the matrix fetch window: PAL cycles 12..54.
        assert!(ba_cycles.contains(&pal_cycle(12)), "BA low at cycle 12");
        assert!(ba_cycles.contains(&pal_cycle(54)), "BA low through cycle 54");
        assert!(!ba_cycles.contains(&pal_cycle(11)), "BA high at cycle 11 (refresh)");
        assert!(!ba_cycles.contains(&0), "BA high at start of line");
    }

    #[test]
    fn no_ba_low_on_non_badline() {
        let mut v = VicII::new();
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
        v.write_reg(R_IRQ_MASK, 0x01);
        v.write_reg(R_RASTER, 5);
        assert_eq!(v.raster_irq_line, 5);
        // Run to the cycle where raster_line becomes 5 and the edge fires.
        while v.raster_line != 5 {
            v.tick();
        }
        // The trigger fires on the line==compare edge inside vicii_cycle.
        assert!(v.irq_status & IRQ_RASTER != 0, "raster IRQ latched");
        assert!(v.irq_line, "IRQ line asserted (enabled + latched)");
        assert!(v.read_reg(R_IRQ_STATUS) & IRQ_SUMMARY != 0);
    }

    #[test]
    fn raster_irq_ack_clears_latch() {
        let mut v = VicII::new();
        v.write_reg(R_IRQ_MASK, 0x01);
        v.write_reg(R_RASTER, 5);
        while v.raster_line != 5 {
            v.tick();
        }
        assert!(v.irq_line);
        v.write_reg(R_IRQ_STATUS, 0x01);
        assert_eq!(v.irq_status & IRQ_RASTER, 0, "latch cleared");
        assert!(!v.irq_line, "IRQ line deasserted after ack");
    }

    #[test]
    fn d011_rst8_extends_raster_irq_line_to_9_bits() {
        let mut v = VicII::new();
        v.write_reg(R_CTRL1, 0x80);
        v.write_reg(R_RASTER, 0x00);
        assert_eq!(v.raster_irq_line, 256);
    }

    #[test]
    fn d012_read_reflects_live_raster() {
        let mut v = VicII::new();
        tick_n(&mut v, 63 * 10);
        assert_eq!(v.read_reg(R_RASTER), 10);
    }

    #[test]
    fn sprite_dma_turns_on_for_enabled_sprite_at_matching_y() {
        let mut v = VicII::new();
        v.write_reg(R_SP_ENABLE, 0x01);
        v.write_reg(0x01, 100); // sprite 0 Y
        // DMA turns on at check_sprite_dma (cycles 55/56) of the matching Y line.
        while !(v.raster_line == 100 && v.raster_cycle == pal_cycle(57)) {
            v.tick();
        }
        assert_eq!(v.sprite_dma & 0x01, 0x01, "sprite 0 DMA on at its Y line");
    }

    #[test]
    fn sprite_dma_turns_off_at_mcbase_63() {
        // A non-Y-expanded sprite runs its 21 lines then turns DMA off when
        // mcbase reaches 63 (sprite_mcbase_update). We just assert the lifecycle
        // is bounded and the turn-off path executes without panic.
        let mut v = VicII::new();
        v.write_reg(R_SP_ENABLE, 0x01);
        v.write_reg(0x01, 50);
        // Run enough lines to see the full sprite-DMA lifecycle: on at Y=50,
        // 21 active data lines, then mcbase==63 turn-off. 80 lines covers it.
        let mut saw_on = false;
        let mut saw_off_after_on = false;
        for _ in 0..(63 * 80) {
            v.tick();
            if v.sprite_dma & 1 != 0 {
                saw_on = true;
            } else if saw_on {
                saw_off_after_on = true;
            }
        }
        assert!(saw_on, "sprite DMA turned on");
        assert!(saw_off_after_on, "sprite DMA turned off after its lines");
    }

    #[test]
    fn reg_kind_classification_matches_ts() {
        assert_eq!(VicII::reg_kind(R_RASTER), Some(VicRegKind::Raster));
        assert_eq!(VicII::reg_kind(R_CTRL1), Some(VicRegKind::Raster));
        assert_eq!(VicII::reg_kind(R_CTRL2), Some(VicRegKind::Mode));
        assert_eq!(VicII::reg_kind(R_MEM_PTR), Some(VicRegKind::Mode));
        assert_eq!(VicII::reg_kind(R_IRQ_STATUS), Some(VicRegKind::Irq));
        assert_eq!(VicII::reg_kind(R_IRQ_MASK), Some(VicRegKind::Irq));
        assert_eq!(VicII::reg_kind(0x20), None);
    }

    #[test]
    fn vic_path_no_badline_matches_plain_path() {
        use crate::{Machine, NullSink};
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
        let prog = [0x78u8, 0xA9, 0x1B, 0x8D, 0x11, 0xD0, 0x4C, 0x05, 0x08];
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
            "badline steal: vic path clk {} must exceed plain path clk {}",
            b.clk,
            a.clk
        );
    }
}
