# Spec 790 — Raw `.bin` Cartridge Attach with a Typed Attach (`Auto` | `Forced`)

**Status:** PROPOSED (2026-07-15); **Slice 1 (S1) SHIPPED; Slice 2 (S2) BUILT
(2026-07-15) — the runtime self-configuring harness.** **Repo:** TRX64.
**Shared cross-repo numbering** (registry = C64RE `specs/README.md`).

> **Model correction (S1, 2026-07-15):** the attach type is **not** an
> unconditional "mandatory parameter". It is `CartType = Auto | Forced(MapperType)`.
> `Auto` is the default (a `.crt` is header-driven; a raw `.bin` runs the S1
> structural-only detect — §790.8 — which may return `BinTypeAmbiguous` and ask the
> caller for an explicit type). `Forced(t)` skips detection and uses `t`'s geometry
> (a `.crt` header override, or the raw-`.bin` split). The sections below are updated
> to this model; the original "mandatory type" framing is superseded.

## Motivation

We can attach a `.crt` (CHIP-packet container; empty banks may be omitted). We
**cannot** attach a raw `.bin` — the full linear flash/ROM image with **every
bank present, no CHIP headers**. A `.bin` carries **no cartridge type** in-band,
so the type (EasyFlash / Magic Desk / Megabyter / C64MegaCart / …) must be
supplied **out of band**.

VICE solves this with a UI prompt ("what CRT is this?") OR a per-type command-line
option. The prompt is UI-gedöns we don't want. We take the **CLI/API** route: the
type is supplied out of band by the human on the CLI or the LLM over the API
(`CartType::Forced`), with an `Auto` default that detects what it structurally can
(§790.8 S1) and asks for an explicit type only when it genuinely cannot tell. No
prompt, no silent guessing.

Leitregel: this is a **capability** → TRX64. C64RE consumes it (media ingress /
sandbox seed) but owns none of the mechanics.

## How VICE does it (source-cited, `vice/src`, read-only)

The reference mechanics we mirror (facts, not copied code):

- **One choke point:** `cartridge_attach_image(int type, const char *filename)`
  (`c64/cart/c64cart.c:1069`). `type == CARTRIDGE_CRT` (**`0`**, *not* `-1`;
  `-1 == CARTRIDGE_NONE`, `cartridge.h:179-180`) ⇒ read the hw type from the
  `.crt` header (`crt_getid`, `crt.c:188`; type word big-endian at header `0x16`,
  `crt.c:169`). Any **other** `type` ⇒ raw bin of that concrete type, header never
  consulted (`c64cart.c:1093-1102`, `:1130-1151`).
- **Raw dispatch:** `cart_bin_attach(type, filename, rawcart)` switch
  (`c64carthooks.c:909`) → per-type `*_bin_attach()`; unknown type → `-1`
  (`:1107`).
- **Raw layout = linear slurp:** every `*_bin_attach` calls
  `util_file_load(filename, rawcart, size, UTIL_FILE_LOAD_SKIP_ADDRESS)`
  (`util.c:343-386`) — one `fread` of the whole file into `rawcart`, an
  **exact-size gate** (`util.c:373 if (length != size) return -1;`), and an
  **optional 2-byte leading load-address strip** when `length & 2` (`util.c:368`,
  PRG-style prepended address). **Bank N lives at linear `N * banksize`;** there
  are no CHIP packets and no absent banks.
  - generic 8k/16k/ultimax: single bank, size-fallback + mirror-fill
    (`c64-generic.c:221-255`).
  - EasyFlash: `util_file_load(..., 0x4000 * 64, ...)` = 1 MiB, 64 banks × 16 KiB,
    de-interleaved to lo/hi flash in `config_setup` (`easyflash.c:493-495,535`).
  - Magic Desk: descending exact-size probe sets the runtime `bankmask`
    (`magicdesk.c:150-162`), 8 KiB banks.
  - Ocean: size table → `io1_mask = (size >> 13) - 1`, 8 KiB banks (`ocean.c:201-210`).
- **`-cartXXX` option table** (`c64carthooks.c:212`): every option passes a
  `CARTRIDGE_*` constant as `extra_param` → `cart_attach_cmdline`
  (`c64cart.c:629`) → `cartridge_attach_image(type, param)`. `-carteasy foo.bin`
  ⇒ `(CARTRIDGE_EASYFLASH, "foo.bin")`; `-cartcrt foo.crt` ⇒ `(CARTRIDGE_CRT=0, …)`
  (auto-detect). **This is exactly "mandatory type parameter, no prompt."**
- **`.bin` fills the same flash buffer as `.crt`:** for every flash cart, both
  `*_bin_attach` and `*_crt_attach` fill the same `rawcart`, then the shared
  `*_config_setup(rawcart)` populates the flash context (EasyFlash
  `easyflash.c:493-495`, GMOD2 `gmod2.c:275`, Megabyter `megabyter.c:306-308`).
  **The raw `.bin` IS the full flash image, byte-for-byte.**
- **Type IDs** (`cartridge.h`, positive = also the `.crt` header type, "must match"
  `cartridge.h:182`): `OCEAN 5`, `MAGIC_DESK 19`, `EASYFLASH 32`, `GMOD2 60`,
  `MAGIC_DESK_16 85`, `MEGABYTER 86 (0x56)`; sentinels `CRT 0`, `NONE -1`;
  VICE-internal negatives `GENERIC_8KB -3`, `GENERIC_16KB -2`, `ULTIMAX -6`.
- **C64MegaCart is NOT a mainline-VICE type** — only Martin Piper's fork defines
  `CARTRIDGE_C64MEGACART 61` (there is no `c64megacart_bin_attach` in mainline).
  Our `.bin` geometry for 61 is defined by **us**, fork-faithful (Spec follows the
  C64MegaCartMapper we already ship: 256 × 8 KiB ROML banks, 2 MiB M29F160FT).

## What already exists on our side (reuse, do NOT rebuild)

- `cart::parse_crt(data, path, Option<MapperType>)` → `ParsedCartridgeImage`
  (banks `BTreeMap<u16, CrtBank>`, `exrom`/`game`, `mapper_type`) — the CHIP-packet
  parser (`crates/trx64-core/src/cart.rs:323`).
- `cart::mapper_from_image(&image)` → `Box<dyn CartMapper>` — builds the concrete
  mapper from an image (`cart.rs:1547`). **Every mapper already knows its own
  geometry** (`build_linear_chip_data(image, accessor, bank_count)`).
- `cart::infer_mapper_type(hw_type, exrom, game, profiles)` → `MapperType` — the
  VICE-number → mapper map (`cart.rs:257`). Basis for the numeric type resolver.
- `Machine::attach_cart_from_bytes(bytes, name)` (`lib.rs:869`) +
  `Session::set_inserted_cart(bytes, name, path)` (`trx64-session/src/lib.rs:217`)
  — the attach choke points; both currently pass `None` (CRT-header type only).
- CLI `--cart` in `sandbox_cmd.rs` / `boot_cmd.rs`; daemon media/mount + cart-insert
  paths (`main.rs:5356, 8724, 8931, 9084`).

**Design pivot:** a raw `.bin` builder produces the **same `ParsedCartridgeImage`**
`parse_crt` does — only the source differs (linear `N*banksize` split vs CHIP
packets). So **all existing mappers work unchanged**; this spec adds a second
front-end, not a second mapper tier.

## What is NEW (this spec)

### 790.1 — `cart::parse_bin` (core)

```rust
pub fn parse_bin(
    data: &[u8], path: &str, name: &str, mapper_type: MapperType,
) -> Result<ParsedCartridgeImage, CrtError>;
pub fn load_cartridge_from_bin(
    data: &[u8], name: &str, mapper_type: MapperType,
) -> Result<(ParsedCartridgeImage, Box<dyn CartMapper>), CrtError>;
```

- Optional 2-byte load-address strip when `data.len()` is `bank_unit*k + 2` (VICE
  `SKIP_ADDRESS`, `util.c:368`) — a PRG-style prepended address on a generic bin.
- Split `data` into banks by the type's **geometry descriptor** (§790.7): for an
  8 KiB-bank ROML type, bank N `roml = data[N*0x2000 .. +0x2000]`; for a 16 KiB-bank
  type, `roml = data[N*0x4000 ..]`, `romh_a000 = data[N*0x4000+0x2000 ..]` (or
  `romh_e000` for ultimax). Set `profiles` + `exrom`/`game` from the descriptor.
- **Size rule (more lenient than VICE's exact gate, matching our existing
  0xFF-pad):** accept any `len == bank_unit * k`, `1 ≤ k ≤ max_banks`. Absent
  trailing banks are 0xFF (as `build_linear_chip_data` already pads). A size that
  is **not** a whole number of bank units, or `k > max_banks`, is a hard
  `CrtError::BadBinSize` (new variant) — never silently truncate.
- Produces the identical `ParsedCartridgeImage` shape; `load_cartridge_from_bin`
  then calls the unchanged `mapper_from_image`.

### 790.2 — type resolver: numeric ID **or** mnemonic → `MapperType`

```rust
pub fn resolve_cart_type(s: &str) -> Result<CartType, CrtError>;
```

Accepts (case-insensitive):
- a **numeric VICE ID** (`"32"`, `"86"`, `"61"`, `"-2"`, …) → `CartType::Forced`
  via the VICE numbering (reuse/extend `infer_mapper_type`'s table + the negatives
  for generic: `-3` 8K, `-2` 16K, `-6` ultimax).
- a **mnemonic** (LLM-friendly): `ef`/`easyflash`, `gmod2`, `megabyter`/`mb`,
  `c64megacart`/`c64mc`, `magicdesk`/`md`, `md16`, `ocean`, `8k`, `16k`, `ultimax`
  → `CartType::Forced`.
- `crt`/`auto`/`0` → `CartType::Auto` (the `CARTRIDGE_CRT` sentinel = "detect from
  header/structure"; §790.3).

Unknown string → `CrtError::UnknownCartType(String)` (new variant) with the valid
list in the message.

> **Return type note.** The sentinel `crt`/`auto`/`0` cannot be a bare `MapperType`;
> `CartType` is the type that carries **both** a concrete mapper and the auto
> sentinel, so `resolve_cart_type` returns `Result<CartType, CrtError>` (consumed
> directly by the CLI / `attach_cart_typed`).

### 790.3 — smart attach entry (`.crt` vs `.bin`, one door) — `Auto | Forced`

Extend the choke point so the daemon/CLI call **one** function, with an explicit
`CartType` intent:

```rust
pub enum CartType { Auto, Forced(MapperType) }

Machine::attach_cart_typed(&mut self, bytes, name, ty: CartType)
    -> Result<(String, MapperType), CrtError>;
```

Dispatch (mirrors VICE `cartridge_attach_image`, where `type == CARTRIDGE_CRT(0)`
is auto and any positive `CARTRIDGE_*` is forced):
- bytes start with `C64 CARTRIDGE   ` ⇒ `parse_crt`. `Forced(t)` is a header
  **override**; `Auto` is header-driven.
- else (raw `.bin`) ⇒ `Forced(t)` splits the linear image per `t`'s geometry
  (`parse_bin`). `Auto` runs the **S1 structural-only** first-cut detect (§790.8):
  eapi / CBM80 / reset-vector + size → a single confident type ⇒ `parse_bin(that)`;
  otherwise `CrtError::BinTypeAmbiguous`, telling the caller to pass an explicit
  `--cart-type`. (Resolving the genuinely ambiguous flash carts is the **S2** runtime
  self-configuring harness — §790.8 — not this slice.)

The old `attach_cart_from_bytes(bytes, name)` stays as a thin
`attach_cart_typed(…, CartType::Auto)` wrapper (no caller churn; `.crt`-only
behaviour unchanged). The daemon/session cart-attach call sites keep passing through
this wrapper (i.e. `Auto`) in S1; threading an explicit `CartType` on the daemon/
session API is the 790.5 wire work.

### 790.4 — CLI: `--cart-type` for a `.bin` (default `Auto`)

`trx64cli sandbox` (and any `--cart` consumer) gains `--cart-type <id|mnemonic>`.
Rule at the CLI boundary:
- `--cart-type` present → `resolve_cart_type` → `Forced` (or `Auto` for
  `crt`/`auto`/`0`).
- `--cart-type` absent → `CartType::Auto`.
- `--cart` is a `.crt` → `--cart-type` is an optional header override.
- `--cart` is a raw `.bin` under `Auto` → the S1 structural detect runs; if it can't
  settle, the attach fails with `BinTypeAmbiguous`, whose message names the mnemonic
  set (no prompt, no default-guess). Passing `--cart-type` resolves it.

(S1 wires `sandbox`. `boot` mounts via `media/mount` — the daemon API path, 790.5 —
so its typed `cart_type` rides that wire work, not the CLI flag.)

### 790.5 — API: `cart_type` field on the wire

Daemon cart-bearing requests (`media/mount`, cart-insert, `runtime_media_mount`)
gain an optional `cart_type: string` (id or mnemonic). Enforcement matches 790.4:
required when the mounted media is a raw `.bin`, ignored/override for `.crt`. This
is the LLM's hand-in path. The `cart_status` `type` wire string is unchanged
(already emitted per mapper, `mapper_type_str`).

### 790.6 — tests (gate)

Extend `crates/trx64-core/tests/cart_mapper_gate.rs`:
- `parse_bin` for an 8 KiB-bank type (Megabyter / C64MegaCart): a 4-bank bin →
  banks 0-3 populated, bank-select reads the right bank.
- `parse_bin` for a 16 KiB-bank type (EasyFlash): lo/hi split correct
  (`roml` vs `romh_a000` per bank).
- `resolve_cart_type`: id and mnemonic both resolve; unknown → error.
- size rules: non-multiple → `BadBinSize`; `k > max_banks` → `BadBinSize`;
  `len == unit*k + 2` → 2-byte strip then k banks.
- smart entry: raw bin + `Forced` → attaches; raw bin + `Auto` with eapi → EF;
  ambiguous raw bin + `Auto` → `BinTypeAmbiguous`; `.crt` bytes stay header-driven
  (with an optional `Forced` override).
- round-trip vs `.crt`: a `.bin` and the equivalent full `.crt` (all banks present)
  build byte-identical flash → identical reads.

### 790.7 — per-type geometry descriptors (the core table)

| Type | VICE id | mnemonic | bank unit | windows / bank | exrom/game | max banks |
|---|---|---|---|---|---|---|
| Generic 8K | −3 | `8k` | 8K | roml `$8000` | 0/1 | 1 |
| Generic 16K | −2 | `16k` | 16K | roml `$8000` + romh `$A000` | 0/0 | 1 |
| Ultimax | −6 | `ultimax` | 16K | roml `$8000` + romh `$E000` | 1/0 | 1 |
| Ocean | 5 | `ocean` | 8K | roml `$8000` (banks ≥16 → `$A000` in 512K) | 0/0 | 64 |
| Magic Desk | 19 | `magicdesk`/`md` | 8K | roml `$8000` | 0/1 | 128 |
| EasyFlash | 32 | `ef`/`easyflash` | 16K | roml `$8000` + romh `$A000` → lo/hi flash | boot ultimax | 64 |
| GMOD2 | 60 | `gmod2` | 8K | roml `$8000` | 0/1 | 64 |
| Magic Desk 16 | 85 | `md16` | 16K | roml `$8000` + romh `$A000` | 0/0 | — |
| Megabyter | 86 | `megabyter`/`mb` | 8K | roml `$8000` | 0/1 | 128 |
| C64MegaCart | 61 (fork) | `c64megacart`/`c64mc` | 8K | roml `$8000` | boot 8K game | 256 |

`exrom`/`game` are the `.bin`'s image lines (no header to carry them). For the
flash/banked mappers these are only a boot hint — the mapper self-determines lines
from its mode register at reset (e.g. C64MegaCartMapper boots 8K game). For the
generic mappers (NormalMapper) they are authoritative. The descriptor lives beside
each mapper's existing `build_linear_chip_data` bank_count so geometry has **one**
source of truth (`cart::bin_geometry(mapper_type) -> BinGeometry`).

### 790.8 — `Auto` detection has two tiers (S1 shipped here, S2 deferred)

A raw `.bin` attached with `CartType::Auto` has **no in-band type**, so detection is
tiered:

- **S1 — structural-only first-cut (this slice).** A single pass over the static
  bytes: (1) the EAPI signature `65 61 70 69` at bank-0 ROMH `$1800` (file offset
  `$3800` under the 16 KiB-interleaved layout) → EasyFlash; (2) the CBM80 autostart
  signature (`C3 C2 CD 38 30`) at ROML `$8004` on a single-bank image → generic 8K
  (`$2000`) / 16K (`$4000`); (3) a 16 KiB image with no CBM80 whose reset vector
  (`$3FFC/$3FFD`) points into `$E000-$FFFF` → ultimax. Anything else (e.g. a
  multi-bank flash dump, which has no reliable static marker) → `BinTypeAmbiguous`,
  which asks the caller for an explicit `--cart-type`. S1 never guesses a banked
  flash family from bytes alone.

- **S2 — runtime self-configuring cart harness (BUILT 2026-07-15).** In the `Auto`
  raw-`.bin` path, an image S1 cannot settle no longer errors `BinTypeAmbiguous`; it
  attaches `cart::SelfConfigCartMapper` (a `CartMapper`) instead. The harness holds
  the raw image + a generic AMD `Flash040`, computes a minimal **structural
  boot-config** (eapi at bank-0 ROMH `$1800` / a 16 KiB reset-vector into
  `$E000-$FFFF` → boot ultimax; else 8 KiB game so bank-0 ROML autostarts), and boots
  the image as a generic `$DE00`-banked cart. It then **watches** the register
  accesses the running loader makes and **locks the concrete type in-place** on the
  first type-specific one (SPECIFIC-FIRST):

  - `$DF00` (IO2) write → **C64MegaCart** (`$DF00` under the eapi cue → EasyFlash
    IO2-RAM instead);
  - `$DE00`-family write with **bit 1 set** (the mode register) → **EasyFlash** if the
    eapi signature is present, else **Megabyter**;
  - `$DE00` read used as an M93C86 EEPROM `DO` poll (after a CS-held/CLK-toggling
    write pattern) → **GMOD2** (the clock-edge guard prevents a false lock on a plain
    C64MegaCart high-bank number);
  - only `$DE00` banking for a long run with no specific access → the residual
    **Magic Desk** (Ocean at 512 KiB) family.

  On lock the harness re-parses the raw image with the concrete type's geometry
  (`load_cartridge_from_bin`), transfers the tracked bank-low, and delegates every
  subsequent `read`/`write`/`peek`/`get_lines`/state/writable call to the concrete
  mapper; `mapper_type()` returns the concrete type thereafter (the pre-lock value is
  `MapperType::SelfConfig`, surfaced by the daemon `cart_status` as `self_config`).
  The nice property — the FIRST cart-register write a loader makes is already the
  discriminator — was confirmed against two real raw flash dumps of one title in two
  cart formats: each boots on the generic config, then locks a **distinct** concrete
  type from the register its loader writes (`$DF00` → C64MegaCart, `$DE02` → Megabyter)
  ~1.8M cycles into boot, with no cross-contamination (the C64MegaCart image never
  touches `$DE02`; the Megabyter image never touches `$DF00`). Gate:
  `crates/trx64-core/tests/cart_bin_gate.rs` — synthetic per-discriminator locks
  (always-run) + a real-data lock gate (`--ignored`, globs the local fixtures, prints
  `sample #N` only, and fails-with-report rather than faking a pass if a fixture does
  not bank within the window).

## Non-goals

- No UI prompt / interactive type picker (the whole point — it's CLI/API only).
- No new mapper tier — `parse_bin` feeds the **existing** mappers unchanged.
- No `.bin` **export** here (writing flash back out as `.bin`) — read-in only;
  write-back stays the `.crt`/writable-image path (714).
- No VICE exact-size strictness — we accept a whole number of bank units and
  0xFF-pad the rest (consistent with `build_linear_chip_data`), but we still hard-
  reject a non-bank-multiple or over-max size (no silent truncation).
- Not a corpus/extraction feature (that's 784/785); this is machine ingress.

## Acceptance

1. `parse_bin` builds a correct `ParsedCartridgeImage` for at least one 8K-bank and
   one 16K-bank type; `mapper_from_image` yields the same mapper class as the
   equivalent `.crt` and reads byte-identical.
2. `resolve_cart_type` maps both numeric ID and mnemonic; unknown → a clear error
   listing valid values.
3. Smart attach: raw `.bin` + `Auto` that the S1 detect can't settle → the attach
   fails `BinTypeAmbiguous`; raw `.bin` + `Forced(t)` (or eapi under `Auto`) →
   attaches; `.crt` bytes still attach header-driven (type an optional override).
4. CLI: `trx64cli sandbox --cart X.bin` on an ambiguous raw bin **without**
   `--cart-type` fails with `BinTypeAmbiguous` naming the mnemonic set;
   `--cart-type c64megacart` (or `61`) attaches the bin. `.crt` still works with no
   `--cart-type`.
5. API: a `media/mount` of a raw `.bin` with `cart_type` attaches; without it, a
   typed error (not a silent no-cart).
6. Gate: the new `cart_mapper_gate` cases green; full workspace + `gate.sh` green.
7. A real end-to-end: dump an existing `.crt` cart's full flash to `.bin`, attach it
   via `--cart-type`, confirm it boots identically to the `.crt` (same first frame).
8. **(S2)** A raw `.bin` + `Auto` that the S1 detect cannot settle now **attaches the
   self-config harness** (`MapperType::SelfConfig`) instead of failing
   `BinTypeAmbiguous` — this supersedes the S1 "fails `BinTypeAmbiguous`" half of #3/#4
   for the machine attach path (the library `detect_bin_type` still returns the error;
   the attach door catches it and installs the harness). Booting the image and running
   it must lock a concrete family; two real fixtures of one title in different formats
   lock **distinct** types (C64MegaCart via `$DF00`, Megabyter via `$DE02`).

## Open questions

- **OQ1 — mandatory-vs-inferred for `.crt`-signatured `.bin`s.** A `.bin` never has
  the signature, so detection is unambiguous. But should a supplied `--cart-type`
  on a real `.crt` be an **override** (allow) or an **error** (reject mismatch)?
  Proposed: override, with a warning if it disagrees with the header — never silent.
- **OQ2 — Ocean / Magic Desk sub-size semantics.** VICE derives a runtime
  `bankmask`/`io1_mask` from the loaded size. Our mappers derive their mask from the
  highest bank present (`bank_mask_for_image`). Confirm a short Ocean/MD `.bin`
  (fewer banks) yields the same effective mask as VICE's size-derived one, or pin
  the mask from the descriptor's declared size. (Test 790.6 covers the equal-banks
  case; the short-bin case is the open one.)
- **OQ3 — mnemonic canon.** Freeze the mnemonic↔id table (790.7) as the public
  API contract before the CLI/MCP surface ships, so the LLM-facing names are stable.

## Follow-up (separate)

- TS-oracle parity: the deprecating TS runtime has no `.bin` front-end; not
  reconciled here (TS is going away). If a TS↔TRX64 conformance case ever needs a
  `.bin`, add it there then.
- Board line 789 (trace-under-armed-observers) is still unregistered on the C64RE
  board — a separate cleanup, noted here so 790 doesn't imply 789 is done.
