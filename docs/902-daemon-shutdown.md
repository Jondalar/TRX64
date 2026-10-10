# 902 (TRX64 half) — The daemon can be asked to end, and ends cleanly however it is asked

**Status:** BUILT on branch spec-902-shutdown
**Repo:** TRX64 (daemon). The C64RE half is `C64ReverseEngineeringMCP/specs/902-shut-down-means-down.md`.
**Number:** 902 (board: `C64ReverseEngineeringMCP/specs/README.md`)
**Origin:** C64RE 902 D6 (owner, 2026-10-10): Windows has no SIGTERM, a kill there is a hard
kill; C64RE asks the daemon to end first and only falls back to `taskkill /T /F` after 5 s.
Patch: additions only.

## 1. What is wrong

Read, not assumed (main at `e0ac127`):

- There is **no request that ends the daemon**. The only orderly exit is the idle watch
  (`--idle-exit`, Spec 887, `main.rs` ~18966): it calls `persist_media_before_exit` (dirty,
  persisting cartridge and both drives' disks, as eject does) and then `exit(0)`.
- **SIGTERM / Ctrl-C are not handled at all.** The process dies on the default action, so a
  cartridge or disk the program wrote since its last write-back is lost. C64RE's "shut down"
  (and every `kill <pid>` today) is therefore lossy on macOS/Linux too, not only on Windows.

## 2. Decision

**D1 — `daemon/shutdown`.** Params: none (unknown params ignored). It:
1. runs the same exit work as the idle exit (D3),
2. replies `{ ok: true, persisted: { cartridge: <path|null>, disks: [<path>…] }, exitCode: 0 }`,
3. makes sure that reply is **written and flushed to the caller's socket**, then ends the process
   with exit 0. The caller sees the reply, then the connection close.

Refused with -32001 only if the exit work itself cannot run (none known today); a running
machine is not a reason to refuse — shutdown pauses it.

**D2 — SIGTERM, SIGINT (Ctrl-C) and, on Windows, Ctrl-C / Ctrl-Break / console close run the
same exit work** and then exit 0. One function, three callers (request, signal, idle watch); the
idle watch's inline block is replaced by a call to it.

**D3 — the exit work, one function** (`shutdown_now` or similar):
- stop the autonomous run if it is running,
- if a trace is recording, end it the way the trace stop request does, so the file on disk is
  complete and readable (read how `debug/trace_stop` / the trace finalize path closes it — do not
  invent a second path),
- `persist_media_before_exit` (unchanged),
- log one line naming the reason (`request`, `SIGTERM`, `SIGINT`, `idle N s`) to stderr.

It runs once: a second shutdown request or a signal arriving during the exit work does not run it
again.

**D4 — listed** wherever `daemon/keep_alive` is (dispatch + `is_operating_method` and any method
listing).

## 3. Out

- No graceful drain of other clients' in-flight requests: they get the connection close.
- No change to `--idle-exit` semantics other than routing through D3.
- C64RE's ledger, hold and `down` command — that is C64RE 902.
- No release (owner's call).

## 4. Acceptance

1. A spawned `trx64-daemon` (test spawns the built binary on a free port) answers
   `daemon/shutdown` with the D1 reply, then the process exits with code 0 within 2 s.
2. Same daemon with a writable, dirty cartridge (or disk) mounted: after `daemon/shutdown` the
   file on disk carries the write.
3. Same as 2 via SIGTERM (unix test) — the write survives; exit code 0.
4. A recording trace at shutdown: the trace file is complete (opens / reads like one stopped by
   request).
5. Idle exit still persists and exits 0 (existing 887 tests green).
6. Full gate green.
