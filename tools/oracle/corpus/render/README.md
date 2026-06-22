# Render gate — VIC pixel parity (`tools/oracle/corpus/render/`)

The VIC pixel-render gate validates that TRX64 produces a displayed framebuffer
**pixel-identical** to the TS oracle (the VICE x64sc literal-port renderer).

## Why a separate runner (not the protocol oracle)

The protocol oracle (`src/oracle.ts`) diffs WS response **values**. A screenshot
response is a base64 PNG `dataUrl`, and **PNG zlib compression differs between the
Rust `png` crate and Node's encoder** — a raw dataUrl byte-diff would spuriously
RED even on identical images. So this gate decodes both PNGs to raw RGBA and
compares **PIXELS**, never container bytes.

## Files

- `png.mjs` — dependency-free PNG decoder (`decodePng`) + RGBA pixel comparator
  (`diffRgba`). Uses only Node's built-in `zlib` for the IDAT inflate.
- `capture.mjs` — spawns a hermetic daemon (TS for golden, TRX64 for compare),
  boots to the BASIC-ready screen, calls `session/screenshot`, decodes, diffs.
- `boot-basic-ready.golden.png` — the TS oracle golden (384×272).
- `boot-basic-ready.render.json` — scenario descriptor.

## Pixel-compare method

1. Boot both daemons `3_000_000` cycles (RAM test ends ~2.4M; KERNAL prints the
   banner + `READY.` and drops to the keyboard wait `$E5CF` by ~2.5M — 3.0M sits
   on the steady deterministic screen). Same fixed cycle ⇒ same cursor phase.
2. `session/screenshot` → `{ dataUrl, width, height }`; the PNG is the VICE PAL
   canvas (`dbuf[104..488] × line[16..288]` = **384×272**).
3. Decode both PNGs to RGBA, compare every pixel. GREEN iff identical.

## Geometry (calibrated against the golden)

- Internal draw buffer: **520 × 312** colour indices (= VICE `VICII_DRAW_BUFFER_SIZE`).
- Display window origin in the draw buffer: **dbuf X 136, line 51** (`DISPLAY_X0`,
  `DISPLAY_Y0` in `crates/trx64-core/src/render.rs`).
- Screenshot crop → canvas **384×272**; display window lands at canvas **(32, 35)**,
  320×200.
- Palette: **colodore** (VICE `colodore.vpl`) — the single runtime palette, copied
  verbatim from the oracle's `palettes.ts`.

## Run

```
cd tools/oracle
node corpus/render/capture.mjs golden     # (re)record the TS golden PNG
node corpus/render/capture.mjs compare     # GREEN/RED pixel parity vs golden
```
