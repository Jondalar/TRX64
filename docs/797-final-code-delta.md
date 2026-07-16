# Spec 797 — Final Code Delta

**Status:** PROPOSED (2026-07-16). **Repo:** C64RE (meaning). **Board:** row 797. **Next: 798.**

**Base:** Spec 796 `candidate_export` (`{ id, patches: [{ space, bank, addr, source }] }`).
**Yardstick #4** — the meaning bridge: the code that goes into the real build.

## What it does

Turn a candidate's exported source-patch-set into a **build-ready delta on disk**.
Build-agnostic by default (the owner's real build pipeline is not assumed): one `.asm`
per target (the source already carries its own org from how the patch was written) +
`delta-manifest.json` (machine) + `DELTA.md` (human). Drop the files into whatever build
you use, or apply per the manifest.

## Surface

- `src/candidate-delta.ts` — pure `buildDelta(export)` (files + manifest) + `writeDelta(
  export, outDir)`. Deterministic filenames `patch_<i>_<space>[_b<bank>]_<addr>.asm`.
- C64RE MCP `runtime_candidate_derive_delta(session_id, id, out_dir?)` — calls the daemon
  `candidate_export`, writes the delta to `out_dir` (default `<project>/delta-<id>`),
  returns `{ outDir, files, manifest }`. In DEFAULT_TOOLS.

## Acceptance

1. Each patch → one `.asm` whose content is the patch source (its own org preserved).
2. `delta-manifest.json` lists every patch's `{ space, bank, addr, file }`.
3. `DELTA.md` renders a human target→file table.
4. Deterministic: same export → same filenames + content.

## Scope boundary

- Default = source-patch files + manifest (build-agnostic). A unified-diff-vs-original or
  a specific assembler-segment format can be added when a concrete build pipeline is named
  (the owner deferred the exact build integration; this ships the universal form).
- No apply-to-original-source automation (the manifest gives the targets; applying is the
  owner's build step).

## Cross-links

Spec 796 (candidate / export) · 795 (overlay) · 794 (eval) · yardstick #4 ·
concept map `docs/concepts-snapshots-scenarios-overlays.md`.
