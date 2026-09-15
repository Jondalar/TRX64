# Spec 850 — The expansion port as a device interface

**Status:** PROPOSED 2026-09-15
**Repos:** TRX64 (`trx64-core`). Its first user is TRX64's own UCI block on the `u64` profile (Spec 852);
a host such as UE2 (`u64-emulator/crates/c64-bridge`) may attach a device of its own. C64RE gains nothing.
**Number:** 850 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** nothing. With no device attached every hook is an `Option` that is `None`, so a stock
session stays bit-identical and pays one predicted branch (the BUG-049 discipline).
**Origin:** UE2's `docs/specs/trx64-uci-requirements.md` (R1–R6), written against trx64-core `69c9b30`.
HEAD is `a448229`, three Spec 843 commits later; none of them touches the lines that document cites
(re-read 2026-09-15).

---

## §1 Why an interface, not UCI

UE2 runs the Ultimate firmware and uses TRX64 as its C64. The firmware serves the Ultimate Command
Interface: five register bytes in `$DE00-$DFFF` whose reads have side effects, which work with or
without a cartridge, raise an IRQ, can hold the 6510 (freeze), trigger on a write to `$FF00`, unlock on
writes to `$D038`/`$D036`, and survive a C64 reset.

TRX64 knows one thing on the port: `Machine::cartridge`. With none attached, no host code runs for
`$DE00-$DFFF` at all (`full.rs` `io_read`, the `$DE00-$DFFF` arm, falls to the open bus since
Spec 840). UE2's only way in is a fake cartridge, and TRX64 then treats the machine as having one
everywhere — `BankInfo.cartridge_attached`, the VSF export lines, `cold_reset` calling its `reset`.

None of the six asks is specific to UCI. An REU (issue #19), an ACIA, a sampler, and the cartridge
IRQ UE2 currently parks on the RESTORE source are all *a device on the port*. So this spec builds the
interface, and nothing on it is a cartridge.

Its first device is TRX64's own. The owner decided (2026-09-15) that the U64's hardware lives in TRX64,
with no fake cartridge plugged in from outside: the UCI block is part of the `u64` profile (Spec 851)
and is specified in Spec 852. A host device — UE2's cartridge slot, a future REU — uses the same
interface.

## §2 The requirements, judged

| | Verdict | What changes against the request |
|---|---|---|
| R1 device beside the cartridge (MUST) | **accepted as asked** | — |
| R2 side-effect-free peek | **accepted** | Also fixes a TRX64 defect of its own: `peek_lens("cart")` says mappers "expose no side-effect-free peek yet", but `CartMapper::peek` exists (`cart.rs:362`) and neither `read_full` nor `peek_lens` calls it. |
| R3 stop request | **accepted** | New `RunStop::Device`. `run_for_full_capped` returns its `RunStop` instead of discarding it; callers that ignore it compile unchanged. |
| R4 write snoop | **accepted, through the same device** | Not a second registry. VICE has exactly this hook: `mainc64cpu.c:288-305`, `STORE` and `STORE_DUMMY` call `reu_dma` on `$FF00`. TRX64's core says it is "folded into the implementor" (`c64_6510core.rs:470-472`); it never was. |
| R5 interrupt sources | **changed: one source, not two** | VICE allocates one source per device (`c64cart.c:1424` "Cartridge", `reu.c:579` "REU"), and a source carries `IK_IRQ` and `IK_NMI` independently. One `INT_SRC_EXPANSION` does what two would. |
| R6 CPU hold | **accepted, plus a reset flavour** | The chips-only loop moves from the bridge (`c64-bridge` `run_chips`) into TRX64. The bridge needs "reset held" too, and it is a hardware fact, not a UE2 wish: the 6569 has no reset pin, so the VIC runs while CPU, CIAs and SID do not. |
| Open Q1 — stretched reads | **the device decides** | The FPGA counts every PHI2 cycle (`slot_slave.vhd:143-144`); VICE reads once after the BA steal. TRX64 hands the device the number of cycles stolen immediately before the read. What a stalled cycle *means* is the device's policy; *that* it stalled is the machine's fact. |
| Open Q2 — R1 or the fake cartridge | **R1** | The fake cartridge goes. |

## §3 Design

### D1 — `ExpansionDevice` (`trx64-core/src/expansion.rs`)

```rust
pub enum AccessKind { Cpu, Dummy, Host }

pub struct Access {
    pub addr: u16,
    pub clk: u64,
    pub kind: AccessKind,
    /// Cycles the BA steal took immediately before this read (0 for writes and host access).
    pub stalled: u32,
}

#[derive(Default, Clone, Copy)]
pub struct PortLines { pub irq: bool, pub nmi: bool, pub hold: bool }

pub trait ExpansionDevice: Send {
    /// A read of $DE00-$DFFF while I/O is banked in. `cart` is the cartridge's answer (None: it had
    /// none). Some(v) is what the bus sees; None keeps today's result — the cartridge byte, else the
    /// open bus.
    fn read(&mut self, a: Access, cart: Option<u8>) -> Option<u8>;
    /// Side-effect-free, for `read_full`, `peek_lens` and the monitor.
    fn peek(&self, addr: u16, cart: Option<u8>) -> Option<u8>;
    /// A write to $DE00-$DFFF while I/O is banked in, in addition to the cartridge. Does not change
    /// whether the cartridge consumed it.
    fn write(&mut self, a: Access, value: u8);
    /// Addresses outside $DE00-$DFFF whose writes the device sees whatever the banking (R4).
    fn snoop_addresses(&self) -> &[u16] { &[] }
    fn snoop_write(&mut self, _a: Access, _value: u8) {}
    /// What the device drives onto the port now. Sampled every cycle (D6) and at every boundary (D7).
    fn lines(&self) -> PortLines { PortLines::default() }
    /// One-shot: true once after an access that must end the run at the next boundary (R3).
    fn take_stop(&mut self) -> bool { false }
}
```

`Machine` gains a port with two places beside `cartridge`: the profile's own device (the UCI block on
`u64`, Spec 852) and `pub expansion: Option<Box<dyn ExpansionDevice>>` for a host. A read asks the
profile's device first, and its `Some` wins over the host device and over the cartridge — on the
hardware the UCI's register answer is tested before cartridge I/O data (`slot_slave.vhd:292-301`).
Writes and snoops reach both. A 65 536-bit snoop set is built from both devices' `snoop_addresses()`
when either changes. Attaching changes none of
`cartridge.is_some()`, `pla_index()`, `BankInfo`, the VSF export, or reset behaviour; `cold_reset` and
`warm_reset` never call the device (UCI survives a C64 reset, `command_protocol.vhd:292-306`).

`FullBus` carries `expansion: Option<&mut Box<dyn ExpansionDevice>>`, `snoop: Option<&SnoopSet>` and
`access_kind` at its four construction sites (`lib.rs` `write_full`, `read_full_live`, the run loop,
and the bus test in `full.rs`).

### D2 — reads and writes (R1)

- `io_read`, `$DE00-$DFFF` arm: `cart = cart_read(addr)`; then the device's `read(access, cart)`;
  `Some(v)` wins, else `cart`, else `vic.last_read_phi1`.
- `io_write`, same arm: `cart_write` exactly as today (the PLA re-run on a consumed write stays), then
  the device's `write`.
- Dummy reads and RMW dummy writes already go through `Bus::read`/`Bus::write`, so they reach the
  device with no extra site. `FullScBus::read_raw_dummy`/`write_raw_dummy` set `access_kind = Dummy`;
  `read_full_live`/`write_full` set `Host`.
- `stalled`: `FullScBus::check_ba` keeps the count it returns; the next `read_raw` passes it and resets
  it.

### D3 — peek (R2)

`read_full` and `peek_lens` ("io", "cart") for `$DE00-$DFFF`: `cartridge.peek` → `expansion.peek(addr,
cart)` → `vic.last_read_phi1`. No live read is ever made on a peek path.

### D4 — stop (R3)

After every device call `FullBus` asks `take_stop()` and latches it; the run loop ends at the next
instruction boundary with `RunStop::Device`, on the plain path and the debug path alike.
`run_for_full_capped` returns the `RunStop`.

### D5 — write snoop (R4)

At the top of `FullBus::write`, before banking is decided: an address in the snoop set calls
`snoop_write`. That single site covers the CPU's real write, the RMW dummy write-back (so `INC $FF00`
reports the old value, then the new one) and host writes, each with its `AccessKind`. The write itself
continues unchanged — `$D036` still reaches the VIC, `$FF00` still lands in RAM or the cartridge.

### D6 — interrupts (R5)

- `INT_SRC_EXPANSION = 4`; `C64_NUM_INT_SOURCES` 4 → 5.
- Per cycle: `C64Core6510Bus` gains `expansion_irq_line`/`expansion_nmi_line` (default `false`), sampled
  in `clk_inc` beside `cia1_irq_line`/`cia2_nmi_line`/`vic_irq_line` (`full_sc.rs`). A device line that
  falls on a `$DF1E` read is therefore seen at that cycle, and the 6510 applies the same delay rules as
  for a CIA.
- At the boundary the run loop refreshes source 4 next to 0–2.
- Host: `Machine::set_expansion_lines(irq, nmi)`, ORed with the device's lines. The cartridge IRQ/NMI
  UE2 puts on `INT_SRC_RESTORE` today moves here, and RESTORE is RESTORE again.
- Snapshots: `c64re_snapshot.rs` restores interrupt sources by NAME (`:550`, `:583`). The canonical list
  gains `"Expansion"`; a snapshot written before this has no such name and leaves the slot clear.

### D7 — hold (R6)

`Machine::hold: Option<Hold>`, `enum Hold { Cpu, Reset }`.

- The hold is the host's hold (`set_hold`) OR a device's `lines().hold`, checked at every instruction
  boundary — effective at the next one at the latest. Each side releases its own: the host by
  `set_hold(None)`, a device by dropping its line (the UCI block drops freeze when the firmware
  validates, Spec 852). For UCI an
  instruction-boundary stop is exact: PUSH_CMD and the `$FF00` write happen in the last cycle of their
  instruction.
- While held, the run loop advances per cycle without executing the 6510: the VIC always; CIAs, SID and
  drive 8 for `Cpu`, the VIC alone for `Reset`. `clk` advances, the CPU registers do not change, and
  `read_full_live`/`write_full` work.
- Port reference: VICE holds the CPU for DMA by stealing cycles (`mainc64cpu.c:122-125`,
  `MAINCPU_BA_LOW_REU`) and services `IK_DMA` at the boundary (`6510core.c:523-527`); `IK_DMA` is
  already defined in TRX64 (`c64_6510core.rs:115`) and unused. The bridge's `run_chips` is the same
  loop, re-implemented outside because this one did not exist.

## §4 Gates

All of R1–R6's acceptance cases, run through the CPU bus (815's lesson: a gate that pokes the chip
proves the wrong door), plus:

- **No device:** the seven-game gate and every existing gate pass unchanged, and
  `docs/perf-compare.md` stays within noise.
- **R1:** no cartridge, device attached, `$01=$37`: `LDA #$41 / STA $DF1D / LDA $DF1C` — the device sees
  the write and the read, A holds its byte. Device answers None: A is the open bus. `$01=$34`: no call.
  Cartridge and device: the device receives the cartridge byte and decides. `LDX #$1F / LDA $DEFF,X`: a
  `Dummy` read at `$DE1E`, then a `Cpu` read at `$DF1E`. `cold_reset`/`warm_reset`: no call,
  `cartridge` stays `None`.
- **R2:** 100 × `read_full($DF1E)` leave the device unchanged.
- **R3:** `STA $DF1C` (device requests a stop) then `INC $D020`: the run ends with PC at the INC and
  `$D020` unchanged, reason `RunStop::Device`.
- **R4:** `STA $FF00` one report; `INC $FF00` two, old value first; `STA $D036` reported and the VIC write
  still happens; an unregistered address reports nothing.
- **R5:** expansion IRQ asserted, `CLI`: taken with a CIA IRQ's delay. A handler that drops the line via
  `LDA $DF1E` gets no second IRQ after RTI. The RESTORE NMI and the expansion NMI are independent. A
  snapshot with source 4 set round-trips; a pre-850 snapshot restores.
- **R6:** hold `Cpu` for 19 656 cycles: `$D012` passes through every line, CIA1 timer A counts, PC and
  registers are unchanged, `write_full($0400, x)` lands; after release execution continues at the same
  PC. Hold `Reset`: the VIC runs, CIA1 timer A does not count.
- **Stalled read:** `LDA $DF1E` timed onto a badline reports `stalled > 0`; the same read off a badline
  reports 0.

## §5 What UE2 does with it

S15 steps 1 and 2 — the fake cartridge with no cartridge, the watch-table stops, the `$FF00`
access-watch — are not built, and neither is S15's own model of `command_protocol.vhd`: that is Spec 852,
inside TRX64. What the bridge uses from this spec directly: `Hold` for the firmware's C64 STOP (its
`run_chips` goes), `set_expansion_lines` for its cartridge's IRQ/NMI (the shared RESTORE source goes),
and a host device if its cartridge slot wants one. `CartProxy`-without-a-cartridge goes.

## §6 Not in this spec

- UCI itself — Spec 852.
- The U64 as a machine profile and its turbo (Spec 851).
- A host device's state in TRX64's checkpoint ring, rewind or `.c64re` dumps: the device belongs to the
  host, and a restore puts back the machine, not the device. A profile's own device decides for itself
  (Spec 852 D7 for the UCI block).
