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
| 783 | [Local quality-gate enforcement](783-local-quality-gate-enforcement.md) | **BUILT** | The gates all exist — `seven_game_gate.rs`, `iso_vic_gate.rs`, `vic_collision_gate.rs`, `cart_mapper_gate.rs`, `tools/oracle/src/conformance.ts`, 7/7 screenshot oracles — and since 2026-08-14 something RUNS them: `core.hooksPath=hooks` is set, `hooks/pre-push` fires on every push and calls `scripts/gate.sh`, and 783.2 decides which pushes need the full gate. This row said HALF BUILT for six days after the spec re-measured itself, which is exactly the drift rule 9 names; the reading it was based on predates `scripts/install-hooks.sh` having been run on this machine. |
| 803 | [Large cartridges](803-large-cartridges.md) | **PARTLY BUILT** | §5.1 (SPI flash) and §5.2 (GMod4) shipped 2026-08-09. Open: AGR (§6) and the vendor questions (§5.4). **No GMod3 sample exists** — not in hand, not in any archive looked at — so it means writing the first implementation AND the first test with hardware as the only oracle. The SPI core it needs is already paid for by GMod4. |
| 804 | [Symbolized runtime](804-symbolized-runtime.md) | **PROPOSED** | Untouched — no `symbol*.rs` in any crate. |
| 807 | [Binary checkpoint ring](807-binary-checkpoint-ring.md) | **BUILT** | All six slices. The per-frame capture no longer encodes the two VIC framebuffers it threw away (167 → 64 µs), and a ring entry no longer holds a live `serde_json::Value` (208.9 → 97.7 KiB, so 10 s at cadence 1 is 48 MiB instead of 102 — and the ring reports what it holds). New `cadence` verb sets capture rate and ring cap together. **The spec's own premise did not survive its baseline** — JSON was never the CPU barrier; it was an 11×-overhead memory tax. §8 names the one measured follow-up (a base64 round trip between capture and ring, ~50 µs, not blocking). Still on `spec-807-binary-checkpoint-ring` — it moves to `_archive/` when that lands on main, which is why a BUILT row is sitting here rather than gone. |

| 808 | [Rewind transport](808-rewind-transport.md) | **PARTLY BUILT** | Play the machine backwards. The design turns on one measurement — a full restore is **177 µs** — so backward playback moves the MACHINE rather than replaying cached pictures, and every existing viewer follows for free. Built: the daemon owns the transport state, the monitor verbs, `transport/play|pause|goto|frame|toggle|status`, F9–F12 in the TUI and the window off ONE shared key table, the frame lens (a capture on an anchor can show the frame the redraw threw away), and `transport/key` — a client hands over the KEY and the daemon answers what it did or that it dropped it, which is what the browser needed since it carried no copy of the table and F9–F12 were simply dead there. **This row said PROPOSED while all of that was shipped**; the spec said it too, so the board gate stayed green on two stale claims. Open: the C64RE ribbon in the scrub UI — the browser has the keys and no visible transport controls. |
| 809 | [Marks and sandboxes](809-marks-and-sandboxes.md) | **PARTLY BUILT** | Marks shipped: named + pinned anchors that survive PLAY cutting the future (the centre of gravity — three attempts from one mark give the identical machine and the mark outlives them), a cap of 32 that REFUSES rather than shrinking the window silently, labels riding the ringdump so a `.c64rering` is a session with its bookmarks, and a name working as an anchor id everywhere. Sandboxes shipped as a bare capability: `sandbox/run` / `runMany` return a state with no name, no verdict and no comparison — gated on carrying none of those, because 810 owns the meaning. Open: copy-on-write media folders per run, and multi-line assembly. |
| 812 | [Capture scenario + release reel](../../C64ReverseEngineeringMCP/specs/812-capture-scenario-and-reel.md) | **BUILT** | The spec is C64RE's and so is the work: it writes the scenario, walks it, and builds the reel. TRX64's share is two RPCs, and both are facts about a machine — `session/frame_indices` (the displayed frame as raw 4-bit colour indices plus its palette, so nobody downstream decodes a PNG to reach pixels) and `session/advance_to_frame` (run to the next raster wrap and report the cycle, because `displayed` is the frozen PREVIOUS frame and a mid-frame grab returns the picture before the interesting one). Gates: `advancing_to_a_frame_lands_on_the_raster_wrap`, `a_held_joystick_is_seen_for_every_frame_it_is_held`. An earlier build of this put a scenario EXECUTOR in this repo — a `trx64cli reel` subcommand with a GIF encoder behind it. It was removed: the machine emulates, a human or C64RE drives it. |
| 815 | [Turbo/speed registers](815-turbo-speed-registers.md) | **PARTLY BUILT** | A C64 release does not ask "am I a C64", it PROBES — `$D031`, then the `$D02F`/`$D030` pair — and takes a different code path when the answer is yes. On a plain C64 every one of those reads is `$FF`, so that path is not hard to find, it is unreachable. Built: the registers and their read-back masks ported from `vicii-mem.c`, and the machine profile as BOTH a parameter (`session/turbo`, `trx64cli boot --turbo`, per sandbox batch item) and a monitor verb (`turbo`, `turbo mode c64|128|u64`, `turbo on|off`, `turbo speed N`), default OFF so a plain session is bit-identical to before. Open: §3 — what a SET speed bit does to the picture. Deliberately unbuilt: the observed fault is bars where text should be with colour RAM intact, which says the main-bus DATA is wrong and the addressing is not, but whether the bars are stable frame-to-frame or move decides between a per-line latch and a per-cycle bus value. Waiting on that hardware answer rather than inventing behaviour no other emulator has. |
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
