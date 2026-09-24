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

### `device` — C64 vs a drive
`device c64` (default) targets the main CPU. `device drive8` (or `drive9` … `drive11`) points
`r`/`m`/`d` at the **own 6502 of the drive at that unit**, 1541 or 1581, for read-inspection —
each drive runs a separate core. Bare `device` lists what is there now: the C64 and every
powered drive by its unit. A folder device has no CPU and is not a `device`.

### The reverse-debug ring — always on, no pre-arming
TRX64 continuously records the recent past into a **bounded in-memory ring**, so you can
look **backward from a crash** without having set anything up first. It is two flat slabs:

- a **delta ring** — per retired instruction: the CPU pre-state + every memory write with
  its `old → new` value. Backs `rstep` (undo) and `whowrote` (who changed an address).
- a **cpu-history ring** — per retired instruction: PC + opcode + registers. Backs `chis`.

Depth is **seconds of CPU time** (`TRX64_REVERSE_SECONDS`, default 10 s). At boot the delta
ring holds 10 s and the cpu-history ring 262 144 instructions, ~92 MB together.
`revdepth <seconds>` (1–600) rebuilds both rings to that depth and discards the current
history: 10 s ≈ 158 MB, 60 s ≈ 945 MB. Bare `revdepth` reports the current size. The ring
is **inspect-only**: `rstep` shows you the prior state, it does not resume from there.

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
| `reset [cold]` | warm reset — the RESET line, RAM and media kept; `reset cold` power-cycles |
| `model` | which C64 this is — PAL, NTSC or PAL-N: the VIC-II, the frame (cycles × lines), the clock, the frame rate, the canvas — and every model this build knows, with what the ones that cannot run are missing |
| `model <row>` | switch the running machine to another model (`c64-pal`, `c64-ntsc`, `c64-paln`) at the next frame boundary. Not a power cycle: the program keeps its state and the standard it detected at boot; `reset` or `power off`/`power on` afterwards for a clean start on the new model. The model survives resets and power cycles, and a snapshot or checkpoint restores the model it was taken on |

### Memory  (prefix any with a bank `lens`)
| command | what it does |
|---|---|
| `m [lens] <a> [b]` | memory dump ($20/row + PETSCII; default length $800) |
| `d [lens] [a] [end]` | disassemble a range, or ~16 lines from a/PC |
| `sd [n]` | step + disasm the **real executed path**, loops folded (dynamic) |
| `df [-i] [a] [n]` | follow-disasm: walk control flow statically (`-i` asks at branches) |
| `screen` | decode the 40×25 text screen (real screen pointer) |
| `io [1\|addr]` | I/O per device: register hex (peek) + decoded state |
| `iec` | the serial bus: each line's level and which device pulls it — the C64, every drive on the bus by its unit, each folder or host device — plus each end's port as its CPU reads it (`$DD00`; `$1800` on a 1541, `$4001` on a 1581) |
| `folder [unit]` | a folder device on the bus: its protocol state, open channels and last status. It has no CPU, so it is not a `device` to select |
| `pot [<1\|2> <x> <y> \| <1\|2> off]` | the POT lines (`$D419`/`$D41A`): which port CIA 1 selects, each port's value or open, what a read answers now, cycles to the next sample. With a port: set its x/y bytes (final, `$FF` = open), or clear it |
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
- `trace [domains]\|off` — start / stop a scoped trace capture (**bracket model**). Domains: `c64-cpu drive8-cpu iec vic memory drive-mechanism cart-read` (default `c64-cpu memory`); `do trace off` stops. The last two are the armed-only READ-SET lanes — `drive-mechanism` records which physical block the 1541 latched bytes off, `cart-read` which cartridge bank served which reads. Neither runs unless its domain is named.

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
| `device [c64\|drive<unit>]` | target the C64, or read-inspect the CPU of the drive at a unit (1541 or 1581) |

### State & trace
| command | what it does |
|---|---|
| `dump` / `undump <p>` | snapshot persist / restore (`.c64re`) |
| `savecrt ["<p>"]` | write live flash state to the mounted `.crt` (or a copy at `<p>`) |
| `swapcrt "<p>"` | hot-swap the `.crt`, **no reset** (same mapper — for build iteration) |
| `trace on\|off\|status\|mark` | the live trace gate |
| `tracedb start\|stop\|status\|mark` | declarative trace |
| `traceindex [path]` | build the queryable `.duckdb` index for a `.c64retrace` |
| `tracering <s> <e> [path]` | build a `.c64retrace` after the fact from the always-on reverse ring, for a cycle window still in it |
| `traprules <path>` / `traprules [clear]` | load / list / clear on-trap dump rules (JSON `{pc, label, dump:[[name,addr,len]], decode}`); printed when that PC is reached on a JAM or breakpoint |

### Media & drives
| command | what it does |
|---|---|
| `mount <path>` | put a `.d64`/`.g64`/`.d81`/`.crt`/`.prg` or a `.c64re` in the machine. The type comes from the file's content; a relative path resolves against `pwd`/`cd`. A cartridge power-cycles, a disk does not |
| `eject [cart\|disk\|<unit>]` | take it out: `eject 9` is the disk in the drive at unit 9, bare `eject` whatever is in (cartridge first). Both write back to the host file first; a cartridge eject cold-resets the machine |
| `drive [unit]` | live status of the drive at that unit (default 8): motor, track, LED, what is mounted, whether it is dirty |
| `cart` | cartridge status: type, bank, read/write activity |
| `drivepower [unit] [on\|off]` | switch that drive on or off (two powered drives at one unit are refused). Bare: power-on-reset the drive's 6502 only — the way out of a wedged fastloader |
| `recent` | the media this daemon has had mounted lately |
| `identify <path>` | what a file is, from its content: `c64re`/`crt`/`g64`/`d64`/`d81`/`prg`, and whether a PRG would autostart |

### Machine
| command | what it does |
|---|---|
| `run` | resume the machine (from a rewound point this cuts the anchors ahead) |
| `pause` | stop the machine and the transport; prints the ring range |
| `warp on\|off` | 8× pacing / real time at the model's frame rate |
| `power on\|off` | power the machine on (full init) or off |
| `rawframe on\|off` | stepping onto an anchor redraws its picture; `on` keeps the first, seamed frame so a one-frame event stays visible |
| `turbo` | which machine this session claims to be (`c64`, `128`, `u64`) and the speed that is set |
| `turbo mode c64\|128\|u64` | `c64`: `$D02F-$D03F` open bus. `128`: the VIC-IIe `$D02F`/`$D030` pair. `u64`: the speed register at `$D031`. Survives a reset |
| `turbo on\|off` | set / clear the speed bit (`$D030` bit 0, or `$D031`) |
| `turbo speed $NN` | the `$D031` value (u64). On `u64` the CPU runs at the clock it selects from the speed table; on `128` the bit is stored and the CPU stays at 1 MHz |

### Expansion port  (read-only — these report, they never change the device)
| command | what it does |
|---|---|
| `reu` | the REU: size, status, command, addresses, length, IRQ line |
| `georam` | the GeoRAM: size, bank, window (either verb reports whichever device is attached) |
| `uci` | the Ultimate Command Interface: state, pointers, lines |

### Marks & the rewind transport
| command | what it does |
|---|---|
| `mark <name>` | name and pin the anchor you are standing on (at most 32) |
| `marks` | list them: cycle, frame, how far back, window cost |
| `unmark <name>` | drop the name and the pin |
| `goto <name>` / `goto <frame>` / `goto c<cycle>` | jump to a mark, a frame, or the anchor at or before a cycle |
| `play back\|fwd [speed]` | play through the anchors; every step is a real restore. `play fwd` at the head just runs |
| `frame -N` / `frame +N` | step N anchors (stops at the ends) |
| `rewind` | mode, position, window, anchors held, and the keys: F9 one back, F10 play back, F11 pause/play, F12 one forward |
| `cadence` | capture rate, window, entry cap, anchors held |
| `cadence <frames> [secs]` | set the capture rate and retune the cap (~98 KiB per anchor); 1–3000 frames, 1–600 s |
| `window [seconds]` | how far back the checkpoint ring reaches (default 60 s, max 600) |

Watching is free: replaying keeps the anchors. An intervention — a write, a key, a resumed
run — cuts the future. A mark survives that cut.

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

### Names — none here
TRX64 is a runtime and holds no symbols: there is no `label`, `note`, `sym`, `inspect`,
`xref`, `save_labels` or `load_labels`. Every `monitor/exec` reply instead says WHERE it
printed each address — `spans: [{line, start, end, addr, space, role, lens?, len?}]` plus the
banking state in `machine` — and C64RE, which owns meaning, joins the names: in its
workbench monitor and in `runtime_monitor`. A name typed into a C64RE monitor command is
substituted by C64RE before the command reaches TRX64 (`a 1000 jmp start` arrives as
`a 1000 jmp $0810`). `trx64cli` on its own shows numbers.

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
