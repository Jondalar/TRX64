# Spec 875 — A controller of the host's own in the 1581

**Status:** PROPOSED (2026-09-24)
**Repos:** TRX64. C64RE: no change.
**Number:** 875 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`, row present).
**Depends on:** Spec 872 (the 1581 board, its CIA glue, the WD1772 and the MFM surface),
Spec 874 (the host-device pattern: trait, `AsAny` downcast, opt-out checkpoint, vacancy on
clone, `rebase`), Spec 871 (positions A/B), Spec 870 (power, own reset, held, stopped, the
connectable reset line, "a board changes only at power-on").
**Enables:** UE2 — the emulator that runs the unmodified Ultimate 64 firmware over
`trx64-core` — models the U64 FPGA's `wd177x.vhd` and lets the firmware
(`software/drive/wd177x.cc`) serve the 1581's sectors from the D81 file it holds open,
as on the real U64.
**Origin:** the owner, 2026-09-24: TRX64 lets a host controller replace the 1581's WD1772;
TRX64's own WD1772 and MFM surface stay the default. Requested by UE2. UE2 confirmed the
signal list on 2026-09-24 (§2.3).

**Sources read end to end:** `fpga/1541/vhdl_source/wd177x.vhd`, `c1581_drive.vhd`,
`cpu_part_1581.vhd`, `drive_registers.vhd`; `software/drive/wd177x.cc`; the 1581 DOS
(`1541ultimate/roms/1581.bin`, the image 872 identifies by SHA-1) at `$CBE8-$CBFF` and
`$CFD0-$CFE3`. TRX64: `drive1581.rs`, `wd177x.rs`, `fdd.rs`, `ciacore.rs` (port hooks),
`drive.rs` (the position), `drive_snapshot.rs`, `c64re_snapshot.rs`, `lib.rs`,
`iec_device.rs`, `trx64-session/src/lib.rs`, and the daemon's and monitor's 1581 read-outs.

---

## §1 What exists today (main, v0.9.0 = `cf756a8`)

Re-checked against the code:

- **The WD is a field of the board, reached by name.** `Drive1581::wd: Wd1770`
  (`drive1581.rs:295`), which owns the mechanism `wd.fdd: Fdd` (`wd177x.rs:114`). The
  drive CPU's bus reaches it in two places: `Bus1581::read` at `addr >> 13 == 3` →
  `wd.read(clk, addr & 3)` (`drive1581.rs:178-181`) and `Bus1581::write` →
  `wd.store(clk, addr & 3, val)` (`:199-202`). Both run the WD's microcode lazily up to
  the access (`wd177x.rs:851-852`, `:885-886`). The side-effect-free `peek` uses
  `wd.peek` (`drive1581.rs:617`).
- **The register window is `$6000-$7FFF`, four registers mirrored**; `$4000-$5FFF` is the
  8520 (`drive1581.rs:169-183`, `:191-204`; U6 Y3 / Y2, 872 §10.4). The U64 decodes the
  same (`cpu_part_1581.vhd:229-230`: `cpu_addr(14 downto 13) = "11"`).
- **Neither INTRQ nor DRQ reaches the drive CPU.** The only interrupt source the board
  feeds its CPU is the CIA's line, replayed from `cia.irq_events` at each instruction
  boundary and at the end of the slice (`drive1581.rs:477-484`). `Wd1770::irq`
  (`wd177x.rs:122`) is read by nobody outside the WD. The U64 agrees: the WD's
  `io_irq` goes to the firmware, not to the 6502 (`wd177x.vhd:91`, `:422`;
  `cpu_part_1581.vhd:150` has the CIA as IRQ). The DOS polls the status register.
- **The CIA glue is three port hooks over the board's parts** (`Ports`,
  `drive1581.rs:51-128`):
  - `store_pa` (`:66-70`) hands PA0 to `fdd.select_head` and PA2 to `fdd.set_motor`.
    `ciacore` calls it with the composed `PRA | !DDRA` and only when that byte changed
    (`ciacore.rs:774-780`); a CIA reset sets `old_pa = 0xff` and calls no hook.
  - `read_pa` (`:100-106`) puts the jumpers on PA3-4 and `!fdd.disk_change()` on PA7.
    **PA1 (/RDY) is never set**: it reads 0 as an input, VICE's always-ready (872 §10.3).
  - `read_pb` (`:109-111`) puts `!read_only` on PB6 (/WPS).
  The accessors `ports()`, `ports_pa_read`, `ports_pb_read` repeat the same formulas
  without side effects (`:569-637`).
- **The drive clock** is `core.clk`, 2 MHz: `run_cycles` advances `stop_clk` by
  `sync_factor_1mhz × CLOCK_FREQUENCY_1581` (`drive1581.rs:439`). The factor is
  `2 × floor(65536·10⁶ / cpu_hz)` (872 §2), so one drive cycle is 0.5 µs on PAL and on
  NTSC alike. It **restarts at 0 at every reset** (`Drive1581::reset`, `:378-382`).
- **Resets of the board funnel through one function.** `Drive1581::reset(number)`
  (`drive1581.rs:376-398`, ending in `wd.reset(0)`) is reached from
  `Drive1541::cold_reset` (`drive.rs:1126-1141`), which every reset path of the position
  takes: the drive's RESET (`reset`, `drive.rs:958`), the C64's RESET over a connected
  line (`reset_from_c64`, `:966`), hold and release (`set_reset_held`, `:1002`), power-on
  (`set_power(true)`, `:979`, and `power_on_reset`, `:950`), a board change
  (`force_board_type`, `:710`), and the machine's build (`boot_from_dir` resets B even
  while it is off, `lib.rs:3307-3316`). Power-off runs no reset: it writes back and stops
  clocking (`drive.rs:979-996`).
- **The medium is the WD's mechanism's.** `Drive1581::attach/detach/flush/image*`
  (`drive1581.rs:503-532`) go to `wd.fdd`; the position routes every media call there for
  a 1581 (`mount`, `drive.rs:797`; `attach_disk_with_unreported_write`, `:1471`;
  `detach_disk`, `:1512`; `sync_disk_bytes`, `:1538`; `disk_as_written`, `:1569`;
  `restore_medium_1581`, `:811-826`).
- **Checkpoints** carry the WD and the mechanism as VICE's `WD1770<n>` + `FDD<4n>`
  modules inside the position's blob (`drive_snapshot.rs:957-975`, restore `:1056`) and
  the D81 as written in `IMAGE0` (`:1068-1100`).
- **Read-outs outside the core** use only `board_1581()` → `head()`, `wd()`, `ports()`:
  the daemon's drive panel (`trx64-daemon/src/main.rs:13312-13330`) and the monitor's
  `drive` verb (`trx64-monitor/src/verbs.rs:951-970`). Direct field access to `wd` is
  confined to the core and two gate lines (`drive1581_gate.rs:766`, `:1160`).

So the board already has the door's frame: one bus arm for the register window, three
port hooks for the glue, one reset funnel. They only know `Wd1770`.

## §2 What the references do

### §2.1 The U64's WD block (`wd177x.vhd`)

The entity's ports (`wd177x.vhd:62-91`), each placed:

| port | dir | from / to | what TRX64 must supply |
|---|---|---|---|
| `clock`, `clock_en` | in | system clock; `clock_en` = the 6502's cycle (register access only, `:64`) | the drive cycle of each access |
| `reset` | in | `drv_reset` = firmware reset bit **or** IEC/C64 reset (when `use_c64_reset`) **or** system reset (`drive_registers.vhd:138`, `c1541_timing.vhd:90`) | the drive's reset, whatever its cause |
| `tick_1kHz`, `tick_4MHz` | in | time bases: stepper, 64 µs write delay (`:359`, `:376`) | a time base: the drive clock |
| `addr`, `wen`, `ren`, `wdata` → `rdata` | in/out | the 6502, `$6000-$7FFF` (`cpu_part_1581.vhd:229-230`) | register read/store |
| `motor_en` | in | `motor_on_i = not PA2 and power` (`cpu_part_1581.vhd:317`, `:343`) — makes the index pulse | PA2 and the power switch |
| `stepper_en` | in | `'1'` (`cpu_part_1581.vhd:342`) | — |
| `cur_track` | in | the mechanics counter in `c1581_drive.vhd:219-230`, stepped by the block's own `do_track_in/out` | — (host side) |
| `step`, `do_track_in/out` | out | the mechanics counter, the sounds | — (host side) |
| `mem_req/resp`, `io_req/resp`, `io_irq` | | the U64's memory (DMA) and the firmware (ITU IRQ) | — (host side) |

The entity takes **no** side, write-protect, ready or disk-change signal and gives the
drive board **no** index, track-0 or interrupt signal. Those are board signals that run
between the CIA and the firmware's `drive_registers.vhd`: `side_0` = PA0 to the firmware's
`side` register (`c1581_drive.vhd:210`, `:294`; `drive_registers.vhd:127-128`);
`write_prot_n` = the firmware's `sensor` → CIA PB6 (`drive_registers.vhd:177`,
`cpu_part_1581.vhd:272`, `:287`); `disk_change_n` = the firmware's `diskchng` → PA7
(`drive_registers.vhd:176`, `cpu_part_1581.vhd:306`); `rdy_n = not(motor_on and
inserted) and not force_ready` → PA1 (`c1581_drive.vhd:217`, `cpu_part_1581.vhd:303`,
`:312`).

What the block does with the 6502 (`:180-217`): a command store sets BUSY and pushes the
command into a FIFO whose non-empty flag is the ITU IRQ (`:183-188`, `:420-422`); a
command store while BUSY is taken only for `$Dx` (FORCE INTERRUPT, `:183`). A data read
clears DRQ if a byte was valid (`:207-212`); a data store marks the byte valid (`:197-199`).
BUSY is cleared by the firmware (`status_clear`, `:232-233`) or by the DMA engine at the
end of a read (`:329-332`) or 64 µs after the end of a write (`:358-378`). The type-I
status bit 1 is the index pulse (`:157-163`). Reset: track 1, sector 0, status 0, FIFO and
DMA idle (`:384-398`).

### §2.2 The firmware (`wd177x.cc`)

The IRQ handler queues `command | completion << 8` (`:106-118`); the drive task serves
it (`:132-152`). Type I commands set the stepper (`stepper_track`, `step_time`) and the
track register, clear the disk-change latch and clear BUSY (`:208-270`, `do_step`
`:156-187`). READ SECTOR finds the sector by stepper track, `drive->side` and the ID
registers, reads the file and starts a 512-byte DMA into the data register
(`:272-312`). WRITE SECTOR starts a DMA out (`:314-336`); the completion IRQ writes the
file (`:384-418`). READ ADDRESS DMAs six ID bytes (`:338-361`). READ TRACK is not
implemented (`:363-367`). WRITE TRACK DMAs 6 250 raw bytes, decodes them and rewrites the
track (`:369-375`, `:420-449`). FORCE INTERRUPT stops the DMA and clears BUSY
(`:377-380`).

### §2.3 What UE2 needs from TRX64 — settled by UE2, 2026-09-24

UE2 checked the list against `wd177x.vhd` and `c1581_drive.vhd`.

TRX64 does **not** provide:
- the **index pulse** — UE2's model makes it from `motor_en` and time (the `stepper`
  entity's `index_out`);
- **track 0 or the head position** — UE2's stepper counts them and serves the firmware's
  track register;
- the **write-protect bit in the WD status** — it comes from the host's medium.

TRX64 provides:
1. register read and store at the drive's cycle, and a catch-up on the drive clock. That
   clock is UE2's time base for the index, the step timing and the 64 µs write delay;
2. CIA → host: motor (PA2) and side (PA0);
3. host → CIA: /RDY (PA1), disk change (PA7), write protect (PB6);
4. the drive's RESET reaching the controller (RESTORE state, status cleared, FIFO
   flushed), as a notification, like 874's `c64_reset`; and power-off as its own
   notification, because TRX64 treats power, the drive's own reset and the connectable
   reset line as three things (870).

The checkpoint opt-out as in 874 is fine for UE2.

### §2.4 What UE2 was told, checked against the code

| told | verdict |
|---|---|
| WD window `$6000-$7FFF`, 4 registers mirrored; `$4000-$5FFF` the 8520 | **correct** (§1) |
| neither INTRQ nor DRQ reaches the CPU; DOS polls; no IRQ lines | **correct** (§1) |
| the host keeps BUSY/DRQ until served | **correct** for the VHDL (§2.1). **Plus a constraint:** the DOS waits for BUSY to go **high** after every command store, without a timeout — `$CBF5 STA $6000`, `$CBF8 LDA #$01`, `$CBFA BIT $6000`, `$CBFD BEQ $CBFA`; the first read is 6 drive cycles after the store. It then waits for BUSY low, also without a timeout (`$CBEC-$CBF1`; FORCE INT at `$CFD1-$CFE0` → `JMP $CBEC`). A controller that finishes a command before the DOS's first status read hangs the DOS in `$CBFA`; one that never finishes hangs it in `$CBEE`. The VHDL sets BUSY at the store (`:185`), so on the real U64 the first holds whenever the firmware serves more than 3 µs after the store (§3, contract) |
| register read/write at the drive cycle, 2 MHz, with a catch-up call | **correct**. Addition: the drive clock is 2 MHz **on every model** (§1), so the trait has no `set_cpu_hz`; and it **restarts at 0 at every drive reset**, so `drive_reset` carries the new clock |
| CIA → host: side (PA0), motor (PA2) | **correct**; the host gets the PA0 *pin level* (the U64's `side_0`) and `motor_on = !PA2 && powered` (the U64's `motor_on_i`) |
| host → CIA: /RDY (PA1), disk change (PA7), write protect | **correct, one precision:** write protect is **PB6** (/WPS), not port A. And PA1 is new wiring: TRX64's own board never drives it (§1) |
| checkpoint without disk/WD state, named, 874-style opt-out | **correct**, with the restore rule of §8 |

## §3 D1 — The trait

In `crates/trx64-core/src/fdc_controller.rs` (new), re-exported from the crate root:

```rust
/// What the drive board drives toward the controller — the U64's `side_0` and
/// `motor_on_i`. Composed from CIA port A as its pins stand (`PRA | !DDRA`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FdcBoardOut {
    /// PA0 pin level: `true` = high. The 1581 reads physical head 0 then
    /// (VICE `side = PA0 ? 0 : 1`, head-inverted surface: 872 §4).
    pub side0: bool,
    /// PA2 low and the drive powered — the spindle turns.
    pub motor_on: bool,
}

/// What the controller's mechanism side drives back into the CIA. `true` = asserted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FdcBoardIn {
    /// PA1 /RDY low.
    pub ready: bool,
    /// PA7 /DISK CHANGE low.
    pub disk_changed: bool,
    /// PB6 /WPS low.
    pub write_protected: bool,
}

/// The 1581's drive clock: 2 MHz on every model. A drive cycle is 0.5 µs.
pub const DRIVE_HZ_1581: u32 = 2_000_000;

pub trait FdcController: AsAny + Send {
    /// Shown in refusals, the monitor's `drive` verb and a checkpoint.
    fn name(&self) -> String;

    /// The 6502 reads register `reg` (0-3) at drive cycle `clk`. The controller has
    /// caught up to `clk` itself first. May change state (a data read clears DRQ).
    fn read(&mut self, clk: u64, reg: u8) -> u8;

    /// The 6502 stores `val` into register `reg` (0-3) at drive cycle `clk`.
    fn store(&mut self, clk: u64, reg: u8, val: u8);

    /// Register `reg` as a read would return it, without side effects (monitor).
    fn peek(&self, reg: u8) -> u8;

    /// Catch up to drive cycle `clk`. Called at the end of every slice the drive
    /// runs. `clk` never decreases between two calls except across `drive_reset`
    /// and `rebase`; the same `clk` may come twice.
    fn clock_to(&mut self, clk: u64);

    /// Port A changed what it drives, at drive cycle `clk` (a CIA store that moved
    /// PA0 or PA2, or the power switch). The controller has been run to no later
    /// than `clk` under the old value.
    fn board_out(&mut self, clk: u64, out: FdcBoardOut);

    /// What the mechanism drives into the CIA now. Read at every CIA port A/B read
    /// and by the side-effect-free accessors; `&self`, no time passes.
    fn board_in(&self) -> FdcBoardIn;

    /// The head, physical track (0-83) and side (0/1), for the monitor and the
    /// daemon's drive panel. Required: TRX64 has no mechanism of its own here.
    fn head(&self) -> (u8, u8);

    /// The drive's RESET ran — its own RESET input, a C64 RESET over a connected
    /// line, hold, release, power-on, the machine's build. The drive clock restarts:
    /// it is `clk` now. Port A stands released after it (`side0`, motor off).
    fn drive_reset(&mut self, clk: u64);

    /// The drive's power switch. Off: no call follows until it is on again. On is
    /// always followed by `drive_reset`.
    fn power(&mut self, _on: bool) {}

    /// The drive clock is `clk` without time having passed for the controller: at
    /// attach, and after a restore that did not carry its state.
    fn rebase(&mut self, clk: u64);

    /// Checkpoint hooks. `None` = opted out (§8).
    fn checkpoint(&self) -> Option<serde_json::Value> { None }
    fn restore(&mut self, _state: &serde_json::Value) -> Result<(), String> {
        Err(format!("{}: carries no checkpoint state", self.name()))
    }

    /// A copy for a cloned machine, or `None` (§7). Default `None`: a host's
    /// controller belongs to the host.
    fn clone_device(&self) -> Option<Box<dyn FdcController>> { None }
}
```

**The contract a controller keeps** (TRX64 imposes no WD semantics; the registers are the
controller's):
- **BUSY is visible at the first status read after a command store.** The DOS's `$CBFA`
  loop (§2.4) waits for it with no timeout. A controller that serves inside the same
  `store` or `clock_to` must leave BUSY set until at least one status read has seen it,
  or until a later drive cycle than the DOS's read 6 cycles after the store. TRX64 does
  not police this; the acceptance's test controller (§12.1) serves no earlier than 48
  cycles after the store, the data sheet's MFM bound (872 §10.2).
- **A command ends.** BUSY that never clears hangs the DOS in `$CBEE`, as a stalled U64
  firmware does. TRX64 adds no timeout.

Decisions behind the shape:
- **Drive cycles, not C64 cycles.** The WD sits on the drive's bus; every call carries
  `core.clk` (`drive1581.rs:158-163`), the value the built-in WD gets. 874's device was
  on the C64's bus and took C64 cycles; this one never sees them.
- **No `set_cpu_hz`.** The drive clock is 2 MHz whatever the model (§1). The constant
  says so once.
- **`drive_reset(clk)` carries the clock** because a reset restarts it at 0; a
  controller that keeps time in µs would otherwise see the clock run backwards. It
  replaces 874's `c64_reset`: the controller never sees the C64's RESET directly, only
  the drive reset it causes when the line is connected — the U64's `drv_reset`
  (`drive_registers.vhd:138`).
- **`board_out` on change only**, as `ciacore` calls `store_pa` (§1). The motor term
  includes power, as the U64's `motor_on_i` does, so `power(false)` is followed by nothing
  — the controller knows its motor stopped from the notification itself.
- **`board_in` is `&self`.** It is read inside every CIA port read, including the
  monitor's peeks, and must not advance anything. A host changes it between runs through
  the downcast, or inside its own `clock_to`/`store`.
- **`head()` required**, where 874's `units()` was advisory: without it the monitor and
  the daemon panel would have to invent a track (§9).

## §4 D2 — Fitting, removing, refusals, the medium

- **A controller is part of the board, so it changes only with the drive off** — the
  owner's rule that a ROM or a board changes at power-on (870 §4, 872 §5):
  `Machine::attach_fdc_controller(pos, dev) -> Result<Option<DiskImage>, String>`,
  `Machine::detach_fdc_controller(pos) -> Result<Box<dyn FdcController>, String>`.
- **Refusals**, by name, in the style of `set_drive_type` (`lib.rs:3259-3282`):
  - the position is powered: `"drive position A is powered; switch it off before fitting
    <name>"` / `"… before removing <name>"`;
  - the position holds a 1541: `"drive position B holds a 1541; <name> fits a 1581
    (set_drive_type first)"`;
  - a controller is already fitted: `"drive position A already has <other>"`;
  - `set_drive_type(pos, 1541)` while a controller is fitted: `"drive position A has
    <name>; remove it before changing the type to 1541"`.
- **The medium goes.** Attaching writes a mounted D81 back and ejects it, and returns it
  as `set_drive_type` returns an ejected medium (`drive.rs:710-748`): the caller persists
  it. TRX64's WD1772 and its `Fdd` stay in the board, at their reset state, with no disk —
  neither clocked nor on the bus, as the 1541 electronics stay in a position that holds a
  1581 (872 §11).
- **No media while fitted.** The position has no medium of TRX64's: the D81 is the host's.
  `Drive1541::mount` refuses: `"drive position A has <name>; its medium is the host's"`.
  `attach_disk_with_unreported_write` refuses the same way through its existing
  `eprintln` path (`drive.rs:1472-1476`) — its callers are a reset keeping the disk and the
  session's power cycle, neither of which has a disk to give then. `detach_disk`,
  `flush_disk_writeback`, `disk_as_written` and `image*` answer "nothing": no disk, no
  write, `None`. The daemon's media verbs reach `mount` and pass the refusal to the wire.
- **Detach** hands the box back. The built-in WD comes back on the bus at the drive's next
  power-on, with no disk, and the position's `disk` is `None`.
- **Positions A and B each take one.** Two positions, two controllers, independent.

## §5 D3 — Where each call happens

**Dispatch once per slice, not per access.** `Bus1581` becomes generic over a crate-private
`trait BoardFdc` with two implementations: `BuiltIn<'a> { wd: &'a mut Wd1770, read_only }`
(today's code, moved behind the trait without change) and `Host<'a> { slot: &'a mut
HostFdc }`. `Drive1581::run_cycles` picks the instantiation once, at the top of the slice.
The built-in path is today's code monomorphised; nothing in it tests for a controller.

| event | today (`drive1581.rs`) | with a host controller |
|---|---|---|
| CPU read `$6000-$7FFF` | `wd.read(clk, addr & 3)` (`:178-181`) | `read(clk, addr & 3)` |
| CPU store `$6000-$7FFF` | `wd.store(clk, addr & 3, val)` (`:199-202`) | `store(clk, addr & 3, val)`; the wrapper also records the last byte stored to reg 0 for `wd().command` |
| `peek` `$6000-$7FFF` | `wd.peek` (`:617`) | `peek(addr & 3)` |
| CIA `store_pa` (PA changed) | `fdd.select_head`, `fdd.set_motor` (`:66-70`) | `board_out(clk, {side0: PA0, motor_on: !PA2})` |
| CIA `read_pa` | PA7 from `fdd.disk_change()`, PA1 never (`:100-106`) | PA1 = `!ready`, PA7 = `!disk_changed`, both from `board_in()`, through the same `(tmp & !DDRA) \| (PRA & DDRA)` |
| CIA `read_pb` | PB6 from `read_only` (`:109-111`) | PB6 = `!write_protected` from `board_in()` |
| `ports()`, `ports_pa_read`, `ports_pb_read` | the same formulas (`:569-637`) | the same, from `board_in()` |
| end of slice | — | `clock_to(core.clk)` after the loop (`:482-485`) |
| board reset | `wd.reset(0)` (`:397`) | `drive_reset(0)` |
| power | `Drive1541::set_power` (`drive.rs:979-996`) | `power(on)`. No `board_out` accompanies power-off: `power(false)` itself says the motor stopped |
| `head()` | `fdd.track`, `fdd.head` (`:545-547`) | `head()` |
| `wd()` | the WD's fields (`:555-566`) | track/sector/data/status from `peek(1/2/3/0)`, `command` the last byte stored to reg 0, `busy` = status bit 0, `type_ = 0`, `step = -1` |

**The catch-up.** The built-in WD is lazy: it runs at each register access. A host
controller runs on its own clock too (UE2's index pulse, step timing and write delay), and
its firmware serves commands on host time. So it gets `clock_to` at the end of every slice
the drive runs, besides the `clk` every access carries. A stopped, held or unpowered drive
runs no slice (`drive.rs:1349-1355`) and so gets no `clock_to`: its clock does not move.

**Time resolution.** The CPU's view is cycle-exact: every register access carries its
cycle. The firmware side is slice-granular: a command the firmware serves between two
`run` calls is visible to the DOS from the next slice on — the granularity 874 gives a
device toward the drives. The DOS polls, so that costs time, not correctness, as long as
the BUSY contract (§3) holds.

## §6 D4 — Reset, power, held, stopped

Every call goes through `Drive1581::reset`, the funnel (§1):

| event | calls |
|---|---|
| drive's RESET pulse, powered (`Drive1541::reset`) | `drive_reset(0)` |
| C64 RESET, line connected, powered (`reset_from_c64`, from `warm_reset`, `lib.rs:1448-1450`) | `drive_reset(0)`; cut: nothing |
| hold (`set_reset_held(true)`), powered | `drive_reset(0)`; nothing until release |
| release (`set_reset_held(false)`), powered | `drive_reset(0)` |
| power on (`set_power(true)`, `power_on_reset`) | `power(true)`, then `drive_reset(0)` |
| power off (`set_power(false)`) | `power(false)` |
| stop / resume (`set_stopped`) | nothing: no slice runs, the clock stands |
| machine build (`boot_from_dir`, B reset while off, `lib.rs:3307-3316`) | `drive_reset(0)` — only if a controller was fitted before `boot_from_dir`, which the session never does (§7) |

The U64's WD reset is exactly this set — firmware reset bit, IEC/C64 reset when enabled,
system reset (`drive_registers.vhd:138`) — and its power bit only gates the motor
(`cpu_part_1581.vhd:317`), which is why `power` is its own call.

## §7 D5 — Clone, C64 power cycle

- **The slot.** `Drive1581` gains `host_fdc: Option<HostFdc>`: the box, its name, the last
  command byte, and `uncovered: bool`. `HostFdc: Clone` is written by hand, as
  `IecDevices` is (`iec_device.rs:160-181`): `clone_device()`, or a **vacancy** — the name
  kept, `uncovered = true`, and a socket with no chip: register reads return the open bus
  (`cpu_last_data`, U6 Y3 selecting nothing that drives D0-D7, the reasoning of 872 §10.4),
  stores go nowhere, `board_in` = not ready, disk changed, protected. The cloned DOS then
  answers `74,DRIVE NOT READY` if asked (872 §9.6). `Drive1581` keeps `#[derive(Clone)]`;
  `Machine`'s hand-written `Clone` (`lib.rs:730`) needs no new line.
- **`Machine::fdc_uncovered() -> Vec<String>`** names every vacancy and every controller a
  restore could not cover (§8), position by position. Cleared by a restore that covers
  it and by detaching it.
- **C64 power cycle in the session** (`trx64-session/src/lib.rs:265-280`, `:186-240`):
  `power_off` takes both controllers (`Machine::take_fdc_controllers() -> [Option<Box<dyn
  FdcController>>; 2]`) beside `drive_types`; `power_on` fits them back while the
  position is off — A is already switched off there for its type (`:204-207`) — before
  the disk re-attach (which then has nothing to attach) and before A/B come back on.
  The controller sees `rebase`, then `power(true)` and `drive_reset(0)`.

## §8 D6 — Checkpoints: hooks and the opt-out

**Decided, as 874 §8: an opt-out controller does not refuse a checkpoint. The checkpoint is
taken without it and names it.**

- **Capture.** A position with a controller writes its blob without the `WD1770<n>` and
  `FDD<4n>` modules (`drive_snapshot.rs:973`) and without `IMAGE0`
  (`capture_image_1581`, `:1068`; `driveDiskImage` / `driveB.disk` null). `DRIVE8`,
  `DRIVECPU0` and `CIA1581D0` are written as now. A new top-level node
  `hostFdc: [{position: "A"|"B", name, state}]` carries `state` = `checkpoint()` or `null`.
  **No controller → no `hostFdc` key and byte-identical blobs**: every existing checkpoint
  and the `cia_alarm_check_gate` digests stay as they are.
- **Restore.** A controller is the host's: a restore never creates or removes one (874 §8,
  the expansion port's rule). Per position, against the live machine:
  - node entry and a live controller of the same name → `restore(state)`, or for
    `state: null` → `rebase(restored drive clk)` and the name into `fdc_uncovered()`; an
    `Err` from `restore` fails the restore with its message;
  - node entry, no live controller, or a live controller of another name → **refused**,
    naming both: `"checkpoint has <name> in drive position A; the machine has <live or
    none>"`;
  - live controller, no node entry for that position (the checkpoint holds TRX64's WD and
    its disk there) → **refused**, naming the live controller.
  874 kept a mismatched host device attached and named it. A controller cannot be treated
  that way: the restored drive CPU is mid-conversation with the WD it was captured with,
  and a DOS polling a different chip is a machine that never existed.
- The blob reader learns that a `1581` blob may end after `CIA1581D0`; it then skips the
  WD/FDD read (`drive_snapshot.rs:1056`) and leaves `resync_iec_output` and the clocks as
  they are now.

## §9 D7 — TRX64's WD1772 stays the default, and stays untouched

- It is **not** made the trait's first implementor, unlike 874's folder. The folder had
  no state layout to protect; the WD has VICE's `WD1770`/`FDD` snapshot modules, the
  checkpoint's `IMAGE0`, the disk write-back and 872's timing tests, all reaching into
  `wd` and `wd.fdd` directly (§1). Wrapping it in a `dyn` call per register access would
  buy nothing and put 872's byte-identity at risk. It moves behind the crate-private
  `BoardFdc` (§5), monomorphised, and keeps every field and function it has.
- `write_track_log` stays the built-in WD's diagnostic; with a controller it stays empty.
- The monitor's `drive` verb adds one line `controller <name>` with a controller fitted
  (`verbs.rs:951-970`), and nothing without one. The daemon's drive panel
  (`main.rs:13312-13330`) reads `head()`/`wd()`/`ports()` as now; no new field.

## §10 D8 — The surface UE2 uses

`trx64-core` as a crate, no daemon, no FFI:

```rust
use trx64_core::fdc_controller::{FdcController, FdcBoardIn, FdcBoardOut, DRIVE_HZ_1581};

impl FdcController for Wd177xModel { … }                     // UE2's model of wd177x.vhd
machine.set_drive_type(DrivePosition::A, DriveType::Drive1581)?;   // with A off
machine.attach_fdc_controller(DrivePosition::A, Box::new(model))?; // A off; Ok(ejected D81)
machine.set_drive_power(DrivePosition::A, true)?;
machine.fdc_controller_as::<Wd177xModel>(DrivePosition::A)       // &T by downcast
machine.fdc_controller_as_mut::<Wd177xModel>(DrivePosition::A)   // &mut T: the FIFO, DMA, inputs
machine.detach_fdc_controller(DrivePosition::A)?;                // A off; the box back
machine.fdc_uncovered()                                          // after a restore or a clone
```

- Between two `run` calls the host reaches its controller by downcast, as 874's devices and
  852's UCI block. During a run only the machine calls it.
- **No daemon verb, no session/state field.** A controller is a Rust object; the wire
  cannot carry one. The daemon never fits one, so its media verbs keep working unchanged
  for every machine it runs.

## §11 Cost

- **None fitted**: one `Option` test per slice in `Drive1581::run_cycles` to pick the
  instantiation, and one per `peek`/`ports()` accessor call. No test per register access or
  port read: the built-in instantiation has none.
- **Fitted**: per slice one `clock_to`; per register access one virtual call; per CIA port
  read one `board_in`. The DOS's IEC loops read PB constantly, so `board_in` is on a hot
  path; it is `&self` and returns three bools.
- 872's `characterise_the_cost_of_a_1581` must not move beyond run-to-run noise without a
  controller.

## §12 Acceptance

**Gate — must pass.** All over the real KERNAL and the real 1581 DOS from the local ROM
directory. `crates/trx64-core/tests/fdc_controller_gate.rs`.

1. **A test controller.** `tests/common/sector_fdc.rs`: `SectorFdc`, the U64's split in
   one struct — a register block shaped on `wd177x.vhd` (BUSY at store, command FIFO,
   DRQ per byte, `$Dx` taken while busy, reset state track 1) and a "firmware" that
   serves the FIFO from an in-memory D81 at sector level as `wd177x.cc` does: type I
   commands set a stepper, READ SECTOR / WRITE SECTOR / READ ADDRESS by stepper track,
   `side0` and the ID registers, WRITE TRACK decoded as `decode_write_track`. It serves a
   command no earlier than 48 drive cycles after its store (§3), and hands bytes to the
   data register one per 64 drive cycles. Index from `motor_on` and the clock. `board_in`
   configurable; records every call with its clock. `checkpoint()` `None`; a `hooks`
   switch for the round trip.
2. **The DOS through it, byte-identical to TRX64's own WD.** For each of 872's
   `a_1581_lists_the_directory_a_static_read_gives`,
   `kernal_load_across_a_track_and_both_sides`, `kernal_save_writes_a_consistent_d81` and
   `format_writes_every_physical_track_and_a_valid_d81`: the same test with `SectorFdc`
   fitted to position A holding the same D81. The listing, the loaded bytes, and the D81
   after SAVE and after format (taken from `SectorFdc`) are byte-identical to the
   built-in run's persisted D81. Recorded, not judged: the C64 cycle at which each test
   reaches `READY.` in both runs.
3. **Side and motor seen.** During test 2's LOAD, which spans both sides, `board_out`
   arrives with `side0` true and false and with `motor_on` rising before the first READ
   SECTOR and falling after it, each at the drive cycle of the `STA $4000` that moved it
   (from the drive trace). No `board_out` is sent without a PA0/PA2 change.
4. **/RDY, disk change, write protect honoured.**
   - `ready: false` held: `LOAD"$",8` ends with the error channel reading
     `74,DRIVE NOT READY` (the DOS's `$CDBC` PA1 check, 872 §10.3).
   - `disk_changed: true`, cleared by `SectorFdc` on a step pulse: the DOS's STEP-IN /
     STEP-OUT recovery (`$CD83-$CDA8`) runs and the listing follows; held set whatever
     the steps: `74,DRIVE NOT READY`.
   - `write_protected: true`: `SAVE"X",8` leaves the error channel at
     `26,WRITE PROTECT ON` and the D81 unchanged.
   - Each input as the CIA reads it, compared with `ports()` at the same instant.
5. **Reset and power reach it.** `drive_reset(0)` at the drive's RESET, at a C64 warm reset
   with the line connected and not with it cut, at hold and at release, at power-on
   after `power(true)`; `power(false)` at power-off; nothing while stopped (the recorded
   clock does not move across 50 stopped frames). After every `drive_reset` the next
   `clock_to` is at or after the reset's clock, and never earlier.
6. **Refusals.** Attach while powered; to a 1541 position; a second controller; detach
   while powered; `set_drive_type(A, 1541)` while fitted; `mount` of a D81 while fitted;
   each names the position and the controller. Attach with a D81 mounted returns that D81,
   written back.
7. **Checkpoint opt-out.** With `SectorFdc` at A mid-LOAD: the capture has no `WD1770` /
   `FDD` module and no `IMAGE0`, and `hostFdc: [{position:"A", name, state:null}]`; the
   restore into the same machine rebases it to the restored drive clock and names it in
   `fdc_uncovered()`. With `hooks`: `restore` gets back what `checkpoint` gave, nothing is
   uncovered, and the restored run holds 500 frames of lockstep with the straight run. A
   checkpoint without `hostFdc` into a machine with a controller, and the reverse, are
   refused by name. Without a controller the capture has no `hostFdc` key.
8. **Clone.** A cloned machine has a vacancy at A, named uncovered; its drive reads the
   open bus in `$6000-$7FFF`; the original is untouched.
9. **Power cycle in the session.** A controller fitted at A survives `power_off` /
   `power_on`: same box (by downcast identity), `rebase`, `power(true)`, `drive_reset`,
   and `LOAD"$",8` lists afterwards.
10. **No controller: everything byte-identical to v0.9.0.** `drive1581_gate` (12 tests)
    green and unchanged; `cia_alarm_check_gate` digests unchanged; `drive_part_gate`,
    `second_drive_gate`, `drive_disk_checkpoint_gate`, `iec_device_gate`,
    `folder_device_gate` green; 872's `.c64re` checkpoints (mid-LOAD, mid-SAVE, 1541 +
    1581 mid-copy) written by a v0.9.0 build restore in 500-frame lockstep on this build.
    7-game gate, default: 7/7, all screenshots byte-identical to v0.9.0's.
    `GATE_DRIVE_B=9 GATE_DRIVE_B_TYPE=1581`: 7/7, all seven pictures byte-identical to
    v0.9.0's (872 §11's table).

**Characterisation — recorded, not pass/fail:**

11. **Cost**, 872's `characterise_the_cost_of_a_1581` re-run: B off, a 1581 idle at 9, a
    1581 idle at 9 with `SectorFdc` fitted, against v0.9.0 on the same machine.
12. **The seven games with a controller at 9** (`GATE_DRIVE_B_TYPE=1581 GATE_FDC=1`):
    expected byte-identical to the 1581-idle-at-9 pictures — no game touches unit 9's
    disk. Any difference is recorded with its first divergence.

## §13 Scope — where this stops

- **In:** the `FdcController` trait and its types; attach/detach/downcast per position,
  drive off only; the register window, the port-A outputs, the three inputs, the
  end-of-slice catch-up; reset, power, hold, stop, the C64 reset over the line; the
  refusal of media while fitted; clone with a vacancy; the session's power cycle; the
  checkpoint hooks, the named opt-out and the strict restore rule; the monitor line.
- **Out:** INTRQ or DRQ toward the CPU (the board has no such wire); a controller in a
  1541 (no WD there) or a 1571 (no 1571 yet); a host mechanism under TRX64's WD (sector
  source without a register model — a different door); fast serial (872 §8); the U64's
  firmware-side registers (`drive_registers.vhd`: power, reset, address, sensor,
  inserted, sound) — the host drives TRX64's own switches for power, reset and jumpers
  through the existing API; a daemon verb or FFI binding; UE2's model itself.

## §14 Open questions for UE2

1. **Where does the firmware serve relative to the drive clock?** If UE2's firmware runs
   between `run` calls, BUSY is up for at least the rest of the slice and `$CBFA` sees it.
   If UE2 serves synchronously inside `store` or `clock_to` (an in-process firmware
   thread that answers at once), the model must keep BUSY up until the DOS has read it
   (§3). Which is it?
2. **Is `head()` available?** UE2's stepper has `cur_track` and the firmware's `side`;
   TRX64 needs `(track, side)` for the monitor and the panel. Side as the head the 1581
   reads (VICE's mapping, `side = PA0 ? 0 : 1`) — or does UE2 prefer the raw `side_0`
   and TRX64 maps it?
3. **The strict restore rule (§8).** A checkpoint and the live machine must agree on the
   controller per position, or the restore is refused. Rewind within one run always
   agrees. Is there a UE2 flow that restores a TRX64-WD checkpoint into a machine with
   the model fitted (or the reverse)?
4. **Power without reset.** `power(false)` is sent at power-off; the U64's `power` bit
   only gates the motor and the CPU clock, and the WD is not reset by it. TRX64 always
   resets at power-on (870). Is `power(true)` followed by `drive_reset` right for UE2's
   model, or does it want the WD to keep its registers across a power-off like the FPGA
   block does?
5. **Hold as two resets.** TRX64 runs the reset sequence at the start of a hold and at its
   release, and clocks nothing between (§6). The U64's `drv_reset` is a level. Two
   notifications with no time between them are enough — or does UE2 want the level
   (`drive_reset_held(bool)`)?
