# TRX64 — Handover (for the next MCP session)

**State: FEATURE-COMPLETE vs the c64re TS headless runtime** (ADR-087, 2026-06-25).
TRX64 is a verbatim Rust re-implementation of the C64ReverseEngineeringMCP (c64re) headless C64 runtime — a
behavior-identical drop-in that speaks the same WS JSON-RPC 2.0 protocol on `ws://127.0.0.1:<port>` and writes
the same `.c64retrace` format. ~8–10× faster than the TS core. 55 build-loop items, 89 ADRs.

Read this, the `README.md` (the drop-in concept), `loop/decisions.md` (the ADR log — the *why* of every
decision), and `docs/integration-report.md` (the feature-complete capstone scorecard).

---

## What's done (the whole surface)
- **Verbatim VICE cores** — `c64_6510core.rs` (x64sc SC CPU), `vic.rs` + `vic_draw.rs` (viciisc per-cycle engine
  + per-cycle pixel/sprite draw), `drive_6510core.rs` (1541 6510core.c) + `viacore.rs`/`rotation.rs`/`iec.rs`
  (the 1:1 drive VIA/rotation/IEC). The bar was **"wie VICE, nicht so ähnlich"** — these are byte-exact, not
  pattern-engine approximations.
- **Behavioral proof** — the scramble custom $DD00/KRILL loader renders clean (VIA1 was the blocker); the
  **7-game gate 7/7** (scramble/polarbear/motm/greenberet/impossible2/lastninja/maniac — .d64 + .g64 GCR/half-
  track loaders); maniac char-select at 600M cyc.
- **Cartridge** — read tier (Normal/MagicDesk/Ocean) + **writable flash** (Flash040 + M93C86 + EasyFlash/GMOD2).
- **Drive** — G64 mount + read (GCR/half-tracks) + **write-back** (.g64/.d64 persist).
- **reSID** — the vendored GPL C++ reSID via cc-FFI (`vendor/resid/`), byte-deterministic; config matches the TS
  exactly (6581, filter OFF).
- **Full WS surface** — protocol b1/b2 (incl. held-key input), audio/media/batch, recorder/scenario, the
  **checkpoint-ring** (rewind + checkpoint/*), **vic-inspect** (9/9, visual provenance), and the **broadcast/push**
  channel (debug/breakpoint_hit, batch/progress, audio/flush) on the hub.
- **Snapshots** — **`.c64re` 100% cross-runtime** (dump on TRX64 → undump+resume in c64re, both directions, even
  mid-game — full RuntimeCheckpoint incl. the VICE drive blob) + VSF byte-parity + reads real VICE x64sc `.vsf`.
- **Observability** — tick hooks (on_interrupt/breakpoint/watch), the ObserverRegistry (breakpoints/watchpoints/
  conditional/until), the A/V binary push (ws-av-tap works — start the daemon with `--stream`).

## Architecture (4 crates)
- `crates/trx64-core` — pure/sync emulation (CPU/VIC/CIA/SID/drive/cart/gcr/snapshot/vic_inspect). No I/O.
- `crates/trx64-daemon` — the WS JSON-RPC 2.0 server (`main.rs` = the method dispatch; `streaming.rs` = the
  StreamHub A/V push + the NotifyHub broadcast). **This is the drop-in surface.**
- `crates/trx64-session`, `crates/trx64-trace` — session glue + the `.c64retrace` trace sink.
- `tools/oracle/` — the differential gate harness (TS-golden vs TRX64, hermetic dual-daemon, byte-exact trace +
  response diff). `src/integration.ts` = the cross-runtime e2e capstone. **`src/oracle.ts` is the gate engine —
  treat as off-limits / additive-only.**

## Build / run / test (always `rtk`-prefix)
```bash
rtk cargo build --release                          # the core + daemon (compiles the vendored reSID C++ via cc)
rtk cargo test --workspace -- --test-threads=1     # 269 pass (resid_oracle FFI-singleton flakes under parallel)
# byte-exact + behavioral gates (the differential oracle):
cd tools/oracle && node_modules/.bin/tsx src/oracle.ts record corpus/drive/drive-boot-deep.json
                   node_modules/.bin/tsx src/oracle.ts compare corpus/drive/drive-boot-deep.json
# the 7-game gate (GATE_BUDGET env for slow multi-file loaders, e.g. maniac):
GATE_BUDGET=600000000 cargo test -p trx64-core --release g8_maniac -- --ignored --nocapture
# cross-runtime e2e vs a live c64re daemon:
cd tools/oracle && node_modules/.bin/tsx src/integration.ts --only scramble --report ../../docs/integration-report.md
```
A live c64re reference daemon (for parity diffs):
`cd ../C64ReverseEngineeringMCP && node_modules/.bin/tsx src/runtime/headless/daemon/run.ts --project /tmp/c64re --port 4312`

## The integration path (how c64re uses TRX64)
The drop-in boundary is the **WS daemon**, NOT an in-process core swap (ADR-066). The recommended path: a c64re
feature branch where `ui.sh`/`npm run workspace` launches the **TRX64 daemon** (`trx64-daemon --port 4312
[--stream]`) instead of the TS daemon — UI + the 50+ MCP tools connect to the same WS port, unchanged. A/B-able
via a backend flag; the TS runtime stays as the golden oracle. The whole feature-complete WS surface exists so
this swap is clean. (Tighter coupling — FFI/N-API or WASM — is a later option; not needed.)

## Key decisions to read first (in loop/decisions.md)
- **The through-line: port the c64re TS classes 1:1, never distill/approximate.** The drive stack + scramble
  only worked once the DISTILLED classes were rebuilt verbatim (ADR-058..061: viacore/rotation/iecbus/via1).
  Same lesson as CPU/VIC. If something diverges on cycle-tight code, suspect an approximation, not a small bug.
- **Bar = BEHAVIORAL parity with c64re** (renders games/demos, .g64 loaders, the 7-game gate) — NOT cycle-exact-
  vs-the-oracle (the c64re oracle itself isn't 101% VICE; TRX64-verbatim gives the *real* VICE cycle count).
  ADR-053.
- **Perf: benchmark c64re via `node` on compiled `dist/`, NEVER `tsx`** (tsx ~22× slower → a bogus 200×). Real
  ratio ~8–10×. ADR-065, `docs/perf-compare.md`.
- **.c64re cross-runtime** (ADR-077/078/079) — the runtime snapshot format; full RuntimeCheckpoint incl. the
  VICE drive blob; additive snapshot-serialization of drive/VIC state is allowed (logic unchanged, byte-exact
  gates the guard).

## The build-loop infra (loop/) — now STOPPED
`loop/` drove an autonomous cron loop: a Driver (dispatch builder → byte-exact gate → merge → advance) over
specialist builder subagents, each porting one piece verbatim. `loop/decisions.md` (89 ADRs), `loop/journal.md`,
`loop/state.json` (`done` = 55 items, `feature_complete: true`), `loop/backlog.md`, `CHECKLIST.md`. The cron
(`501ae922`) was cancelled at feature-complete. **Builders must NOT edit `loop/` files.** Restart with `/loop`
only for the optional beyond-parity extras.

## Open follow-ups (ALL beyond TS parity — optional)
- Real-VICE VSF **write** (TRX64 reads real VICE `.vsf`; writing needs the 123KB VIC-IISC pipeline blob).
- The J2 `derived_asset` trace-chain in vic-inspect (no trace source yet; exact/runtime_generated paths done).
- The remaining c64re broadcasts (debug/running|paused|stopped, session/frame_available, media/cart_persisted) —
  each a one-line `st.notify.broadcast(...)` on the existing NotifyHub.
- `checkpoint/thumbnails` filmstrip is done; vic/inspect oracle goldens (no corpus yet).
- Test hygiene: mark `resid_oracle` tests serial (the FFI singleton flakes under `--test-threads`>1).

## Landmines
- **No `git worktree` auto-isolation** in this harness — parallel file-mutating builders need *manual* `git
  worktree add` (the loop used `/tmp/trx64-mm` merge worktrees; file-disjoint branches merge clean).
- The 7-game gate `g8_maniac` needs `GATE_BUDGET=600000000` (slow standard-serial multi-file loader).
- `samples/motm.vsf` (a copyrighted game snapshot) is tracked as a test fixture — **remove before any public
  push.** Repo `Jondalar/TRX64` is private.
- `resid_oracle` flakes only under parallel test scheduling (the reSID shim is a process-global singleton).
- Pre-existing harmless REDs: `iso-vic-badline-irq`/`-sprites` (VIC-cycle-count gaps vs the imperfect oracle),
  `scramble-load-progress` end5 (1-byte custom-loader phase residual). Documented; not regressions.
