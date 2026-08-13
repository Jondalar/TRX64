# TRX64 spec board

The one place to look for what is open here. Every spec under `docs/NNN-*.md` has a row,
and the row's status is checked against the file by `scripts/check-spec-board.sh` — the
board and the spec cannot disagree without something going red.

**Numbers are shared with C64RE.** The registry is `../C64ReverseEngineeringMCP/specs/README.md`
and the next free number comes from there. This board is the STATUS of TRX64's own
specs, never a second number registry — two registries is how a number gets used twice.

**Why this exists (2026-08-12).** TRX64 had no board. Status lived only in each file, so
nothing compared it to the tree, and two specs sat wrong for weeks: 791 said PROPOSED
with a shipped CLI command, a round-trip test and a parity probe behind it, and 790 said
PROPOSED with both its slices done in their own header. C64RE learned this the hard way
first — nine specs closed in one evening and not one needed building.

---

| # | Spec | Status | What is left |
|---|---|---|---|
| 776 | [Overlay intervention + diff](776-overlay-intervention-diff.md) | **SUPERSEDED** | Its subject — the active experiment loop, `run → intervene → diff` — was delivered by 795 (banked-cart overlay), 796 (candidate model, `candidate.rs` + its MCP tools) and 797 (final code delta). Nothing here is open; the charter reads as a plan for work that exists. Close it or fold the remainder into 796. |
| 783 | [Local quality-gate enforcement](783-local-quality-gate-enforcement.md) | **HALF BUILT** | The gates all exist: `seven_game_gate.rs`, `iso_vic_gate.rs`, `vic_collision_gate.rs`, `cart_mapper_gate.rs`, `tools/oracle/src/conformance.ts`, 7/7 screenshot oracles. The ENFORCEMENT does not — `.git/hooks/` is empty and no `core.hooksPath` is set. The spec says of itself "they exist — this is wiring + enforcement", and the wiring is the missing half. Consequence worth stating plainly: the regression protection that CLAUDE.md and every runtime commit point at holds only because someone runs `cargo test` by hand. |
| 790 | [BIN cartridge typed attach](790-bin-cartridge-typed-attach.md) | **BUILT** | S1 shipped and S2 built, both recorded in the spec's own header while the status line still said PROPOSED. Verify nothing is left, then close. |
| 791 | [VSF → `.c64re` converter](791-vsf-to-c64re-converter.md) | **BUILT** | `vsf.rs`, `vsf_export.rs`, `convert_cmd.rs` (`trx64cli convert-vsf`), `convert_vsf_roundtrip.rs`, `vsf_parity_probe.rs`, and a fidelity report in both directions. Said PROPOSED until 2026-08-12. Close it. |
| 792 | [Snapshot-restore fidelity](792-snapshot-restore-fidelity.md) | **RESOLVED** | Colour RAM was read from RAM-under-IO instead of `io_shadow`; fixed in `acec8bc`. Old dumps have the wrong colour RAM baked in and need re-dumping. |
| 793 | [Undump media materialization](793-undump-media-materialization.md) | **BUILT** | — |
| 794 | [Whitebox component diff](794-whitebox-component-diff.md) | **BUILT** | Both repos, e2e-verified. |
| 795 | [Banked cart code overlay](795-banked-cart-code-overlay.md) | **BUILT** | Both repos. |
| 796 | [Candidate model](796-candidate-model.md) | **BUILT** | Both repos; 7 MCP tools. |
| 797 | [Final code delta](797-final-code-delta.md) | **BUILT** | Both repos. |
| 798 | [Cheat candidate finder](798-cheat-candidate-finder.md) | **BUILT** | Both repos; subsumes 762. Full auto-cheat codegen still waits on a real target. |
| 802 | [Native trace read](802-native-trace-read.md) | **BUILT** | `trx64-traceindex`; the sidecar is deleted. |
| 803 | [Large cartridges](803-large-cartridges.md) | **PARTLY BUILT** | §5.1 (SPI flash) and §5.2 (GMod4) shipped 2026-08-09. Open: AGR (§6) and the vendor questions (§5.4). **No GMod3 sample exists** — not in hand, not in any archive looked at — so it means writing the first implementation AND the first test with hardware as the only oracle. The SPI core it needs is already paid for by GMod4. |
| 804 | [Symbolized runtime](804-symbolized-runtime.md) | **PROPOSED** | Untouched — no `symbol*.rs` in any crate. |

---

**BUILT** = shipped and verified. **PARTLY BUILT** = a named part is open, and the row
says which. **PROPOSED** = written down, nothing built. **SUPERSEDED** = another spec
delivered its subject.

There is no "in progress". Nothing here is being worked on right now, and a status that
claims otherwise is a lie the folder tells every visitor.

**The three rows that need a decision rather than work:** 776 (superseded — close it),
790 and 791 (built — close them). That is three of fourteen carrying a status that does
not match the tree, which is the argument for the check running beside this file.
