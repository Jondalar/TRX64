# 918 — Protovision TwoMegabyter as a cartridge

**Status:** DONE — merged to main 2026-11-06 (`99319bd`), C64RE half on master (`f50c7c1a`); ships with the next release.

## Why

No TwoMegabyter game exists yet. To look at one, someone first has to build one, from
Wasteland 2 or the LN engine for example. TRX64 has to run that cartridge so the build can
be looked at. The U64 emulator and then the firmware project build on top of it. Today CRT
hardware type 89 is reserved and nothing more: `cart.rs` has no entry for it, so the CRT is
refused.

## Sources

- **Protovision's flash API 1.0.0** by Chester Kollschen / Knights of Bytes (`twomegabyter.ca65.s`,
  `twomegabyter-examples.ca65.s`, `twomegabyter-highscore-demo.ca65.s`, `boot.ca65.s`). The owner
  has it locally. It carries no licence, so it **never goes into this repo**: no copy, no quote of
  its code, no test that reads it. Behaviour is re-implemented from what the code does.
- **The TRX64-Ultimate descriptor** `tools/cart_descriptors/2f_twomegabyter.json`, which cites
  TREX-PROP-002 §9.2. It is ours too, so it does not count as independent proof. Where it goes
  further than the API code, this spec says so (D3, D4).
- **Mainline VICE has no TwoMegabyter.** `megabyter.c` belongs to the MegaByter, a related
  but different cart: 1 MB, ROML only, and a different mode table.

## What the API code establishes

These points come from the instructions themselves, not from comments.

- **Flash:** 2 MiB, addresses $000000-$1FFFFF, 32 sectors of 64 KiB each.
  - Commands use AMD unlock cycles: `$AA`→`$8555`, `$55`→`$82AA`, then:
    - program `$A0`→`$8555`;
    - sector erase `$80`→`$8555`, `$AA`/`$55`, `$30`→ the sector address;
    - chip erase `$80`, `$AA`/`$55`, `$10`→`$8555`.
- **Banks:** 128 banks of 16 KiB each.
  - `$DE00` takes bank = flash address bits 20..14 (`twomegabyter_set`: `(A & $1F) << 2 | Y >> 6`).
  - Offset `$0000-$1FFF` within a bank is ROML (`$8000-$9FFF`).
  - Offset `$2000-$3FFF` is ROMH: read at `$A000-$BFFF` in 16K mode, written at `$E000-$FFFF` in Ultimax.
  - The bank increments when `$BFFF` wraps (`twomegabyter_next`).
- **Mode register `$DE02`:**
  - `0` = 16K: reads at `$A000+` after `command_end` must see the flash.
  - `1` = Ultimax: every command and program write goes here.
  - `3` = cartridge off: the kill path jumps through `($FFFC)` to the KERNAL.
- **Status polling:**
  - Program waits until bit 7 of the read-back equals bit 7 of the written byte (DATA# polling).
  - Erase polls `$8000` until bit 7 = 1.
- **Boot:** CBM80 at `$8000`, so the cart starts in 16K (or 8K) with bank 0.

## Decisions

- **D1 — CRT hardware type 89.** Shared with TRX64-Ultimate, TRX_CRT_cli and C64RE.
  - Mnemonic `twomegabyter` / `tmb` for `--cart-type`.
  - The CRT layout matches EasyFlash: one CHIP packet per 8 KiB, bank field = bank (0..127).
    Load `$8000` is ROML; `$A000` or `$E000` is ROMH.
  - Header EXROM=0, GAME=0.
  - An image smaller than 2 MiB fills the rest with `$FF` (erased flash).
- **D2 — Power-on and reset state.** Bank 0, mode 0 (16K).
- **D3 — Mode 2 = 8K.** The API never writes 2. The values 0, 1 and 3 together read naturally as "bit 0 = EXROM line, bit 1 =
  GAME line" (the descriptor says the same). Under that reading, 2 is EXROM low, GAME high,
  which is 8K. Taken over from the descriptor and marked here as such.
- **D4 — Register decoding.**
  - IO1 writes decode A1: clear writes `$DE00` (bank, 7 bits), set writes `$DE02` (mode, 2 bits).
    This is the same as MegaByter and the descriptor.
  - IO1 reads and IO2 are open bus: the API never reads a register, and nothing shows them to be readable.
  - Both are taken from the descriptor, and both are changed in one place if hardware says otherwise.
- **D5 — Flash chip MX29F016.**
  - Manufacturer `$C2`, device `$AD`.
  - Uniform 32 × 64 KiB sectors.
  - Unlock addresses `$555`/`$2AA` masked with `$7FF`, so the bank does not matter.
  - Built on `Flash040` as its own type profile, next to `FLASH040B_XL`, which already has the
    32-sector decode, with its own ID.
  - Busy and polling behaviour is whatever `Flash040` already does for its other AMD parts.
- **D6 — Flash is written only through ROML and through ROMH in Ultimax.** A write in 16K or 8K
  mode goes to C64 RAM, as on every other flash cart here.
- **D7 — Persist and snapshot.**
  - Flash writes survive reset and snapshot (`.c64re`), as on the other flash families.
  - `crt_image` writes back every written bank. That includes banks the CRT had no CHIP packet
    for: the #23 lesson, and the same rule as `EasyFlashMapper::crt_image`.
- **D8 — Cart overlay and bank patch.** These follow the other flash families, with bank =
  register00 and flash cloned. `cart_bin` gets a geometry with bank unit 16 KiB, layout ROML+ROMH,
  and 128 banks.
- **D9 — Lens and monitor.** `peek_lens` "cart" and the monitor's cart verbs show the current
  bank as they do for EasyFlash.

## C64RE half

- `cartridge-type.ts` gives `NAMES[89] = "Protovision TwoMegabyter"`.
- `cartridge-io.ts` handles 89:
  - names: `twomegabyter_bank` at `$DE00`, `twomegabyter_mode` at `$DE02`, decoded by A1 and
    repeating every 4 bytes in IO1;
  - register text: bank bits 0-6, and mode with bit 0 EXROM and bit 1 GAME (0 = 16K, 1 = Ultimax,
    2 = 8K, 3 = off).

## Scenarios (each one is a test that is red without the change)

All test code is our own: 6502 written in the test as bytes or with the in-tree assembler,
never PTV's.

- **S1 Boot.** A CRT of type 89 with CBM80 in bank 0 starts, and the CPU runs code from `$8000`.
  `$A000` reads ROMH of bank 0 in mode 0.
- **S2 Banking.** Write `$DE00`=n.
  - `$8000` and `$A000` read bank n's ROML and ROMH, for n = 0, 1, 64, 127.
  - Bit 7 of `$DE00` is ignored.
  - `$DE04` acts as `$DE00`, and `$DE06` as `$DE02` (A1 decoding).
- **S3 Modes.** Check all four `$DE02` values against the PLA:
  - 16K: `$8000` and `$A000` from the cart;
  - Ultimax: `$8000` and `$E000` from the cart, `$A000` open;
  - 8K: only `$8000`;
  - off: RAM and KERNAL.
- **S4 Program.**
  - In Ultimax, unlock and program a byte at bank 37 offset `$0123` (ROML) and at bank 37
    offset `$2345` (ROMH, written at `$E345`).
  - Polling bit 7 ends.
  - Back in 16K, `$8123` and `$A345` read the bytes.
  - Only 1→0 bits change.
- **S5 Sector and chip erase.**
  - Erasing sector 31 (banks 124-127) sets exactly those 64 KiB to `$FF`. Bank 123 stays unchanged.
  - Chip erase sets all 2 MiB to `$FF`.
  - Polling `$8000` bit 7 ends.
- **S6 ID.** The autoselect read gives `$C2`/`$AD`.
- **S7 Persist.**
  - Programming a bank the CRT has no packet for, then `crt_image` and a remount, reads the byte back.
  - Reset and a snapshot round trip keep the flash.
- **S8 By hand, on the owner's machine, not in the repo.** Assemble PTV's examples and the
  highscore demo with ca65 locally. Run them on TRX64. Erase, write and read back must agree.
  The result goes into this file. The PTV sources stay outside the repo.

## As built

- `TwoMegabyterMapper` in `cart.rs`, flash profile `FLASH_MX29F016` in `flash040.rs`; mnemonics
  `89`, `twomegabyter`, `tmb`. A CHIP packet for a bank above 127 is refused (`BankOutOfRange`),
  not dropped.
- Programming a byte that asks for a 0->1 bit leaves the AND of old and new in the cell and the chip
  in its DQ7 error state until a `$F0` reset, as `Flash040` does for every AMD part here.
- C64RE also got a type 89 row in `CART_TYPE_PROFILES` (`view-builders.ts`), so the bank view
  covers it like 86-88.
- Tests: `tests/cart_twomegabyter_gate.rs` (S1-S7, 17 tests), plus cases in `cart_bin_gate`,
  `cart_overlay_rollback` and `cart_overlay.rs`; C64RE `e2e-908-io-names` 63/63.

**S8 (2026-11-06, by hand).** The TRX64-Ultimate test cartridge (`tests/cart/twomegabyter/build.sh`
there: Protovision's boot block, flash API and highscore demo, assembled with ca65 from the local
zip, plus bank markers in banks 0-123), built in a scratch directory and booted with
`trx64cli boot` twice, each time in a fresh process:

- bank errors 0 (markers of all 124 banks read through `$DE00`), mode errors 0 (`$DE02` = 2, 3,
  1, 0), save errors 0, green border;
- the save counter read 1 on the first run and 2 on the second: the highscore demo erased and
  wrote sector 31, and the write-back added the CRT packet for bank 124 (2033664 -> 2041872 bytes).

## Out

- **A real game CRT.** It comes from a build (TRX_CRT_cli has no TwoMegabyter target yet; that is
  its own work in its own repo).
- **The LED.** No register bit is known.
