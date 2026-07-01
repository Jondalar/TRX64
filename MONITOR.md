# TRX64 Monitor — command reference

The monitor is a **VICE superset**: one method, `monitor/exec`, drives an interactive
debugger over the live machine. It is part of the runtime library, so the **same monitor**
is available everywhere TRX64 runs — the `trx64cli` cockpit, C64RE, the native app, and any
WS client.

How you reach it:

- **trx64-cli cockpit** — three command namespaces. A **bare line** is the monitor
  (`d c000`, `r`, `bk e000`); a **`/`-prefixed** line is machine control (`/run`,
  `/mount`, `/reset` — see the cockpit README); a **`!`-prefixed** line is the
  filesystem (`!ls`, `!cd`, `!load "…"` — the *File* verbs below, re-prefixed).
  **Tab** completes verbs in all three namespaces and paths for path arguments.
- **WebSocket** — `{"method":"monitor/exec","params":{"command":"d c000"}}`.
- **In the monitor** — `help` (or `?`) prints the live verb list.

Numbers are hex by default (`c000`, `$c000`). Run-control verbs (`g`, `until`, `z`, `n`)
advance the machine; everything else inspects without disturbing it (unless noted).

---

## Concepts

### Bank lens — what memory you see
The C64 maps RAM, ROM, and I/O into the same address space. `m`/`d` take a **lens** so you
read the layer you mean:

| lens | sees |
|---|---|
| `cpu` (default) | exactly what the CPU sees right now (the current bank config) |
| `ram` | the 64 KiB RAM underneath, ignoring ROM/I/O |
| `rom` | the KERNAL/BASIC/CHARGEN ROM |
| `io` | the I/O area ($D000–$DFFF: VIC/SID/CIA/colour) |
| `cart` | the cartridge's mapped ROM |

`bank [lens]` sets a sticky default so you don't repeat it. `sidefx on` makes monitor reads
trigger I/O side effects; the default (`off`) is a clean **peek**.

### `device` — C64 vs the 1541
`device c64` (default) targets the main CPU. `device drive8` points `r`/`m`/`d` at the
**1541's own 6502** for read-inspection — the drive runs a separate core.

### The reverse-debug ring — always on, no pre-arming
TRX64 continuously records the recent past into a **bounded in-memory ring**, so you can
look **backward from a crash** without having set anything up first. It is two flat slabs:

- a **delta ring** — per retired instruction: the CPU pre-state + every memory write with
  its `old → new` value. Backs `rstep` (undo) and `whowrote` (who changed an address).
- a **cpu-history ring** — per retired instruction: PC + opcode + registers. Backs `chis`.

Depth is **wall-clock seconds** (`TRX64_REVERSE_SECONDS`, default 10 s ≈ ~90 MB), and you
can change it live with `revdepth <seconds>` (1–600; this rebuilds the rings and discards
the current history). The ring is **inspect-only**: `rstep` shows you the prior state, it
does not resume from there.

### The checkpoint ring — scrub & diff
Separately, periodic **full-machine snapshots** (anchors) feed the scrub-filmstrip and
`diff <idA> <idB>` (what changed between two points). Anchor ids come from
`checkpoint/list`. `diff` is read-only.

### Traces — the forensic firehose
For deep analysis, **capture a trace**: `trace on` records CPU/drive/IEC/memory events to a
binary `.c64retrace` log; `traceindex` builds a queryable DuckDB index (oldest→newest, no
cap). The `map` / `taint` / `swimlane` analysis verbs read that trace.

### `ringdump` / `ringload` — the tester → dev hand-off
`ringdump <path>` serializes the **whole** reverse-debug buffer (checkpoint + delta +
cpu-history rings) into one gzipped `.c64rering` file. A dev `ringload`s it elsewhere and
then `scrub` / `rstep` / `whowrote` / `chis` / `diff` all work on the captured run.

### Observers & flow focus
**Observers** (`obs`) are conditional watchpoints (on exec/load/store) that can break, log
fields, mark, run a command, or toggle tracing. **Flow focus** (`focus`, `sf`/`nf`) scopes
stepping to a control-flow lane (main / IRQ / NMI / BRK), so you step through only the code
you care about across interrupts.

---

## Commands

### Execution
| command | what it does |
|---|---|
| `g [addr]` | go / resume the run-loop (optionally set PC); the Pause button halts |
| `x` | exit / resume (= `g`) |
| `until <addr>` | run until PC = addr, then stop (synchronous) |
| `z` / `step` | step **into** — may enter IRQ/NMI (VICE-correct) |
| `n` / `next` | step **over** — skips `JSR`, runs through IRQ/NMI |
| `ret` / `return` | run until the current frame returns (`RTS`/`RTI`) |
| `focus [mode]` | flow focus: `auto`/`main`/`irq`/`nmi`/`brk`/`clear` |
| `sf` / `nf` | step into / over, stopping only in the focused flow |
| `flow` | the interrupt/trap flow-frame stack |
| `bt` | backtrace (stack scan + flow frames) |
| `reset` | cold reset |

### Memory  (prefix any with a bank `lens`)
| command | what it does |
|---|---|
| `m [lens] <a> [b]` | memory dump ($20/row + PETSCII; default length $800) |
| `d [lens] [a] [end]` | disassemble a range, or ~16 lines from a/PC |
| `sd [n]` | step + disasm the **real executed path**, loops folded (dynamic) |
| `df [-i] [a] [n]` | follow-disasm: walk control flow statically (`-i` asks at branches) |
| `screen` | decode the 40×25 text screen (real screen pointer) |
| `io [1\|addr]` | I/O per device: register hex (peek) + decoded state |
| `bitmap <a> [w h] [mode]` | render a RAM range to a PNG (`hires`/`charset`/`sprite`) |
| `bank [lens]` | show / set the sticky default lens for `m`/`d` |
| `wr [lens] <a> <b..>` | write exactly these bytes from a |
| `f <a> <b> <d..>` | fill a..b with repeating data |
| `a <a> [instr]` | assemble; `a c000` enters assemble mode (empty line exits) |
| `t <a> <b> <dst>` | move/copy a..b to dst (overlap-safe) |
| `c <a> <b> <dst>` | compare a..b vs dst (list diffs) |
| `h <a> <b> <d..>` | hunt for a byte pattern (`xx` = wildcard) |

### Breakpoints & observers
| command | what it does |
|---|---|
| `bk` | list breakpoints (`#num $addr`) |
| `bk <a>` / `bk -<a>` | set / remove a breakpoint by address |
| `del <n..>` / `del` | delete by `#num` / delete all |
| `obs <name> when exec\|load\|store <a[..b]> [if <cond>] do <action>` | conditional observer (actions below) |
| `obs` / `obs log` | list observers / show log lines |
| `obs <name> on\|off\|del` | toggle/delete (name may glob: `obs * del`, `obs c* off`) |
| `ignore <name> [n]` | ignore the next n hits |

**`do <action>`** — one of:

- `break` — halt the run on hit (default).
- `log [fields]` — append a log line (non-halting). Fields: `a x y sp pc fl` or `$addr[:w]` (`:w` = 16-bit). E.g. `do log $fd $fe $ff a x y`.
- `mark ["label"]` — drop a trace bookmark on hit (default label = the observer name).
- `cmd "<monitor command>"` — run any monitor command on each hit.
- `trace [domains]\|off` — start / stop a scoped trace capture (**bracket model**). Domains: `c64-cpu drive8-cpu iec vic memory` (default `c64-cpu memory`); `do trace off` stops.

**Trace-bracket example** — capture only the `$4000..$4100` region, driven by exec events:

```
obs cap     when exec $4000 do trace c64-cpu memory   # start at $4000
obs cap_off when exec $4100 do trace off              # stop at $4100
```

**Condition operators:** `== != < > <= >= && || ( )` over `a/x/y/pc/sp/fl/rl/val/addr`.

### CPU
| command | what it does |
|---|---|
| `r` | registers (+ flow + IRQ/NMI vectors) |
| `r a=$42 x=$10` | set registers (`a/x/y/sp/pc/fl`) |
| `sidefx [on\|off]` | monitor-read side effects (default `off` = peek) |
| `device [c64\|drive8]` | target the C64 or the 1541 CPU |

### State & trace
| command | what it does |
|---|---|
| `dump` / `undump <p>` | snapshot persist / restore (`.c64re`) |
| `savecrt ["<p>"]` | write live flash state to the mounted `.crt` (or a copy at `<p>`) |
| `swapcrt "<p>"` | hot-swap the `.crt`, **no reset** (same mapper — for build iteration) |
| `trace on\|off\|status\|mark` | the live trace gate |
| `tracedb start\|stop\|status\|mark` | declarative trace |
| `traceindex [path]` | build the queryable `.duckdb` index for a `.c64retrace` |

### Analysis  (need a trace — `trace on` first)
| command | what it does |
|---|---|
| `map [cpu]` | memory map: free RAM / persistence surface |
| `taint <a> [cyc]` | data-flow taint backward from (cyc, addr) |
| `swimlane [list\|name] [s] [e]` | trace lanes (cpu/irq/nmi/io/1541) |
| `chis [cyc]` / `chis <s> <e>` | CPU instruction history (live ring first, then the trace) |

### Reverse-debug  (the always-on ring — no pre-arming, inspect-backward)
| command | what it does |
|---|---|
| `rstep [n]` / `reverse [n]` | **undo** the last n instructions: restore CPU+RAM+I/O bytes; report the landed regs + the writes rolled back |
| `whowrote <addr> [n]` | last n writer(s) of an address (newest first): PC + cycle + `old → new` — the stack-crash shortcut |
| `triage [pc]` | guided crash-triage: the causal chain (crash → wild `RTS`/`JMP` → stack corruptor). Auto-printed on a JAM, confidence-tagged |
| `revdepth [seconds]` | report / set the ring depth (1–600 s; rebuilds the rings, discards history) |
| `diff <idA> <idB>` | typed by-ID diff of two checkpoint anchors (RAM runs + per-chip register changes). Read-only |
| `ringdump <path>` | serialize the whole reverse-debug buffer → one gzipped `.c64rering` |
| `ringload <path>` | restore a `.c64rering` + the machine; scrub/rstep/whowrote/chis/diff then work on it |

### Knowledge  (reads the project `_analysis.json` covering the address)
| command | what it does |
|---|---|
| `inspect <a> [stem]` | segment kind/label + xrefs at an address |
| `xref <a> [stem]` | who calls/jumps/reads/writes an address (in + out) |
| `sym <name> [stem]` | reverse lookup: named routine/label → address |

### File  (rooted at the project dir)
> In the **trx64cli cockpit** these File verbs are reached with a `!` prefix
> (`!ls`, `!cd`, `!load "…"`); a bare `ls`/`cd`/… there prints a nudge to the `!`
> form. Everywhere else (WebSocket, C64RE `runtime_monitor`) they stay bare-callable.

| command | what it does |
|---|---|
| `pwd` / `cd [dir]` / `ls [dir]` | FS shell (`cd` with no arg = project dir) |
| `mkdir <dir>` / `rmdir <dir>` | make / remove a directory |
| `load "<f>" [addr]` | load a PRG into RAM (2-byte header, or override addr) |
| `save "<f>" <a1> <a2>` | save a1..a2 as a PRG |
| `bload "<f>" <addr>` | raw binary load (no header) |
| `bsave "<f>" <a1> <a2>` | raw binary save (no header) |

---

## Reverse-debug walkthrough — find what trashed the stack

A program crashes (or JAMs). Because the ring is always on, you can work backward
immediately:

```
triage                 # the causal chain: crash PC → the wild jump → the stack corruptor
whowrote 01fe          # who last wrote that stack slot (PC + cycle + old→new)
rstep 8                # undo 8 instructions — inspect the regs + RAM as they were
chis 2000              # the last 2000 cycles of executed instructions, around it
```

On a JAM the monitor auto-prints `triage` for you. To carry the whole scene to another
machine: `ringdump bug.c64rering` → send it → the dev `ringload`s it and runs the same
verbs on your exact run.
