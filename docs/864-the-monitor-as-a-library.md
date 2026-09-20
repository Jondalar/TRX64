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
| `main.rs:675` `MonitorState` | cursors, device selection, assemble mode, pending prompt — **not** `fs_cwd` (§10.1) |
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

    /// A device's CPU view (§3.1). `machine()` is the shorthand for `Device::C64`.
    /// A host answers what it has; the lib prints "not available on this host"
    /// for the rest, in the same sentence a missing timeline verb gets.
    fn cpu(&mut self, dev: Device) -> Option<&mut dyn CpuView> { … }

    /// Resume, step, halt. The DEFAULTS drive the machine, which is what the
    /// daemon wants; a host whose machine is driven by something else overrides
    /// them (§5.1). The lib never advances a machine behind a host's back.
    fn resume(&mut self, until: RunUntil) -> Result<Resumption, String> { … }
    fn step(&mut self, n: u64, over: bool) -> Result<StopInfo, String> { … }
    fn set_halted(&mut self, halted: bool) -> Result<(), String> { … }

    /// A stop that arrived outside a command (§5.1). Default: nothing — the
    /// daemon broadcasts it, UE2 answers its next `status` with it.
    fn on_stop(&mut self, stop: &StopInfo) {}

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

### §3.1 `CpuView` — the register list AND the address width

UE2's answer to §12, and it is the half I had underweighted: the register list is easy,
the address space is not. Their firmware view is 32-bit — devices at `0x1004_0000` (where
the UCI window lives), cartridge ROM at `0x03C0_0000`, config pages at `0xFE_8000`. A `u16`
anywhere in the shared path and `device fw` can address none of it.

```rust
pub trait CpuView {
    fn addr_bits(&self) -> u8;                       // 16 for the 6502, 32 for the RISC-V
    fn read(&mut self, addr: u64) -> Option<u8>;
    fn write(&mut self, addr: u64, v: u8) -> Result<(), String>;
    fn registers(&self) -> Vec<Reg>;                 // name, width in bits, value
    fn set_register(&mut self, name: &str, v: u64) -> Result<(), String>;
    fn flags(&self) -> Option<FlagSpec>;             // None: this CPU has no flag register
    fn disasm(&mut self, addr: u64) -> Option<(u8, String)>;  // None: no decoder here
    fn banks(&self) -> &[&str];                      // empty: `bank` is not for this device
}
```

The lib parses and formats addresses against `addr_bits`, so `m` and `d` on a 32-bit view
neither truncate nor pad. Three consequences, each a refusal rather than a guess:

- **Flags are an option, not an assumption.** `p`/`fl` renders a flag string for a CPU that
  has one; the RISC-V does not, so the verb says so on that device.
- **`r <reg>=<v>` is refusable per view**, not globally — a view may expose a register it
  will not let you write.
- **The debug machinery is a capability of the C64 view.** The watch tables are
  `[u8; 0x10000]` (core lib.rs:2939-2940) — a 6502 shape, not a general one. So
  breakpoints, exec/access watches and the observer registry apply to the C64 device, and
  on another device the lib answers "not available on this device" rather than silently
  watching the wrong 64 KB. UE2 debugs its RISC-V through its GDB stub, which is the right
  tool for it.

## §4 D3 — Effect is classified in the lib

Spec 808's rule — an intervention while rewound truncates the future — is daemon policy
that today sits inside the monitor (main.rs:3740-3758), keyed on a verb string with a
special case: `r` only writes when the command contains `=`. That special case is the
proof the classification belongs to the parser. So:

```rust
enum MachineEffect { Observes, Mutates, Replaces }
```

The lib classifies each command and calls `on_effect` **before the verb runs**. That
ordering is a correction the build forced: this section first said "after", and the first
host showed why it cannot be. `g` classifies as `Mutates` and appends timeline anchors
*while it runs*, so a truncation fired afterwards would cut the anchors the command had
just made. Nothing in the golden transcript would have caught it — a timeline is not text.

The daemon truncates on `Mutates` and discards the timeline on `Replaces`; UE2 does
nothing for either.

**A write needs its lens, and that is a second call.** `on_machine_write(lens)` fires per
write, after it, naming the bank it went through. The write itself is the library's — it
is `Machine`'s own memory and both hosts have the same one — but the daemon's bus
selection latches `injected` and `io_injected` **separately**: an `io` write means the VIC
has to be ticking, a `ram` write does not. `on_effect` fires once per command and carries
no lens, so it cannot say this, and one flag for both would be the 2026-08-12
observers-wreck-everything bug arriving by a new road. No host ever sees a
verb string, so no host can hold a stale verb list.

**It is named for what it measures.** UE2's first host verb is `config` — it reads the
Ultimate's flash config pages and can hand the firmware a settings delta, so it changes a
great deal, and none of it is the C64. Under a name like `Reads` that declaration looks
like a lie; under `MachineEffect::Observes` it is exactly true. The question this enum
answers is "what did this do to the C64 and its timeline", not "did anything change
anywhere".

## §5 D4 — Reset is intercepted, not announced

The lib never calls `Machine::warm_reset` (core lib.rs:1237) itself. `reset()` on the
trait **refuses by default**, and this too is a correction: the section first gave it the
machine-level reset as its default, and the first host proved that default WRONG rather
than merely incomplete. The daemon's warm reset also clears the keyboard, runs five
million cycles, resets the flow stack and the cursors and marks the machine running; its
cold reset is a whole-host power cycle — media re-attach, audio epoch, transport reset.
Neither is expressible through `&mut Machine`, and a host that silently took the weaker
one would look reset without being reset. A refusal is a sentence the user can act on; a
half-reset is not. The library clears its OWN state (cursors, flow stack) after a
successful reset, so no host has to know those exist. UE2 must: their firmware owns `C64_STOP` and the reset line and
restores the cartridge afterwards, so a lib-side reset followed by a notification would
leave the firmware describing a machine that no longer exists. The daemon's
implementation resets and then discards its timeline, keeping today's ordering rather than
turning it into an effect fired at the wrong moment.

### §5.1 Resuming, stepping and halting are host business too

The same cut, one step further than I had drawn it. UE2's firmware is a second CPU with
its own clock: when a C64 breakpoint hits there, the firmware keeps running and will
notice a C64 that stopped answering. So "the machine halted" and "time stopped" are not
the same statement, and only the host knows which one it can make.

`g`, `until`, `step`/`z`/`n` therefore go through `resume`/`step`, and a halt is announced
through `set_halted`. UE2 overrides them and routes through the firmware's own `C64_STOP`
path, which keeps the firmware consistent with the machine it is hosting.

**Neither has a machine-driving default, and the reason is a shape this spec had wrong.**
The daemon's step is `step_one_with_flow`: it classifies the step — was an interrupt
dispatched, was it an RTI, which PCs did it go between — and pushes or pops a frame on the
`FlowTracker`. That tracker is the LIBRARY's state, inside `MonitorSession`, and a
`&mut self` host method cannot reach it. A default that drove the machine would therefore
have left the `flow` panel quietly wrong on every host that took it — the worst kind of
default, because it works.

So the host says what it stepped and the library keeps its own books: `StopInfo` carries
`steps: Vec<StepClass>`, one entry per retired instruction, and the library applies them
to the tracker after the call returns. A host that cannot tell returns an empty vector,
which is honest rather than wrong. The alternative — passing `&mut MonitorSession` into
the host method — would have made every host import the library's state to implement a
run loop, which is the coupling this whole spec exists to avoid.

**A resume may answer before the stop happens.** This is the deepest difference between
the two hosts and it shapes the reply, not just the call. The daemon drives the machine,
so `g` can block and answer "stopped at $XXXX". UE2 does not drive it — the firmware's
clock does, and the C64 catches up in batches inside somebody else's advance. So there:

```rust
enum Resumption { Stopped(StopInfo), Resumed { until: RunUntil } }
fn resume(&mut self, until: RunUntil) -> Result<Resumption, String>;
```

The daemon returns `Stopped` as it does today. UE2 returns `Resumed` at once and the
breakpoint fires in a later advance, possibly milliseconds of firmware time later, so the
lib must be able to print "running until …" — which is the real requirement on `RunUntil`:
it has to be re-statable as a line a human reads, not merely matchable.

**A stop may therefore arrive outside a command.** The host calls `after_advance` from
wherever the halt actually happened, and the lib holds a `StopInfo` nobody asked for. It
keeps it as the last stop in `MonitorState` and hands it to the host through `on_stop`,
and the host decides where it surfaces: the daemon broadcasts it on its `NotifyHub`
(streaming.rs:172), UE2 puts it on the control connection and answers the next `status`
with it. The lib neither blocks nor invents a channel of its own.

`step`, `z` and `n` keep the blocking shape on both hosts — one instruction with
`max_instructions = 1`, out of band; the firmware's clock does not move and the C64 simply
consumes a little of the lag it carries.

UE2 also notes the consequence that is theirs rather than this spec's: a halted C64 under
a running firmware is a state the real device never holds for long — the firmware polls
the C64, serves the UCI and drives the drives, and will time things out. Bounding that
halt is the UE2 host's business, and it is right that it is not the library's.

**One arm, one-to-many core runs.** UE2's `run_cpu` is a loop, not a call: cartridge hints
split a run, and since their UCI fix they deliberately halt mid-catch-up on a write to the
control register and then continue. So the contract is `arm()` … *1..n* core runs …
`after_advance()`, and re-arming between those runs is allowed: `sync_observers` preserves
live hit and ignore counts across a rebuild (it snapshots the prior registry), which is
the property that makes the loop safe. A contract of one core run per arm would have been
wrong for the host that asked for this spec.

## §6 D5 — Host verbs register into the dispatch

The lib sees every line first. It has to: `MonitorState` carries `asm_cursor` and
`pending_prompt`, so the monitor is **modal**, and anything sitting in front of it would
have to know when `a` swallows the next line and when an empty line exits a mode. That is
lib knowledge, and a second copy of it is how two hosts start behaving differently.

```rust
fn register(&mut self, verb: &str, aliases: &[&str], help: &str, effect: MachineEffect, handler: …);
```

- A name or alias the lib owns is **refused at construction**, and so is one a previously
  registered host verb took. A collision is an error, never a silent last-wins.
- A host verb declares its `MachineEffect`; the default is `Observes` (§4) — a verb that
  changes the host's own world but not the C64 declares exactly that.
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
- **Machine identity (Spec 863) is captured per REPLY, never per session.** A reply says
  which C64 it is on, from `Machine::timing()` via `machine()`. UE2's clock is whatever
  turbo currently is and moves at runtime (`C64_SPEED_UPDATE`), so an identity struct
  captured once at attach would be stale by the second command. A host answers truthfully
  rather than assuming PAL, and reports rather than hides what it cannot do: UE2's core is
  PAL-only, so a firmware asking for 60 Hz produces a warning in the reply, not a silent
  approximation. The monitor states what the machine IS, not what a profile wishes.

## §9 D8 — The daemon is the first host, and nothing above it moves

`monitor/exec` becomes a thin call into the lib against the daemon's host impl.
`trx64-ffi` is unchanged — it is an RPC client and stays one. The WS wire contract,
including the marked spans and the identity fields, is byte-for-byte what it is today,
which is why C64RE needs no change: `runtime_monitor` cannot tell the difference, and
that is the acceptance (§11.1).

### §9.1 As built, so far — and what is still in the daemon

The extraction is deliberately partial, and the line it stopped at is the line where
moving would have meant *inventing* a service rather than extracting one.

**In the library:** `observers.rs`, `assembler.rs`, `addr_spans.rs` whole; `MonitorState`,
`Breakpoints`, `FlowTracker` and the trap rules gathered into `MonitorSession`, which the
daemon's `State` now holds as one field instead of five; the arming handshake; and the
verbs that need nothing but a machine — `r wr m d screen f a t c h bank sidefx obs o
ignore bk del flow io iec focus bt triage help df whowrote revdepth reu georam uci`, plus
`device`/`dev` and the drive8 gate. The library sees **every** line first, so the modal
prompt and the assemble mode are its business on both hosts; it returns "not my verb" only
for what it does not own. `main.rs` went from 25 350 to 22 871 lines.

**Still in the daemon, each for the same reason:** run control (`g x until z n ret sf`),
`reset`/`power`, everything timeline (`play pause run mark marks frame goto rewind rstep
sd diff cdiff ringdump ringload`), everything trace (`trace tracedb map taint traceindex
swimlane chis`), everything media and file (`dump savecrt bitmap pwd cd ls load save
bload bsave traprules`), and `model turbo warp`. Each needs a service the trait declares
and nobody has implemented yet. The shapes of `Timeline`, `Files` and `Traces` in §3 are
therefore still guesses, and two gaps are already visible: `Timeline` has nothing for
"restore a checkpoint by id", which `cdiff` needs, and `Files` has no "write this file and
give me back its path", which `bitmap` needs.

**A constraint for any host author.** `try_exec(session, host, …)` needs the session and
the host mutably at the same time, so they must be **disjoint borrows**. The daemon does
it by destructuring its own `State` and handing the pieces to a `DaemonHost` of borrowed
fields. A host that owns its `MonitorSession` inside the same struct its `MonitorHost`
impl borrows will not compile — better said here than discovered after it is written.

**`CpuView` is declared but not yet on any path.** The moved verbs still reach both 6502s
through `machine()` and `machine().drive8`, exactly as they did in the daemon, so `r`/`m`
/`d` under `device drive8` do not consult it. Wiring them through the view is real
remaining work, and until it is done `device fw` cannot answer on any host.

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

### §10.1 The filesystem verbs, and where the cwd lives

UE2's answer to the second §12 question, and it is right: `fs_cwd` is not monitor state,
it is shell state of a filesystem. A host without a filesystem never has a cwd, so
carrying one it cannot use is carrying a lie. The cwd moves into the media/FS service with
its verbs, and the lib hides those verbs entirely when the service is absent instead of
printing a prompt about a directory that does not exist. `MonitorState` keeps what is
genuinely about looking at a machine: the memory and disassembly cursors, the selected
device, the assemble cursor, the side-effects flag, the pending prompt.

One cut inside that cut, which UE2 spotted and I had not: `load`, `save`, `bload` and
`bsave` are two halves glued together. The file half is the service; **the memory half is
the lib plus the machine**, and it takes the same path as `wr`. Drawn that way a host with
plain file access — UE2 is an ordinary host process — implements a handful of file calls
and gets the verbs. Drawn the other way the whole verb leaves, and every host writes the
memory half again, which is precisely the drift this spec exists to prevent.

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
   `Observes`, `reset` as `Replaces`. The host also records `on_machine_write` and sees
   the lens each write went through. The daemon's truncation numbers are unchanged
   against today's transport tests.
7. **Composition.** A test chains the armed registry with a second observer through the
   core `TeeObserver` and proves both see every callback, and that merged watch tables
   trip both sides.
8. **No dependency inversion.** `trx64-monitor` compiles against `trx64-core` and
   `trx64-static` alone — enforced by the crate graph, not by intention.
9. **A 32-bit device.** The in-test host registers a fake device with `addr_bits() = 32`,
   no flag register and no decoder. `m` and `d` address it without truncating, `p` says
   the CPU has no flags, `d` says there is no decoder, and a breakpoint or watch on that
   device is refused by name rather than applied to the C64's 64 KB.
10. **Run control through the host.** The in-test host counts `resume`/`step`/`set_halted`
    and refuses one of them; the lib never reaches the machine behind its back, proven by
    the machine's cycle count being unchanged after a refused resume. A host that arms
    once and performs three core runs before `after_advance()` keeps its hit and ignore
    counts across all three.
11. **An asynchronous stop.** The in-test host returns `Resumed` from `resume` and calls
    `after_advance` later, from outside any command: the lib prints "running until …" for
    the resume, holds the stop when it arrives, hands it to `on_stop`, and the next
    `status` states it. Nothing blocks and nothing is lost.
12. **Identity is per reply.** Two replies across a clock change report the two different
    clocks. (On the daemon: a model switch at a frame boundary, Spec 863.)
13. The existing gates stay green: the daemon suite, the core suites, the 7-game
    screenshots byte-identical.
14. **UE2 runs the port audit over its bridge host** in its own `cargo test`. Drift shows
    up on their side, in their CI, without me watching.

## §12 Open

Both original questions were answered by the host that asked for this spec, and their
answers are folded in above: `CpuView` carries its address width as well as its register
list (§3.1), and the filesystem verbs with their cwd are a service (§10.1).

What is still open:

- The `Reg` and `FlagSpec` shapes — how much the lib formats and how much the view hands
  it pre-rendered. The rule is "the view decides its register list"; the boundary between
  a value and its presentation still needs drawing.
- ~~a host-opaque resume token~~ — answered: no token. Everything needed to continue is
  already in the machine and the loop state is derived, so a token would only be a second
  place for two hosts to disagree. What was needed instead is a resume that may answer
  before the stop (§5.1).
- The daemon's own `resume`/`step` defaults must reproduce `step_one_with_flow` exactly,
  including what the flow tracker records. That is an extraction detail, but it is the one
  most likely to change behaviour invisibly, so it gets its own line in the golden
  transcript (§11.1).
