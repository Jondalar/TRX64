# Spec 870 — The drive as a part: power, reset, ROM and unit number

**Status:** MERGED (2026-09-23, v0.8.8)
**Repos:** TRX64 only. C64RE: no change.
**Number:** 870 (registry: `../../../C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** the 1541 port (`drive.rs`, `iec.rs`, `viacore.rs`), Spec 850 (the hold a
device can put on the machine), Spec 863 (the drive's sync factor per model).
**Enables:** Spec 871 (a second drive on the bus). 1571 and 1581 get their own specs later.
**Origin:** the UE2 emulator, 2026-09-23. UE2 runs the unmodified U64 firmware over
trx64-core and plays the U64's FPGA drive A with TRX64's drive 8. The firmware treats a
drive as a part it can switch off, hold in reset, give a ROM and a device number; TRX64's
drive 8 is none of those things, and UE2 works around each of them.

---

## §1 What the drive is today (v0.8.7)

Re-checked against the code, item by item from UE2's list:

- **Always powered, always clocked.** Drive 8 is caught up after every C64 instruction and
  part-way on `$DD00` accesses (`lib.rs` full path, `catch_up_to`), and on the lockstep path
  after every instruction (`run_cycles`). There is no "off" and no "held".
- **Always on the bus.** Its VIA1 port B output is folded into the IEC lines
  unconditionally (`iec_drive_write(!via1_pb_iec_output())` after every catch-up), and
  `cold_reset` rebuilds `IecCore` with unit 8 present.
- **ROM from a file only.** `rom` is private; `load_rom(dir)` reads one named file, insists
  on exactly 16 KiB and puts it at `$C000` of a 32 KiB buffer. A 32 KiB image cannot be
  given, and nothing can be given from memory.
- **Unit number fixed at 8.** The VIA1 backend is built with `number: 0`
  (`drive.rs`, six sites); `viacore.rs` turns it into the device-ID jumper bits on VIA1
  port B (`(number << 5) & 0x60`) and into the bus slot (`number + 8`). The DOS reads those
  bits at its reset — so the mechanism for 8-11 already exists and is only never set.
- **Reset follows the C64.** `warm_reset` flushes the disk write-back and cold-resets
  drive 8 (`lib.rs:1325-1327`). On real hardware the 1541 has its own power and its own
  reset, and a C64 reset reaches it only through the IEC RESET line, which the U64 lets the
  user disconnect (its drive RESET register, bit 1 = follow the C64).
- **Accessors.** `drive_ram_read` / `drive_ram_write` / `drive_peek` work per byte; there
  is no read of VIA2 port state (motor, LED, head stepping) beyond `led_on()`.

## §2 D1 — Power

A drive has a power state: **on** or **off**.

- **Off** means: not clocked, no CPU, no VIAs, and **its IEC outputs released** — the bus
  reads as if no device were plugged in. The drive's port is not folded into the IEC
  lines at all.
- **On** from off is a power-on: fresh RAM contents, CPU and VIAs through their power-on
  state, the disk (if one is mounted) kept — the disk is a medium in the mechanism, not
  state of the electronics.
- Power is part of the machine's state: it survives a C64 reset and is carried in a
  checkpoint.

The emulated equivalent of the U64's `DRIVE_POWER` bit. Not a model of a real power
switch's transient — a drive switched on is a drive that has just been switched on.

## §3 D2 — Reset, separate from the C64's

- A drive has its own reset input. **`warm_reset` of the C64 no longer resets the drive by
  itself.** Whether a C64 reset reaches the drive is a property of the connection — the IEC
  RESET line — and defaults to **connected**, which keeps today's behaviour for every
  caller that does not ask.
- Disconnected, a C64 reset leaves the drive running exactly where it was. That is what a
  1541 with its reset line cut does, and what the U64 does with bit 1 clear.
- A drive can be held in reset on its own (the U64's per-drive reset bit). Held means: not
  clocked, CPU in reset; releasing it runs the reset sequence. Its IEC outputs are
  released while held, as a 1541 in reset drives nothing.

## §3a D2a — Stopped: powered, clock frozen

A third state beside off and reset, from the U64's own drive (`fpga/1541/vhdl_source`,
UE2's reading): the drive's RESET register carries **bit 2 `stop_when_frozen`**, and
`c1541_drive.vhd:164-165` gates the drive's whole 16 MHz tick with it while the C64 is
frozen (menu, freeze, DMA load). `c1581_drive.vhd:163` does the same.

- **Stopped** means: powered, **not clocked at all** — no catch-up on `$DD00`, no
  `run_cycles`, no rotation. CPU, VIAs and head stand exactly where they were.
- Its VIA outputs **keep driving the IEC lines as they were**. Stopped is not off: the bus
  does not see the drive disappear.
- Released, it resumes where it stood, **no reset**, and its clock re-anchors to the C64's
  without replaying the gap — the time it was stopped did not happen to it.
- A flag per drive, set and cleared by the host. TRX64 does not decide when to stop a
  drive; the U64 couples it to its freeze, and that coupling is the host's.

## §4 D3 — The ROM is a value

- The ROM is given as bytes: **16 KiB** (placed at `$C000`) or **32 KiB** (the whole
  `$8000-$FFFF`, for the ROMs that use it — JiffyDOS, SpeedDOS and friends ship as 32 K on
  some boards). Any other size is refused by name.
- The file loader stays as a convenience on top of it and keeps its current names.
- A ROM given to a powered drive takes effect at its next **power-on**, and only then. A
  reset keeps the ROM the drive has. That is the hardware: a ROM is fixed for as long as
  the drive has power, and even a board with a ROM switch has to be switched off and on
  for the other ROM to run (owner, 2026-09-23).

## §5 D4 — The unit number

- A drive has a unit number, **8 to 11**, set through the device-ID jumper bits on VIA1
  port B — the mechanism `viacore.rs` already has. The DOS reads them at reset, so a change
  takes effect at the drive's next reset, exactly like moving the jumpers.
- **Numbers above 11 are not a hardware property** of a 1541 and are not modelled. A host
  that wants 12+ (the U64's `HW_ADDR` allows it) writes the DOS's own variables after the
  drive's reset, as UE2 does today — that is software, and it stays on the host's side.
- The bus slot follows the unit number (`number + 8`), which is what Spec 871 needs.

## §6 D5 — Accessors

Read-only, side-effect free, for the monitor and for hosts:

- drive RAM as a slice, not per byte;
- VIA1 and VIA2 port state as the pins see it — motor on, LED, head step phase, write
  mode, density zone;
- the current half-track;
- power, reset-held and unit number.

No writes beyond what exists. A host that needs to change drive state does it through the
drive's own inputs (the disk, the bus, power, reset).

## §7 Scope — where this stops

- **In:** power, own reset, reset-line connection, stopped, ROM from bytes (16 K / 32 K),
  unit number 8-11, read accessors — for the one drive that exists today.
- **Out:** a second drive (Spec 871); 1571 and 1581 (their own specs); a disk surface fed
  from outside, write hooks and per-track bit time (decided against, 2026-09-23); a
  parallel cable; unit numbers above 11.

## §8 Acceptance

1. **Off leaves the bus alone.** With drive 8 off, the C64's `$DD00` reads the IEC lines
   as with no drive attached, and `LOAD"$",8` ends in DEVICE NOT PRESENT.
2. **Off to on is a power-on.** Switched on, the drive runs its reset and a directory load
   works; the mounted disk is still mounted.
3. **Held costs nothing and resumes cleanly.** A drive held in reset for a second and
   released behaves like one freshly reset; while held, the IEC lines are released.
4. **The reset line.** Connected: a C64 warm reset resets the drive (today's behaviour,
   the existing gates unchanged). Disconnected: a warm reset during a drive-side loop leaves
   the drive's PC and RAM exactly as they were.
5. **ROM from bytes.** A 16 K and a 32 K image given from memory boot; any other size is
   refused with the size in the message. The file loader still works.
6. **Unit number.** With the jumpers at 9, `LOAD"$",9` works and `LOAD"$",8` does not.
7. **Nothing else moved.** The 7-game screenshot gate and every drive gate byte-identical
   with the defaults (on, connected, unit 8, stock ROM).
8. **Stopped.** A drive stopped mid-transfer for a second of C64 time and released resumes
   at the same PC with the same VIA and rotation state, and the transfer completes; while
   stopped its IEC outputs are unchanged and no drive cycle runs.
9. **Checkpoints.** Power, held, stopped, connection and unit number survive dump/undump.

## §9 Open

- ~~Whether a checkpoint taken with a drive off should carry the drive's RAM.~~ Decided
  at build: **carried**. The drive blob is captured exactly as before whatever the power
  state; a restore needs no special case.
- ~~The drive's power-on RAM pattern.~~ VICE allocates the unit context with
  `lib_calloc`, so its pattern is zeros; power-on clears the 2 KiB to zero. Stays open
  only in the sense the spec gave it: until a disk says otherwise.
- **The drive ROM is not in a checkpoint** (neither is the C64's). Restoring a checkpoint
  taken with a JiffyDOS drive into a machine with the stock ROM runs the drive on the
  stock ROM. The host gives the ROM; whether a checkpoint should carry a non-stock one
  is undecided.
- **16 K at `$C000`, `$8000-$BFFF` zero.** VICE copies a 16 K image into the lower half
  as well (`iecrom.c`, "ROM was loaded to the upper part of the buffer"), so its
  `$8000-$BFFF` mirrors `$C000-$FFFF`. TRX64's file loader always left it zero and the
  spec says "placed at `$C000`"; kept, so the stock machine does not move. Whether to
  mirror is a separate decision.
- **The VICE snapshot (`.vsf`) export** still writes the IEC lines of slot 8. A drive at
  another unit or off exports wrongly there; the `.c64re` checkpoint is right.

## §10 As built (2026-09-23, branch `spec-870-drive-as-a-part`)

**Where it lives.** All state is on `Drive1541` (`drive.rs`); the machine only asks it.
`powered`, `reset_held`, `stopped`, `reset_line_connected`, `unit` (in force) and
`unit_jumpers` (as set), plus a pending ROM. The six `number: 0` VIA-backend sites take
`unit − 8`.

- **Not clocked** (off / held / stopped) is one gate at the top of `run_cycles`: no
  cycle runs and the drive's `stop_clk` target does not move. `catch_up_to` still
  returns the C64 clock, so the machine's catch-up reference moves on without the drive
  — that is the "re-anchored without replaying the gap" of §3a, with no extra code on
  release.
- **On the bus** is `bus_slot()`: `Some(unit)` when powered and not held, `None`
  otherwise. Every place that folded `iec_drive_write(~pb, 0)` now calls
  `Drive1541::fold_into_iec` / `set_iec_data_no_fold`, which fold into the drive's own
  slot and first run `IecCore::sync_drive_slot` — a one-compare no-op on the stock
  machine. When the slot changes it sets the device map the way `iecbus_status_set`
  would for a single true drive (Conf1 at 8, Conf2 at 9, Conf3 at 10/11, **Conf0 for
  none**) and releases the vacated slot like `iec_drive_port_default`. It does this
  directly, not through `iecbus_status_set`, whose function-static arrays are shared by
  every machine on the thread.
- **Off and held both leave the bus as Conf0** — VICE's "no device" callback, where the
  C64 reads only its own outputs. Releasing the slot alone (Conf1 with `drv_bus = 0xff`)
  is not enough: the 1541's ATN-acknowledge term would still pull DATA when the C64
  asserts ATN, and the KERNAL would never say DEVICE NOT PRESENT. §3 says a drive in
  reset drives nothing; taken literally, that includes the ATN-ack gate. Leaving Conf0
  seeds `iec_fast_1541` from the C64's port; rejoining re-derives `cpu_bus` /
  `iec_old_atn` from it (`iecbus_cpu_undump`), because Conf0 stops maintaining them.
- **Reset.** `Drive1541::reset()` is the drive's RESET input: flush a pending disk
  write, `cold_reset`, re-attach the disk (the sequence `warm_reset` used to do inline).
  A drive without power ignores it. `warm_reset` calls `reset_from_c64()`, which resets
  only with the line connected. `cold_reset` brings the pending ROM and the jumpers into
  force.
- **Held.** Asserting and releasing both run the reset sequence (a 6522's RES clears it
  at once; release starts the CPU's sequence). Held without power only moves the flag.
- **Power.** Off flushes the disk write-back (VICE `drive_disable`). On from off clears
  RAM and `cpu_last_data`, then runs the reset sequence with the disk kept.
- **Stopped — decisions the spec did not state.** (a) A reset pulse reaches a stopped
  drive (RESET is not a clocked input); it stays stopped, standing at the reset state.
  (b) ATN edges that arrive while stopped are not latched by VIA1 (its clock is
  frozen). On release, if ATN ended somewhere other than where the VIA last saw it, the
  one net edge is delivered — what a clocked edge detector sees on its first tick. ATN
  low-then-high while stopped delivers nothing.
- **ROM.** `set_rom(&[u8])`: 16 KiB at `$C000` (lower half zero, see §9), 32 KiB for the
  whole `$8000-$FFFF`, anything else `RomError::BadDriveRomSize(n)` whose message names
  the size. Pending until the drive's next power-on (`set_power(true)`, `power_on_reset`); a reset keeps the ROM. `load_rom(dir)` keeps its file names
  and now goes through `set_rom` (so a 32 K file loads too).
- **Unit.** `set_unit(8..=11)`, anything else refused by name. Latched into `unit` at
  the next reset; the jumper bits `read_prb` returns and the bus slot follow `unit`.
  `drive_peek($1800)` now includes the jumper bits too (it had left them out; 0 for
  unit 8, so nothing moved).
- **Accessors.** `ram() -> &[u8]`, `ports() -> DrivePorts` (VIA1 PA/PB-as-read/PB-out,
  VIA2 PA/PB out, PCR, motor, LED, step phase, density, write mode), `half_track()`,
  `powered()`, `reset_held()`, `stopped()`, `reset_line_connected()`, `unit()`,
  `unit_jumpers()`, `part() -> DrivePart`. Write mode is `PCR bit 5 clear`, the bit
  `via2d_update_pcr` reads. VIA2 PB is the composed `oldpb`, the byte the mechanism
  acts on; `led_on` is the existing accessor unchanged.
- **Checkpoint.** A new `drivePart` node (`DrivePart`, camelCase). A checkpoint without
  it restores `DrivePart::default()` — the stock drive — not whatever the live machine
  had. The IEC device map is adopted without touching the restored lines.
- **Daemon.** Untouched; it compiles and behaves as before. No wire verbs (871).
- **`cia_alarm_check_gate` goldens re-recorded.** Its digests hash the whole checkpoint
  tree, so the new node moves every one of them. With `drivePart` left out of the
  capture all twenty digests were the old ones — checked before re-recording — so the
  machine did not move; only the checkpoint says more (the 843 precedent).

**Gate.** `crates/trx64-core/tests/drive_part_gate.rs`, seven tests over the real KERNAL
and DOS, each answered through `LOAD"$",n`: off → DEVICE NOT PRESENT and on → power-on
with the disk kept (§8.1/8.2); held (§8.3); reset line connected / cut (§8.4); ROM 16 K,
32 K, bad size, file loader (§8.5); unit 9 (§8.6); stopped mid-transfer for 50 frames
(§8.8); checkpoint round-trip and old-checkpoint defaults (§8.9). Each was shown red
with its change taken out. §8.7: the 7-game gate 7/7 PASS with all seven screenshots
byte-identical to main's, the existing drive gates unchanged, `cargo test -p trx64-core`
616 passed / 0 failed, the daemon suite 381 passed.

