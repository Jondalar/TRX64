# Spec 871 — A second drive on the bus

**Status:** PROPOSED (2026-09-23)
**Repos:** TRX64. C64RE: no change in this spec — its media tools keep addressing drive 8
until a separate request asks for more.
**Number:** 871 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** Spec 870 (power, own reset, ROM from bytes, unit number). Without 870 a
second drive would be a second copy of everything 870 fixes.
**Origin:** the UE2 emulator, 2026-09-23. The U64 has two FPGA drives, A and B, each with
its own power, reset and device number. UE2 plays drive A with TRX64's drive 8; drive B
exists there as registers only.

---

## §1 What exists today

- **One drive.** `Machine` holds `drive8: Drive1541`, caught up to the C64 clock at every
  sync point and folded into the IEC lines.
- **The IEC core is already shaped for more.** `IecCore` carries per-unit arrays
  (`IECBUS_NUM` slots, `unit_type` per unit) exactly as VICE does; its own comment calls
  the current state "single-1541 shape: unit 8 = Drive1541, rest unused". The bus logic
  (wired-AND of every device's port, ATN to every device) is written for N devices and
  used with one.
- **The daemon refuses a second drive by name.** Media ingress rejects `drive9` / slot 9
  ("v1 drive8-only").

So the bus is ready; the machine has one drive to put on it.

## §2 D1 — Two drive positions, A and B

- The machine has **two drive positions**, A and B, as the U64 has. Each holds a complete
  1541: its own 6502, VIAs, RAM, ROM, disk, power, reset and unit number — everything
  Spec 870 gives a drive.
- **Defaults: A on at unit 8, B off.** With B off, nothing is clocked for it and nothing of
  it is on the bus, so every existing machine is exactly today's machine.
- Two and not four. VICE models four units; the U64 has two, and two is what is asked for.
  The IEC core keeps its slots for all four, so a later third is a position, not a
  redesign.

## §3 D2 — One bus, two devices

- Both drives are on the same IEC bus: both ports are folded into the lines (wired-AND),
  both see ATN edges (VIA1 CA1), both answer to their own unit number through the jumper
  bits of Spec 870.
- **Two powered drives with the same unit number are refused**, naming both positions. A
  real bus with two devices at one address produces collisions no program relies on;
  modelling the garbage buys nothing.
- **Clocking:** at every sync point both powered drives are caught up to the C64 clock
  first, and only then are both ports folded — so the order in which the two are advanced
  cannot change what the bus reads. Between the two drives the sync granularity is the
  same as between drive and C64 today (one C64 instruction, part-way on `$DD00`), which is
  also VICE's.

## §4 D3 — Media and surfaces

- Each position has its own disk. Mount, eject, persist and the dirty-media guard work per
  position, addressed **by unit number**, which is what a user types (`LOAD"$",9`).
- The daemon's drive-9 refusal goes; the wire gains a unit argument where it takes a drive
  today, defaulting to 8 so every existing caller keeps working.
- Monitor: `device drive8` / `drive9` (and whatever the unit number is) select the drive a
  monitor command inspects — the Spec 864 `Device` list offers what the machine has.
- Checkpoints carry both positions, and a checkpoint taken before this spec restores with
  B off.

## §5 D4 — Cost

A second drive that is off costs nothing — it is not clocked. A second drive that is on
roughly doubles the drive-emulation share of a frame. Measured and reported when built;
not optimised in this spec.

## §6 Scope — where this stops

- **In:** two 1541s on one bus, per-position media and power, per-unit addressing in the
  daemon and the monitor, checkpoints.
- **Out:** 1571 and 1581 in either position (their own specs — a position is a 1541 here);
  more than two positions; C64RE's media tools for drive 9 (a separate request); drive-to-
  drive fast copiers that need a parallel cable.

## §7 Acceptance

There is no existing drive-9 test in the corpus, so the tests are written here. The owner's
expectation, which the tests are built around: `$DD00` fastloaders are written for drive 8
and will almost always run only from 8 — some may run from 9 when the disk is booted from
9, if they take the device number from `$BA` instead of hard-coding it. Nothing in this
spec may change what happens on drive 8.

**Gate — must pass:**

1. **The 7-game gate with B on.** The existing 7-game screenshot gate (fastloaders from
   drive 8) run unchanged, and run again with B powered at unit 9 with a disk mounted and
   idle. Both 7/7. A second device on the bus must not disturb a transfer it is not
   addressed in. If a game fails only with B on, that is investigated before anything
   else: a real bus with a passive second 1541 on it does not break these loaders.
2. **Two disks, two drives.** A at 8 and B at 9, each with its own D64: `LOAD"$",8` and
   `LOAD"$",9` each list their own disk.
3. **KERNAL load and save on 9.** `LOAD"FILE",9` loads a PRG byte-identical to the one in
   B's image; `SAVE"NEW",9` writes a PRG that, after persist, is byte-identical in B's
   image, and A's image is unchanged.
4. **A copy between them.** A file copied from 8 to 9 with a plain BASIC loop
   (`OPEN 2,8,2,"F,S,R"` / `OPEN 3,9,3,"F,S,W"` / `GET#` / `PRINT#`) arrives byte-identical
   in B's image after persist.
5. **B off is today.** The 7-game gate and every drive gate byte-identical to main with B
   off (the default).
6. **Same number refused.** Powering B on at unit 8 while A is at 8 is refused, naming A.
7. **Checkpoints.** Two drives with disks mid-transfer round-trip through dump/undump; an
   older checkpoint restores with B off.

**Characterisation — recorded, not a pass/fail gate:**

8. **The seven games booted from 9.** A off, the game's disk in B at unit 9,
   `LOAD"*",9,1` and `RUN`. Recorded per game: does the KERNAL stage load, does the
   fastloader stage load, where does it stop. Expected: most stop at the fastloader
   because it talks to 8. Any game that boots fully from 9 is written down by name — that
   game becomes the regression test for a fastloader on a drive other than 8.
9. **Cost.** Frame time with B off and on, on the same workload.

## §8 Open

- Whether "position" should be visible on the wire at all, or only unit numbers. Unit
  numbers alone are enough for everything in §7; positions matter only to a host that
  mirrors the U64's A/B registers. Leaning: unit numbers on the wire, positions inside.
