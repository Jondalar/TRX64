# Spec — TRX64cli: cross-platform CLI + minimal emulator window

**Status:** PROPOSED (brainstorm → planning). **Repo:** TRX64 (`crates/trx64-cli`).
**Motivation:** the easiest cross-platform frontend. One Rust binary — a scriptable CLI
+ an optional native emulator window — that links the runtime **in-process** and runs on
macOS, Linux, and Windows. No Swift, no browser, no daemon, no WS, no FFI: Rust calls the
core directly. It's the natural answer to the inevitable Linux/Windows demand, and a
power-user / CI / scripting tool on top.

## Placement (it lives fully in TRX64)
`crates/trx64-cli` — a first-party frontend sibling to `trx64-core/trace/session/daemon/ffi`.
It links `trx64-daemon` (the `[lib]` target: `dispatch`, `SharedState`, the A/V-pull helpers)
and drives it **in-process**.

Contrast: the SwiftUI app lives in **TRX64-App** (Apple-specific UI, pinned). C64RE consumes
over **WS**. TRX64cli is Rust + cross-platform + in-process → it belongs in the runtime repo.

## Two parts

### 1. The TUI cockpit (ratatui) — the primary experience
A real terminal UI (ratatui), not a bare readline loop. It should FEEL like a debugger
cockpit: live panels + a command line you drive the machine from.
- **Panels (live, polled from `session/state`):** CPU registers + flags, cycles + run-state,
  VIC raster, a flow/backtrace line, and a scrolling output/log pane for command results.
- **One command line, two vocabularies:**
  - **High-level machine verbs** (the "feels like a real machine" layer, NOT raw monitor):
    `power on` / `power off` (cold boot / shut down), `reset [cold|warm]`, `run` / `pause` /
    `step`, `mount <path>` / `eject`, `load <prg>` / `run <prg>`, `warp on|off`,
    `window` (spawn the emulator window — see part 2), `dump`/`restore`, `help`, `quit`.
  - **Monitor syntax passthrough:** any monitor verb (`d`, `m`, `r`, `bk`, `g`, `trace`,
    `rstep`, `whowrote`, `diff`, …) → `monitor/exec` → the output pane. The full
    ~128-verb VICE-superset is available verbatim alongside the high-level verbs.
- **Also a non-TUI one-shot mode** for scripting / CI: `trx64 mon "d c000"` (run one command,
  print, exit) — pipeable, headless.
Reuses what exists: `dispatch()` + `monitor/exec` drive everything; the TUI is a ratatui
front + a small high-level-verb layer that maps to the same dispatch calls. In-process.

### 2. Spawnable emulator window (the `window` verb)
From the TUI you **spawn** a native window showing the live C64 — cross-platform via **winit**
(window/input) + a pixel blit + **cpal** (audio). It runs alongside the TUI (same machine):
play in the window, debug in the TUI.
- **Video:** pull `frameBuffer()` (the A/V-pull API — 384×272 palette+indices) per frame →
  blit to the window (a 16-colour LUT → RGBA). ~50 Hz.
- **Input:** keyboard → `session/key_down`/`key_up`; gamepad/keys → `session/joystick_*`.
- **Audio:** `audioDrain()` (mono i16 @ 44100) → a cpal output stream (the same ring/pre-roll
  idea as the SwiftUI `AudioOutput`).
- The monitor REPL runs alongside (a second thread / the terminal), so you **play in the
  window and debug in the CLI at the same time** — the same one machine.

Reuses the A/V-pull API built for the app (`frameBuffer`/`audioDrain`) verbatim — Rust calls
the core helpers directly (no FFI needed in-process).

## Cross-platform
winit + cpal both run natively on macOS, Linux, and Windows → **one binary, three OSes, no
Swift, no browser.** This is the most direct cross-platform play+debug front. The reSID C++
cross-compile is the only build wrinkle — covered by `spec-cross-platform-linux-windows.md`
(native CI matrix is cleanest; cargo-zigbuild for local mac-cross).

## How it fits the ecosystem (no duplication)
- **TRX64cli** — lightweight, cross-platform, scriptable: play + CLI-debug. Linux/Win + power users.
- **SwiftUI app** (TRX64-App) — the polished Apple front (scrub filmstrip, touch, AVAudioEngine).
- **C64RE** — the browser workbench (semantic RE + the analysis pipeline).
- All share THE runtime. TRX64cli is thin (a driver) + the window is minimal — not a second
  workbench, a cross-platform companion.

## Acceptance
- `trx64 mon "<cmd>"` and `trx64 repl` drive the in-process runtime (monitor + methods),
  headless, on macOS/Linux/Windows.
- `trx64 --window` opens a native window: live 384×272 video (frameBuffer), keyboard/joystick
  input, audio (audioDrain via cpal) — playable, with the monitor REPL usable alongside on the
  same machine.
- Pure Rust, in-process (links `trx64-daemon` lib), no daemon/WS/FFI dependency.
- Additive: a new crate; `trx64-core/daemon` behaviour + the conformance gate unchanged.

## Out of scope
- The full scrub/diff/time-travel UX (that's the SwiftUI app + C64RE's UI). The window is
  functional play; the CLI is functional debug. Deeper UX lives in the richer frontends.
- A GUI debugger window (use the CLI/REPL, or the app/C64RE).
