# trx64cli — a C64 + 1541 you can interrogate

`trx64cli` is a Commodore 64 (+ 1541 drive) runtime with a terminal **cockpit** and an
optional native **emulator window**. It plays the real thing — cracks, fastloaders,
EasyFlash carts — and lets you do what a normal emulator can't: snapshot and rewind,
step *backwards*, and ask who corrupted the stack.

Pick your binary:

| OS | file |
|---|---|
| macOS (Apple Silicon) | `macos-arm64/trx64cli` |
| Linux (x86_64) | `linux-x86_64/trx64cli` |
| Windows (x86_64) | `windows-x86_64/trx64cli.exe` |

## Run

```
trx64cli            # the TUI cockpit
trx64cli --window   # cockpit + a native emulator window (play here, debug in the cockpit)
```

- **macOS:** the binary is unsigned — the first time, right-click → Open (or
  `xattr -d com.apple.quarantine trx64cli`), then it runs normally.
- **Windows:** SmartScreen may warn on the unsigned `.exe` → "More info" → "Run anyway".
- **Linux:** `chmod +x trx64cli` if needed.

## ROMs

trx64cli needs the C64 ROMs (KERNAL / BASIC / CHARGEN + the 1541 DOS). It looks for them
in a `roms/` directory next to the binary, or wherever you point it:

```
trx64cli --rom-dir /path/to/c64-roms
```

Expected files: `kernal-901227-03.bin`, `basic-901226-01.bin`,
`characters-901225-01.bin` (+ the 1541 `dos1541`). These are the standard VICE C64 ROM
dumps.

## Driving it

The cockpit has one rule: **a bare line goes to the monitor; `/`-commands drive the
machine.**

```
/help              # the machine commands
/window            # open the emulator window
/mount game.d64    # insert a disk (auto-runs)
/reset             # power-cycle
/pause   /run      # freeze / resume

d c000             # disassemble (monitor)
m 0400             # memory dump
r                  # registers
bk e000            # breakpoint
g                  # go
```

The good stuff — reverse-debug, always on, no setup:

```
whowrote d020      # who last wrote an address (PC + cycle + old→new)
rstep 8            # undo the last 8 instructions, byte-exact
triage             # on a crash: the causal chain (crash → wild jump → corruptor)
chis 2000          # the last 2000 cycles of executed instructions
```

In the **emulator window**: arrow keys = cursor, type into BASIC normally (German/other
layouts work). `/joystick port2` makes WASD+Space the joystick.

Full monitor reference: see `MONITOR.md` (in the source tree) or `/help` + `help`.

— TRX64. Have fun. 🕹
