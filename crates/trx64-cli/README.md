# trx64-cli — cross-platform TUI cockpit + emulator window

A single Rust binary that drives the TRX64 C64 runtime **in-process**: a terminal
**cockpit** (ratatui) for control + debugging, and an optional native **emulator
window** (winit + cpal) to play and watch the live machine. No daemon, no WebSocket, no
FFI — it links the runtime library and calls it directly. One machine, shared by the
cockpit, the per-frame pump, and the window.

It runs on macOS, Linux, and Windows (winit + cpal + ratatui are all cross-platform);
the only build wrinkle is the vendored reSID C++, handled by the cross-build chain.

---

## Quick start

```bash
# the TUI cockpit (default)
cargo run -p trx64-cli --release

# cockpit + the emulator window at launch
cargo run -p trx64-cli --release -- --window

# one-shot: run a single command, print, exit (scripting / CI)
cargo run -p trx64-cli --release -- mon "d c000"
```

`--rom-dir <dir>` overrides the ROM directory (KERNAL/BASIC/CHARGEN + 1541); it defaults
to `$C64RE_ROOT/resources/roms`, matching the daemon.

The machine is **powered on and running** at startup — a real C64 boots when switched
on. `/pause` freezes it; `/run` resumes.

---

## Install — run it as `trx64cli`

The binary is named **`trx64cli`** (no dash). Two ways to get it on your shell, all three
OSes:

**Self-compile (needs Rust)** — the simplest path:

```bash
cargo install --path crates/trx64-cli
```

`cargo install` builds in release mode and drops `trx64cli` into `~/.cargo/bin`
(`%USERPROFILE%\.cargo\bin\trx64cli.exe` on Windows), which rustup already put on your
`PATH`. Then run **`trx64cli`** from any shell — macOS, Linux, Windows.

**Prebuilt (no Rust)** — the maintainer cross-builds a `trx64cli` per OS (macOS arm64
natively; Linux + Windows from the Mac via the Apple `container` chain — reSID C++
compiles inside a Linux container) and hands the binaries out directly. Drop the one for
your OS on your `PATH`.

> **ROMs are not bundled.** `trx64cli` needs `resources/roms` (KERNAL/BASIC/CHARGEN +
> 1541) — Commodore IP, never shipped in the binary or a release. Install them separately
> (the ROM script) and point `--rom-dir` at them (or keep them under
> `$C64RE_ROOT/resources/roms`).

---

## Three namespaces: bash for the emulator

The command line has three namespaces, picked by the first character:

| You type | Goes to |
|---|---|
| a **bare line** (`d c000`, `r`, `bk e000`, `g`, `trace on`, `whowrote d020`) | the **monitor** — the full ~128-verb VICE superset |
| a **`/`-prefixed** line (`/run`, `/mount disk.d64`, `/reset`) | a **VM / machine command** |
| a **`!`-prefixed** line (`!ls`, `!cd docs`, `!load "game.prg"`) | the **filesystem** — the monitor's file shell, re-prefixed |

The monitor is the surface you live in, so it's frictionless (no prefix). Machine and
meta commands are `/`-namespaced — discoverable (`/help`) and collision-free. The
filesystem verbs sit behind `!`, like a coding tool's shell escape.

> **`!` is a cockpit routing layer only.** The FS verbs (`pwd cd ls dir mkdir rmdir
> load save bload bsave`) live in the shared monitor and stay **bare-callable**
> everywhere else (WebSocket, C64RE `runtime_monitor`). In *this cockpit* a bare FS
> verb prints a one-line nudge to the `!` form, so `/`, `!`, and bare read cleanly.

### `/`-commands

| command | action |
|---|---|
| `/power on` · `/power off` | cold boot / halt + reset to powered-off |
| `/reset [cold\|warm]` | power-cycle (fresh DRAM) / RESET line (RAM kept) — boots + runs |
| `/run` | resume free-running |
| `/run <prg>` | load + autostart a `.prg` |
| `/pause` · `/step` | freeze / single-step one instruction |
| `/mount <path>` · `/eject` (`/umount`) | insert a `.d64`/`.g64` (disk swaps live) or `.crt` (cold-boots) / eject the cart or unmount drive 8 |
| `/load <prg>` | load a `.prg` into RAM (no run) |
| `/warp on\|off` | 8× / real-time PAL pacing |
| `/joystick off\|port1\|port2` | route WASD+Space to the joystick (off = type) |
| `/window` | spawn the native emulator window |
| `/dump <path>` · `/restore <path>` (`/undump`) | write / load a `.c64re` snapshot |
| `/ringdump <path>` · `/ringload <path>` | write / load a `.c64rering` reverse-debug buffer |
| `/settings` | read-only status (pacing / warp / joystick / disk / cart) |
| `/help` · `/quit` | list commands / exit |

### `!`-commands (the filesystem)

The monitor's file shell, re-prefixed with `!`. Paths are rooted at the project dir.

| command | action |
|---|---|
| `!pwd` · `!cd [dir]` · `!ls` (`!dir`) `[dir]` | print / change / list (`!cd` no arg = project dir) |
| `!mkdir <dir>` · `!rmdir <dir>` | make (recursive) / remove an empty directory |
| `!load "<f>" [addr]` · `!save "<f>" <a1> <a2>` | PRG load into RAM / save a RAM range as a PRG |
| `!bload "<f>" <addr>` · `!bsave "<f>" <a1> <a2>` | raw binary load / save (no header) |

Everything else (a bare line) is sent verbatim to the monitor — see
**[MONITOR.md](../../MONITOR.md)** for the full command reference (disassemble/dump/
assemble, breakpoints + observers, flow/backtrace, tracing, memory-map / taint /
swimlanes, and the reverse-debug verbs `rstep` / `whowrote` / `diff` / `chis` / crash
triage). Or run `help` (bare) in the cockpit for the live verb list.

### Media semantics (CRT ≠ Disk)

`/mount` and `/eject` behave like the real hardware — a disk swap and a cartridge
change are not the same event:

- **Disk mount** (`.d64`/`.g64`) swaps the medium only — **no reset, no
  power-cycle**. The floppy state and the running program survive, exactly like sliding
  a new disk into a live 1541.
- **CRT mount** (`.crt`) is a **power-cycle cold boot**: power off → insert → power on.
  Atomic, and the cockpit's run-state is reconciled so the machine visibly boots the
  cart (the pump resumes).
- **`/eject`** targets what's actually mounted (cart first, else disk). Ejecting a
  cartridge power-cycles (RAM wiped, like pulling a cart from a real C64); ejecting a
  disk just removes the medium.

---

## The cockpit

```
┌ CPU ───────────┐┌ MACHINE ───────┐┌ VIC ───────────┐
│ PC A X Y SP P  ││ run/pause warp ││ raster mode bg ││
└────────────────┘└────────────────┘└────────────────┘
┌ FLOW / VECTORS ────────────────────────────────────┐   IRQ/NMI vectors · stop reason
┌ OUTPUT / LOG ──────────────────────────────────────┐   command echoes + results
┌ command: > _ ──────────────────────────────────────┐
```

The panels refresh ~20 Hz from the live machine. The MACHINE panel shows RUNNING/PAUSED
(the host pump's authority), the pacing, and the cycle count.

### Cockpit UX

- **Mouse wheel** scrolls the OUTPUT/LOG pane. Any new output snaps back to the live
  tail; a `▲N` indicator in the title shows you're in scrollback.
- **↑ / ↓** walk the command history — persisted to `~/.trx64/history`, consecutive
  duplicates deduped, capped at ~2000 lines.
- **Tab** completes namespace-aware: `/`-verbs, `!`-verbs, or bare monitor verbs by
  prefix, and **paths** for any path argument (through quotes + spaces). A single match
  fills in (dir → trailing `/`, file → trailing space); an ambiguous prefix fills the
  longest common prefix and lists the candidates — path candidates **colored by
  filetype** (below).
- **← / → / Home / End** move the cursor; **Backspace / Delete** edit anywhere in the
  line (editing a recalled history line detaches it).
- **Readline muscles:** Ctrl-A (home), Ctrl-E (end), Ctrl-K (kill to end), Ctrl-U (kill
  to start), Ctrl-W (delete word before), Ctrl-L (clear the log).
- **Ctrl-C** clears a non-empty line (bash convention) or quits when the line is empty;
  **Ctrl-D** deletes forward, or quits on an empty line (EOF).

### Filetype colors (LS_COLORS-lite)

`!ls` output and Tab path-candidate lists are colored by type:

| color | filetypes |
|---|---|
| blue + bold | directories |
| yellow | `.crt` (cartridge) |
| cyan | `.d64` / `.g64` / `.p64` (disk image) |
| green | `.prg` / `.bin` (program / raw) |
| magenta | `.c64re` / `.c64retrace` / `.c64rering` (snapshot / trace / ring) |
| gray | `.asm` / `.tass` / `.md` / `.json` (source / text) |

---

## The emulator window (`--window` or `/window`)

A native window showing the live C64, on the same machine as the cockpit — **play in the
window, debug in the cockpit at the same time**.

- **Video** — the VIC framebuffer (384×272), blitted ~50 Hz.
- **Audio** — the runtime's persistent reSID engine, drained per frame into a ring and
  played via cpal (pre-roll + governor, underrun = silence; mirrors the SwiftUI app's
  AudioOutput).
- **Keyboard** — the **C64RE Spec 310 symbolic mapping**: printable keys map by the
  host-layout-resolved character (correct on QWERTZ etc. — no Y/Z swap, right
  punctuation); special keys (RETURN, DEL, RUN/STOP, the left-edge keys, function keys)
  map by physical position; the **arrow keys are the cursor keys**.
- **Joystick** — off by default (so WASD/Space type). `/joystick port1|port2` routes
  **WASD = directions, Space = fire** to that port; `/joystick off` returns them to the
  keyboard.

> The window and the cockpit are separate OS windows — only the focused one receives
> keys. Click the window to type into the C64, click the terminal to drive the cockpit.

---

## Run-state model

The host pump is the clock: a `/run`/`/pause` flag drives a per-frame loop that advances
the machine by **real wall-clock time** (so it runs at true PAL rate and audio stays at
44100 Hz). Daemon-side run intents are reconciled automatically — `/mount` and the
monitor `g`/`x` continue both resume the machine. A **JAM/KIL** halts cleanly (PC frozen
at the opcode, the FLOW line shows the stop) instead of hanging, so you can then
`whowrote` / `rstep` / triage the crash.

---

## Architecture

`trx64-cli` links `trx64-daemon`'s `[lib]` target and calls `dispatch()` + the A/V-pull
helpers directly. The winit `EventLoop` owns the main thread (a macOS requirement); the
cockpit and the per-frame pump run on worker threads; audio runs on cpal's thread — all
sharing one `Arc<Mutex<State>>`.
