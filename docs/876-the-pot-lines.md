# Spec 876 — The POT lines: paddles, mouse and extra fire buttons at `$D419`/`$D41A`

**Status:** PROPOSED 2026-09-24
**Repos:** TRX64. C64RE: no change.
**Number:** 876 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`, row present).
**Depends on:** Spec 855 (several SIDs, `sid_chip_read`, the host read hook), Spec 851/856
(the U64 turbo, `clk` counted in PHI2 cycles), Spec 850 D7 (`Hold::Cpu` / `Hold::Reset`),
Spec 863 (models), Spec 874 §15 (`Machine: Clone` written by hand).
**Enables:** UE2, the emulator that runs the unmodified Ultimate 64 firmware over
`trx64-core`. The firmware turns a USB mouse, the REST input API and joystick extra buttons
into `C64_PADDLE_n_X/Y` writes (`software/io/c64/joystick_output.cc:128-138`,
`software/io/usb/usb_hid.cc:74-91`). With this spec those writes reach a C64 program that
reads `$D419`/`$D41A`.
**Origin:** Owner decision, 2026-09-24. UE2 confirmed it the same day (§14).

---

## §1 What exists today

Checked against the code:

- **`$D419`/`$D41A` answer a constant `$80`.** `Sid6581::read` (`sid.rs:218-226`) returns
  `0x80` for regs `0x19` and `0x1a` (`sid.rs:220-221`). The comment there says "VICE default
  per Spec 429". The comment is wrong: VICE's fastsid answers `$FF`
  (`sid/fastsid.c:1092-1098`), and so does reSID (§2). The comment at `full.rs:469` repeats
  the claim.
- **Every CPU read of a SID goes through one function.** `FullBus::sid_chip_read`
  (`full.rs:797-811`) runs the host hook first (Spec 855 D5, `sid_host.read`). Then chip 0
  goes to `self.sid.read(reg, self.sid_regs)`, and chips 1.. go to
  `sid_extra[..].engine.read`, which is the same `Sid6581::read` and gives the same `$80`.
  It has two callers: the `$D400-$D7FF` branch (`full.rs:477-481`), and a SID mapped into
  `$DE00-$DFFF` ahead of the expansion port (`full.rs:603-605`).
- **The peek does not show what the CPU reads.** Both peek paths (`lib.rs:2642-2650` and
  `lib.rs:2718-2726`) return `sid_chip_regs(chip)[reg]`, which is the register shadow. For
  `$19`/`$1A` that is the last byte written there (normally `$00`). The CPU gets `$80`.
  `m d419` and the `io` verb therefore show a value the program never reads.
- **reSID is not on the read path.** `Resid::read` (`resid_ffi.rs:252-257` →
  `resid_shim.cc:202-203`) has no caller in the workspace, and reSID is audio only
  (`resid_audio.rs`). Its POT is `Potentiometer::readPOT()`, which returns `0xff` with the
  note "Not modeled" (`vendor/resid/pot.cc:25-29`, called from `sid.cc:179-184`).
- **The isolated SID bus** (`SidBus`, `lib.rs:486-489`, the chip-isolation gate) also calls
  `Sid6581::read`. It has no CIA.
- **CIA1 port A is already composed the way the multiplexer sees it.**
  `Cia::pa_output()` (`cia.rs:921-925`) = `(PRA & DDRA) | !DDRA`: a pin set as input counts
  as high. `cia1_pa_pins`/`cia1_pb_pins` (`keyboard.rs:322-356`) use the same byte for the
  keyboard.
- **CIA1 is written in three places:** the bus (`full.rs:685`), `poke_io`
  (`lib.rs:1594-1597`, at `cpu6510.clk`), and the isolated `CiaBus` (`lib.rs:448`, a gate
  without a SID). `write_full` goes through `FullBus::write`. `warm_reset` replaces CIA1
  with a fresh chip (`lib.rs:1427`), and the checkpoint restore rewrites it
  (`c64re_snapshot.rs:1949`).
- **`clk` counts C64 PHI2 cycles, including under turbo.** Below the divider,
  `clk_inc` returns before the clock moves (`c64_6510core.rs:836-851`). The fast path runs
  instructions as long as `clk` has not moved (`lib.rs:3836-3846`). A reset does not touch
  `clk` (`cold_reset`, `lib.rs:1297`). `run_held` advances it under both holds
  (`lib.rs:2201-2203`).
- **The checkpoint has a placeholder.** `"paddles": [0, 0, 0, 0]` is written as a constant
  (`c64re_snapshot.rs:1496`). This is the TS shape, and nothing reads it. `joystick1`/`2` are
  constants in the same way (`:1494-1495`).
- **The CIA gate hashes whole checkpoints.** `cia_alarm_check_gate` puts
  `serde_json::to_string(&checkpoint)` into its frozen digests
  (`tests/cia_alarm_check_gate.rs:218-225`). A key that every checkpoint gains would change
  every digest.
- **Input verbs exist for joysticks only.** The daemon has `session/joystick_set`,
  `session/joystick_clear` and `session/input_status` (`trx64-daemon/src/main.rs:7148-7210`).
  The monitor has no input verb (`OWNED`, `trx64-monitor/src/verbs.rs:743-786`).

So the SID answers a constant. It does not know the control ports, the multiplexer, or
its own sampling period.

## §2 What the references do

**The machine.** Each control port has two POT lines (pins 5 and 9). One 4066 analog switch
per port connects that port's POTX/POTY to the SID's POTX/POTY pins. CIA1 PA6 closes
port 1's switch, and PA7 closes port 2's. A closed switch needs its control pin high, and
a CIA pin set as input is pulled high. The SID measures each line with a 512-cycle
sequence. For 256 cycles it grounds the capacitor. For the next 256 it counts until the RC
charge crosses its threshold. The count, or `$FF` if it never crosses, is latched into the
register and stays there until the next sequence ends. The count grows with R. An open
line never crosses, so it reads `$FF`.

**VICE** (`sid/sid.c`, `c64/c64cia1.c`, `joyport/joyport.c`):

- The mux: `store_ciapa` calls `set_joyport_pot_mask((b >> 6) & 3)` (`c64cia1.c:161-166`).
  `b` is `PRA | ~DDRA`, composed in `core/ciacore.c:807-815`, so an input pin selects.
- The combination (`joyport.c:483-544`, and the same for Y): mask 1 → port 1, 2 → port 2,
  3 → `calc_parallel_paddle_value`, 0 → `0xff`. `calc_parallel_paddle_value`
  (`joyport.c:449-480`) handles two special cases first. Either side `0` gives `0` (wired to
  VCC). Either side `255` gives the other side (open). Otherwise it computes
  `t1·t2/(t1+t2)` (the resistor scale cancels) in `double` and truncates to `uint8_t`.
- The sampling (`sid.c:220-253`): chip 0 only. VICE samples on the first read in a new
  `clk & ~511` block, using the values at the moment of the read ("simplistic 512 cycle
  sampling", `:236`). A read within 512 cycles of a mux change that selected or deselected
  a connected pot gives a "bad value" (`makebadpotval`, `:203-218`; the change clock is
  `joyport.c:208-213`). Every regular sample gets random fuzz (`makepotval`, `:184-200`).
  The alarm-driven version that would sample every 512 cycles is `#if 0`
  (`sid.c:153-178`).
- Chips 1.. go to the engine: fastsid answers `0xff` (`fastsid.c:1092-1098`), and reSID
  answers `0xff` (`resid/pot.cc:25-29`). With sound off the fallback is `0xff`
  (`sid.c:274-277`).
- Devices: a joystick's POT lines are its 2nd/3rd buttons, `0x00` when pressed and `0xff`
  when released (`joyport/joystick.c:709-719`). The 1351 returns
  `(pos & 0x7f) + 0x40` (`joyport/mouse_1351.c:130`, `:136`). Paddles return a host axis or
  `0xff - value` (`joyport/mouse_paddle.c:170-194`), and `0xff` with nothing there.

**The Ultimate 64 firmware** writes finished per-port values into core registers
`C64_PADDLE_1_X` … `C64_PADDLE_2_Y` (`software/system/u64.h:139-142`), with
`C64_MOUSE_EN_1/2` (`:143-144`), `C64_PADDLE_EN` (`:121`) and `C64_PADDLE_SWAP` (`:123`,
set from the joystick swap, `u64_config.cc:1087`). The core turns those into what the SID
registers read. The core is closed.

**The U64 hardware test** (`1541ultimate/tests/e2e/api/input_test.py:686-727`) pauses the
machine and writes `$DC02 = $C0`. It then writes `$DC00 = $40` (port 1) or `$80` (port 2),
sleeps 100 ms, and reads `$D419`/`$D41A`. It pauses because otherwise "the KERNAL's
keyboard-scan IRQ overwrites `$DC00` within ~20ms" (`:702-704`). The extra-button check
treats bit 7 clear as pressed (`:731-739`).

**The KERNAL** (901227-03) writes `$DC00` at `$EA90` (`$00`, all columns), `$EAA5` (`$FE`),
`$EAD7` (each next column, rotated), and `$EB44` (`$7F`, when the scan ends). Inside the IRQ
it writes `$F6C9` (`$BD`, the STOP key test) and `$F6D4`. IOINIT writes `$FDAB` (`$DC00`)
and `$FDC8` (`$DC02 = $FF`). Between scans `$DC00 = $7F`, which means PA7 low and PA6 high:
port 1 is selected. During a scan the selection runs through all four states: `$00`
selects neither, `$FE`-`$DF` select both, `$BF` selects port 2, `$7F` selects port 1.

What this spec takes: VICE's mux (the composed port-A byte, inputs select), VICE's
parallel combination, and the real chip's fixed 512-cycle latch instead of VICE's
sample-on-read. From VICE's defaults it takes `$FF` everywhere a line is open.

## §3 D1 — What selects

`sel = (cia1.pa_output() >> 6) & 3`: bit 0 is port 1 (PA6), bit 1 is port 2 (PA7). It is
exactly VICE's mask. A pin programmed as input selects, so after a reset (DDRA = `$00`) both
ports are selected until IOINIT runs.

The mux follows CIA1's output byte, not the pin level. A key pressed while the keyboard
matrix pulls PA6/PA7 low from the port B side does not open a switch. VICE does not model
that either (§13).

The KERNAL scan moves the mux (§2). This is real behaviour and it is not smoothed over. A
program that reads POT with the IRQ running can latch a sample taken mid-scan. Paddle
drivers SEI, or read inside the IRQ after selecting. §12.8 records what a running KERNAL
does to the reading.

## §4 D2 — The source: what the host sets

```rust
impl Machine {
    /// The POT lines of control port `port` (1 or 2) now read `x` / `y`: the byte the
    /// SID latches while only this port is selected. `$FF` = open.
    pub fn set_pot(&mut self, port: u8, x: u8, y: u8) -> Result<(), String>;
    /// Nothing on the POT lines of `port`: both read `$FF`.
    pub fn clear_pot(&mut self, port: u8) -> Result<(), String>;
    /// What is set on `port` (None = cleared).
    pub fn pot(&self, port: u8) -> Option<(u8, u8)>;
}
```

- **The byte is final.** `set_pot` takes the POT byte exactly as the host computed it, and
  TRX64 does no position mapping. A paddle host passes the knob's count. A 1351 host passes
  its computed value: UE2's bridge maps the firmware's 7-bit position
  (`C64_PADDLE_1_X = mouse_x & 0x7F`, `usb_hid.cc:89-90`) the way the U64 core does, and a
  real 1351 puts its position in bits 1-6. A 2nd/3rd fire button passes `$00` when pressed
  and `$FF` when released. That mapping is not TRX64's concern (UE2, 2026-09-24).
  `C64_PADDLE_EN`, `C64_MOUSE_EN_n` and `C64_PADDLE_SWAP` are the core's registers.
  UE2 turns them into `set_pot`/`clear_pot` for the right port. TRX64 has no joystick swap.
- **No device trait.** A POT line is a byte, not a protocol. The real 1351 synchronises to
  the SID's discharge phase so that it can produce an exact count. With the byte handed
  over, that synchronisation is the host's result, not TRX64's mechanism.
- **Port outside 1-2** is refused: `"pot: control port 1 or 2, not <p>"`.
- **Combination at a sample**, per axis, with `a` = port 1 and `b` = port 2 (a cleared port
  counts as `$FF`):

  | sel | value |
  |---|---|
  | 0 (neither) | `$FF` |
  | 1 | `a` |
  | 2 | `b` |
  | 3 (both) | `a == 0 \|\| b == 0` → `0`; `a == $FF` → `b`; `b == $FF` → `a`; else `⌊a·b/(a+b)⌋` |

  Both switches closed put the two resistances in parallel, and the count grows with R, so
  the count is `R1·R2/(R1+R2)` in counts. An open line is infinite R, and `$FF` is the
  saturated count, the only reading an open line can give. So `$FF` means open: combined
  with anything, it leaves the other side. A set `$FF` and a clear therefore read the same.
  The quotient is exact integer arithmetic in `u16`, rounded down. VICE computes the same
  quotient in `double` and truncates. Where the true quotient is a whole number, VICE's
  `double` could land a hair below and truncate one lower; §12.4 checks the table against
  exact arithmetic.
- **Only chip 0 has POT lines.** SIDs 1.. (Spec 855) read `$FF` on `$19`/`$1A`, as VICE's
  engines do. The constant in `Sid6581::read` becomes `0xff`, which also fixes the stale
  comment.

## §5 D3 — The latch: 512 PHI2 cycles

```rust
/// crates/trx64-core/src/pot.rs (new)
#[derive(Clone, Debug)]
pub struct PotLines {
    set: [Option<[u8; 2]>; 2],   // port 1, port 2: [x, y]
    latch: [u8; 2],              // what $D419 / $D41A read now
    sampled: u64,                // the last 512-cycle boundary the latch reflects (clk >> 9)
    pub reads: u64,              // CPU reads of $D419/$D41A on chip 0 — a counter, not state
}
```

- **Model.** The SID finishes one measurement every 512 PHI2 cycles. The boundaries fall at
  `clk ≡ 0 (mod 512)`, and at a boundary the latch takes the value of §4 under the
  selection and the set values that stand then. Between two boundaries the register does
  not move. A change at cycle `t` is seen at the first boundary after `t`: at most 512
  cycles later.
- **Phase: the power-on clock.** `clk` is 0 at `Machine::new`. A reset does not touch it
  (§1), and the session's power cycle builds a new machine. So the phase is `clk mod 512`,
  and a checkpoint that carries `clk` carries the phase. `sampled` is stored as well
  (§8), so the latch stays exact across a restore. VICE uses the same grid
  (`clk & ~511`, `sid.c:235-236`).
- **Lazy, exact.** No per-cycle work. `settle(clk, sel)` does: if `clk >> 9 != sampled`,
  then `latch = combine(sel, set)` and `sampled = clk >> 9`. It runs:
  - on a CPU read of `$D419`/`$D41A` on chip 0, then answers `latch[axis]` and counts
    `reads`;
  - **before** anything that changes the selection or a set value, with the old state:
    the CIA1 write of `$DC00` or `$DC02` in the bus (`full.rs:685`) and in `poke_io`
    (`lib.rs:1597`), `set_pot`, `clear_pot`, and `warm_reset` before it replaces CIA1
    (`lib.rs:1427`).

  Every change settles first, so every boundary since the previous settle saw the state
  that stood before the change. The read's settle therefore gives exactly the value at the
  last boundary. It is not VICE's value at the moment of the read.
- **A change on the boundary cycle.** A write at `clk = 512·k` settles with `clk >> 9 = k`
  before it applies, so boundary `k` takes the old value. The boundary is sampled at the
  start of its cycle, before that cycle's bus access.
- **Not modelled:** the sample that a mux change during the counting half garbles on the
  real chip, and VICE's random fuzz and "bad value". TRX64 is deterministic, and restores
  must replay in lockstep. §13 and §14 cover this.

## §6 D4 — Where it is read

- **CPU reads.** `FullBus::sid_chip_read` (`full.rs:797-811`): the host hook first, as
  today (855 D5 keeps its precedence). Then, for chip 0 and reg `0x19`/`0x1a`,
  `pot.read(clk, sel, axis)`. Everything else is unchanged. Both callers (`full.rs:481`,
  `:605`) and every mirror of chip 0 in `$D400-$D7FF` reach it. `FullBus` gains
  `pot: &'a mut PotLines`, and every constructor passes it (`lib.rs:1626`, `:1695`,
  `:2005`, `:3768`, the test constructor at `full.rs:1239`).
- **Peeks** (`lib.rs:2642-2650`, `:2718-2726`): for chip 0, `$19`/`$1A` answer
  `pot.peek(clk, sel, axis)`. That is the same computation as `settle` without writing
  anything, so `&self` is enough. The monitor's `m d419` and `io` then show what the
  CPU would read. This fixes the shadow disagreement of §1. OSC3/ENV3 peeks stay as they
  are (§13).
- **`SidBus`** (`lib.rs:486-489`): reads `Sid6581::read`, so `$FF`. It has no CIA and no
  POT lines.
- **reSID:** no change. `pot.cc` already answers `$FF`, and `Resid::read` has no caller.

## §7 D5 — Model, turbo, holds, reset, clone

- **Models (Spec 863).** Every C64 model has the same 4066 wiring and the same 512-cycle
  SID sequence, counted in PHI2 cycles. The wall-clock time differs (≈520 µs PAL,
  ≈500 µs NTSC), and the latch does not care. A model switch changes nothing here.
- **U64 profile and turbo.** The latch is counted in `clk`, and `clk` is PHI2 (§1). At
  48 MHz a boundary falls on the same PHI2 cycle as at 1 MHz, and the fast path moves no
  boundary. A POT read is IO, so it ends a fast-path batch (`lib.rs:3850-3851`) and is
  answered at its PHI2 cycle. The U64 core's own override registers are UE2's (§4).
- **`Hold::Cpu` / `Hold::Reset`.** `run_held` advances `clk` (`lib.rs:2201-2203`), so
  boundaries pass. The next read or settle sees them, and no call is added to the hold. The
  U64's pause is a held CPU with PHI2 running, and the SID keeps measuring through it,
  which is the premise of the 100 ms wait in `input_test.py`.
- **Reset.** `set` is kept: a reset does not unplug a paddle. The phase is kept (§5, and
  §14 for the SID's RES pin). The latch is re-sampled at the next boundary under the reset
  CIA1 (DDRA `$00` → both selected).
- **Clone.** `PotLines` is plain data. The hand-written `Machine: Clone` (874 §15, the impl
  at `lib.rs:732`) lists it, and the compiler enforces that. A clone carries the set values
  and the latch: unlike a host's IEC device, these are values, not the host's objects.

## §8 D6 — Checkpoints

A new top-level node, written **only when it is not the default**:

```json
"pot": { "set": [[x, y] | null, [x, y] | null], "latch": [x, y], "sampled": n }
```

- **Default** = nothing set and `latch = [$FF, $FF]`. Then no key is written. Every existing
  checkpoint and the `cia_alarm_check_gate` digests stay as they are.
- **Restore** with a `pot` node restores it exactly. Without one (an older checkpoint, or
  the default), the result is nothing set, `latch = [$FF, $FF]` and
  `sampled = restored clk >> 9`. An older checkpoint was captured under the `$80` constant.
  Its continuation reads `$FF` where the original run read `$80`, which is the default
  change of §10 and not a restore defect.
- **The `paddles` placeholder** (`c64re_snapshot.rs:1496`) stays as it is: a TS-shape
  constant, never read. Reusing it would turn every old checkpoint's `[0, 0, 0, 0]` into
  "both ports pulled to zero", which is fire 2 and 3 held on both ports.
- `reads` is a counter and is not checkpointed.
- The ring, rewind and `.c64re` all take this tree (`checkpoint_ring.rs` stores the
  checkpoint `Value`), so they need nothing of their own.

## §9 D7 — Surface

**Library (UE2):**

```rust
machine.set_pot(1, x, y)?;        // what the core's C64_PADDLE_1_X/Y + MOUSE_EN_1 yield
machine.clear_pot(2)?;            // nothing on port 2
machine.pot(1)                    // Option<(u8, u8)>
machine.pot_lines()               // &PotLines: latch, sampled, reads (read-only)
```

Called between runs, the same as any host input. A value set at clock `t` is latched at
the first boundary after `t`. The host's slice length is therefore the POT's input latency,
plus at most 512 cycles.

**Monitor:** one verb, `pot`. It is added to `OWNED` and the help text.

```
pot                   selection (from $DC00/$DC02), each port's set value or "open",
                      the latch, cycles to the next sample, reads
pot <1|2> <x> <y>     set_pot
pot <1|2> off         clear_pot
```

C64RE reaches it through the monitor passthrough and needs no tool of its own.

**Daemon:** `session/pot_set {port, x, y}` and `session/pot_clear {port?}` (absent means
both). These are the shape of `joystick_set`/`joystick_clear` (`main.rs:7148-7181`).
`session/input_status` gains `pots: [[x, y] | null, [x, y] | null]` (an additive field).
No UI work in this spec. `trx64-ffi` is untouched.

## §10 Byte identity — the default change

`$80` → `$FF` changes the value of every POT read on a machine where nothing is set, on
chip 0 and on chips 1... Any program that reads POT can therefore change course. Bit 7 is
set in both, so a test on bit 7 or on the sign (`BMI`/`BPL`) takes the same branch. A
compare against a threshold between `$80` and `$FF`, or arithmetic on the value, does not.

**Last Ninja Remix** gates its intro on POTX bit 7 (`$0917 LDA $D419` … `$091F CMP #$00` /
`$0921 BMI $08F9` intro, else `$0923 JMP $0B7F` game; C64RE Spec 429). With `$FF`, bit 7 is
set, `BMI` is taken, and the intro runs as it does today. This is the case that once broke
when POT read `$00`, and §12.7 pins it.

The earlier `$80` came from a VICE gold trace (26 676 reads of `$80`, Spec 429). The VICE code
read for this spec gives `$FF` for the default joystick device and for no device (§2). Where
that trace's `$80` came from is not established here (§14).

Acceptance §12.13 runs the seven-game gate. For every game it records the POT read count
and whether the picture changed. Every changed picture must have a POT read behind it and a
first-divergence explanation.

## §11 Cost

- **Per `$DC00`/`$DC02` write:** one `settle`, which is a shift and a compare.
- **Per POT read:** the same, plus the combine at most once per 512 cycles.
- **Nothing per cycle, per instruction or per frame.** No alarm, and no work under a
  hold.
- Measured with `perf_bench` on main against the branch. It must stay within run-to-run
  noise.

## §12 Acceptance

**Gate — must pass.** `crates/trx64-core/tests/pot_gate.rs` unless stated otherwise.

1. **Default.** On a fresh machine, for each of the four selections (`$DC02 = $C0`,
   `$DC00 = $00/$40/$80/$C0`) and for DDRA = `$00`: CPU read and peek of `$D419`/`$D41A` give
   `$FF` after a boundary. An extra SID (Spec 855) reads `$FF` on `$19`/`$1A`.
2. **UE2's hardware test, replayed** (`input_test.py:686-727`). KERNAL booted to READY.
   `set_pot(1, $12, $34)`, `set_pot(2, $56, $78)`. Under `Hold::Cpu` (the pause):
   `poke_io($DC02, $C0)`, `poke_io($DC00, $40)`, run 100 ms held (PAL 98 525 cycles), read
   `$D419`/`$D41A` by peek and by a CPU `LDA` after the hold → `$12`/`$34`. Then `$DC00 = $80` gives
   `$56`/`$78`. The same on the NTSC model (102 273 cycles). Recorded beside it, not judged:
   the same writes without the hold. The KERNAL's scan puts `$DC00` back to `$7F` within
   one IRQ, so port 1 answers whatever was written, which is the race the test's comment
   names.
3. **The latch at its cycle.** A loop of `LDA $D419 / STA $C100,X / INX / BNE` with known
   read cycles. The selection moves from port 2 to port 1 by `STA $DC00` at cycle `W`,
   stepped over every cycle of one 512-cycle window. Every read before the first boundary
   after `W` gives port 2's value. Every read at or after it gives port 1's. A write exactly
   on a boundary cycle leaves that boundary with the old value. The same for `set_pot`
   between two runs.
4. **Combination.** Both selected, exact table: `(100,100)→50`, `(200,56)→43`,
   `(0,n)→0`, `($FF,n)→n`, `(n,$FF)→n`, `($FF,$FF)→$FF`, one side cleared → the other side.
   Neither selected → `$FF` whatever is set.
5. **Inputs select.** DDRA = `$00` with PRA anything → both. DDRA = `$C0`, PRA = `$00` →
   neither.
6. **Fire buttons.** `set_pot(2, $00, $FF)` with port 2 selected: POTX bit 7 clear and
   POTY bit 7 set. That is fire2 pressed and fire3 released, by `input_test.py:731-739`'s rule.
7. **Last Ninja's gate.** A program that is `$0917`'s sequence (`LDA $D419 / CMP #$00 /
   BMI intro / JMP game`), run with nothing set, takes the intro. With `set_pot(1, $00,
   $00)` and port 1 selected it takes the game. That is the old defect, reproduced on
   purpose.
8. **The KERNAL moves the mux** (characterisation, recorded). Port 1 = `(10,10)`,
   port 2 = `(200,200)`, KERNAL idle with the IRQ on. Every `$D419` value read over 100
   frames is in `{10, 200, 9, $FF}` (port 1, port 2, parallel, neither). The distribution is
   recorded.
9. **Turbo.** On the `u64` profile at 48 MHz, test 3's sweep puts the boundaries on the
   same PHI2 cycles as at 1 MHz. `reads` counts every read once, with or without the fast
   path.
10. **Holds.** A boundary that falls inside `Hold::Cpu` and inside `Hold::Reset` is visible
    to the first read after the hold.
11. **Checkpoints.** Nothing set → the capture has no `pot` key. Set mid-window → the
    capture has it, and the restore reads the same values on the same cycles as the
    straight run, in 500-frame lockstep. A checkpoint without the key restores to nothing
    set and `$FF`. A `.c64re` written by main restores.
12. **Clone and reset.** A clone reads what the original reads. `warm_reset` keeps the set
    values, and after it both ports are selected until IOINIT.
13. **The seven games.** `cargo test --release -p trx64-core --test seven_game_gate --
    --ignored`, then again with `GATE_DRIVE_B=9` and with `GATE_FOLDER=9`. The gate prints
    each game's `pot.reads` and the values seen. For each game, record whether its picture
    is byte-identical to main's. A changed picture needs `reads > 0` and the first-divergence
    reason. Expected: 7/7 pass, lastninja shows the intro and not Central Park, and every
    picture is byte-identical. C64RE Spec 429 measured on the TS runtime that the six others
    read no POT; this re-measures it.
14. **Surface.** The monitor golden transcript gains `pot` (help and output), and nothing
    else in it moves. Daemon tests cover `pot_set`/`pot_clear` and `input_status.pots`.
15. **Nothing else moved.** `cia_alarm_check_gate` digests unchanged. `sid_multi_gate`,
    `u64_turbo_gate`, `turbo_fastpath_gate` and `ntsc_gate` green. Run the workspace suite.

**Characterisation, recorded:** 16. Cost: `perf_bench` main against the branch.

## §13 Scope — where this stops

- **In:** the mux from CIA1's port-A output; `set_pot`/`clear_pot`/`pot`; the parallel
  combination; the 512-cycle latch with the power-on phase; every CPU read and peek path;
  `$FF` for SIDs 1..; the checkpoint node; the `pot` monitor verb; two daemon verbs and
  `input_status.pots`; the stale comments at `sid.rs:220` and `full.rs:469`.
- **Out:** the RC analog itself; the garbled sample after a mux change during the count
  half; VICE's fuzz and "bad value"; the mux seeing the pin level rather than the output
  byte (a key pulling PA6/PA7 low); any mapping from device position to byte (the host's,
  §4); a device trait; the U64 core's override registers (UE2's); OSC3/ENV3 peeks; the
  constant `joystick1`/`2` checkpoint nodes; UI controls; the light pen (PB4/PA4, not POT).

## §14 Open

**Settled by UE2, 2026-09-24:** the `$FF` default; `$FF` with neither port selected; the
parallel-resistance combination for both; the 512-cycle latch. UE2 raised no objection to
the shape. `set_pot` takes the finished POT byte and TRX64 does no mapping. How the closed
U64 core turns the firmware's 7-bit position into that byte has not been measured on a
C64U yet. That is UE2's bridge, not TRX64's.

**Open:**

- **Does the SID's RES pin restart the 512-cycle sequence?** The C64's reset line reaches
  the SID. If RES restarts the sequence, the phase after a reset is the reset's clock and
  not power-on. Nothing in the tree says either way: reSID does not model POT, and VICE
  does not reset `pot_cycle` (`sid.c:79`, `sid_reset` `:530-541`). Decided here: power-on
  phase. Revisit only if a measurement shows otherwise. No program can tell the difference
  unless it counts cycles from reset to a POT change.
- **Where VICE's `$80` came from** in C64RE Spec 429's gold trace. The candidates are a
  configured paddle device answering a centred host axis (`mouse_paddle.c:170-171`) or a
  1351 at rest. This does not block: bit 7 is the same.
