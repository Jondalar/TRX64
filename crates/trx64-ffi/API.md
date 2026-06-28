# TRX64 FFI — Typed Swift API

The `trx64-ffi` crate is the **typed uniffi façade** that lets a native Swift app
(TRX64-App) embed the TRX64 runtime **in-process** — no daemon subprocess, no
WebSocket. Every typed method is a thin wrapper over the SAME `dispatch()` the
WebSocket daemon uses, so the typed Swift API **cannot drift** from the wire
contract (the typed-binding definition is versioned here, in TRX64).

- **Object**: `Runtime` (one machine per process, shared — Single-Path / One-Machine
  contract).
- **Errors**: every typed method `throws` a `Trx64Error` (no JSON error blobs).
- **Events**: a typed `RuntimeEvent` stream via the `EventListener` callback.
- **Escape hatch**: `call(method, paramsJson) -> String` covers every method not in
  the typed surface → 100 % coverage.

Swift names are uniffi's camelCase rendering of the Rust snake_case below.

---

## Construction

| Method | Signature | Purpose |
|--------|-----------|---------|
| `Runtime(romDir:)` | `init(romDir: String) throws -> Runtime` | Boot a fresh singleton machine from ROMs in `romDir`; cold-reset to the reset vector, paused. |

---

## session

| Method | Signature | Purpose |
|--------|-----------|---------|
| `createSession` | `(pal: Bool) throws -> SessionInfo` | Attach to the singleton session (the machine is built at construction; `create` always attaches). |
| `state` | `() throws -> MachineState` | Full machine state: CPU regs, cycles, run-state, VIC, flow, vectors, SID. |
| `reset` | `(cold: Bool) throws -> ResetResult` | `cold` = power-cycle (fresh DRAM); else warm (RAM preserved). Runs the KERNAL to READY. |
| `screenshot` | `() throws -> Data` | PNG bytes of the current displayed frame (decoded from the handler's data URL). |

## Live A/V (pull)

The native app renders video at ~50 Hz and feeds audio to AVAudioEngine by **pulling**
A/V from the runtime in-process — at its OWN cadence (per video frame, per audio
callback). A/V is **binary** and deliberately bypasses the JSON-RPC `dispatch` + event
channel (JSON cannot carry a frame / PCM efficiently), so these methods reach the core
**directly** (the same `SharedState` lock every handler uses); they are **additive** and
do not touch `dispatch` or any existing method. `Vec<u8>` / `Vec<i16>` map to Swift
`Data` / `[Int16]`, so there is no base64 and no JSON on the hot path.

| Method | Signature | Purpose |
|--------|-----------|---------|
| `frameBuffer` | `() -> FrameBuffer` | The CURRENT displayed frame at FULL resolution as a palette + index image — the 384×272 VICE PAL canvas (the same `displayed` buffer `screenshot()` and the scrub thumbnails come from, here full-res + un-palettized). Pull once per video frame and blit. Non-throwing (a pure read). |
| `audioDrain` | `() -> [Int16]` | Drain + return the SID PCM accumulated since the last `audioDrain()` — **mono `Int16`** at `audioSampleRate()` (44100 Hz). Draining EMPTIES the buffer, so repeated calls don't re-deliver. Pull in the AVAudioEngine source callback. The FIRST call installs the SID capture hook + spawns the audio render thread (which constructs reSID once + primes it) and returns empty (no cycles elapsed yet); thereafter each call returns the samples for exactly the cycles run since the previous drain. Rendering is a **continuous persistent-engine render** off a PCM ring (mirrors the `--stream` loop / C64RE Spec 768): the render thread holds ONE reSID engine for the whole session, fed by a SID write-ring (emu→render); `audioDrain()` just pops the PCM ring — no per-pull engine reconstruct, so the stream is continuous across drain boundaries (no clicks, no hum). Non-throwing. |
| `audioSampleRate` | `() -> UInt32` | The runtime's fixed SID sample rate (Hz) — **44100**. Fetch once when configuring the AVAudioEngine format. Every `audioDrain()` sample is mono at this rate. Non-throwing. |

**Audio format**: mono, signed 16-bit PCM (`Int16`), 44100 Hz (reSID, single SID). To
feed AVAudioEngine, fill an `AVAudioPCMBuffer`'s `int16ChannelData` with the returned
`[Int16]` (or duplicate L=R for a stereo node).

**Frame format**: see `FrameBuffer` below. To draw, for each of `width*height` pixels,
`i = indices[p]` (0..15) selects RGB `palette[i*3 ..< i*3+3]`.

## run / step

| Method | Signature | Purpose |
|--------|-----------|---------|
| `run` | `() throws -> DebugState` | Flip run-state to running. |
| `pause` | `() throws -> DebugState` | Pause (`stop.reason = "pause"`). |
| `step` | `() throws -> MachineState` | Single-step one instruction; returns the new full state. |
| `runCycles` | `(n: UInt64) throws -> RunResult` | Advance exactly `n` C64 cycles (may stop early on a breakpoint). |
| `setPacing` | `(pacing: Pacing) throws -> DebugState` | Set pacing mode (`pal`/`warp`/`fixed-ratio`) + ratio. |

## input

| Method | Signature | Purpose |
|--------|-----------|---------|
| `keyDown` | `(key: String) throws` | Press a key (c64re key id, e.g. `"A"`, `"RETURN"`, `"RUN_STOP"`, `"L_SHIFT"`). |
| `keyUp` | `(key: String) throws` | Release a key. |
| `typeText` | `(text: String) throws -> TypeResult` | Type a PETSCII string through the keyboard matrix. |
| `joystick` | `(port: UInt8, state: JoystickState) throws` | Set a port's joystick (all-false → release). |
| `loadPrg` | `(bytes: Data) throws -> LoadResult` | Load a PRG into RAM (honours its 2-byte load-address header). Does not run. |
| `runPrg` | `(bytes: Data) throws -> RunPrgResult` | Load + autostart (BASIC `RUN` for $0801, else JMP to load address). |

## monitor

| Method | Signature | Purpose |
|--------|-----------|---------|
| `monitorExec` | `(command: String) throws -> String` | Run one monitor-REPL command (`d`, `m`, `r`, `g`, `bk`, `obs`, `flow`, …); returns the output text. |

## media

| Method | Signature | Purpose |
|--------|-----------|---------|
| `mount` | `(path: String, slot: UInt8) throws -> MediaResult` | Mount a disk (.d64/.g64) or cartridge (.crt) from a host path. |
| `swap` | `(path: String) throws -> MediaResult` | Swap the mounted disk (drive stays attached). |
| `unmount` | `(slot: UInt8) throws -> UnmountResult` | Unmount the drive (slot 8). |
| `recentMedia` | `() throws -> [MediaEntry]` | Recent-media list (newest-first, mount timestamps). |
| `cartStatus` | `() throws -> CartStatus?` | Attached cartridge status, or `nil` when no cart. |

## trace

| Method | Signature | Purpose |
|--------|-----------|---------|
| `traceStart` | `(domains: [String]) throws -> TraceRun` | Start a capture-all trace over the given domains. |
| `traceStop` | `() throws -> TraceStatus` | Stop the active trace; returns the finalized run status. |
| `traceIndex` | `(path: String?) throws -> IndexResult` | Index a `.c64retrace` into DuckDB (`nil` = last finalized). |
| `buildTraceFromRing` | `(start: UInt64, end: UInt64) throws -> TraceFile` | Build a `.c64retrace` (+ DuckDB) from a ring cycle-window. |

## checkpoint / scrub

| Method | Signature | Purpose |
|--------|-----------|---------|
| `checkpointCapture` | `() throws -> Checkpoint` | Capture a checkpoint into the in-memory ring. |
| `checkpointRestore` | `(id: String, then: String, render: Bool) throws` | Restore a checkpoint; `then` = `pause`/`run`/`keep`; `render` re-presents the frame. |
| `checkpointList` | `() throws -> [Checkpoint]` | List the ring's checkpoints. |
| `thumbnails` | `() throws -> [Thumbnail]` | Scrub-filmstrip thumbnails (palette + indices) per checkpoint. |
| `diffCheckpoints` | `(idA: String, idB: String) throws -> SnapshotDiff` | Typed, by-ID diff of two ring anchors (RAM runs + per-chip register changes). **Read-only** — the live machine is byte-identical afterwards. (WS: `runtime/diff_checkpoints {idA,idB}`; monitor: `diff <idA> <idB>`.) |

## reverse-debug

| Method | Signature | Purpose |
|--------|-----------|---------|
| `reverseStep` | `(n: UInt64) throws -> ReverseResult` | Inspect-step backward `n` instructions over the reverse history. |
| `whoWrote` | `(addr: UInt16, limit: UInt32) throws -> [Writer]` | Recent writers of `addr` (PC + cycle + old/new), most recent first. |
| `crashTriage` | `() throws -> TriageChain` | Walk the crash cause chain from the live PC. |
| `setReverseDepth` | `(seconds: UInt64) throws -> ReverseDepth` | Set the reverse-history depth in seconds (resizes the buffers). |

## snapshot

| Method | Signature | Purpose |
|--------|-----------|---------|
| `dump` | `(path: String) throws -> SnapshotInfo` | Dump a full machine snapshot (`.c64re`) to `path`. |
| `undump` | `(path: String) throws -> SnapshotInfo` | Restore a machine snapshot from `path`. |

## events

| Method | Signature | Purpose |
|--------|-----------|---------|
| `setListener` | `(listener: EventListener)` | Register a typed event listener (replaces any existing). |
| `clearListener` | `()` | Detach the listener (stops the forwarder thread). |

## escape hatch

| Method | Signature | Purpose |
|--------|-----------|---------|
| `call` | `(method: String, paramsJson: String) -> String` | Raw JSON-RPC: any method + params-object string → full JSON-RPC response string. Errors are returned in the JSON, not thrown. |

---

## Result structs (uniffi records)

### SessionInfo
`sessionId: String`, `mode: String`, `diskPath: String`, `attached: Bool`,
`c64Cycles: UInt64`, `pc: UInt32`, `trace: TraceRun?`

### MachineState
`c64Cycles: UInt64`, `driveCycles: UInt64`, `mode: String`,
`runState: String` ("running"|"paused"), `cpu: CpuState`, `vic: VicState`,
`flow: FlowState`, `vectors: Vectors`, `sid: SidState`, `stopReason: String?`

- **CpuState** — `pc, a, x, y, sp, flags: UInt32`, `cycles: UInt64`
- **VicState** — `rasterLine, rasterCycle, mode, bank, screenPtr, chargenPtr, bitmapPtr, border, background: UInt32`
- **FlowState** — `focus: String`, `current: String`
- **Vectors** — `irq, nmi, cinv, cbinv: UInt32`
- **SidState** — `regs: [UInt32]`, `streaming: Bool`

### ResetResult
`c64Cycles: UInt64`, `pc: UInt32`, `mode: String` ("cold"|"soft")

### Pacing (input)
`mode: String` ("pal"|"warp"|"fixed-ratio"), `ratio: Double`

### DebugState
`runState: String`, `pacing: PacingState`, `pc: UInt32`, `cycles: UInt64`,
`frame: UInt64`, `breakpoints: [BreakpointInfo]`, `stop: StopInfo?`,
`controlOwner: String`

- **PacingState** — `mode: String`, `ratio: Double`
- **BreakpointInfo** — `num: UInt32`, `addr: UInt32`
- **StopInfo** — `reason: String`, `pc: UInt32`, `cycles: UInt64`

### RunResult
`c64Cycles: UInt64`, `breakpoint: RunBreakpoint?`
- **RunBreakpoint** — `pc: UInt32`, `num: UInt32`

### TypeResult
`c64Cycles: UInt64`, `queued: UInt64`

### JoystickState (input)
`up, down, left, right, fire: Bool`

### LoadResult
`loadAddress: UInt32`, `endAddress: UInt32`, `bytesLoaded: UInt64`, `path: String`

### RunPrgResult
`loadAddress: UInt32`, `action: String`

### MediaResult
`mountedPath: String`, `type: String` ("d64"|"g64"|"crt"), `sha256: String`,
`paused: Bool`, `slot: UInt32?` (disk), `mapperType: String?` (cart)

### UnmountResult
`ok: Bool`, `paused: Bool`, `wasRunning: Bool`

### MediaEntry
`path: String`, `name: String`, `type: String`, `mountedAt: String?`

### CartStatus
`type: String`, `bank: UInt32`, `activity: String` ("write"|"read"|"idle"),
`booted: Bool`, `sourceName: String?`

### TraceRun
`runId: String`, `definitionId: String`, `definitionVersion: Int64`,
`cycleStart: UInt64`, `eventCount: UInt64`, `bytesWritten: UInt64?`,
`media: TraceMedia?`
- **TraceMedia** — `sha256: String`, `sourceName: String`

### TraceStatus
`run: TraceRun`, `status: String`, `index: IndexResult?`

### IndexResult
`duckdbPath: String`, `eventsIndexed: UInt64`, `bounded: Bool`,
`boundedFrom: UInt64?`, `cap: UInt64?`, `indexedFromOldest: Bool`

### TraceFile
`retracePath: String`, `duckdbPath: String`, `eventsEncoded: UInt64`

### Checkpoint
`id: String`, `frame: UInt64`, `cycles: UInt64`, `pinned: Bool`

### Thumbnail
`id: String`, `cycles: UInt64`, `frame: UInt64`, `pinned: Bool`,
`width: UInt32`, `height: UInt32`, `palette: String` (b64 RGB),
`indices: String` (b64 indices)

### FrameBuffer (live A/V pull)
`width: UInt32` (384), `height: UInt32` (272), `palette: Data` (RGB, 16×3 = 48 bytes),
`indices: Data` (`width*height` bytes, each 0..15 indexing `palette`).
Full-resolution counterpart of `Thumbnail` — raw `Data`, NOT base64 (in-process pull,
no JSON). `i = indices[p]` → RGB `palette[i*3 ..< i*3+3]`.

### ReverseResult
`stepsTaken: UInt64`, `pc, a, x, y, sp, p: UInt32`, `cycle: UInt64`,
`undoneWrites: [UndoneWrite]`, `inspectOnly: Bool`, `note: String`
- **UndoneWrite** — `addr, old, new: UInt32`

### Writer
`pc: UInt32`, `cycle: UInt64`, `addr, old, new: UInt32`

### TriageChain
`lines: [String]`

### ReverseDepth
`seconds: UInt64`, `deltaEntryCapacity: UInt64`, `deltaWriteCapacity: UInt64`,
`cpuHistoryCapacity: UInt64`, `estimatedRamMb: Double`,
`discardedHistory: Bool`, `note: String`, `warning: String?`

### SnapshotInfo
`path: String`, `cycle: UInt64`, `pc: UInt32`, `machine: String`,
`media: [SnapshotMedia]`, `breakpoints: UInt64`, `fileBytes: UInt64?` (dump only)
- **SnapshotMedia** — `role: String`, `format: String`, `sourceName: String`, `sha256: String`, `bytes: UInt64`

### SnapshotDiff
`cycleA, cycleB: UInt64`, `ram: [RamRun]`, `cpu, vic, cia, sid, drive: [RegChange]`
- **RamRun** — `start: UInt32`, `byteCount: UInt32`, `old: Data`, `new: Data` (one contiguous run of changed RAM; `old`/`new` are the run's bytes before/after — NOT a 64 K byte list)
- **RegChange** — `name: String`, `old, new: UInt32` (CPU names: `pc`/`a`/`x`/`y`/`sp`/`flags`; chips: `$NN`; CIA tagged `cia1.$NN`/`cia2.$NN`; drive tagged `cpu.pc`/`via1.$NN`/`headHalfTrack`/…)
- A chip's list is empty when unchanged; `drive` is empty unless **both** anchors carried a 1541 DRIVECPU.

---

## RuntimeEvent (enum)

Delivered to `EventListener.onEvent(event:)`. Every known `NotifyHub` broadcast
maps to a typed variant; anything else (and future events) falls through to
`other` with the raw method + params JSON, so nothing is dropped.

| Variant | Associated values | Source broadcast |
|---------|-------------------|------------------|
| `frameAvailable` | `sessionId: String, frame: UInt64, c64Cycles: UInt64` | `session/frame_available` |
| `running` | `sessionId: String` | `debug/running` |
| `paused` | `sessionId: String, reason: String, pc: UInt32, cycles: UInt64` | `debug/paused` |
| `stopped` | `sessionId: String, reason: String, pc: UInt32, cycles: UInt64` | `debug/stopped` |
| `breakpointHit` | `sessionId: String, pc: UInt32, num: UInt32` | `debug/breakpoint_hit` |
| `observerHit` | `sessionId: String, name: String` | `debug/observer_hit` |
| `observerLog` | `sessionId: String, message: String` | `debug/observer_log` |
| `checkpointRestored` | `sessionId: String, id: String` | `debug/checkpoint_restored` |
| `controlChanged` | `sessionId: String, controlOwner: String` | `debug/control` |
| `audioFlush` | `sessionId: String` | `audio/flush` |
| `mediaChanged` | `sessionId: String` | `media/cart_persisted` |
| `batchProgress` | `paramsJson: String` | `batch/progress` |
| `other` | `method: String, paramsJson: String` | (any other / future) |

### EventListener (callback interface)
```swift
protocol EventListener: AnyObject {
    func onEvent(event: RuntimeEvent)
}
```

How it's wired: `setListener` subscribes a channel to the daemon's single
`NotifyHub` (the same hub the WebSocket transport fans notifications through). A
dedicated forwarder thread block-drains the channel, parses each JSON-RPC
notification envelope, maps it to a typed `RuntimeEvent`, and calls `onEvent`. The
subscription + thread are owned by the `Runtime`; `clearListener` (or dropping the
`Runtime`) stops and joins the thread, so the Swift callback always outlives every
`onEvent` call.

---

## Trx64Error (error enum)

Thrown by every typed method.

| Variant | Fields | Meaning |
|---------|--------|---------|
| `boot` | `message: String` | Runtime could not be constructed (e.g. ROMs not found). |
| `dispatch` | `code: Int64, message: String` | A JSON-RPC handler returned an error. |
| `decode` | `message: String` | The handler's JSON did not match the typed shape (façade bug / contract change); includes the raw JSON. |
| `invalidArgument` | `message: String` | Bad caller argument (e.g. un-decodable base64). |

---

## Coverage note

The typed surface covers the App-UI workflows (session / run / input / monitor /
media / trace / checkpoint / reverse-debug / snapshot / events). The full TRX64
JSON-RPC surface is far larger; everything not typed above is reachable verbatim
through `call(method:paramsJson:)`, which returns the raw JSON-RPC response string.
Because both paths funnel through the one `dispatch()`, the typed methods and the
escape hatch can never disagree with the WebSocket daemon.
