# Spec 852 — The Ultimate Command Interface as U64 hardware

**Status:** BUILT 2026-09-16 — `uci_gate` 15/15, full gate green (67 gate tests, daemon 378, seven
games 7/7), core lib 295/0; a stock C64 pays nothing measurable (§8).
**Repos:** TRX64 (`trx64-core`, `trx64-daemon`). UE2 serves the firmware side through the API in D3.
**Number:** 852 (registry: `../../../C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** Spec 850 (the port hooks it sits on) and Spec 851 (the `u64` profile that contains it).
**Origin:** the owner, 2026-09-15, deciding where UCI lives: in TRX64, as part of the U64 machine, and
"kein Fake-CRT-Workaround". The UCI block is identical on every Ultimate: `command_intf` is built for
the `u64`, `u64ii`, `u2`, `u2plus` and `u2plus_L` firmware targets, against one open VHDL entity.

---

## §1 What the block is

The Ultimate Command Interface is register logic in the FPGA, between the C64 bus and the firmware.
Five C64-visible bytes (`$DF1B-$DF1F` by default), a 2048-byte dual-port RAM, and sixteen registers on
the firmware side. The firmware's targets (DOS, network, control, SoftIEC, HTTP) answer the commands;
the block only moves bytes, keeps the handshake state and drives two lines into the C64: IRQ and freeze.

It is a hardware fact of the U64, not something a consumer adds. So it lives in TRX64 on the `u64`
profile. UE2's S15 planned its own model of `command_protocol.vhd` in ue2-core; that model is this spec,
and S15's open question 9 ("where the state lives") is answered: here.

Without a firmware behind it — TRX64 standalone — the block exists and is disabled, which is the
firmware's own power-on default (`c64.cc:115`, "Command Interface" off). The window reads open bus,
as on a U64 whose menu has UCI off.

## §2 The port: `command_protocol.vhd`, one to one

Source: `firmware/1541ultimate/fpga/io/command_interface/vhdl_source/` — `command_protocol.vhd` (313
lines), `command_if_pkg.vhd`, `command_interface.vhd`. File: `trx64-core/src/u64/uci.rs`.

**State** (`command_protocol.vhd:65-89`): `enabled`, `slot_base` (6 bits), `bus_id` (bits 4:0 settable,
7:5 read 0), `irq_mask` (3), `command_pointer`, `response_pointer`, `status_pointer`, `response_length`,
`status_length` (11 bits), `freeze`, `trigger`, `cmd_irq_en`, and the status byte: b7 response valid,
b6 status valid, b5:4 state, b3 error, b2:0 handshake_in (abort, data accepted, new command). RAM 2048
bytes: command 0-895, response 896-1791, status 1792-2047 (`command_if_pkg.vhd:33-41`).

**Decode** (`:108`, `:141`): the block answers when address bits 8:3 equal `slot_base` and `enabled`.
IO1/IO2 qualify the access (`slot_slave.vhd:143-144`), which 850 guarantees by calling only for
`$DE00-$DFFF` with I/O banked in. Register = bits 2:0 (`command_if_pkg.vhd:27-31`).

**C64 reads** (`:97-106`), combinational from address and state:

| bits 2:0 | value |
|---|---|
| 3 bus id | `bus_id` |
| 4 control | status byte |
| 5 command | `$C9`, `$49` while the IRQ is active |
| 6 response | `RAM[response_pointer]` if response valid, else `$00` |
| 7 status | `RAM[status_pointer]` if status valid, else `$00` |
| 0-2 | `$FF` |

Side effects after the value is taken (`:174-189`): a read of 6 clears `cmd_irq_en` and advances
`response_pointer` (clamped at 1791); a read of 7 the same for `status_pointer` (clamped at 2047). Byte
available or not.

**C64 writes** (`:141-173`): 5 stores `RAM[command_pointer]` and advances it (clamped at 895). 4:
b3 clears error; b0 PUSH latches freeze ← b7, trigger ← b6, `cmd_irq_en` ← b5, and moves state 00 → 01
with new-command set, or sets error if the state was not 00; b1 DATA_ACC in a data state records
data-accepted (only for "more"), leaves the data state and clears `cmd_irq_en`; b2 sets abort.

**Valid flags** (`:131-140`) are registered on the FPGA clock: response valid = pointer below length AND
data state AND no abort; the same for status. At C64 granularity they are recomputed before every
access.

**Firmware side** (`:197-290`, offsets from `CMD_IF_BASE`): the table in UE2's
`docs/hw/11-drives-iec-periph.md` §UltiCommand is the contract, ported exactly — including the quirks
it documents (`STATUS_LENGTH` reads the pointer's low byte, `RESPONSE_LEN_L/H` read the pointer, writes
to 4/5 are mask set/clear while reads return the buffer bounds).

**Lines** (`:109`, `:310-311`): C64 IRQ = state(1) AND `cmd_irq_en`; NMI never. Firmware IRQ =
(handshake_in AND NOT `irq_mask`) ≠ 0, a level. Freeze = `freeze`.

**Trigger** (`:192-195`): a write to `$FF00` while `trigger` is set sets freeze and clears trigger.

**Reset** (`:292-306`): only the FPGA system reset. In TRX64 that is `Machine::new` and a power cycle
(Spec 786). A C64 warm or cold reset leaves the block alone and ERROR survives it.

## §3 Design

**D1 — on the port.** On the `u64` profile the machine owns a `Uci` and serves it through 850's device
interface, before any host device. On a read its answer wins over the cartridge's I/O data
(`slot_slave.vhd:292-301`: `reg_output` is tested first). Writes go to both, as on the hardware
(`slot_slave.vhd:105`).

**D2 — the lines through 850.** IRQ on `INT_SRC_EXPANSION`, sampled per cycle (850 D6). Freeze is
`Hold::Cpu` (850 D7): entered at the instruction boundary after PUSH with b7 or after the `$FF00`
trigger, released when the firmware's validate or reset clears `freeze`. The `$FF00` write arrives
through 850's snoop (D5), including the RMW dummy write — the FPGA sees every write cycle
(`slot_slave.vhd:142`).

**D3 — the firmware side, as an API.** A host that runs the firmware maps `CMD_IF_BASE` onto:

```rust
impl Uci {
    pub fn fw_read(&self, off: u16) -> u8;            // 0x000-0x7FF registers (decode is bits 3:0, so they
                                                      // repeat every 16 bytes), 0x800-0xFFF RAM, above reads 0;
                                                      // no side effects. See §8.
    pub fn fw_write(&mut self, off: u16, value: u8);
    pub fn fw_irq(&self) -> bool;                     // ITU low bit 4, a level
    pub fn take_events(&mut self) -> UciEvents;       // since the last call
}
pub struct UciEvents { pub c64_reset: bool, pub unlock: bool }
// Machine::uci() / uci_mut() -> Option<&(mut) Uci>, Some on the u64 profile.
// uci_mut() also hands the block any pending C64 reset before returning it: it QUEUES
// that event (take_events reports it), it never consumes one. So a caller that only
// wants set_routed transfers the reset too, and loses nothing by it.
```

A firmware write that changes `freeze` or the IRQ takes effect at the next instruction boundary, which
is when a host between runs is able to write anyway.

**D4 — the two events the firmware needs.**
- `c64_reset` — set by `warm_reset`/`cold_reset`. The firmware learns about a C64 reset through ITU low
  bit 7 and then aborts a pending command (`command_intf.cc` `ResetInterruptHandlerCmdIf`). TRX64 does
  not raise ITU bits; it says the reset happened.
- `unlock` — `$AB` written to `$D038`, then `$CD` to `$D036` (`megabyter.tas`, "exact order, nothing in
  between"). The firmware's `unlock_irq` then maps the internal bus, enables UCI at `$DF1C` and clears
  `$D038` (`u64_config.cc:1012-1019`). The rule is modelled as: the next C64 write after `$D038 = $AB`
  is `$D036 = $CD`, any other C64 write re-arms. Only `Cpu` and `Dummy` writes count: a host write is the
  firmware's DMA while the C64 runs and is not on the C64 bus, so it neither advances nor re-arms the
  sequence. What the closed core accepts in between is not known.
  Standalone TRX64 raises the event and does nothing else: without a firmware nothing enables the block,
  and a UCI that answers `$C9` with no server behind it would hang every program that probes it.

**Neither event is a level, and TRX64 raises no interrupt for either.** `take_events` drains both
(`uci.rs`, a `mem::take`), so each is delivered exactly once and nothing here re-raises it. The only
level this block computes is `fw_irq()`, the command handshake. A host that models the ITU — where the
unlock arrives as a high IRQ with no ack register — therefore owns both raising that level and dropping
it again; UE2 drops it on the firmware's write of 0 to `$D038`, which is UE2 policy, not a fact about
the hardware.

**D5 — stretched reads.** `slot_slave.vhd` latches an I/O read per PHI2 cycle from R/W and IO1/IO2
(`:131-142`). During a BA stall those are active only while AEC is high; once AEC falls the VIC drives
the address and the PLA selects no I/O. So a read of 6 or 7 advances the pointer `stalled_on_bus + 1`
times (850's count), not once per stolen cycle — which would be about forty on a badline. Whether the
U64's internal bus behaves like the U2+ cartridge is closed and stays an assumption.

> **DISPROVED 2026-09-17, fixed in 0.7.2.** The assumption above was wrong, and it said of itself
> that it was an assumption. The pointer advances by exactly ONE per completed C64 read.
>
> UE2 found it with UBoot64 v3.0.1: a UCI DOS read of a 24480-byte `DMBSLT.CFG` delivered 24279
> bytes, the C64 re-issued `DOS_CMD_READ_DATA` without a new `OPEN_FILE`, the file was at EOF, the
> firmware answered zero-length and the C64 waited for ever. The stop byte moved every run — 24261,
> 24270, 24279, 24285, 24289, 24324 — which is the badline dependency in plain sight. Verified by
> advancing once per read in a scratch copy: UBoot64 reaches its menu.
>
> The reasoning needs no VHDL. A BA-stretched read is ONE 6502 bus cycle: address and R/W are held
> across the stretch, the data is taken at its end. One completed read consumes one byte. Any other
> model loses bytes on every badline, and real Ultimates do not lose them. The contract is the one
> UE2 named: the count the C64 reads equals the length the firmware validated.
>
> The gate case that should have caught this asserted `stalled_on_bus + 1` — the code's own model —
> so it confirmed the defect. Its replacement is a difference test: screen on and screen blanked
> must deliver the same bytes. Third time this repo has learned that an absolute assertion cannot
> catch a consistently wrong count.

**D6 — visibility.** A read-only monitor verb `uci`: profile, enabled, window, state, pointers, lengths,
the two lines. The daemon's `session/uci` returns the same. Without it, a hung UCI program is a black box.

**D7 — snapshots.** The block is not in the checkpoint ring, rewind or `.c64re` dumps. Its other half is
the firmware's RAM and task state, which live in the host; restoring one half gives a handshake neither
side remembers. A restore resets the block to power-on (disabled), which is correct standalone and a
stated limit under UE2, where rewind is not used.

## §4 Gates

Driven like `command_intf.cc` from the test (`HANDSHAKE_*` values `command_intf.h:45-53`), C64 side
through the CPU bus:

- **Disabled:** `u64` profile, power-on: `$DF1B-$DF1F` read the open bus; the `c64` profile has no block.
- **Identify:** `fw_write(1, 1)`, `fw_write(0, 0x47)`: `$DF1D` reads `$C9`, `$DF1C` state 00.
- **Command round trip:** C64 writes 3 bytes to `$DF1D` and `$01` to `$DF1C` → `fw_irq()` true, command
  length 3, RAM holds them; firmware writes `ACCEPT_COMMAND`, response and status bytes into RAM, lengths,
  `VALIDATE_LAST` → `$DF1C` b7/b6 set, state 10; C64 reads the response from `$DF1E` while b7, the status
  from `$DF1F` while b6; `$02` to `$DF1C` → state 00.
- **More data:** `VALIDATE_MORE`, DATA_ACC → state 01, data-accepted set, `fw_irq()` true.
- **Busy:** PUSH in state 01 sets ERROR; `$08` clears it.
- **Clamps:** 900 command bytes stop at 895; response reads past 1791 stay there.
- **IRQ:** PUSH with b5, validate → the 6510 takes the IRQ; `LDA $DF1E` in the handler drops it, no
  second IRQ after RTI.
- **Freeze:** PUSH with b7 → the CPU is held, `$D012` runs; `VALIDATE_LAST` releases it at the same PC.
- **Trigger:** PUSH with b6, then `STA $FF00` → held; `INC $FF00` triggers too.
- **Windows:** slot base `0x07` answers at `$DE1C`, `0x7F` at `$DFFC`.
- **Cartridge beside it:** a cartridge serving I/O data in the window loses to the UCI on reads; both see
  the writes.
- **Reset:** `warm_reset` during a pending command keeps state and ERROR and sets `c64_reset`; a power
  cycle clears the block.
- **Unlock:** `$AB → $D038`, `$CD → $D036` sets `unlock`; with a write in between it does not.
- **Stretched read:** `LDA $DF1E` timed onto a badline advances the pointer `stalled_on_bus + 1` times,
  never once per stolen cycle. — **Replaced 2026-09-17 (0.7.2): this case asserted the defect.** Two
  cases now. A stretched read advances the pointer by exactly ONE, still driven one instruction at a
  time over two frames, because only a read whose stall had AEC HIGH can tell the two models apart and
  those are rare (7 in 5309 under D5's own measurement). And the contract: a 200-byte transfer that
  crosses badlines delivers the same bytes with the display on and blanked. The second deliberately does
  not require the port read itself to be the stretched access — a badline's steal is taken by the FIRST
  access on the line, and the loop's opcode fetches come from RAM, so the read carrying it is almost
  never the one at `$DF1E`. A guard demanding otherwise cannot be met; this one asserts the raster lines
  the transfer actually crossed.

## §5 UE2

- ue2-core maps `0x10044000-0x10044FFF` to `fw_read`/`fw_write` through a new `C64Backend` method,
  recomputes ITU low bit 4 from `fw_irq()` after every C64 run and firmware access, raises ITU low bit 7
  on `c64_reset` and high IRQ 6 on `unlock`, and reports `CAPAB_COMMAND_INTF`.
- The planned ue2-core model of `command_protocol.vhd` (S15 §3 item 1) and the bridge window decode
  (item 5) are not built; S15 steps 1-2 disappear.
- `C64_BUS_INTERNAL`/`EXTERNAL` routing stays in the bridge (it is the U64 bus multiplexer, and the
  bridge already models it for `--cart-slot`); it decides whether TRX64's window is on the bus, through
  `Uci::set_routed(io1: bool, io2: bool)` — bits 0 and 1 of `C64_BUS_INTERNAL`, which gate IO1 and IO2
  separately (`c64.cc:1536-1590`; `unlock_irq` sets bit 1 "to reach UCI", `u64_config.cc:1015-1016`). D1
  applies the bit of the range the window decodes to: IO1 for `$DE18`, IO2 for `$DF18`/`$DFF8`. The
  bridge feeds it from core-config offset `0x2B`. Standalone TRX64 routes both.
- The profile goes in at construction: `Machine::set_machine_profile(u64)` right after `Machine::new()`,
  before power-on. `CAPAB_COMMAND_INTF` is reported only when `uci()` is `Some`.

## §6 Not in this spec

- The command targets. They are firmware and run unmodified in UE2.
- An UCI server inside TRX64 (a DOS target on host files, say). Possible later; it would be a firmware
  replacement, and none exists.
- The exact U64 answers to D4's unlock rule and D5's stretch count, which only the hardware knows.

## §8 As built

**Where it lives.** `crates/trx64-core/src/uci.rs` — `trx64_core::uci::{Uci, UciEvents, UciStatus}` —
rather than §2's `src/u64/uci.rs`: the core has no `u64` module, and the profile's other parts live in
`vic.rs` and `c64_6510core.rs`. `lib.rs`: `set_speed_profile` → `sync_profile_device` installs and
removes the block; `uci`, `uci_mut`, `uci_status`, `reset_uci_to_power_on`; the reset flag `cold_reset`
raises. `c64re_snapshot.rs`: `restore_runtime_checkpoint` resets the block. Daemon: the monitor verb
`uci`, the RPC `session/uci`, and the undump re-installing the block for a `u64` dump. Gate:
`crates/trx64-core/tests/uci_gate.rs` (15 tests covering every §4 case), in `scripts/gate.sh` step
[2/4]; daemon test `the_uci_block_is_visible_read_only_and_a_restore_resets_it`.

**What the build settled against §2-§3.**
- **The C64 reset event.** No reset calls a device (850), so `cold_reset` — and `warm_reset`, which goes
  through it — raises a flag on the machine, and `Machine::uci_mut()` hands it to the block before
  returning it: `m.uci_mut().unwrap().take_events()` reports it exactly as D3 has it, and `uci_status()`
  shows it without taking it. Booting raises it too (`boot_from_dir` runs `cold_reset`): a power-on
  resets the C64. A freshly installed block has heard of no reset.
- **The profile owns the block through `set_speed_profile`.** The daemon's `do_power_on` and `turbo mode`
  both go through it, so a power cycle builds a new block; entering `u64` again keeps the block that is
  there; leaving the profile removes it. The undump sets the profile claim directly to keep the restored
  VIC registers (851) and so came back *without* a block — it now calls `sync_profile_device`.
- **D7 in one place.** Every restore — `checkpoint/restore` and the transport, the monitor's `sd`,
  scenario seeds, `snapshot/undump`, `trx64cli sandbox --seed` — ends in `restore_runtime_checkpoint`,
  which resets the block to power-on. The host's routing (`set_routed`) survives it: that is the U64 bus
  multiplexer's state, not the block's.
- **The valid flags are computed where they are looked at** — the control register, the `$DF1E`/`$DF1F`
  data mux, `fw_read(3)` — which is what the FPGA has registered long before the C64's next access, and
  what lets `fw_read` take `&self`.
- **Stretched reads.** The CPU gets the byte at the pointer as it stood before the read; the pointer then
  advances `stalled_on_bus + 1` times, clamped. Which byte the U64 latches at the end of a stretch is part
  of D5's assumption. — **Corrected 2026-09-17 (0.7.2): it advances by exactly ONE per completed read.
  See the DISPROVED note under D5; `stalled_on_bus` no longer has a production consumer, and stays only
  as 850's `Access` contract, which `expansion_port_gate` still checks the producer against.**
- **What counts as a C64 write.** The unlock detector sees only what the device is shown: the snooped
  `$FF00`/`$D036`/`$D038`, whatever the banking, and `$DE00-$DFFF` with I/O banked in. A write elsewhere
  between the two keys — to RAM, say — is invisible and does not re-arm; megabyter writes nothing in
  between. `Host` writes count for neither the unlock nor the `$FF00` trigger (the FPGA takes the
  trigger from a C64 write cycle, `slot_server_v4.vhd:863`). A host access to the register window does
  act — `write_full($DF1D)` stores a byte, a `sidefx on` read advances the pointer — because a live
  monitor access asks for exactly that.
- **The firmware window** decodes as `command_interface.vhd` splits it: 0x000-0x7FF are the sixteen
  registers, repeating every 16 bytes (their decode looks at address bits 3:0 only), 0x800-0xFFF the RAM,
  anything above reads 0 — not only D3's 0x000-0x00F. `bus_id` is not in the VHDL reset block;
  `Uci::new` starts it at 0, as the FPGA configuration does.
- **D6** is one `UciStatus` behind both the verb and the RPC. Bare `uci` reports; `uci <anything>` refuses
  and says the firmware side is an API.
- Two first drafts of the gate were wrong and the block right: the identify probe's own `LDA $DF1E`/`$DF1F`
  had already advanced both pointers (byte available or not), and the firmware ISR's flags include the
  state bits, of which `IRQMASK_SET` uses only 2:0.

**Measured.** Over two frames of `LDA $DF1E / JMP` with the display on: 5302 reads unstalled and 7 on a
badline, each 43 stolen cycles with 3 on the bus — each advanced the pointer 4 times, not 44 (**that
behaviour is the defect, corrected in 0.7.2: it advances ONCE. The stall figures themselves still
hold — see the DISPROVED note under D5**). Full gate
green: 67 gate tests, daemon 378, seven games 7/7; core lib 295/0; `trx64-cli` 109/0. `perf_bench` pure
headless, 30 M cycles, median of seven, the branch and the `b99a639` baseline alternated in three rounds:
branch 11.225 / 11.211, 11.170 / 11.266, 11.240 / 11.119 MHz; baseline 11.366 / 11.368, 11.107 / 11.153,
11.233 / 11.252 MHz. Means 11.205 against 11.247 (−0.4 %), while the baseline alone drifted 2 % between
rounds and the sign flipped from round to round: noise. Nothing on a `c64` machine's run path changed — the
block exists only on `u64`, and `cold_reset` sets one flag.
