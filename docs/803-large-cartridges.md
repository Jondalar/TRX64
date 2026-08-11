# Spec 803 — Large cartridges: the SPI-flash family, GMod4, and vendor-sourced fidelity

**Status:** PARTLY BUILT — §5.1 (SPI flash) and §5.2 (GMod4 mapper) shipped 2026-08-09;
AGR (§6) and the vendor-question follow-ups (§5.4) remain open.

**GMod3 sample availability (2026-08-11): there is none.** Not in the user's hands, not in any archive we have looked at. That puts GMod3 in the same position as AGR — writing the first implementation AND the first test, with hardware as the only oracle. Worth weighing before starting: the SPI core it needs is already built and paid for by GMod4.

By contrast MegaByter and C64MegaCart now HAVE real samples (1 MB raw `.bin`, the same title in both mappers), which answered the register question by diff: `$de00` carries the bank in both; the second register is `$de02` for MegaByter and `$df00` for C64MegaCart. See C64RE spec 785 §3.1.
**Repo:** TRX64 (cartridge emulation). C64RE is unaffected — new mappers need no meaning-layer
change.
**Number:** 803 (shared board `C64ReverseEngineeringMCP/specs/README.md`).
**Framing:** not "add GMod4" but **the large-cartridge tier as a whole** — because the carts that
matter now (GMod3, GMod4, MegaByter, C64MegaCart) share one missing primitive, one design
question in our trait, and one shift in where truth comes from.

---

## 1. The landscape

| | GMod2 | MegaByter | C64MegaCart | GMod3 | GMod4 | AGR |
|---|---|---|---|---|---|---|
| **VICE** (+ the upstream patch) | ✅ | ✅ | 2 MB fork | ✅ | patch, unmerged | ❌ bit stored, unused |
| **Ultimate-64** | ✅ (2018) | ✅ (`large-cart`, 2026-07) | ❌ | ❌ `CART_NOT_IMPL` | ❌ absent | ❌ |
| **TRX64** (us) | ✅ | ✅ | ✅ 2 MB | ⏳ unblocked | ✅ **built** | ❌ deliberate |

Two things fall out of this table:

**We are not behind — we are level.** On GMod3, the Ultimate-64's `c64_crt.cc` literally carries
`{ 62, 0xFF, CART_NOT_IMPL, "GMod3" }`, the same standstill as our `62 => Unsupported`, for the
same reason: **SPI flash**. Nobody's second implementation exists to copy from.

**For GMod4 there is exactly one implementation in the world** (an unmerged VICE patch), and
**for AGR there is none at all.**

## 2. The one missing primitive

Our cartridge tier models *parallel* flash (`Flash040`: `$AAA/$555` unlock sequences, DQ status
polling, erase alarms). The whole blocked set needs *serial* flash instead — command, address and
data clocked bit-by-bit over CS/CLK/DI/DO lines exposed in a control register.

**One SPI-flash device model unblocks both GMod3 and GMod4.** That is not a guess: the patch
adds `src/core/spi-flash.c` as a **generic core** and moves `gmod3.c` onto it in the same diff.

We are not starting from nothing — GMod2's **M93C86 EEPROM** is already a serial device here,
driven by CS/CLK/data bits with the read returning a data bit in bit 7. The *shape* is modelled;
the SPI flash *device* is not. Chip identity matters (`$9F` JEDEC ID, `$90` ID): the GMod4 vendor
ships datasheets for W25Q64CV, EN25QH64A and Zetta ZD25Q32/64/128, and a `flash-identify` example
exists precisely because software must ask.

## 3. The design question in OUR code

`CartMapper::get_lines() -> CartLines` is **global**: one EXROM/GAME state for the whole cart.

GMod4 pulls the lines **per accessed area and on CPU reads only** — `$8000` pulls EXROM; `$A000`
pulls EXROM+GAME; `$E000` pulls GAME (ultimax, always showing bank #1, the point being custom
IRQ/NMI vectors); a disabled area pulls nothing. That is not expressible as one global state.

This is a **trait-shape** decision, not a mapper detail, and it should be settled before anyone
writes a GMod4 mapper. VICE's `c64cartmem.c` is in the patch, so the upstream answer is probably
there. Note we already have a hint of this problem: `cart.rs:99` mentions a read-path-dependent
memconfig for GMod3.

## 4. Where truth comes from now

Since the 2026-07-15 realignment, VICE is *Vorlage*, not authority. For the large-cart tier that
turns into three concrete sources, in descending order of trust:

1. **Vendor specification.** For **MegaByter** (Protovision) this is reachable through the team.
   Our implementation today is a port of VICE's port — a vendor document would let us verify
   against the *source* instead of against a reproduction. Same for GMod4: the iComp GitLab is
   public and ships the register header (`include/gmod4.inc`) plus working test software.
2. **The Ultimate-64's VHDL** (`fpga/cart_slot/vhdl_source/all_carts_v5.vhd`), **GPL-3.0** — licence-
   compatible with TRX64, so usable and not merely readable. For the carts it *does* implement it
   is a hardware description, i.e. closer to silicon than VICE's C for line behaviour and address
   decoding. Value: a **third opinion** when VICE and a vendor doc disagree — and it has already
   paid off, see §5.4: diffing 15 lines of its MegaByter against ours surfaced a straight
   contradiction in the mode encoding that neither implementation alone would have revealed.

   Its shape is worth understanding: the FPGA carries a **generic banking engine**, so each
   cartridge reduces to a decode rule (`c_megabyter` is 15 lines). That also bounds its
   usefulness — those rules describe *banking and lines*, never flash. It does **not** help past
   the SPI wall, and its `large-cart` branch work (4 MB carts on U64E-II) is an FPGA memory-budget
   problem that does not transfer to us — we just allocate.
3. **Real hardware, via a tester.** For **AGR this is the only possible oracle**, because no
   emulator implements it (see §6).

## 5. Scope

### 5.1 The primitive — **BUILT**

`crates/trx64-core/src/spi_flash.rs`, 9 tests. Two details a naive port gets wrong and
that are now pinned: page program **ANDs** into the image (flash only clears bits; only an
erase raises them), and the output phase — the device shifts DO on the RISING edge while
the vendor's `spi_read_byte` samples after the FALLING one, so the host reads the bit
placed by the PREVIOUS edge and the first data bit of a read appears during the last clock
of the *address* phase. Our first test made exactly that mistake; the vendor's assembler
settled it, not our reasoning. GMod3 needs no further primitive work.

Original scope:

Port an **SPI-flash device model** (reference: `src/core/spi-flash.c` in the patch, 129 lines
added). Must answer JEDEC/ID commands, support read (`$03`), page program (`$02`), write-enable
(`$06`), block erase (`$D8`), status (`$05`), and the reset pair (`$66`/`$99`).

Retires our `62 => Unsupported` (GMod3) as a side effect.

### 5.2 GMod4 — **BUILT (except AGR)**

`Gmod4Mapper` in `cart.rs` + `tests/cart_gmod4_gate.rs` (14 tests, including the vendor's
`$9F` JEDEC identify driven through the cartridge registers into the flash). The one
change outside the mapper: `CartMapper::fake_ultimax()` (default false) plus the two bus
fallbacks it gates — our bus showed open bus at `$C000`/`$E000` for a declined window,
correct for a real ultimax cart and wrong for this one. GMod3 will need the same.

Three things that are easy to get backwards, now pinned by tests: the ROM-enable bits are
**inverted**; reset deliberately does **not** clear the banking registers (the hardware
leaves them undefined and the vendor documentation requires software to initialise them,
so zeroing them would hide that class of bug); and banking-off means the bank bits drop
out of the address entirely, not "bank 0 selected".

Known limitation, shared with upstream: a `$DE00` write the cart declines lands in our I/O
shadow where VICE writes RAM underneath. Upstream has an open TODO in the same region
(intrusive mode should make `$DE00-$DE0F` always reach the register).

Original scope:

CRT hardware type **87** — free in our table, right after MegaByter's 86. (The number has moved
twice; an older summary says 83. The current patch defines 87.)

8 MB as 512 × 8K banks. Registers, per the vendor header:

```
$DE00  context A select (write) / SPI data-in on read (bit 7) when bitbang is on
$DE01/02/03  context A: $8000 bank / $A000 bank / common 16K bank
$DE09/0A/0B  context B: same
$DE04  CONTROL (mirrored at $DE0C-$DE0F; the block mirrors 16× across $DExx)
```

`CONTROL` bits, meaning switching on bit 0:

| bit | bitbang OFF | bitbang ON |
|---|---|---|
| 0 | 1 = enable SPI bitbang | — |
| 1/2/3 | 1 = disable ROM at $8000 / $A000 / $E000 | same |
| 4 | flash address line A22 | — |
| 5 | intrusive mode | SPI /CS (0 = selected) |
| 6 | enable banking registers | SPI data out |
| 7 | **AGR** | SPI CLK (latch on 0→1) |

Two banking contexts exist so an IRQ/NMI handler can switch banks without saving the foreground
set. Banking registers are **not initialised at power-up** — software must set them after every
reset, and our reset path must reproduce that rather than helpfully zeroing them.

**Two hardware generations with different register addresses.** The vendor README warns the repo
now describes the *2026 prototype*; the header keeps aliases — `$8000` bank was `$DE02` (now
`$DE01`), common bank was `$DE00` (now `$DE03`). Decide whether we serve one generation or both.

### 5.3 Snapshots — ours to solve

The patch explicitly does not implement snapshots. We cannot skip that: checkpoints, rewind and
`.c64re` are the product. `c64re_snapshot.rs` already carries GMod2/C64MegaCart serial state;
GMod4 adds context/control/SPI state.

### 5.4 MegaByter — two concrete questions for the vendor

We implement it (`register00` bank, `register02` mode+LED, `Flash040` `FLASH800_CB`, 128 × 8K =
1 MB, flash write included) as **a port of VICE's port**. Diffing that against the Ultimate-64's
independent FPGA implementation (`all_carts_v5.vhd`, `c_megabyter`, 15 lines) turns "verify
someday" into two decidable questions. Requested from the vendor 2026-08-09.

**Agreement first, so the disagreements are meaningful:** register decode is the same rule —
it tests `io_addr(1)`, we test `address & 2`, both mirroring across the whole `$DExx` page — and
it ignores the LED bit in `register_02` bit 7 as we do (it has no bus effect).

**Q1 — the mode encoding. The two middle modes are swapped between the implementations.**

Its lines are `game_n <= not mode_bits(1)` and `exrom_n <= mode_bits(0)`, with
`mode_bits(1 downto 0) <= io_wdata(1 downto 0)` (no reordering in between):

| `register_02 & 3` | ours (`cart.rs`) | FPGA (`exrom_n`, `game_n`) → meaning |
|---|---|---|
| `00` | 8K | (0,1) → 8K ✓ |
| `01` | **16K** | (1,1) → **cart off** ✗ |
| `10` | **RAM / off** | (0,0) → **16K** ✗ |
| `11` | ULTIMAX | (1,0) → ULTIMAX ✓ |

`00` and `11` agree; `01` and `10` contradict outright. One side is wrong, and being a port of a
port is no reason to assume it is theirs.

**Q2 — bank width: 7 bits or 8?** We mask `value & 0x7f` (128 banks); it assigns the full byte
(`bank_bits(21 downto 14) <= io_wdata`). Also worth confirming which address bits the bank value
drives, since its placement at 14+ and our `register00 * 0x2000` (8 K units) are not obviously
the same mapping.

**What that implementation does NOT answer:** it has no flash at all — those 15 lines are pure
banking, with no write path, erase or chip identification. Our `Flash040`/`FLASH800_CB` behaviour
has no second opinion in it, so the vendor's erase/program semantics and the device ID remain
worth asking about separately.

### 5.5 Out of scope for a first pass

- **AGR** (§6).
- **C128 behaviour** — untested even by the patch author.

## 6. AGR — the part with no reference anywhere

`CONTROL` bit 7. With it on, the VIC sees **RAM at `$1000` and `$9000`** instead of the character
generator, so bitmap and sprite data can live in the regions the char ROM normally shadows.

Verified state of the world:

- The patch's own `vice/readme.txt`: *"AGR mode is not working at all"*. Its code has
  `static int gmod4_agr_enabled = 0;` — the bit is **stored and never acted on**, and the diff
  touches **zero VIC files**.
- Ultimate-64: absent entirely.
- The vendor's example programs: `banking/main.asm` and `lib/gmod4.asm` mention AGR **once each**
  (constants), `stresstest` and `cartridge-skeleton` not at all. **No AGR test exists anywhere.**

For us the mechanism is a small, well-isolated change — `vic.rs:135` is the chargen overlay:

```rust
if (a & 0x7000) == 0x1000 {
    if let Some(cr) = self.char_rom { … }
}
```

AGR is one more condition there, fed from cart state. **The code is easy; the confidence is not.**
Implementing it means writing the first AGR implementation *and* the first AGR test, with real
hardware as the only oracle. That is allowed under the current doctrine — but it must be a
decision, not something discovered mid-port.

## 7. Verification

- **Conformance corpus:** the GMod4 vendor's own programs (`flash-identify`, `banking`,
  `flash-read`, `flash-write`, `stresstest`) as regression tests — far better than "does a game
  boot", which was all we had for C64MegaCart.
- **Cross-check** the carts we already ship against the Ultimate-64's VHDL and, where obtainable, vendor
  specs.
- **Hardware oracle** via a tester for anything neither VICE nor the Ultimate implements. A
  physical PCB is of no direct use to us — we cannot plug it into an emulator; what we need from
  the hardware side is *observations*.

## 8. Open questions

1. **Do we want AGR?** It is the feature that makes GMod4 interesting beyond "more banks", and it
   is the one with no reference and no oracle but hardware.
2. **Which GMod4 generation** is authoritative — 2026 prototype only, or both register layouts?
3. **Priority.** Nothing blocks on this today; it is capability ahead of demand. Weigh against
   the open items in Specs 801/802.
