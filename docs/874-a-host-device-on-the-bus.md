# Spec 874 — A device of the host's own on the bus: a public IEC device trait

**Status:** PROPOSED 2026-09-23
**Repos:** TRX64. C64RE: no change.
**Number:** 874 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`, row present).
**Depends on:** Spec 873 (the folder device, the `IECDEVICE` slot, conf3, the sync points),
Spec 871 (two drive positions, units on the wire, the refusal of two devices at one unit),
Spec 870 (power, reset line), Spec 850 D7 (`Hold::Cpu` / `Hold::Reset`), Spec 863 (the
model's `cpu_hz`).
**Enables:** UE2 — an emulator that runs the unmodified Ultimate 64 firmware over
`trx64-core` as a library — puts the U64's FPGA IEC processor
(`fpga/io/iec_interface/vhdl_source/iec_processor.vhd`, microcode
`software/io/iec/iec_code.iec`) on TRX64's bus, so the firmware's own Software IEC DOS
answers the C64 at line level beside TRX64's 1541s.
**Origin:** UE2 (the C64U_emu session), 2026-09-23. UE2 has the processor as a pure state
machine (`crates/c64-bridge/src/iec_proc.rs` in their repo), stepped in µs, mapping C64
cycles through the model's `cpu_hz`, so PAL and NTSC both work. It needs no checkpoints. The
shape below was agreed with UE2 in our answer; this spec refines it against the code.

---

## §1 What exists today (main after 873)

Re-checked against the code:

- **The bus has one kind of non-drive device, and it is wired in by name.** `Machine::folders:
  Vec<FolderDevice>` (`lib.rs:557-560`) is the only thing that stands in an
  `IECBUS_DEVICE_IECDEVICE` slot. The IEC core knows it as `IecCore::folder_units: u16`
  (`iec.rs:267-270`), set by `set_folder_units` (`iec.rs:920-944`) and honoured by
  `adopt_drive_slots` (`iec.rs:953-978`), which marks those slots `IECDEVICE` and so selects
  conf3 through `calculate_callback_index` (`iec.rs:685-707`).
- **Three sync points, each a `folders.is_empty()` test then `folders_sync`**
  (`folder_device.rs:1753-1767`):
  - end of every instruction: `Machine::catch_up_drives` (`lib.rs:2856-2870`), called from
    the run loop at `lib.rs:3617` and at the end of a `Hold::Cpu` span (`run_held`,
    `lib.rs:2158`);
  - `$DD00` read: `iec_push_flush_to` (`full.rs:397-413`), after both drives are caught up
    and folded, before `iecbus_callback_read` (`full.rs:546-549`);
  - `$DD00`/`$DD02` write that changes the port: after the conf write and the drives' ATN
    edges, at the write instant `clk + 1` (`full.rs:709-732`).
- **What a sync does.** For each device in unit order: the lines as everybody else drives
  them (`cpu_bus` ANDed with every other slot 4-11), ATN from `cpu_bus`, `advance(t, others,
  atn_low)` (`folder_device.rs:1400-1414`: every timed transition due before `t` under the
  lines it saw last, settled at its due time, then the new lines at `t`), its pull into
  `drv_bus[unit]`; one `iec_update_ports` at the end.
- **ATN edges go to drives only.** `iecbus_cpu_write_conf3` returns an edge per
  `TRUEDRIVE` slot (`iec.rs:598-627`), delivered by `pair_deliver_atn_edge`
  (`drive.rs:2064-2096`). The folder has no edge call; it sees the ATN level at its next
  `advance` — which on a write is the same cycle.
- **`Hold::Reset` clocks nobody on the bus.** `run_held` with `Reset` runs only the VIC and
  moves the drives' reference along (`lib.rs:2155-2160`); the folder is not advanced until
  the next sync after the hold.
- **Slots 4-7 are folded but never used.** `iec_update_ports` ANDs `drv_bus[4..12]`
  (`iec.rs:371-387`) and `calculate_callback_index` includes slots 4-7 (`iec.rs:686-693`),
  but nothing in TRX64 writes them. The drives' local re-fold after a `$1800` store also ANDs
  4-11 (`viacore.rs:2455`, `:2497`; `drive1581.rs:83`), but each drive copies in only slots
  8-11 from the IEC core before it runs (`drive.rs:1376-1380`, `drive1581.rs:443-447`) — a
  device at slot 4-7 would be invisible to a drive after the drive's first `$1800` store in a
  slice.
- **The folder is named in every consumer.** Reset (`lib.rs:1291-1302`), model switch
  (`lib.rs:969-972`), attach/detach/reattach and the drive-unit refusal (`lib.rs:2872-2962`,
  `:3009-3015`), the checkpoint node and its restore (`c64re_snapshot.rs:1538-1544`,
  `:1552-1573`, `:1889-1900`), the `FullBus` borrow (`full.rs:214`, and the test constructor
  `full.rs:1260`), the session's power cycle (`trx64-session/src/lib.rs:79`, `:231-235`,
  `:273`), the monitor (`trx64-monitor/src/verbs.rs:2043-2047`, `:2105-2115`) and the daemon
  (`trx64-daemon/src/main.rs:7099-7101`, `:7566-7601`).
- **A precedent for host devices exists on the expansion port.** `ExpansionDevice: AsAny +
  Send` (`expansion.rs:93-153`) with `clone_device() -> None` by default ("a host's device
  belongs to the host"), `PortSlot`'s `Clone` built on it (`expansion.rs:302-308`), downcast
  through `AsAny` (`expansion.rs:79-91`), and a machine that says when a restore did not cover
  a device (`expansion_ram_uncovered`, `lib.rs:656`, `:1744-1750`; `c64re_snapshot.rs:1966-2021`).

So the bus already has everything a host device needs except a door: the slot, the fold,
conf3 and the sync points are there, and they only know `FolderDevice`.

## §2 What the references do

**VICE.** One kind of line-level non-drive device, `serial/serial-iec-device.c`, enabled per
unit through `iecbus_status_set(IECBUS_STATUS_IECDEVICE, …)`; conf3's read and write run
`drive_cpu_execute_all(clock)` then `serial_iec_device_exec(clock)` (`iecbus/iecbus.c:353-370`).
There is no plug-in interface: the device is a compiled-in module. The device map
(`iecbus.c:493-510`, TRX64 `iec.rs:199-217`) is the only generic part.

**The Ultimate's IEC processor** (`iec_processor.vhd`). One open-collector driver per line —
`clk_o`, `data_o`, `atn_o`, `srq_o` from `a_drivers` (`:100-103`) — and the wire as input
(`inputs_raw <= srq_i & atn_i & data_i & clk_i`, `:105`): it reads the bus including its own
pull. Instructions run at the system clock; only the timer counts at the 1 MHz `tick`
(`:139-145`). A falling ATN with interrupts enabled flushes the down FIFO and jumps to
microcode address 1 (`:246-251`): ATN is a **vector**, not a polled level. It has **no unit
number in hardware** — the device numbers live in the microcode's compare instructions,
patched by the firmware (873 §2, `iec_interface.cc:83-145`), up to four slave addresses, and
the firmware can change them while the machine runs.

What this spec takes from them: VICE's slot and conf3 (already in TRX64), and from the
Ultimate the facts a trait must allow — a device that reads the wire and pulls open
collector, wants ATN as an edge, keeps its own time, and may answer to several unit numbers
or none that TRX64 can know.

## §3 D1 — The trait

In `crates/trx64-core/src/iec_device.rs` (new), re-exported from the crate root:

```rust
/// The lines as the REST of the bus drives them — the C64 and every other device,
/// not this one. `true` = high (released). ATN is only ever driven by the C64.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IecLines { pub atn: bool, pub clk: bool, pub data: bool }

impl IecLines {
    /// The wire as the device reads it: the rest ANDed with its own pulls
    /// (the Ultimate's `inputs_raw`).
    pub fn with(self, own: IecOut) -> IecLines;
}

/// What the device pulls low, open collector. `false` = released.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IecOut { pub clk: bool, pub data: bool }

pub trait IecDevice: AsAny + Send {
    /// Shown in refusals, the monitor's `iec` column and a checkpoint's list.
    fn name(&self) -> String;

    /// Catch up to exactly C64 cycle `clk`. Until `clk` the lines were the ones the
    /// previous call gave; from `clk` on they are `bus`. `clk` never decreases between
    /// two calls except across `rebase`; the same `clk` may come twice (a `$DD00` read
    /// and the end of the same instruction).
    fn clock_to(&mut self, clk: u64, bus: IecLines);

    /// Its pulls now. Read right after every `clock_to`; folded into the wired-AND.
    fn outputs(&self) -> IecOut;

    /// The machine's clock is `clk` and the lines are `bus`, without time having
    /// passed for the device: at attach, after a restore that did not carry its
    /// state, after a C64 power cycle it lived through.
    fn rebase(&mut self, clk: u64, bus: IecLines);

    /// ATN changed to `level` at cycle `clk` (a `$DD00`/`$DD02` write). Always
    /// followed by `clock_to(clk, …)` carrying the same level; the device has been run
    /// to no later than `clk` under the old lines. Redundant with the level — ATN
    /// changes only at a write, and every write is a sync point — and provided because
    /// the Ultimate's processor takes ATN as a vector and the drives get it the same way.
    fn atn_edge(&mut self, _clk: u64, _level: bool) {}

    /// The unit numbers it answers to, a bit per unit (bit 8 = unit 8). Advisory —
    /// used for refusals only (§4). Default: none the machine can know.
    fn units(&self) -> u16 { 0 }

    /// The model's clock rate, at attach and at every model switch (Spec 863).
    fn set_cpu_hz(&mut self, _hz: u32) {}

    /// The C64's RESET reached the bus (cold/warm reset, power-on). Whether a device
    /// is wired to the IEC RESET line is its own business. Default: nothing.
    fn c64_reset(&mut self) {}

    /// Checkpoint hooks. `None` = opted out (§8).
    fn checkpoint(&self) -> Option<serde_json::Value> { None }
    fn restore(&mut self, _state: &serde_json::Value) -> Result<(), String> {
        Err(format!("{}: carries no checkpoint state", self.name()))
    }

    /// A copy for a cloned machine, or `None` (§7). Default `None`, as
    /// `ExpansionDevice::clone_device`: a host's device belongs to the host.
    fn clone_device(&self) -> Option<Box<dyn IecDevice>> { None }
}
```

Decisions behind the shape:

- **`bus` is "the others", not the wire.** The folder needs it this way — it changes its
  own pull between two syncs and recomputes the wire itself (`folder_device.rs:1443`,
  `lines = others & pull`) — and a device that wants the wire calls `bus.with(outputs)`.
  Passing the wire would make the folder's own pull at the last sync look like somebody
  else's.
- **The slot byte is the folder's.** `IecOut` becomes `drv_bus[slot] = (!clk as u8) << 6 |
  (!data as u8) << 7` — `0xc0` released, exactly what the folder writes today
  (`W_CLK | W_DATA`, `folder_device.rs:1216-1217`) and what `set_folder_units` seeds
  (`iec.rs:931`). Bits 0-5 stay 0, as they are for the folder now, so `cpu_port` and every
  checkpoint's `iec` node are unchanged.
- **Only CLK and DATA out.** The Ultimate also drives ATN and SRQ (`atn_o`, `srq_o`); TRX64's
  fold derives the drives' ATN from `cpu_bus` alone (`iec.rs:383-384`), so a device's ATN pull
  would reach nobody. Master mode and SRQ are out of scope (§13).
- **1 MHz.** Every call is stamped in C64 cycles. The Ultimate's processor acts within a
  system clock of an input change, far inside one C64 cycle, and counts its waits in 1 MHz
  ticks; a device converts through `set_cpu_hz`. Sub-cycle order is not modelled.
- **`rebase` is required.** A device that keeps its own time (UE2's, stepped in µs) cannot
  survive a clock that jumps backwards on a restore; a silent default would hide that.

## §4 D2 — Slots, units, several devices, refusals

- **A slot is a bus position, a unit is a number the device answers to.** For a drive and a
  folder they coincide (`drv_bus[unit]`). For a device that does its own addressing they do
  not: the Ultimate's processor has no unit in hardware (§2). So attach names the **slot**:
  `Machine::attach_iec_device(slot: u8, dev: Box<dyn IecDevice>)`, slot 4-11.
- **Slots 4-7 are the place for a device without a unit.** VICE folds them (they are its
  printer/plotter positions); TRX64 has no printer and never writes them (§1). A device there
  takes no unit from any drive or folder. UE2's processor goes at slot 4. The two copy loops
  that feed a drive the other slots (`drive.rs:1376`, `drive1581.rs:443`) widen from 8-11 to
  4-11 so a drive's own re-fold sees it. Without a device those slots are `0xff` everywhere,
  so nothing changes.
- **Slots 8-11** are allowed for a device that wants to stand where a unit's drive would.
  The slot is then also its unit: a drive cannot answer there.
- **`units()` claims numbers without a slot.** A device at slot 4 that answers 9 (the
  firmware patched 9 into its microcode) reports bit 9; the machine then refuses drive B or
  a folder at 9, as 871/873 refuse two devices at one unit. `units()` is read at attach and
  at every configuration change that takes a unit (drive power and unit, folder attach) —
  not at every sync. A device whose firmware changes its numbers while running is not
  policed; the monitor shows what it claims now.
- **Refusals**, each by name, the occupant named as 871/873 do:
  - slot outside 4-11;
  - slot held — by a powered drive position (its `bus_slot`, or its jumpers for the next
    reset, as `attach_folder` checks, `lib.rs:2895-2903`), a folder, or another device;
  - a claimed unit that a powered drive or a folder answers to;
  - and the reverse, in `unit_claimed_by_other` (`lib.rs:3007-3015`) and `attach_folder`: a
    unit a device's slot or `units()` holds.
- **Several devices**: one per free slot, eight slots shared by at most two drives, the
  folders and the host's devices. Order at a sync is **slot order** (today's unit order for
  folders); each device's pull is in its slot before the next one computes its `bus`, as
  `folders_sync` does now.
- **Detach** returns the box to the host: `detach_iec_device(slot) -> Result<Box<dyn
  IecDevice>, String>`. The slot is released (`0xff`) and the map loses it — 870's "off".

In the IEC core, `folder_units` becomes `device_slots: u16` (bits 4-11) and
`set_folder_units` becomes `set_device_slots`, looping 4-11 instead of 8-11; `adopt_drive_slots`
releases every unoccupied slot 4-11. Slots 4-7 are `NONE`/`0xff` on every machine without a
device, so the map and the callback are byte for byte what they were.

## §5 D3 — Where each call happens

`Machine::iec_devices: IecDevices` (a `Vec<(slot, Box<dyn IecDevice>)>` kept in slot order,
with the `Clone` of §7) replaces `Machine::folders`; `FullBus::folders` (`full.rs:214`)
becomes `FullBus::iec_devices`. `folders_sync` becomes `iec_devices_sync(devs, iec, t)`:

```
for (slot, dev) in devs:                      // slot order
    others = cpu_bus & drv_bus[s] for every s in 4..12, s != slot
    dev.clock_to(t, IecLines { atn: cpu_bus & 0x10 != 0, clk: others & 0x40 != 0,
                               data: others & 0x80 != 0 })
    drv_bus[slot] = slot_byte(dev.outputs())
iec_update_ports()
```

The call sites are today's three, unchanged in place and order:

| sync point | function | `t` | before it |
|---|---|---|---|
| end of instruction | `Machine::catch_up_drives` (`lib.rs:2856`) from the run loop (`lib.rs:3617`) | post-instruction `clk` | both drives caught up and folded |
| `$DD00` read | `FullBus::iec_push_flush_to` (`full.rs:397`) | `clk` | both drives caught up and folded |
| `$DD00`/`$DD02` write | the CIA2 write branch (`full.rs:700-733`) | `clk + 1` | drives caught up (`iec_catch_up_to`), conf write with the new `cpu_bus`, drives' ATN edges |

**ATN edges** are added to the write path only: before the conf write the branch keeps
`old = iec.iec_old_atn`; after it, if `iec.iec_old_atn != old`, every device gets
`atn_edge(clk + 1, iec_old_atn != 0)`, then `iec_devices_sync(…, clk + 1)`. conf3 keeps
returning edges for true drives only (`iec.rs:606-613`); a device's edge is not an `AtnEdge`
variant, because nothing in the core has to decide it per drive type.

**What the C64 sees is cycle-exact.** A `$DD00` read syncs every device to the read cycle
before `iecbus_callback_read` returns `cpu_port`, so a pull a device makes at cycle *P* is
seen by the first read at or after *P*. What the drives see of a device is slice-granular:
its pull reaches a drive at the drive's next feed — the granularity 871 gives between A and
B, and 873 gives the folder (873 §4).

## §6 D4 — While the CPU is held

- **`Hold::Cpu`**: `run_held` already ends with `catch_up_drives(clk)` (`lib.rs:2158`), so
  every device is clocked to the end of the held span with the drives, as now.
- **`Hold::Reset`**: `run_held`'s other branch (`lib.rs:2159-2160`) gains
  `iec_devices_sync(clk)` when devices are attached — the devices alone; the drives still
  stand still, as 850 D7 decided. The U64 holds the C64 in reset for 2.06 s after a reset
  (TRX64 BUG-061) while its FPGA runs; UE2's processor must keep time through that.
- **Granularity under a hold is the held span.** `run_held` runs out the whole remaining
  budget and syncs once (`lib.rs:2129-2160`). With the C64 held nobody reads `$DD00`, so
  only device-to-drive exchanges are coarse — as they already are for drive A and B. The
  host sets the granularity with its run budget; no sub-span sync is added (§14).
- **The folder under `Hold::Reset`** is now advanced at the end of the span where it was
  not. The result is the same: with the lines constant, `advance` runs each due transition at
  its due time whenever it is called (`folder_device.rs:1400-1414`), and after the span the
  first sync would have done the same work.

## §7 D5 — Reset, clock rate, clone, power cycle

- **C64 reset**: `cold_reset` rebuilds the IEC core (`lib.rs:1281`). Today it then gives the
  folders their slots back and calls `reset_from_c64` on each (`lib.rs:1291-1302`); now it
  calls `set_device_slots`, then `c64_reset()` on every device and writes each `outputs()`
  into its slot, then one fold.
- **Model switch** (`lib.rs:969-972`): `set_cpu_hz(t.cpu_hz)` on every device. Also at
  attach.
- **Attach**: `set_cpu_hz`, `rebase(clk, lines)`, `set_device_slots`, slot seeded from
  `outputs()`.
- **Clone.** `Machine: Clone` is load-bearing (`lib.rs:515-517`). `IecDevices::clone` asks
  each device for `clone_device()`. A device that answers `None` leaves a **vacant** entry
  in the clone: same slot, same name, outputs released, no-op calls — so the clone's device
  map and callback stay what they were, and the clone reports it through
  `Machine::iec_devices_uncovered()` (§8). The folder answers `Some(self.clone())`: a cloned
  machine carries its folder as today (the source `Arc` shared).
- **C64 power cycle in the session** (`trx64-session/src/lib.rs:273`, `:231-235`): the
  session takes every device (`Machine::take_iec_devices`) and gives them to the new machine
  (`reattach_iec_devices`: `set_cpu_hz`, `rebase(clk, lines)`, `c64_reset`, slots, fold) —
  the generalisation of `folders_held` / `reattach_folders` (`lib.rs:2932-2952`).

## §8 D6 — Checkpoints: hooks and the opt-out

**Decided: an opt-out device does not refuse a checkpoint. The checkpoint is taken without
it and names it.** Refusing would take rewind, the ring and every `.c64re` from a host whose
device cannot be saved — and UE2 said it needs none. Silently leaving it out would make a
restored machine look whole when it is not (bug 792's lesson, which the expansion port
already follows).

- **Capture.** A `FolderDevice` goes into the `folders` node exactly as today
  (`c64re_snapshot.rs:1538-1544`) — same key, same serde, so 873's `.c64re` files and the
  folder gate's checkpoints are unchanged. Every other device goes into a new `iecDevices`
  node: `[{slot, name, units, state}]`, `state` = `checkpoint()` or `null` for an opt-out.
  **No non-folder device → no `iecDevices` key**: every existing checkpoint and the
  `cia_alarm_check_gate` digests stay as they are.
- **Restore.** Devices are the host's, so a restore never creates or removes one (the
  expansion port's rule, `c64re_snapshot.rs:1966-1971`):
  - node entry with `state` and a live device at the same slot and name → `restore(state)`;
    an `Err` fails the restore with its message, as a malformed `folders` node does;
  - node entry with `state: null`, a live device the node does not list, or a node entry
    with no live device at that slot → the device (if any) stays as it is live,
    `rebase(restored clk, restored lines)`, and its name goes into
    `Machine::iec_devices_uncovered()`;
  - after the checkpoint's `iec` node is restored (`restore_iec`,
    `c64re_snapshot.rs:645-658`, which writes the pull the device had at capture), each
    uncovered device's slot is overwritten from its live `outputs()` and the bus is folded
    once: the lines are the live device's, not a stranger's.
  - the device map rebuild (`c64re_snapshot.rs:1889-1900`) uses `device_slots` = the
    restored folders' units | the live devices' slots.
- `iec_devices_uncovered()` is cleared by a restore that covers every device and by
  detaching the one it names. The daemon puts it into `session/state` beside
  `expansionRamUncovered` if it ever carries a host device; it carries none today.

## §9 D7 — The folder device is the first implementor

`impl IecDevice for FolderDevice`, byte-identical by construction:

| trait | folder |
|---|---|
| `name` | `"folder {unit}"` — the monitor's column header stays `folder 9` |
| `clock_to(t, bus)` | `advance(t, others, !bus.atn)`, `others = 0x3f | clk<<6 | data<<7` — the byte `folders_sync` builds today (`folder_device.rs:1757-1764`) |
| `outputs` | from `line.pull` (`0x40` CLK, `0x80` DATA) |
| `rebase(clk, bus)` | `line.now = clk`, `line.timeout = min(timeout, clk)`, `line.atn_low = !bus.atn` — what `attach_folder` (`lib.rs:2906-2908`) and `reattach_folders` (`lib.rs:2938-2940`) do. After a power cycle `reattach_folders` left `atn_low` at the old machine's value; the next `clock_to` overwrites it before any transition reads it (a device just reset waits for nothing: `due()` is `None`, `folder_device.rs:1379-1397`) |
| `atn_edge` | default (nothing): the folder sees the level at the `clock_to` of the same cycle, as now |
| `units` | `1 << unit`; attached at slot = unit |
| `set_cpu_hz` | `cpu_hz = hz` |
| `c64_reset` | `reset_from_c64()` (honours `reset_line_connected`) |
| `checkpoint` / `restore` | not used — the `folders` node is its checkpoint (§8) |
| `clone_device` | `Some(Box::new(self.clone()))` |

`attach_folder` / `detach_folder` / `folder(unit)` / `folder_mut(unit)` keep their
signatures: they build the device and go through `attach_iec_device(unit, …)` with the 8-11
check they have now, and find a folder by downcast (`as_any`). `detach_folder` still calls
`close_all` before it drops the device. The daemon's verbs, `session/state` `folders`,
`folderEvents` and the monitor's `folder` verb reach the folder the same way; the monitor's
`iec` verb gets a column per device (header `name()`), byte-identical with only folders.

## §10 D8 — The surface UE2 uses

`trx64-core` as a crate, no daemon, no FFI:

```rust
use trx64_core::iec_device::{IecDevice, IecLines, IecOut};

impl IecDevice for IecProc { … }                      // UE2's c64-bridge
machine.attach_iec_device(4, Box::new(proc))?;        // slot 4: no unit taken
machine.iec_device_as::<IecProc>(4)                    // &T by downcast, between runs
machine.iec_device_as_mut::<IecProc>(4)                // &mut T: the firmware side's FIFOs
machine.detach_iec_device(4)?;                         // the box back
machine.iec_devices_uncovered()                        // after a restore or a clone
```

- Between two `run` calls the host reaches its device by downcast, as it reaches the UCI
  block (Spec 852). During a run only the machine calls it; a device that must hand data to
  the firmware mid-run holds its FIFOs behind whatever sharing the host chooses.
- **No daemon verb.** A host device is a Rust object; the wire cannot carry one. The daemon
  keeps `device/folder_*`. `trx64-ffi` (the Swift façade over the daemon) is untouched.

## §11 Cost

- **None attached**: one `is_empty()` per sync point — the test that stands there today —
  and on a `$DD00` write the ATN compare only inside it. The drive copy loops (§4) copy four
  more bytes per drive slice; measured, and gated on the slot mask if it shows.
- **Attached**: per sync point per device two virtual calls (`clock_to`, `outputs`) where the
  folder had direct calls. 873 measured an idle folder at ×1.036 of no folder
  (873 §14, §11.12); 874 must not move either number beyond run-to-run noise.

## §12 Acceptance

**Gate — must pass:**

1. **A test device.** `crates/trx64-core/tests/iec_device_gate.rs` carries `ProbeListener`, a
   minimal listener: on ATN falling it pulls DATA; it takes bytes under ATN with the
   listener handshake; a primary that is not `$20+u`/`$40+u`/`$3F`/`$5F` releases both lines
   until ATN rises; it records every `atn_edge` and `clock_to` with its cycle, and can be told
   to release DATA at a set cycle. `units()` configurable, `checkpoint()` `None`.
2. **Cycle-exact at `$DD00`.** A C64 loop of `LDA $DD00` / store with the cycle of each read
   known from the trace: the device releases DATA at a chosen cycle *P*; the first read that
   sees DATA high is the first read at or after *P*, for *P* stepped over every cycle of one
   loop pass. Same with the device at slot 4 and at slot 9.
3. **ATN edge at its cycle.** `STA $DD00` asserting ATN at store cycle *W*: `atn_edge` arrives
   with `clk = W + 1` and `level = false`, before a `clock_to(W + 1, …)` with `atn: false`;
   the next `LDA $DD00` sees DATA low. Release likewise. `$DD02` changing the port likewise.
   KERNAL `LISTEN 9` against the probe at slot 9: no DEVICE NOT PRESENT.
4. **Under hold.** `Hold::Cpu` for *N* cycles: the last `clock_to` has `clk` = the held
   span's end; a DATA release scheduled inside the span is seen by the first `$DD00` read
   after it. `Hold::Reset` likewise (today: no call at all). And drive 8 under `Hold::Cpu`
   sees the probe at slot 4 (the widened copy loop).
5. **Refusals.** Slot 3 and 12; slot 8 with drive A at 8; slot 9 with drive B's jumpers at
   9 while powered; a folder's unit; `units()` = bit 9 with drive B at 9, and in reverse
   `set_drive_power(B)` at 9 and `attach_folder(9)` with the probe claiming 9; each names the
   occupant.
6. **Checkpoint opt-out.** With the probe at 4: the capture has `iecDevices` naming it with
   `state: null`; the restore leaves it attached, `rebase`d to the restored clock,
   `iec_devices_uncovered()` names it, and its slot holds its live pull. Without a host device
   the capture has no `iecDevices` key. A device with hooks: `restore` gets back what
   `checkpoint` gave.
7. **Clone.** A cloned machine has a vacant, released slot 4 and names it uncovered; the
   original is untouched. A cloned machine with a folder carries the folder, as today.
8. **Reset and model.** `c64_reset` on a C64 reset; `set_cpu_hz` at attach and on a switch
   to NTSC (Spec 863) and back.
9. **The folder, byte-identical.** `folder_device_gate.rs` unchanged: 13 green, the two
   characterisations as recorded; the 873 checkpoints (mid-LOAD, mid-SAVE, folder alone)
   restore in 500-frame lockstep, including a `.c64re` written by 873's build.
10. **The seven games.** Default: 7/7, screenshots byte-identical to main's. `GATE_FOLDER=9`:
    7/7 with 873's `folder_expected` unchanged (4 byte-identical, greenberet/scramble/maniac
    differing). `GATE_DRIVE_B=9`: 7/7 with 871's expectations.
11. **Maniac Mansion with a host device.** Drive 8 alone plus the probe at slot 4 (no unit):
    the drive-8 fastloader, which asserts ATN and holds it after `M-W` ×16 / `U3`, meets a
    device that keeps DATA pulled — the break 873 §14.1 item 7 measured for the folder at 9
    ("Diskettenfehler"). Expected: the picture differs from the no-device run and equals the
    `GATE_FOLDER=9` maniac picture (the probe and the folder answer a held ATN identically:
    DATA from the fall, no byte ever sent). If it does not equal it, the gate reports the
    first divergence, not a pass.
12. **Nothing else moved.** `cia_alarm_check_gate` digests unchanged; the monitor golden
    transcript unchanged (`iec` with no device and with a folder); every drive and bus gate
    byte-identical.

**Characterisation — recorded, not pass/fail:**

13. **Cost**, 873's `characterise_the_cost_of_a_folder` re-run: no device, idle folder at 9,
    idle probe at 4, folder serving a LOAD — against 873's 4.949 / 5.125 / 3.368 ms/frame.

## §13 Scope — where this stops

- **In:** the `IecDevice` trait and its types; slots 4-11; attach/detach/downcast; the
  three sync points, the write-path ATN edge, both holds; reset, clock rate, clone, power
  cycle; the checkpoint hooks and the named opt-out; the folder as the first implementor;
  the monitor column.
- **Out:** a device driving ATN or SRQ (bus master, the Ultimate's `atn_o`/`srq_o`); fast
  serial and burst over SRQ; parallel cables; a device on the drives' side of a cable; a
  device that runs mid-instruction or below C64-cycle resolution; a daemon/wire verb or an
  FFI binding for host devices; UE2's processor itself (it lives in UE2's repo).

## §14 Open

- **`units()` while running.** The U64 firmware can re-patch its device numbers at any time.
  This spec checks claims only at configuration changes. If UE2 needs the machine to refuse
  or report a claim that appears later, that is a notification from the device (a
  `units_changed` flag read at instruction end), not built here. For UE2.
- **A device that must end a run.** `ExpansionDevice::take_stop` lets a device stop the run
  at the next instruction boundary (`expansion.rs:116-120`). The Ultimate's flow control is
  honest (it holds DATA or CLK until the firmware has served its FIFO, 873 §2), so a host
  that runs in slices does not need it. Added only if UE2 measures a case that does.
- **Sync inside a held span.** Under a hold, devices and drives exchange lines once per
  held span (§6). If UE2's firmware holds the C64 while its processor talks to a TRX64 1541
  (drive to U64, the C64 not involved), a sub-span sync quantum may be needed. Not
  measured; not built.
- **The IEC RESET line for a host device.** `c64_reset` is a notification; whether the U64's
  processor is reset by the C64's RESET is the firmware's wiring and UE2's call.
