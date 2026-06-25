# TRX64 Parity-Loop Journal (Phase 2)

Newest first. One entry per iteration: item, model, gate result, commit, divergences.

## 2026-06-25 — loop armed
Backlog from `docs/ts-parity-reconciliation.md` (6-class audit). 19 items across 3 tiers.
Already landed before the loop (freeze + pause/power roots):
- 434b3e3 debug/* lifecycle broadcasts + session/state runState
- 62d76dd session/state shape 1:1 (drop controlOwner)
- d58c732 debug/checkpoint_restored
- 8efeefb stream_loop honors run-state (pause/power/reset-stream)
Driver = this session. Start: Tier 1.

## 2026-06-25 — Tier-1+2 workflow (wj3ssxqec) COMPLETE: 13/13 done
T1.4 drive_power-shape, T1.5 create.attached(+integrate-fix→true, 2 stale goldens re-recorded),
T1.3 set_pacing, T1.2 debug/control-owner, T2.1 runtime/call-bridge, T2.5 media/cart_persisted,
T2.2 observer_log-drain, T2.6 trace/run/start|mark+current, T2.7 memory_access_map,
T2.4 cart write-LED, T1.6 warm_reset(soft-reset), T2.3 gcr sector-under-head, T1.7 joystick-model.
Integrate-gate caught T1.5 regression (attached false-on-first) → fixed to shared-attach true,
re-recorded boot-basic-ready + iso-cia-icr goldens. Full no-disk oracle GREEN 24/0. core 216/216.
Remaining: T2.8 monitor-command-parity + Tier-3 deep ports.

## 2026-06-25 — Tier-3 workflow (wvovswrtu) COMPLETE → BACKLOG DONE
T3.4 run_prg-autostart, T3.6 scenario-save-durability, T2.8 monitor-command-parity
(full VICE-superset: d/m/r/g/x/until/z/n/ret/bk/del/bank/f/t/c/h/reset/help; map/taint/
inspect/xref graceful-deferred), T3.3 media/ingress full Spec-709.13 + CRT path,
T3.1 vic-inspect-engine (was already ported; matrix stale), T3.2 time-travel
(overlay_run + snapshot_tree + promote_branch). Integrate-gate: full no-disk oracle
GREEN 24/0, release build clean, monitor verbs verified live (d/m/r/bk/bank/help).
loop_status=DONE. Only deferred: T3.5 trace/read (DuckDB query layer, Phase-2 by design).

## 2026-06-25 — UX self-test + step-past fix
User hit "dauerpause": root = a stray `bk $E5CD` I left from the T2.8 monitor verb probe
(PC idles on $E5CD → re-trips every run). del fixed it. Found + fixed a real parity gap:
debug/run did step-past-current-bp only for continue; TS run() does it always (PC-based).
Now both. UX verified live: run/pause(freeze)/reset(clean)/audio(SID ON)/monitor verbs
(d/m/r/bk/bank/help) all good.

## 2026-06-25 — careful UX sweep: CRT insert/eject (3 bugs found+fixed)
User: "kann KEIN crt einlegen". Root: the Inspector CART dropdown uses media/mount
slot:0 (not media/ingress, which T3.3 fixed). media/mount + media/swap + media/unmount
all ignored slot → treated cart as disk-on-drive8 / ejected drive8. Fixed all three to
route by extension/slot like TS adaptMount. Verified live: im3_MAGICDESK runs+renders
(mech title) + SID music; EF/GMOD2/MegaByter attach; eject → EMPTY; disk path unaffected.
Lesson: T3.3 fixed media/ingress but the live UI uses media/mount|swap|unmount — must
test the ACTUAL UI path, not just the audited handler.

## 2026-06-25 — UX sweep part 2 (tabs/warp/scrub) + warp fix
Tested live in Chrome: tabs (Memory Map / Payloads / Dashboard / Docs) render (empty in
verprobe project, expected). Scrub: pause → checkpoint filmstrip → click frame → seek
restores ✓. Reset (warm) → clean BASIC ✓. Found+fixed: Warp was a no-op (stream loop
ignored pacing_mode); wired warp → 8x cycles/frame (~5x live). Eject→BASIC verified clean
(vic.mode 0) — earlier black was a transient mid-boot grab, not a bug. Joystick model
wired (CIA1, core 216/216) — not deep-tested with a live joystick game. MON popout opens
a separate window (verbs verified headless: d/m/r/bk/bank/help).

## 2026-06-25 — KNOWN BUG: scrub-restore wedges daemon (drive VIA alarm spin)
User: pause → scrub (click filmstrip) → run → "VM tot". Diagnosed via `sample` of the
wedged trx64-daemon: trx64-av-stream thread stuck in stream_loop → run_for_full →
Drive1541::run_cycles → viacore::run_pending_alarms INFINITE loop (alarm_set/set_int
spinning). No panic — a true hang. run_pending_alarms is `while clk > next_pending_alarm_clk
{ dispatch }`; after a checkpoint/restore the drive clock (core.clk) and the VIA
alarm_context (next_pending_alarm_clk) are NOT restored on the same time-base, so clk >>
next deadline → dispatches forever → holds the state Mutex → every WS handler blocks →
daemon-wide wedge (new connections can't even handshake). FIX (next focused session):
make restore_live_checkpoint restore the drive clock + via1/via2 alarm_context consistently
(same CLK_BASE), and/or bound run_pending_alarms catch-up. Repro: boot, run a few s (ring
auto-captures), pause, restore an earlier checkpoint, run.

## 2026-06-25 — scrub/long-run wedge: ROOT FOUND + safety shipped
sample(wedged daemon) → viacore::run_pending_alarms infinite spin. ROOT: drive viacore
arms alarm deadlines u32-masked (& 0xffff_ffff) but drive clock is u64-monotonic
(Spec-743/BUG-025 not extended to the 1541 drive viacore) → drive clk > 2^32 makes the
deadline unreachable → ~4e9-iter catch-up holding the state Mutex → daemon-wide wedge.
Shipped a wedge-SAFETY (bail on gap > 2^31). REAL FIX = u64-monotonic viacore (drop the
~20 u32 masks), differential-tested vs VICE — own focused task. Repro: warp ~9 min (or
71 min real) to push drive clk past 2^32, then run_pending_alarms spins.
