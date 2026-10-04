# 892 — EasyFlash 3 as a cartridge

**Status:** BUILT (2026-10-04, on branch `spec-892-ef3`; not merged).
**Source:** skoe's EF3 sources, as carried in github.com/FrankBuss/kerberos `1bc1352`,
`skoe-easyflash/` (hg node fb0211c, CPLD 1.1.1):
- `Hardware/ef3-vhdl/src/` — `ef3.vhdl`, `cart_easyflash.vhdl`, `cart_io2ram.vhdl`,
  `cart_usb.vhdl`, `reset_generator.vhdl`, `exp_bus_ctrl.vhdl`;
- `EasySDK/eapi/eapi-mx29640b.s`;
- `EF3BootImage/` (`efmenu.c`, `efmenu_asm.s`, `directory.s`, `trampoline.s`).

The licence is zlib-style: behaviour may be re-implemented freely, and copied code must keep
the notice. Line references below are to these files.

## Scope (owner, 2026-10-04)

**In:**
- the 8 slots, selected with `$DE01`;
- EasyFlash banking with `$DE00`/`$DE02` and the 256-byte IO2 RAM;
- flash read, program and erase on the one 8 MB chip, so the EF3 EAPI works;
- the three buttons (Menu, Reset, Special);
- the `$DE0F` mode register for modes 0, 1 and 7.

**Out, by decision:**
- KERNAL mode (2);
- the freezer modes AR/RR/NP (4) and SS5 (5);
- C128 mode (6);
- USB (FT245 at `$DE09`/`$DE0A`).

What TRX64 does when a program selects one of them is D9.

## The CRT image

- CRT hardware type **90**. The numbering is shared with TRX64-Ultimate: 87 GMod4, 88 C64MegaCart,
  89 TwoMegabyter, 90 EasyFlash3.
- 8 slots × 64 banks × 16 KB = 8 MB.
- CHIP packets are slot-major: the bank field is `slot*64 + bank` (0..511). Each packet holds 8 KB.
  Load address `$8000` is ROML; `$A000` and `$E000` are ROMH.
- The header has EXROM=1, GAME=0.
- An image smaller than 8 MB fills the rest with `$FF`, as erased flash does.

## Decisions

**D1 — Registers, as `ef3.vhdl:627-636` and `cart_easyflash.vhdl:132-209` decode them.** Only
`$DE00-$DE0F` is decoded; there are no mirrors in `$DE10-$DEFF`.

| Register | Write | Read |
|---|---|---|
| `$DE00` bank | bits 5:0; bits 7:6 ignored | open bus |
| `$DE01` slot | bits 2:0 | `slot` (bits 7:3 = 0) |
| `$DE02` control | bit 0 GAME, bit 1 EXROM, bit 2 M, bit 3 no-VIC, bit 7 LED; bits 6:4 ignored | open bus |
| `$DE08` CPLD version | — | `$49` (1.1.1) |
| `$DE09`/`$DE0A` USB | ignored (out of scope) | `$00`: no data, not ready |
| `$DE0F` mode | D3 | open bus |

- `$DE08`-`$DE0A` answer in EF mode only.
- A slot or bank write takes effect at once, with no reset.
- The monitor's peek still shows the write-only registers as shadows, as it does for the
  EasyFlash today.

**D2 — The cartridge lines from `$DE02` and the boot flag, not from a jumper**
(`cart_easyflash.vhdl:191-239`).

| `$DE02` low bits | boot = 1 | boot = 0 |
|---|---|---|
| M=0, EXROM bit 0 | Ultimax | off |
| M=0, EXROM bit 1 | 16K | 8K |
| M=1, GAME 0, EXROM 0 | off | off |
| M=1, GAME 1, EXROM 0 | Ultimax | Ultimax |
| M=1, GAME 0, EXROM 1 | 8K | 8K |
| M=1, GAME 1, EXROM 1 | 16K | 16K |

- **The boot flag:**
  - It is 1 at power-on, after an external reset, and after Menu and Reset.
  - It is 0 after Special.
  - It reaches the GAME line only at a reset, or at a `$DE02` write with M=0.
- **No-VIC (bit 3):** the lines are asserted only while PHI2 is high. In the VIC's half-cycle
  the cart is invisible, so `vic_romh` returns nothing.
- TRX64's EasyFlash keeps `& $87` and drops bit 3. For EF3 bit 3 is honoured. The EasyFlash 1
  mapper is not changed.

**D3 — `$DE0F` (`ef3.vhdl:667-735`).**
- `$DE0F` is decoded only while the "menu" enable is set.
- Any write clears all mode enables, then sets the one the value selects.
- After that write `$DE0F` is gone. It comes back only after an external reset or the Menu button;
  a reset the cart generates does not bring it back.
- The values:
  - **0:** EF, then a generated reset.
  - **1:** EF, no reset (EasyProg and PRG start).
  - **7:** kill, then a reset. Afterwards the cart is invisible: no lines, no registers, no
    IO2 RAM. Only the Menu button or an external reset brings it back.
  - **3 and 8-15:** everything disabled with no reset, as the hardware does
    (`when others => null`).
  - **2, 4, 5, 6:** see D9.

**D4 — Resets (`ef3.vhdl:669-684`, `cart_easyflash.vhdl:160-175`, `reset_generator.vhdl:60-96`).**

| Event | Mode | `$DE0F` | Slot | Bank | Boot flag | `$DE02` |
|---|---|---|---|---|---|---|
| Power-on, external reset (the C64's reset) | EF | enabled | 0 | 0 | 1 | cleared |
| Menu button | EF | enabled | 0 | 0 | 1 | cleared |
| Reset button | unchanged | unchanged | kept | 0 | 1 | cleared |
| Special button | unchanged | unchanged | kept | 0 | 0 | cleared |
| Reset the cart generates (`$DE0F` 0 or 7) | as set | unchanged | kept | kept | kept | cleared |

- A reset the cart generates holds the C64 in reset for 8 PHI2-low edges
  (`reset_generator.vhdl:60-80`). `architecture.txt` says 7, but the code wins.
- `go_64` pulls GAME low from the start of the reset until the first ROMH access
  (`:87-96`). The reset vector is therefore read in Ultimax.
- The buttons work only after all three have been released once since power-on.

**D5 — The buttons** are a new cartridge control: RPC `cart/button {button: "menu"|"reset"|"special"}`,
the monitor verb `cart button <menu|reset|special>` (reachable from C64RE through
`runtime_monitor`), and the FFI. A press is a down-and-up within one call. The C64's own reset
is the external reset. On a cart without buttons, the press is refused by name.

**D6 — Flash: one MX29LV640EB, 8 MB, byte mode.**
- **Address** (`ef3.vhdl:786-820`):
  `flash = slot<<20 | bank[5:3]<<17 | half<<16 | bank[2:0]<<13 | (addr & $1FFF)`.
  `half` is 0 for ROML and 1 for ROMH. The CRT loader places each CHIP packet with this same
  formula. A bank is not 16 KB contiguous in the chip: its ROML and ROMH halves lie 64 KB
  apart.
- **ID:** manufacturer `$C2`, device `$CB` at address 2 (`eapi-mx29640b.s:31-32,319-365`).
- **Unlock:** `$AA` to `$AAA`, then `$55` to `$555`, with byte-mode addresses.
- **Sectors:**
  - the boot blocks are 8 × 8 KB at `$00000-$0FFFF` (slot 0, banks 0-7, ROML — the KERNAL
    banks);
  - the rest is 127 × 64 KB;
  - a 64 KB sector is (slot, bank[5:3], half): 8 banks of one half.
  The flash model gets non-uniform sectors; the existing types stay uniform.
- **Writes** go to the flash only in EF mode, and only in a window the PLA asserts (in
  practice Ultimax at `$8000` and `$E000`).
- **Status polling and timings** come from the MX29LV640EB datasheet, not from the EF3
  sources: DQ7 data polling and the DQ6 toggle, plus typical program, sector-erase and
  chip-erase times. The values used are cited in the code.

**D7 — EAPI.** TRX64 does not swap the image's EAPI for EF3. `cart.rs:1229` replaces it with the
AM29F040 block for EasyFlash 1, and that would break the EF3 driver ("MX29LV640EB 1.2", which
installs into `$DF80-$DFFB` and returns Y = 8 slots). The EF3 EAPI runs unchanged against the
emulated chip.

**D8 — The IO2 RAM** is one 256-byte RAM shared by all slots (`cart_io2ram.vhdl:46-65`). In the
kill state it does not answer. Its power-on contents are not in the source, so it starts with
the same pattern as TRX64's EasyFlash IO2 RAM.

**D9 — Out-of-scope modes selected by a program (2, 4, 5, 6): kill (owner, 2026-10-04).** The
cart does what mode 7 does — a reset, then invisible until Menu or an external reset — and
TRX64 says so: the daemon's cart status carries `ef3Mode` with `notEmulated: true`, and the log
names it ("EF3 mode 4 (AR/RR/NP) is not emulated — the cartridge is off").

**D10 — The menu in slot 0.** TRX64 runs whatever the image holds. A full EF3 image boots its
menu from slot 0, bank 0, ROMH `$FFFC` (the trampoline at `$FF00` → bank 8). TRX64 has no menu
of its own. An image without a menu in slot 0 just runs, as the hardware would: the reset
vector comes from slot 0's flash, whatever is there.

**D11 — State.** Checkpoints and `.c64re` carry:
- the slot, bank, `$DE02`, the boot flag and the mode enables (EF, menu, kill);
- the IO2 RAM;
- the flash contents, as a delta against the image, the same way the other flash carts do;
- the flash chip's command state.

The cartridge persists flash writes into the original image like every flash cart (`savecrt`),
in the CHIP layout above.

## Acceptance

1. A type-90 image with skoe's EF3 boot image in slot 0 boots to the EF3 menu. Starting slot n
   from the menu runs that slot's EasyFlash program.
2. `$DE01` switches slots at once; a read of `$DE01` returns the slot. `$DE08` reads `$49`.
3. Every row of D2's table, including no-VIC, gives the lines and windows shown.
4. `$DE0F` (mode 4 also shows `notEmulated` and leaves the cart off):
   - 0 resets into the selected slot;
   - 1 does not reset;
   - 7 makes the cart invisible until Menu;
   - after any write `$DE0F` no longer decodes.
5. Menu, Reset and Special each produce their D4 row, measured on registers, lines and boot.
6. The EF3 EAPI ("MX29LV640EB 1.2"), driven from a test program:
   - `EAPIInit` returns `$CB`/`$C2`/8;
   - it programs and erases a 64 KB sector in slot 5 and a boot block in slot 0;
   - it reads back what it wrote, and nothing else changes.
7. A flash write survives a checkpoint round trip, `savecrt` and reload.
8. The EasyFlash 1 gates are unchanged, and all suites and the 7-game gate are green.

## Open

- Whether the chip compares the high address bits in the unlock cycles. The EAPI issues them
  at slot 0 / bank 0, which satisfies both readings. TRX64 compares the low 12 bits, as
  `FLASH040_160` does (mask `$FFF`).

## As built

One new module, `crates/trx64-core/src/ef3.rs` (`Ef3Mapper`), the `MapperType::EasyFlash3` row in
`cart.rs`, a new chip row and four small extensions in `flash040.rs`, and the machine, daemon,
monitor and FFI doors. The EasyFlash 1 mapper is untouched. File:line below are at the commit
that closes this spec.

**Type and image.** CRT type 90 and the mnemonics `ef3`/`easyflash3`: `cart.rs:616`, `:2709`,
`:2718`; builder `:2068`; raw `.bin` geometry (512 banks of 16 KB, slot-major) `:2562`. CHIP
packets are placed by the D6 formula in `Ef3Mapper::new` (`ef3.rs:151`, formula `:38`); a packet
numbered past 511 is an error (`CrtError::BankOutOfRange`), not dropped.

- **D1** — registers: `ef3.rs:284` (read), `:326` (write); `$DE08`..`$DE0A` answer in EF mode
  only; no mirrors; peek shows the write-only registers (`:305`).
- **D2** — lines `ef3.rs:272`; `ctrl_game` is registered at the `$DE02` write (`:339`) and at a
  reset (`:196`), so the boot flag reaches GAME only there. No-VIC: `vic_romh` returns nothing
  (`:458`); the CPU's lines are unchanged.
- **D3** — `$DE0F`: `write_mode` `ef3.rs:205`; decoded only while `enable_menu`; 2/4/5/6 act as
  mode 7 and set `not_emulated`.
- **D4** — external reset `ef3.rs:381`; generated reset `:391` (keeps mode, slot, bank, boot
  flag; clears `$DE02`; pulls GAME); the pull ends at the vector fetch (`:396`). Machine side:
  `cart_generated_reset` (`lib.rs:1334`) holds the C64 for 8 PHI2 cycles with the VIC running,
  then runs a warm reset that tells the cartridge the reset is its own (`cold_reset_by`
  `lib.rs:1365`); the run loop picks up a request made by a CPU write after the instruction that
  made it (`lib.rs:4104`).
- **D5** — buttons: `press_button` `ef3.rs:410`; `Machine::cart_press_button` `lib.rs:1342`
  (refuses by name); RPC `cart/button` `trx64-daemon/src/main.rs:7591`; monitor
  `cart button <menu|reset|special>` (`main.rs:6445`, help in `trx64-monitor/src/verbs.rs`,
  `MONITOR.md`, golden re-blessed); FFI `cart_button` (`trx64-ffi/src/lib.rs:422`).
- **D6** — chip: `FLASH_MX29LV640EB` (`flash040.rs:202`), non-uniform sectors (`sector_num`
  `:366`, `erase_sector` `:620`), a 24-byte erase mask, status polling during a program
  (`ProgramBusy`, `:475`). Writes reach the chip only in EF mode with the lines at Ultimax
  (`ef3.rs:367`).
- **D7** — no EAPI replacement; the real driver runs (gate A6).
- **D8** — IO2 RAM: one 256-byte RAM, answers in EF mode only (`ef3.rs:296`, `:353`).
- **D9** — `notEmulated`/`ef3Mode` in `session/cart_status` (`main.rs:13980`) and the log line on
  stderr (`lib.rs:4107`).
- **D10** — nothing added: the image decides (gate A1 boots skoe's menu from slot 0).
- **D11** — state: `Ef3State` rides `FlashCartState::ef3` (`cart.rs:173`) and the checkpoint's
  `ef3State` (`c64re_snapshot.rs:1275`); the chip's command state is `flashLoState`; the flash is
  the writable image (8 MB, chip order) like the other flash carts; `savecrt` writes a type-90 CRT
  from the flash (`ef3.rs:531`). `get_state` does not clone the 8 MB chip.

**Chip values (Macronix MX29LV640E T/B datasheet, PM1328 REV. 1.7, 2011-12-27).** Byte program 9 us
typ (300 us max); sector erase 0.5 s typ (2 s max — rev 1.7 changed this from 0.7 s); chip erase 45 s
typ (65 s max); sector-erase time-out 50 us; byte-mode commands `$AAA`/`$555`; autoselect `$C2` at
X00 and `$CB` at X02 (bottom boot); eight 8 KB boot sectors then 127 × 64 KB. Typical values are
used, 1 us = 1 PHI2 cycle. Status as the datasheet tables give it: program in progress Q7#, Q6
toggling; erase in progress Q7 = 0, Q6 toggling, Q3 low during the time-out. Q5 (exceeded time limit)
and Q2 are not modelled.

**Where the spec left a choice, and what was chosen.**
- *The first ROMH access after a generated reset.* `go_64` pulls GAME low until the first ROMH
  access (D4). On the board the CPLD sees that access, lets GAME go and the PLA swaps ROMH for
  the KERNAL inside the same bus cycle; the Special button (boot 0) and the kill state exist to
  start the C64 as it is, so both vector bytes must come from the KERNAL, and a half-and-half
  vector would crash. TRX64 takes the access at the vector fetch: the pull is visible in the lines
  from the start of the reset, and the vector is read through the map the released lines give.
  Where the cartridge's own lines are Ultimax (boot 1, every reset but Special and kill) that is
  the same map, so "the reset vector is read in Ultimax" holds there.
- *Reset and Special in the kill state* do nothing (`if enable = '1'` in `cart_easyflash.vhdl`);
  the table's "mode unchanged" is the only row they have. Menu always acts.
- *A cart-generated reset keeps the bank* (D4), so a program has to select bank 0 before `$DE0F` = 0
  if it wants the slot's reset vector; skoe's menu does.
- *`overlay_bank_write`* takes `slot*64 + bank` as the bank.
- *VSF export* leaves an EF3 out: VICE has no such module and the EF module is a 1 MB EasyFlash.
- *The 8 MB checkpoint blob* is the whole chip, as for GMod4 and the other flash carts.

**Tests** (`crates/trx64-core/tests/ef3_gate.rs`, in `scripts/gate.sh`; `crates/trx64-daemon`
`ef3_status_buttons_and_savecrt`): one per acceptance item.
- A1 builds skoe's boot image from the local clone with his Makefile and local tools, boots it,
  checks the screen text by decoding the bitmap with the menu's own glyph tables (slot 5's name
  from the directory in slot 0, bank `$10`), starts slot 5 with its key, checks the Menu button and
  the version screen (`1.1.1`, read from `$DE08`). Skips, loudly, without the clone, a tool, or the
  ROMs. The third-party binaries of `EF3BootImage/images/` are not used; EasyProg is replaced by an
  empty file of the right shape. Two toolchain workarounds are applied to the scratch copy only: the
  stale prebuilt objects are removed and the linker configs get the `ONCE` segment cc65 2.18 wants.
- A6 assembles skoe's `eapi-mx29640b.s` and a test program with acme and runs them: `EAPIInit`
  gives `$CB`/`$C2`/8, a 64 KB sector in slot 5 (ROML and ROMH) and a boot block in slot 0 are
  erased and programmed, read back by the program, and the whole chip is compared against a model of
  exactly those edits. A program of 0 bits to 1 without an erase is an error the driver reports, as
  on the chip.
- A7: checkpoint through JSON and back, `crt_image` and reload, and a write into a slot the image had
  no packets for.
