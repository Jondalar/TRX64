# TRX64

A headless, cycle-accurate Commodore 64 + 1541 runtime in Rust.

**A daemon + API, N front ends.** Headless, API-first: every capability is a
JSON-RPC method, so a script or an LLM agent drives it as completely as a person does. One
machine per process, shared by every client connected to it.

Several user interfaces: **`trx64cli`**, a terminal cockpit with a native emulator
window that links the runtime in-process, and
**[C64RE](https://github.com/Jondalar/C64ReverseEngineeringMCP)**, a reverse-engineering
workbench in the browser.

Tested against multi-stage games, custom fastloaders and cartridges.

![The trx64cli cockpit and the emulator window](docs/img/cockpit.png)

*`trx64cli` — terminal cockpit, native emulator window, standalone.*

C64RE is the sibling project: capability lives here, meaning and memory live there. Either
works without the other.

![The C64RE workbench driving TRX64](docs/img/c64re-workbench.png)

*C64RE — embedded via WS in the browser, live CPU, VIC, SID, drive and cart panels.*

---

## Install

Binaries for macOS, Linux and Windows (x86_64 + arm64):
**[Releases](https://github.com/Jondalar/TRX64/releases)** — archives hold `trx64cli`
and `trx64-daemon`. C64 ROMs are not included; point at your own with `--rom-dir`.

```sh
brew install jondalar/tap/trx64
```

From source: `cargo build --release`. Builds natively (for Windows it uses MSVC).

---

## Capabilities

- **Rewind** — play the machine backwards, stop anywhere, run on. Each
  step restores registers, RAM, I/O and the medium
- **Reverse stepping** — `rstep` undoes the last instructions, byte-exact
- **`whowrote <addr>`** — PC, cycle, old → new
- **JAM triage** — crash PC → wild jump → stack corruptor
- **Observers** — watch an address for exec, read or write; condition, action
  Indirect addressing included: `sta ($fb),y`, `lda ($f0,x)`, `jmp ($5000)`
- **Traces** — CPU, drive, IEC and memory to a binary log; query as swimlanes, memory
  maps or data-flow taint
- **Marks & sandboxes** — name a point, jump back to it, branch, discard.
- **Cartridges** — EasyFlash, Ocean, Magic Desk, GMOD2/3, MegaByter. Flash and EEPROM
  writes survive a reset and a snapshot round trip.
- **Disks** — `.d64` / `.g64`, 35 to 42 tracks. Drive-side GCR writes reach the host file.
- **Expansion port** — REU (1700/1764/1750, oversized to 16 MB), GeoRAM, and the Ultimate
  Command Interface. Devices, not cartridges: several at once, and a host can lend its own RAM.
- **Machines** — `--machine c64|u64|128`. `u64` is the Ultimate 64 / Elite II / C64 Ultimate:
  the turbo registers, and a CPU that really runs — the firmware's own speed table, to 64 MHz.
- **PAL and NTSC** — `--model c64-pal|c64-ntsc|c64-paln` (or `--video pal|ntsc`). A C64 model
  is a row of `crates/trx64-core/models.toml`: the VIC-II and its cycle table, the frame, the
  clock, the mains the TOD counts, the ROMs. NTSC is the 6567R8 — 65 cycles × 263 lines at
  1 022 730 Hz, ~59.83 frames/s, a 384×247 picture whose bottom rows are raster lines 0–11.
  `model <row>` switches a running machine at the next frame; the program keeps its state and
  the standard it detected at boot. The C64C and first-revision rows are listed but need parts
  TRX64 does not have yet (the 6526A CIA, the custom-IC glue, KERNAL rev1/rev2), and are refused
  by name.
- **Shared sessions** — one machine, several clients, human and agent at once.
- **Snapshots** — `.c64re` full machine, `.c64rering` the reverse-debug buffers.

TRX64 includes reSID and DuckDB. The always-on reverse-debug ring costs ~120 MB at its
default depth of 10 seconds — ten seconds on every C64 model, the faster NTSC clock
included; `revdepth <s>` changes it, and 60 seconds is closer to a gigabyte.

---

## Standalone: the CLI cockpit

Terminal cockpit plus a native window in modern TUI style.

```sh
trx64cli                      # cockpit
trx64cli --window             # cockpit + emulator window
trx64cli mon "d c000"         # one-shot, prints and exits
trx64cli disasm game.prg      # static disassembly, no machine, no ROMs
```

Three namespaces on one command line: 
`/` drives the machine
`!` the filesystem
bare line goes to the monitor 

```
/power on · /reset · /run · /pause · /warp on · /mount game.d64 · /window
F9 ◀| one frame back   F10 ◀◀ play back   F11 ⏸/▶   F12 |▶ one frame forward
```

Details: [`crates/trx64-cli/README.md`](crates/trx64-cli/README.md).

---

## Monitor commands

Superset based on VICE, 123 verbs. Full reference: **[MONITOR.md](MONITOR.md)**;
`help` prints the live list.

| | |
|---|---|
| **Run** | `g [addr]` go · `z`/`n` step into/over · `until <addr>` · `ret` |
| **Memory** | `m`/`d`/`a` dump / disassemble / assemble · `>` write · `f` fill · `t` transfer · `h` hunt |
| **Bank lens** | `m io d000`, `m ram e000` — see what the CPU sees, or the RAM under it |
| **Breakpoints** | `bk` exec · `wa`/`ws` watch read/write · `obs` conditional observers |
| **CPU** | `r` registers · `chis` history · `bt` backtrace · `flow` IRQ/NMI focus |
| **Reverse** | `rstep` step back · `whowrote <addr>` · `chis` · `crash` triage |
| **Time** | `mark <name>` · `goto <name>` · `frame ±N` · `play back\|fwd` · `cadence` · `window <s>` |
| **State** | `dump`/`undump` `.c64re` · `ringdump`/`ringload` · `trace on\|off` |
| **Analysis** | `map` memory map · `taint` · `swimlane` · `diff <a> <b>` |
| **Drive** | `device drive8` then `r`/`m`/`d` — the 1541's own 6502 |
| **Expansion** | `reu` / `georam` the device decoded · `uci` the command interface · `turbo` the machine |

---

## Daemon & API

```sh
trx64-daemon --project <dir> --port 4312      # A/V streams by default
trx64-daemon --machine u64 --reu 512          # an Ultimate with a 512 KiB REU
trx64-daemon --georam 512                     # GeoRAM instead — the port holds one device
trx64-daemon --machine u64 --speed-table u64  # the first Ultimate 64 (default: u64ii)
trx64-daemon --model c64-ntsc                 # an NTSC C64 (--video ntsc is the same)
trx64-daemon --headless                       # no A/V, no auto-run: command-driven only
```

JSON-RPC 2.0 over WebSocket. One machine per process, serving one project: `ping` names it,
and `project/set` moves the daemon to another one in place — media written back and ejected,
the machine cold-started, every client told (`project/changed`).
`vic/line_trace` answers, for a frozen inspect checkpoint, what the VIC and the CPU did on a
raster line cycle by cycle — replayed in a clone of the machine, never on the live one —
and `vic/frame_map` the whole frame: the cell grid, the stores to the VIC, the objects on the
screen with their memory, and the raster techniques the frame uses.

```json
{ "jsonrpc": "2.0", "id": 1, "method": "session/create", "params": { "model": "c64-pal" } }
```

`session/models` lists every model with whether it runs here and what it lacks;
`session/model { "name": "c64-ntsc" }` switches the running machine at the next frame
boundary. `session/state` says which machine it is: `model`, `videoStandard`, `chip`,
`cyclesPerLine`, `linesPerFrame`, `cyclesPerFrame`, `cpuHz`, `frameRate`. Snapshots and
checkpoints record the model, and restoring one puts the machine back on it.

A typical flow: `session/create` → `debug/run` → `monitor/exec` / `trace/*` / `vic/inspect`
→ `checkpoint/*` to scrub → `snapshot/dump` to persist.

The per-frame driver — video, breakpoints, JAM auto-break, recorder — is on by default.
`--headless` opts out: no A/V push, no auto-run on connect, and the machine advances only
on an explicit `session/run`. That is the mode for byte-exact oracle and tool daemons.

For embedding in the Apple universe, `trx64-ffi` exposes a typed uniffi library (Swift bindings) —
[`crates/trx64-ffi/API.md`](crates/trx64-ffi/API.md).

**Formats:** `.c64re` machine snapshot, `.c64rering` reverse-debug buffers, `.c64retrace`
trace log. VICE `.vsf` imports, `.reu` images load with `--reu-image`.

### Environment switches

Everything below is read once, when a machine is built. Each defaults to ON and is turned off
with `0`, `off`, `false` or `no` — they exist to take a feature out of the hot path, or to put
the old behaviour back when something misbehaves.

| variable | default | what it does |
|---|---|---|
| `TRX64_CPUHISTORY` | on | The always-on reverse-debug rings: the last N instructions (`chis`) and the full-delta undo ring behind `reverse_step` / `who_wrote`. Off means both are inert — no recording, no reverse step, no ring dump. The per-frame checkpoint ring and its rewind transport are a different thing and stay. |
| `TRX64_REVERSE_SECONDS` | `10` | How deep those rings reach, in seconds of a 1 MHz machine. At turbo the same ring covers that many seconds of CPU time, so at 64 MHz it is about a sixtieth of the wall time. |
| `TRX64_TURBO_FASTPATH` | on | Above 1 MHz, instructions that neither advance the PHI2 clock nor touch anything but RAM run back to back inside one bus. No effect at 1 MHz. |
| `TRX64_CIA_ALARM_CHECK` | on | Catch a CIA up only when one of its timer alarms is due, as VICE's core does, instead of on every instruction and every cycle. |
| `TRX64_BIND` | `127.0.0.1` | The address the daemon binds. `0.0.0.0` in the container image. |

---

## What to expect

This is my (dkl / Jondalar) personal emulator I developed for my own needs when
reverse engineering C64 games. You might need different features or things -
and you are invited to contribute code. Use issues here on GitHub please. PRs only to contributors,
please reach out if you want to send code.

I will not answer feature requests without sample code / structured requirements and I
have no capabilities to give real support.

---

## License

**GPL-3.0-or-later** — see [LICENSE](LICENSE). The emulation cores are a source-faithful
port of [VICE](https://vice-emu.sourceforge.io/) (GPL-2.0-or-later, used under "or later").
Credits in [THANKS.md](THANKS.md).

> At the request of Count Zero on behalf of the CSDb staff, any CSDb association has been
> removed. For TRX64 or C64RE, please reach out via GitHub.
