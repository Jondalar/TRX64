# Spec 864 — The monitor as a library: one implementation, two machines

**Status:** PROPOSED (2026-09-20)
**Repos:** TRX64 (the crate, the trait, the daemon as its first host). C64RE: **no
change** — `runtime_monitor` keeps talking to `monitor/exec`, and §9 shows why its wire
contract is untouched.
**Number:** 864 (registry: `../../C64ReverseEngineeringMCP/specs/README.md`).
**Depends on:** Spec 754 (the monitor itself), 804 (the marked address spans), 808 (the
transport truncation this spec turns into a host decision), 863 (the machine identity a
reply carries).
**Origin:** the UE2 emulator (`Jondalar/UE2-C64U-Emulator`), 2026-09-20. UE2 runs the
unmodified U64-II firmware, and the C64 behind the firmware's cartridge and DMA registers
**is trx64-core**, through their bridge, pinned at `4ab20e5` (v0.7.3). Their API can show
a screen, send keys and save a cartridge; there is no way to look at the 6510 — no
registers, no memory, no disassembly, no breakpoints. Their GDB stub is the RISC-V
firmware CPU, not the C64.

They looked at using ours and found two dead ends, both real: `trx64-ffi`'s
`monitor_exec` (ffi/src/lib.rs:370) is an RPC **client**, and the daemon's library face
(`build_state`/`dispatch`, daemon/src/lib.rs) builds its own `Session` and therefore its
own `Machine` — theirs lives inside the bridge, wrapped by the firmware's registers.
Re-implementing the verbs on their side would drift from ours within a release. So: one
implementation, two hosts.

---

## §1 What the monitor actually is today

`run_monitor_marked` is `main.rs:3736-7490` — about 3750 lines — plus the helpers it
closes over. It reads a narrow slice of the daemon's `State`: the session, `mon`,
`breakpoints`, `flow`, `observers` + `dsl_observers`, `checkpoint_ring`, `trap_rules`,
`transport`. The other sixty-odd `State` fields — streaming, recorder, candidates, media
events, the audio thread, pacing, batches — barely appear. That is the shape of a library
that has never been extracted, not the shape of something entangled with a daemon.

Three facts decide the design below, and each is checkable in the tree:

- **`impl CoreObserver for ObserverRegistry`** (observers.rs:965). The registry *is* a
  `trx64-core` observer — condition AST, hit/ignore counters, pending-effect queues — and
  it depends on nothing above core.
- **`sync_observers` runs before every advance**, at exactly two call sites: the one-shot
  `run_debug_control` (main.rs:2606) and the per-frame stream driver
  `stream_debug_gated_advance` (main.rs:2903). It rebuilds the registry from the
  breakpoint surfaces while preserving live counts, because a rebuild would otherwise wipe
  the DSL observers.
- **The registry's effects are drained by the host** (main.rs:2509-2566):
  `drain_pending_log`, `drain_pending_marks`, `drain_pending_cmds`,
  `drain_pending_trace`. Each means something only a host can mean — a mark is a timeline
  anchor, a cmd re-enters the monitor, a trace goes to a sink.

So the policy is portable and the cadence is not. That is the seam.

## §2 D1 — The crate

`crates/trx64-monitor`, depending on **`trx64-core` and `trx64-static` only**. What moves,
whole:

| from | what |
|---|---|
| `main.rs` verb dispatch + formatting | every verb, its output shape, `help` |
| `main.rs:675` `MonitorState` | cursors, device selection, `fs_cwd`, assemble mode, pending prompt |
| `main.rs:183` `Breakpoints` | the breakpoint/watchpoint surfaces the verbs edit |
| `main.rs:788` `FlowTracker` | the interrupt/trap frame tracker behind `flow`/`focus` |
| `main.rs` trap rules | `TrapRule` and its map |
| `observers.rs` (all 1333 lines) | `ObsSpec`, the condition AST and parser, `CondEnv`, `Observer`, `ObserverRegistry` |
| `assembler.rs` (342 lines) | the one-line 6502 assembler behind `a` — it already depends only on `trx64_core::tables` |
| `addr_spans.rs` | the marked-span machinery (§8) |

Nothing in that list reaches above core today, which is why this is an extraction and not
a redesign. The daemon keeps: the WS-RPC transport, traceindex, media, streaming,
recorder, pacing, candidates, the checkpoint ring, and its own host impl.

## §3 D2 — The host trait

```rust
pub trait MonitorHost {
    /// The C64. The one method with no default.
    fn machine(&mut self) -> &mut Machine;

    /// A device's CPU view. `machine()` is the shorthand for `Device::C64`.
    /// A host answers what it has; the lib prints "not available on this host"
    /// for the rest, in the same sentence a missing timeline verb gets.
    fn cpu(&mut self, dev: Device) -> Option<&mut dyn CpuView> { … }

    /// What a verb did, AFTER it ran. Default: nothing.
    fn on_effect(&mut self, effect: Effect) {}

    /// Replace the machine. The DEFAULT does the machine-level reset; a host that
    /// implements it is the only thing that runs (§5).
    fn reset(&mut self, kind: ResetKind) -> Result<String, String> { … }

    // Defaulted services, each answering "not available in this host":
    // timeline  — rewind, goto, frame, mark, rstep, the checkpoint ring
    // media/fs  — mount, ls, load, save, bload/bsave, cart, disk
    // traces    — tracedb, traceindex, swimlane, taint, whowrote
    // identity  — which model, which clock (§8)
}
```

**The arming handshake.** The lib owns the registry; the host borrows it for the length of
one advance:

```rust
let armed = mon.arm();            // syncs from the bp surfaces, preserving live counts
// host advances its machine with `armed` installed as a core observer
mon.after_advance(stop_info);     // counts, halt info, and the effect queues come back
```

The host then drains what it can act on. The lib does not advance the machine and never
will: who runs the CPU is the one thing the two hosts genuinely disagree about — the
daemon streams frames, UE2's firmware drives.

**Watch tables merge, they do not stack.** `Machine::run_for_full_capped_dbg`
(core lib.rs:2934-2942) takes ONE `exec_watch` and ONE `access_watch`, each
`&[u8; 0x10000]`. Two callers cannot each pass one. So `arm()` hands the host the lib's
tables and the host ORs its own in. This is written into the trait, not left to be
discovered — UE2 already runs its own access-watch table (they stop a run mid-catch-up
after a C64 write to the UCI control register).

## §4 D3 — Effect is classified in the lib

Spec 808's rule — an intervention while rewound truncates the future — is daemon policy
that today sits inside the monitor (main.rs:3740-3758), keyed on a verb string with a
special case: `r` only writes when the command contains `=`. That special case is the
proof the classification belongs to the parser. So:

```rust
enum Effect { Reads, Mutates, ReplacesMachine }
```

The lib classifies each command and calls `on_effect`. The daemon truncates on `Mutates`
and discards the timeline on `ReplacesMachine`; UE2 does nothing for either. No host ever
sees a verb string, so no host can hold a stale verb list.

## §5 D4 — Reset is intercepted, not announced

The lib never calls `Machine::warm_reset` (core lib.rs:1237) itself. `reset()` on the
trait has a default that does the machine-level reset, and a host that implements it
replaces that entirely. UE2 must: their firmware owns `C64_STOP` and the reset line and
restores the cartridge afterwards, so a lib-side reset followed by a notification would
leave the firmware describing a machine that no longer exists. The daemon's
implementation resets and then discards its timeline, keeping today's ordering rather than
turning it into an effect fired at the wrong moment.

## §6 D5 — Host verbs register into the dispatch

The lib sees every line first. It has to: `MonitorState` carries `asm_cursor` and
`pending_prompt`, so the monitor is **modal**, and anything sitting in front of it would
have to know when `a` swallows the next line and when an empty line exits a mode. That is
lib knowledge, and a second copy of it is how two hosts start behaving differently.

```rust
fn register(&mut self, verb: &str, aliases: &[&str], help: &str, effect: Effect, handler: …);
```

- A name or alias the lib owns is **refused at construction**, and so is one a previously
  registered host verb took. A collision is an error, never a silent last-wins.
- A host verb declares its `Effect`; the default is `Reads`.
- Host verbs are **non-modal in v1**. Opening a mode means owning `asm_cursor` and
  `pending_prompt`; one owner of the prompt, or none.
- They appear in `help` in their own section, and the port audit walks them like ours.

## §7 D6 — `TeeObserver` moves to `trx64-core`

It already exists, one crate too low: `main.rs:3072`, generic over two `Observer`s,
forwarding every callback to both, used at `:3185` to run the armed registry and the trace
sink over a single advance. It is a pure combinator over core's own trait, it is the
mechanism a second host needs to chain its observer with the lib's registry, and it has no
business being private to the daemon.

## §8 D7 — What a reply carries

- **Marked address spans (Spec 804) stay the library's API.** `run_monitor_marked` returns
  text with its addresses marked and `run_monitor` is the `addr_spans::plain()` wrapper
  (main.rs:3727). C64RE joins symbol names onto those spans — that is how `d 1000` comes
  back with labels. Each host decides whether to strip; nobody strips before the wire.
- **Machine identity (Spec 863).** A reply says which C64 it is on, from
  `Machine::timing()` via `machine()`. A host answers truthfully rather than assuming PAL:
  UE2 reports its U64 profile, the PAL-only core behind a firmware that may ask for 60 Hz,
  and the CPU clock turbo currently gives. The monitor states what the machine IS, not
  what a profile wishes.

## §9 D8 — The daemon is the first host, and nothing above it moves

`monitor/exec` becomes a thin call into the lib against the daemon's host impl.
`trx64-ffi` is unchanged — it is an RPC client and stays one. The WS wire contract,
including the marked spans and the identity fields, is byte-for-byte what it is today,
which is why C64RE needs no change: `runtime_monitor` cannot tell the difference, and
that is the acceptance (§11.1).

## §10 Scope

- **Not in this spec:** a second instruction set in `trx64-static`. UE2 wants `device fw`
  (the RISC-V running the firmware) and has no RV32 disassembler; `r` and `m` answer
  through `CpuView`, `d` says why it cannot. TRX64 is the C64 runtime — a second ISA in
  `trx64-static` is the accretion the capability cut was drawn to prevent, and its cost
  would land on every consumer of that crate forever. A host implements its own view; if
  a second ISA ever earns a home it gets its own crate.
- **Not in this spec:** modal host verbs (§6), dynamic re-registration, and any change to
  what a verb prints. This is an extraction. A behaviour change hiding inside it would be
  invisible to every gate below.
- **UE2's own verbs** (`fw`, `itu`, `cart`, `flash`, `sd`, `usb`, `net`, `audio`, `clock`)
  are theirs to write against this trait, not mine. Their omissions are as deliberate as
  their additions: `reu`, `uci` and `turbo` stay MY verbs, because in UE2 those devices are
  the real ones and a second opinion beside them would be a lie waiting to happen.
- Drive A in UE2 **is** `Machine::drive8` (their `drive.rs` swaps the real drive in and
  out of it), so `device drive8` behaves identically on both hosts. `fw` is the only
  genuinely new device.

## §11 Acceptance

1. **The daemon's monitor answers exactly as it does today.** A golden transcript —
   every verb in `help`, run against a booted machine, before and after the extraction —
   compares byte-for-byte, marked spans included. This is the gate that makes the other
   ten safe.
2. **The help text, `MONITOR.md` and the crate README still agree.** `docs_agree.rs`
   (trx64-cli) pins that today; it moves to the lib and gains the host-verb sections, so
   a host that registers a verb without documenting it fails its own build.
3. **The port audit walks lib verbs AND host verbs.** Every verb in `help` answers
   something on the host it is registered with; a defaulted service answers the standard
   "not available on this host" sentence rather than an error or a panic.
4. **A second host exists in the test suite.** A minimal in-test host implementing only
   `machine()` proves the defaults: the C64 verbs work, every timeline/media/trace verb
   answers the standard sentence, and nothing panics.
5. **Reset interception.** The in-test host overrides `reset()` to record and refuse; the
   machine is proven unchanged afterwards. The daemon's own override still discards the
   timeline.
6. **Effects.** The in-test host records `on_effect`; `wr`, `a`, `f`, `c`, `t`, `g`, `x`,
   `step`, `n` and `r $xx=` classify as `Mutates`, `r` alone and every read verb as
   `Reads`, `reset` as `ReplacesMachine`. The daemon's truncation numbers are unchanged
   against today's transport tests.
7. **Composition.** A test chains the armed registry with a second observer through the
   core `TeeObserver` and proves both see every callback, and that merged watch tables
   trip both sides.
8. **No dependency inversion.** `trx64-monitor` compiles against `trx64-core` and
   `trx64-static` alone — enforced by the crate graph, not by intention.
9. The existing gates stay green: the daemon suite, the core suites, the 7-game
   screenshots byte-identical.
10. **UE2 runs the port audit over its bridge host** in its own `cargo test`. Drift shows
    up on their side, in their CI, without me watching.

## §12 Open

- The `CpuView` trait's exact shape (registers are a fixed set for the 6502; the RISC-V
  has 32 plus CSRs). Probably: the lib formats what the view hands it, and the view
  decides its own register list.
- Whether `fs_cwd` and the FS verbs belong in the lib at all, or are entirely a media
  service. They are lib state today because the daemon is the only host.
