# 892 — EasyFlash 3 as a cartridge

**Status:** PROPOSED (2026-10-04) — scope set by the owner; decisions D9 and D10 open.
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

**D9 — Out-of-scope modes selected by a program (2, 4, 5, 6). OPEN.**

**D10 — The menu in slot 0.** TRX64 runs whatever the image holds. A full EF3 image boots its
menu from slot 0, bank 0, ROMH `$FFFC` (the trampoline at `$FF00` → bank 8). TRX64 has no menu
of its own. **OPEN:** whether a type-90 image without a menu in slot 0 should be refused,
warned about, or just run.

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
4. `$DE0F`:
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

- D9: what a program selecting mode 2, 4, 5 or 6 gets.
- D10: a type-90 image without a menu in slot 0.
- Whether the chip compares the high address bits in the unlock cycles. The EAPI issues them
  at slot 0 / bank 0, which satisfies both readings. TRX64 compares the low 12 bits, as
  `FLASH040_160` does (mask `$FFF`).
