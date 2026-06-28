# TRX64

**A headless, API-first, cycle-accurate Commodore 64 + 1541 runtime — a faithful,
enhanced port of VICE built to be driven by humans, tools, and LLM agents alike.**
Talk to it over WebSocket JSON-RPC or embed it in-process, then do what a normal
emulator can't: snapshot and rewind the machine, step *backwards* through code, and
ask a live trace who corrupted the stack — all cycle by cycle.

It runs the same scene software a real C64 does — multi-stage cracks, custom
fastloaders, EasyFlash cartridges — and its fidelity is gated against VICE on every
change.

---

## Why TRX64

A normal emulator runs code. TRX64 lets you *interrogate* it:

- **Time-travel debugging** — a checkpoint ring snapshots the machine continuously;
  scrub back to the exact cycle a fastloader flips a bank, then run on.
- **Reverse stepping** — `rstep` undoes the last instructions and restores CPU + RAM
  + I/O **byte-exact**. An always-on ring keeps the last ~10 s of instructions and
  writes, so it works with no pre-arming.
- **`whowrote <addr>`** — ask the trace who last wrote an address (PC + cycle +
  old→new value). The fastest way to find what corrupted the stack.
- **JAM auto-triage** — on a crash, the monitor prints the causal chain:
  crash PC → the wild jump → the stack corruptor.
- **A forensic trace firehose** — capture CPU / drive / IEC / memory to a binary
  log, index it, and query it as swimlanes, memory maps, or data-flow taint.
- **Faithful cartridges, including Save-to-Flash** — EasyFlash and the commercial
  CRT families (Ocean, Magic Desk, GMOD2/3, MegaByter …) run on source-faithful
  mapper cores. Writable flash and EEPROM genuinely persist: an EasyFlash
  *Save to Flash* survives a reset and round-trips through a snapshot.
- **Shared human + LLM sessions** — one machine per process, co-driven. A person and
  an agent inspect, step, and steer the *same* live C64 at the same time.
- **API-first** — every capability above is one JSON-RPC method. No GUI assumptions,
  no hidden state: scriptable by a tool, an LLM, or your own frontend.

---

## Quick start

```bash
cargo build --release
./target/release/trx64-daemon --project <dir> --port 4312 [--stream]
```

- `--project <dir>` — working directory for media, snapshots, and traces.
- `--port <p>` — the WebSocket port (C64RE's default is 4312).
- `--stream` — run the continuous per-frame driver for live free-run (video frames,
  breakpoints, JAM auto-break, recorder, observers). Omit it for a pure
  request/response core.

---

## Three ways to use it

TRX64 is one runtime with three front doors:

- **As a daemon** — the `trx64-daemon` binary serves WebSocket JSON-RPC. This is how
  [C64ReverseEngineeringMCP](https://github.com/Jondalar/C64ReverseEngineeringMCP)
  uses it (select it with `TRX64=1 ./ui.sh restart`), and how any client or LLM
  connects.
- **In-process (typed)** — the `trx64-ffi` crate exposes a typed library (uniffi →
  Swift bindings) so a native app embeds the runtime directly, no subprocess. See
  [`crates/trx64-ffi/API.md`](crates/trx64-ffi/API.md). A native macOS frontend is in
  progress on top of it.
- **`trx64-cli`** — a cross-platform terminal **cockpit + emulator window** that embeds
  the runtime in-process (Rust → Rust, no daemon/WS/FFI). Play in a native window, debug
  in a TUI, drive it with `/`-commands + the full monitor. See
  [`crates/trx64-cli/README.md`](crates/trx64-cli/README.md).

---

## The API

Everything is **JSON-RPC 2.0**. One request:

```json
{ "jsonrpc": "2.0", "id": 1, "method": "session/create", "params": { "pal": true } }
```

There is **one machine per process**, shared by every connected client. A typical
flow: `session/create` → `debug/run` → inspect via `monitor/exec` / `trace/*` /
`vic/inspect` → `checkpoint/*` to scrub → `snapshot/dump` to persist.

The surface, by area:

- **Session & run** — create/state/reset/screenshot; run, pause, step, run an exact
  cycle budget; pacing (`realtime` or `warp` 8× fast-forward); execution breakpoints.
- **Input** — keys, typed text, joystick, and load/autostart a `.prg`.
- **Media** — mount/swap/unmount disks (`.d64`/`.g64`) and cartridges (`.crt`)
  through one checkpointing ingress (cartridge fidelity above).
- **Trace** — start a capture over chosen domains, index it, query it, or carve a
  trace for an exact cycle window straight out of the always-on ring.
- **Checkpoint / scrub & snapshots** — the ring-bound rewind (a 10 s
  scrub-filmstrip) plus full-machine snapshots to `.c64re`.
- **Reverse-debug** — `reverse_step`, `who_wrote`, `crash_triage`, and live CPU
  history (`chis`).
- **Recorder, branching & audio** — bounded data-stream recorder, snapshot-tree /
  overlay-run / branch promotion, scenarios, and audio export.

### The monitor

A single method — `monitor/exec` — drives a full interactive monitor (~128 verbs, a
VICE superset): run/step, bank-aware dump/disassemble/assemble, breakpoints and
observers, dynamic and flow disassembly, transfer/compare/hunt, interrupt-flow and
backtrace, tracing, memory-map / taint / swimlanes, and the reverse-debug verbs
above. **[MONITOR.md](MONITOR.md)** is the full command reference (tables per category
+ the bank lens, the reverse-debug ring, checkpoint ring, traces, and a walkthrough);
`help` prints the live verb list.

---

## Interchange formats

Two binary formats move a machine between instances — daemon ↔ embedded app ↔ the
parity oracle:

- **`.c64re`** — a full machine snapshot (CPU, RAM, VIC, CIA, SID, drive).
- **`.c64retrace`** — the binary trace log.

Both are written byte-faithfully to the C64RE contract. TRX64 also imports VICE
`.vsf` snapshots; it does not export a faithful `.vsf`.

---

## Architecture

Separation of concerns *is* the performance — the core stays monomorphized and
branch-free:

```
trx64-daemon   tokio · WS JSON-RPC 2.0 · binary frames · the stream loop
trx64-ffi      typed uniffi bindings for in-process embedding (e.g. Swift)
trx64-session  session lifecycle · run control · snapshot / rewind · warp
trx64-trace    TraceOp encoder → .c64retrace (the immovable format)
trx64-core     pure, deterministic, synchronous emulation · zero-cost Observer · Clone-able state
```

The 1541 drive runs a **separate** 6502 core (`crates/trx64-core/src/vice1541/` +
`drive_6510core.rs`), kept faithful under the Spec-612 port-fidelity doctrine.

---

## Faithfulness

TRX64's emulation cores are a **source-faithful port of VICE** — one C file maps to
one Rust file, one function to one function, names preserved — so behaviour can be
diffed against VICE cycle by cycle. That correctness discipline is what makes the
time-travel and reverse-debug features trustworthy: a rewound or reverse-stepped
machine is the *real* machine state, not an approximation.

---

## License & Credits

TRX64 is licensed under the **GNU General Public License v3.0 or later**
(`GPL-3.0-or-later`). See [LICENSE](LICENSE).

The emulation cores are a source-faithful port of
[VICE](https://vice-emu.sourceforge.io/) (GPL-2.0-or-later; TRX64 uses the "or later"
permission). Full credits — VICE, C64RE, the scene, and ROM/media notices — are in
[THANKS.md](THANKS.md).

> At the request of Count Zero on behalf of the CSDb staff, any CSDb association has
> been removed. For TRX64 or C64RE, please reach out via GitHub.
