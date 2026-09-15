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

## A paused machine still has a picture — 837

Issue #13: a cockpit stuck on "No frame yet" after an `undump`, curable only by killing the daemon.
The stream renders only while the machine runs, so a client that ARRIVED during a pause started a loop
that drew nothing, for ever. Every operation that leaves a machine paused already asked for a present;
nothing asked on arrival.

**Decision:** a new subscriber sets the existing `force_present_frame` one-shot — one frame on arrival,
not a stream, and no second mechanism beside the one the paused branch already had. Noticing a stream
that dies while the machine runs is C64RE's half.

## The empty expansion port — 840

Found while scoping issue #19 (REU): with no cartridge, `$DE00-$DFFF` read the write-through I/O
shadow, so an empty port behaved like RAM and every write-read-compare probe found a device that was
not there.

**Decision:** an unclaimed read there returns the VIC's last phi1 fetch (`viciisc/vicii-phi1.c:34`), at
all four sites — the CPU read, the peek, both monitor lenses — because a debugger that shows the last
write hides exactly the evidence it exists to show. Spec 850 builds on it: a device that declines a read
leaves the open bus, never the shadow.

## The expansion port as a device interface — 850

UE2 runs the Ultimate firmware on TRX64 and needed the Ultimate Command Interface on the port; the only
way in was a fake cartridge, which the machine then believed in everywhere. None of the six requests was
specific to UCI, so the port got an interface: a place for the machine profile's own device and one for
a host's, side-effect-free peeks, `RunStop::Device`, a write snoop that sees `$FF00` whatever the banking
(VICE's REU hook, which TRX64's core had claimed and never had), one `INT_SRC_EXPANSION` sampled per
cycle, and CPU hold as a run state with a reset flavour.

**Decision:** nothing on the port is a cartridge, and a stock machine pays nothing for the port existing.
The second half was measured, not assumed: the first build cost 6 % and was brought back to noise by
putting every port step behind one flag computed per run. A device is told how long a read was stalled
and how much of that stall had its address on the bus (43 and 3 on a badline); what that means is the
device's business. A DMA engine is not part of this — the REU spec ports VICE's `reu.c`.

---

Everything here is finished. If a row's subject turns out to be open after all, it needs
a NEW number from the registry — not a reopening, because a spec that closes twice
teaches everyone that "closed" means nothing.
