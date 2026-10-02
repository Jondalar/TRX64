# 887 — Idle exit

**Status:** MERGED on main 2026-10-02 — archived with its decision (`_archive/README.md`, 887)
**Pairs with:** C64RE 886 (the auto-start passes `--idle-exit 600`, a `runtime_keep_alive` tool, the deadline in its status).

## Problem

C64RE starts the shared daemon detached, so it survives an MCP reconnect. Nothing ever
ends it: every closed or killed session leaves one running for good. The owner decided
that an auto-started daemon ends itself after a stretch of idleness. The next tool call
starts a fresh one; the machine state is lost, and that is accepted.

## Decisions

**D1 — `--idle-exit <seconds>`.** Off by default (`0` = never). When set, the daemon
exits once it has been idle for that long: exit code 0, one stderr line
`[trx64] idle for N s — exiting`. A daemon nobody started with the flag behaves exactly
as before.

**D2 — what idle means.** All of these at once, for the whole window:

- no RPC request from any client — every request resets the clock, `ping` included (a
  client that pings is a client that is there);
- no A/V stream subscriber — a plain RPC connection holds nothing, only what it sends
  counts. Streaming daemons subscribe every connection to the A/V stream on connect, so a
  client that only speaks RPC (C64RE's MCP keeps one socket for its whole life) connects
  with `?av=0`: no A/V push, no pacing loop started on its account, not a subscriber.
  A `--headless` daemon has no A/V stream, so no connection holds it;
- no trace recording.

While a subscriber is connected or a trace records, the clock is held at "now", so the
window starts when the last of them ends, not when it began. An open socket that says
nothing does not keep the daemon: when it ends, that socket is simply closed.

**D3 — media are persisted before the exit.** A daemon that ends itself must not throw away
a write the user made. Before exiting it persists the cartridge and every drive's disk
exactly as eject does (dirty and persisting only). A trace cannot be recording (D2), so
nothing is half-written.

**D4 — `daemon/keep_alive { seconds: number | null }`.** Hold the daemon for at least
`seconds` from now; `null` means never exit on idle (until a later `keep_alive` with a
number replaces it). The request itself is activity (D2). Reply: the `idleExit` object of D5.

**D5 — the status.** `ping` and `session/state` carry

```json
"idleExit": {
  "armedSeconds": 600,          // 0 = idle exit off
  "deadlineMs": 1759400000000,  // epoch ms the daemon ends at if nothing happens; null = never
  "keptAliveUntilMs": null,     // epoch ms a keep_alive holds it until; null = none
  "keptForever": false,         // keep_alive { seconds: null } is in force
  "holding": "subscriber"       // what holds the clock now: "subscriber" | "trace" | null
}
```

`deadlineMs` is `null` when idle exit is off, when kept forever, or while something holds
the clock (D2). Otherwise it is the later of the idle deadline and the keep-alive deadline.

## Acceptance

- After the window with no client, no request and no trace, the daemon exits 0 with the line.
- A request resets the window; `keep_alive {seconds}` holds it at least that long; `null` holds
  it indefinitely; an A/V subscriber or a recording trace holds it; an `?av=0` socket that
  sends nothing does not.
- `ping` and `session/state` report `idleExit`; `keep_alive` replies with it.
- Without `--idle-exit` nothing changes.
