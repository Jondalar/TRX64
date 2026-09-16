# Spec 854 — The expansion RAM the host owns

**Status:** BUILT 2026-09-16.
**Repos:** TRX64 only.
**Number:** 854 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** Spec 853 (the REU and GeoRAM as port devices). This is additive — a second
storage mode, not a correction.
**Origin:** UE2's integration of 853, 2026-09-16. Its acceptance was green on
`spec-853-reu-georam` (391 tests, `uci_targets` 46 checks, cartridge smoke 27/27), and it
came back with the one thing 853 §5 had deliberately left to the bridge.

---

## §1 Why an REU that owns its RAM is wrong in a host

On the U64 the REU's RAM is not the REU's. It is the firmware's DDR:
`REU_MEMORY_BASE 0x1000000`, `REU_MAX_SIZE 0x1000000` (`c64.h:14-15`). The firmware
**preloads an image by writing it there with its own CPU** (`filetype_reu.cc:117,127`),
and the menu's settings are `C64_REU_ENABLE` (cart regs `+0x8`) and `C64_REU_SIZE`
(`+0x9`, `c64.h:62-63`). GeoRAM is the same setting at `TYPE 0x1F` over the same region.

So if TRX64 allocates sixteen megabytes of its own, there are two copies: the one the
firmware fills and the one the C64 reads. Every preload lands in the wrong one. That is a
correctness problem, not a preference, and it cannot be papered over by copying on attach
— the firmware writes that region at runtime, whenever it likes.

**This does not make 853 wrong.** An REU that owns its RAM is correct for standalone
TRX64, and owning it is what makes a `.c64re` dump round-trip at all. What is missing is
the other mode: a device whose store belongs to whoever embeds the core.

The pattern already exists one device along. The bridge lends the cartridge its DDR
(`C64Backend::lend_ddr` hands `CartProxy` a `&mut [u8]` for the duration of an access and
takes it back). 854 gives the REU and GeoRAM the same shape.

## §2 Where the seam already is

853 put every REU access to its own RAM behind exactly two functions — `store_to_reu` and
`read_from_reu` (`reu.rs:263`, `:271`), both already gated on `not_backedup_addresses`,
which is how a 256 KiB REU addressed by a 512 KiB chip returns the bus latch for the half
that has no DRAM. A store that is not there behaves the same way as DRAM that is not
there, so the "nothing lent" case is not a new concept — it is the existing one.

GeoRAM is different and decides the design: it reads its RAM on **every** C64 access to
`$DE00-$DEFF`, not only during a transfer. A store handed into `run_dma` would therefore
serve the REU and not GeoRAM. The store has to be reachable from
`ExpansionDevice::read`/`write`/`peek`, which is why it is an object the device holds
rather than a borrow threaded through a call.

## §3 Design

**D1 — the store is a trait, and owning one is the default.**

```rust
pub trait ExpansionRam: Send {
    fn len(&self) -> u32;
    fn read(&self, off: u32) -> u8;
    fn write(&mut self, off: u32, value: u8);
}
```

A `Vec<u8>` implementation ships with it and is what `attach_reu(size_kb)` installs, so a
standalone machine is bit-identical to 853 and every existing caller keeps working. A host
implements the trait over its own memory. Rust lifetimes are the reason this is a trait
object and not a stored `&mut [u8]`: a `'static` device cannot hold a borrow, and
inventing a lifetime parameter on `Machine` to carry one would reach every call site in
the crate for the sake of one device.

**D2 — attaching without allocating.** `Machine::attach_reu_borrowed(size_kb, store)` and
`attach_georam_borrowed(size_kb, store)`, plus `set_expansion_ram(Option<Box<dyn
ExpansionRam>>)` so a host can hand the store over, take it back, and hand a different one
in without rebuilding the device.

**D3 — nothing lent is not a panic.** With no store, a read returns the floating-bus latch
and a write is dropped — the behaviour 853 already has for an address with no DRAM behind
it (`reu.c:1110-1152`). The bridge lends only around accesses that can reach the C64 bus,
so "not lent right now" is the normal state between them, not an error.

**D4 — size and enable move at runtime.** `set_size_kb()` recomputes `RecOptions` and the
backed-up boundary and touches no storage, so the firmware toggling `C64_REU_SIZE` neither
rebuilds the device nor drops what is in the store. Detaching is the host clearing the
device, not the RAM.

**D5 — GeoRAM over the same region falls out.** Once both devices read through the store,
the U64's "REU and GeoRAM are one setting sharing one region" is expressed by lending both
the same store. Nothing device-specific is needed for it.

**D6 — the accessors change shape, and that is the cost.** `Reu::ram()` returning `&[u8]`
cannot exist for a store behind a trait object. It becomes `ram_len()`,
`ram_byte(off)`, `set_ram_byte(off, v)` and `ram_slice(off, len) -> Vec<u8>`. The call
sites are `Machine::expansion_ram_slice`, `load_expansion_image`, the snapshot's
`expansion_node` and its restore, and the 853 gate's fixtures. This is the one place 854
touches 853's surface, and it is mechanical.

**D7 — a borrowed store is in no snapshot.** `expansion_node` writes `ram: null` for it by
construction: the bytes belong to the host's memory image, which has its own persistence.
That is already the shape 853 built for a ring entry, and `expansion_ram_uncovered()`
already says so on restore. Nothing new — it simply becomes the normal case for a host.

## §4 Gates

- **A standalone machine is unchanged**: the whole 853 gate passes with `attach_reu`
  installing the owned store, and `perf_bench` stays in the band.
- **A borrowed store round-trips a transfer**: stash into a host-provided store, read the
  host's own buffer back, fetch it out again.
- **Nothing lent**: a transfer against no store completes, reads the floating-bus value,
  writes nothing, and does not panic. The registers end where a transfer says they end.
- **The store can be swapped** between runs without touching the registers, and what the
  new store holds is what the next transfer sees.
- **Size changes at runtime** without dropping the store: shrink to 128 KiB, the
  not-backed-up boundary moves, the bytes below it are still there.
- **GeoRAM and REU over one store** see the same bytes.
- **Snapshot**: a dump of a machine with a borrowed store writes `ram: null`, and a restore
  of it sets `expansion_ram_uncovered()` rather than zeroing the host's memory.

## §5 UE2

The bridge maps rather than lends a copy: it forwards `C64_REU_ENABLE` and
`C64_REU_SIZE` (0-7 → 128 KiB … 16 MB) and hands TRX64 a store over
`REU_MEMORY_BASE 0x1000000`. GeoRAM at `TYPE 0x1F` lends the same store.

## §6 Not in this spec

- **Changing what a standalone TRX64 does.** The owned store stays the default, and
  `--reu 512` keeps meaning what it means today.
- **The battery-backed GeoRAM** (853 §6) — still out.
- **Writing a borrowed store to disk.** It is the host's memory; the host persists it.

## §7 As built

**Where it lives.** `ExpansionRam` + `OwnedRam` in `expansion.rs`; the store itself in
`Reu` and `GeoRam` as `Option<Box<dyn ExpansionRam>>`. On `Machine`:
`attach_reu_borrowed`, `attach_georam_borrowed`, `set_expansion_ram`. Gate: the 854 cases
in `tests/reu_gate.rs` (38 total with 853's).

**What the build settled against §3.**

- **The seam was already there.** 853 had put every REU access to its own RAM behind
  `store_to_reu` and `read_from_reu`, so swapping the storage touched two functions and
  nothing else inside the device. That was luck turned into design by 853, not by this
  spec, and it is why 854 is small.
- **D6 cost 38 call sites**, 10 in production code and 28 in 853's gate fixtures. A regex
  pass did them and the compiler found exactly the two it could not: `usize` loop indices
  handed to a `u32` offset. Nothing subtle survived, which is what a mechanical change
  should look like.
- **A bulk `write_ram` was added** after the first cut wrote images a byte at a time
  through a trait object. Once at startup that is fine; in the snapshot restore it is not.
- **`is_owned()` lives on the STORE, not on the device.** The snapshot asks the bytes who
  they belong to rather than the device what mode it is in, so a host that lends an owned
  store gets the honest answer without the device tracking a second flag.
- **No daemon change, and that is deliberate.** `--reu 512` still means an owned store;
  the borrowed mode is an embedding API for a host that runs the core in-process, and a
  flag for it would describe a machine the daemon cannot be.
- **D3 needed no new concept.** "Nothing lent" reads the floating-bus latch and drops
  writes, which is exactly what 853 already did for an address with no DRAM behind it. The
  gate asserts a transfer against no store still COMPLETES — end-of-block set, registers
  where a finished transfer leaves them.

**Measured.** `reu_gate` 38/38, full gate green (7 suites / 105 tests, daemon 382, seven
games 7/7), core lib 321. The decisive case is `a_transfer_lands_in_the_hosts_own_memory`:
the host reads the very bytes the device wrote, and a host write is what the C64 fetches
back — the two-copies problem UE2 found, demonstrated gone.
