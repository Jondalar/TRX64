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

## A faster CPU pays per PHI2 cycle, not per instruction — 856

The owner, on the UE2 emulator: the core holds at 1 MHz and collapses as the MHz setting goes
up. 851's turbo is CPU-only, so 64 MHz is 64× the 6502 work, and that part is simply the price
of a software CPU. What was charged on top: the run loop synchronised at every INSTRUCTION
boundary — CIA catch-up, interrupt restamp, drive and SID sync, the bus rebuilt — and at
64 MHz almost all of those boundaries find `clk` where it was. UE2's profile of a real 64 MHz
demo put that work at about 40 % of the emulation thread.

**Decision:** batch instructions inside one bus while the clock has not moved AND nothing but
RAM was touched. "The clock moved" alone is not enough: within one PHI2 cycle the boundary
restamp is the only path an IRQ acknowledge has to `IntStatus`, and without it every handler
at 64 MHz storms. The access flag is deliberately conservative — IO, cartridge windows, the
processor port, snooped addresses — so cartridge-ROM code gets no fast path; a false positive
costs one sync, a missed one breaks interrupts.

**Decision:** at a divider of 1 the batch cannot run twice, so the stock machine's path is the
one it had. The gate is exact equality, fast path off against on, over the instruction stream,
every bus record and every interrupt — a batched instruction that kept the previous opcode's
pre-fetch state was invisible in the machine state and wrong in every trace, and only the bus
records caught it. Both failure kinds were provoked before the gate was trusted.

Measured on a RAM loop at 64 MHz: 0.86× → 1.40× real time with the reverse rings off. On the
real thing — UltimateDemo2026 at 64 MHz in UE2 — 0.72–0.98× became real time in every window,
and the audio underruns stopped. The rings stay per instruction; that cost is theirs and has
its own switch. What dominates now is the CIA timer update, which this change barely touched:
its cost is the real per-PHI2 advance, not the repeats.

## More than one SID — 855

A C64 program could only ever talk to one SID at `$D400`, while the U64 decodes up to eight
across `$D400-$D7FF` and `$DE00-$DFFF` from its firmware's socket and UltiSID registers.

**Decision:** TRX64 carries N SID instances and a per-block mapping (a 32-byte block to a chip,
optionally ahead of the expansion port); the host builds the map. TRX64's half — handles, the
map, reads through chip 0's model, the host door for the others, the `.c64re` snapshot — was
gated in `sid_multi_gate` (sixteen cases). UE2 built its half against it: the map from the
firmware's decoders, one reSID per receiver, the mixer, readback through the door; checked by
unit tests, a bridge test, firmware-in-the-loop smokes (a second SID alone at 1000.0 Hz) and an
8-SID demo by ear. Closed 2026-09-19 when UE2 confirmed; stereo, filter curves and socket 2 stay
deliberately unbuilt on UE2's side. Spec: [855-more-than-one-sid.md](855-more-than-one-sid.md).

## The CIA alarm is a comparison, not an update — 857

UE2's profile after 856 left one item at the top: the CIA timer update, 12–16 % of the
emulation thread, which 856 had barely moved. VICE's cycle-exact core checks alarms in every
instruction prologue and every cycle by COMPARING against the next pending alarm; TRX64 has no
alarm context and called `Cia::update_to` there instead — which is VICE's register-access
catch-up. At 64 MHz that ran about thirty-six times per PHI2 cycle, at 1 MHz three times.

**Decision:** catch a CIA up only when an alarm is due, and treat a running timer with no
prediction as due — exactly the case the old code repaired lazily. Measured, alternating A/B,
9 of 9 pairs: booted at 1 MHz 10.53× → 13.33× real time, RAM loop at 64 MHz 0.97× → 1.22×. On
UE2's demo the process CPU per window fell from 76/85/69/97 % to 66/74/59/81 % and
`Ciat::update` left the profile's top thirty entirely.

**Decision:** the alarms must first carry what the catch-up carried, in its own commit and
provably neutral. Timer B's alarm was predicted on every write and never dispatched; a cascade
step did not re-predict it; `peek` and the snapshot capture read counters that were current only
because of the catch-up. Those readers now work on a copy caught up to `checked_clk`, the clock
the machine last checked the alarms at — **not** `Cia::clk`, which is the TOD tick counter and
runs one ahead of the CPU.

**The gate that made this safe is two things, not one.** Lockstep equality between check off and
check on cannot show that the preparation left the old path alone, because it moves both sides
at once. So the gate also freezes digests of the pre-857 behaviour. They earned it twice: they
caught the `Cia::clk` mistake immediately, and the restore case diverged at 64 MHz on untouched
code — checkpoints did not carry the turbo phase, a defect since 851, fixed in its own commit.

**Measured after the release, alternating against a `v0.7.2` worktree:** +18 % warp throughput on
a stock 1 MHz machine (4 of 4 pairs), and with the check switched off **6 % below 0.7.2** (3 of
3). The preparation is not free, so the kill switch is a fallback for behaviour, not a way back
to the old speed — UE2 had seen the same in their measurement and could not separate it from
noise.

## Turbo/speed registers — 815

A C64 release PROBES for a turbo machine — `$D031`, then the `$D02F`/`$D030` pair — and on a
plain C64 every one of those reads `$FF`, so its whole turbo half is unreachable.

**Decision:** the registers with VICE's read-back masks, and a machine profile (`c64` default,
`128`, `u64`) as both a parameter and a monitor verb; the claim belongs to the session and
survives reset and power-cycle, because a cartridge probes inside the boot warm-up. What a set
speed bit does to the PICTURE split by profile: on `u64` nothing — the VIC keeps its bus slots
(Gideon Zweijtzer, 1541ultimate#665, via UE2 2026-09-19), which is what TRX64 does; on `128`
the real machine shows bars with colour RAM intact, and whether they are stable or move decides
the model. There is no C128 to measure on, so that half is closed **deliberately unbuilt** and
the gate asserting an IDENTICAL picture marks the hole. Closed 2026-09-19. Spec:
[815-turbo-speed-registers.md](815-turbo-speed-registers.md).

## The local quality gate — 783

The gates existed (the seven-game gate, `iso_vic_gate`, `vic_collision_gate`, `cart_mapper_gate`,
the conformance oracle, the screenshot oracles) and nothing ran them.

**Decision:** no CI; the gate runs locally. `core.hooksPath=hooks` points git at `hooks/pre-push`,
which calls `scripts/gate.sh` and blocks the push on red; 783.2 decides which pushes need the
full gate. The hook is the only thing that runs the tests. Built 2026-08-14. Spec:
[783-local-quality-gate-enforcement.md](783-local-quality-gate-enforcement.md).

## The binary checkpoint ring — 807

The per-frame capture encoded two VIC framebuffers it then threw away, and a ring entry held a
live `serde_json::Value`.

**Decision:** move the file format off the per-frame path — JSON + base64 is how a checkpoint is
persisted and transmitted, not how it is held. Capture 167 → 64 µs, ring entry 208.9 → 97.7 KiB,
and a `cadence` verb sets capture rate and ring cap together. The spec's premise did not survive
its baseline: JSON was never the CPU barrier, it was an 11× memory tax. The `.c64re` format and
every WS response stayed byte-identical. One measured follow-up is left in §8 (a base64 round
trip between capture and ring, ~50 µs, not blocking). Merged to main. Spec:
[807-binary-checkpoint-ring.md](807-binary-checkpoint-ring.md).

## The split and the capability cut — the charter, 774

The 2026-06-29 plan had three parts: TRX64 as the runtime, a `trx64-mcp` server beside
C64RE's, and the static decode/parse/classify capability migrating into `trx64-static`.

**Decision:** only the first survived. TRX64 is the only runtime (Spec 806) — for `trx64cli`,
C64RE and the UE2 emulator — and C64RE's `runtime_*` tools are its permanent door. There is
no `trx64-mcp` (owner, 2026-09-19). Static analysis stays C64RE's, in TS: media parsing was
dropped 2026-08-11 (the drive must refuse what the workbench must read), the classifiers
2026-09-19; `trx64-static` keeps only the decoder the runtime's monitor and `trx64cli disasm`
use. Closed 2026-09-19. Docs: [spec-c64re-trx64-split-charter.md](spec-c64re-trx64-split-charter.md),
[capability-cut-decisions.md](capability-cut-decisions.md).
