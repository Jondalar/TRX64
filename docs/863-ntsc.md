# Spec 863 — NTSC: a second video standard, chosen before power-on

**Status:** PROPOSED (2026-09-19) — open questions in §9, to be settled one at a time.
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

v1 builds **C64 NTSC (6567R8)**; §9 Q1 decides whether R56A and PAL-N come along — the model
record in §3 makes each of them a row, not a rewrite.

What the 65-cycle line changes (VICE `cycle_tab_ntsc`, `vicii-chip-model.c:272-403`):
cycles 11–57 are identical to PAL (refresh 11–15, BA 12–54, c-access Φ2 15–54, g-access Φ1
16–55, VC/RC/MCBASE, crunch, border checks). The two extra cycles are **idle at 10 and at 58**:
sprites 3–7 fetch one cycle earlier (s3 DMA at 1, s4 2–3 … s7 8–9), sprites 0–2 one later
(s0 59–60, s1 61–62, s2 63–64, s3 pointer at 65); ChkSprDma at 56/57, ChkSprDisp at 59,
sprite-0 BA from 56. xpos starts at `$19C` and repeats `$184` (Φ1 62, Φ2 62, Φ1 63) — so the
table's xpos column comes back instead of the PAL formula. Line 0 is still seen at cycle 2
and the compare stays edge-triggered.

## §3 TRX64 — one timing record, one source

**D1 — a video model record.** `VideoModel { chip, cycles_per_line, lines, cycle_table,
cpu_hz, tod_hz, color_latency, lightpen_retrigger_x, display_window }`, one constant per
row of §2, the cycle tables ported **1:1 from VICE** with their xpos column. The table stops
being part of the type: `cycle_table` becomes a slice/array of 65 with `cycles_per_line`
deciding how much of it runs. Derived and never stored twice: `cycles_per_frame`,
`frame_rate`, `frame_period`, `drive_sync_factor = floor(65536e6 / cpu_hz)` (66517 / 64079).

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

**D5 — choosing the standard.** Orthogonal to the machine profile (a U64 can be NTSC):
- daemon `--video pal|ntsc` (default `pal`), applied **before** any cycle runs — the same
  place `--machine` is applied, before the warm-up in `do_power_on`;
- `trx64cli --video`, sandbox batch items, `session/create` (whose `pal` flag is accepted
  and ignored today) — all the same field;
- a runtime switch, `session/video {standard}` and a monitor verb `video pal|ntsc`, which is
  **a power cycle**: VICE does exactly this (`machine_change_timing` ends in
  `MACHINE_RESET_MODE_POWER_CYCLE`), RAM is not kept, the drives reset;
- the standard is session identity like 815's claim: it survives a warm reset (which
  rebuilds `VicII` and `Cia` — so it is re-applied there, as `speed_profile` is) and a
  power-cycle; only a new session starts at the default.

**D6 — identity is recorded and enforced.** Every snapshot format records the chip:
the native manifest's `machine.model` (`c64-pal` / `c64-ntsc`, no longer truncated at the
`-`), the c64re `VicSnapshot.model`, the drive snapshot's sync, VSF export/import's VIC model
byte. A restore into a machine of another standard is **refused** with both names — no
conversion, as VICE refuses (`SNAPSHOT_VICII_MODEL_MISMATCH`). The latent panic goes: a
restored `raster_cycle ≥ cycles_per_line` is rejected before it can index the table.

**D7 — the state says what the machine is.** `session/state`, `monitor/state` and the A/V
hello carry `videoStandard`, `chip`, `cyclesPerLine`, `linesPerFrame`, `cyclesPerFrame`,
`cpuHz`, `frameRate`. The line recorder's header already has fields for chip,
cyclesPerLine, linesPerFrame and fbOrigin — they stop being constants.

**D8 — the line recorder and the frame map (859/860) take the model.** Loops over
`lines × cycles_per_line`; `frame_position`, `displayed_frame_start` and `record_frame` from
the timing record; the trick rules read cycle positions from the cycle table's flags
(first c-access, sprite DMA slots, border checks) instead of the literal PAL numbers 15, 20,
29, 56–63; the visible-line test reads the display window.

## §4 C64RE — the switch, and every place that counts in frames

**C1 — the switch.**
- The Live tab's machine controls get a **PAL / NTSC** control beside the power button. It
  says what it does ("switching is a power cycle — the machine restarts") and asks before it
  does it. It calls `session/video`; it never talks to the runtime about anything else.
- MCP: `runtime_session_start` gets `video: "pal" | "ntsc"` (replacing the `pal` boolean
  that today says "NTSC is not supported"); `runtime_sandbox_run` the same per run; the
  monitor verb is reachable through `runtime_monitor` as for every verb (TRX64 owns it).
- **A project remembers its standard.** `knowledge/project.json` → `machine.videoStandard`,
  set by `project_init` (default PAL) or later; the workspace launcher starts the daemon with
  `--video` from it, so an NTSC release boots as NTSC without anyone remembering to switch.

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
- **Not in v1:** the NTSC colour decoder (VICE's YIQ path and CRT filter) — the palette stays
  Colodore for both, and §9 Q3 decides whether that holds; pixel aspect; the lightpen beyond
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
7. **Identity.** `--video ntsc` survives `reset` and `power off/on`; a PAL snapshot restored
   into an NTSC session is refused with both names and no panic, and vice versa; `c64-ntsc`
   round-trips through dump/undump.
8. **The picture.** An NTSC frame arrives with the NTSC canvas size in the BIN_VIC header, at
   ~59.83 frames/s in real-time pacing; the bottom rows are raster lines 0–11 of the same
   displayed frame (a raster bar at line 5 shows at the bottom, not the top).

## §7 Acceptance — C64RE

1. The Live-tab control switches the running session after a confirmation, and the machine
   comes back NTSC (`session/state.videoStandard`); cancelling changes nothing.
2. `runtime_session_start { video: "ntsc" }` starts an NTSC machine; the project's
   `machine.videoStandard` makes the workspace start NTSC.
3. A scenario recorded on PAL refuses to replay on NTSC, naming both; `hold_frames: 60` on
   NTSC holds 60 × 17 095 cycles.
4. The VIC view on an NTSC frame draws 65 cycles × 263 lines, with the wrapped window where
   the recorder says it is.

## §8 Files (sketch)

TRX64: `vic.rs` (the model record, the tables, the field), `render.rs` / `vic_inspect.rs` /
`vic_line_trace.rs` (the display window), `cia.rs`, `drive.rs`, `resid_ffi.rs`, `lib.rs`
(`timing()`, warm reset, the setter), `streaming.rs`, `main.rs` (flags, `session/video`, the
monitor verb, state, snapshot identity, every `CYC_PER_FRAME`), `c64re_snapshot.rs`,
`native_snapshot.rs`, `vsf*.rs`, `drive_snapshot.rs`, `scenario_player.rs`, the CLI (`--video`,
pump, window). C64RE: `MachineControls.tsx`, `headless.ts`, `runtime-sandbox.ts`,
`scenario-gherkin.ts` + `reel/*`, `ExploreOverlay.tsx` / `VicLineView.tsx`, `Export.tsx`,
`project-knowledge/types.ts` + `service.ts` (the project default), `scripts/workspace.mjs`.

Found along the way, fixed with it: `scenario_player.rs:23` says "NTSC = 17030" (it is 17 095
for R8, 16 768 for R56A); `docs/vice-iec-arc42.md:603-605` quotes sync factors 66514/64092
(they are 66517/64079).

## §9 Open questions — one at a time

1. **Which NTSC machines?** 6567R8 only, or also the old NTSC 6567R56A (64 cycles, rev1
   KERNAL) and PAL-N 6572 (Drean)? Each is a row in D1 plus its gates.
2. **Switching at runtime.** A power cycle from the Live tab (VICE's behaviour, RAM lost) —
   or only at start (daemon flag / project default), no runtime switch at all?
3. **Colour.** Keep Colodore for NTSC in v1, or bring an NTSC palette / VICE's YIQ decoder?
