# Use cases — the snapshot-anchored whitebox dev sandbox (the YARDSTICK)

**Status:** NORTH-STAR / yardstick (2026-07-16, owner-defined). The specs
(snapshots/scenarios/overlays/sandbox — see `concepts-snapshots-scenarios-overlays.md`)
are measured against THIS. If a spec does not serve a use case here, it is off-target;
if a use case here is not reachable from the specs, that is the gap to close.

## The goal (owner, reframed 2026-07-16)

**A snapshot-anchored, WHITEBOX development sandbox for your OWN code on a fixed
baseline.** The trigger is: *the moment your own code is added* to an existing game —
to find cheats, make enhancements, fix bugs, or test before release. **Cracking is a
minor side case, not the point.**

Two things make it new:

1. **Whitebox, not blackbox.** TRX64 is OUR machine, so you reach *every component* —
   inspect and diff registers, memory regions, chip state, drive state — **not** just
   compare outcomes (screenshot / RAM hash) against a VICE oracle. Component-level truth,
   not black-box outcome comparison.
2. **Overlay + inline-compile REPLACES the build pipeline.** You anchor on a *fixed
   version + a defined snapshot*, inject/iterate code there (assembled inline, no
   rebuild → reboot → replay), and then **distill what must go into the final code.** The
   snapshot is the baseline; the overlay is your work-in-progress; the output is the code
   delta for the real build.

### The building blocks (bottom-up) + the pipeline

- **Ring (always-on capture) = the FOUNDATION**, under everything else. Because it runs
  continuously you can **time-travel back and DUMP a `.c64re` at any past moment WITHOUT a
  replay** — mint the exact baseline where the bug/life-loss happens. Also the basis for
  traces + reverse-debugging.
- **Snapshot (`.c64re`)** = the fixed baseline version, minted from the ring (or live) by
  `dump`. Portable point-in-time currency.
- **Sandbox** = a scoped/scratch TRX64 that `undump`s a `.c64re`; isolated execution of
  your code on that baseline. **N scratch instances = N parallel scenarios** (Spec 787).
- **Overlay + inline-compile** = your code/data injected, no build pipeline.
- **Scenario = state + overlay** = deterministic iteration of that code on the baseline.
- **Whitebox validate** = reach the components (not just the outcome hash) → **derive the
  final code** (the delta for the real build).

**The pipeline:** `ring → (traces / reverse-debug / point-in-time snapshots) → dump →
sandboxed TRX64 undump → scenario (= state + overlay) → validate (whitebox) → derive final
code`, with **N scenarios in parallel**.

**The human-tester handoff (important — testers do NOT use an LLM):** a tester rewinds via
the ring to the moment a bug appears, dumps a `.c64re`, and hands that file over; the
dev/LLM then runs the sandbox → scenario → overlay → validate loop on it. The ring is the
man↔machine interchange, not just an LLM convenience.

## The use cases (all = "your own code on a baseline")

**UC0 — Substrate: snapshot sandbox with code overlay.** Pin a baseline snapshot → inject
your code via overlay + inline-compile → run a scenario deterministically → inspect the
components whitebox → distill "this goes into the final code." *Every case below is a
specialization of this.*

**UC1 — Find a cheat.** Baseline = game at a state with N lives. Whitebox: find the
decrementer in RAM. Overlay: patch (NOP / freeze the counter). Scenario: lives hold. →
Output = the cheat code.

**UC2 — Enhancement.** Baseline = game. Your own feature (code) via overlay + compile.
Scenario iterates it. Whitebox: does it integrate cleanly? → Output = the enhancement code
for the final build.

**UC3 — Bugfix.** Baseline = a snapshot that reproduces the bug (a scenario triggers it).
Overlay = the fix. Scenario: the bug state no longer occurs (checked whitebox, not just on
screen). → Output = the fix.

**UC4 — Check a refactoring.** Baseline = the original code at a point. Overlay = the
refactored code. Same scenario. **Whitebox diff: components identical** (not merely
outcome-equal) → behaviour-preserving, proven. → Output = confidence + the refactored code.

**UC5 — Pre-release testing.** Baseline = a release-candidate snapshot. A scenario library
(regression). Whitebox: assert component invariants. → Output = pass/fail before shipping.

## The outlook — the AUTONOMOUS loop (the endgame)

The owner should be able to say:

> *"Look at this snapshot — I'm about to lose a life. Find me the best cheat."*

…and the agent, **on its own (alleine)**, from that snapshot point:

1. **Trace** forward from the anchor → understand the mechanism (what decrements the
   life, when, from which code).
2. **Fan out N candidate approaches** (overlays) — e.g. NOP the decrement, freeze the
   counter, short-circuit the collision check.
3. **Test each in parallel** — one scoped **scratch** sandbox instance per candidate
   (Spec 787: **1 live + N scratch**), each restoring the same anchor, applying its
   overlay, running the scenario.
4. **Evaluate whitebox** — component diff per candidate: does the life hold? side-effects?
   cleanest / least collateral?
5. **Rank and propose the best** + the exact code delta.

This is a fan-out → test → evaluate → rank agent loop over the sandbox. It generalizes
past cheats: the same loop proposes the best bugfix, validates a refactor across N
scenarios, or A/B's two enhancement designs — all from a fixed snapshot, autonomously.

## What this says about the specs (re-weighted priorities)

Measured against the use cases + the autonomous loop, the priority is NOT the
blackbox/oracle/crack material. It is:

1. **Sandbox as the core — Spec 787 (scoped instances: 1 live + N scratch) + 788
   (real-core sandbox).** This is THE substrate for "test 3 approaches"; it was
   under-weighted in the concept map. Build/finish this first.
2. **Whitebox component diff.** The differentiator. Not `ramHash` (blackbox, Spec 231) and
   not the VICE oracle — a component-granular inspect + diff (build on the 754 monitor /
   inspect + `diffCheckpoints` / `snapshot_diff`, but at component granularity with a
   human/agent-legible verdict, not a byte-hash).
3. **Overlay + inline-compile as the working model.** The mechanism exists (769
   `runtime_overlay_run` + `assemble_source`); what's missing is the *model* "fixed
   version + snapshot instead of a build" and the recorded branch that carries the
   candidate code (Spec 776).
4. **The "final code delta" output — the meaning bridge.** The payoff is NOT a generic
   `FindingRecord`; it is **the patch / code-delta that goes into the real build.** This
   is the least-specified piece relative to the runtime machinery, and it is where every
   use case actually lands.
5. **The autonomous orchestration** (trace → fan-out → test-N → rank → propose) — the loop
   that ties it together, running over 787's scratch instances.

## Discipline that still holds

Static/disasm-first remains the discovery doctrine: the agent READS to form the
hypothesis (where the life is decremented) before it traces/overlays to confirm and
iterate. The sandbox is the confirm + iterate engine for *your own code*, not a substitute
for understanding the target. Meaning (the final code delta, provenance) lands in C64RE,
always the last step.
