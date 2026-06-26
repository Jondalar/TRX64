# TRX64

**The runtime behind [C64ReverseEngineeringMCP](https://github.com/Jondalar/C64ReverseEngineeringMCP).**
A native (Rust) cycle-exact Commodore 64 + 1541 emulator that you drive over a
WebSocket JSON-RPC API.

This is a faithful but enhanced vice clone, made with a modern headless and API first aproach in mind. You can use this together with C64RE or any other application - be it a separate front end (I am building my macOS native version currently) or just with an llm. 

.c64re formats are compatible with C64RE, a .vsf compatibility layer is included. TRX64 can read and import .vsf but N O T export. 

As per Request by Count Zero on behalf of csdb staff, I removed any associtation on csdb with this. If you want to interact with me on both, TRX64 or C64RE please use github.

---

## Quick start

```bash
cargo build --release
./target/release/trx64-daemon --project <dir> --port 4312 [--stream]
```

- `--project <dir>` — working dir for media, snapshots, traces.
- `--port <p>` — the WebSocket port (C64RE's default is 4312).
- `--stream` — run the continuous per-frame driver (live free-run: video frames,
  breakpoints, JAM auto-break, recorder, observers). Omit for a request/response core.

Inside the C64RE project you select TRX64 as the backend with `TRX64=1 ./ui.sh restart`
(it points `C64RE_RUNTIME_BIN` at this binary).

---

## The API

Everything is **JSON-RPC 2.0 over a WebSocket**. One request:

```json
{ "jsonrpc": "2.0", "id": 1, "method": "session/create", "params": { "pal": true } }
```

There is **one machine per process**, shared by every client (human + LLM co-drive the
same session). A typical flow: `session/create` → `debug/run` → inspect via
`monitor/exec` / `trace/*` / `vic/inspect` → `checkpoint/*` to scrub → `snapshot/dump`
to persist.

### Session lifecycle
| method | purpose |
|---|---|
| `session/create` | construct or **attach** to the live machine (`{pal}`); returns `sessionId`, `pc`, `c64Cycles` |
| `session/list` · `session/state` | list sessions · full machine state (pc, cycles, run-state, regs) |
| `session/reset` · `session/close` | cold reset · close |
| `session/screenshot` · `session/render_screen` | PNG of the current frame · decoded screen |

### Run control & pacing
| method | purpose |
|---|---|
| `debug/run` · `debug/pause` · `debug/continue` | free-run (async) · pause · resume |
| `debug/step` · `session/run` | step one instruction · run an exact cycle budget (returns at budget or a breakpoint) |
| `debug/state` | the full controller state (run-state, pacing, pc, cycles, frame, breakpoints, stop) |
| `session/set_pacing` | `realtime` \| `warp` (8× fast-forward) — let an LLM search fast |
| `debug/break_add` · `debug/break_del` · `debug/break_list` | execution breakpoints |

### Input
`session/key_down` · `session/key_up` · `session/type` · `session/release_keys` ·
`session/joystick_set` · `session/joystick_clear` · `session/load_prg` ·
`runtime/run_prg` (load + autostart a `.prg`).

### Monitor — the VICE-superset REPL
**`monitor/exec` `{ session_id, command }`** drives the whole interactive monitor
(~128 verbs) over one method: `g`/`x`/`n`/`z`/`until`/`ret` (run/step), `m`/`d`/`a`
(dump/disasm/assemble, bank-lens aware), `bk`/`obs` (breakpoints/observers),
`sd`/`df` (dynamic/flow disasm), `t`/`c`/`h` (transfer/compare/hunt), `flow`/`bt`
(interrupt frames / backtrace), `trace`, `map`/`taint`/`swimlane`, `screen`/`bitmap`,
`io`, `inspect`/`xref`/`sym`, and the reverse-debug verbs below. `help` lists them all.

### Media
`media/mount` · `media/swap` · `media/unmount` · `media/recent` · `media/browse` ·
`media/persist` · `media/ingress` · `session/cart_status` · `session/drive_status` ·
`session/drive_power`. Disk (`.d64`/`.g64`) and cartridge (`.crt`, all VICE mapper
families incl. writable flash) mount through one checkpointing ingress.

### Trace (the forensic firehose)
| method | purpose |
|---|---|
| `trace/start_domains` | start a live capture for domains `c64-cpu`/`drive8-cpu`/`iec`/`memory` → a `.c64retrace` |
| `trace/read` | query a finalized trace (swimlane / map / taint / events / top-pcs …) |
| `trace/build_from_ring` | **carve a `.c64retrace` for an exact cycle window** out of the always-on delta ring (no pre-arming) |
| `trace/current` · `runtime/mark` | active-trace status · stamp a named marker |

### Checkpoint / scrub & snapshots
`checkpoint/capture` · `checkpoint/restore` · `checkpoint/list` ·
`checkpoint/thumbnails` · `checkpoint/pin` · `checkpoint/unpin` · `checkpoint/clear`
— the ring-bound rewind (10 s scrub-filmstrip). `snapshot/dump` · `snapshot/undump`
write the full machine to `.c64re`. `vsf/save` · `vsf/load` interop with VICE `.vsf`.

### Reverse-debug — *TRX64 superset* (the TS runtime cannot do these)
An **always-on, no-pre-arming** ring keeps the last ~10 s of instructions + writes.
| method / verb | purpose |
|---|---|
| `chis` (monitor) | live CPU instruction history (registers per step), while running, no trace needed |
| `runtime/reverse_step` · monitor `rstep` | UNDO the last N instructions — restore CPU+RAM+IO byte-exact |
| `runtime/who_wrote` · monitor `whowrote <addr>` | who last wrote an address (PC + cycle + old→new) — the stack-crash shortcut |
| `runtime/crash_triage` · monitor `triage` | on JAM, the causal chain: crash PC → wild transfer → stack corruptor |
| `trace/build_from_ring` | dump the window of interest from the ring → full disasm + taint |

On the TS runtime these methods return a clean `not supported by the TypeScript
runtime — use the TRX64 runtime` decline.

### Recorder, time-travel, scenarios, audio
`recorder/start|stop|list|dump|status` (bounded data-stream recorder) ·
`runtime/snapshot_tree` · `runtime/overlay_run` · `runtime/promote_branch` ·
`runtime/swap_disk_and_continue` (branch / overlay-debug) · `runtime/scenario_save|load|run|list|delete` ·
`audio/start|stop|export` · `vic/inspect` · `runtime/call` (trace-backed agent queries).

---

## Interchange formats

`.c64re` (snapshot: full machine state) and `.c64retrace` (binary trace log) are the
two formats used to **move a machine between instances** — daemon ↔ a future standalone
app ↔ the parity oracle. Both are written byte-faithfully to the C64RE contract.

---

## Architecture

Separation of concerns is the performance — the core is monomorphized and branch-free:

```
trx64-daemon   tokio · WS JSON-RPC 2.0 · binary frames · the stream loop
trx64-session  session lifecycle · run control · snapshot/rewind · warp
trx64-trace    TraceOp encoder → .c64retrace (the immovable format)
trx64-core     pure/deterministic/sync emulation · zero-cost Observer · Clone-able state
```

The 1541 drive CPU is a **separate** 6502 (`crates/trx64-core/src/vice1541/` +
`drive_6510core.rs`), governed by the Spec-612 port-fidelity doctrine.

---

## License & Credits

TRX64 is licensed under the **GNU General Public License v3.0 or later**
(`GPL-3.0-or-later`). See [LICENSE](LICENSE).

TRX64's emulation cores are a **source-faithful port of [VICE](https://vice-emu.sourceforge.io/)**
(GPL-2.0-or-later; TRX64 uses the "or later" permission). Full credits — VICE, C64RE,
the scene, and ROM/media notices — are in [THANKS.md](THANKS.md).
