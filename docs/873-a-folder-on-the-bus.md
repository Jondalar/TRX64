# Spec 873 — A folder on the bus: an IEC device backed by a host directory

**Status:** PROPOSED (2026-09-23)
**Repos:** TRX64. C64RE: no change in this spec — its media tools keep addressing disks.
**Number:** 873 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`; the board still
says "next free 872" and does not list this spec yet).
**Depends on:** Spec 870 (the drive as a part: power, reset line, unit 8-11), Spec 871 (two
drive positions on one bus, units on the wire, the refusal of two devices at one unit).
**Enables:** game ports whose files live in a host folder and load through the stock KERNAL
from a device that needs no 1541 drivecode. A later spec may open disk images inside the
folder (§12).
**Origin:** the owner, 2026-09-23. A target for ports: a game that loads from this device
loads through the serial protocol only, so a port that runs from it has provably shed its
drivecode. It must live on the same line-level bus as the 1541s — timing the C64 sees must
be plausible and it must sit beside a 1541 on another unit. **Not a KERNAL trap.**

---

## §1 What exists today (v0.8.8)

Re-checked against the code:

- **The bus has a slot for a device that is not a drive, and nothing ever sits in it.**
  `iec.rs` carries VICE's device classes — `IECBUS_DEVICE_IECDEVICE` (`iec.rs:106`),
  `IECBUS_STATUS_IECDEVICE` (`iec.rs:93`) and the full `iecbus_device_index` table
  (`iec.rs:200-217`) — but the map is only ever built by `sync_drive_slots` /
  `adopt_drive_slots` (`iec.rs:856-907`), which know `TRUEDRIVE` and `NONE`.
- **The wired-AND already folds every slot 4-11.** `iec_update_ports` ANDs `drv_bus[4..12]`
  into `cpu_port` (`iec.rs:366-380`), and `iecbus_device_write(unit, data)` writes one slot
  and re-folds (`iec.rs:728-737`). A device that writes its own `drv_bus[unit]` is on the
  bus without any change to the fold.
- **Conf3 has an empty chair.** VICE's conf3 read/write call `serial_iec_device_exec(clock)`
  after the drives; TRX64's say it is "a no-op in the 1541 shape" (`iec.rs:553-559`,
  `iec.rs:569-571`). Conf3 is what the map selects as soon as anything other than one true
  drive at 8 or 9 is present (`iec.rs:655-674`).
- **Each 1541 sees the other devices through a copy.** `feed_iec` hands a drive `drv_port`,
  `cpu_bus` and every slot's `drv_bus` before it runs (`drive.rs:694-698`); `pair_catch_up`
  feeds, runs both positions, and writes their outputs back unfolded (`drive.rs:1655-1685`).
  A third device's pull reaches a drive only if it is in the IEC core before that feed.
- **Sync points.** The drives are caught up at the end of every C64 instruction
  (`Machine::catch_up_drives`, `lib.rs:2819-2829`) and part-way on `$DD00` reads and writes
  (`full.rs:529-548`, `full.rs:700-727`). Nothing else observes the lines.

So the bus is ready for a third kind of device. What is missing is the device.

## §2 What the references do

### VICE — yes, a line-level device beside true drives

VICE has two non-drive paths. The **virtual device** is a KERNAL trap (`serial/serial-trap.c`)
and is not on the bus. The **IEC device** is: `serial/serial-iec-device.c` is a line-level
slave, enabled per unit 4-11 by the `BusDeviceN` resources
(`serial/iec-ieee488-shared.c:89-107`), which call `iecbus_status_set(IECBUS_STATUS_IECDEVICE,
unit, 1)` (`serial/serial-iec-device.c:59-78`). In the device map the IEC-device bit
outranks a true drive at the same unit (`iecbus/iecbus.c:493-510`), and any IEC device puts
the bus on conf3 (`iecbus/iecbus.c:432-463`), whose read and write run
`drive_cpu_execute_all(clock)` then `serial_iec_device_exec(clock)` (`iecbus/iecbus.c:353-370`).
So VICE does put a folder on the line-level bus with true drive emulation on — the answer
to "does it, or why not" is: it does, through conf3.

- **The state machine** (`serial-iec-device.c:241-743`): `P_PRE0..P_FRAMEERR1` plus flags
  `P_ATN / P_LISTENING / P_TALKING`. ATN falling pulls DATA ("I am here") and ignores the
  bus for 100 µs (`:279-290`, `:387-394`); a received primary not addressed to it and not
  `$3F`/`$5F` sends it to `P_DONE0` — lines released until ATN rises (`:497-504`). EOI is
  detected when CLK stays high 200 µs after ready-for-data, acknowledged with 60 µs of DATA
  (`:412-438`). Talker: turnaround 80 µs (`:552-568`), 60 µs per half-bit (`:625-673`), 1 ms
  for the frame ack, then a frame-error sequence (`:674-740`). Timeouts are C64 cycles via
  `US2CYCLES` from the machine's clock rate (`:229-237`, set at `c64/c64.c:1348`).
- **It only moves when the C64 touches `$DD00`.** `serial_iec_device_exec` has exactly two
  callers, the conf3 read and write (`iecbus.c:356`, `:369`); each call advances at most one
  state. On a write it runs *before* `iec_update_cpu_bus`, so it sees the new ATN at the
  C64's next access.
- **DOS above it** (`serial/fsdrive.c`, `fsdevice/`): the file named after `OPEN` (`$Fx`) is
  opened at the following UNLISTEN (`serial-iec-device.c:360-366` → `fsdrive.c:249-270` →
  `serialcommand` `$F0`, `fsdrive.c:141-184`). Errors are `"%02d,%s,%02u,%02u\r"`, power-on
  message `73,VICE FS DRIVER V2.0` (`fsdevice/fsdevice.c:105-155`).
- **M-W is accepted, M-E answers OK.** `fsdevice_flush` routes `M-R/M-W/M-E`
  (`fsdevice/fsdevice-flush.c:632-655`) to `vdrive_command_memory_write`, which writes a
  fake 32 K RAM (`vdrive/vdrive-command.c:3520-3544`), and `vdrive_command_memory_exec`,
  which logs "needs TDE" and returns `00, OK` (`vdrive-command.c:3678-3688`). A program
  that uploads drivecode is told it worked.
- **Names and types.** Raw host files are all listed as PRG under their full host name
  (`fsdevice-read.c:419-427`, type from the caller, `fileio/cbmfile.c:130-138`); P00
  headers carry a type. Names over 16 chars become 14 chars + counter + `/`
  (`fsdevice-filename.c:50-146`). Blocks `(len+253)/254` capped at 65535
  (`fsdevice-read.c:510-518`); the last line is always `65535 BLOCKS FREE.`
  (`fsdevice-read.c:604-616`). Listing order is `readdir` order.
- **Snapshot.** Only the FSDRIVE name buffer and its length (`fsdrive.c:376-455`). The line
  state machine has no snapshot module, and open files are not saved.

### Ultimate — a hardware slave with a software DOS behind FIFOs

- **Line level in microcode.** `software/io/iec/iec_code.iec` runs on an FPGA IEC processor
  ticked at 1 MHz (`fpga/fpga_top/ultimate_fpga/vhdl_source/ultimate_logic_32.vhd:1211`,
  `tick_1MHz`); waits are in µs. ATN is an interrupt vector: DATA pulled at once, 20 µs,
  wait CLK low (`iec_code.iec:133-143`) — **every ATN is answered**, as a 1541 does. The
  address check is done on the first **7** bits (`:480-490`, `CHECK_ATN_BYTE` `:70-131`) so
  the 8th-bit window can detect JiffyDOS (`:492-509`). Not for us: release both lines, wait
  for ATN high (`:165-168`).
- **Timings** (`iec_code.iec:3-11`, used in `TX_BYTE` `:273-334`, `RECEIVE_BYTE` `:416-440`):
  bit low/high 80/80 µs, 40 µs before the first bit, 90 µs between bytes, talk turnaround
  wait 200 µs and 80 µs `Tda`, EOI detection window 1475 µs, frame ack 1 ms.
- **Flow control is honest.** As listener it keeps DATA low until the upstream FIFO has
  room (`:421`); as talker it holds CLK until software has pushed the next byte (`READB`,
  `:255`). The protocol allows a device to take as long as it needs at those two points.
- **Slots by patching.** Up to 4 slave slots; the device numbers are patched into the
  microcode's compare instructions (`iec_interface.cc:83-126`, `configure` `:128-145`).
  The software task turns FIFO control codes into `push_ctrl` / `push_data` / `talk`
  (`iec_interface.cc:165-336`).
- **DOS** (`iec_drive.cc`, `iec_channel.cc`, `cbmdos_parser.cc`): channels 0-14 plus a
  command channel 15 (`iec_drive.cc:157-160`); a command is executed at the EOI of its
  bytes (`SLAVE_CMD_EOI` → `push_command(0x00)`, `iec_drive.cc:328-330`,
  `iec_channel.cc:1837-1866`), trailing CR dropped, 64-byte command buffer
  (`iec_channel.h:348`, `iec_channel.cc:1303-1310`). Bus id 8-30, default 11
  (`iec_drive.cc:26-30`). Power-on `73,U64HD ULTIMATE DOS V2.0` (`iec_drive.cc:79`),
  reset closes channels and returns to `/` (`iec_drive.cc:292-309`).
- **M-anything is refused.** `M` goes to `dir_command`, which accepts only `MD`; `M-W`,
  `M-E`, `M-R` answer `ERR_UNKNOWN_CMD` = 33 (`cbmdos_parser.cc:333-352`,
  `cbmdos_parser.h:65-68`). Block commands on a folder answer a custom `78,BLOCK ACCESS
  DENIED` (`iec_drive.cc:84`, `iec_channel.cc:1334-1406`).
- **Names and types.** `.PRG/.SEQ/.USR/.REL` decide the type and are cut off; anything
  else is listed with its extension as SEQ (`iec_channel.h:109-140`,
  `iec_channel.cc:766-771`); a save appends the extension (`iec_channel.cc:1312-1331`).
  Names truncate to 16. Blocks `(size+253)/254` capped 9999 (`iec_channel.cc:800-804`);
  BLOCKS FREE is the file system's real free space (`iec_channel.cc:746-751`, `:899`).
  Subdirectories list as `DIR`; `CD`, `CD_`, `MD`, `RD` supported
  (`iec_channel.cc:1430-1496`); partitions are named host paths (`iec_drive.cc:163-168`);
  the file manager can descend into a disk image and show its CBM volume label
  (`iec_channel.cc:911-918`).

### Where they part

| | VICE IEC device | Ultimate Software IEC |
|---|---|---|
| clocking | advanced only on C64 `$DD00` accesses, one state per access | free-running 1 MHz microcode |
| address check | after 8 bits | after 7 bits (JiffyDOS window) |
| bit timing | 60/60 µs, 80 µs turnaround | 80/80 µs, 40 µs lead, 90 µs gap |
| EOI detect | 200 µs | 1475 µs |
| open executes at | UNLISTEN | EOI of the name |
| M-W / M-E | accepted, `00, OK` | refused, `33` |
| unknown extension | PRG, name incl. extension | SEQ, name incl. extension |
| BLOCKS FREE | always 65535 | real free space |
| long names | 14 + counter + `/` | cut at 16 |
| same unit as a true drive | the IEC device wins | n/a (separate bus id) |
| snapshot | name buffer only | none |

## §3 D1 — A device, not a drive

- A **folder device** is a third kind of thing on the bus beside the two drive positions of
  871: no CPU, no VIA, no disk, no ROM. It is a timed state machine that reads the three
  lines and pulls CLK and DATA, and a DOS that serves a host folder.
- It stands at **one unit, 8-11**, given when it is attached. Numbers 4-7 and 12+ are not
  offered (the 1541 positions stop at 11 as well, 870 §5).
- **Attached means powered.** Detached, its slot is released (`drv_bus = 0xff`) and the map
  loses it — exactly the 870 "off" rule.
- **Several may be attached**, one per free unit. Only one is needed for the owner's case;
  the map has four slots and the cost is per attached device.
- **A folder device and a powered drive at the same unit are refused**, naming the drive's
  position, as 871 refuses two drives. VICE lets the IEC device win (§2); that is a
  configuration no real bus has.

## §4 D2 — On the bus: map, clock, order

- **Map.** `sync_drive_slots` / `adopt_drive_slots` grow a third input: the units that
  carry a folder device. Such a slot is `IECBUS_DEVICE_IECDEVICE`; the callback index then
  selects conf3 as VICE's does. With no folder attached the map is today's, byte for byte.
- **Clock.** The device is advanced at **every sync point the drives have** — end of each
  C64 instruction and part-way on `$DD00` — not only on `$DD00` accesses as in VICE. Its
  state machine is event-driven on the C64 clock: at a sync point to cycle *T* it runs every
  transition whose condition holds, each stamped at the later of its due time and the input
  change that enabled it, and may take several steps between two sync points. The C64 still
  observes only at `$DD00`, so it cannot see the difference from VICE except where VICE
  collapses several due steps into one per access.
- **Order at a sync point:** (1) feed and run both drive positions with the device's current
  pull in the bus they are fed (`feed_iec`), (2) write their outputs back, (3) run the
  device up to *T* against the lines as they now stand, (4) write its pull into its slot,
  (5) one `iec_update_ports`. On a `$DD00` write the device sees the new C64 lines at the
  write cycle (VICE sees them one access later — a divergence taken on purpose: the
  device's reaction latency, not the sampling, decides when it answers).
- **Its pull reaches the 1541s at their next feed**, the same granularity 871 gives
  between A and B.

## §5 D3 — The line-level slave

States as VICE names them, with the timing profile of §6. What it does:

- **ATN falls:** abort whatever it was doing (talking, listening, idle), release CLK, pull
  DATA within `t_atn_ack`, ignore the lines for `t_atn_settle`. **Every ATN is answered**,
  addressed or not — VICE (`serial-iec-device.c:279-290`) and the Ultimate
  (`iec_code.iec:133-137`) both do, as does a 1541 in hardware. Consequence, stated now:
  like an idle second 1541 (871 §9.1), an attached folder device will stretch or disturb
  loaders that use ATN as a request line (Green Beret, MOTM). That is the modelled physics,
  not a defect.
- **Bytes under ATN:** received with the listener handshake, acknowledged with DATA. After
  the first byte: `$20+u` LISTEN, `$40+u` TALK, `$3F` UNLISTEN, `$5F` UNTALK are taken;
  any other primary means "not for me" — release both lines and wait for ATN high. The
  second byte is the secondary: `$60+ch` data, `$E0+ch` CLOSE, `$F0+ch` OPEN.
- **ATN rises:** as VICE's `P_ATN` rising branch (`serial-iec-device.c:291-381`) — LISTEN
  enters listening (unless the channel's OPEN failed, which leaves the C64 without a
  listener: its own error path), TALK enters the turnaround, UNLISTEN after an OPEN
  executes the open (§7), CLOSE closes; neither listening nor talking → both lines
  released.
- **Listener:** ready-for-data by releasing DATA; EOI when CLK stays high `t_eoi_detect`
  (not under ATN), acknowledged with `t_eoi_ack` of DATA; 8 bits clocked on CLK rising;
  frame acknowledged with DATA. **Flow control:** DATA stays low while the DOS is not ready
  for the next byte — in emulation the DOS is always ready, so the hold is zero, but the
  state exists so a future slow path (a host read that is deferred) does not need a new
  protocol.
- **Talker:** after the turnaround the device pulls CLK, waits for ready-for-data, sends
  8 bits, waits up to `t_frame_ack` for the frame acknowledge, signals EOI before the last
  byte by holding CLK released until the listener's EOI handshake. A missing frame ack
  runs VICE's frame-error sequence (`:711-740`). Nothing to send (file not found) → stop
  talking without a byte, which the KERNAL reports as FILE NOT FOUND (`:599-605`).
- **UNTALK / UNLISTEN / ATN release** leave the lines released. **An unaddressed device
  never holds a line after ATN rises.**
- **C64 reset (the IEC RESET line):** all channels closed, uncommitted writes dropped,
  current directory back to the root, status `73` — VICE `fsdrive_reset` +
  `serial_iec_device_reset` (`fsdrive.c:347-365`, `serial-iec-device.c:146-162`) and the
  Ultimate's `IecDrive::reset` agree. Whether the line is connected follows the 870 rule
  (default connected).

## §6 D4 — Timing profile

One table, C64 cycles derived from µs with the machine's clock (PAL/NTSC, Spec 863), as
VICE's `US2CYCLES`. **Default = VICE's numbers**: they are the ones proven against the stock
KERNAL on the VICE bus TRX64 ports. The Ultimate's are shown because they are a real
device's and run against the same KERNAL.

| name | default (VICE) | Ultimate | what |
|---|---|---|---|
| `t_atn_ack` | at the next evaluation | immediate (vector) | ATN low → DATA low |
| `t_atn_settle` | 100 µs | 20 µs | ignore lines after ATN falls |
| `t_eoi_detect` | 200 µs | 1475 µs | listener: CLK high this long = EOI |
| `t_eoi_ack` | 60 µs | 70 µs | listener: DATA pulse acknowledging EOI |
| `t_turnaround` | 80 µs | 80 µs (`Tda`) | talker: CLK low → first ready-to-send |
| `t_bit_low` / `t_bit_high` | 60 / 60 µs | 80 / 80 µs | talker: per bit |
| `t_byte_gap` | 0 | 90 µs | talker: after an acknowledged byte |
| `t_frame_ack` | 1000 µs | 1000 µs | talker: wait for the listener's ack |

- The profile is **data, not code**, and carried in the checkpoint, so a test can run the
  device at the slow edge and a host can choose the Ultimate's numbers.
- **The KERNAL's margins are to be read off the ROM at build**, not taken from a timing
  chart: device-present after ATN, the frame-ack wait as talker, and the EOI timeout as
  listener are CIA-timer loops in the KERNAL, and the gate (§11.9) asserts the device's
  worst case against the numbers read there. Orientation only, not yet read from code:
  the published serial-bus timing gives ~1 ms for the first two and ~200-250 µs for the
  third.

## §7 D5 — The DOS surface

**Channels.** 0 = LOAD (read, PRG default), 1 = SAVE (write, PRG default), 2-14 data
channels, 15 command/status. A name's `,S` `,P` `,U` and `,R` `,W` `,A` suffixes set type
and mode; `@0:` / `@:` replaces an existing file. The open executes at the UNLISTEN that
follows the name (the 1541's and VICE's point, §2), not at its EOI. One file per channel;
an OPEN on a channel in use closes the old one first. Command buffer 58 bytes as the
1541's; longer → `32,SYNTAX ERROR`. A trailing CR is dropped.

**Status.** `nn,TEXT,tt,ss\r`, read on channel 15; reading it once resets it to
`00, OK,00,00`. Power-on and reset: `73,TRX64 FOLDER DOS V1.0,00,00` — an honest name, not
`CBM DOS V2.6 1541`: a program that detects a 1541 by this string should not find one.

**Commands** on channel 15 (and as the name of `OPEN 15`):

| command | answer |
|---|---|
| `S:name[,name…]` (wildcards) | `01, FILES SCRATCHED,nn,00` |
| `R:new=old` | `00`; `62` old missing; `63` new exists |
| `CD:dir`, `CD/dir/`, `CD_`, `CD:_` (← parent), `CD//` (root) | `00`; `39` not found |
| `MD:dir` / `RD:dir` | `00`; `63` exists / `63` RD not empty; `62` missing |
| `I`, `V` | `00, OK` (nothing to initialise or validate) |
| `UI`, `UJ`, `U:` | `73` power-on message, channels closed |
| `N:…` | refused `31` (a folder is not formatted) |
| `C:new=old…` | refused `31` in this spec (§12) |
| **`M-W`, `M-E`, `M-R`, `B-E`, `U3`-`U8`** | **refused `31,SYNTAX ERROR`**, nothing stored, nothing run |
| `B-R`, `B-W`, `U1`, `U2`, `B-A`, `B-F`, `B-P`, `#` buffer open | refused `31` — there are no blocks |
| `P` (REL position), anything else | `31` |

There is no drive CPU, so memory commands are refused rather than answered `OK` as VICE
does: a port that still uploads drivecode must fail here, visibly, not hang later waiting
for code that never ran. The refusal is also an event on the daemon side (`unit 9 refused
M-E $0500`) so the human sees which command a port still sends.

**LOAD specials.** `"$"` = directory as a BASIC program (§8). `"*"` alone = the device's
boot file if one is set at attach, else the first PRG in listing order; `"name*"` = first
match. `"0:name"` accepted (drive 0). `$:pattern` and `$:pattern=P|S|U|R|D` filter.

**REL files** are listed (`.rel`) but opening one answers `64,FILE TYPE MISMATCH` — REL is
out of this spec (§12).

## §8 D6 — Host file mapping

- **Types from the extension** (the Ultimate's rule): `.prg .seq .usr .rel`,
  case-insensitive, give the type and are not shown. Any other file is listed **as PRG with
  its extension in the name** — a raw host file (`intro`, `title.bin`) loads with
  `LOAD"INTRO",9,1`, which is what a port needs; the Ultimate's SEQ would refuse it. A save
  writes `name.prg` / `.seq` / `.usr`. Subdirectories list as `DIR`. Dot-files, symlinks
  pointing outside the root, and anything neither file nor directory are not listed.
- **PETSCII ↔ host.** `$41-$5A` ↔ `a-z`, `$C1-$DA` ↔ `A-Z`, `$20-$40` and `$5B-$5D` as
  their ASCII selves except `/`; every other byte, and `/` and `%`, as `%XX` hex on the host.
  The mapping is total and reversible, so any name the C64 can write, the host can hold, and
  reading it back gives the same bytes. Lookups compare case-insensitively on letters, so a
  case-sensitive host and a case-insensitive one resolve the same name to the same file;
  two host names that differ only in case: the first in listing order wins.
- **16 characters.** Names are shown and matched on their first 16 PETSCII characters.
  Two host files with the same first 16: both listed, an open takes the first in listing
  order. No VICE-style counter marker — it makes names the user never typed.
- **Wildcards:** 1541 semantics — `?` one character, `*` matches the rest and ends the
  pattern.
- **Listing order is sorted** by host name bytes, not `readdir` order. Both references use
  `readdir`, which differs between hosts and runs; a sorted order makes `"*"` and a
  directory the same bytes everywhere.
- **Directory format** as a 1541's BASIC listing, load address `$0401`: header line
  `0 ␒"<folder, last 16 chars>" 00 2A`, one line per entry
  `<blocks> "<name>" <type>` with blocks `(size+253)/254` capped 65535, splat `*` never
  (there is no unclosed file on a host), `<` for a file the host will not let us write,
  lines padded to the 32-byte entry length both references keep for old programs
  (`fsdevice-read.c:592-599`), last line `<n> BLOCKS FREE.`.
- **BLOCKS FREE is the host volume's real free space** in 254-byte blocks, capped 65535, and
  never the constant 65535 VICE prints: free space that is not there is not reported. It is
  sampled when the directory is opened and becomes part of the listing bytes (§9).
- **Paths stay inside the root.** `CD` cannot leave the attached folder; `CD_` at the root
  answers `39`.

## §9 D7 — Writes, checkpoints, restore

The checkpoint must reproduce the device. A host folder is outside the machine and changes
under it, so the device keeps everything a restore needs **inside its own state**, and the
host is touched in two places only: reads when a file or directory is opened, writes when
the device persists.

- **Reads.** Opening a file for read reads the whole file into the channel (limit 16 MiB,
  larger → `52,FILE TOO LARGE`). Opening a directory builds the complete listing bytes at
  once. From then on the channel serves from its buffer; a checkpoint taken mid-LOAD
  carries the buffer and the position, and a restore finishes the LOAD without the host.
- **Writes** collect in the channel and are committed at CLOSE into the device's
  **overlay**: the set of changes since attach — files written, scratched, renamed,
  directories made and removed — each written file with its bytes, each overwritten or
  scratched file with its **before-image**. Reads go through the overlay first, then the
  host. A write never closed (reset, detach, power) is dropped; nothing half-written
  exists anywhere.
- **Persist** makes the host folder equal *attach-time folder + overlay*: it writes the
  overlay's files and applies its scratches, renames and directories. It runs on the same
  triggers as a disk's auto-persist (debounced after a commit, on detach, project switch,
  session close — the daemon's `DiskAutoPersist` model) and on request.
- **A checkpoint carries:** unit, root path (as information), timing profile, line state
  machine (state, flags, byte, primary/secondary, pending timeouts as absolute cycles, its
  current pull), per channel (mode, type, name, buffer, position, EOI flag, status), the
  command buffer, the status string, the current directory, the boot file, the read-only
  flag, and the overlay. Buffers and overlay files go into the content-addressed pool the
  ring already keeps for disk images (`checkpoint_ring.rs`, Spec 714.4), so the ring's 50
  captures a second do not copy a folder each time.
- **Restore** puts all of that back and re-binds the root path. A restore that drops an
  overlay entry makes the next persist **undo it on the host** — delete a file the device
  created, put back a before-image it overwrote or scratched. That is what a rewound disk
  image does to its host file today (the whole image is rewritten), carried over to a
  folder. The device only ever deletes or rewrites files **it** wrote; a host file it
  never touched is never touched.
- **Absent → as built.** A checkpoint without the node restores with no folder attached; a
  machine with no folder attached writes no node — every existing checkpoint and the
  `cia_alarm_check_gate` digests stay as they are (the 871 rule).
- **Read-only attach** (`read_only: true`): every write, scratch, rename, MD/RD answers
  `26,WRITE PROTECT ON`. Default is writable, as disks are.

## §10 D8 — Daemon and core surface

- **Core:** `folder_device.rs` holds the state machine and the DOS. It reads the host only
  through a `FolderSource` trait (list, stat, read) the host supplies, and writes nothing:
  the overlay is handed out by `take_persist_plan()` and applied by whoever owns the disk.
  `Machine::attach_folder(unit, source, opts)` / `detach_folder(unit)` with the refusal of
  §3. The daemon owns host I/O, as it owns disk persistence.
- **Wire:** `device/folder_attach {unit, path, read_only?, boot?}`,
  `device/folder_detach {unit}`, `device/folder_persist {unit}`; `session/state` gains
  `folders: [{unit, path, read_only, dirty}]`. `media/*` keeps meaning disks.
- **Monitor:** a folder device has no CPU and is not a `Device` to select. The `iec` verb
  gains its slot in the per-device column (who is holding the line), and a `folder [unit]`
  verb prints its protocol state, open channels and last status.
- **Cost:** no folder attached = one flag test per sync point. Attached and idle = one
  state-machine evaluation per sync point that does nothing while ATN is high and it is
  neither talker nor listener. Measured when built.

## §11 Acceptance

**Gate — must pass:**

1. **Directory.** Folder at 9 with a known set of files, subdir and odd names:
   `LOAD"$",9` produces exactly the expected listing bytes (header, sorted entries, block
   counts, `DIR`, BLOCKS FREE line — the free count taken from the same host query).
2. **KERNAL load.** `LOAD"FILE",9,1` byte-identical to the host file; `LOAD"*",9,1` loads
   the boot file, and without one the first PRG; a missing name → `?FILE NOT FOUND`.
3. **KERNAL save.** `SAVE"NEW",9` → after persist `new.prg` byte-identical on the host;
   `SAVE` over an existing name → `63,FILE EXISTS`; `SAVE"@0:NEW",9` replaces.
4. **Command channel.** Power-on status `73`; `S:`, `R:`, `CD`/`MD`/`RD`, `UI` each with
   the §7 answer and the expected host result after persist; `M-W` + `M-E` answer `31`,
   store nothing, and the device is still usable after.
5. **SEQ and a copy.** `OPEN 2,9,2,"F,S,W"` / `PRINT#` / `CLOSE`, read back with `GET#`, all
   256 byte values; and 871's 8 → 9 BASIC copy with a D64 in drive 8 and the folder at 9,
   byte-identical after persist.
6. **Not present, not in the way.** Detached: `LOAD"$",9` → DEVICE NOT PRESENT. Attached at
   9 with the 1541 at 8: `LOAD"$",8` unchanged and, after every transaction on 8, the
   device's slot is released.
7. **A 1541 on 8 and the folder on 9 — the 7-game gate.** Unchanged gate with no folder:
   7/7, screenshots byte-identical to v0.8.8. Then each game again with an idle folder at
   9, judged by picture against its no-folder run as 871 §9.1 does; games expected to
   differ named in the gate with their reason (candidates: Green Beret and MOTM, whose
   loaders use ATN; Scramble, whose KERNAL stage every ATN listener slows) — a game
   expected to differ that matches fails too.
8. **Checkpoints.** Mid-LOAD from 9 and mid-SAVE to 9, through a `.c64re` container into a
   freshly booted machine: restored state equal, the restored run finishes byte-identical
   and in cycle lockstep for 500 frames with the straight run. A restore to before a SAVE
   and a persist leave the host without the file; to before a scratch, with it back.
9. **Timing against the KERNAL.** The margins read off the ROM (§6) asserted against the
   device's worst case; the gate's load/save tests pass again with the profile at its slow
   edge (the Ultimate's numbers).
10. **Nothing else moved.** Every existing drive and bus gate byte-identical with no folder
    attached; `cia_alarm_check_gate` digests unchanged.

**Characterisation — recorded, not pass/fail:**

11. **The seven games from the folder.** Drive A off, the game's files extracted into a
    folder at 8, `LOAD"*",8,1` / `RUN`. Per game: does the KERNAL stage load, where does it
    stop, and which refused command (`M-W`/`M-E`) it sent. Expected: every game with its
    own drivecode stops at the first `M-E`. That list is the port backlog this spec exists
    for.
12. **Cost** with no folder, an idle folder, and a folder serving a LOAD.

## §12 Scope — where this stops

- **In:** a line-level folder device at units 8-11 beside the two 1541 positions; the
  standard serial protocol; LOAD/SAVE, channels 0-15, the §7 commands, subdirectories;
  the host mapping of §8; the overlay, persist and checkpoint of §9; daemon and monitor
  surface.
- **Out:** every fast protocol — JiffyDOS (the device answers the JiffyDOS probe as a plain
  CBM device, so a JiffyDOS KERNAL falls back), the Ultimate's Warp, 1571/1581 burst over
  SRQ, parallel cables, SpeedDOS/DolphinDOS; REL files; `C:` copy; partitions (`CP`,
  `/`); CMD path prefixes in file names (`//DIR/:NAME`); P00/S00 containers; disk images
  opened as directories; the printer at 4/5; units outside 8-11; C64RE's media tools.

## §13 Open

- **Disk images inside the folder.** Shown here as plain files (a `.d64` lists as PRG
  `GAME.D64` and loads its raw bytes). The Ultimate descends into them
  (`iec_channel.cc:911-918`); VICE attaches an image to a unit instead of a folder. Leaning:
  a later spec — `CD:GAME.D64` opens it read-only through the D64/D81 reader the media code
  has, listed as `DIR`. Owner's call whether that is wanted at all for ports.
- **The refusal code.** `31,SYNTAX ERROR` is what a CBM drive says for a command it does
  not know, so a program branching on the number sees a stock code. The Ultimate uses 33
  for M-commands and a custom 76/78 elsewhere. Keep 31, or a distinct number a human spots
  in the status (e.g. `76,NO DRIVE CPU`)?
- **Persist after a rewind deletes files the device wrote.** §9 decides it as the disk
  analogue. The alternative — persist only forward, a rewind never removes a host file —
  is simpler and never deletes anything, at the price that the host folder and a restored
  machine disagree.
- **Default timing profile.** VICE's (decided above) or the Ultimate's, which is a real
  device's?
- **Boot file.** An attach-time `boot` for `"*"` is decided; whether the folder should also
  honour a marker file (so the choice travels with the folder) is not.
- **UE2.** The U64 firmware has its own Software IEC (§2). If UE2 wants it, the choice is
  between emulating the firmware's IEC processor (the firmware's DOS runs) and mapping the
  U64's Software IEC onto this device (TRX64's DOS runs). Not decided here; nothing in this
  spec prevents either.
