# Spec 809 — Marks and sandboxes: a fixed point, and N machines from it

**Status:** DONE 2026-09-26. Marks (§3), sandboxes (§4, now isolated — §9), copy-on-write
media per run (§4), `nearestMark` (§5b) and the block assembler (§5) are built and gated.
The sandbox half was first reported shipped while it ran on the live machine; §9 records
how that was found and fixed.
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

809 is the first and third bullets, and **only the capability in them**. TRX64 has nothing
to do with the tests except that it owns the runtime: it offers a named point and N isolated
machines from it, and never learns what a branch is, why it exists or whether it won. The
middle bullet — and the naming, the bookkeeping and the verdict — is 810.

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
3. **Fan-out.** `overlay_run` runs ONE patch-set once, on the live machine. "Give me four
   isolated machines from Alpha, these bytes in each, and the four end states" is
   composition nobody owns yet.
4. **Source in, bytes out.** Only one line at a time exists. A patch is usually a few
   instructions, and typing them one at a time through `a` is not a loop anybody will run
   twice.

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

**The monitor surface is exactly four verbs** — set, list, drop, jump. Nothing about
sandboxes or scenarios is typed by a human here; that is C64RE's, over RPC.

```
mark <name>              name + pin the anchor the transport is standing on
marks                    list: name · cycle · frame · how far back · pinned-cost
unmark <name>            drop the name and the pin
goto <name>              jump there (the verb already takes a frame or a cycle)
```

A mark earns its place in the runtime the same way a breakpoint does: it is a navigation
aid for whoever is driving, useful while debugging with no test in sight.

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

## §4 Sandboxes — a capability, not a concept

**TRX64 has nothing to do with the tests, except that it owns the runtime and therefore the
capability.** The first draft of this section got that wrong: it had branches as named,
declared objects with a lifecycle and a registry, and a `run-all` over "every branch on this
mark". That is scenario vocabulary. It would have made TRX64 know what a branch *is*, why it
exists and whether it won — none of which is its business.

What it offers instead is one sentence with no nouns from that world:

> restore this state into N isolated machines, put these bytes in, run them this long, hand
> back the end states

```
sandbox/run { from, patches[], cycles }        -> one end state
sandbox/runMany { from, runs: [{patches[], cycles}] }  -> N end states, in parallel
```

- `from` is a mark name or an anchor id.
- `patches` are address+bytes (RAM, or a cart bank — 795).
- The reply is a state reference plus what it cost. **No name, no verdict, no comparison.**

One call, no lifecycle, no registry. C64RE names the runs, remembers which belongs to which
scenario, decides what to compare and who won. It calls this N times, or once with N.

**Fan-out is 787's scratch instances.** The concept doc: *"N scenarios in PARALLEL = N
scratch instances (Spec 787: 1 live + N scratch)"*. The live machine is never used for a
sandbox run — doctrine rule 2, and the only way the fan-out can be parallel at all.

**A sandbox that writes media writes into its OWN folder.** Copy-on-write, created lazily —
the folder appears the first time a run actually dirties something, and a run that only
reads never makes one.

```
<project>/sandbox/<run-id>/
    game.d64        only if this run wrote to it
    cart.crt        only if this run wrote flash
```

The originals are never touched. Four runs saving a game in parallel write four files, not
one file four times — which is what would happen today, because TRX64 mounts everything
writable and persists dirty tracks straight into the ORIGINAL image (a known defect, and
this is the shape that contains it).

The alternative — show sandboxes read-only media — was rejected because it fails SILENTLY.
A run exercising the save routine would come back clean because its write went nowhere, and
a clean result that proved nothing is worse than none.

Half of it exists: `savecrt` writes a cart image out and `undump` reads a whole machine
back, so the cartridge side has its mechanism. 809 adds the same for the 1541 medium plus
the folder convention that keeps runs apart. The folder is part of the run's result, so a
caller that cares about a written disk can fetch it.

## §5 Source in, bytes out

The assembler assembles **one line**. A patch is typically a handful of instructions with
at least one label, and typing them through `a` one at a time is not a loop anyone runs
twice.

```
asm <addr> [<<TAG]       assemble a block at addr: lines are collected until `end` (or TAG),
                         then assembled as one; sent as one command with newlines it is
                         assembled at once
asm-file <path> [addr]   the same from a file; the file's `*=` gives the address if none
```

Both WRITE, like `a`, and only when the whole block assembled — a patch half in memory is
worse than none. `asm/block` over RPC writes nothing.

- Multi-line, labels, `.byte`/`.word`, `*=`/`.org`. Two passes: collect labels, then emit.
- Documented NMOS set only, as today. The undocumented table stays out of the assemble
  index — reading it back is `d`'s job, writing it is not v1's.
- Output is bytes + a load address, which is exactly what `sandbox/run` takes as a patch.

**Explicitly not a build system.** No includes, no macros, no linker. If a patch needs
that, it is a `.prg` and `bload` already exists. This is the small door: "these six
instructions, at this address".

## §5b The API, and what the TUI does with it

808 taught this the hard way: the daemon owns the state and the clients render it, so the
SHAPE OF THE REPLY IS THE DESIGN. Writing the verbs and leaving "there is an RPC twin" as a
gate is how a client ends up composing messages again.

### RPC

```
mark/set      { name }              -> { mark: Mark, used, cap, message }
mark/list     {}                    -> { marks: [Mark], cap, used, windowSeconds, windowCostSeconds }
mark/drop     { name }              -> { dropped, cycles, message }
mark/goto     { name }              -> transport status (808)

sandbox/run     { from, patches[], cycles }              -> Run
sandbox/runMany { from, runs: [{patches[], cycles}] }    -> { runs: [Run] }

asm/block     { origin, source }    -> { bytes, origin, labels, errors[{line,message}],
                                         patch: {addr, bytes} | null, message }
                                       (writes NOTHING — `patch` goes straight into sandbox/run)
```

```
Mark  { name, anchorId, cycle, frame, secondsBack, message }
Run   { id, from, anchorId, instance, state: done|failed, cycles,
        applied, reads, registers, ramDigest, endStateId, media?, message }
```

`Run` carries no name and no verdict — an id, where it started, what it cost and where its
end state is. Naming it, remembering it and judging it are 810's.

*As built (§9):* the first shape had `endAnchorId`, and the first implementation filled it
with the START anchor. `anchorId` is the start; `endStateId` (`sb-r-NNNN`) is the run's
real end state, captured from its clone and kept — the last 32 — so `runtime/component_diff`
compares two of them. `media` replaces `folder?`: present only when the run wrote, with the
folder and each file in it. Run ids are numbered daemon-wide.

Every reply carries `message`, a ready-to-print line. Not decoration: it is the rule that
came out of 808, where the buffer range appeared on `/pause` and not on F11 because two
client call sites assembled the same text from different fields.

`transport/status` gains `nearestMark: { name, framesAway }` so the transport line can show
it without a second call.

### TUI — no new panel

The cockpit's panels are full and the log is where sequences belong. Marks are a
sequence, so:

```
> mark alpha
MARK alpha @ Cycle 10757570 Frame 500  (-0.4s)   ·   3 marks · window 59.9s of 60.0

> marks
  alpha        Cycle 10757570  Frame  500   -0.4s
  before-boss  Cycle  8120004  Frame  368   -1.3s
  intro-end    Cycle  1204880  Frame   61   -6.5s
  3 of 32 marks · window 59.9s of 60.0

```

Sandbox runs are not typed here at all — they arrive over RPC from C64RE. If a human wants
to see what is running, that is a status read, not a control surface:

```
> sandboxes
  r-0041  from alpha  running  inst 2
  r-0042  from alpha  done     inst 3   240000 cyc
  2 runs · 1 live machine + 2 scratch
```

The transport line gains the nearest mark, because while scrubbing the useful question is
"how far am I from a mark", not the absolute frame:

```
 REPLAY  ◀◀  frame 340/3000   -53.2s   ·   alpha +160
```

**Deliberately not a panel.** A panel costs rows the machine state is using, and it would
have to be kept live — which means polling, which means the client asking questions it does
not need to ask. A mark list is read when you ask for it.

**And the C64RE UI gets the same objects**, which is the point of designing the RPC rather
than gating it: a marks sidebar and a run grid are the natural rendering there, and they
need no endpoint the TUI does not already use.

## §6 What 809 does NOT do

- **No goals, no assertions, no acceptance.** A sandbox run returns a state and what it
  cost. What "correct" means is 810, in C64RE. TRX64 must not learn the word "expected".
- **No branches.** A branch is a named thing that belongs to a scenario and can win. That
  is 810's object. TRX64 takes bytes and a budget and hands back a state; it does not know
  the run had a purpose.
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
- **G7 — fan-out isolates.** `runMany` with N runs touches the live machine not at all:
  its cycle count and state are identical before and after.
- **G7b — and it isolates the FILES.** Run N sandboxes that each write the disk; afterwards
  the original image is byte-identical to before, and each run's folder holds its own
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

## §9 Reopened 2026-09-26 — the sandbox is the live machine

Found while writing C64RE's Spec 884 (ring marks in C64RE), by reading the code rather than
this spec's status line.

**What the code does.** `sandbox/run` / `sandbox/runMany` say in their own comment that
*"the live machine is NEVER used for a sandbox run (doctrine rule 2)"*. They then build a
`runtime/overlay_run` request and dispatch it against the same daemon state, and
`overlay_run` restores with `restore_live_checkpoint(&mut st.session, …)` and ends with
`st.session.running = false`. A sandbox run therefore restores, patches, runs and **pauses
the machine the human is watching**. A `runMany` of four does it four times in a row. That
is serial, not parallel, and nothing about it is isolated.

**Why nobody saw it.** The sandbox tests assert that a run starts from a mark name, carries
no verdict, and leaves the mark in place. None of them asserts what G7 says: that the live
machine's cycle count and state are identical before and after. The comment was believed
instead — the same failure G7b's own wording warns about (*"the flags said read-write and
everyone believed the comment instead"*).

**What was done** (branch `spec-809`):

1. **Sandbox runs are clones.** The shared state is touched twice — to take the anchor's
   snapshot and a clone of the machine, and to file the results — and never while a run
   executes. Each run restores into its own clone on its own thread, as many at once as
   the host has cores. G7 is a test: live clock, PC, RAM digest and run state identical
   after a three-run fan-out. It was run against the OLD handler first and went red.
2. **`nearestMark`** rides `transport/status` and the transport line (`alpha +160`).
3. **G5 was narrower than claimed** — `component_diff` and `pin` refused a mark name
   while `overlay_run` took it. The ring resolves names at the one lookup every door
   passes through (id first). `unpin` stays id-only: unpinning a mark by name would
   release the pin G1 depends on.
4. **Copy-on-write media** (§4) and **G7b** with real writes: a GCR sector onto the
   clone's track, and an EasyFlash programming sequence. The copies diverge, the
   originals and the live media stay byte-identical. Written is decided on the bytes,
   not a dirty flag.
5. **The block assembler** (§5), and **G6**: every documented opcode, disassembled by
   `d`, assembled back as one block, gives the same bytes.

One smaller correction on the way: the monitor classified a line by its verb only, so
the lines inside `a` mode and an `asm` block counted as observing. The verb that enters
either mode already cuts the future, so nothing was wrong in practice; `classify_in` now
classifies each line for what it does.

**Found on the way, not fixed here** (outside 809, and outside the quality gate, which runs
the core and daemon suites but not `trx64-ffi`): `audio_persistent_engine_continuity` in
`crates/trx64-ffi/tests/smoke.rs` asserts that the process-wide reSID construct counter did
not move while it drained, and the other tests in that binary construct reSID in parallel.
Measured: 7 of 10 runs green in parallel, 10 of 10 with `--test-threads=1`. The assertion is
right and the counter is shared; the test needs its own counter or a serial runner.
