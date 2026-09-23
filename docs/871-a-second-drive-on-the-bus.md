# Spec 871 — A second drive on the bus

**Status:** BUILT (on branch spec-871-second-drive, not merged)
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

- ~~Whether "position" should be visible on the wire at all, or only unit numbers.~~
  Decided with the leaning: **unit numbers on the wire, positions inside.** Every daemon
  surface takes a unit; `DrivePosition` exists only in the core API (and in the refusal
  text, which names the position so a U64-mirroring host can map it to its A/B
  registers).

## §9 As built (2026-09-23, branch `spec-871-second-drive`, on top of 870 incl. d5c66b0)

**Where it lives.** `Machine::drive_b` beside `Machine::drive8`; `drive8` is position A
and keeps its name (documented as historical — A may stand at unit 8-11). B is built by
`Drive1541::new_position_b()`: off, jumpers at 9 (`DrivePart::default_for(B)`), same
sync factor as A. `IecCore` carries `drive_slot` (A) and `drive_slot_b` (B);
`sync_drive_slots(a, b, pa)` / `adopt_drive_slots(a, b)` build the device map
`iecbus_status_set` would for the true drives present — Conf1/Conf2 for one drive at 8/9,
Conf3 for two (or for 10/11), Conf0 for none. The one-drive `sync_drive_slot` /
`adopt_drive_slot` remain as wrappers with B off.

**One bus, two devices (D2).** Every sync point goes through three free functions in
`drive.rs`: `pair_catch_up` feeds both drives the bus as it stands (`feed_iec`: `drv_port`,
`cpu_bus` and every slot's `drv_bus`), runs A, runs B, and only then writes both ports
back; `pair_fold_into_iec` folds both; `pair_deliver_atn` hands each ATN edge the IEC core
computed (per unit, as VICE's conf3 loop does) to the drive at that unit. Used by the
`$DD00` read/write paths in `full.rs` and by the end-of-instruction catch-up in `lib.rs`
(`Machine::catch_up_drives`). One `drive_c64_ref` serves both: they are always advanced
together. **B off costs two flag tests** per sync point: not fed, not run, not folded.

- **Decided, not stated:** each drive's own `v_iecbus` now carries the other devices'
  pulls (copied into the slots other than its own before it runs). VICE's `via1d1541`
  `store_prb` folds against the ONE global `iecbus`; TRX64 gives each drive its own copy,
  and without the copy a drive's `$1800` store would compute the lines as if it were
  alone. On a one-drive machine every other slot is released (`0xff`), which is what the
  copy already held — byte-identical, and the 7-game screenshots say so.
- **Refusal.** `Machine::set_drive_power(pos, on)` refuses switching on when the other
  position is powered at the unit this drive's jumpers would bring it up at;
  `Machine::set_drive_unit(pos, unit)` refuses a powered drive's jumpers onto the other
  powered drive's unit. The message names both: "drive position B cannot answer to unit
  8: position A is powered at unit 8". The drive's own `set_power`/`set_unit` do not
  know the other position; if a collision arises through them anyway, B stays off the
  bus (`pair_bus_slots`). The daemon is stricter, see below.
- **RESET line** reaches both (`warm_reset` → `reset_from_c64` for A and B).
- **ROM (870 d5c66b0, merged).** B follows the power-on rule: `boot_from_dir` gives B the
  same DOS and runs `power_on_reset()` for B only when B is on with the machine; off, the
  DOS waits in `rom_next` until `set_power(true)`. A checkpoint restore that switches B on
  (it was off before the restore) is B's power-on for the ROM (`latch_rom`, now
  `pub(crate)`).

**Checkpoints.** A `driveB` node — `drivePart`, the drive core blob, B's disk as mounted
(kind, bytes, backing path, read-only: the host keeps no record of B's medium, so the
image rides) and the GCR overlay. **Omitted while B is as built** (off, no disk, stock
part), so every checkpoint of a one-drive machine is the tree it was — the
`cia_alarm_check_gate` digests did not move. No node restores B as built (detach, part
default). The capture reads B from a clone (`capture_drive1541` re-syncs VIA clocks).

- **Found and fixed on the way:** `restore_drive1541` left a never-run drive's pending
  hardware reset armed (`reset_pending` + `IK_RESET` survive `cold_reset` until the first
  catch-up, and the DRIVECPU module does not carry the interrupt status that would
  overwrite them). The first catch-up after the restore then ran the reset sequence over
  the restored CPU (clock to 6 under a restored `stop_clk` of millions) — the restored
  machine hung. It hit B whenever B was off until the restore, and position A too when a
  checkpoint is restored into a freshly booted machine that has not run (measured: the
  same hang with one drive). `snapshot_clear_pending_reset` after every drive restore.

**Media and surfaces (D3) — the daemon.** The drive-9 refusal is gone. A unit comes from
`unit`, else a `slot` of 8-11, else a `role` of `driveN` (or a bare number); default 8.
The position is `Machine::position_for_media(unit)`: the powered drive at that unit, else
a drive that is off with its jumpers there. `media/ingress`, `media/mount`, `media/swap`,
`media/unmount`, `media/persist`, `session/drive_status` take it; the session's
`disk_path` stays A's, B's medium is its image's backing path. The dirty-media guard is
cart-only and unchanged. Auto-persist runs for B's disk as for A's (`DiskAutoPersist`
per position). A C64 power cycle (cart insert/eject, `session/power`) keeps B's disk,
jumpers and power (`Session::inserted_disk_b`, `drive_b_state`); a project switch
persists and ejects B's disk like A's.

- `session/drive_power {unit?, on?}` — with `on`: `set_drive_power` (refusal as above);
  without: the press it was, `power_on_reset()` of the drive at `unit`.
- `session/drive_unit {unit, to}` (new) — set the jumpers of the drive at `unit`.
  **Stricter than the core:** refused, naming the other position, when the other position
  is at `to` even while off — on the wire a drive is addressed by its unit, so two
  positions may not share one at all. Swapping A and B therefore goes through a third
  unit.
- `session/state` gains `drives: [{unit, powered, disk: {path}|null}]` (A first; the unit
  of an off drive is its jumpers) and `device.drive<unit>` for B when it is on.
- `session/read_memory` reads `space: "drive<unit>"`; `drive8` still reads A when nothing
  is powered at 8.
- Monitor forwards: `drive [unit]`, `drivepower [unit] [on|off]`, `eject <unit>`.

**Monitor.** `Device::Drive8` became `Device::Drive(unit)`; `MonitorHost::devices()` now
takes `&mut self` and by default offers the C64 plus `drive<unit>` for every POWERED
position — "the drives the machine has on its bus" (decided: an off drive is not
offered). `device drive9` → `r`/`m`/`d` read that drive; a drive selected and later
switched off is said so instead of answering for the C64. Address spans carry the unit.
The UE2 host needs a one-word change if it overrides `devices()`.

**Not extended to B (unchanged, A only):** the `drive8-cpu` trace domain and the head
trace (Spec 784), the `iec` monitor verb's drive column, and the VICE `.vsf` export.

## §9.1 Acceptance, as run

`crates/trx64-core/tests/second_drive_gate.rs` (8 run by default, 6 ignored heavy),
daemon tests in `main.rs` (`batch1_tests`), monitor `minimal_host.rs`.

1. **7-game gate.** B off (unchanged gate): **7/7 PASS**, all seven screenshots
   byte-identical to 870's (which match main's). The B-off verdict does not look at the
   picture: PASS is game code sustained in RAM or >= 8 colours on screen, and the PNGs
   are only diffed against the previous run as a note in `scripts/gate.sh`. A game that
   reaches its code with a wrong picture passes it.

   **With B on (`GATE_DRIVE_B=9`)** the first build of this gate reported 7/7 PASS by
   that same criterion, although Green Beret never drew its title and MOTM showed a
   corrupt bitmap — a false green. Corrected: each game now runs B off and then B on in
   the same process, and the B-on run is judged by its PICTURE against the B-off frame of
   the same build, pixel for pixel. Result, as run: **7/7 as expected — 4 byte-identical
   (polarbear, impossible2, lastninja, maniac), 3 expected to differ**, each named in
   `b_on_expected` with its reason; a game expected to differ that matches fails too.
   - **Green Beret** — differs everywhere (58 931 px). Its loader, read from RAM:
     `$0380 LDA #$0A / STA $DD00` asserts ATN, `$0385 LDA $DD00 / BPL $0385` waits for DATA
     high, `$038A LDA #$02 / STA $DD00` releases ATN, then four `LDA $DD00` 2-bit reads.
     ATN is its request line — measured, ~2 000 ATN edges per emulated second while it loads.
   - **Murder on the Mississippi** — the title bitmap arrives corrupt (30 029 px). Its
     in-game loader asserts ATN too (C64 `$4270`-`$4290`), a few times per block;
     measured, e.g. at C64 cycle 30 398 888 drive 9 answers (DATA pulled, B in its ATN IRQ
     at `$FE68`) and pulls DATA again 300 cycles later from `$E9AD`.
   - **Scramble** — not byte-identical either, which the first write-up missed (it said
     "title intact"). It loads its first file through the KERNAL; every command byte under
     ATN is received by drive 9 as well and the C64 waits for the slower listener. First
     divergence from the B-off run at C64 cycle 24 540 206 (C64 at `$ED23` vs `$ED33`,
     drive 9 in its ATN code at `$E8EF`). The game runs the same, later; the frame
     differs in 388 px, all inside the blinking "Loading" label (x 298-351, y 239-248),
     caught in the other phase. The gate allows a difference only inside that box.

   **Cause:** a 1541 answers every ATN — in hardware first (the ATN-acknowledge gate pulls
   DATA the instant ATN falls, before any software runs), then in its DOS ATN routine
   (VICE iecbus.c conf3, via1d1541.c store_prb: nothing isolates an idle drive). A second,
   idle 1541 therefore pulls DATA whenever a loader raises ATN, and stretches every KERNAL
   handshake it takes part in. This is the 1541's circuit and DOS as ported from VICE, not
   something 871 adds. The spec's expectation "a real bus with a passive second 1541 on it
   does not break these loaders" holds for five of the seven, byte-identical for four; for
   Green Beret and MOTM the modelled physics says it does break them.
   - **Also found:** B switched on 0.8 s before a `LOAD` (its DOS still in its power-on
     routine) made all seven hang in the KERNAL (`$ED5A`/`$EEA9`): an ATN that falls
     before the DOS has set VIA1's CA1 edge is never serviced, and the ATN-acknowledge
     gate holds DATA from then on. A drive is switched on with the C64 or given ~1 s; the
     gate and the tests do that.
2. **Two disks, two drives** — `two_drives_list_their_own_disks`: pass (Conf3 in force).
3. **KERNAL load and save on 9** — `kernal_load_and_save_on_nine`: `LOAD"FILE",9` equals
   B's PRG byte for byte; `SAVE"NEW",9` is byte-identical in B's image after the
   write-back; A's image unchanged. Pass.
4. **A copy between them** — `basic_copy_from_eight_to_nine`: every byte value incl.
   CHR$(0), 300 bytes, 8 → 9 through `GET#`/`PRINT#`; byte-identical SEQ in B's image,
   source unchanged. Pass.
5. **B off is today** — the B-off 7-game screenshots above; the drive gates
   (`drive_part_gate`, `drive_write_readback_gate`, …) unchanged and green;
   `b_off_is_not_clocked_and_not_on_the_bus` (B's clock stays 0, Conf1, no `driveB`).
6. **Same number refused** — `the_same_unit_twice_is_refused_naming_the_other`, and on the
   wire `two_positions_at_one_unit_are_refused_naming_the_other`. Pass.
7. **Checkpoints** — `two_drives_mid_transfer_round_trip_a_checkpoint`: mid-copy (both
   files open) through a `.c64re` container into a freshly booted machine; the restored
   state equals the captured one (C64 RAM, both drive RAMs, all clocks and PCs, bus map),
   and the restored run finishes the copy byte-identical. **Not asserted: cycle-lockstep
   continuation** — it does not hold for one drive either (measured: one-drive restore
   parts after 50 frames, two drives after 450), because TRX64's DRIVECPU module leaves
   out the drive CPU's interrupt status, which VICE's `drivecpu_snapshot_write_module`
   writes (`interrupt_write_snapshot`). Pre-existing; a checkpoint-format change, not 871.
   `an_older_checkpoint_restores_with_b_off`: pass. Also
   `position_b_takes_its_rom_at_its_own_power_on` (870 rule for B).

**§7.8 — the seven games from drive 9** (A off, B at 9, `LOAD"*",9,1` + `RUN`, 100 M
cycles, judged as the 7-game gate judges; `characterise_the_seven_games_from_nine`):

| game | KERNAL stage | fastloader stage | where it stops |
|---|---|---|---|
| scramble | loads (chains on) | **boots** — title + game live; drive 9 runs code in its RAM, head moves | in the game |
| polarbear | loads (chains on) | **boots** — game live, same picture as from 8; drive 9 runs code in its RAM | in the game |
| motm | loads (chains on) | no — drive 9 never runs code in its RAM, head never moves | C64 `$EA0E` (IRQ over a blank screen); its loader talks to 8 |
| greenberet | loads (chains to its loader at `$0385`) | **boots** — title rendered (15 colours); drive 9 runs code in its RAM | title |
| impossible2 | loads the first file (to READY, end `$0314`) | no — the next file is never found; drive 9 idle | "SEARCHING FOR IMP", READY |
| lastninja | loads (chains on) | **boots** — title + game live; drive 9's head moves, no drive code in its RAM | in the game |
| maniac | loads (chains to `$0406`) | no — drive 9 never runs code in its RAM, head never moves after RUN | C64 waiting at `$0406`; its loader talks to 8 |

Four boot fully from 9 — **scramble, polarbear, greenberet, lastninja** — and each is now
a regression test (`from_nine_*`, ignored like the 7-game gate). Green Beret boots from 9
alone and breaks with an idle drive 9 beside drive 8: its loader takes the unit from `$BA`,
but it signals with ATN, which every drive on the bus answers.

**§7.9 — cost** (`characterise_the_cost_of_a_second_drive`, release, `LOAD"*",8,1` of
scramble, 1500 frames, best of 3): **B off 1.661 ms/frame** (B's drive clock stays 0),
**B on at 9, idle with a disk, 1.834 ms/frame — ×1.10**. An idle second drive costs ~10 %
of a frame here, less than "doubles the drive share" because the DOS idle loop is cheap.

**Shown red** with the change taken out, each test then green again: B not caught up
(gates 2/3/4/7 and 6's load), B's port not folded (same), ATN not delivered to B (gate 2),
B built on (gate 5), no collision check (gate 6), no `driveB` capture (gate 7), an older
checkpoint leaving B as it is (gate 7b), the restore not latching B's ROM (gate 7), the
pending reset kept on restore (gate 7 hangs), `set_power` not latching (B-ROM test); on
the wire: the drive-9 refusal put back, `devices()` offering drive 8 only (daemon and
`minimal_host`), `drive_unit` without its refusal, B not carried over a power cycle.

**Suites.** `cargo test -p trx64-core --no-fail-fast`: **624 passed, 0 failed, 57
ignored** (870: 616; +8 `second_drive_gate`). `cargo test -p trx64-daemon`: **389 passed,
0 failed** (870: 381; +4 tests, compiled into both the lib and the bin target). Monitor
golden (with `TRX64_ROM_DIR`) re-blessed for the three help lines only. Two probes (`drive_sector_read`,
`gcr_sync_probe`) booted a bare `Drive1541` with `cold_reset` and ran an empty ROM since
870's d5c66b0 — red on the 870 branch too; they now boot with `power_on_reset()`.

