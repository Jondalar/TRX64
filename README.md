# TRX64

A headless, cycle-accurate Commodore 64 + 1541 runtime. Snapshot the machine, rewind it,
step backwards, and ask a live trace who wrote an address.

**One daemon, one API, N front ends.** Headless and API-first: every capability is a
JSON-RPC method, so a script or an LLM agent drives it as completely as a person does. One
machine per process, shared by every client connected to it.

Humans get front ends on top: **`trx64cli`**, a terminal cockpit with a native emulator
window that links the runtime in-process, and
**[C64RE](https://github.com/Jondalar/C64ReverseEngineeringMCP)**, a reverse-engineering
workbench in the browser that talks to the daemon over the socket. Same verbs either way.

It runs real scene software: multi-stage cracks, custom fastloaders, EasyFlash carts.

![The trx64cli cockpit and the emulator window](docs/img/cockpit.png)

*`trx64cli` — terminal cockpit, native emulator window, standalone.*

C64RE is the sibling project: capability lives here, meaning and memory live there. Either
works without the other.

![The C64RE workbench driving TRX64](docs/img/c64re-workbench.png)

*The same machine through C64RE — live CPU, VIC, SID, drive and cart panels.*

---

## Install

Binaries for macOS, Linux and Windows (x86_64 + arm64):
**[Releases](https://github.com/Jondalar/TRX64/releases)** — each archive holds `trx64cli`
and `trx64-daemon`. C64 ROMs are not included; point at your own with `--rom-dir`.

```sh
brew install jondalar/tap/trx64
```

From source: `cargo build --release`. macOS, Linux and Windows all build natively —
MSVC included; the release binaries are built that way.

---

## Capabilities

- **Rewind** — play the machine backwards. Every step is a real restore, so registers,
  RAM and the drive are correct at each frame, and you can run on from where you stop.
- **Reverse stepping** — `rstep` undoes the last instructions byte-exact. An always-on
  ring keeps the recent past; nothing to arm in advance.
- **`whowrote <addr>`** — who last wrote here: PC, cycle, old → new.
- **JAM triage** — on a crash the monitor prints the chain: crash PC → wild jump → the
  stack corruptor.
- **Observers** — watch an address for exec, read or write, with a condition and an
  action, and *without halting the machine*. They watch the **address**, not the
  instruction: a write through `($fb),y`, a read through `($f0,x)` and a `jmp ($0314)` all
  trigger, because the hooks sit after the addressing mode is resolved.
- **Traces** — capture CPU / drive / IEC / memory to a binary log, index it, query it as
  swimlanes, memory maps or data-flow taint.
- **Marks & sandboxes** — name a point, come back to it, branch from it, throw the branch
  away.
- **Cartridges incl. Save-to-Flash** — EasyFlash, Ocean, Magic Desk, GMOD2/3, MegaByter.
  Flash and EEPROM writes persist across reset and through a snapshot.
- **Shared sessions** — one machine, several clients. A human and an agent drive the same
  live C64 at once.
- **Disks** — `.d64` / `.g64`, 35 to 42 tracks, with drive-side GCR writes persisted back
  to the host file.

---

## Standalone: the CLI cockpit

`trx64cli` is a complete emulator on its own — a terminal cockpit plus a native window, no
daemon, no server.

```sh
trx64cli                      # cockpit
trx64cli --window             # cockpit + emulator window
trx64cli mon "d c000"         # one-shot, prints and exits
trx64cli disasm game.prg      # static disassembly, no machine, no ROMs
```

Three namespaces on one command line: `/` drives the machine, `!` the filesystem, and a
bare line goes to the monitor. Tab completes all three.

```
/power on · /reset · /run · /pause · /warp on · /mount game.d64 · /window
F9 ◀| one frame back   F10 ◀◀ play back   F11 ⏸/▶   F12 |▶ one frame forward
```

Details: [`crates/trx64-cli/README.md`](crates/trx64-cli/README.md).

---

## Monitor

A VICE superset, ~128 verbs, on every front end. Full reference: **[MONITOR.md](MONITOR.md)**;
`help` prints the live list.

| | |
|---|---|
| **Run** | `g [addr]` go · `z`/`n` step into/over · `until <addr>` · `ret` |
| **Memory** | `m`/`d`/`a` dump / disassemble / assemble · `>` write · `f` fill · `t` transfer · `h` hunt |
| **Bank lens** | `m io d000`, `m ram e000` — see what the CPU sees, or the RAM under it |
| **Breakpoints** | `bk` exec · `wa`/`ws` watch read/write · `obs` non-halting observers |
| **CPU** | `r` registers · `chis` history · `bt` backtrace · `flow` IRQ/NMI focus |
| **Reverse** | `rstep` step back · `whowrote <addr>` · `chis` · `crash` triage |
| **Time** | `mark <name>` · `goto <name>` · `frame ±N` · `play back\|fwd` · `cadence` · `window <s>` |
| **State** | `dump`/`undump` `.c64re` · `ringdump`/`ringload` · `trace on\|off` |
| **Analysis** | `map` memory map · `taint` · `swimlane` · `diff <a> <b>` |
| **Drive** | `device drive8` then `r`/`m`/`d` — the 1541's own 6502 |

---

## Daemon & API

```sh
trx64-daemon --project <dir> --port 4312 [--stream]
```

JSON-RPC 2.0 over WebSocket. One machine per process, shared by every client.

```json
{ "jsonrpc": "2.0", "id": 1, "method": "session/create", "params": { "pal": true } }
```

A typical flow: `session/create` → `debug/run` → `monitor/exec` / `trace/*` / `vic/inspect`
→ `checkpoint/*` to scrub → `snapshot/dump` to persist. `--stream` adds the per-frame
driver (video, breakpoints, JAM auto-break, recorder); omit it for pure request/response.

For embedding, `trx64-ffi` exposes a typed uniffi library (Swift bindings) —
[`crates/trx64-ffi/API.md`](crates/trx64-ffi/API.md).

**Formats:** `.c64re` (full machine snapshot), `.c64rering` (the reverse-debug rings),
`.c64retrace` (binary trace log). VICE `.vsf` imports; export is not faithful.

---

## Contributing

This is my personal emulator, written for my own reverse-engineering work. You will want
things it does not do.

**Open an issue** — bugs, missing hardware behaviour, a title that misbehaves. A concrete
repro beats a feature description: which image, what you did, what happened. Pull requests
welcome. I cannot offer support, and I do not work from feature requests without either
code or a testable requirement.

---

## License

**GPL-3.0-or-later** — see [LICENSE](LICENSE). The emulation cores are a source-faithful
port of [VICE](https://vice-emu.sourceforge.io/) (GPL-2.0-or-later, used under "or later").
Credits in [THANKS.md](THANKS.md).

> At the request of Count Zero on behalf of the CSDb staff, any CSDb association has been
> removed. For TRX64 or C64RE, please reach out via GitHub.
