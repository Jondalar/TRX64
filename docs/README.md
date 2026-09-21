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

| 808 | [Rewind transport](808-rewind-transport.md) | **PARTLY BUILT** | Play the machine backwards. The design turns on one measurement — a full restore is **177 µs** — so backward playback moves the MACHINE rather than replaying cached pictures, and every existing viewer follows for free. Built: the daemon owns the transport state, the monitor verbs, `transport/play|pause|goto|frame|toggle|status`, F9–F12 in the TUI and the window off ONE shared key table, the frame lens (a capture on an anchor can show the frame the redraw threw away), and `transport/key` — a client hands over the KEY and the daemon answers what it did or that it dropped it, which is what the browser needed since it carried no copy of the table and F9–F12 were simply dead there. **This row said PROPOSED while all of that was shipped**; the spec said it too, so the board gate stayed green on two stale claims. Open: the C64RE ribbon in the scrub UI — the browser has the keys and no visible transport controls. |
| 809 | [Marks and sandboxes](809-marks-and-sandboxes.md) | **PARTLY BUILT** | Marks shipped: named + pinned anchors that survive PLAY cutting the future (the centre of gravity — three attempts from one mark give the identical machine and the mark outlives them), a cap of 32 that REFUSES rather than shrinking the window silently, labels riding the ringdump so a `.c64rering` is a session with its bookmarks, and a name working as an anchor id everywhere. Sandboxes shipped as a bare capability: `sandbox/run` / `runMany` return a state with no name, no verdict and no comparison — gated on carrying none of those, because 810 owns the meaning. Open: copy-on-write media folders per run, and multi-line assembly. |
| 868 | [A colour per pixel under turbo](868-a-colour-per-pixel-under-turbo.md) | **PROPOSED** | The first thing TRX64 cannot render that real hardware does, found by UE2 running Xander Mol's Mandelbrot Upic on the U64 firmware. Aleksi Eeben's UPic technique blanks the display and writes 384 values to `$D020` per raster line — one per VIC pixel — which a 64 MHz CPU can do and a 1 MHz one cannot. Our VIC samples the register once per PHI2 cycle, so the picture arrives at one eighth of its horizontal detail. The fix is cheap because `turbo_phase` already IS the sub-cycle position: the CPU mirrors it, a colour store fills eight per-cycle slots (carrying forward, last write wins), and the override applies where a token is resolved into a colour rather than where the token is placed — so the 6569's one-pixel latency still rides the same pipeline and `draw_border8` does not change. Gated on the U64 profile with `turbo_div > 1`, which keeps every C64 and C128 byte-identical to VICE — proved by the 7-game screenshots. `$D021` is a second entry, not a second mechanism; the display-logic registers are explicitly out. | 2026-09-21 |
| 864 | [The monitor as a library](864-the-monitor-as-a-library.md) | **PARTLY BUILT** (v0.8.2) | The UE2 emulator runs the unmodified U64 firmware over trx64-core and has no way to look at the 6510. Its API shows a screen and sends keys; re-implementing the verbs there would drift within a release. So the monitor becomes `crates/trx64-monitor` (core + static only) and the daemon becomes its first host: verb dispatch, formatting, MonitorState, Breakpoints, the observer registry with its condition AST, the flow tracker, the trap rules and the one-line assembler all move; the host trait keeps what only a host can mean — who advances the machine, what a mark is, where a trace goes. The lib classifies what a verb DID (reads / mutates / replaces the machine) so no host holds a verb list; reset is intercepted rather than announced, because UE2 must go through the firmware; host verbs register into the dispatch, because the monitor is modal and nothing may sit in front of it. `TeeObserver` moves to core, where a combinator over core's own trait belongs. The wire contract does not move: C64RE needs no change. | 2026-09-20 **As built:** the crate (core + static, nothing else), the host trait, `TeeObserver` moved to core, and every verb that needs only a machine — the library sees each line first, so the modal prompt and assemble mode are its business on both hosts. main.rs 25350 → 22871. **Left:** run control, reset, and each verb that needs a timeline, a file or a trace sink; their service traits are still guesses, and `CpuView` is declared but not yet on any path, so `device fw` cannot answer yet. Golden transcript byte-identical throughout (one commit on that file: the one that recorded it). | 2026-09-20 |
---

**804 moved to C64RE (2026-09-19) and is built there.** TRX64 holds no symbols: names are
joined in C64RE, which owns meaning. What TRX64 delivers for it — address spans on
`monitor/exec`, the banking state (`monitor/state`), the structured `monitorDisasm` fields,
`read_memory space:"drive8"` — is recorded in
`../../C64ReverseEngineeringMCP/specs/_archive/804-symbols-joined-in-c64re.md`.

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
