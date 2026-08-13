# Closed specs — what was decided, and why

`docs/` holds OPEN work only. A spec that shipped moves here, because a folder of
finished plans reads to every visitor as a backlog and the reader has no way to tell
which is which. What survives the move is the DECISION — the thing that would otherwise
be re-derived from an argument nobody remembers having.

Numbers are shared with C64RE; the registry is
[`../../../C64ReverseEngineeringMCP/specs/README.md`](../../../C64ReverseEngineeringMCP/specs/README.md).

---

## The overlay workflow — 776 → 795 → 796 → 797

**776** asked for the active experiment loop: `run → intervene → diff`, as against the
passive one (`run → rewind → diff two existing checkpoints`) that already worked. It was
never built under its own number. Three specs delivered it instead, and the charter sat
at PROPOSED until 2026-08-12 reading like open work:

* **795** lifted `overlay_run` from RAM-only to cartridge banks, with an explicit space
  and bank, ephemeral. The prerequisite: you cannot intervene in banked code you cannot
  reach.
* **796** made the experiment a THING — a live scenario-bound overlay branch with an
  accumulating patch set and 794's evaluation folded in. Daemon-side candidate store,
  seven MCP tools.
* **797** turned a candidate into a build-ready delta, which is the bridge back to
  meaning: an experiment you cannot express as source is a result you cannot keep.

**Decision:** the loop is the three of them. If something is still missing, it belongs
to 796 as a slice, not to a fourth charter.

## Verification without an oracle — 794

The TS runtime and VICE stopped being the authority (2026-07-15), then stopped being
anything at all (2026-08-12, Spec 806 over in C64RE). That removed the answer to "is this
still right?" and 794 replaced it: a checkpoint-level equivalence verdict between two
runs of THIS runtime, with an explicit exclusion mask — floppy RAM included, because a
drive that legitimately differs would otherwise make every verdict red.

**Decision:** regression protection is self-comparison plus the gates, never an external
emulator. The techniques from the oracle era survive as rules, not as a dependency.

## Snapshots and media — 792, 793

**792** was a real defect with a long tail: `read_color_ram` captured `ram[$D800]`
(RAM-under-IO) instead of `io_shadow[$0800]` (the real colour RAM), so every restored
snapshot had wrong per-cell colour and multicolour flags — rooms turned to garbage after
an undump. Fixed in `acec8bc`. **Snapshots taken before that fix have the wrong colour
RAM baked in and cannot be repaired; re-dump them.**

**793** made an undump materialise its embedded media instead of assuming the original
file is still where it was.

**Decision:** a snapshot is only as good as the state it captured. Where a chip's state
is shadowed, read the shadow, not the RAM underneath — that is the class of bug 792 was.

## Cartridges — 790, 803's shipped half

**790** gave a bare `.bin` a typed attach, so a cartridge without a CRT container still
says what mapper it is instead of being guessed at.

**Decision:** the container is not the type. A `.crt` header names one; a `.bin` needs
telling, and inferring it from size is how a MagicDesk becomes an Ocean.

## Cheats — 798

Snapshot-diff → decrementer → the candidate. Subsumes the older 762, which had proposed
the same thing and was never built. Full automatic cheat codegen still waits on a real
target to try it against.

**Decision:** 762 is closed by 798, not deferred. Two numbers for one idea is how both
end up half-done.

## Trace reading — 802

`trx64-traceindex` reads the binary trace natively; the sidecar process is deleted. A
trace that needs a second process to be readable is a trace that is unreadable whenever
that process is missing.

---

Everything here is finished. If a row's subject turns out to be open after all, it needs
a NEW number from the registry — not a reopening, because a spec that closes twice
teaches everyone that "closed" means nothing.
