# TRX64

A Rust drop-in for the [C64ReverseEngineeringMCP] headless C64 runtime.

You don't swap "a core" — you swap **the process behind `ws://127.0.0.1:4312`**.
UI and the 50+ MCP tools stay byte-for-byte unchanged; they just talk to whatever
serves the WebSocket. TRX64 speaks the same JSON-RPC 2.0 protocol and writes the same
`.c64retrace` trace format, so the TS runtime can act as a golden oracle for parity.

## Why

The current Node daemon is a single-threaded event loop with one default session and
serialized mutations. That — not raw compute — is the ceiling. A native binary unlocks:
- **warp** — native cycle loop, no GC/interp; thousands of fps to let the LLM search fast.
- **parallel** — 75 KiB machine state is `Clone`; COW-fork thousands of branches on a
  thread pool instead of one Node VM per scenario.
- **explore/overlay (Phase 2)** — apply coder-overlay/crack mutations, warp each forked
  branch, return compact probe verdicts. The reason this project exists.

## Layering (separation of concerns = the performance)

```
trx64-daemon   tokio · WS JSON-RPC 2.0 · binary frames · port-race · ping · crash-log
trx64-session  lifecycle · boot-paused · opChain · snapshot/rewind · warp+parallel (P2)
trx64-trace    TraceOp encoder → .c64retrace (immovable format)
trx64-core     pure/deterministic/sync emulation · generic zero-cost Observer · Clone-able
```

The core knows nothing of sockets, async, or the trace format. Everything above adds
exactly one concern. That keeps the hot path monomorphized and branch-free.

## Phases

- **Phase 1** — behavior-identical drop-in. Verified by `tools/oracle` trace-diff vs TS.
- **Phase 2** — warp / parallel / `explore()` mutation-search as *additive* new tools.

## Build process

An autonomous, context-stateless / disk-stateful build loop (`loop/`). State lives in
`loop/state.json` + `loop/backlog.md` + `loop/journal.md` + git, so it survives token
resets and resumes. See `loop/loop-prompt.md`.

```
cargo build        # workspace compiles (skeletons; emulation ported per backlog)
```

[C64ReverseEngineeringMCP]: ../C64ReverseEngineeringMCP
