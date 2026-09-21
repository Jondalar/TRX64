# Spec 868 — A colour per pixel: what a 64 MHz CPU can do to `$D020`

**Status:** BUILT (2026-09-21) — §8.3 confirmed on real firmware by the UE2 session
(run lengths of one colour collapse from 13-14 px to 1-2 px; colour changes per drawn line
median 13 → 43), pending merge.
**Repo:** TRX64 only. C64RE: no change — it renders the frame TRX64 hands it.
**Number:** 868 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** Spec 851 (the U64 machine profile and the faster CPU), 856 (turbo pays
per PHI2 cycle), and the VIC's cycle-exact draw path as ported from VICE.
**Origin:** the UE2 emulator, 2026-09-21, running Xander Mol's *Mandelbrot Upic*
(`github.com/xahmol/mandelbrot-upic`) on the U64 firmware over trx64-core. The fractal
appears, its vertical detail is right, and its horizontal detail is one eighth of what
real hardware shows. The technique is Aleksi Eeben's **UPic** (csdb.dk/release/?id=263980).

---

## §1 What the program does, and why our VIC cannot see it

UPic turns the border into a 384×256 16-colour picture. `DEN=0` ($D011 bit 4) blanks the
display so the whole visible area is border, and then a fully unrolled, `SEI`-protected
sequence writes **384 values to `$D020` per raster line** — one per VIC pixel. Per packed
byte, two pixels:

```asm
    ldx $e000,y     ; 4
    stx $d020       ; 4
    lda nybbles,x   ; 4
    sta $d020       ; 4
```

Eight CPU cycles per pixel. 192 bytes = 3072 CPU cycles per line, and at 64 MHz a 63-cycle
PAL line affords 4032 — so it fits, with eight stores landing inside every PHI2 cycle,
exactly one per pixel that cycle draws. On the U64 the FPGA VIC samples the border colour
at the **pixel clock**, so each of those stores is a pixel.

Our VIC samples it once per PHI2 cycle, because that is what a 1 MHz 6510 can produce.
Three places say so, and each is correct for the machine it was written for:

- `c64_6510core.rs:826` `clk_inc` — below the turbo divider the cycle is not a PHI2 cycle:
  no alarms, no clk, **no VIC tick**. The VIC never runs between the turbo CPU's stores.
- `vic.rs:2753` `color_reg_store` — one pending write per register
  (`last_color_reg`/`last_color_value`, then `cregs[reg]`). VICE's model, and right for a
  CPU that cannot write twice in a cycle.
- `vic_draw.rs:445` `draw_colors_6569` — resolves each pixel as `cregs[token]`, one
  register file for all eight pixels of the cycle.

Net: 48 border values per visible line instead of 384. The shape survives; seven eighths
of the horizontal detail does not.

**This is the first thing TRX64 cannot render that real hardware does.** Not a divergence
from VICE — VICE cannot render it either, and for the same reason. It is a machine we now
emulate (the U64 at 64 MHz) doing something no C64 can do.

## §2 The one fact that makes it cheap

`turbo_phase` (`c64_6510core.rs:657`) already IS the sub-PHI2 position. With
`turbo_div = 64` at 64 MHz, a store at phase `p` belongs to pixel

```
pixel = p * 8 / turbo_div        // 64 CPU cycles per PHI2, 8 pixels → 8 cycles per pixel
```

with no rounding slack: the program's eight-cycle-per-pixel loop lands one store in each
of the eight. Nothing has to be measured or inferred; the number is already in the core
and is simply never passed on.

## §3 D1 — The phase reaches the VIC as a mirror, not as an argument

The CPU core writes its current `turbo_phase` and `turbo_div` into a field the VIC's store
path reads. **Not** an extra argument threaded through every write path: that would touch
code which has nothing to do with turbo, and a parameter nobody else uses is a parameter
that rots. Outside turbo the mirror is constant (`div = 1`, `phase = 0`), which is exactly
the condition §5 gates on, so every non-turbo machine takes the same path it takes today.

## §4 D2 — Eight slots per cycle, resolved where the colour is resolved

`color_reg_store` today latches one value per register. Under the §5 gate it also fills a
per-cycle array:

```rust
struct SubCycleColour {
    reg: u8,          // 0x20 to begin with; §7 says why not more
    slots: [u8; 8],   // the value in force at each pixel of this PHI2 cycle
    armed: bool,
}
```

A store at pixel `k` writes `slots[k..8]` — the value stays in force until another store
replaces it, which is what the register does. The array is cleared at the cycle boundary,
and `cregs[reg]` keeps taking the **last** value, so everything that reads the register
outside the draw path (a monitor `io`, a snapshot, the next cycle) sees what the CPU last
wrote.

**The resolve is the place to apply it, not the token.** `draw_border8` fills the render
buffer with the token `COL_D020` (`vic_draw.rs:406`) — "this pixel is the border colour" —
and `draw_colors_6569` turns a token into a colour with `cregs[token]`
(`vic_draw.rs:447`). So the override belongs at that lookup: when the array is armed and
the token names the overridden register, the pixel takes `slots[i]` instead of
`cregs[token]`. Two consequences worth stating, because both are easy to get wrong:

- **The 6569's one-pixel colour latency still applies.** `draw_colors_6569` resolves
  through `pixel_buffer[(i + 1) & 7]`, one pixel behind the token it stored. The override
  must ride that same pipeline — it replaces the value the lookup yields, not the moment
  at which the lookup happens. Bypassing the pipeline would make the turbo path a pixel
  sharper than the machine is.
- **`draw_border8` does not change at all.** It says *which register* a pixel comes from;
  it never said *which value*. Keeping the change on the resolve side means the same
  mechanism covers every path that resolves a colour token.

## §5 D3 — The gate, and why it is not negotiable

The array is filled only when `speed_profile == U64 && turbo_div > 1`. Everywhere else
`color_reg_store` does exactly what it does today.

Every C64 and C128 therefore stays **bit-identical to VICE**, and that is not an aspiration
but a thing the gate already proves: the 7-game screenshot gate compares rendered frames
byte for byte, and it runs on every push. A change to the VIC's colour resolve that did not
carry this gate would be a change to every game we have.

At 48 MHz (the U64 mk1 speed table's top index) a line's 3072 CPU cycles do not fit in 63
PHI2 cycles at all, so the technique is inherently an Elite II / C64 Ultimate one. That is
a consequence of arithmetic, not a decision, and it needs no new machine type: the
`U64SpeedTable::U64II` default already expresses it.

## §5a D3a — A speed change restarts the phase

Found by the second host reading UPic's `render_frame()` after §8.3 had already passed,
and it belongs to this spec rather than to 851: the phase was an invisible counter until
the slots made it decide which pixel a store paints.

Every picture row begins with

```asm
    lda #$80        ; index 0 — 1 MHz
    ldx #$8f        ; max again
    sta $d031
    stx $d031
```

and Aleksi's own comment calls it a **resync**. It has to be: a technique built on one
store per pixel needs the sub-cycle counter to start a row at a known place, or the row's
384 stores walk relative to the pixel clock and the picture shears.

At 1 MHz this is not a modelling choice. A CPU cycle **is** a PHI2 cycle there, so a cycle
can only end on a boundary and the phase is zero by definition; re-engaging the divider
therefore starts from zero. So `sync_turbo_from_vic` — the instruction-boundary read that
851 already did — resets `turbo_phase` **when the divider is 1**, which is the whole of
UPic's resync because it goes through index 0.

**And no further, on purpose.** The first version of this also reset on any divider
change, including one turbo speed replacing another without passing 1. Whether the
hardware's divider restarts there is undocumented and unmeasured, and the host that
reported the resync could not separate the two from the picture either — its robust
metric put both inside noise. So the unknown case leaves the phase alone. Guessing would
have cost nothing visible and been a lie in the tree, which is the expensive kind.

Before this spec nothing could observe the difference, because the whole cycle took one
colour whatever the phase said. That is the shape of the bug this feature exposes rather
than causes.

## §6 D4 — `$D021` comes second, and on purpose

The same trick works on the background colour inside `DEN=1`, and UPic's own package uses
it. The array carries `reg` rather than assuming `0x20` so that adding `$D021` is a
second entry rather than a second mechanism — but this spec ships `$D020` alone, because
that is what the corpus program needs and because one register is enough to prove the
resolve is in the right place.

## §7 Scope — where the idea stops

**Colour registers only.** `$D020` and `$D021` are values the VIC *samples*; a sub-cycle
write to one changes a colour and nothing else. `$D011`, `$D016`, `$D018` and the sprite
registers are not that: they change what the display logic *does*, and a sub-PHI2 write to
them would need the whole draw sequence to run at sub-cycle granularity. That is a
different and much larger machine, and this spec does not open the door to it. The
distinction goes in the code as a comment, not only here — the next reader will otherwise
generalise it.

**Not in scope:** the 8565's grey-dot path (`draw_colors_8565`) beyond making sure it is
unaffected while unarmed; sprite colours; any change to how turbo cycles are counted.

## §8 Acceptance

Deterministic, and testable without hardware — the picture is a function of the bytes.

1. **Phase to pixel.** A unit test over `turbo_div` ∈ {1, 2, 8, 64} and every phase: the
   mapping is `p * 8 / div`, a store at the last phase of a cycle lands in pixel 7, and
   `div = 1` never arms the array.
2. **Carry-forward.** A store at pixel 3 and another at pixel 6 produce
   `[a,a,a,a,b,b,b,b]` from the prior value `a` — the register holds its value between
   writes, and unwritten slots are not "no colour".
3. **The picture.** Xander's `tools/upic_convert.py` output and his synthetic
   `src/upic_test.c` pattern give a known 384×256 nibble grid. A frame rendered from a run
   of the viewer at `c64-pal` + U64 profile + 64 MHz matches it pixel for pixel, and the
   same run on the pre-868 build matches it at one pixel in eight — the second half is
   what proves the test is measuring the right thing.
4. **`cregs` still holds the last write.** After a cycle with eight stores, a monitor `io`
   read and a snapshot both report the last value, not the first and not slot 0.
5. **The 6569 latency is intact.** A single store mid-cycle under turbo lands one pixel
   later than the naive reading suggests, matching the unarmed path's behaviour for the
   same store at PHI2 granularity.
6. **Nothing else moved.** The 7-game screenshots byte-identical, the VIC gates
   (`iso_vic_gate`, `vic_collision_gate`, `vic_line_trace_gate`) green, and the ntsc gate
   green — the whole point of §5.
7. **Turbo off, path off.** With `turbo_div == 1` the array is never armed, proven by a
   counter in a debug build rather than by inference.

## §9 Open

- **When does a `$D031` write take effect — next instruction, or next cycle?** Measured on
  our side (`u64_turbo_gate.rs`): the resync pair costs **4 PHI2 cycles**, because 851
  applies a speed write from the next INSTRUCTION, so the `stx` runs entirely at divider
  1. That is ~256 turbo-cycle-equivalents for two stores.

  **It was briefly believed to cost UPic every second picture row; it does not.** With the
  access watch armed through the real API, that row loop measures one picture row per
  raster line, 63 PHI2 cycles apart, for all 256 rows — the row fits. The earlier reading
  rested on a hand count and an instrument that was never wired, and the host that
  produced both withdrew them. What is left is a modelling question standing on its own.

  The evidence that the model is wrong is not a datasheet: Aleksi built the pair against a
  real machine and the technique works there, so a turbo CPU can pass through index 0
  without paying a PHI2 cycle per instruction. 851 recorded "the divider refreshed at each
  instruction boundary" as a build decision with no source — a convenience, not a
  measurement. Changing it is a timing-model change that touches every turbo program, so
  it wants the owner's U64 first: run the pair in a loop and count instructions per frame
  against a build without it.
- **Does a change between two turbo speeds restart the divider?** Not documented, not
  measured, and currently modelled as "no" (§5a). A `DEN=1` program that switches between
  two turbo speeds mid-line and paints would answer it; so would the owner's U64.
- Whether the array should live on `VicII` or beside the CPU mirror. On the VIC is the
  obvious answer for the draw path; the store path is the CPU's side of the bus, so a
  measurement may say otherwise.
- The snapshot question: a `.c64re` taken mid-cycle under turbo. The slots are per-cycle
  scratch and a snapshot is taken at a cycle boundary, so they should never need to be in
  it — worth proving rather than assuming, because "should never" is where snapshot bugs
  come from.
- Whether `vic/line_trace` (859) should show the per-pixel values when armed. It reports
  what the VIC did per cycle, and under turbo that is now eight things rather than one.
