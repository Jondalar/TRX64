# Command-line syntax convention — `/` = machine, bare = monitor

**Status:** ADOPTED in `trx64-cli` (commit `a978451`). **Recommended for every command
surface that drives the runtime** — the TUI cockpit, C64RE's monitor UI, and the App's
monitor console. One convention, same muscle memory everywhere.

## The rule

In any command line that drives the runtime:

| Input | Goes to | Examples |
|---|---|---|
| **bare line** (no prefix) | the **monitor** (`monitor/exec`, the ~128-verb VICE superset) | `d c000` · `m 0400` · `r` · `bk e000` · `g` · `trace on` · `rstep` · `whowrote d020` · `diff a b` |
| **`/`-prefixed** | a **VM / machine command** | `/power on` · `/reset cold` · `/run` · `/pause` · `/window` · `/quit` |

That's it: `if line.starts_with('/') { vm_command(line[1..]) } else { monitor_exec(line) }`.

## Why this way (and why everywhere)

- **The monitor is the surface you live in.** Disassemble, dump, break, trace, reverse-step —
  hundreds of times a session. It must be **frictionless**: a bare line, zero ceremony.
- **Machine/meta commands are rare and namespaced.** Power, reset, mount, snapshot, open a
  window — a handful, `/`-prefixed so they're **discoverable** (`/help`) and can't **collide**
  with the monitor's verb space.
- **It's the slash-command idiom** users already know (Claude Code, Slack, Discord, IRC): the
  bare channel is the content (here: the monitor); `/` is the command channel (here: the machine).
- **Same convention on every surface** = one muscle memory. The TUI, C64RE's monitor panel, and
  the App's console all parse identically, so a command typed in one works in another.

## The VM command set (`/`-prefixed)

| command | action |
|---|---|
| `/power on` \| `/power off` | cold boot / halt + reset to powered-off |
| `/reset [cold\|warm]` | power-cycle (fresh DRAM) / RESET line (RAM kept) — **boots + runs** |
| `/run` | resume free-running |
| `/run <prg>` | load + autostart a `.prg` |
| `/pause` | freeze the machine |
| `/step` | single-step one instruction |
| `/mount <path>` \| `/eject` | mount a `.d64`/`.g64`/`.crt` / unmount drive 8 |
| `/load <prg>` | load a `.prg` into RAM (no run) |
| `/warp on` \| `/warp off` | 8× / real-time PAL pacing |
| `/window` | spawn the native emulator window (TUI only) |
| `/dump <path>` \| `/restore <path>` | write / load a `.c64re` snapshot |
| `/ringdump <path>` \| `/ringload <path>` | write / load a `.c64rering` reverse-debug buffer |
| `/help` | list the VM commands |
| `/quit` | exit |

An unknown `/verb` is reported (`unknown command: /foo — try /help`) — it does **not** fall
through to the monitor, because the user explicitly chose the machine namespace. A bare
unknown line goes to the monitor, which reports its own error.

## Adopting it on C64RE / the App

Both already have a monitor input (bare → `monitor/exec` over WS / FFI). To adopt the
convention, add the thin `/`-dispatch in front of that input and map the VM verbs to the
existing JSON-RPC methods (`session/reset`, `debug/run`/`pause`/`step`, `media/mount`,
`snapshot/dump`, `ringbuffer/dump`, …). No runtime change — it's a front-end parsing layer,
identical to `trx64-cli`'s `exec_line` (`crates/trx64-cli/src/engine.rs`). The VM verbs that
need a window (`/window`) are TUI-only; the rest apply everywhere.
