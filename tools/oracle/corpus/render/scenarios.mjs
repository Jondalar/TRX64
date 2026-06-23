// Render-gate scenario registry — VIC pixel-parity beyond the BASIC-ready boot.
//
// Each scenario is a named `{ setup: string[] }` list of monitor commands that
// program the VIC + screen/colour/sprite RAM on the deterministic BASIC-ready
// machine (border 14, bg 6, screen $0400, VIC bank 0). We use the `wr io` lens so
// $D0xx writes hit the VIC chip and $D800 writes hit colour RAM — on BOTH the TS
// oracle daemon (whose `wr` runs the banked CPU write with real I/O effects) and
// the TRX64 daemon (whose `wr io` routes to the chip via Machine::poke_io). No
// CPU program runs; the post-setup frame is a pure function of the registers +
// memory, so it is fully deterministic. The gate pixel-diffs the two screenshots.

// ── command emitters ──────────────────────────────────────────────────────────
const h = (n) => (n & 0xffff).toString(16);
const h2 = (n) => (n & 0xff).toString(16).padStart(2, "0");

// `wr io <addr> <byte..>` — write bytes through the I/O lens.
function wr(addr, ...bytes) {
  return `wr io ${h(addr)} ${bytes.map(h2).join(" ")}`;
}
// Fill `n` bytes at `addr` with a repeating `pattern` (array of bytes), emitted
// as a single wr (the monitor writes exactly the listed bytes).
function fill(addr, pattern, n) {
  const out = [];
  for (let i = 0; i < n; i++) out.push(pattern[i % pattern.length]);
  return wr(addr, ...out);
}

// Program one sprite. opts: { n, x, y, color, ptr, data:byte[63], xexp, yexp,
//   mc, pri, mc0, mc1, msb }.  Returns an array of commands (data + ptr + regs).
// Register writes for shared regs ($D010/$D015/$D017/$D01B/$D01C/$D01D) are the
// CALLER's responsibility via spriteRegs() so multiple sprites compose cleanly.
function spriteData(opts) {
  const cmds = [];
  const dataAddr = opts.ptr * 64;
  cmds.push(wr(dataAddr, ...opts.data));
  // pointer lives at screen_base + $3F8 + n = $07F8 + n.
  cmds.push(wr(0x07f8 + opts.n, opts.ptr));
  // per-sprite X/Y/color
  cmds.push(wr(0xd000 + opts.n * 2, opts.x));
  cmds.push(wr(0xd001 + opts.n * 2, opts.y));
  cmds.push(wr(0xd027 + opts.n, opts.color));
  return cmds;
}

// Build the shared per-bit sprite-control registers from a list of sprite opts.
function spriteRegs(sprites, { mc0 = 0x0a, mc1 = 0x0d } = {}) {
  let enable = 0, msb = 0, xexp = 0, yexp = 0, mc = 0, pri = 0;
  for (const s of sprites) {
    const b = 1 << s.n;
    enable |= b;
    if (s.msb) msb |= b;
    if (s.xexp) xexp |= b;
    if (s.yexp) yexp |= b;
    if (s.mc) mc |= b;
    if (s.pri) pri |= b;
  }
  return [
    wr(0xd010, msb),
    wr(0xd017, yexp),
    wr(0xd01b, pri),
    wr(0xd01c, mc),
    wr(0xd01d, xexp),
    wr(0xd025, mc0),
    wr(0xd026, mc1),
    wr(0xd015, enable), // enable last
  ];
}

// 63-byte sprite images.
const SOLID = Array(63).fill(0xff);
// vertical stripes: every row = $AA,$55,$CC → distinct bit pairs across the 24 px.
const STRIPE = (() => { const a = []; for (let r = 0; r < 21; r++) a.push(0xaa, 0x55, 0xcc); return a; })();
// a diagonal/asymmetric image so X- and Y-doubling bugs surface.
const DIAG = (() => {
  const a = [];
  for (let r = 0; r < 21; r++) {
    const bit = 1 << (r % 8);
    a.push(bit, (bit << 1) & 0xff | (bit >> 7), 0x80 >> (r % 8));
  }
  return a;
})();

function sceneSprites(sprites, shared) {
  const setup = [];
  for (const s of sprites) setup.push(...spriteData(s));
  setup.push(...spriteRegs(sprites, shared));
  return { setup };
}

// ── graphics-mode scene helpers ───────────────────────────────────────────────
// Multicolor TEXT: set $D016 MCM=1, fill a patch of screen with chars whose colour
// RAM has bit3 set (→ multicolor) and set $D022/$D023 background colours.
function sceneMulticolorText() {
  const setup = [];
  setup.push(wr(0xd016, 0xd8));            // CSEL=1 XSCROLL=0 MCM=1
  setup.push(wr(0xd022, 0x02), wr(0xd023, 0x07)); // bg1 red, bg2 yellow
  // Top-left 8×4 cells: char codes 0..N, colour RAM bit3 set (multicolor) + a hue.
  for (let row = 0; row < 4; row++) {
    const chars = [];
    const cols = [];
    for (let c = 0; c < 8; c++) { chars.push((row * 8 + c) & 0x7f); cols.push(0x08 | ((row + c) & 0x07)); }
    setup.push(wr(0x0400 + row * 40, ...chars));
    setup.push(wr(0xd800 + row * 40, ...cols));
  }
  return { setup };
}

// Standard (hi-res) BITMAP: $D011 BMM=1, bitmap at $2000, screen $0400 = colours.
// We draw a simple wedge so fg/bg nibbles both show.
function sceneStandardBitmap() {
  const setup = [];
  // $D018 = screen $0400 ($10) | bitmap $2000 (bit3) = $18.
  setup.push(wr(0xd018, 0x18));
  setup.push(wr(0xd011, 0x3b)); // DEN=1 RSEL=1 BMM=1 YSCROLL=3
  // Bitmap bytes for the first two cell-rows: a diagonal per cell row.
  for (let cell = 0; cell < 16; cell++) {
    const base = 0x2000 + cell * 8;
    const bytes = [];
    for (let r = 0; r < 8; r++) bytes.push(0x80 >> ((cell + r) & 7));
    setup.push(wr(base, ...bytes));
  }
  // Screen RAM = colour pairs: hi nibble fg, lo nibble bg.
  const colors = [];
  for (let c = 0; c < 16; c++) colors.push(((c & 0x0f) << 4) | ((c + 6) & 0x0f));
  setup.push(wr(0x0400, ...colors));
  return { setup };
}

// Multicolor BITMAP: $D011 BMM=1 + $D016 MCM=1.
function sceneMulticolorBitmap() {
  const setup = [];
  setup.push(wr(0xd018, 0x18));
  setup.push(wr(0xd011, 0x3b)); // BMM=1
  setup.push(wr(0xd016, 0xd8)); // MCM=1
  for (let cell = 0; cell < 16; cell++) {
    const base = 0x2000 + cell * 8;
    const bytes = [];
    for (let r = 0; r < 8; r++) bytes.push(0x1b); // pairs 00 01 10 11
    setup.push(wr(base, ...bytes));
  }
  // screen RAM hi/lo nibble + colour RAM provide 01/10/11 colours.
  const screen = [], cram = [];
  for (let c = 0; c < 16; c++) { screen.push(((c & 0x0f) << 4) | ((c + 3) & 0x0f)); cram.push((c + 9) & 0x0f); }
  setup.push(wr(0x0400, ...screen));
  setup.push(wr(0xd800, ...cram));
  return { setup };
}

// ECM: $D011 ECM=1. char code bits6-7 select bg0..bg3.
function sceneEcm() {
  const setup = [];
  setup.push(wr(0xd011, 0x5b)); // DEN=1 RSEL=1 ECM=1 YSCROLL=3
  setup.push(wr(0xd022, 0x02), wr(0xd023, 0x05), wr(0xd024, 0x07)); // bg1/2/3
  // Top row: codes 0x01,0x41,0x81,0xC1 → glyph 1 on bg0/1/2/3.
  setup.push(wr(0x0400, 0x01, 0x41, 0x81, 0xc1, 0x01, 0x41, 0x81, 0xc1));
  setup.push(wr(0xd800, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01)); // white fg
  return { setup };
}

// ── border / fine-scroll edges ───────────────────────────────────────────────
// 38-column mode (CSEL=0): $D016 bit3=0. The main border eats 8 px each side.
function scene38Col() { return { setup: [wr(0xd016, 0xc0)] }; } // CSEL=0 XSCROLL=0
// 24-row mode (RSEL=0): $D011 bit3=0.
function scene24Row() { return { setup: [wr(0xd011, 0x13)] }; } // DEN=1 RSEL=0 YSCROLL=3
// Fine X scroll = 5.
function sceneXScroll() { return { setup: [wr(0xd016, 0xcd)] }; } // CSEL=1 XSCROLL=5
// Fine Y scroll = 1.
function sceneYScroll() { return { setup: [wr(0xd011, 0x19)] }; } // DEN=1 RSEL=1 YSCROLL=1

// ── scenario registry ─────────────────────────────────────────────────────────
export const SCENARIOS = {
  // Diagnostic: no register change — TS runs 2 frames (cursor may blink), TRX64
  // screenshots the frozen boot frame. Used to confirm the run is cursor-neutral.
  "identity": () => ({ setup: [wr(0xd020, 0x0e)] }), // re-write the border (no-op value)

  "sprite-hires": () =>
    sceneSprites([{ n: 0, x: 0x60, y: 0x60, color: 0x02, ptr: 0x0d, data: SOLID }]),

  "sprite-stripe": () =>
    sceneSprites([{ n: 0, x: 0x50, y: 0x50, color: 0x07, ptr: 0x0d, data: STRIPE }]),

  "sprite-xexp": () =>
    sceneSprites([{ n: 0, x: 0x40, y: 0x50, color: 0x07, ptr: 0x0d, data: DIAG, xexp: 1 }]),

  "sprite-yexp": () =>
    sceneSprites([{ n: 0, x: 0x50, y: 0x40, color: 0x03, ptr: 0x0d, data: DIAG, yexp: 1 }]),

  "sprite-xyexp": () =>
    sceneSprites([{ n: 0, x: 0x40, y: 0x40, color: 0x05, ptr: 0x0d, data: DIAG, xexp: 1, yexp: 1 }]),

  "sprite-multicolor": () =>
    sceneSprites([{ n: 0, x: 0x50, y: 0x50, color: 0x07, ptr: 0x0d, data: STRIPE, mc: 1 }],
      { mc0: 0x0a, mc1: 0x01 }),

  "sprite-mc-xexp": () =>
    sceneSprites([{ n: 0, x: 0x48, y: 0x50, color: 0x02, ptr: 0x0d, data: STRIPE, mc: 1, xexp: 1 }],
      { mc0: 0x05, mc1: 0x01 }),

  "sprite-msb": () =>
    sceneSprites([{ n: 0, x: 0x20, y: 0x60, color: 0x02, ptr: 0x0d, data: SOLID, msb: 1 }]),

  "sprite-priority-sprite": () =>
    sceneSprites([
      { n: 0, x: 0x50, y: 0x50, color: 0x02, ptr: 0x0d, data: SOLID },
      { n: 1, x: 0x60, y: 0x58, color: 0x07, ptr: 0x0e, data: SOLID },
    ]),

  "sprite-behind-fg": () =>
    sceneSprites([{ n: 0, x: 0x30, y: 0x36, color: 0x02, ptr: 0x0d, data: SOLID, pri: 1 }]),

  "sprite-front-fg": () =>
    sceneSprites([{ n: 0, x: 0x30, y: 0x36, color: 0x02, ptr: 0x0d, data: SOLID, pri: 0 }]),

  "mode-multicolor-text": sceneMulticolorText,
  "mode-standard-bitmap": sceneStandardBitmap,
  "mode-multicolor-bitmap": sceneMulticolorBitmap,
  "mode-ecm": sceneEcm,

  "edge-38col": scene38Col,
  "edge-24row": scene24Row,
  "edge-xscroll": sceneXScroll,
  "edge-yscroll": sceneYScroll,
};
