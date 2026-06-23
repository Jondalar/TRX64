//! render.rs — VIC-II pixel framebuffer (RGBA), pixel-identical to the TS oracle.
//!
//! The cycle-exact timing core (vic.rs) deliberately omits pixel output. This
//! module produces the *displayed image* from a frozen machine state: it builds
//! the same 520×312 internal colour-index buffer the VICE x64sc literal port
//! (`vic/literal/vicii-draw-cycle.ts`) fills per cycle, then crops it to the
//! exact VICE PAL canvas the screenshot pipeline emits and converts to RGBA via
//! the colodore palette.
//!
//! WHY a state-render instead of a per-cycle port: for a STATIC, deterministic
//! screen (the BASIC-ready screen, a frozen game frame) the per-character-cell
//! pixel logic is a pure function of the video matrix + colour RAM + char/bitmap
//! memory + the mode registers. Rendering the whole 40×25 grid once reproduces
//! the literal port's output byte-for-byte for that frame. The geometry constants
//! below are CALIBRATED against the TS golden (the BASIC-ready screen) and match
//! VICE: display origin in the 520-wide draw buffer = dbuf X 136 / line 51, and
//! the screenshot canvas = dbuf[104..488] × line[16..288] = 384×272.
//!
//! Pure / sync / deterministic. No I/O, no allocation beyond the output buffer.

// ── Colodore palette (VICE colodore.vpl, the ONE runtime palette) ──────────────
// Verbatim from palettes.ts COLODORE — the single source of truth. Index order:
// 0 black,1 white,2 red,3 cyan,4 purple,5 green,6 blue,7 yellow,8 orange,9 brown,
// 10 light-red,11 dark-grey,12 mid-grey,13 light-green,14 light-blue,15 light-grey.
pub const COLODORE: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00],
    [0xff, 0xff, 0xff],
    [0x96, 0x28, 0x2e],
    [0x5b, 0xd6, 0xce],
    [0x9f, 0x2d, 0xad],
    [0x41, 0xb9, 0x36],
    [0x27, 0x24, 0xc4],
    [0xef, 0xf3, 0x47],
    [0x9f, 0x48, 0x15],
    [0x5e, 0x35, 0x00],
    [0xda, 0x5f, 0x66],
    [0x47, 0x47, 0x47],
    [0x78, 0x78, 0x78],
    [0x91, 0xff, 0x84],
    [0x68, 0x64, 0xff],
    [0xae, 0xae, 0xae],
];

// ── Internal draw-buffer geometry (= VICE viciisc, vicii-draw-cycle.c) ─────────
/// Internal draw buffer width: 65 cycles × 8 px (= VICII_DRAW_BUFFER_SIZE 520).
pub const FB_W: usize = 520;
/// PAL raster lines.
pub const FB_H: usize = 312;

/// X of display column 0 inside the 520-wide draw buffer (CALIBRATED: the TS
/// golden puts display col 0 at screenshot-canvas X 32, canvas starts at dbuf
/// X 104, so dbuf X = 104 + 32 = 136). Equivalently 17 cycles of border precede
/// the first visible graphics pixel.
pub const DISPLAY_X0: usize = 136;
/// Y (raster line) of display row 0 = VICII_25ROW_START_LINE 0x33 = 51.
pub const DISPLAY_Y0: usize = 51;
/// 24-row top start line (RSEL=0). VICII_24ROW_START_LINE 0x37 = 55.
pub const DISPLAY_Y0_24: usize = 55;
/// 25-row bottom stop line (exclusive). VICII_25ROW_STOP_LINE 0xFB = 251.
pub const V_STOP_25: usize = 251;
/// 24-row bottom stop line (exclusive). VICII_24ROW_STOP_LINE 0xF7 = 247.
pub const V_STOP_24: usize = 247;
/// First DMA / badline raster line (VICII_FIRST_DMA_LINE 0x30 = 48). The content
/// origin line is this + YSCROLL (the first badline at or after DMA start).
pub const VICII_FIRST_DMA_LINE: usize = 48;
/// 38-column (CSEL=0) main-border inset: 7 px on the left, 9 px on the right
/// (40-col window is 320 px; 38-col is 304 px → 16 px trimmed, 7 L + 9 R), matching
/// VICE's draw_border8 CSEL=0 path / the 0x1F..0x14F vs 0x18..0x158 X comparisons.
pub const CSEL0_INSET_L: usize = 7;
pub const CSEL0_INSET_R: usize = 9;
pub const DISPLAY_W: usize = 320;
pub const DISPLAY_H: usize = 200;

/// Draw-buffer X of sprite X-coordinate 0 (CALIBRATED vs the TS oracle: a sprite
/// with X register `sx` lands its leftmost pixel at canvas X `sx + 8`, and canvas
/// X 0 = dbuf X 104, so dbuf X = sx + 8 + 104 = sx + 112). Equivalently sprite
/// X 24 ($18) = display col 0 = dbuf X 136 = the left display edge.
pub const SPRITE_DBUF_X0: usize = 112;

// ── Screenshot crop (= renderLiteralPortRgba, integrated-session.ts) ───────────
/// VICE x64sc PAL canvas: X = dbuf[104..488] (384 px, balanced 32 L/R borders),
/// Y = fb[16..288] (272 px, first displayed PAL line = 16).
pub const CANVAS_X0: usize = 104;
pub const CANVAS_W: usize = 384;
pub const CANVAS_Y0: usize = 16;
pub const CANVAS_H: usize = 272;

/// VIC graphics mode (ECM<<2 | BMM<<1 | MCM), the 3-bit mode select that drives
/// the per-pixel colour logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VicMode {
    /// ECM=0 BMM=0 MCM=0.
    StandardText,
    /// ECM=0 BMM=0 MCM=1.
    MulticolorText,
    /// ECM=1 BMM=0 MCM=0.
    Ecm,
    /// ECM=0 BMM=1 MCM=0.
    StandardBitmap,
    /// ECM=0 BMM=1 MCM=1.
    MulticolorBitmap,
    /// Any "invalid" mode combination (ECM with BMM/MCM): VIC outputs black in
    /// the display window. We render black to match.
    Invalid,
}

/// Everything the renderer needs, lifted from a frozen machine. All memory is
/// addressed as the VIC sees it: a 16 KiB bank base + the char-ROM shadow that
/// appears at $1000-$1FFF / $9000-$9FFF (banks 0 / 2).
pub struct RenderInput<'a> {
    /// VIC register file ($D000-$D02E), index = offset & 0x3f.
    pub regs: &'a [u8; 0x40],
    /// Full 64 KiB RAM (the VIC reads RAM directly, ignoring CPU-port banking).
    pub ram: &'a [u8; 0x10000],
    /// CHARGEN ROM (4 KiB) — appears to the VIC at bank-relative $1000 in banks 0/2.
    pub char_rom: &'a [u8; 0x1000],
    /// Colour RAM low nibbles ($D800-$DBFF), 1024 entries (only low nibble used).
    pub color_ram: &'a [u8; 0x0400],
    /// VIC bank base (0, $4000, $8000, $C000) = (3 - CIA2_PA[1:0]) * $4000.
    pub bank_base: u16,
}

impl<'a> RenderInput<'a> {
    #[inline]
    fn reg(&self, off: u8) -> u8 {
        self.regs[(off & 0x3f) as usize]
    }

    /// Decode the graphics mode from $D011 (ECM bit6, BMM bit5) + $D016 (MCM bit4).
    pub fn mode(&self) -> VicMode {
        let d011 = self.reg(0x11);
        let d016 = self.reg(0x16);
        let ecm = d011 & 0x40 != 0;
        let bmm = d011 & 0x20 != 0;
        let mcm = d016 & 0x10 != 0;
        match (ecm, bmm, mcm) {
            (false, false, false) => VicMode::StandardText,
            (false, false, true) => VicMode::MulticolorText,
            (true, false, false) => VicMode::Ecm,
            (false, true, false) => VicMode::StandardBitmap,
            (false, true, true) => VicMode::MulticolorBitmap,
            _ => VicMode::Invalid,
        }
    }

    /// Screen-RAM (video matrix) base within the VIC bank: ($D018 & 0xF0) << 6.
    #[inline]
    fn screen_base(&self) -> u16 {
        self.bank_base.wrapping_add(((self.reg(0x18) as u16 & 0xf0) << 6) as u16)
    }

    /// Character generator base within the VIC bank: ($D018 & 0x0E) << 10.
    #[inline]
    fn char_base(&self) -> u16 {
        self.bank_base.wrapping_add(((self.reg(0x18) as u16 & 0x0e) << 10) as u16)
    }

    /// Bitmap base within the VIC bank: ($D018 & 0x08) << 10 (= 0 or $2000).
    #[inline]
    fn bitmap_base(&self) -> u16 {
        self.bank_base.wrapping_add(((self.reg(0x18) as u16 & 0x08) << 10) as u16)
    }

    /// VIC memory read: RAM direct, except the char-ROM shadow at bank-relative
    /// $1000-$1FFF in banks 0 ($0000) and 2 ($8000) — there the VIC sees CHARGEN,
    /// not RAM (VICE vbank char-ROM mapping). Used for char-generator fetches.
    #[inline]
    fn vic_read(&self, addr: u16) -> u8 {
        // The char-ROM shadow is at absolute $1000-$1FFF and $9000-$9FFF.
        let lo = addr & 0x3fff;
        if (lo & 0x3000) == 0x1000 && (self.bank_base == 0x0000 || self.bank_base == 0x8000) {
            self.char_rom[(lo - 0x1000) as usize]
        } else {
            self.ram[addr as usize]
        }
    }
}

/// Build the full 520×312 colour-index buffer (one byte per pixel, palette index).
/// Border colour fills everywhere outside the display window; inside, each char /
/// bitmap cell is rendered per the active mode. Returns the index buffer.
pub fn render_index_buffer(inp: &RenderInput) -> Vec<u8> {
    let (fb, _fg) = render_index_and_fg(inp);
    fb
}

/// Build the colour-index buffer AND a parallel "foreground" mask (one byte per
/// pixel: 1 if the graphics pixel has priority over a low-priority sprite, i.e.
/// `px & 0x2` in the VICE draw-cycle). The mask drives sprite-to-background
/// priority and sprite-background collisions. Border pixels are foreground=0.
fn render_index_and_fg(inp: &RenderInput) -> (Vec<u8>, Vec<u8>) {
    let mut fb = vec![0u8; FB_W * FB_H];
    let mut fg = vec![0u8; FB_W * FB_H];
    let border = inp.reg(0x20) & 0x0f;
    fb.fill(border);

    let mode = inp.mode();
    let d011 = inp.reg(0x11);
    let d016 = inp.reg(0x16);
    let den = d011 & 0x10 != 0;
    let rsel = d011 & 0x08 != 0;
    let csel = d016 & 0x08 != 0;

    // Display blanked (DEN=0): the whole window is border colour — nothing drawn.
    if !den {
        return (fb, fg);
    }

    // ── Display geometry: the main-border window and the content origin are
    // INDEPENDENT (this is what makes fine scroll work).
    //
    // Vertical border WINDOW (the lines where graphics, not border, can show):
    //   RSEL=1 → [VICII_25ROW_START_LINE, STOP) = [51, 251)
    //   RSEL=0 → [VICII_24ROW_START_LINE, STOP) = [55, 247)
    // Horizontal border WINDOW (draw-buffer X where graphics can show):
    //   CSEL=1 → [136, 456) (= DISPLAY_X0 .. +320)
    //   CSEL=0 → inset 7 px left / 9 px right → [143, 447)
    //
    // CONTENT origin — where char/bitmap row 0, column 0, sub-row 0 is emitted —
    // is set by the fine-scroll registers, NOT by RSEL/CSEL:
    //   content_y0 = VICII_FIRST_DMA_LINE (48) + YSCROLL  (boot YSCROLL=3 → 51)
    //   content_x0 = DISPLAY_X0 (136) + XSCROLL           (boot XSCROLL=0 → 136)
    // The 25×40 grid is rendered from that origin, then CLIPPED to the border
    // window; lines/columns outside the window keep the border colour. For the
    // boot defaults (RSEL=1 CSEL=1 YSCROLL=3 XSCROLL=0) origin == window start, so
    // this reduces to the previously-calibrated geometry.
    let yscroll = (d011 & 0x07) as usize;
    let xscroll = (d016 & 0x07) as usize;
    let (win_top, win_bot) = if rsel {
        (DISPLAY_Y0, V_STOP_25)
    } else {
        (DISPLAY_Y0_24, V_STOP_24)
    };
    let (win_left, win_right) = if csel {
        (DISPLAY_X0, DISPLAY_X0 + DISPLAY_W) // [136, 456)
    } else {
        (DISPLAY_X0 + CSEL0_INSET_L, DISPLAY_X0 + DISPLAY_W - CSEL0_INSET_R) // [143, 447)
    };
    let content_y0 = VICII_FIRST_DMA_LINE + yscroll;
    let content_x0 = DISPLAY_X0 + xscroll;

    let bg0 = inp.reg(0x21) & 0x0f;
    let bg1 = inp.reg(0x22) & 0x0f;
    let bg2 = inp.reg(0x23) & 0x0f;
    let bg3 = inp.reg(0x24) & 0x0f;

    let screen_base = inp.screen_base();
    let char_base = inp.char_base();
    let bitmap_base = inp.bitmap_base();

    // Pre-fill the border WINDOW interior before drawing content. Two distinct
    // idle fills (verified against the TS oracle for fine scroll):
    //  • Window lines WITHIN the 25-row content band [content_y0, +200): the VIC
    //    is in display state, so an uncovered in-row pixel (the left XSCROLL gap)
    //    shows the BACKGROUND colour ($D021).
    //  • Window lines OUTSIDE the content band but still inside the vertical border
    //    window (the YSCROLL gap above/below the rows): the VIC is in IDLE state
    //    and outputs BLACK (index 0), NOT the background colour.
    let band_top = content_y0;
    let band_bot = content_y0 + DISPLAY_H;
    for line in win_top..win_bot.min(FB_H) {
        let in_band = line >= band_top && line < band_bot;
        let fillc = if in_band { bg0 } else { 0 };
        let lo = line * FB_W;
        for x in win_left..win_right {
            fb[lo + x] = fillc;
        }
    }

    // 25 text rows × 8 raster lines, drawn from the content origin and clipped to
    // the border window. Always 40 columns wide.
    for trow in 0..25usize {
        for sub in 0..8usize {
            let line = content_y0 + trow * 8 + sub;
            if line >= FB_H || line < win_top || line >= win_bot {
                continue; // outside the vertical border window → border colour
            }
            let row_off = line * FB_W + content_x0;
            for col in 0..40usize {
                let vm_index = trow * 40 + col;
                let screen_byte = inp.ram[screen_base.wrapping_add(vm_index as u16) as usize];
                let color = inp.color_ram[vm_index & 0x3ff] & 0x0f;
                let (px, pri) = pixels_for_cell(
                    inp, mode, screen_byte, color, sub, col, trow, char_base, bitmap_base,
                    bg0, bg1, bg2, bg3,
                );
                let base = row_off + col * 8;
                for (i, &c) in px.iter().enumerate() {
                    let x = content_x0 + col * 8 + i;
                    if x < win_left || x >= win_right {
                        continue; // outside the horizontal border window
                    }
                    fb[base + i] = c;
                    fg[base + i] = pri[i] as u8;
                }
            }
        }
    }

    // Sprites are painted on top of the graphics, honouring per-sprite priority
    // ($D01B) against the foreground mask and sprite-sprite priority (lower
    // sprite number wins). Border colour stays untouched where no sprite pixel.
    render_sprites(inp, &mut fb, &fg);

    (fb, fg)
}

/// Paint the 8 hardware sprites onto the draw buffer. Calibrated against the TS
/// oracle (per-cycle literal port): a sprite with X register value `sx` lands its
/// leftmost pixel at draw-buffer column `sx + SPRITE_DBUF_X0`, and its first row
/// at draw-buffer line `sy + 1` (where `sy` is the $D001+2s Y register). Each
/// sprite is 24 px wide (×2 with X-expand) and 21 rows tall (×2 with Y-expand).
///
/// Priority: for sprite `s`, when its priority bit ($D01B bit s) is set AND the
/// underlying graphics pixel is foreground (`fg[..]==1`), the sprite pixel is
/// hidden. Among sprites, the LOWEST-numbered sprite with an opaque pixel wins
/// (matches the VICE draw_sprites `for s = 7..0` last-write-lowest semantics).
fn render_sprites(inp: &RenderInput, fb: &mut [u8], fg: &[u8]) {
    let enable = inp.reg(0x15);
    if enable == 0 {
        return;
    }
    let x_msb = inp.reg(0x10);
    let y_exp = inp.reg(0x17);
    let pri = inp.reg(0x1b);
    let mcm = inp.reg(0x1c);
    let x_exp = inp.reg(0x1d);
    let mc0 = inp.reg(0x25) & 0x0f; // $D025 sprite multicolor 0
    let mc1 = inp.reg(0x26) & 0x0f; // $D026 sprite multicolor 1
    let screen_base = inp.screen_base();

    // Build, per draw-buffer pixel touched, the winning sprite's colour. We paint
    // sprites from highest number (7) down to lowest (0) so the lowest overwrites
    // — exactly the priority order the VICE port produces. The per-pixel hide test
    // against the foreground mask uses the *graphics* fg, evaluated per pixel.
    for s in (0..8usize).rev() {
        let m = 1u8 << s;
        if enable & m == 0 {
            continue;
        }
        let sx = inp.reg(0x00 + 2 * s as u8) as usize | (((x_msb >> s) & 1) as usize) << 8;
        let sy = inp.reg(0x01 + 2 * s as u8) as usize;
        let col = inp.reg(0x27 + s as u8) & 0x0f; // $D027+s sprite colour
        let is_mc = mcm & m != 0;
        let is_xe = x_exp & m != 0;
        let is_ye = y_exp & m != 0;
        let spri = pri & m != 0; // sprite is BEHIND foreground graphics when set

        // Sprite data pointer: screen RAM $3F8+s (in the VIC bank), ×64.
        let ptr = inp.ram[screen_base.wrapping_add(0x3f8 + s as u16) as usize] as u16;
        let data_base = ptr.wrapping_mul(64);

        let dbuf_x0 = sx + SPRITE_DBUF_X0;
        let height = if is_ye { 42 } else { 21 };

        for row in 0..height {
            let data_row = if is_ye { row / 2 } else { row };
            let line = sy + 1 + row;
            if line >= FB_H {
                break;
            }
            // Three data bytes per row: 24 source pixels, MSB-first.
            let b0 = inp.vic_read(data_base.wrapping_add((data_row * 3) as u16));
            let b1 = inp.vic_read(data_base.wrapping_add((data_row * 3 + 1) as u16));
            let b2 = inp.vic_read(data_base.wrapping_add((data_row * 3 + 2) as u16));
            let bits24 = ((b0 as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;

            if is_mc {
                // 12 multicolor pixels of 2 source-px width (×2 again with X-exp).
                for p in 0..12usize {
                    let val = (bits24 >> (22 - p * 2)) & 0x03;
                    let c = match val {
                        0 => continue, // transparent
                        1 => mc0,
                        2 => col,
                        _ => mc1,
                    };
                    let pxw = if is_xe { 4 } else { 2 };
                    let px0 = dbuf_x0 + p * pxw;
                    for k in 0..pxw {
                        put_sprite_px(fb, fg, line, px0 + k, c, spri);
                    }
                }
            } else {
                // 24 hires pixels of 1 source-px width (×2 with X-exp).
                for p in 0..24usize {
                    if (bits24 >> (23 - p)) & 1 == 0 {
                        continue; // transparent
                    }
                    let pxw = if is_xe { 2 } else { 1 };
                    let px0 = dbuf_x0 + p * pxw;
                    for k in 0..pxw {
                        put_sprite_px(fb, fg, line, px0 + k, col, spri);
                    }
                }
            }
        }
    }
}

/// Write one sprite pixel at draw-buffer (line,x), unless it is hidden behind a
/// foreground graphics pixel because the sprite has low priority (`spri`=true and
/// the underlying graphics pixel is foreground). Out-of-range columns are ignored.
#[inline]
fn put_sprite_px(fb: &mut [u8], fg: &[u8], line: usize, x: usize, color: u8, spri: bool) {
    if x >= FB_W {
        return;
    }
    let off = line * FB_W + x;
    if spri && fg[off] != 0 {
        return; // foreground graphics wins over a low-priority sprite
    }
    fb[off] = color;
}

/// Render the 8 horizontal pixels of one character/bitmap cell row. Returns the
/// 8 colour indices AND the 8 per-pixel *foreground* flags (= VICE `px & 0x2`:
/// true for graphics pixels with priority over a low-priority sprite). In hires
/// modes a set bit is foreground; in multicolor modes bit-pairs 10/11 are
/// foreground while 00/01 are background.
#[allow(clippy::too_many_arguments)]
fn pixels_for_cell(
    inp: &RenderInput,
    mode: VicMode,
    screen_byte: u8,
    color: u8,
    sub: usize,
    col: usize,
    trow: usize,
    char_base: u16,
    bitmap_base: u16,
    bg0: u8,
    bg1: u8,
    bg2: u8,
    bg3: u8,
) -> ([u8; 8], [bool; 8]) {
    let mut out = [0u8; 8];
    let mut fg = [false; 8];
    match mode {
        VicMode::StandardText => {
            // Char ROM row: (char<<3 | sub). FG = colour RAM, BG = $D021. MSB left.
            let row = inp.vic_read(char_base.wrapping_add(((screen_byte as u16) << 3) + sub as u16));
            for i in 0..8 {
                let set = row & (0x80 >> i) != 0;
                out[i] = if set { color } else { bg0 };
                fg[i] = set;
            }
        }
        VicMode::MulticolorText => {
            let row = inp.vic_read(char_base.wrapping_add(((screen_byte as u16) << 3) + sub as u16));
            if color & 0x08 == 0 {
                // Colour RAM bit3 = 0 → this cell is hi-res (standard text) with
                // the low 3 bits as foreground.
                let cfg = color & 0x07;
                for i in 0..8 {
                    let set = row & (0x80 >> i) != 0;
                    out[i] = if set { cfg } else { bg0 };
                    fg[i] = set;
                }
            } else {
                // Multicolor: 4 double-wide pixels from bit pairs. 10/11 = fg.
                let cfg = color & 0x07;
                for p in 0..4 {
                    let bits = (row >> (6 - p * 2)) & 0x03;
                    let c = match bits {
                        0 => bg0,
                        1 => bg1,
                        2 => bg2,
                        _ => cfg,
                    };
                    let is_fg = bits & 0x02 != 0;
                    out[p * 2] = c;
                    out[p * 2 + 1] = c;
                    fg[p * 2] = is_fg;
                    fg[p * 2 + 1] = is_fg;
                }
            }
        }
        VicMode::Ecm => {
            // ECM: char code low 6 bits index the glyph; bits 6-7 select bg0..bg3.
            let glyph = (screen_byte & 0x3f) as u16;
            let row = inp.vic_read(char_base.wrapping_add((glyph << 3) + sub as u16));
            let bg = match (screen_byte >> 6) & 0x03 {
                0 => bg0,
                1 => bg1,
                2 => bg2,
                _ => bg3,
            };
            for i in 0..8 {
                let set = row & (0x80 >> i) != 0;
                out[i] = if set { color } else { bg };
                fg[i] = set;
            }
        }
        VicMode::StandardBitmap => {
            // Bitmap byte: base + trow*320 + col*8 + sub. Bit=1 → upper nibble of
            // screen byte (fg), bit=0 → lower nibble (bg).
            let addr = bitmap_base
                .wrapping_add((trow as u16) * 320)
                .wrapping_add((col as u16) * 8)
                .wrapping_add(sub as u16);
            let row = inp.vic_read(addr);
            let fgc = (screen_byte >> 4) & 0x0f;
            let bg = screen_byte & 0x0f;
            for i in 0..8 {
                let set = row & (0x80 >> i) != 0;
                out[i] = if set { fgc } else { bg };
                fg[i] = set;
            }
        }
        VicMode::MulticolorBitmap => {
            let addr = bitmap_base
                .wrapping_add((trow as u16) * 320)
                .wrapping_add((col as u16) * 8)
                .wrapping_add(sub as u16);
            let row = inp.vic_read(addr);
            // 00→$D021, 01→screen hi nibble, 10→screen lo nibble, 11→colour RAM.
            let c01 = (screen_byte >> 4) & 0x0f;
            let c10 = screen_byte & 0x0f;
            let c11 = color & 0x0f;
            for p in 0..4 {
                let bits = (row >> (6 - p * 2)) & 0x03;
                let c = match bits {
                    0 => bg0,
                    1 => c01,
                    2 => c10,
                    _ => c11,
                };
                let is_fg = bits & 0x02 != 0;
                out[p * 2] = c;
                out[p * 2 + 1] = c;
                fg[p * 2] = is_fg;
                fg[p * 2 + 1] = is_fg;
            }
        }
        VicMode::Invalid => {}
    }
    (out, fg)
}

/// Crop the internal index buffer to the VICE PAL screenshot canvas and convert
/// to RGBA (colodore). Returns (width, height, rgba). This is exactly what
/// `renderLiteralPortRgba` produces in the TS oracle.
pub fn index_buffer_to_canvas_rgba(fb: &[u8]) -> (usize, usize, Vec<u8>) {
    let mut rgba = vec![0u8; CANVAS_W * CANVAS_H * 4];
    for cy in 0..CANVAS_H {
        let sy = cy + CANVAS_Y0;
        if sy >= FB_H {
            continue;
        }
        for cx in 0..CANVAS_W {
            let sx = cx + CANVAS_X0;
            if sx >= FB_W {
                continue;
            }
            let idx = (fb[sy * FB_W + sx] & 0x0f) as usize;
            let [r, g, b] = COLODORE[idx];
            let off = (cy * CANVAS_W + cx) * 4;
            rgba[off] = r;
            rgba[off + 1] = g;
            rgba[off + 2] = b;
            rgba[off + 3] = 0xff;
        }
    }
    (CANVAS_W, CANVAS_H, rgba)
}

/// One-shot: render a frozen machine state to the VICE PAL canvas RGBA.
pub fn render_canvas_rgba(inp: &RenderInput) -> (usize, usize, Vec<u8>) {
    let fb = render_index_buffer(inp);
    index_buffer_to_canvas_rgba(&fb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_matches_colodore() {
        assert_eq!(COLODORE[6], [0x27, 0x24, 0xc4]); // blue
        assert_eq!(COLODORE[14], [0x68, 0x64, 0xff]); // light blue
        assert_eq!(COLODORE[0], [0x00, 0x00, 0x00]);
    }

    #[test]
    fn blank_screen_is_all_border() {
        let ram = [0u8; 0x10000];
        let char_rom = [0u8; 0x1000];
        let color_ram = [0u8; 0x0400];
        let mut regs = [0u8; 0x40];
        regs[0x20] = 14; // border light blue
        // DEN=0 → whole canvas is border.
        let inp = RenderInput { regs: &regs, ram: &ram, char_rom: &char_rom, color_ram: &color_ram, bank_base: 0 };
        let (w, h, rgba) = render_canvas_rgba(&inp);
        assert_eq!((w, h), (CANVAS_W, CANVAS_H));
        let lb = COLODORE[14];
        for i in (0..rgba.len()).step_by(4) {
            assert_eq!(&rgba[i..i + 3], &lb[..]);
        }
    }

    #[test]
    fn display_window_origin_calibrated() {
        // DEN=1, border=14, bg=6, space chars ($20) with blank glyph → display
        // window is bg colour; border outside. Verify the window edges in canvas
        // coords are (32,35)..(351,234) — the measured TS golden geometry.
        let ram = [0x20u8; 0x10000];
        let char_rom = [0u8; 0x1000]; // space = all-zero rows → bg everywhere
        let color_ram = [14u8; 0x0400];
        let mut regs = [0u8; 0x40];
        regs[0x11] = 0x1b; // DEN=1, RSEL=1, YSCROLL=3
        regs[0x16] = 0xc8; // CSEL=1, XSCROLL=0
        regs[0x18] = 0x14; // screen $0400, char $1000
        regs[0x20] = 14;
        regs[0x21] = 6;
        let inp = RenderInput { regs: &regs, ram: &ram, char_rom: &char_rom, color_ram: &color_ram, bank_base: 0 };
        let fb = render_index_buffer(&inp);
        let (_w, _h, rgba) = index_buffer_to_canvas_rgba(&fb);
        let at = |x: usize, y: usize| {
            let o = (y * CANVAS_W + x) * 4;
            [rgba[o], rgba[o + 1], rgba[o + 2]]
        };
        let bg = COLODORE[6];
        let border = COLODORE[14];
        assert_eq!(at(32, 35), bg, "display top-left is bg");
        assert_eq!(at(351, 234), bg, "display bottom-right is bg");
        assert_eq!(at(31, 35), border, "just left of display is border");
        assert_eq!(at(32, 34), border, "just above display is border");
        assert_eq!(at(352, 234), border, "just right of display is border");
        assert_eq!(at(32, 235), border, "just below display is border");
    }

    #[test]
    fn standard_text_glyph_pixels_fg_bg() {
        // One char cell at grid (0,0): char code 1, colour RAM = white (1),
        // bg = blue (6). Put a known glyph row in char ROM and check the 8 pixels
        // map MSB-left, fg=colour-RAM, bg=$D021.
        let mut ram = [0x20u8; 0x10000];
        ram[0x0400] = 1; // screen[0,0] = char code 1
        let mut char_rom = [0u8; 0x1000];
        // char 1, row 0 = 0b1010_0001 → pixels: fg bg fg bg bg bg bg fg
        char_rom[1 * 8 + 0] = 0b1010_0001;
        let mut color_ram = [0u8; 0x0400];
        color_ram[0] = 1; // white foreground
        let mut regs = [0u8; 0x40];
        regs[0x11] = 0x1b; // DEN=1 RSEL=1
        regs[0x16] = 0xc8; // CSEL=1
        regs[0x18] = 0x14; // screen $0400, char $1000
        regs[0x20] = 14;
        regs[0x21] = 6; // bg blue
        let inp = RenderInput { regs: &regs, ram: &ram, char_rom: &char_rom, color_ram: &color_ram, bank_base: 0 };
        let fb = render_index_buffer(&inp);
        // display row 0 = fb line 51; col 0 starts at dbuf X 136.
        let base = 51 * FB_W + DISPLAY_X0;
        let got: Vec<u8> = (0..8).map(|i| fb[base + i]).collect();
        assert_eq!(got, vec![1, 6, 1, 6, 6, 6, 6, 1], "MSB-left fg/bg per glyph row");
    }
}

#[cfg(test)]
mod sprite_tests {
    use super::*;
    #[test]
    fn solid_sprite_paints_at_calibrated_pos() {
        let mut ram = [0x20u8; 0x10000];
        // sprite ptr at $07F8 = $0D, data $0340 solid $FF
        ram[0x07f8] = 0x0d;
        for i in 0..63 { ram[0x0340 + i] = 0xff; }
        let char_rom = [0u8; 0x1000];
        let color_ram = [14u8; 0x0400];
        let mut regs = [0u8; 0x40];
        regs[0x11] = 0x1b; regs[0x16] = 0xc8; regs[0x18] = 0x14;
        regs[0x20] = 14; regs[0x21] = 6;
        regs[0x00] = 0x60; regs[0x01] = 0x60; // X=96 Y=96
        regs[0x15] = 0x01; // enable
        regs[0x27] = 2;    // red
        let inp = RenderInput { regs: &regs, ram: &ram, char_rom: &char_rom, color_ram: &color_ram, bank_base: 0 };
        let fb = render_index_buffer(&inp);
        // expect red (2) at dbuf (line = 0x60+1 = 97, x = 0x60+112 = 208)
        let off = 97 * FB_W + 208;
        assert_eq!(fb[off], 2, "sprite top-left red");
        assert_eq!(fb[97 * FB_W + 208 + 23], 2, "sprite right edge red");
        assert_eq!(fb[(97 + 20) * FB_W + 208], 2, "sprite bottom red");
    }
}
