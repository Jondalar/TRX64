# Spec 809 — Marks and branches: a fixed point you can iterate from

**Status:** PROPOSED
**Repos:** TRX64 only. The goals, the acceptance and the BDD layer are **810** in C64RE —
this spec knows nothing about what "correct" means.
**Number:** 809 (shared board `C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** 808 (the transport that gets you to the point), 787 (1 live + N scratch
instances), 794 (whitebox component-diff), 796 (candidate patch-sets).
**Framing:** the owner's model, in his order —

> - ein spezieller State wird als Baseline gesetzt
> - der Coder (oder LLM, oder Tester) definiert 1..n Pfade, die von diesem Punkt ab
>   geprüft werden sollen
> - TRX64 stellt dann Sandboxes n-mal, um die einzelnen Bäume abzuarbeiten

809 is the first and third bullets. The middle one — *what* is checked and what the goal
is — is 810.

---

## §1 What already exists, and what is actually missing

Most of this is built. Naming that up front, because the spec is small only if the
existing pieces are used rather than re-derived.

| piece | state |
|---|---|
| Rewind to a point, exact machine | **808**, shipped |
| Iterate from an anchor: restore → patch → run → observe | **769.2** `runtime/overlay_run`. Its own comment: *"Repeatable: each call restores fresh (the prior patch is rolled back by the restore), so the LLM iterates a fix from a fixed point without rebuild/reboot"* |
| Overlays on cart banks, not just RAM | **795** |
| An accumulating patch-set bound to a scenario | **796** candidate store |
| A build-ready delta from a candidate | **797** |
| Byte-exact verdict + exclusion mask | **794** component-diff |
| N parallel scratch machines | **787** (1 live + N scratch) |
| Assemble ONE line of 6502 | `assembler.rs`, behind the monitor `a` verb |

**Missing, and this is the whole spec:**

1. **A named mark.** Anchors are generated ids (`cp_1247_88`). You cannot say "run this
   from Alpha", and an id that means nothing is not something a human returns to.
2. **A mark that survives being returned to.** See §2 — this is the requirement that makes
   it a mark rather than a bookmark, and it is not free.
3. **Branches as a first-class thing.** `overlay_run` runs ONE patch-set once. "Try these
   four from Alpha and give me the four end states" is composition nobody owns yet.
4. **Source in, bytes out.** Only one line at a time exists. A branch is a patch, a patch
   is usually a few instructions, and typing them one at a time through `a` is not a loop
   anybody will run twice.

## §2 The requirement that shapes everything: iterating must be reliable

A bookmark only has to be findable. A mark you **iterate from** has to be three things,
and the third is the one that costs:

1. **Stable** — the ring rolls forward at 50 anchors/s; the mark must not roll off. A pin
   does that (`checkpoint_ring::pin`, exists).
2. **Exactly reproducible** — every return lands on the identical state. The restore gives
   that, and 808's picture regeneration is already gated as deterministic (same anchor,
   same picture, twice).
3. **It must survive the return.** This is the one. 808 decision 4 was *reversed* so that
   PLAY cuts the anchors ahead — which means every attempt from a mark truncates. Iterating
   is: go there, try, come back, try differently. If attempt 1 removes the mark, you get to
   iterate exactly once.

   The ring can already do it: `truncate_after(id, keep_pinned)` exempts pins. It has to be
   called that way **everywhere**, and asserted, or "reliable" is a promise instead of a
   property.

**Gate G1 is therefore the spec's centre of gravity:** start N times from the same mark,
each run lands on an identical state, and the mark is still there afterwards.

## §3 Marks

```
mark <name>              name + pin the anchor the transport is standing on
marks                    list: name · cycle · frame · how far back · pinned-cost
unmark <name>            drop the name and the pin
goto <name>              jump there (the verb already takes a frame or a cycle)
```

- `label: Option<String>` on `RuntimeCheckpointRef` and in the ring dump format.
- **`ringdump` carries them.** The dump already round-trips `pinned` per anchor; adding the
  label makes a `.c64rering` *a session with its bookmarks* — dump after the bug, send the
  file, the other person loads it and jumps straight to `Alpha`.
- Anywhere an anchor id is taken (`overlay_run`, `diff`, `goto`, candidates), a mark name is
  accepted in its place. `overlay_run --anchor Alpha`, not `cp_1247_88`.

**A pin costs window.** A pinned anchor is exempt from eviction, so it holds its slot while
the 60-second window rolls past it. Twenty marks is nothing against 3000 anchors; two
hundred silently shrinks the rewind window. `marks` reports the cost, and the cap is 32 —
see §8.

## §4 Branches

```
branch <name> from <mark>      declare a branch: a patch-set applied at a mark
branch patch <name> ...        add to it (RAM, or a cart bank — 795)
branch run <name> [cycles]     one scratch instance, run, return the end state
branch run-all <mark>          every branch on that mark, in parallel (787)
branch diff <a> <b>            794 verdict between two branch outcomes
```

A branch is **a mark plus a patch-set plus a run budget**. 796's candidate store is that
object already; what it lacks is the binding to a *named* point and the fan-out.

**Fan-out uses 787's scratch instances.** The concept doc has it: *"N scenarios in PARALLEL
= N scratch instances (Spec 787: 1 live + N scratch)"*. The live machine is never used for
a branch run — doctrine rule 2, and also the only way `run-all` can be parallel at all.

**A branch that writes media writes into its OWN folder.** Copy-on-write, created lazily —
the folder appears the first time a branch actually dirties something, and a branch that
only reads never makes one.

```
<project>/branches/<mark>/<branch>/<run>/    e.g. branches/alpha/patch-dec/003/
    game.d64        only if this run wrote to it
    cart.crt        only if this run wrote flash
```

The originals are never touched. Four branches saving a game in parallel write four files,
not one file four times — which is what would happen today, because TRX64 mounts everything
writable and persists dirty tracks straight into the ORIGINAL image (that behaviour is a
known defect, and this is the shape that contains it).

The alternative — show branches the media read-only — was rejected for a specific reason:
it fails SILENTLY. A branch testing the save routine would run green because its write went
nowhere, and a green run that proved nothing is worse than no run.

Half of this exists: `savecrt` writes a cart image out and `undump` reads a whole machine
back, so the cartridge side already has its mechanism. What 809 adds is the same for the
1541 medium, plus the folder convention that keeps runs apart.

**And a winner ships its folder.** If branch 3 wins and its correctness involved a written
disk, that disk is part of the answer — 797's build-ready delta is incomplete without it.

**Provenance is not optional.** When branch 3 wins, it must be derivable *which* mark,
*which* patch-set and *which* source produced it. 796 stores the patch-set and 797 turns it
into a build-ready delta; the chain exists and only needs the mark on its front.

## §5 Source in, bytes out

The assembler assembles **one line**. A branch is typically a handful of instructions with
at least one label, and typing them through `a` one at a time is not a loop anyone runs
twice.

```
asm <<EOF ... EOF        assemble a block at a given origin
asm-file <path>          the same from a file
```

- Multi-line, labels, `.byte`/`.word`, `*=`/`.org`. Two passes: collect labels, then emit.
- Documented NMOS set only, as today. The undocumented table stays out of the assemble
  index — reading it back is `d`'s job, writing it is not v1's.
- Output is bytes + a load address, which is exactly what `branch patch` takes.

**Explicitly not a build system.** No includes, no macros, no linker. If a branch needs
that, it is a `.prg` and `bload` already exists. This is the small door: "these six
instructions, at this address".

## §5b The API, and what the TUI does with it

808 taught this the hard way: the daemon owns the state and the clients render it, so the
SHAPE OF THE REPLY IS THE DESIGN. Writing the verbs and leaving "there is an RPC twin" as a
gate is how a client ends up composing messages again.

### RPC

```
mark/set      { name }              -> Mark
mark/list     {}                    -> { marks: [Mark], cap, used, windowCost }
mark/drop     { name }              -> { dropped, marks: [Mark] }

branch/define { name, mark, patches[], cycles } -> Branch
branch/patch  { name, patch }                   -> Branch
branch/run    { name }                          -> Run
branch/runAll { mark }                          -> { runs: [Run] }
branch/diff   { a, b, mask? }                   -> Verdict   (794)

asm/block     { origin, source }     -> { bytes, origin, labels, errors[] }
```

```
Mark  { name, anchorId, cycle, frame, secondsBack, message }
Run   { branch, mark, instance, state: queued|running|done|failed,
        cycles, endAnchorId, message }
```

Every reply carries `message` — a ready-to-print line. That is not decoration; it is the
one rule that came out of 808, where the buffer range appeared on `/pause` and not on F11
because two client call sites assembled the same text from different fields.

`transport/status` gains `nearestMark: { name, framesAway }` so the transport line can show
it without a second call.

### TUI — no new panel

The cockpit's panels are full and the log is where sequences belong. Marks and branches are
sequences, so:

```
> mark alpha
MARK alpha @ Cycle 10757570 Frame 500  (-0.4s)   ·   3 marks · window 59.9s of 60.0

> marks
  alpha        Cycle 10757570  Frame  500   -0.4s
  before-boss  Cycle  8120004  Frame  368   -1.3s
  intro-end    Cycle  1204880  Frame   61   -6.5s
  3 of 32 marks · window 59.9s of 60.0

> branch run-all alpha
  patch-dec     running   inst 2
  patch-nop     running   inst 3
  patch-jsr     done      inst 4   240000 cyc
  patch-lda     failed    inst 5   JAM @ $8514
  4 branches from alpha · 1 done · 1 failed · 2 running
```

The transport line gains the nearest mark, because while scrubbing the useful question is
"how far am I from a mark", not the absolute frame:

```
 REPLAY  ◀◀  frame 340/3000   -53.2s   ·   alpha +160
```

**Deliberately not a panel.** A panel costs rows the machine state is using, and it would
have to be kept live — which means polling, which means the client asking questions it does
not need to ask. A mark list is read when you ask for it.

**And the C64RE UI gets the same objects**, which is the point of the RPC being designed
rather than gated: a marks sidebar and a branch grid are the natural rendering there, and
they need no endpoint the TUI does not already use.

## §6 What 809 does NOT do

- **No goals, no assertions, no acceptance.** A branch run returns a state and a diff. What
  "correct" means is 810, in C64RE. TRX64 must not learn the word "expected".
- **No exclusion mask policy.** 794 has the mechanism. WHICH fields are legitimately allowed
  to move (cycle counters, raster position, TOD) belongs to the criterion, i.e. 810 — or
  every test would carry its own mask and no two would be comparable.
- **No UI.** Verbs and RPC, both front-ends, per 808 §2.

## §7 Gates

- **G1 — iterating is reliable.** From one mark, run N times: every run lands on a
  byte-identical state (794 verdict, empty mask), and the mark still exists afterwards.
  This is the spec.
- **G2 — a mark survives a cut.** Rewind to a mark, PLAY (which truncates the future),
  return to the mark. It is still there.
- **G3 — marks round-trip.** `ringdump` → `ringload` → the names and pins are back, and
  `goto <name>` works on the loaded ring.
- **G4 — parity.** Every verb has an RPC twin returning the same object (808 §2 / G2).
- **G5 — a name is an id.** Every door that takes an anchor id takes a mark name.
- **G6 — the assembler round-trips.** Assemble a block, disassemble it with `d`, get the
  source back for the documented set. Labels resolve forwards and backwards.
- **G7 — fan-out isolates.** `run-all` with N branches touches the live machine not at all:
  its cycle count and state are identical before and after.
- **G7b — and it isolates the FILES.** Run N branches that each write the disk; afterwards
  the original image is byte-identical to before, and each branch's folder holds its own
  divergent copy. Asserted with a real write, not by reading the mount flags — the flags
  said read-write and everyone believed the comment instead.
- **G8 — board + the client-owns-no-state gate** stay green.

## §8 Decided in refinement

**Marks are capped, and the cap refuses.** 32 of them; the 33rd is rejected with the
count and the instruction to release one. Not for thrift — 32 pins against 3000 anchors
costs about a fifth of a second of window. The reason is the failure mode: a pin is exempt
from eviction, so unlimited marks let the rewind window shrink **silently**, and you find
out when a rewind comes up short and looks broken. That is the exact class of defect that
cost a full day in 808 — a bound that was real, invisible and blamed on something else.
A refusal you read beats a degradation you discover.

`marks` still prints the arithmetic (`18 marks · window 59.8s of 60.0`), so the cost is
visible long before the cap is reached.
