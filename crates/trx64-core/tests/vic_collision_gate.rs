//! Behavioral validation of the $D01E (sprite-sprite) / $D01F (sprite-background)
//! collision registers, exercised end-to-end through the CPU on the VIC-isolated
//! bus (the same path the iso-vic gates use). A 6502 routine programs sprites,
//! reads the collision registers via the live bus, and stores the results to RAM
//! — verifying the registers are COMPUTED, READ-CLEAR on read, and (for $D01E)
//! latch the sprite-sprite overlap bitmask.
//!
//! Source of truth (ported verbatim): c64re/vic/literal/vicii-draw-cycle.ts
//! draw_sprites() (the gate authority) + VICE viciisc vicii-cycle.c collision
//! IRQ edge-trigger. See render::render_collisions / vic::apply_collisions.

use trx64_core::{Machine, NullSink};

/// Lay sprite DMA data + a video-matrix/sprite-pointer setup into flat RAM, then
/// run a CPU routine that reads $D01E + $D01F, disables the sprites, then re-reads
/// $D01E to prove the read-CLEAR took (with the sprites off, the per-read
/// re-render contributes no new collisions, so the cleared latch stays 0 — this
/// is exactly the hardware semantic: clear-on-read, then re-accumulate as sprites
/// are redrawn). Results land in $C000.. .
///
/// Routine (assembled below) at $0800. NOTE: every $D01E/$D01F read re-renders
/// the frame and re-accumulates collisions (faithful — the raster continuously
/// redraws sprites), so to prove read-CLEAR we DISABLE the sprites between the two
/// reads of the SAME register; with the sprites off the post-clear re-render
/// contributes nothing and the cleared latch stays 0.
///
///   $C000 ← $D01E         (sprite-sprite, latches + schedules clear + IRQ edge)
///   $C002 ← $D019         (IRQ latch — collision IRQ sources, bit 2)
///   $D015 ← 0             (disable all sprites)
///   $C003 ← $D01E         (re-read: must be 0 — read-cleared, no re-collide)
///   $C001 ← $D01F         (sprite-background — 0 here, see dedicated test)
///   JMP self
fn run_collision_probe(setup: impl FnOnce(&mut Machine)) -> [u8; 4] {
    let mut m = Machine::new();
    setup(&mut m);

    let prog = [
        0x78u8, // SEI
        0xAD, 0x1F, 0xD0, // LDA $D01F        (sample $D01F first, before disable)
        0x8D, 0x01, 0xC0, // STA $C001
        0xAD, 0x1E, 0xD0, // LDA $D01E        (sprite-sprite latch + clear + IRQ)
        0x8D, 0x00, 0xC0, // STA $C000
        0xAD, 0x19, 0xD0, // LDA $D019        (IRQ latch after the collision read)
        0x8D, 0x02, 0xC0, // STA $C002
        0xA9, 0x00, // LDA #$00
        0x8D, 0x15, 0xD0, // STA $D015        (disable all sprites)
        0xAD, 0x1E, 0xD0, // LDA $D01E        (re-read: read-cleared, no re-collide)
        0x8D, 0x03, 0xC0, // STA $C003
        0x4C, 0x1E, 0x08, // JMP $081E (self)
    ];
    m.poke(0x0800, &prog);
    m.set_pc(0x0800);

    let mut o = NullSink;
    // A handful of instructions is enough to execute the linear probe; the run is
    // instruction-stepped on the VIC-isolated bus (raster/sprites advance per
    // master cycle). Budget generously then read back RAM.
    m.run_for_vic(2000, &mut o);

    [
        m.read_full(0xc000), // $D01E sprite-sprite
        m.read_full(0xc001), // $D01F sprite-background
        m.read_full(0xc002), // $D019 IRQ latch (collision sources)
        m.read_full(0xc003), // $D01E re-read (read-clear proof)
    ]
}

/// Program two solid sprites at the same X/Y over flat RAM. RAM-resident data
/// (sprite DMA bytes, sprite pointers, screen RAM, colour RAM) is `poke`d to flat
/// RAM — which is exactly what the VIC-isolated bus (`VicBus { mem: &ram }`)
/// reads. The VIC REGISTERS ($D000-$D3FF) are written to the chip via `poke_io`
/// (→ `vic.write_reg`), since on the isolated path the VIC owns that window.
fn program_two_overlapping_sprites(m: &mut Machine) {
    // Sprite DMA data: solid 24×21 block at $0340 (= ptr $0D).
    let data = [0xffu8; 63];
    m.poke(0x0340, &data);
    // Sprite pointers in the (bank-0) video matrix at $07F8 (sprite 0) / $07F9.
    m.poke(0x07f8, &[0x0d, 0x0d]);
    // Screen RAM: spaces (no foreground graphics) for sprite-sprite isolation.
    let blanks = [0x20u8; 1000];
    m.poke(0x0400, &blanks);

    // VIC registers (→ vic.regs via write_reg).
    m.poke_io(0xd011, &[0x1b]); // DEN=1 RSEL=1 YSCROLL=3
    m.poke_io(0xd016, &[0xc8]); // CSEL=1 XSCROLL=0
    m.poke_io(0xd018, &[0x14]); // screen $0400, char $1000
    m.poke_io(0xd020, &[14]); // border
    m.poke_io(0xd021, &[6]); // background
    // sprite 0 + 1 at the same position (full overlap).
    m.poke_io(0xd000, &[0x60, 0x60]); // sp0 X=96, sp0 Y=96
    m.poke_io(0xd002, &[0x60, 0x60]); // sp1 X=96, sp1 Y=96
    m.poke_io(0xd027, &[2, 7]); // sprite colours
    m.poke_io(0xd015, &[0x03]); // enable sprites 0 + 1
}

#[test]
fn d01e_sprite_sprite_collision_computed_and_read_clears() {
    let r = run_collision_probe(program_two_overlapping_sprites);
    // $D01E read = both sprites overlapping → bits 0 and 1 set.
    assert_eq!(r[0], 0x03, "$D01E = sprite-sprite overlap (bits 0+1)");
    // $D01F = 0 (blank background, no foreground graphics under the sprites).
    assert_eq!(r[1], 0x00, "$D01F = 0 (no foreground under sprites)");
    // $D019 (IRQ latch) bit 2 = sprite-sprite collision IRQ source set. Open bits
    // $70 are always read as 1 (d019_read = irq_status | 0x70).
    assert_eq!(
        r[2] & 0x04,
        0x04,
        "$D019 bit2 = sprite-sprite collision IRQ source latched"
    );
    // $D01E re-read AFTER read-clear + sprites disabled = 0 (read-clears on read —
    // vicii-mem.c d01e_read; no re-accumulation with sprites off).
    assert_eq!(r[3], 0x00, "$D01E read-clears on read");
}

#[test]
fn d01f_sprite_background_collision_computed() {
    let r = run_collision_probe(|m| {
        // One solid sprite over a solid foreground glyph.
        let data = [0xffu8; 63];
        m.poke(0x0340, &data);
        m.poke(0x07f8, &[0x0d]);
        // Screen cell (0,0) = char code 1; we cannot inject CHARGEN on the
        // VIC-isolated path (char_rom is zeroed there), so use a bank where the
        // glyph data is RAM: put char base at $2000 (D018 char bits = 0x08 → char
        // $2000) and write a solid glyph there. char 1 row data at $2000 + 1*8.
        let blanks = [0x20u8; 1000];
        m.poke(0x0400, &blanks);
        m.poke(0x0400, &[1]); // screen[0,0] = char 1
        let solid = [0xffu8; 8];
        m.poke(0x2008, &solid); // char 1 glyph rows (char base $2000)
        // colour RAM (white) for the cell — flat RAM at $D800 (= VicBus reads it).
        m.poke(0xd800, &[1]);

        m.poke_io(0xd011, &[0x1b]);
        m.poke_io(0xd016, &[0xc8]);
        m.poke_io(0xd018, &[0x18]); // screen $0400, char $2000
        m.poke_io(0xd020, &[14]);
        m.poke_io(0xd021, &[6]);
        // sprite 0 at X=24 ($18) Y=50 → dbuf X 136 / line 51 = display cell (0,0).
        m.poke_io(0xd000, &[0x18, 0x32]);
        m.poke_io(0xd027, &[2]);
        m.poke_io(0xd015, &[0x01]); // enable sprite 0
    });
    // $D01F = bit 0 (sprite 0 over foreground graphics).
    assert_eq!(r[1], 0x01, "$D01F = bit0 (sprite over foreground char)");
    // $D01E = 0 (single sprite, no sprite-sprite overlap).
    assert_eq!(r[0], 0x00, "$D01E = 0 (single sprite)");
    // $D019 bit1 = sprite-background collision IRQ source latched.
    assert_eq!(
        r[2] & 0x02,
        0x02,
        "$D019 bit1 = sprite-background collision IRQ source latched"
    );
}
