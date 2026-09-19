# Spec 863 — NTSC: a second video standard, chosen before power-on

**Status:** BUILT (TRX64 half, 2026-09-19, branch `spec-863-ntsc`) — `models.toml` with VICE's seven rows (`c64-pal`, `c64-ntsc`, `c64-paln` run), the three cycle tables ported 1:1, `Machine::timing()` as the one source, NTSC's wrapped window, the frame-boundary switch, the model in every snapshot; every §6 item has a test (§10). Gate GREEN: 16 core suites / 519 tests, daemon suite 422 tests, 7-game 7/7 with the screenshots byte-identical to the pre-863 build, clippy at the 404-line backlog (nothing on a line this spec's follow-ups touched). Follow-ups closed the same day (§10): the reverse-debug ring holds its seconds on every model, the scenario list names the recorded model, a switch while recording is journaled and replayed at its cycle. **C64RE half BUILT** (C64RE branch `spec-863-ntsc`): a Live-tab model selector from `session/models` (unrunnable rows disabled with the missing block; it asks, then `session/model`; cancel sends nothing); `model` on `runtime_session_start` (replaces `pal`), `runtime_sandbox_run`, `runtime_scene_reel`; `knowledge/project.json → machine.model` (`project_init` default `c64-pal`) passed as `--model` by the one spawn resolver; every frame count reads the machine's timing (`PAL_CYCLES_PER_FRAME` removed); recorded scenarios carry their model and are refused on another one naming both; the VIC view and line strip take their geometry from the recorder header. §7 1–4 tested in `smoke:863` (50/50) and `e2e:863-model` (61/61, CI). Both branches merge together — the C64RE side refuses to guess PAL against a pre-863 runtime.
**Repos:** TRX64 (the machine) + C64RE (the switch, and every place that counts in frames).
**Number:** 863 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** nothing structural. Follows the path Spec 851 laid for the machine profile.
**Origin:** the owner, 2026-09-19: "NTSC Mode support in TRX64 und ein Switch im C64RE
dafür." Until now PAL was the only machine by decision ("PAL/6569 first, NTSC deferred").
**References:** VICE 3.10 `src/viciisc/vicii-chip-model.c` (the cycle tables), `vicii-timing.h`
(the display window), `c64/c64scmodel.c` (the models), `c64/c64.c:1305-1368`
(`machine_change_timing`), `drive/drivesync.c`, `core/ciacore.c`. Read end to end before the
port (doctrine rule 5); every table below is copied, not re-derived.

---

## §1 What exists (verified 2026-09-19)

**No video-standard model exists.** No enum, no struct, nothing named ntsc / 6567 in the core.
What is already parameterised: `VicII.cycles_per_line` and `screen_height` drive the line
wrap and the end of frame; BA, sprite DMA, borders, VC/RC and crunch are read from cycle-table
flags, not cycle numbers; the raster IRQ is an equality compare; `FB_W = 520` (65×8) and
`FB_H = 312` are already large enough for NTSC; `color_latency` is the 6569/8565 knob;
`ResidConfig.clock_freq` and an unused `NTSC_CLOCK_FREQ = 1022730` exist; `Cia.tod_power_freq`
is a field and is saved; `ScenarioPlayer` already takes `cycles_per_frame`; the BIN_VIC header
carries width and height and both clients honour them.

What is PAL by construction:

| what | where | count |
|---|---|---|
| the fetch schedule | `vic.rs:404-537` `CYCLE_TAB_PAL: [Row; 126]`, `build_cycle_table() -> [u32; 63]`, field `cycle_table: [u32; 63]`; xpos from a PAL formula (`vic.rs:584`) | 1 literal table, fixed in the type |
| frame length 19656 | `streaming::CYC_PER_FRAME` + copies (`cli/engine.rs:29`, `scenario_player.rs:26`, literals) | 12 non-test sites, ~35 in tests |
| CPU clock 985248 | `cia.rs:58`, `resid_ffi.rs:95`, `streaming.rs:68`, `main.rs:16111`, `main.rs:16741` (6 uses), `cli/main.rs:502` | 7 definitions |
| 50 fps / 20 ms | `main.rs:15626, 17009, 17118`, `checkpoint_ring.rs:85`, `cli/window.rs:137` | 5 |
| drive sync factor | `drive.rs:514-519` `DRIVE_SYNC_FACTOR_PAL = 66517` | 1 |
| CIA TOD tick | `cia.rs:653` `PAL_CYCLES_PER_SEC / tod_power_freq`; `Cia::new` sets 50 Hz | 1 |
| the visible crop 384×272 from line 16 | `render.rs:83-86`, `vic_inspect.rs:515-518`, `vic_line_trace.rs:514-517`, `cli/window.rs:100`, `main.rs:13047`, FFI + play-API docs | 3 copies in core |
| 63 / 312 in the line recorder and frame map (859/860) | `vic_line_trace.rs` (~20 uses; the trick rules test literal cycles 15, 20, 29, 56-63) | 1 file |
| `"c64-pal"` | `main.rs:7105, 14404, 14503, 17258`, `convert_cmd.rs:86` | 5 |

**Identity is neither recorded nor enforced.** `machine_profile_from_model` drops everything
after the `-`, so `"c64-ntsc"` parses as a PAL C64; the c64re and VSF VIC model bytes are
written as 0 and ignored on read; the drive snapshot's `MACHINE_SYNC_PAL` is read and thrown
away. **A restore across standards can panic today:** a `raster_cycle` ≥ 63 never equals
`cycles_per_line` and indexes past `cycle_table` (`vic.rs:1366, 1426`).

**No KERNAL patch is needed.** VICE ships no NTSC KERNAL: C64 NTSC uses 901227-03, the same
file as PAL (`c64scmodel.c:126`), and nothing writes `$02A6` — the KERNAL's own raster test
sets the PAL/NTSC flag from whatever line count the VIC produces.

## §2 The machines (from VICE, `c64scmodel.c:105-191`, `vicii-chip-model.c`, `c64.h:35-60`)

| model | chip | cycles × lines | cycles/frame | CPU Hz | frame rate | TOD | table |
|---|---|---|---|---|---|---|---|
| C64 PAL (today) | 6569 | 63 × 312 | 19 656 | 985 248 | 50.1245 | 50 Hz | `cycle_tab_pal` |
| **C64 NTSC** | **6567R8** | **65 × 263** | **17 095** | **1 022 730** | **59.826** | **60 Hz** | `cycle_tab_ntsc` |
| C64 OLD NTSC | 6567R56A | 64 × 262 | 16 768 | 1 022 730 | 60.993 | 60 Hz | `cycle_tab_ntsc_old` |
| C64 PAL-N (Drean) | 6572 | 65 × 312 | 20 280 | 1 023 440 | 50.466 | 50 Hz | `cycle_tab_ntsc` |

All of these are rows of one model file (§3 D1); which of them run is decided by the building
blocks that exist, not by this spec.

What the 65-cycle line changes (VICE `cycle_tab_ntsc`, `vicii-chip-model.c:272-403`):
cycles 11–57 are identical to PAL (refresh 11–15, BA 12–54, c-access Φ2 15–54, g-access Φ1
16–55, VC/RC/MCBASE, crunch, border checks). The two extra cycles are **idle at 10 and at 58**:
sprites 3–7 fetch one cycle earlier (s3 DMA at 1, s4 2–3 … s7 8–9), sprites 0–2 one later
(s0 59–60, s1 61–62, s2 63–64, s3 pointer at 65); ChkSprDma at 56/57, ChkSprDisp at 59,
sprite-0 BA from 56. xpos starts at `$19C` and repeats `$184` (Φ1 62, Φ2 62, Φ1 63) — so the
table's xpos column comes back instead of the PAL formula. Line 0 is still seen at cycle 2
and the compare stays edge-triggered.

## §3 TRX64 — one timing record, one source

**D1 — a C64 model is configuration, not code.** The owner, on which NTSC machines: "wenn
wir das gleich in eine Property auslagern können — dann ist doch jeder weitere C64-Subtyp nur
Config." So the model is a **data file**, `crates/trx64-core/models.toml` (embedded at build,
parsed at startup), one row per model, mirroring VICE's model table (`c64scmodel.c:105-191`):
name and aliases, VIC chip, cycle-table family, raster lines, CPU Hz, TOD mains Hz,
`color_latency`, lightpen retrigger X, display window, CIA model, SID model, glue logic, KERNAL /
BASIC / chargen file names. Derived, never written in a row: `cycles_per_line` (from the
family), `cycles_per_frame`, frame rate, `drive_sync_factor = floor(65536e6 / cpu_hz)`
(66517 / 64079).

Code holds only the **building blocks** a row can name: the cycle-table families, ported 1:1
from VICE with their xpos column (`pal` today; `ntsc` and `ntsc_old` with this spec), the
6569/8565 `color_latency` path, reSID's 6581/8580. A row that names a block TRX64 does not have
is **refused at startup with the block's name** — never silently mapped to something close.
Today that refuses the 6526A CIA, the custom-IC glue logic and KERNAL rev1/rev2 (not in the ROM
set), so of VICE's rows these run once 863 is built:

| row | chip / family | CIA / SID | runs after 863 |
|---|---|---|---|
| `c64-pal` (default) | 6569 / pal | 6526 / 6581 | yes — today's machine |
| `c64-ntsc` | 6567R8 / ntsc | 6526 / 6581 | **yes — the point of this spec** |
| `c64-paln` (Drean) | 6572 / ntsc, 312 lines | 6526 / 6581 | yes |
| `c64c-pal`, `c64c-ntsc` | 8565, 8562 | **6526A** / 8580 | no — needs the 6526A and custom glue |
| `c64-old-pal` | 6569R1 / pal | 6526 / 6581 | no — needs KERNAL rev2 (and 6569R1's lightpen/lumas) |
| `c64-old-ntsc` | 6567R56A / ntsc_old | 6526 / 6581 | no — needs KERNAL rev1 |

Each "no" becomes a yes by building its block — never by editing the engine for that model.
The cycle tables become data the VIC indexes (65 entries, `cycles_per_line` deciding how many
run), no longer part of the type.

**D2 — the machine carries it and everyone reads it.** `Machine::timing()` is the one
answer to "how long is a frame, how fast is the clock". Every copy in §1's table goes:
`CYC_PER_FRAME` (streaming, run/tick defaults, checkpoint restore re-sim, transport
stepping, autocapture), the seven clock constants, the 50 fps / 20 ms constants (ring
sizing, transport span, `ms_per_step`, the CLI pump and window), `ScenarioPlayer`'s default,
the `advance_to_frame` cap. What is PAL only by name (`pal_cycle`, pacing mode `"pal"`) is
renamed to what it means (`"realtime"`), with `"pal"` kept as an accepted alias.

**D3 — every clock consumer takes the model's clock.** reSID `clock_freq` (sampling
re-initialised, VICE `sid_init` → `set_sampling_parameters`); CIA TOD mains tick
`cpu_hz / tod_hz` with the CRA bit-7 ring counter unchanged (60 Hz mains on NTSC — the
hardware tick comes from the model, CRA7 only picks the divider); the drive's catch-up ratio
(`drive_sync_factor`) — the 1541 stays at a true 1 MHz, only its ratio to the C64 moves.

**D4 — the picture.** NTSC's display window (VICE `vicii-timing.h`: lines `$1C–$112`) runs
past the last raster line, so raster lines 0–11 are drawn at the bottom and the frame's
visible edge — VICE emits vsync at line 12, not 0 (`vicii.c:440-452`) — is where the
displayed buffer swaps. The crop stops being one contiguous range starting at line 16: it
is the model's `display_window`, and the three core copies (render, vic_inspect,
vic_line_trace) and the thumbnail path read it. The canvas size follows VICE's NTSC window
(to be confirmed at build against `vicii-timing.h`; the BIN_VIC header already carries it,
the web player and the CLI blit already honour it). `SPRITE_DBUF_X0` and `DISPLAY_X0` are
re-derived for the NTSC xpos, not assumed.

**D5 — choosing the model.** Orthogonal to the machine profile (a U64 can be NTSC):
- daemon `--model <row>` (default `c64-pal`; `--video pal|ntsc` as shorthand for the two
  base rows), applied **before** any cycle runs — the same place `--machine` is applied,
  before the warm-up in `do_power_on`;
- `trx64cli --model`, sandbox batch items, `session/create` (whose `pal` flag is accepted
  and ignored today) — all the same field; `session/models` lists the rows and which of
  them run;
- a runtime switch, `session/model {name}` and a monitor verb `model <row>`, which is
  **not** a power cycle. VICE power-cycles (`machine_change_timing` ends in
  `MACHINE_RESET_MODE_POWER_CYCLE`) and a real machine would need another crystal; the owner:
  "An der Framegrenze wäre gut — freeze, State halten, cycle, restore." So the switch is a
  **transplant at the frame boundary**: pause → `advance_to_frame` (raster line 0, cycle 1 —
  the VIC's most neutral point, before any display fetch) → capture the whole machine state →
  build the machine on the new row → restore the captured state into it → run on. Nothing in
  the state is tied to the standard: CPU, RAM, CIA registers and timers and SID registers are
  values; the drive keeps its own clock and only its catch-up ratio moves; reSID is re-sampled
  at the new clock; the TOD ticks at the new mains rate from here on. The VIC continues at line
  0 of the new geometry.
  What it means, said where it is shown: the running program detected its standard at boot —
  `$02A6`, its timer values, its raster lines stay what they were. The switch shows **how this
  running program behaves on the other machine**; a program that should boot as NTSC is
  started with the power button or a reset after the switch;
- the standard is session identity like 815's claim: it survives a warm reset (which
  rebuilds `VicII` and `Cia` — so it is re-applied there, as `speed_profile` is) and a
  power-cycle; only a new session starts at the default.

**D6 — the model is part of the state.** Every snapshot format records the row: the native
manifest's `machine.model` (`c64-pal` / `c64-ntsc` / …, no longer truncated at the `-`), the
c64re `VicSnapshot.model`, the drive snapshot's sync, VSF export/import's VIC model byte, and
every checkpoint-ring and recorder anchor. **A restore puts the machine back on the row it was
captured on** — that is the D5 transplant run backwards, so rewinding across a switch just
works: an anchor from before it is PAL and makes the machine PAL again. A row that cannot run
here (§3 D1) is refused by name. The latent panic goes with it: geometry and state always
arrive together, and a restored `raster_cycle ≥ cycles_per_line` is rejected before it could
index the table.

**D7 — the state says what the machine is.** `session/state`, `monitor/state` and the A/V
hello carry `model`, `videoStandard`, `chip`, `cyclesPerLine`, `linesPerFrame`, `cyclesPerFrame`,
`cpuHz`, `frameRate`. The line recorder's header already has fields for chip,
cyclesPerLine, linesPerFrame and fbOrigin — they stop being constants.

**D8 — the line recorder and the frame map (859/860) take the model.** Loops over
`lines × cycles_per_line`; `frame_position`, `displayed_frame_start` and `record_frame` from
the timing record; the trick rules read cycle positions from the cycle table's flags
(first c-access, sprite DMA slots, border checks) instead of the literal PAL numbers 15, 20,
29, 56–63; the visible-line test reads the display window.

## §4 C64RE — the switch, and every place that counts in frames

**C1 — the switch.**
- The Live tab's machine controls get a **model** selector beside the power button, filled
  from `session/models` (rows that cannot run are shown, disabled, with the missing block).
  It says what it does ("switches at the next frame; the running program keeps its state and
  the standard it detected at boot — power-cycle for a clean NTSC start") and asks before it
  does it. It calls `session/model`; it keeps no list of models of its own.
- MCP: `runtime_session_start` gets `model` (replacing the `pal` boolean that today says
  "NTSC is not supported"); `runtime_sandbox_run` the same per run; the monitor verb is
  reachable through `runtime_monitor` as for every verb (TRX64 owns it).
- **A project remembers its model.** `knowledge/project.json` → `machine.model`, set by
  `project_init` (default `c64-pal`) or later; the workspace launcher starts the daemon with
  `--model` from it, so an NTSC release boots as NTSC without anyone remembering to switch.

**C2 — nothing in C64RE assumes 19656.** Every frame count reads the runtime's timing
(D7): `PAL_CYCLES_PER_FRAME` in `scenario-gherkin.ts` and its users (`reel/run-sandbox`,
`run-scenario`, `record-scenario`, the headless `hold_frames`), the Export tab's seconds
(`985248`), the tool descriptions that say "0..311 (PAL)" and "312×63". A recorded
scenario records the standard it was recorded on and refuses to replay on the other one —
input stamped by cycle means nothing across a different clock.

**C3 — the VIC view (860) and the line strip (859) take the geometry from the header.**
Cycles per line, lines, the framebuffer origin and the blanking cycles come from the
recorder's header, not from `63`, `312` and `FB_ORIGIN (104, 16)`; NTSC's wrapped visible
window is drawn as the recorder describes it.

## §5 Scope

- **PAL stays bit-identical.** No PAL output, timing or snapshot changes; every existing
  gate passes unchanged. This is the first acceptance item, not the last.
- **Not in this spec:** the NTSC colour decoder (VICE's YIQ path and CRT filter) — the palette
  stays Colodore for every model (§9 Q3); pixel aspect; the lightpen beyond
  its retrigger X; datasette and RS-232 rates; turbo profiles on NTSC (851's clock divider
  keeps `clk` as PHI2 — nothing prevents it, nothing tests it); C128.

## §6 Acceptance — TRX64

1. **PAL unchanged.** `scripts/gate.sh` green; the 7-game gate, `iso_vic_gate`,
   `vic_line_trace_gate`, `cia_tod_gate`, the snapshot tests — all unchanged, byte-identical
   where they compare bytes.
2. **The table is VICE's.** A test compares the NTSC cycle table row by row, both phases,
   with VICE `cycle_tab_ntsc` (fetch type, BA mask, flags, xpos) — as 859 did for PAL.
3. **The frame.** On NTSC a frame is 17 095 cycles; the raster wraps at 263; line 0 appears at
   cycle 2; a raster IRQ on line 262 fires, one on line 263 never does.
4. **The KERNAL detects it.** A cold boot on NTSC leaves `$02A6 = 0`; on PAL `1` — with the
   same ROM file.
5. **Clocks.** CIA TOD counts 60 ticks per emulated second on NTSC (and CRA7 still picks the
   divider); reSID is sampled at 1 022 730 Hz (a fixed register value gives the NTSC pitch);
   a D64 `LOAD` completes on NTSC (the drive ratio).
6. **Stolen cycles.** On an NTSC bad line with sprites 0 and 3 on, the line recorder shows
   the BA/stall cycles where VICE's table puts them (sprite 0 from 56, sprite 3 at 1 and 65).
7. **Identity and the transplant.** `--model c64-ntsc` survives `reset` and `power off/on`.
   A running PAL program switched to NTSC keeps its RAM, CPU, CIA and SID state bit-identical
   across the switch (compared field by field), the VIC continues at line 0 of 263, and the
   next frame is 17 095 cycles. Rewinding to a checkpoint from before the switch makes the
   machine PAL again with the checkpoint's state; `c64-ntsc` round-trips through dump/undump;
   a snapshot of a row that cannot run here is refused by name, and nothing panics.
8. **Models are rows.** `models.toml` parses; `session/models` lists every row with whether
   it runs; `--model c64c-pal` is refused at startup naming the missing block (6526A), and
   `--model c64-paln` runs 65 × 312 at 1 023 440 Hz with a 50 Hz TOD — without a line of
   engine code written for it.
9. **The picture.** An NTSC frame arrives with the NTSC canvas size in the BIN_VIC header, at
   ~59.83 frames/s in real-time pacing; the bottom rows are raster lines 0–11 of the same
   displayed frame (a raster bar at line 5 shows at the bottom, not the top).

## §7 Acceptance — C64RE

1. The Live-tab selector switches the running session at the next frame boundary after a
   confirmation that says what the switch keeps (the running program, its detected standard)
   and what a cold start would do instead; the machine is NTSC afterwards
   (`session/state.model`) with the program still running; cancelling changes nothing.
2. `runtime_session_start { model: "c64-ntsc" }` starts an NTSC machine; the project's
   `machine.model` makes the workspace start NTSC.
3. A scenario recorded on PAL refuses to replay on NTSC, naming both; `hold_frames: 60` on
   NTSC holds 60 × 17 095 cycles.
4. The VIC view on an NTSC frame draws 65 cycles × 263 lines, with the wrapped window where
   the recorder says it is.

## §8 Files (sketch)

TRX64: `models.toml` (the rows) + its loader, `vic.rs` (the table families, the field), `render.rs` / `vic_inspect.rs` /
`vic_line_trace.rs` (the display window), `cia.rs`, `drive.rs`, `resid_ffi.rs`, `lib.rs`
(`timing()`, warm reset, the setter), `streaming.rs`, `main.rs` (flags, `session/model` + `session/models`, the
monitor verb, state, snapshot identity, every `CYC_PER_FRAME`), `c64re_snapshot.rs`,
`native_snapshot.rs`, `vsf*.rs`, `drive_snapshot.rs`, `scenario_player.rs`, the CLI (`--video`,
pump, window). C64RE: `MachineControls.tsx`, `headless.ts`, `runtime-sandbox.ts`,
`scenario-gherkin.ts` + `reel/*`, `ExploreOverlay.tsx` / `VicLineView.tsx`, `Export.tsx`,
`project-knowledge/types.ts` + `service.ts` (the project default), `scripts/workspace.mjs`.

Found along the way, fixed with it: `scenario_player.rs:23` says "NTSC = 17030" (it is 17 095
for R8, 16 768 for R56A); `docs/vice-iec-arc42.md:603-605` quotes sync factors 66514/64092
(they are 66517/64079).

## §9 Open questions — one at a time

1. ~~Which NTSC machines?~~ **Settled 2026-09-19:** every C64 model is a row of one model
   file (D1); a row runs when the blocks it names exist. After 863: `c64-pal`, `c64-ntsc`,
   `c64-paln`.
2. ~~Switching at runtime.~~ **Settled 2026-09-19:** at the frame boundary, as a transplant
   (freeze → capture → rebuild on the new row → restore), not a power cycle (D5); the model is
   part of every snapshot and checkpoint (D6).
3. ~~Colour.~~ **Settled 2026-09-19:** the palette stays Colodore for every model ("Lass mal die
   Palette"). VICE itself gives the 6567R8 the same palette as the 6569 (`vicii-color.c:630-649`);
   its NTSC difference is the optional YIQ/CRT path, which stays out.

## §10 As built — TRX64 half (2026-09-19, branch `spec-863-ntsc`)

**Rows.** `crates/trx64-core/models.toml` carries VICE's seven C64 rows; `model.rs` parses and
validates them once (unknown fields, families, chips, a second row per chip, a window the
framebuffer cannot hold are errors; the daemon and CLI check the file at startup and exit
cleanly). A row that names a block TRX64 lacks is listed with `missing` and refused by name:
`c64c-pal`/`c64c-ntsc` (6526A CIA, custom-IC glue logic), `c64-old-pal` (KERNAL rev2 — not in
the ROM set; 6569R1 light-pen IRQ mode and luminances), `c64-old-ntsc` (KERNAL rev1; the
6567R56A's). The rows are identified in snapshots by their VIC-II (VICE `VICII_MODEL_*`), so the
file keeps one row per chip.

**Tables.** `cycle_tab_pal`, `cycle_tab_ntsc`, `cycle_tab_ntsc_old` are in `vic.rs` row for row,
every column including xpos and the log-only `visible`; `ntsc_gate` compares all 384 rows with
VICE's source text. The compiled PAL table hashes to the pre-863 formula-built one. The VIC
holds a 65-entry table; `cycles_per_line` decides how many run. `LineGeometry` reads the first
c-/g-access, sprite 0's pointer fetch and the last right-border check from the flags; the line
recorder's trick rules and sample points use it. `sprite_dbuf_x`/`display_dbuf_x0` derive the
two calibrated origins from each table (136/112 on all three; NTSC sprites at X ≥ $188 land 8 px
further right, past the repeated $184).

**The picture.** A wrapped window draws lines before its first line into the rows below the
frame (VICE `raster_draw_buffer_ptr_update`), so NTSC's canvas is rows 28–274, 384×247, one
contiguous crop; the displayed buffer swaps at the end of line 11 (VICE's vsync at line 12).
PAL keeps its swap at the frame wrap and its historical draw row for the frame's first line
(rows 0 and 311 are outside every PAL window — the framebuffer bytes are unchanged). A displayed
NTSC frame for the recorder starts at line 12; `frame_map.cells` stay indexed by raster line.

**The switch is in place.** `session/model` / `model <row>` pause, advance to the raster wrap,
and call `Machine::switch_model`: every value in the machine stays where it is and only what the
row decides is replaced — the VIC's table and frame (the flags of the cycle in hand re-read from
the new table), the CIAs' TOD tick rate, the drive's catch-up ratio; reSID is re-sampled by the
stream (and the CLI's render thread) when it sees the new clock. This is §3 D5's
capture → rebuild → restore with the copy elided: the rebuilt machine would be this one with
those fields replaced. The reply reports `kept: {cpu, ram, cia, sid}`, and a daemon test compares
the machine field by field across the switch. A restore runs the same step first
(`put_on_model` from the checkpoint's VIC model byte), after checking that the snapshot's raster
position exists on that row — which is what retires the latent `raster_cycle ≥ cycles_per_line`
panic (the compact VSF, which carries no model, is checked against the machine's own row).

**Identity.** Session identity like the machine profile: `Session.model` builds every machine
the session makes (power-on, power-off blank), a warm reset rebuilds VIC and CIAs on the
machine's row, and after any request the daemon adopts the machine's row (a rewind across a
switch makes the session PAL again) and re-sizes the checkpoint ring for the frame rate. The dump
manifest's `machine.model` is the row name on the `c64` profile (`c64-pal` reads as it always
did) and profile-prefixed otherwise (`u64-ntsc`); the drive snapshot writes VICE's
`MachineVideoStandard` for NTSC/PAL-N and keeps the facade's 0 for PAL. `session/create {model}`
on another model **switches** the shared machine at the frame boundary exactly as `session/model`
does — the owner: "Switch von PAL → NTSC oder umgekehrt immer nur bei neuem Frame" — and replies
with `modelSwitch` (from, switchedAt, kept); a model never changes by a power cycle. A clean start
on the new model is the power button after the switch; a machine that is off becomes the model at
its next power-on. The A/V hello is a JSON notification `av/hello`, sent on subscribe and on every model
change. The input journal records the model it was armed on; a scenario naming another model is
refused, naming both.

**Wire.** Additive: `session/models`, `session/model`, `session/create.model`, the identity
fields in `session/state` / `monitor/state` / `session/create`, `av/hello`, `frame.model`,
`firstLine`, `displayWindow` and `geometry.visible.{lastLine,wraps}` in the frame map, `model`
in the input journal, the sandbox JSON and the `runtime/scenario_list` summaries; journal entries
of `kind: "model"`, and `runtime/scenario_run` inputs of `kind: "model"` with `modelSwitches` and
`model` in its result. One VALUE changed: `pacing.mode` reads `"realtime"`
where it read `"pal"` (`"pal"` is still accepted on input). No field changed shape; the epoch
stays `trx64-runtime/2`.

**Left out, on purpose:** the C64RE half (§4, §7) — built in C64RE against this branch; the
blocks the refused rows need; the NTSC colour decoder, pixel aspect, datasette/RS-232 rates and
turbo on NTSC (§5).

**The reverse-debug ring holds its seconds on every model.** It is sized by an
instructions-per-second estimate; the estimate is PAL's 300 000/s scaled to the fastest clock a
runnable row has (PAL-N, 1 023 440 Hz → 311 630/s — `delta_ring::ring_instr_per_second`), so the
10 s default is ≥ 10 s on NTSC and PAL-N (PAL's own figure held them ~9.6 s). One size for every
model rather than a resize on a switch: `DeltaRing::resize` drops the history by contract, so a
ring that followed the model would lose the whole reverse window at every switch and every rewind
across one, and nothing in an entry is tied to the model. PAL pays 3.9 % — 3 000 000 → 3 116 300
entries at 10 s, +4.1 MiB. Tests: `the_ring_holds_its_seconds_on_every_model` (`delta_ring.rs`),
`the_reverse_ring_holds_its_seconds_on_every_model_and_a_switch_keeps_it` (`ntsc_gate.rs`: the
knob reports the seconds it was given on each row, a PAL machine with history switched to NTSC
keeps capacity and entries).

**The scenario list names the recorded model.** A `runtime/scenario_list` summary carries `model`
— the machine the scenario was recorded on (a recording carries the model its input journal was
armed on), `null` for a scenario that names none — so a client turns `cycleBudget` into seconds
with the RECORDED machine's clock. C64RE's Export tab does (`scenarioDuration`), falling back to
the running machine's clock only for a runtime without the field, and says so on the tab. Tests:
`scenario_summaries_carry_the_recorded_model` (daemon; an NTSC-recorded scenario listed while the
machine is PAL), C64RE `e2e:863-model` and `smoke:863`.

**A switch while recording is in the recording.** The input journal records a model switch as
an entry `kind: "model"` at the cycle it happened on — after the advance, so a frame boundary by
construction — with `detail.name` / `detail.from`, who asked and through which door
(`session/model`, `session/create`, the monitor's `model`; on a machine that is off,
`atPowerOn`). The transplant is one function on the session, `switch_at_frame_boundary`, which
the live switch and the replay share: `runtime/scenario_run` takes an input `kind: "model"`
(payload: the row) and performs it at its cycle — the scenario player's `Model` step; durations
after it count the new model's frames — and reports `modelSwitches` and the model it ended on. A
scenario still starts on the model it names and is refused on another, naming both. C64RE writes
the step `the machine switches to <row>` where the journal has the switch (the wait before it
rounded down, so it ends inside the frame the switch closes; a press held across it is
held through it, written as `I start holding …` / `I release …`), runs it through `session/model` in the reel and sandbox runners, and counts the frames
after it in the new model's frames; REC no longer warns about a switch. Tests:
`a_switch_while_recording_is_journaled_and_replays_at_the_same_cycle` (a live run from a restore,
switched mid-frame, a press after it; the journal turned into a scenario replays the switch at
the same cycle and ends on the same cycle, RAM hash and raster position),
`a_switch_of_a_machine_that_is_off_is_journaled`, `a_model_step_switches_and_later_frames_are_the_new_models`
(`scenario_player.rs`); C64RE `e2e:863-model`, `smoke:863`, `smoke:814`.

**Acceptance, item by item (§6):**

| § | test | where |
|---|---|---|
| 1 | the full gate; `the_pal_table_is_the_one_trx64_had` (compiled table hash); the 7-game screenshots and a boot + 40 M-cycle disk-game fingerprint (state, checkpoint JSON, VSF, canvas, frame map) compared byte for byte with the pre-863 build | `scripts/gate.sh`, `vic.rs` |
| 2 | `the_cycle_tables_are_vices_row_by_row` — all three families, 384 rows, every column, against VICE's source; `the_ntsc_table_is_pal_with_two_idle_cycles` | `ntsc_gate.rs`, `vic.rs` |
| 3 | `an_ntsc_frame_is_17095_cycles_and_wraps_at_263` (periodicity, wrap at 263, line 0 at cycle 2, raster IRQ on 262 fires, on 263 never); `a_paln_frame_is_20280_cycles` | `vic.rs` |
| 4 | `a_cold_boot_detects_the_standard_from_the_same_kernal` ($02A6 = 0 NTSC, 1 PAL, one 901227-03) | `ntsc_gate.rs` |
| 5 | `tod_counts_sixty_mains_ticks_a_second_on_ntsc` (and CRA7 still picks the divider); `resid_samples_at_the_ntsc_clock` (pitch ratio 1 022 730 / 985 248); `a_d64_load_completes_on_ntsc` (same bytes as PAL; drive factor 64079) | `ntsc_gate.rs` |
| 6 | `ntsc_stolen_cycles_are_where_vices_table_puts_them` (bad line with sprites 0 and 3: BA 12–54, released at 55, sprite 0 from 56, sprite 3's s-accesses at 65 and 1) | `ntsc_gate.rs` |
| 7 | `the_switch_is_a_transplant_and_rewind_undoes_it` (field-by-field state across the switch, line 0 of 263, 17 095-cycle frames, warm/cold reset and power cycle keep it, rewind to a PAL anchor makes it PAL with the anchor's RAM); `ntsc_round_trips_through_dump_and_a_foreign_row_is_refused` (dump/undump; a C64C checkpoint refused naming the 6526A; cycle 64 on PAL refused; nothing changes); `set_model_keeps_state_and_rereads_the_flags` | daemon `main.rs`, `vic.rs` |
| 8 | `models_toml_parses_and_every_row_is_checked`, `the_three_rows_that_run_and_why_the_others_do_not`, `session_models_lists_every_row_and_what_it_lacks`, `a_row_that_cannot_run_is_refused_at_startup_by_name` (the real binary, `--model c64c-pal` → 6526A), `a_row_that_runs_starts_the_daemon_on_it` and `paln_is_a_row_and_c64c_is_refused_by_name` (65 × 312, 1 023 440 Hz, 50 Hz TOD) | `model.rs`, daemon, `ntsc_gate.rs` |
| 9 | `an_ntsc_frame_carries_its_canvas_in_the_bin_vic_header` (384 × 247, ~59.83 fps), `the_pace_is_the_models_frame_rate`, `an_ntsc_raster_bar_at_line_5_is_at_the_bottom`, `an_ntsc_replay_reproduces_the_picture_on_screen`, `an_ntsc_checkpoint_is_read_on_the_ntsc_window` | daemon, `streaming.rs`, `ntsc_gate.rs`, `vic_inspect.rs` |
