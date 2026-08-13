# TRX64 spec board

The one place to look for what is open here. Every spec under `docs/NNN-*.md` has a row,
and the row's status is checked against the file by `scripts/check-spec-board.sh` — the
board and the spec cannot disagree without something going red.

**Numbers are shared with C64RE.** The registry is `../../C64ReverseEngineeringMCP/specs/README.md`
and the next free number comes from there. This board is the STATUS of TRX64's own
specs, never a second number registry — two registries is how a number gets used twice.

**Why this exists (2026-08-12).** TRX64 had no board. Status lived only in each file, so
nothing compared it to the tree, and four specs sat wrong for weeks: 791 said PROPOSED
with a shipped CLI command, a round-trip test and a parity probe behind it; 790 said
PROPOSED with both its slices done in their own header; 776 said PROPOSED for work three
other specs had delivered; and 783 said PROPOSED for something half built, where the
missing half is the enforcement everything else leans on. C64RE learned this the hard
way first — nine specs closed in one evening and not one needed building.

---

| # | Spec | Status | What is left |
|---|---|---|---|
| 783 | [Local quality-gate enforcement](783-local-quality-gate-enforcement.md) | **HALF BUILT** | The gates all exist: `seven_game_gate.rs`, `iso_vic_gate.rs`, `vic_collision_gate.rs`, `cart_mapper_gate.rs`, `tools/oracle/src/conformance.ts`, 7/7 screenshot oracles. The ENFORCEMENT does not — `.git/hooks/` is empty and no `core.hooksPath` is set. The spec says of itself "they exist — this is wiring + enforcement", and the wiring is the missing half. Stated plainly: the regression protection that CLAUDE.md and every runtime commit point at holds only because someone runs `cargo test` by hand. |
| 803 | [Large cartridges](803-large-cartridges.md) | **PARTLY BUILT** | §5.1 (SPI flash) and §5.2 (GMod4) shipped 2026-08-09. Open: AGR (§6) and the vendor questions (§5.4). **No GMod3 sample exists** — not in hand, not in any archive looked at — so it means writing the first implementation AND the first test with hardware as the only oracle. The SPI core it needs is already paid for by GMod4. |
| 804 | [Symbolized runtime](804-symbolized-runtime.md) | **PROPOSED** | Untouched — no `symbol*.rs` in any crate. |
| 807 | [Binary checkpoint ring](807-binary-checkpoint-ring.md) | **PARTLY BUILT** | §4.1 shipped: the per-frame capture no longer encodes the two VIC framebuffers it then threw away — 167 → 64 µs, tree 530 → 98 KB. The baseline that slice was measured against **overturned the spec's own premise**: JSON is not a CPU problem (two producers at 50 Hz cost 1.67 % of wall-clock even before the fix), it is a **memory** problem — a ring entry is 209 KiB resident while the ring accounts 64 KiB, so a 10 s per-frame window costs 102 MiB and reports 32. Open: §4.2–§4.6 (native struct, ring stores it, recorder on the same path, honest accounting, a `cadence` verb). |

---

**HALF BUILT** / **PARTLY BUILT** = a named part is open, and the row says which.
**PROPOSED** = written down, nothing built.

There is no "in progress". Nothing here is being worked on right now, and a status that
claims otherwise is a lie the folder tells every visitor.

**Closed specs are not here.** Eleven moved to [`_archive/`](_archive/README.md) on
2026-08-12 — everything BUILT, RESOLVED or SUPERSEDED. A folder of finished plans reads
as a backlog, and a reader has no way to tell which is which. `_archive/README.md`
carries what was DECIDED rather than the plans themselves; that is the part that would
otherwise be re-derived from an argument nobody remembers having.

**One row is worth reading twice.** 783 is the only spec here whose missing half is
load-bearing: every gate it names exists, and nothing runs them unasked.
