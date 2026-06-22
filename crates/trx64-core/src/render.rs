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
pub const DISPLAY_W: usize = 320;
pub const DISPLAY_H: usize = 200;

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
    let mut fb = vec![0u8; FB_W * FB_H];
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
        return fb;
    }

    // Vertical display window (RSEL): 25 rows from line 51, or 24 rows from 55.
    let disp_y0 = if rsel { DISPLAY_Y0 } else { DISPLAY_Y0_24 };
    let disp_rows = if rsel { 25 } else { 24 };
    // Horizontal display window (CSEL): 40 cols from dbuf X 136, or 38 cols
    // inset by 8 px (one cycle) on each side. The graphics still render full
    // 40 cols; the main-border flip-flop simply overpaints the 8-px margins.
    let disp_x0 = DISPLAY_X0;

    let bg0 = inp.reg(0x21) & 0x0f;
    let bg1 = inp.reg(0x22) & 0x0f;
    let bg2 = inp.reg(0x23) & 0x0f;
    let bg3 = inp.reg(0x24) & 0x0f;

    let screen_base = inp.screen_base();
    let char_base = inp.char_base();
    let bitmap_base = inp.bitmap_base();

    // 25 text rows × 8 raster lines. Always iterate 40 cols; border overlay handles CSEL.
    for trow in 0..disp_rows {
        for sub in 0..8usize {
            let line = disp_y0 + trow * 8 + sub;
            if line >= FB_H {
                continue;
            }
            let row_off = line * FB_W + disp_x0;
            for col in 0..40usize {
                let vm_index = trow * 40 + col;
                let screen_byte = inp.ram[screen_base.wrapping_add(vm_index as u16) as usize];
                let color = inp.color_ram[vm_index & 0x3ff] & 0x0f;
                let px = pixels_for_cell(
                    inp, mode, screen_byte, color, sub, col, trow, char_base, bitmap_base,
                    bg0, bg1, bg2, bg3,
                );
                let base = row_off + col * 8;
                for (i, &c) in px.iter().enumerate() {
                    fb[base + i] = c;
                }
            }
        }
    }

    // Main-border overlay for the 38-col / 24-row sub-windows (CSEL=0 / RSEL=0).
    // VICE flips the main border in at the narrower comparison; the simplest exact
    // reproduction is to overpaint the trimmed margins with border colour.
    if !csel {
        // 38-col: trim 8 px (7+1 per VICE draw_border8 csel=0 path → net one cycle)
        // each side of the 320-px window.
        for line in disp_y0..(disp_y0 + disp_rows * 8).min(FB_H) {
            let lo = line * FB_W + disp_x0;
            for i in 0..8 {
                fb[lo + i] = border;
                fb[lo + DISPLAY_W - 1 - i] = border;
            }
        }
    }

    fb
}

/// Render the 8 horizontal pixels of one character/bitmap cell row.
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
) -> [u8; 8] {
    match mode {
        VicMode::StandardText => {
            // Char ROM row: (char<<3 | sub). FG = colour RAM, BG = $D021. MSB left.
            let row = inp.vic_read(char_base.wrapping_add(((screen_byte as u16) << 3) + sub as u16));
            let mut out = [0u8; 8];
            for i in 0..8 {
                out[i] = if row & (0x80 >> i) != 0 { color } else { bg0 };
            }
            out
        }
        VicMode::MulticolorText => {
            let row = inp.vic_read(char_base.wrapping_add(((screen_byte as u16) << 3) + sub as u16));
            let mut out = [0u8; 8];
            if color & 0x08 == 0 {
                // Colour RAM bit3 = 0 → this cell is hi-res (standard text) with
                // the low 3 bits as foreground.
                let fg = color & 0x07;
                for i in 0..8 {
                    out[i] = if row & (0x80 >> i) != 0 { fg } else { bg0 };
                }
            } else {
                // Multicolor: 4 double-wide pixels from bit pairs.
                let fg = color & 0x07;
                for p in 0..4 {
                    let bits = (row >> (6 - p * 2)) & 0x03;
                    let c = match bits {
                        0 => bg0,
                        1 => bg1,
                        2 => bg2,
                        _ => fg,
                    };
                    out[p * 2] = c;
                    out[p * 2 + 1] = c;
                }
            }
            out
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
            let mut out = [0u8; 8];
            for i in 0..8 {
                out[i] = if row & (0x80 >> i) != 0 { color } else { bg };
            }
            out
        }
        VicMode::StandardBitmap => {
            // Bitmap byte: base + trow*320 + col*8 + sub. Bit=1 → upper nibble of
            // screen byte (fg), bit=0 → lower nibble (bg).
            let addr = bitmap_base
                .wrapping_add((trow as u16) * 320)
                .wrapping_add((col as u16) * 8)
                .wrapping_add(sub as u16);
            let row = inp.vic_read(addr);
            let fg = (screen_byte >> 4) & 0x0f;
            let bg = screen_byte & 0x0f;
            let mut out = [0u8; 8];
            for i in 0..8 {
                out[i] = if row & (0x80 >> i) != 0 { fg } else { bg };
            }
            out
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
            let mut out = [0u8; 8];
            for p in 0..4 {
                let bits = (row >> (6 - p * 2)) & 0x03;
                let c = match bits {
                    0 => bg0,
                    1 => c01,
                    2 => c10,
                    _ => c11,
                };
                out[p * 2] = c;
                out[p * 2 + 1] = c;
            }
            out
        }
        VicMode::Invalid => [0u8; 8],
    }
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
