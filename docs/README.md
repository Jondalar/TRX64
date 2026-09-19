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
| 803 | [Large cartridges](803-large-cartridges.md) | **PARTLY BUILT** | §5.1 (SPI flash) and §5.2 (GMod4) shipped 2026-08-09. Open: AGR (§6) and the vendor questions (§5.4). **No GMod3 sample exists** — not in hand, not in any archive looked at — so it means writing the first implementation AND the first test with hardware as the only oracle. The SPI core it needs is already paid for by GMod4. |
| 804 | [Symbolized runtime](804-symbolized-runtime.md) | **PROPOSED** | Untouched — no `symbol*.rs` in any crate. |

| 808 | [Rewind transport](808-rewind-transport.md) | **PARTLY BUILT** | Play the machine backwards. The design turns on one measurement — a full restore is **177 µs** — so backward playback moves the MACHINE rather than replaying cached pictures, and every existing viewer follows for free. Built: the daemon owns the transport state, the monitor verbs, `transport/play|pause|goto|frame|toggle|status`, F9–F12 in the TUI and the window off ONE shared key table, the frame lens (a capture on an anchor can show the frame the redraw threw away), and `transport/key` — a client hands over the KEY and the daemon answers what it did or that it dropped it, which is what the browser needed since it carried no copy of the table and F9–F12 were simply dead there. **This row said PROPOSED while all of that was shipped**; the spec said it too, so the board gate stayed green on two stale claims. Open: the C64RE ribbon in the scrub UI — the browser has the keys and no visible transport controls. |
| 809 | [Marks and sandboxes](809-marks-and-sandboxes.md) | **PARTLY BUILT** | Marks shipped: named + pinned anchors that survive PLAY cutting the future (the centre of gravity — three attempts from one mark give the identical machine and the mark outlives them), a cap of 32 that REFUSES rather than shrinking the window silently, labels riding the ringdump so a `.c64rering` is a session with its bookmarks, and a name working as an anchor id everywhere. Sandboxes shipped as a bare capability: `sandbox/run` / `runMany` return a state with no name, no verdict and no comparison — gated on carrying none of those, because 810 owns the meaning. Open: copy-on-write media folders per run, and multi-line assembly. |
| 863 | [NTSC](863-ntsc.md) | **READY** | A second video standard, chosen before power-on: a video model record (6567R8: 65×263, 1 022 730 Hz, 60 Hz TOD; the cycle table ported 1:1 from VICE `cycle_tab_ntsc`), one timing record every frame and clock consumer reads (today 12 copies of 19656, 7 of the clock), NTSC's wrapped display window, the standard recorded and enforced in every snapshot (a cross-standard restore can panic today). PAL bit-identical. Settled: models are rows of `models.toml`, the switch is a transplant at the frame boundary, the palette stays Colodore. |
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

**The gate is the pre-push hook, and it is the only thing running the tests.** There is
no cloud CI (783). `hooks/pre-push` → `scripts/gate.sh`, bypassable for one push with
`GATE_SKIP=1`, which is a decision to make on purpose and not a habit.
