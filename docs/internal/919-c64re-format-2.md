# 919 — `.c64re` format 2: TRX64 writes what the board writes

**Status:** PROPOSED (2026-11-08), refined with the owner, ready to build.

## Why

The board (TRX64-Ultimate, "trx64u") has moved `.c64re` to format 2, because format 1 cannot work
there: no deflate compressor, a 7.8 MB heap, and a file the board wrote that it could not read
back. The owner, 2026-11-08: the emulator adapts, the limiting constraints are on the board and in
the gateware. Format 1 keeps loading; new files are only format 2.

Today a file is one-way at best. TRX64 reads format 1 only and refuses version 2 by name, so
nothing the board now saves opens in the emulator.

## Sources

TRX64-Ultimate `b2506f6` (branch `t66`), app `f3c8e991` (branch `t66-app`), Gitea only:

- `docs/specs/U8-c64re-v2-state-and-medium.md` (§3, the format),
  `docs/specs/U9-state-puts-the-medium-back.md`, `docs/adr/0067-…`;
- `software/io/c64/trx_c64re.cc` (`trx_c64re_dump_ex`, `header_json`, `block_section`, `stage_v2`,
  `trx_c64re_apply`), `trx_c64re_nodes.cc` (block bit positions, read off the gateware),
  `trx_ring.cc` (`trx_ring_cart_state_ok`, `replay_cart`), `trx_ring.h` (`TRX_BLOCK_WORDS`);
- `tools/c64re_read.py` + `tools/test_c64re_read.py`: an independent reader, the conformance check.

## The format (as the board has it)

All little-endian. Every section starts on a 4-byte boundary, the padding is zero and is covered
by the digest.

```
0..7    "C64RESNP"
8       u8 format version = 2
9..40   sha256 of everything from byte 41 to EOF
41..44  u32 header length
45..    header JSON (ASCII), pad to 4
        u16 section count, u16 0
        section table, 8 bytes each: u8 id, u8 flags, u16 0, u32 length
        payloads in table order, each padded to 4
```

| id | name | length | what |
|---|---|---|---|
| 1 | BLOCK | `4*blockWords + 8` (58 words today) | state-bus words, `u16 x`, `u16 flags` (b0: turbo valid), `u32 turbo pack` |
| 2 | RAM | 65536 | `$D000-$DFFF` is the RAM under I/O |
| 3 | COLOR | 1024 | low nibbles |
| 4 | CARTRAM | 65536 | only with a cartridge on |
| 5 | CARTROM | the medium | cartridge contents **at the freeze**, once |
| 6 | DRIVE | `sizeof(TrxDriveState)`, 2092 | drive A RAM and registers, not its image |
| 7 | EEPROM | 2048 | GMod2's 93C86 |

An unknown id is skipped by its length. A section that does not apply is not in the table.
A reader checks `blockWords` (refuses a mismatch) and `machine`. Header keys: `kind`, `version`,
`writer`, `appVersion`, `machine`, `bitstreamId`, `caps`, `caps2`, `blockWords`, `createdAt`,
`frame`, `cycles`, `pc`, `cartridgeType`, `source`, `mediumBytes`, optional `machineState`
(`cartridge`, `drive {image, mode}`, `settings`).

## Constraints that come from the board

- Not captured: SID, VIC internals and write-only latches, REU. A board state does not hold them.
- A state is a ring frame at a snap position (`x`), not an arbitrary cycle.
- Cartridge banking is replayed as IO1 bus writes. Types that cannot be replayed are refused **at
  save time**. U9 D0: what a dump holds, an undump puts back.
- No compression on the board. Peak heap is one section.
- `cartridgeType` is the firmware's cart byte (17 = EasyFlash), **not** the CRT hardware id.

## The problem this spec has to solve

TRX64's checkpoint holds far more than the board's: SID, VIC internals, full drive mechanics and
GCR, a second drive, 1581, REU, turbo, the ring's anchors. Anchors, sandbox seeds, feature files and
state maps (711, 905, 911, 912) depend on a dump coming back **exactly**. Sections 1-7 alone cannot
carry that.

## Decisions

- **D1 — Reader.** One entry point tells the versions apart at byte 8. Version 1 goes the existing
  way, unchanged. Every caller of `read_native_snapshot` keeps working.
- **D2 — Writer.** Every door that writes a `.c64re` (daemon dump paths, `boot`, `convert-vsf`)
  writes format 2 only.
- **D3 — What TRX64's own file holds (owner, 2026-11-08).** Sections 1-7 as the board reads them,
  plus one TRX64-only section with the rest of the checkpoint. The board skips it by length;
  TRX64 restores exactly from it. That section is gzipped inside (TRX64 has a compressor, the
  board never opens it).
- **D4 — A disk (owner, 2026-11-08).** The image goes into the TRX64-only section, so a seed or
  anchor stands alone; the path goes into `machineState.drive` as on the board, which mounts it
  from there if the file exists. No firmware change.
- **D5 — A board file in TRX64 (owner, 2026-11-08).** Load what is in it and report what is not.
  CPU, CIAs, VIC registers and raster position from BLOCK; RAM, cart RAM and flash from the
  sections; the SID starts silent until the program writes it. An image that cannot be found (a
  `/SD/...` path) leaves the drive empty, and the report says so. Never refused for that.
- **D6 — A TRX64 file on the board (owner, 2026-11-08).** TRX64 always writes: its own section
  brings everything back, so "what a dump holds, an undump puts back" holds for TRX64. On the board
  the board's own refusals apply. `machine` is the model as it is (the board does NTSC, owner); the
  firmware's load at `f3c8e991` still refuses anything but `c64-pal`, which is the firmware's to
  change. What the board would drop without refusing (a second drive, a REU, a 1581) is named in
  the header under `trx64.notOnBoard`.
- **D7 — Cartridge type.** One table maps the firmware's cart byte to `MapperType` and back.
- **D8 — The BLOCK.** Bit positions as in `trx_c64re_nodes.cc`, the inverse of what the firmware
  does for a format-1 file; words the emulator has no source for are zero, and listed.

- **D9 — The TRX64 section.** Id **128**, `TRX64`, flags 0: gzip of the JSON checkpoint with the
  values that sections 2-7 already hold removed, plus the disk images (D4). A reader that finds it
  restores exactly from it and takes RAM, colour RAM, cart RAM and flash from sections 2-5.
- **D10 — The cartridge once.** Section 5 is the flat contents at the freeze, as the firmware lays
  them out (bank n ROML at `n*0x4000`, ROMH at `n*0x4000+0x2000`, erased `$FF`, for 16K-bank carts;
  each type by its own geometry). `cartFlash` and the CHIP payloads of `cartBytes` are not stored
  again: the TRX64 section keeps the CRT header and the packet table without payloads, and the load
  rebuilds both. A type the firmware's table does not have writes no section 5, `cartridgeType` is
  absent, and its bytes stay in the TRX64 section.
- **D11 — The API does not move.** `write_native_snapshot` keeps its arguments and now writes
  format 2; `read_native_snapshot` returns the same checkpoint and media for either version, so no
  caller changes. The format 1 writer survives only as a test fixture.
- **D12 — A board file becomes a checkpoint.** A fresh machine's checkpoint with the file's values
  written in. The result carries the report of D5, through the same loaded/coarse/absent shape
  `convert-vsf` prints, and `snapshot/undump` returns it.
- **D13 — Header of a TRX64 file.** `writer: "trx64"`, `appVersion` the crate version, `machine`,
  `blockWords: 58`, `createdAt`, `frame`, `cycles`, `pc`, `cartridgeType`, `source`, `mediumBytes`,
  `machineState` as the board's; `bitstreamId`, `caps`, `caps2` are 0.

## Scenarios (each a test, red without the change, unless marked by hand)

- **S1 Format 1 still loads.** A file written by the fixture restores as before.
- **S2 New files are format 2.** Every write door: byte 8 is 2, digest right, sections in order and
  padded to 4, no bytes after the last.
- **S3 Exact round trip.** `snapshot_roundtrip_fidelity` and the existing drive, 1581, second-drive,
  turbo, Action Replay and folder-device gates stay green on format 2. A run continued from a file
  equals the run continued from the live machine.
- **S4 The cartridge once.** Dump after programming flash: section 5 holds the written byte, the
  payload appears in the file once, the restored machine reads it, and a CRT write-back still has
  the original packet layout.
- **S5 A disk stands alone.** Dump with a disk, delete the image, restore: the drive has it.
- **S6 A synthetic board file.** Built in the test from the spec: no section 128. CPU, CIA, VIC
  registers, raster position, RAM, cart bank come back; the report names SID, VIC internals, drive
  mechanics.
- **S7 Refusals.** Bad digest, wrong `blockWords`, truncated table, a section past EOF: refused by
  name. An unknown section id is skipped.
- **S8 Missing image.** A board file naming `/SD/x.d64`: loads, drive empty, the report says which.
- **S9 Type table.** Every firmware cart byte in the table maps to its `MapperType` and back.
- **S10 By hand.** `~/Downloads/wasteland_ef_v1.0.1-20261108-152059.c64re` loads and runs on: picture
  rendered, section 5 equals the flat CRT. It stays out of the repo.
- **S11 By hand.** A file TRX64 writes passes the firmware's `tools/c64re_read.py` with exit 0.
- **S12 By hand, on the board.** A TRX64 file loads on the board. The owner or TRX64U-FW runs it.

## Out

- The ring file (`.c64rering`). The board dropped writing rings; rings stay as they are here.
