# Spec 798 — Cheat-Candidate Finder

**Status:** PROPOSED (2026-07-16). **Repo:** TRX64 (finder) + C64RE (tool). **Board:** row 798.
**Next: 799.** (Reuses the number 762 reserved for the cheat-finder — implemented here as 798.)

**Bases:** 794 (checkpoint diff / RAM decode) · 796 (candidate model = the verify half) ·
765/705 (ring anchors). **Yardstick #5** — the first end-to-end consumer that exercises the
whole dev-sandbox stack on a real use case.

## What it does

The **finder half** of the cheat loop: **snapshot-diff → decrementer**. Diff two checkpoint
anchors' FULL 64K RAM (not the 794 capped sample) and return the addresses that **decreased**
(candidate life / health / ammo counters), ranked smallest-delta first (a life counter is
usually −1). The **verify half** is the existing candidate model (796): freeze a candidate
address + run the scenario + confirm it holds (auto-eval 794).

The autonomous cheat loop is then: bracket the life-loss with two ring anchors → `find_cheat`
→ for each candidate, `candidate_patch` freezes the address (write its original value) →
`candidate_run` checks the value holds across the scenario → rank → `derive_delta` (797) emits
the cheat as build-ready code.

## Surface

- **Core** `trx64-core::checkpoint_diff::find_ram_decrements(a, b, max) -> Vec<Value>` —
  decodes both checkpoints' `ram` `$ta`, collects `after < before`, ranks by (delta, addr).
- **Daemon** WS `runtime/find_cheat_candidates(before, after, max?)` — restores both ring
  anchors, runs the finder. READ-ONLY.
- **C64RE MCP** `runtime_find_cheat(session_id, before, after, max?)` — ranked candidates +
  the next-step hint (freeze + candidate_run). In DEFAULT_TOOLS.

## Acceptance

1. Decreases found, increases ignored, ranked smallest-delta first (unit test).
2. Two ring anchors bracketing a life-loss → the life counter appears in the ranked list.
3. Composes: a found address → `candidate_patch` freeze → `candidate_run` → verdict shows the
   counter held (the verify half is 796, already gated green).

## Scope boundary (stated)

- Ships the **finder + the verify-loop mechanics** (via 796). The FULL autonomous cheat —
  fan out N freeze *approaches* and auto-generate working freeze CODE for a per-frame
  decrement (an IRQ-hook that rewrites the counter, needing the writer's location via
  taint/trace + code-gen) — needs a **real game target** (owner's corpus). That live demo is
  the next step; this spec is the reusable finder + the loop wiring.
- Address-only decrement detection (byte counters). Multi-byte counters / non-decreasing
  cheats (score-freeze, flags) are a later heuristic.

## Cross-links

Spec 794 (diff) · 796 (candidate verify) · 797 (delta output) · 762 (original cheat-finder
number, subsumed) · yardstick #5 · `docs/usecases-runtime-dev-sandbox.md`.
