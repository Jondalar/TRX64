# Spec 870 — The drive as a part: power, reset, ROM and unit number

**Status:** PROPOSED (2026-09-23)
**Repos:** TRX64 only. C64RE: no change.
**Number:** 870 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`).
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

## §4 D3 — The ROM is a value

- The ROM is given as bytes: **16 KiB** (placed at `$C000`) or **32 KiB** (the whole
  `$8000-$FFFF`, for the ROMs that use it — JiffyDOS, SpeedDOS and friends ship as 32 K on
  some boards). Any other size is refused by name.
- The file loader stays as a convenience on top of it and keeps its current names.
- Changing the ROM of a powered drive takes effect at its next reset — the CPU is not
  swapped under a running program.

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

- **In:** power, own reset, reset-line connection, ROM from bytes (16 K / 32 K), unit
  number 8-11, read accessors — for the one drive that exists today.
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
8. **Checkpoints.** Power, held, connection and unit number survive dump/undump.

## §9 Open

- Whether a checkpoint taken with a drive off should carry the drive's (meaningless) RAM,
  or omit it. Omitting is smaller; carrying is simpler to restore. Decide when building.
- The drive's power-on RAM pattern: VICE fills it; whether the real 1541 pattern matters
  to any loader is unmeasured. Use VICE's until a disk says otherwise.
