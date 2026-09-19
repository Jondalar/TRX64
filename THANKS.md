# Thanks & Credits

TRX64 stands on the shoulders of the people and projects below. It is a
faithful port, not a clean-room reimplementation — where it emulates the C64,
1541, VIC-II, CIA, VIA, IEC, GCR, the monitor, or the trace format, it follows
an existing, proven design line by line.

## VICE — the reference implementation

The vast majority of TRX64's emulation cores are a **direct, source-faithful
port of [VICE](https://vice-emu.sourceforge.io/), the Versatile Commodore
Emulator** — one C file → one Rust file, one C function → one Rust function,
same names, same structure (see `crates/trx64-core/**` and the Spec-612 port
doctrine). In particular:

- the cycle-exact **VIC-II** is a port of VICE's `viciisc/` (single-cycle) core,
- the **1541 drive** ports `vice1541/**` (`viacore`, `drivecpu`, `rotation`/GCR, …),
- the **6510 CPU** ports the verbatim VICE `mainc64cpu.c` core (BA stealing, the
  microcoded execution path),
- the **CIA / VIA / IEC** behavior follows VICE's chips,
- the **`.c64retrace`** trace format and the parity model mirror the VICE-shaped design.

VICE is licensed under the GNU General Public License version 2 **or later**.
TRX64 uses the "or later" permission and is distributed under
**GPL-3.0-or-later** (see [LICENSE](LICENSE)). Deep gratitude to the VICE team
and its contributors — this project would not exist without their decades of
meticulous, documented work.

## C64ReverseEngineeringMCP (C64RE)

TRX64 is the runtime **behind** [C64RE](https://github.com/Jondalar/C64ReverseEngineeringMCP):
it speaks the same WebSocket JSON-RPC protocol and writes the same trace format,
so the C64RE workbench, its UI, and its 50+ MCP tools talk to TRX64 unchanged.
The C64RE TS runtime also served as the golden oracle for TRX64's behavioral
parity (the differential conformance gate). Thanks to the C64RE project for the
contract, the test corpus, and the shared vision.

## vicspector, Linus Åkesson, Christian Bauer — the VIC-II, cycle by cycle

The per-cycle record behind `vic/line_trace` and `vic/frame_map` (Specs 859/860) is drawn in
C64RE in the layout of [vicspector](https://github.com/elysium64/vicspector) by elysium64 (MIT),
an interactive map of one VIC-II raster line. The raster techniques the frame map names — FLI,
FLD, linecrunch, DMA delay (VSP), open borders, sprite crunch and stretch — are rules over what
the chip did, named after the literature vicspector builds on: Linus Åkesson's
[VIC-II timing chart](https://www.linusakesson.net/programming/vic-timing/) and
[MISC notes](https://www.linusakesson.net/scene/lunatico/misc.php), and Christian Bauer's
[VIC-II article](https://ist.uwaterloo.ca/~schepers/MJK/ascii/VIC-Article.txt). Thank you.

## The C64 scene & primary sources

Thanks to the Commodore 64 community — crackers, demosceners, and documenters —
and to the primary-source archives (chip datasheets, the PLA dissection, KERNAL
disassemblies, schematics on zimmers.net) that make cycle-accurate emulation
possible. Scene-authentic software was the test corpus that kept TRX64 honest.

## ROMs & third-party media

Commodore ROM images, commercial disks, cartridges, and other copyrighted media
are **not** part of this project's license. Provide them locally through your own
legally obtained copies.
