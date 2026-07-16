# Spec 795 — Banked / Cart Code Overlay (lift the RAM-only overlay)

**Status:** PROPOSED (2026-07-16). **Repo:** TRX64 (capability) + C64RE (tool passthrough).
**Board:** row 795 on `C64RE:specs/README.md`. **Next free after: 796.**

**Base it extends:** Spec **769.2** — `runtime/overlay_run` (the snapshot-anchored code-overlay
what-if loop: rewind to an anchor → apply patches → run → observe, each call restores fresh so
the prior patch rolls back). Today the patch write is **RAM-only** (`machine.ram[addr]`).

**Yardstick:** `docs/usecases-runtime-dev-sandbox.md` priority #3 (overlay + inline-compile as
the working model). The owner chose, in a planning dialogue (2026-07-16), to build the overlay
**infrastructure first** and, within it, to **lift the RAM-only limit first** (banked/cart
overlay) — so own code can be injected where cart-resident game code lives (his EF corpus), not
just in RAM.

## Planning decisions (owner, 2026-07-16)

1. Infra-first (generic overlay workflow before a use-case vertical).
2. Banked/cart overlay first; the durable candidate model comes after.
3. **Explicit `space`+`bank` targeting** — no auto-resolve of a flat address to "whatever bank
   is currently mapped" (that is ambiguous for multi-bank code).
4. Spaces in scope: **`ram` + cart `roml`/`romh`**. C64 KERNAL/BASIC/CHAR ROM spaces are later.

## Model

### Patch schema (additive, back-compatible)

Each patch gains an optional `space` and `bank`:

```jsonc
{ "space": "ram" | "roml" | "romh",   // default "ram" → today's behaviour
  "bank":  3,                          // required for roml/romh; ignored for ram
  "addr":  "0x8123",                   // CPU window address
  "bytes": [0x4c, 0x00, 0x80],
  "read":  false }                     // optional read-back
```

- **`ram`** (default, unchanged): write `machine.ram[(addr+i) & 0xffff]`. A patch with no
  `space` is byte-identical to the current behaviour (back-compat gate).
- **`roml`** (bank window **$8000–$9FFF**) / **`romh`** ($A000–$BFFF): patch the cart mapper's
  bank image. `offset = addr - window_base`; for EF the linear flash offset is
  `bank * 0x2000 + offset` into `flash_lo` (roml) / `flash_hi` (romh). `addr` is the CPU window
  address the user reads in the bank disassembly — natural, no separate offset arithmetic.

### Where the write lands + rollback (ephemeral, like RAM)

Applied **after the anchor restore**, before the run. The cart write goes into the live mapper's
flash/bank image (a new core method `overlay_bank_write(space, bank, offset, byte)`), so the
running CPU sees the patched bank immediately. It is **ephemeral**: the next iteration restores
the anchor, and Spec 792 cart restore re-applies the original `cartFlash` — rolling the overlay
back. Same what-if semantics as the RAM overlay; the `.c64re` on disk is NOT mutated (persisting
a candidate = dump a new `.c64re`, or the later candidate model).

### Read-back

`read: true` returns the location's current byte: `ram` → `machine.ram[addr]`; `roml`/`romh` →
the mapper's byte for (bank, offset) (the patched flash image).

### Cart family

**EF (EasyFlash) first** — the owner's corpus, and the substrate exists: `cartFlash` in the
checkpoint (lo+hi 0x80000 each), `writable_image()`, the `cart_write` mapper path. The core
method `overlay_bank_write` is a mapper-trait method: **EF implements it**; other banked mappers
return a clear "banked overlay not implemented for <mapper>" error until added (documented
generalization, not a silent no-op).

## Surfaces

- **Core** (`trx64-core`): mapper-trait `overlay_bank_write(space, bank, offset, byte) ->
  Result<(), String>` (EF impl patches the flash image at the linear bank offset; default impl
  errors). A read helper for the read-back.
- **Daemon** (`trx64-daemon`): `runtime/overlay_run` patch loop reads `space`/`bank`; routes
  `ram` → `machine.ram` (today), `roml`/`romh` → `overlay_bank_write`. Applied-list + read-back
  echo the space/bank.
- **C64RE MCP** (`runtime_overlay_run`, `headless.ts`): patch schema gains optional `space`/
  `bank`; passthrough to the daemon. Tool description documents cart-bank overlay + the
  $8000/$A000 window convention.
- **TS runtime** (`ws-server.ts` `runtime/overlay_run`): the default backend is TRX64 (Spec
  771); the TS path stays RAM-only as the parity oracle (a cart patch there returns a "TRX64
  backend only" note) — not a second full implementation.

## Acceptance / gate

1. **RAM back-compat.** A patch with no `space` (or `space:"ram"`) lands in `machine.ram` and
   iterates/rolls-back exactly as before — byte-identical to pre-795.
2. **Cart overlay executes.** On an EF `.c64re` baseline, overlay bytes into
   `(roml, bank N, $8xxx)` that change control flow (e.g. plant a `JMP`/`RTS`), run with an
   `until_pc` breakpoint, and confirm the patched code ran (hitPc / observed state), OR read-back
   shows the patched byte when bank N is mapped.
3. **Ephemeral rollback.** After one cart-overlay iteration, the next anchor restore serves the
   ORIGINAL bank byte (read-back proves the overlay is gone).
4. **Bank isolation.** Patching bank N does not change bank M (a read-back of M is unchanged).

## Scope boundary (stated, not silent)

- **No assemble-source integration** — bytes are pre-assembled (as the RAM overlay is today via
  `assemble_source`). The source→overlay tie ("compile this asm at the bank window") lands with
  the **candidate model** (the owner's deferred 2nd step).
- **No C64 ROM spaces** (kernal/basic/char) — later.
- **No persistent flash mutation** — the what-if loop is ephemeral; persist by dumping a `.c64re`.
- **Non-EF banked mappers** error clearly — generalization deferred.

## Cross-links

Spec 769.2 (overlay_run base) · 713 (cart fidelity) · 790 (`.bin` typed attach) · 792
(cartState / cart restore = the rollback mechanism) · 794 (the whitebox eval of the overlaid
run) · yardstick `docs/usecases-runtime-dev-sandbox.md` #3 ·
concept map `docs/concepts-snapshots-scenarios-overlays.md`.
