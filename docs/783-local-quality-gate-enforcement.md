# Spec 783 — Local Quality-Gate Enforcement (no cloud CI)

**Status:** HALF BUILT (measured 2026-08-12). The gates all exist —
`seven_game_gate.rs`, `iso_vic_gate.rs`, `vic_collision_gate.rs`, `cart_mapper_gate.rs`,
`tools/oracle/src/conformance.ts` and 7/7 screenshot oracles. The ENFORCEMENT does not:
`.git/hooks/` is empty and no `core.hooksPath` is set. This spec says of itself "they
exist — this is wiring + enforcement", and the wiring is the missing half. Consequence,
stated plainly: the regression protection that CLAUDE.md and every runtime commit point
at holds only because someone runs `cargo test` by hand. **Repo:** TRX64.
**Shared cross-repo numbering** (registry = C64RE `specs/README.md`).

## Motivation

1:1-parity is achieved — the oracle's job (TS:VICE, then TRX64:TS) is done; TRX64
is feature-complete-vs-TS + ahead (dump-ring / reverse-debug / time-travel).
**What's needed now is regression protection**, not the oracle: reliable quality
gates so a change can't *silently* regress the runtime. Deliberately **local, no
GitHub CI** (two-person / local-first workflow) — the point is "automatic + blocks
on every change", not "cloud".

## What already exists (reuse, do NOT rebuild)

- `crates/trx64-core/tests/seven_game_gate.rs` — the 7-game gate.
- `crates/trx64-core/tests/iso_vic_gate.rs` · `vic_collision_gate.rs` · `cart_mapper_gate.rs`.
- `tools/oracle/src/conformance.ts` — TS↔TRX64 conformance harness.
- `traces/gate_*_trx64.png` — 7 screenshot oracles (motm/scramble/lastninja/polarbear/maniac/greenberet/impossible2).

The gates exist **as `cargo test`**. The only gap = nobody is *forced* to run them.

## What is NEW (this spec)

### 783.1 — one gate command
`scripts/gate.sh` runs the full gate and **exits non-zero on any red**:
- `cargo clippy --all-targets -- -D warnings` (lint) — **but only blocking if HEAD is
  already clippy-clean; if there is a pre-existing warning backlog, run it
  non-blocking / at a baseline and flag the cleanup as follow-up** (do not red the
  whole gate on legacy warnings),
- `cargo test` for the gate tests (`seven_game_gate`, `iso_vic_gate`,
  `vic_collision_gate`, `cart_mapper_gate`) — release profile,
- the `tools/oracle` conformance run (node),
- screenshot compare vs `traces/gate_*_trx64.png` (if not already inside the game gate).
One command, GREEN/RED, quiet on green, first-failure-loud on red.

### 783.2 — pre-push hook (automatic + blocking)
`hooks/pre-push` runs `scripts/gate.sh` and **blocks the push on red**. Shared via
a committed `hooks/` dir + `git config core.hooksPath hooks` + a one-line installer
(`scripts/install-hooks.sh`, idempotent) so both machines get it. An escape hatch
`GATE_SKIP=1 git push` exists for deliberate WIP pushes (loud warning), never the default.

**It reads the push first (2026-08-14).** As first written the hook gated every push,
so nine commits of Markdown spent a full clippy + release-test + 7-game run proving
that documentation had not broken the emulator. A gate belongs to a change, not to an
operation. The hook now resolves each pushed ref from stdin
(`<local-ref> <local-sha> <remote-ref> <remote-sha>`), collects the changed paths, and
skips when **every** one of them is inert.

The test is a whitelist of inert files — `.md`, `.txt`, images, `LICENSE`, `THANKS.md`,
`.gitignore` — and **not** a blacklist of code. Anything unrecognised runs the gate: a
new file type, a `Cargo.toml`, a build script, a vendored `.cpp`, an unresolvable range,
an empty stdin. Guessing wrong that way costs a build; guessing wrong the other way
ships a red push. A branch deletion carries no files and is marked as such, so deleting
one branch while pushing code to another still gates on the code.

`GATE_DRYRUN=1 git push` prints the decision and the files behind it without running
anything.

### 783.3 — mandatory-green before the pin
The 771.6 version-pin (TRX64 tag + SHA → `runtime/TRX64_VERSION`) runs `gate.sh`
first; **no tag/SHA is pinned on a red gate** ("freeze only a green state").

## Non-goals
- GitHub Actions / any cloud CI (deliberately local).
- Building new gates (they exist — this is wiring + enforcement).
- Running the heavy game gates on every `cargo build` (wrong phase, too slow) — the
  danger points are **push + pin**, not compile. Not a `build.rs` concern.

## Acceptance
1. `scripts/gate.sh` is GREEN on current TRX64 HEAD (exit 0).
2. A deliberately-broken gate makes `gate.sh` **and** the pre-push hook exit
   non-zero (the push is blocked) — verified.
3. `scripts/install-hooks.sh` wires `core.hooksPath=hooks`; a fresh clone + install
   → the hook fires on push.
4. `GATE_SKIP=1` bypasses with a loud warning; absence of the var enforces.
5. Documented in `README.md` / `CHECKLIST.md` (how to run, how it enforces).

## Follow-up (separate, AFTER this lands green)
Once the enforced QG owns "don't regress", retire the oracle/proof/single-path
**doctrine**: C64RE Spec 715 (proof baseline) + 723 (single-path) demoted/archived,
and the CLAUDE.md mandatory blocks (both repos) rewritten — the TS oracle is no
longer the authority. Tracked on the C64RE board (doctrine-timing decision).
