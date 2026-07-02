# TRX64 ↔ c64re TS Headless — WS Surface Reconciliation (2026-06-25)

Class-by-class behavioral audit of every WS JSON-RPC method: TS (`src/workspace-ui/
ws-server.ts` + `runtime-controller.ts`) is the authority; TRX64 (`crates/trx64-daemon/
src/main.rs` + core) must match 1:1. **TRX64 adapts to TS, never the reverse.**

Method-set: TS 90 / TRX64 87. All findings verified against source.

## Legend
✅ 1:1 · ⚠️ shape/behavior diverges · ❌ missing/stub/no-op · 🔵 extra (TRX64-only)

## Tier 1 — live-UI bugs (user-reported) + quick wins

| Item | Status | Root cause | Fix |
|---|---|---|---|
| Freeze / can't type | ✅ FIXED | runState/broadcast | done (commits 434b3e3, 62d76dd) |
| Pause/Power/Reset-stream churn | ✅ FIXED | stream_loop ignored `running` | done (8efeefb) |
| **Reset (soft) broken** | ❌ | `session/reset` soft → `cold_reset`; **no `resetWarm`** (no $01 banking restore) | port `resetWarm()` into trx64-core; soft uses it |
| **Joystick dead** | ❌ | no joystick model in core (`full.rs:366`); `joystick_set/clear/release_keys/input_status` all no-op/hardcoded | port joystick model → CIA1 port1/2 read; wire all 4 |
| **Audio light OFF** | ❌ | `session/state.sid.streaming` hardcoded `false` | `StreamHub::has_subscribers()` → real flag |
| `session/set_pacing` | ❌ MISSING | not implemented | add handler (pal/warp/fixed-ratio) → setPacing |
| `debug/control` broadcast | ❌ | `source` param never read; owner never tracked/broadcast | track control_owner in State; broadcast on change in run/pause/continue/step |
| `session/drive_power` shape | ⚠️ | extra `"mode"` key on failure path | match TS (omit mode on fallback) |
| `session/create.attached` | ⚠️ | hardcoded `true` | reflect real attach |

## Tier 2 — UI tabs / observability

| Item | Status | Note | Fix |
|---|---|---|---|
| **`runtime/call`** | ❌ MISSING | UI Snapshots/Scenarios/Trace tabs call it (AgentQueryApi facade) | add dispatch → op allowlist |
| `debug/observer_log` | ❌ | drain infra EXISTS in observers.rs, never called in run_debug_control | drain pending_log/marks/cmds after run + broadcast |
| `session/drive_status.sector` | ❌ | hardcoded 0 (TODO) | GCR sector-under-head decoder (HIGH) |
| `session/cart_status` write-LED | ⚠️ | no `writableGeneration`; activity never "write" | port write-pulse counter (BUG-042) |
| `media/cart_persisted` broadcast | ❌ | never fired; TS auto-persists per-frame | hook persist + broadcast |
| `session/input_status` joystick | ❌ | hardcoded released | wire to joystick model (dep Tier-1 joystick) |
| `trace/run/start` | ❌ MISSING | definition-id trace start (vs start_domains) | add handler (reuse start_domains) |
| `trace/run/mark` | ❌ MISSING | manual marker | add marks vec + handler + finalize |
| `trace/current` | ❌ MISSING | last-store path convenience | track last_trace_path/run_id |
| `debug/memory_access_map` | ❌ STUB | returns NOT_IMPLEMENTED | port MemoryAccessTracker |
| `audio/start|stop` return | ⚠️ | hardcoded (singleton hub) | reflect hub state |

## Tier 3 — deep ports (deferred / high-effort)

| Item | Status | Note |
|---|---|---|
| `vic/inspect/at`,`/region` + provenance | ❌ deferred | port vic-inspect engine (Spec 710/721) |
| `runtime/snapshot_tree`,`promote_branch` | ❌ MISSING | time-travel branch tracker (Spec 769) — high effort |
| `runtime/overlay_run` | ❌ MISSING | anchor→patch→run→read (Spec 769.8) — medium |
| `runtime/run_prg` autostart | ⚠️ | only sets PC, no BASIC RUN | low |
| `media/ingress` full contract | ⚠️ | no checkpoint/dirty-guard/resume; **CRT path = -32601** (mount path works → MM loads) | port Spec 709.13 |
| `trace/read` | ❌ STUB | 7 query ops; needs DuckDB | Phase 2 |
| `scenario_save` durability | ⚠️ | in-memory vs file | low |

## Confirmed OK (1:1, no action)
checkpoint/* (list/capture/pin/unpin/restore/clear/thumbnails), vsf/save|load (incl
real-VICE autodetect), scenario_list/load/run/delete, media/mount|unmount|swap|browse|
list_paths|recent|events, session/state(core)|run|type|key_down|key_up|load_prg|
screenshot|list|close, debug/run|pause|continue|step|state|break_*, debug/running|
paused|stopped|breakpoint_hit|observer_hit|checkpoint_restored, audio/export|flush,
api/call, monitor/exec (real run_monitor dispatch), runtime/mark|swap_disk_and_continue.

## Extra in TRX64 (keep — benign / variant)
🔵 `recorder/start|stop|capture` (explicit lifecycle vs TS implicit — deterministic
variant), `runtime/render_screen` (upscale feature-add), `vic/inspect` (limited
descriptor).
