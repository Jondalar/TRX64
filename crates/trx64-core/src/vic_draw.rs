//! vic_draw.rs — VERBATIM per-cycle VIC-II pixel + sprite + border draw.
//!
//! This is a 1:1 port of VICE's viciisc/vicii-draw-cycle.c (cross-checked against
//! the c64re literal port vic/literal/vicii-draw-cycle.ts). Each VIC master cycle,
//! `vicii_draw_cycle` emits 8 pixels — the graphics shift register, the 8 sprite
//! shift registers (with MC / X-expand / Y-expand / priority), and the border unit
//! — into the chip's `dbuf` framebuffer, AND accumulates the $D01E/$D01F collision
//! latches. The pipeline runs ONE cycle behind (it consumes `cycle_flags_pipe`,
//! the previous cycle's flags), exactly like VICE.
//!
//! SOURCE OF TRUTH (every fn cites its line):
//!   - vice/src/viciisc/vicii-draw-cycle.c
//!   - C64ReverseEngineeringMCP/.../vic/literal/vicii-draw-cycle.ts
//!
//! The module-static pipeline state of the C lives on `VicII` (so the chip stays
//! Clone-able for COW forks); the free functions here take `&mut VicII`. The
//! already-fetched buffers (`v.gbuf`, `v.vbuf[]`, `v.cbuf[]`, `v.sprite[].data`)
//! are produced by the Φ1/Φ2 fetches in `vic.rs::tick()` before the draw runs.
//!
//! Pure / sync / deterministic. No I/O.

use crate::render::{FB_H, FB_W};
use crate::vic::{
    cycle_get_sprite_num, cycle_get_xpos, cycle_is_check_spr_disp, cycle_is_sprite_dma1_dma2,
    cycle_is_sprite_ptr_dma0, cycle_is_visible, VicII,
};

// ── Colour resolution tokens (vicii-draw-cycle.c:30-46) ────────────────────────
// render_buffer holds these tokens; draw_colors resolves them via cregs[].
const COL_NONE: u8 = 0x10;
const COL_VBUF_L: u8 = 0x11;
const COL_VBUF_H: u8 = 0x12;
const COL_CBUF: u8 = 0x13;
const COL_CBUF_MC: u8 = 0x14;
const COL_D02X_EXT: u8 = 0x15;
const COL_D020: u8 = 0x20;
const COL_D021: u8 = 0x21;
const COL_D022: u8 = 0x22;
const COL_D023: u8 = 0x23;
const COL_D025: u8 = 0x25;
const COL_D026: u8 = 0x26;
const COL_D027: u8 = 0x27;

/// vicii-draw-cycle.c:133 — the (vmode|px) → COL token table.
const COLORS: [u8; 32] = [
    COL_D021, COL_D021, COL_CBUF, COL_CBUF, // ECM=0 BMM=0 MCM=0
    COL_D021, COL_D022, COL_D023, COL_CBUF_MC, // ECM=0 BMM=0 MCM=1
    COL_VBUF_L, COL_VBUF_L, COL_VBUF_H, COL_VBUF_H, // ECM=0 BMM=1 MCM=0
    COL_D021, COL_VBUF_H, COL_VBUF_L, COL_CBUF, // ECM=0 BMM=1 MCM=1
    COL_D02X_EXT, COL_D02X_EXT, COL_CBUF, COL_CBUF, // ECM=1 BMM=0 MCM=0
    COL_NONE, COL_NONE, COL_NONE, COL_NONE, // ECM=1 BMM=0 MCM=1
    COL_NONE, COL_NONE, COL_NONE, COL_NONE, // ECM=1 BMM=1 MCM=0
    COL_NONE, COL_NONE, COL_NONE, COL_NONE, // ECM=1 BMM=1 MCM=1
];

// =============================================================================
// SECTION  draw_graphics()  (vicii-draw-cycle.c:144)
// =============================================================================

/// vicii-draw-cycle.c:144 draw_graphics — emit one graphics pixel `i` into
/// render_buffer/pri_buffer from the gbuf shift register + the pipelined mode.
#[inline]
fn draw_graphics(v: &mut VicII, i: usize) {
    // Load new gbuf/vbuf/cbuf values at offset == xscroll.
    if i as u8 == v.xscroll_pipe {
        v.vbuf_reg = v.vbuf_pipe1_reg;
        v.cbuf_reg = v.cbuf_pipe1_reg;
        v.gbuf_reg = v.gbuf_pipe1_reg;
        v.gbuf_mc_flop = 1;
    }

    // Read pixels depending on the video mode.
    if v.vmode16_pipe2 != 0 {
        if (v.vmode11_pipe & 0x08) != 0 || (v.cbuf_reg & 0x08) != 0 {
            // mc pixels
            if v.gbuf_mc_flop != 0 {
                v.gbuf_pixel_reg = v.gbuf_reg >> 6;
            }
        } else {
            // hires pixels
            v.gbuf_pixel_reg = if v.gbuf_reg & 0x80 != 0 { 3 } else { 0 };
        }
    } else {
        // $d023 glitch kludge (MCM=0 -> 1 transition).
        if (v.vmode11_pipe & 0x08) != 0 || (v.cbuf_reg & 0x08) != 0 {
            v.gbuf_pixel_reg = if v.gbuf_reg & 0x80 != 0 { 2 } else { 0 };
        } else {
            v.gbuf_pixel_reg = if v.gbuf_reg & 0x80 != 0 { 3 } else { 0 };
        }
    }
    let px = v.gbuf_pixel_reg;

    // Shift the graphics buffer.
    v.gbuf_reg = (v.gbuf_reg << 1) & 0xff;
    v.gbuf_mc_flop ^= 1;

    // Determine pixel colour token + priority.
    let vmode = v.vmode11_pipe | v.vmode16_pipe;
    let pixel_pri = px & 0x2;
    let mut cc = COLORS[(vmode | px) as usize];

    match cc {
        COL_NONE => cc = 0,
        COL_VBUF_L => cc = v.vbuf_reg & 0x0f,
        COL_VBUF_H => cc = v.vbuf_reg >> 4,
        COL_CBUF => cc = v.cbuf_reg,
        COL_CBUF_MC => cc = v.cbuf_reg & 0x07,
        COL_D02X_EXT => cc = COL_D021 + (v.vbuf_reg >> 6),
        _ => {}
    }

    v.render_buffer[i] = cc;
    v.pri_buffer[i] = pixel_pri;
}

/// vicii-draw-cycle.c:227 draw_graphics8 — the 8-pixel graphics block, with the
/// mode-register pipelining (vmode11/16 latched mid-block) and the next-cycle
/// gbuf/vbuf/cbuf pipe load.
#[inline]
fn draw_graphics8(v: &mut VicII, cycle_flags: u32) {
    let vis_en = cycle_is_visible(cycle_flags);

    draw_graphics(v, 0);
    draw_graphics(v, 1);
    draw_graphics(v, 2);
    draw_graphics(v, 3);
    // pixel 4
    v.vmode16_pipe = (v.regs[0x16] & 0x10) >> 2;
    if v.color_latency {
        v.vmode11_pipe |= (v.regs[0x11] & 0x60) >> 2;
    }
    draw_graphics(v, 4);
    draw_graphics(v, 5);
    // pixel 6
    if v.color_latency {
        v.vmode11_pipe &= (v.regs[0x11] & 0x60) >> 2;
    }
    draw_graphics(v, 6);
    // pixel 7
    if v.vmode16_pipe != 0 && v.vmode16_pipe2 == 0 {
        v.gbuf_mc_flop = 0;
    }
    v.vmode16_pipe2 = v.vmode16_pipe;
    draw_graphics(v, 7);

    if !v.color_latency {
        v.vmode11_pipe = (v.regs[0x11] & 0x60) >> 2;
    }

    // Shift the pipe.
    v.vbuf_pipe1_reg = v.vbuf_pipe0_reg;
    v.cbuf_pipe1_reg = v.cbuf_pipe0_reg;
    v.gbuf_pipe1_reg = v.gbuf_pipe0_reg;

    // Keep gbuf 0 outside the visible area.
    if vis_en && !v.vborder {
        v.gbuf_pipe0_reg = v.gbuf;
        v.xscroll_pipe = v.regs[0x16] & 0x07;
    } else {
        v.gbuf_pipe0_reg = 0;
    }

    // Only update vbuf/cbuf in the display state.
    if vis_en && !v.vborder {
        if !v.idle_state {
            v.vbuf_pipe0_reg = v.vbuf[v.dmli];
            v.cbuf_pipe0_reg = v.cbuf[v.dmli];
            v.dmli += 1;
        } else {
            v.vbuf_pipe0_reg = 0;
            v.cbuf_pipe0_reg = 0;
        }
    } else {
        v.dmli = 0;
    }
}

// =============================================================================
// SECTION  draw_sprites()  (vicii-draw-cycle.c:304)
// =============================================================================

/// vicii-draw-cycle.c:304 get_trigger_candidates — sprites whose x (rounded to
/// the 8-pixel block) matches xpos.
#[inline]
fn get_trigger_candidates(v: &VicII, xpos: u16) -> u8 {
    let mut candidate_bits = 0u8;
    for s in 0..8 {
        if (xpos & 0x1f8) == (v.sprite_x_pipe[s] & 0x1f8) {
            candidate_bits |= 1 << s;
        }
    }
    candidate_bits
}

/// vicii-draw-cycle.c:318 trigger_sprites — start rendering a pending sprite on
/// the exact-xpos match.
#[inline]
fn trigger_sprites(v: &mut VicII, xpos: u16, candidate_bits: u8) {
    if candidate_bits == 0 || v.sprite_pending_bits == 0 {
        return;
    }
    for s in 0..8 {
        let m = 1u8 << s;
        if (candidate_bits & m) != 0
            && (v.sprite_pending_bits & m) != 0
            && (v.sprite_active_bits & m) == 0
            && (v.sprite_halt_bits & m) == 0
            && xpos == v.sprite_x_pipe[s]
        {
            v.sbuf_expx_flops |= m;
            v.sbuf_mc_flops |= m;
            v.sprite_active_bits |= m;
        }
    }
}

/// vicii-draw-cycle.c:342 draw_sprites — emit the winning sprite pixel at `i`
/// (with priority vs the foreground graphics pixel) and accumulate collisions.
#[inline]
fn draw_sprites(v: &mut VicII, i: usize) {
    if v.sprite_active_bits == 0 {
        return;
    }

    let mut active_sprite: i32 = -1;
    let mut collision_mask: u8 = 0;
    for s in (0..8).rev() {
        let m = 1u8 << s;
        if v.sprite_active_bits & m == 0 {
            continue;
        }
        // Render pixels if the shift register / pixel reg still has data.
        if v.sbuf_reg[s] != 0 || v.sbuf_pixel_reg[s] != 0 {
            if v.sprite_halt_bits & m == 0 {
                if v.sbuf_expx_flops & m != 0 {
                    if v.sprite_mc_bits & m != 0 {
                        if v.sbuf_mc_flops & m != 0 {
                            // fetch 2 bits
                            v.sbuf_pixel_reg[s] = ((v.sbuf_reg[s] >> 22) & 0x03) as u8;
                        }
                        v.sbuf_mc_flops ^= m;
                    } else {
                        // fetch 1 bit -> 0 or 2
                        v.sbuf_pixel_reg[s] = (((v.sbuf_reg[s] >> 23) & 0x01) << 1) as u8;
                    }
                }

                // Shift + handle expansion flags.
                if v.sbuf_expx_flops & m != 0 {
                    v.sbuf_reg[s] = (v.sbuf_reg[s] << 1) & 0xffff_ffff;
                }
                if v.sprite_expx_bits & m != 0 {
                    v.sbuf_expx_flops ^= m;
                } else {
                    v.sbuf_expx_flops |= m;
                }
            }

            if v.sbuf_pixel_reg[s] != 0 {
                active_sprite = s as i32;
                collision_mask |= m;
            }
        } else {
            v.sprite_active_bits &= !m;
        }
    }

    if collision_mask != 0 {
        let pixel_pri = v.pri_buffer[i];
        let asx = active_sprite as usize;
        let spri = v.sprite_pri_bits & (1 << asx);
        if !(pixel_pri != 0 && spri != 0) {
            match v.sbuf_pixel_reg[asx] {
                1 => v.render_buffer[i] = COL_D025,
                2 => v.render_buffer[i] = COL_D027 + asx as u8,
                3 => v.render_buffer[i] = COL_D026,
                _ => {}
            }
        }
        // Foreground pixel under a sprite -> sprite-background collision.
        if pixel_pri != 0 {
            v.sprite_background_collisions |= collision_mask;
        }
    }

    // 2+ opaque sprites -> sprite-sprite collision.
    if collision_mask & collision_mask.wrapping_sub(1) != 0 {
        v.sprite_sprite_collisions |= collision_mask;
    }
}

/// vicii-draw-cycle.c:433 update_sprite_mc_bits_6569.
#[inline]
fn update_sprite_mc_bits_6569(v: &mut VicII) {
    let next_mc_bits = v.regs[0x1c];
    let toggled = next_mc_bits ^ v.sprite_mc_bits;
    v.sbuf_mc_flops &= !toggled;
    v.sprite_mc_bits = next_mc_bits;
}

/// vicii-draw-cycle.c:442 update_sprite_mc_bits_8565.
#[inline]
fn update_sprite_mc_bits_8565(v: &mut VicII) {
    let next_mc_bits = v.regs[0x1c];
    let toggled = next_mc_bits ^ v.sprite_mc_bits;
    v.sbuf_mc_flops ^= toggled & !v.sbuf_expx_flops;
    v.sprite_mc_bits = next_mc_bits;
}

/// vicii-draw-cycle.c:451 update_sprite_data — load the freshly-DMA'd sprite data
/// into the shift register at the DMA1/DMA2 Φ1 cycle.
#[inline]
fn update_sprite_data(v: &mut VicII, cycle_flags: u32) {
    if cycle_is_sprite_dma1_dma2(cycle_flags) {
        let s = cycle_get_sprite_num(cycle_flags);
        v.sbuf_reg[s] = v.sprite[s].data;
    }
}

/// vicii-draw-cycle.c:459 update_sprite_xpos — pipe the sprite X positions.
#[inline]
fn update_sprite_xpos(v: &mut VicII) {
    for s in 0..8 {
        v.sprite_x_pipe[s] = v.sprite[s].x;
    }
}

/// vicii-draw-cycle.c:469 draw_sprites8 — the 8-pixel sprite block: per-pixel
/// trigger + draw, the DMA halt/active bookkeeping, and the per-cycle pri/expx/mc
/// register pipelining.
#[inline]
fn draw_sprites8(v: &mut VicII, cycle_flags: u32) {
    let xpos = cycle_get_xpos(cycle_flags);
    let spr_en = cycle_is_check_spr_disp(cycle_flags);

    let mut dma_cycle_0 = 0u8;
    let mut dma_cycle_2 = 0u8;
    if cycle_is_sprite_ptr_dma0(cycle_flags) {
        dma_cycle_0 = 1 << cycle_get_sprite_num(cycle_flags);
    }
    if cycle_is_sprite_dma1_dma2(cycle_flags) {
        dma_cycle_2 = 1 << cycle_get_sprite_num(cycle_flags);
    }
    let candidate_bits = get_trigger_candidates(v, xpos);

    // pixel 0
    trigger_sprites(v, xpos, candidate_bits);
    draw_sprites(v, 0);
    // pixel 1
    trigger_sprites(v, xpos + 1, candidate_bits);
    draw_sprites(v, 1);
    // pixel 2
    v.sprite_active_bits &= !dma_cycle_2;
    trigger_sprites(v, xpos + 2, candidate_bits);
    draw_sprites(v, 2);
    // pixel 3
    v.sprite_halt_bits |= dma_cycle_0;
    trigger_sprites(v, xpos + 3, candidate_bits);
    draw_sprites(v, 3);
    // pixel 4
    if spr_en {
        v.sprite_pending_bits = v.sprite_display_bits;
    }
    update_sprite_data(v, cycle_flags);
    trigger_sprites(v, xpos + 4, candidate_bits);
    draw_sprites(v, 4);
    // pixel 5
    trigger_sprites(v, xpos + 5, candidate_bits);
    draw_sprites(v, 5);
    // pixel 6
    if !v.color_latency {
        update_sprite_mc_bits_8565(v);
    }
    v.sprite_pri_bits = v.regs[0x1b];
    v.sprite_expx_bits = v.regs[0x1d];
    trigger_sprites(v, xpos + 6, candidate_bits);
    draw_sprites(v, 6);
    // pixel 7
    if v.color_latency {
        update_sprite_mc_bits_6569(v);
    }
    v.sprite_halt_bits &= !dma_cycle_2;
    trigger_sprites(v, xpos + 7, candidate_bits);
    draw_sprites(v, 7);

    update_sprite_xpos(v);
}

// =============================================================================
// SECTION  draw_border()  (vicii-draw-cycle.c:541)
// =============================================================================

/// vicii-draw-cycle.c:541 draw_border8 — overwrite render_buffer with the border
/// colour token across a border transition (CSEL handling for the partial-cycle
/// left/right edge).
#[inline]
fn draw_border8(v: &mut VicII) {
    let csel = v.regs[0x16] & 0x8;

    // Early exit: no border this cycle.
    if !(v.border_state != 0 || v.main_border) {
        return;
    }
    // Early exit: continuous border.
    if v.border_state != 0 && v.main_border {
        v.render_buffer.fill(COL_D020);
        return;
    }

    // Border transition.
    if csel != 0 {
        if v.border_state != 0 {
            v.render_buffer.fill(COL_D020);
        }
        v.border_state = u8::from(v.main_border);
    } else {
        if v.border_state != 0 {
            for k in 0..7 {
                v.render_buffer[k] = COL_D020;
            }
        }
        v.border_state = u8::from(v.main_border);
        if v.border_state != 0 {
            v.render_buffer[7] = COL_D020;
        }
    }
}

// =============================================================================
// SECTION  draw_colors()  (vicii-draw-cycle.c:585)
// =============================================================================

/// vicii-draw-cycle.c:585 update_cregs — pull the last colour-register write into
/// the draw's local last_color_reg/value, then arm for the next.
#[inline]
fn update_cregs(v: &mut VicII) {
    v.draw_last_color_reg = v.last_color_reg;
    v.draw_last_color_value = v.last_color_value;
    v.last_color_reg = 0xff;
}

/// vicii-draw-cycle.c:592 draw_colors_6569 — resolve the pipelined pixel ring
/// (one-cycle latency) and write to dbuf. `base` = the dbuf line+offset origin.
#[inline]
fn draw_colors_6569(v: &mut VicII, base: usize, i: usize) {
    let lookup_index = (i + 1) & 0x07;
    v.pixel_buffer[lookup_index] = v.cregs[v.pixel_buffer[lookup_index] as usize];
    if base + i < FB_W * FB_H {
        v.dbuf[base + i] = v.pixel_buffer[i];
    }
    v.pixel_buffer[i] = v.render_buffer[i];
}

/// vicii-draw-cycle.c:606 draw_colors_8565 — the no-latency path (grey-dot).
#[inline]
fn draw_colors_8565(v: &mut VicII, base: usize, i: usize) {
    let lookup_index = i;
    if i == 0 && v.pixel_buffer[lookup_index] == v.draw_last_color_reg {
        v.pixel_buffer[lookup_index] = 0x0f;
    } else {
        v.pixel_buffer[lookup_index] = v.cregs[v.pixel_buffer[lookup_index] as usize];
    }
    if base + i < FB_W * FB_H {
        v.dbuf[base + i] = v.pixel_buffer[i];
    }
    v.pixel_buffer[i] = v.render_buffer[i];
}

/// vicii-draw-cycle.c:626 draw_colors8 — the 8-pixel colour-resolve + dbuf store,
/// then advance dbuf_offset by 8.
#[inline]
fn draw_colors8(v: &mut VicII) {
    let offs = v.dbuf_offset;
    // Guard (= VICE: offs > VICII_DRAW_BUFFER_SIZE - 8). Our dbuf is a full frame;
    // a line never exceeds FB_W, so the per-line offset stays < FB_W-8.
    if offs > FB_W - 8 {
        return;
    }
    let base = v.dbuf_line * FB_W + offs;

    // Apply a pending colour-register write to cregs (VICE draw_colors8:682).
    if v.draw_last_color_reg != 0xff {
        v.cregs[v.draw_last_color_reg as usize] = v.draw_last_color_value;
    }

    if v.color_latency {
        draw_colors_6569(v, base, 0);
        draw_colors_6569(v, base, 1);
        draw_colors_6569(v, base, 2);
        draw_colors_6569(v, base, 3);
        draw_colors_6569(v, base, 4);
        draw_colors_6569(v, base, 5);
        draw_colors_6569(v, base, 6);
        draw_colors_6569(v, base, 7);
    } else {
        draw_colors_8565(v, base, 0);
        draw_colors_8565(v, base, 1);
        draw_colors_8565(v, base, 2);
        draw_colors_8565(v, base, 3);
        draw_colors_8565(v, base, 4);
        draw_colors_8565(v, base, 5);
        draw_colors_8565(v, base, 6);
        draw_colors_8565(v, base, 7);
    }
    v.dbuf_offset += 8;

    update_cregs(v);
}

// =============================================================================
// SECTION  vicii_draw_cycle()  (vicii-draw-cycle.c:672)
// =============================================================================

/// vicii-draw-cycle.c:672 vicii_draw_cycle — the per-cycle draw entry. Resets the
/// dbuf offset at raster_cycle 1 (capturing the line being drawn), runs the four
/// 8-pixel passes on the LAGGED `cycle_flags_pipe`, then advances the pipe.
pub(crate) fn vicii_draw_cycle(v: &mut VicII) {
    // Reset rendering on raster cycle 1. Capture the line whose pixels we are
    // about to lay down so the full-frame dbuf indexes the correct row. (VICE
    // flushes a single-line buffer per line; we accumulate into a full frame.)
    if v.raster_cycle == 1 {
        v.dbuf_offset = 0;
        v.dbuf_line = v.raster_line as usize;
    }

    let cycle_flags = v.cycle_flags_pipe;

    draw_graphics8(v, cycle_flags);
    draw_sprites8(v, cycle_flags);
    draw_border8(v);
    draw_colors8(v);

    v.cycle_flags_pipe = v.cycle_flags;
}
