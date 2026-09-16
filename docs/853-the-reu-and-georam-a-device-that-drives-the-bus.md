# Spec 853 — The REU and GeoRAM: a device that drives the bus

**Status:** PROPOSED 2026-09-16.
**Repos:** TRX64 only. C64RE gains nothing: this is a machine fact.
**Number:** 853 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** Spec 850 (the port as a device interface) for the device place, the
snoop, the interrupt source and the hold; Spec 840 (an empty port is not RAM) for what
an unclaimed read returns. Spec 852 is a sibling, not a dependency.
**Origin:** issue #19 (C64RE), closed 2026-09-13 — a REU-cruncher jammed on the `$DF00`
presence probe. That defect was **not** a missing REU: an empty port echoed writes, so
the probe found a device that was not there, and 840 fixed it. What remains is the
feature the issue asked for, and it is a want, not a bug.
**Owner decisions:** 2026-09-16 — VICE is the reference; the REU is a **core** device, on
`c64` as well as `u64`; its RAM is **out of the checkpoint ring** and **in the `.c64re`
dump**; nothing is written back to the filesystem; **GeoRAM is in scope**.

---

## §1 Why this is not "another cartridge"

Everything TRX64 has ever put on the expansion port answers the bus. The REU **drives**
it. A transfer is the REC (the 8726 RAM Expansion Controller) holding the 6510 and then
reading and writing C64 memory itself, byte after byte, at its own pace, while the VIC
keeps stealing cycles underneath.

Spec 850 built the hold. It did not build the bus:

> "A DMA engine — a device reading and writing the C64 bus while the CPU is held, as an
> REU does. The hold here runs the chips; it gives the device no bus. That belongs to the
> REU spec, which ports VICE's `reu.c`." — 850 §6

That sentence is this spec. Everything else here — the registers, the `$FF00` trigger,
GeoRAM — is small beside it.

GeoRAM is in scope because it is the same shelf and the opposite cost. It has **no DMA
at all**: a 256-byte window and two write-only registers, and the CPU moves every byte
itself. `georam.c` is 723 lines against `reu.c`'s 1688. Once the port carries a RAM
device, leaving GeoRAM out would be a choice to do the cheap half later for no reason.

## §2 The hardware, ported not invented

VICE is the reference, exactly (owner, 2026-09-15). Where VICE and the Ultimate's VHDL
differ — mirroring below 512 KB, `$DF20-$DFFF`, the end state after a verify error —
**VICE wins**, because it is the implementation that has been measured against real
hardware for twenty years and the VHDL is a closed core we may not read into.

### The REU register file (`reu.c:129-141`)

| Offset | Register |
|---|---|
| `$00` | status (read) |
| `$01` | command |
| `$02`/`$03` | C64 base address, low/high |
| `$04`/`$05` | REU RAM address, low/high |
| `$06` | REU RAM bank |
| `$07`/`$08` | transfer length, low/high |
| `$09` | interrupt mask |
| `$0A` | address control |
| `$0B-$1F` | unused |

Window `$DF00-$DFFF`: registers at `$DF00-$DF1F`, **mirrors at `$DF20-$DFFF`**
(`reu_io2_device`, `reu.c:270-289`). The device decides read validity per access; it does
not claim the range unconditionally.

**Status bits** (`:146-150`) — chip version in `$0F`; `$10` set means 256K DRAMs (1764,
1750) and clear means 64K (1700); `$20` verify error; `$40` end of block; `$80` interrupt
pending. **The last three clear on read**, which makes the status register a
side-effecting read and therefore a `peek`/`read` split on our side (850 D2).

**Command bits** (`:155-163`) — transfer type in `$03`: `00` C64→REU (stash), `01`
REU→C64 (fetch), `02` swap, `03` verify. `$10` disables the `$FF00` trigger, `$20`
autoload, `$80` execute. `$4C` is writeable and unused.

**Reset** (`:602-615`) — registers zeroed, then: command = `$FF00`-trigger **disabled**,
transfer length = `$FFFF`, bank = the unused mask, interrupt mask = its unused mask. A
reset REU does not trigger on `$FF00`; software enables that deliberately.

### GeoRAM (`georam.c:142-173`)

Two devices, not one. IO1 `$DE00-$DEFF` is the **window** and its read is always valid.
IO2 `$DF80-$DFFF` holds the registers at `$DFFE-$DFFF` with mirrors at `$DF80-$DFFD`, and
is **write-only** — "read is never valid, regs are write only". Two registers: window and
bank. The bank wraps by repeated subtraction against the fitted size (`:215-216`), which
is the behaviour to port rather than a mask.

VICE's own summary (`:51`): "The GeoRAM is a banked memory system." There is no DMA in
that file.

### Where VICE hangs the DMA onto the CPU

`reu_ba_register(ba_check, ba_steal, ba_var, ba_mask)` (`reu.c:590-599`) lets the machine
install two callbacks and a BA variable. Then, per transferred byte:

- `nonsc_reu_clk_inc_pre` (`:816`) — the non-cycle-exact core: `maincpu_clk++`, nothing
  more.
- `reu_clk_inc_post_write` / `reu_clk_inc_post_read` (`:824`, `:842`) — the cycle-exact
  core: `maincpu_clk++`, then **keep stealing while the VIC holds BA low**.

TRX64 has only the cycle-exact shape, so only the second pair is ported.

## §3 Design

**D1 — the port becomes a list.** 850 gave `Machine` two device places: `port_profile`
(the profile's own, `lib.rs:549`) and `expansion` (a host's, `:546`), read in the order
profile → host → cartridge → open bus (`full.rs:670-687`). A `u64` running UCI *and* an
REU *under UE2* is three devices in two places.

VICE already answers this: its REU and GeoRAM sit in the "IO Slot", where
"any number of 'IO Slot' carts can be, in theory, active at a time" (`c64cart.c:180-210`),
registered into a list (`io_source_register`). Both claim no `game`/`exrom`
(`export_res_reu`, `reu.c:289`) and map only into IO1/IO2, which is *why* they compose.

TRX64 does not need the core opened for this: `ExpansionDevice` is a trait, so a
composite device that holds several and fans out satisfies the same contract. The read
order stays defined and the first non-`None` answer wins, as today.

**D2 — the REU device.** The register file of §2 on 850's port, with `read` and `peek`
split because the status register clears bits on read. Size is a construction parameter
(the 1700/1764/1750 set); the status bit follows the DRAM type, not the size alone.
Mirrors at `$DF20-$DFFF` are real and are ported.

**D3 — the DMA engine, the actual work.** A device gets a way to read and write C64
memory while the CPU is held, and to spend cycles doing it. This is new: 850's hold runs
the chips and hands the device nothing.

- Entry: the `EXECUTE` bit, or the `$FF00` trigger (D4).
- Per byte: one cycle, then keep stealing while the VIC has BA low — a port of
  `reu_clk_inc_post_read`/`_post_write`, not an approximation of them. TRX64's side of
  that already exists: `check_ba(&mut self, &mut u32, bool) -> u64` returns the cycles
  stolen (`c64_6510core.rs:505`), and `full_sc.rs:366-372` records how much of a stall had
  the address on the bus from `vic.last_steal_on_bus`.
- The four transfer types are one loop with a direction and a comparison, as in VICE
  (`reu_dma_host_to_reu`, `_reu_to_host`, `_swap`, and verify).
- Address control (`$0A`) fixes either side's address; autoload restores the shadow
  registers at the end; the end sets END_OF_BLOCK and may raise the interrupt.
- The transfer is **not** an instruction-boundary event. It runs where it was triggered.

**D4 — the `$FF00` trigger, and why the dummy write matters.** VICE does not hook this in
the cartridge layer at all: `mainc64cpu.c` calls `reu_dma(-1)` from inside the CPU's store
macros (`:290-303` and `:373-386`, once per core variant) behind a `reu_dma_triggered`
latch, plus `c64mem.c:533`.

TRX64 has the snoop instead (850 D5, `$FF00` registered whatever the banking) — and 850's
own acceptance says `INC $FF00` reports **two** writes, the old value first. So the device
sees both cycles of a read-modify-write and must trigger on the first and stay latched
until the instruction ends. That latch is `reu_dma_triggered`, ported, not invented: this
is the one place where our snoop is *more* faithful than VICE's hook and would fire twice
without it.

**D5 — GeoRAM.** Two windows as in §2, the window/bank pair, the subtractive wrap. No
DMA, no interrupt, no hold. It shares D1's list and D6's storage rule and nothing else.

**D6 — the RAM is in the snapshot and out of the ring.** These are two different things
and the code says so itself:

> "TRANSIENT: in-memory only. NOT persistence (Spec 707 `.c64re` dump does that)."
> — `checkpoint_ring.rs`

The ring's budget is 32 MiB at 64 KiB per slot, about 512 entries
(`checkpoint_ring.rs:55`). A 16 MB REU in every entry does not shrink that ring, it
destroys it. So the REU/GeoRAM RAM is **excluded from the ring** and **included in the
`.c64re` dump**, where it is paid once and where VICE also carries it
(`reu_write_snapshot_module` stores size, the 32 register bytes read without side effects,
and the whole RAM — `reu.c:1591-1618`).

Both are built from one function, so this is an option on the capture, not a second
serializer — and that pattern already exists: Spec 807 added `omit_framebuffer` to
`capture_runtime_checkpoint_opts` and its doc comment already draws exactly this line
("Callers that PERSIST … pass `false`… the two per-frame producers … pass `true`"). The
signature is at eight parameters with `too_many_arguments` already suppressed, so the
second flag arrives as an options struct rather than a ninth column.

**D7 — what a rewind then means.** Excluding the RAM makes a rewind restore the C64 but
not the expansion RAM: a program using the REU lands with its CPU at cycle X and its REU
at cycle Y. That is the shape of bug 792, where a restore brought back half a state and
nobody was told.

So a rewind must not be silently half. The ring entry records that expansion RAM is not
covered, and a restore onto a machine with an REU or GeoRAM attached **says so** — in the
daemon's reply, in the monitor, and in the lens. The alternative, refusing the rewind
outright, is worse for the workbench: most of what a human rewinds through does not touch
the REU at all, and a debugger that refuses is a debugger that gets worked around.

**D8 — interrupts.** The REU interrupt goes on `INT_SRC_EXPANSION`, which 850 built as one
source carrying IRQ and NMI independently, sampled per cycle. Nothing new is needed.

**D9 — attach and persistence.** No write-back. VICE's image file is optional and off by
default (`REUfilename`, `REUImageWrite`, default `0`), and the Ultimate only ever *pre*
loads (`reu_preloader.cc`). TRX64 attaches with a size, and may **load** a `.reu` image;
it never writes one back on detach.

The one exception to state rather than discover: the **battery-backed GeoRAM (BBG)** is a
real variant where retaining content *is* the hardware's behaviour — VICE even skips RAM
initialisation when a file supplies the content (`georam.c:253`). That is a persistence
case sitting inside a device we otherwise treat as volatile, and it is deliberately **not**
built here.

**D10 — profiles.** The REU is a core device (owner, 2026-09-16): attachable on `c64` and
on `u64`, not installed by `set_speed_profile` the way 852's UCI block is. That matters
because the profile slot is rewritten on every profile change, so a core device must not
live in it.

On the U64 the two are not two devices but **one setting**: `REU_ENABLE`/`REU_SIZE`, a
1764-style REU at `$DF00`, memory at DDR `0x01000000`, and **GeoRAM when `TYPE=0x1F`** —
the same memory region (`u64-emulator/docs/hw/10-c64-machine.md:332`,
`00-memory-map.md:70`). TRX64 mirrors that: on `u64`, REU and GeoRAM are alternatives, not
neighbours.

**D11 — visibility.** A monitor verb reporting size, the registers decoded, the current
transfer and whether one is running; a lens over the expansion RAM. Without it a stalled
transfer is a black box, which is the complaint 852 D6 answered for UCI.

## §4 Gates

Driven through the CPU bus, never by poking the device (815's lesson):

- **Detection.** With no REU attached, `$DF00-$DFFF` reads the open bus (840). With one
  attached, the documented probe finds it, and the mirrors at `$DF20-$DFFF` answer too.
- **Stash / fetch.** Write a pattern to C64 RAM, stash it, clear the RAM, fetch it back:
  byte-identical. Length, base and bank land where the registers say.
- **Swap** exchanges both sides. **Verify** reports equality, and on a mismatch sets the
  verify error bit with VICE's end state — the register values after a failed verify are
  part of the assertion, not an afterthought.
- **Status clears on read**: verify error, end of block and interrupt pending are gone on
  the second read.
- **Autoload** restores the shadow registers; without it they advance.
- **The `$FF00` trigger**: armed, `STA $FF00` starts one transfer; `INC $FF00` also starts
  exactly **one**, not two. Disabled by the command bit, neither does.
- **Cycle cost**: a transfer of N bytes advances `clk` by the VICE-equivalent count, and a
  transfer crossing a badline steals more — asserted against the badline, not just a total.
- **The CPU is unchanged** across a transfer: PC, registers and flags are where they were.
- **Interrupt**: an enabled end-of-block interrupt is taken with the same delay as a CIA's.
- **GeoRAM**: the window reads and writes; the two registers are write-only; the bank wraps
  by subtraction at the fitted size; no cycle is stolen by any of it.
- **Ring and dump**: a ring entry does not grow by the REU's size and declares expansion RAM
  uncovered; a `.c64re` dump round-trips the RAM byte-exact; a restore reports the gap (D7).
- **Stock machine pays nothing**: the seven-game gate and `perf_bench` unchanged with no
  device attached. 850's 6 % lesson applies — this is measured, not asserted.

## §5 UE2

UE2 forwards its REU setting: enable, size, and `TYPE=0x1F` for GeoRAM. The RAM itself
lives in the firmware's DDR at `0x01000000`, so the bridge decides whether TRX64 owns the
storage or maps the host's. That choice is the bridge's and is not settled here.

## §6 Not in this spec

- **RAMLink, DQBB, RamCart, Expert** and the rest of VICE's RAM-expansion family. The list
  from D1 makes them cheap later; cheap is not a reason to build them now.
- **The battery-backed GeoRAM (BBG)** and its file-backed content (D9).
- **Writing an REU image back to disk** (owner, 2026-09-16).
- **Expansion RAM in the checkpoint ring** (owner, 2026-09-16), and therefore full rewind
  fidelity for REU software.
