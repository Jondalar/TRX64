# Spec 840 — An empty expansion port is not RAM

**Status:** **BUILT 2026-09-10**
**Repo:** TRX64 (`crates/trx64-core`)
**Origin:** issue #19 asked for REU emulation because a Level-Squeezer "+E" cruncher
JAMs at `$1D24` on `CMP $DF00,y`. Reading VICE to scope that request surfaced a
defect that has nothing to do with the REU and is older than the request.

## 1. The defect

`FullBus::io_read` resolved `$DE00-$DFFF` like this: ask the cartridge, and if no
cartridge answers, return `self.io[addr - 0xd000]`.

That array is the **write-through I/O shadow**. `io_write` stores into it in its
very first line, before any dispatch, for the whole `$D000-$DFFF` window:

```rust
fn io_write(&mut self, addr: u16, value: u8) {
    // Keep the open-bus shadow for unclaimed-register reads.
    self.io[(addr as usize) - 0xd000] = value;
```

So on a machine with an **empty expansion port**, `$DE00-$DFFF` behaved like RAM:
write `$55`, read `$55` back, indefinitely. The shadow starts as `[0u8; 0x1000]`,
so an untouched `$DF00` read `$00` — a stable, repeatable value.

A real C64 with nothing in the port has the **open bus** there: whatever the VIC
last fetched on phi1, which changes with the raster position and is video-matrix
data, character data, sprite data or the `$3FFF` idle fetch depending on where in
the frame the read lands.

**The consequence is not cosmetic.** Hardware detection is conventionally written
as "write a pattern, read it back, compare". Against a write-through shadow that
probe **passes on a machine with nothing plugged in**. Every program that looks for
a cartridge, a freezer, a GeoRAM or an REU was told the device is present, and then
drove a device that was not there.

## 2. The fix

Return the open bus, ported from VICE's cycle-accurate VIC, where it is one line
(`viciisc/vicii-phi1.c:34`):

```c
uint8_t vicii_read_phi1(void)
{
    return vicii.last_read_phi1;
}
```

and is what `c64io.c:353-354` returns when no I/O device claims a read.

TRX64 already maintained that field — `vic.rs:1586-1607` stores `last_read_phi1` on
every phi1 fetch, matching `vicii-fetch.c` — it simply was not consulted here.

**Four sites, not one.** The CPU path was the defect; the other three were the same
defect seen from the debugger, which is the more insidious half:

| site | what it is |
|---|---|
| `full.rs` `io_read` | the CPU's own read |
| `lib.rs` `read_full` | the side-effect-free peek |
| `lib.rs` `peek_lens` — `io` lens | monitor `m io` |
| `lib.rs` `peek_lens` — `cart` lens | monitor `m cart` |

The three peek paths showing the shadow is the BUG-049 lesson repeating: there, a
`$DF00` read went through `cia1.peek`, the bare latch, "so it showed the last value
the KERNAL wrote and never a joystick or a key" — the readback that was supposed to
settle an argument hid the evidence instead. A monitor that disagrees with the CPU
about an address is worse than no monitor.

The shadow is still **written** — one unconditional line covering the whole I/O
window, and `$D800-$DBFF` colour RAM genuinely reads from it. It is simply no
longer what `$DE00-$DFFF` reads.

## 3. What this means for issue #19

Possibly that the REU is not the fix, and certainly that the diagnosis in the issue
is inverted. #19 says:

> With no REU attached, `$DF00` is open bus, the probe result is garbage, and
> execution runs off into an illegal opcode.

In TRX64 it was not garbage — it was a **perfect echo**. A probe that wrote and
read back was told the REU is present, and the cruncher then issued transfers to
nothing. Whether that binary now takes a clean no-REU path, or still crashes
because the "+E" build simply has no fallback (the toolset ships a separate non-E
build for machines without a 1764, which is what a user would have run), is a
measurement, and it belongs to the reporter with his binaries.

**The REU work is therefore unblocked from this defect, not settled by it.** If the
"+E" cruncher still needs a real REU, the reading done for #19 says what that costs:
the register file with its read side effects, the four transfer loops, and
`clk += n` per byte — and *not* the BA arbitration, which only cycle-counted demo
code needs. That is Spec 841 if it is wanted.

## 4. Gate

`an_empty_expansion_port_reads_the_open_bus_not_what_was_written`
(`crates/trx64-core/src/lib.rs`). It writes **two different patterns** on purpose —
a single one can match the bus by luck — asserts the readback is not the byte just
written, asserts it is the phi1 value, and asserts the monitor and the CPU agree.
Verified to fail without the fix: *"peek and CPU disagree at $de00"*.

Suites: `trx64-core` 12 binaries green.

## 5. Not in scope

- REU emulation itself (Spec 841, if wanted).
- The I/O2 occupancy rule — a REU and a cartridge both want `$DF00`, and a real C64
  has one expansion port, so the answer is a refusal at attach time rather than
  VICE's read-time collision policy. Recorded here so it is not re-derived: VICE's
  policy exists, is well documented, and **never fires on the C64**, because
  `io_source_valid` in `c64io.c` is a 0/1 flag that the verdict at line 357 tests as
  if it were a count. `vic20io.c` has the same function written correctly with a
  real counter; `plus4io.c`, `cbm2io.c` and `petio.c` carry the C64's version of the
  bug. Port the intent, not the behaviour.
