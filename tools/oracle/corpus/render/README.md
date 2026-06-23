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

## Sprite + graphics-mode + fine-scroll scenarios (`scene.mjs` / `scenarios.mjs` / `compare.mjs`)

Beyond the static boot screen, a scenario harness pixel-diffs the VIC in
non-default states. Each scenario programs the VIC + screen/colour/sprite RAM
**directly via the monitor `wr io` lens** (no CPU program needed): on the TS
oracle `wr` runs the banked CPU write with real I/O effects; on TRX64 the `io`
lens routes through `Machine::poke_io` to the VIC chip / colour RAM. The TS run
then elapses ≥1 frame on a **parked CPU** (`JMP self`) so its frame accumulator
re-renders the new register state without the cursor blinking; TRX64's
state-renderer reads the registers directly. Both screenshots are decoded to RGBA
and pixel-diffed.

```
node corpus/render/compare.mjs                 # run ALL scenarios
node corpus/render/compare.mjs sprite-hires …  # run named scenarios
```

Scenarios (all GREEN, 384×272):

- **sprites** — `sprite-hires`, `-stripe`, `-xexp`, `-yexp`, `-xyexp`,
  `-multicolor`, `-mc-xexp`, `-msb` (X>255), `-priority-sprite` (sprite-sprite,
  lower # wins), `-behind-fg` / `-front-fg` ($D01B sprite-to-background priority).
- **graphics modes** — `mode-multicolor-text`, `mode-standard-bitmap`,
  `mode-multicolor-bitmap`, `mode-ecm`.
- **border / fine-scroll edges** — `edge-38col` (CSEL=0, 7 L / 9 R inset),
  `edge-24row` (RSEL=0), `edge-xscroll` / `edge-xscroll-max` (XSCROLL),
  `edge-yscroll` / `edge-yscroll-down` (YSCROLL). The renderer decouples the
  border WINDOW from the CONTENT origin (`content_y0 = 48 + YSCROLL`,
  `content_x0 = 136 + XSCROLL`); in-row uncovered gaps fill background ($D021),
  the idle region above/below the 25-row band fills BLACK (verified vs TS).

## scramble-gold — the custom-loader BEHAVIORAL acid test (`scramble-gold.mjs`)

`scramble_infinity.d64` ships a custom **$DD00 (CIA2) serial loader**. The only
remaining RED on this title, the cycle-exact `scramble-load-progress` trace gate,
is a sample-boundary phase artifact (ADR-047: the rotation engine + track-1
sync-lock are bit-identical to the c64re reference). That gate never proves the
loader actually **LOADS, RUNS, and RENDERS** — only this one does.

This is the c64re team's BEHAVIORAL proof (`scripts/diff-scramble-vs-vice.mjs` +
`scripts/probe-scramble-stages.mjs` in the C64RE repo, which diff stage
screenshots vs VICE) **recycled as a TS-vs-TRX64 differential**. It drives the
SAME sequence on BOTH hermetic daemons over the WS protocol and pixel-diffs the
384×272 framebuffers at each stage:

1. `session/create` (pal) → boot ~5M cyc → `media/ingress` the scramble `.d64`
   (kind:disk) → settle 2M.
2. `session/type` `LOAD"*",8,1\r` → run 60M (custom loader takes over $DD00).
3. `session/type` `RUN\r` → settle per-stage budget → `session/screenshot`.
4. Decode both PNGs to RGBA, PIXEL-diff (same `png.mjs` infra as the render gate).

Stages (cumulative settle, mirroring the probe's `captureStage` budgets):

- **loaderbar** — 30M settle after RUN; the custom loader's raster bar. The
  headline result: GREEN here ⇒ the $DD00 loader renders pixel-exact on TRX64.
- **credits** — +150M; deeper into the loader / credits screen.
- **post-space** — `session/type " "` then +60M; first frame after leaving credits.

```
cd tools/oracle
node corpus/render/scramble-gold.mjs              # all stages
node corpus/render/scramble-gold.mjs loaderbar    # one stage (runs the prefix to reach it)
node corpus/render/scramble-gold.mjs loaderbar --dump   # on RED, write both PNGs + a diff mask
```

**GREEN** = stage pixel-identical TS-vs-TRX64 → the custom loader runs correctly;
the cycle-exact `scramble-load-progress` stands as a documented sample-boundary
known-RED, not a functional gap. **RED** = the real behavioral loader bug — the
runner reports the first divergent stage, the divergent bounding box, a
per-region histogram (BORDER / display top/middle/bottom third), and with
`--dump` writes `scramble-gold-out/scramble-<stage>-{ts-golden,trx64}.png` plus a
`-diffmask-384x272.rgba` (raw RGBA, differing pixels white) for visual triage.

> Env overrides: `SCRAMBLE_D64`, `C64RE_ROOT`, `TRX64_DAEMON_BIN`.

### Result of the first run (2026-06-23) — RED, root-caused to the POST-RUN custom loader

The `loaderbar` stage is **RED** (57124 / 104448 px differ). The two dumped
framebuffers tell the story precisely:

- **TS golden @ 30M post-RUN** — the full title screen: the multicolor-bitmap
  "SCRAMBLE INFINITY" artwork, "Graphics 12", "v1.2", "Ready Joy 2".
- **TRX64 @ 30M post-RUN** — the loader bar: "ENTERING SCRAMBLE SYSTEM" + an
  **empty** progress bar on a grey screen. A sweep (`scramble-gold-probe.mjs`)
  shows the TRX64 frame is **byte-identical from 30M to 120M post-RUN** — the
  machine is FROZEN on the loader bar; the bar never fills.

This is **NOT a renderer bug** (every render gate is GREEN, and the loader-bar
frame itself renders pixel-clean) and **NOT a first-file load failure**: the
`scramble-load-progress` $AE/$AF probe shows the **first file loads correctly on
TRX64**, tracking the golden to within a handful of bytes (the known cycle-exact
phase lead):

| checkpoint | TS golden $AE/$AF | TRX64 $AE/$AF |
|---|---|---|
| end5 | $097F | $098C |
| end6 | $0B1F | $0B1F |
| end7 | $0CB3 | $0CBC |
| end8 | $0E41 | $0E49 |

The first file (loaded via the **KERNAL** serial routines) completes. The
**title artwork is loaded AFTER `RUN` by the game's own custom $DD00 bit-bang
loader** — and THAT is where TRX64 wedges. The custom loader paints its bar then
makes no further progress, so the title never appears. This is the same
underlying $DD00/drive timing the cycle-exact `scramble-load-progress` flags: the
KERNAL tolerates the sub-byte phase skew, the tighter custom loop does not.

**Fix target** (drive/IEC, NOT renderer): the post-RUN custom $DD00 loader's
serial handshake/timing — localise the first divergent drive/IEC event with the
trace tooling (`scramble-load-progress` + a drive-cpu trace) right after `RUN`,
where TRX64's bar stops advancing. The screenshot harness has done its job: it
proved the loader RUNS + RENDERS up to the bar, isolated the stall to the
post-RUN custom-loader stage (not boot, not first-file load, not the renderer),
and points the fix at the drive serial timing.

### Diagnostic companion — `scramble-gold-probe.mjs`

Not a gate: drives ONE daemon through boot→load→RUN and screenshots at a sweep of
post-RUN cycle checkpoints, to locate the loader stage each machine is in at a
given cycle (used to prove the TRX64 freeze above).

```
node corpus/render/scramble-gold-probe.mjs trx64 30 45 60 90 120
node corpus/render/scramble-gold-probe.mjs ts 8 16 24
```
