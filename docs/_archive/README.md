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

## The U64 machine profile and a faster CPU — 851

The owner wanted one start parameter — "Default C64, optional U64/UE2/128" — and turbo while at it.
`--machine c64|u64|128`: `u64` is one machine for U64, Elite II and C64 Ultimate, which differ only in
the speed table. The turbo registers follow the firmware's own layout, which exposed 815 reading
`$D031 = $80` (1 MHz, badline timing) as turbo.

**Decision:** `clk` stays the PHI2 clock everything is keyed on. A faster CPU is a divider in `clk_inc`,
ported from VICE's TurboMaster, so timers, raster, rings and traces never learn the CPU got faster, and a
stock machine pays nothing. What the closed core does not reveal — `$D031` read-back, I/O stretching,
the exact `$D0BC` value — is modelled minimally and gated, not guessed richly.

## The Ultimate Command Interface as U64 hardware — 852

The owner decided where UCI lives: in TRX64, as part of the U64, with no fake cartridge and no separate
model inside UE2 — the block is one open VHDL entity on every Ultimate firmware target.
`command_protocol.vhd` was ported one to one onto 850's port: registers whose reads have side effects,
the IRQ, freeze as the hold line, the `$FF00` trigger and the `$D038`/`$D036` unlock through the snoop,
and the firmware side as an API a host maps onto `CMD_IF_BASE`.

**Decision:** the `u64` profile owns the block, disabled at power-on as the firmware itself defaults, so
standalone TRX64 reads the open bus and a program probing for UCI takes its no-UCI path instead of
waiting on a server that does not exist. The block survives a C64 reset — only the FPGA reset clears it
— and is in no snapshot: its other half is the firmware, so every restore puts it back to power-on. What
only the hardware can answer stays a stated assumption: what the unlock tolerates in between, and that a
read the VIC stretched counts once per cycle its address was on the bus (4 on a badline, not 44).

**The second of those is DISPROVED, 2026-09-17, fixed in 0.7.2.** A read the VIC stretched counts ONCE,
not once per cycle on the bus. The assumption lost the C64 a byte on every stalled read: UBoot64 asked
for its own 24480-byte file over UCI DOS, got 24279, and hung waiting for a remainder the firmware had
already sent — stopping at a byte that moved with the badline every run. A BA-stretched read is one 6502
bus cycle, so one completed read consumes one byte. The gate case that should have caught it asserted
the assumption itself and therefore confirmed it; it is now two cases, one of them a difference test.
The unlock question stands. Detail under Spec 852's DISPROVED note.

## A device that drives the bus — 853

Issue #19 asked for the REU and was closed by 840: the cruncher jammed because an empty
port echoed writes, not because an REU was missing. What remained was the feature, and it
is a want rather than a defect — which is why it could wait for the decisions instead of
being rushed.

**Decision:** VICE's `reu.c` is the reference, ported exactly, including the parts that
read like mistakes — the 1700's own wrap, the "hacked REU" above 512 KB where the REC chip
still wraps at 512 KB, the Half-Autoload-Bug, and verify's three documented weirdnesses.
Where VICE and the Ultimate's closed VHDL differ, VICE wins.

**Decision (owner, 2026-09-16):** the REU is a CORE device, on `c64` and not only `u64`;
its RAM is OUT of the checkpoint ring and IN the `.c64re` dump; nothing is written back to
the filesystem; GeoRAM is in scope, because it is the same shelf at the opposite cost — no
DMA at all, 723 lines against `reu.c`'s 1688.

**Decision:** ring and snapshot are two different things, and the split is an option on
the one capture rather than a second serializer — Spec 807 had drawn that line already.
16 MB in a 32 MiB ring would not shrink it, it would destroy it. And where a restore
cannot cover the expansion RAM it SAYS so: 792's lesson was that the silence is the
defect, not the gap. That is also why an absent node leaves the device alone instead of
ejecting it the way a missing cartridge node does.

**Decision:** 850's two device places became a list. VICE answered this first with its IO
Slot, where any number coexist because they claim no `game`/`exrom` and map only into
IO1/IO2 — and the core did not have to be opened for it, because a device that holds
several satisfies the same trait.

## The expansion RAM the host owns — 854

UE2's integration of 853 was green, and it came back with the one thing 853 §5 had left to
the bridge: on the U64 the REU's RAM is the firmware's DDR, and the firmware preloads an
image by writing there with its own CPU. A device that allocates its own sixteen megabytes
gives two copies, and every preload lands in the one the C64 never reads.

**Decision:** 853 stays closed and this is a SECOND storage mode, not a correction. Owning
the RAM is right for a standalone machine and is what makes a `.c64re` dump round-trip;
854 adds the borrowed store, the same shape the bridge already uses for the cartridge.

**Decision:** the store is a trait the device HOLDS, not a borrow threaded through a call.
GeoRAM decided it — it reads its RAM on every `$DE00-$DEFF` access, not only during a
transfer, so a store passed into `run_dma` would serve the REU and not it. And a `'static`
device cannot hold a `&mut [u8]` without putting a lifetime on `Machine` and every call
site in the crate.

**Decision:** "nothing lent" is not an error state. It reads the floating-bus latch and
drops writes — the behaviour 853 already had for an address with no DRAM behind it — so a
bridge that lends only around bus accesses is in the normal case between them, not a
broken one. And a borrowed store is in no snapshot: those bytes belong to the host's memory
image, which persists them itself.

---

Everything here is finished. If a row's subject turns out to be open after all, it needs
a NEW number from the registry — not a reopening, because a spec that closes twice
teaches everyone that "closed" means nothing.
