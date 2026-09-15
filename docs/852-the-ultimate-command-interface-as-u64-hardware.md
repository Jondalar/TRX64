# Spec 852 — The Ultimate Command Interface as U64 hardware

**Status:** PROPOSED 2026-09-15
**Repos:** TRX64 (`trx64-core`, `trx64-daemon`). UE2 serves the firmware side through the API in D3.
**Number:** 852 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`).
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
    pub fn fw_read(&mut self, off: u16) -> u8;        // 0x000-0x00F registers, 0x800-0xFFF RAM
    pub fn fw_write(&mut self, off: u16, value: u8);
    pub fn fw_irq(&self) -> bool;                     // ITU low bit 4, a level
    pub fn take_events(&mut self) -> UciEvents;       // since the last call
}
pub struct UciEvents { pub c64_reset: bool, pub unlock: bool }
// Machine::uci() / uci_mut() -> Option<&(mut) Uci>, Some on the u64 profile
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
  is `$D036 = $CD`, any other write re-arms. What the closed core accepts in between is not known.
  Standalone TRX64 raises the event and does nothing else: without a firmware nothing enables the block,
  and a UCI that answers `$C9` with no server behind it would hang every program that probes it.

**D5 — stretched reads.** `slot_slave.vhd` qualifies every PHI2 cycle, so on a U2+ a read of 6 or 7
stretched over n BA cycles advances the pointer n+1 times. The port does that, from 850's `stalled`
count. Whether the U64's internal bus does the same is closed; the gate records it so one measurement on
the owner's U64 settles it.

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
- **Stretched read:** `LDA $DF1E` timed onto a badline advances the pointer `stalled + 1` times.

## §5 UE2

- ue2-core maps `0x10044000-0x10044FFF` to `fw_read`/`fw_write` through a new `C64Backend` method,
  recomputes ITU low bit 4 from `fw_irq()` after every C64 run and firmware access, raises ITU low bit 7
  on `c64_reset` and high IRQ 6 on `unlock`, and reports `CAPAB_COMMAND_INTF`.
- The planned ue2-core model of `command_protocol.vhd` (S15 §3 item 1) and the bridge window decode
  (item 5) are not built; S15 steps 1-2 disappear.
- `C64_BUS_INTERNAL`/`EXTERNAL` routing stays in the bridge (it is the U64 bus multiplexer, and the
  bridge already models it for `--cart-slot`); it decides whether TRX64's window is on the bus, through a
  `Uci::set_routed(bool)` that D1 honours.

## §6 Not in this spec

- The command targets. They are firmware and run unmodified in UE2.
- An UCI server inside TRX64 (a DOS target on host files, say). Possible later; it would be a firmware
  replacement, and none exists.
- The exact U64 answers to D4's unlock rule and D5's stretch count, which only the hardware knows.
