# 907 (TRX64 half) — A block read is filed under its own sector

**Status:** DONE — merged to main 2026-10-14 (TRX64 half; the C64RE half is C64RE 907 D1/D3/D4).
Tests: `crates/trx64-core/tests/block_read_907.rs`, `rotation::tests::block_region_*`,
`trx64-traceindex` `block_read2_fields_width_and_legacy_0x35`, `trx64-trace`
`write_block_read2_byte_layout`. Decision in `README.md` here.
**Repo:** TRX64 (core, trace, traceindex, daemon). The C64RE half is
`C64ReverseEngineeringMCP/specs/907-a-loader-capture-tells-the-truth.md`.
**Number:** 907 (board: `C64ReverseEngineeringMCP/specs/README.md`)
**Origin:** C64RE issue #77 (Impossible Mission II): a custom-GCR loader reads a raw block
from `$1C01` after a standard header; the read-set filed it under the next sector.
Patch: additions only.

## 1. What was wrong

- The read-set credited bytes with `sector_under_head()`, which looks FORWARD to the next
  header: over header N and data N it reports N+1.
- `arm_head_trace(true)` cleared the buffers at every `session/run`, so a read spanning two
  runs lost its bytes; the bucket open at trace stop was never written.
- BLOCK_READ (0x35) carried only the drive clock, which restarts at 0 on a drive reset and runs
  at a different rate from the C64 clock (`sync_factor`), so no constant offset relates them.

## 2. What it does now

- `Rotation::block_region_at_head()` returns the block the head is IN: the last header (0x08)
  or data (0x07) sync at or before the head, wrapping around the track; a data block takes the
  sector of the header before it. The block list of a half-track is cached and rebuilt when
  the half-track, its GCR bits (`track_gen`: head write, attach, detach, snapshot restore) or
  its size change. A sync that straddles the index is found.
- Per sampled drive step the bytes latched since the previous step go into the open bucket
  of (half-track, sector), as header or data bytes. A change of (half-track, sector) closes the
  bucket. No header on the half-track: sector 0xff, all bytes data.
- Only a false→true arm clears; a true→false disarm and the daemon's trace finalize flush the
  open bucket; finalize then disarms.
- New record **BLOCK_READ2, opcode 0x37**, 23 bytes: op(1), drv_clk f64 LE(8), c64_clk f64
  LE(8), half-track u8, sector u8, header_bytes u16 LE, data_bytes u16 LE. Both clocks are taken
  when the bucket opens. New captures carry 0x37 instead of 0x35; 0x35 still decodes.
  `FORMAT_VERSION` stays 2 (a reader without 0x37 stops on an unknown opcode either way, as with
  0x36).

## 3. Decisions taken while building

- **D1 — the C64 clock in the record, not an offset.** The drive runs at 1 MHz, the C64 at its
  model's rate; an offset per drive reset cannot map one onto the other. Each record carries
  `c64_clk`, the C64 master clock at the same sample, and that is the one to compare with
  C64-side events. `drv_clk` stays for drive-side joins.
- **D2 — bytes go to the region at the sample.** A drive instruction is shorter than one GCR
  byte, so the region at the end of the instruction is the region of the byte it latched.
- **D3 — finalize disarms.** Otherwise the next trace's first arm is not a transition and the
  previous trace's bucket leaks into it.
