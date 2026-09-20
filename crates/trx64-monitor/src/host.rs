//! Spec 864 §3 — the host trait.
//!
//! The monitor knows what a verb means. A host knows what its world can answer. This
//! file is the line between them, and every default here is a sentence rather than a
//! panic: a host that cannot rewind says so, and the verb still exists.
//!
//! Two hosts exist as this is written. The daemon drives its own machine, holds a
//! timeline, mounts media and writes traces. The UE2 emulator runs the unmodified U64
//! firmware: its C64 is driven by the firmware's clock, it has no timeline, its reset
//! must go through the firmware, and one of its CPUs is a 32-bit RISC-V. Everything that
//! differs between those two is in this file; everything they share is in the rest of the
//! crate.

use trx64_core::Machine;

/// A device the monitor can look at. `device`/`dev` selects one; `r`, `m` and `d` follow
/// the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    /// The C64's 6510.
    C64,
    /// The 1541's 6502. On both hosts this is `Machine::drive8` — UE2 swaps the real
    /// drive in and out of it, so the device means the same thing on both.
    Drive8,
    /// Anything a host adds. The str is the name the user types after `device`.
    Host(&'static str),
}

/// One CPU register, as the view that owns it describes it.
#[derive(Debug, Clone)]
pub struct Reg {
    pub name: &'static str,
    /// Width in bits — 8 for the 6502's A, 32 for a RISC-V x-register.
    pub bits: u8,
    pub value: u64,
}

/// A flag register, for a CPU that has one. The 6502 does; the RISC-V does not, and
/// `p`/`fl` says so there rather than rendering eight dashes.
#[derive(Debug, Clone)]
pub struct FlagSpec {
    /// Most significant first, e.g. "NV-BDIZC".
    pub letters: &'static str,
    pub value: u8,
}

/// What the monitor needs of a CPU to show it.
///
/// The address is `u64` and the view states its own width, because the second host's
/// firmware CPU is 32-bit: its devices sit at `0x1004_0000`, its cartridge ROM at
/// `0x03C0_0000`. A `u16` anywhere in this path and that device could address nothing it
/// has.
pub trait CpuView {
    /// 16 for a 6502, 32 for the firmware's RISC-V. The lib parses and formats against
    /// this, so `m` and `d` neither truncate nor pad.
    fn addr_bits(&self) -> u8;

    fn read(&mut self, addr: u64) -> Option<u8>;
    fn write(&mut self, addr: u64, value: u8) -> Result<(), String>;

    fn registers(&mut self) -> Vec<Reg>;

    /// A view may expose a register it will not let you write; refusing is per view, not
    /// a global rule.
    fn set_register(&mut self, name: &str, _value: u64) -> Result<(), String> {
        Err(format!("register {name} is read-only on this device"))
    }

    /// `None` — this CPU has no flag register.
    fn flags(&mut self) -> Option<FlagSpec> {
        None
    }

    /// `None` — no decoder for this instruction set here. `d` says why rather than
    /// printing nonsense.
    fn disasm(&mut self, _addr: u64) -> Option<(u8, String)> {
        None
    }

    /// Empty — `bank` is not a question for this device.
    fn banks(&self) -> &[&'static str] {
        &[]
    }

    /// Breakpoints and the exec/access watch tables are `[u8; 0x10000]` — a 6502 shape,
    /// not a general one. A view that is not the C64 answers `false` and the debug verbs
    /// refuse by name instead of watching the wrong 64 KB.
    fn supports_debug_gates(&self) -> bool {
        false
    }
}

/// What a command did to the C64 and its timeline.
///
/// It is named for what it measures. The second host's first verb reads and writes the
/// Ultimate's own settings — a great deal changes, none of it the C64 — and under a name
/// like `Reads` that declaration would have read as a lie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineEffect {
    /// Looked at the machine, or changed something that is not the machine.
    Observes,
    /// Changed machine state: memory, a register, the PC.
    Mutates,
    /// Replaced the machine. The anchors are about one that no longer exists.
    Replaces,
}

/// How far a resume should go.
///
/// It must be re-statable as a line a human reads — "running until $c000" — because a
/// host that does not drive its own machine answers `Resumed` and the stop arrives later
/// (§5.1). A condition that can only be matched, never stated, would leave that host
/// unable to say what it is waiting for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunUntil {
    /// Until something stops it: a breakpoint, an observer, the user.
    Forever,
    /// Until the PC reaches this address.
    Pc(u16),
    /// Until this many cycles have passed.
    Cycles(u64),
}

impl RunUntil {
    /// The words a host prints after "running": `d` on a resume that has not stopped yet.
    pub fn describe(&self) -> String {
        match self {
            RunUntil::Forever => "until something stops it".into(),
            RunUntil::Pc(a) => format!("until PC=${a:04x}"),
            RunUntil::Cycles(n) => format!("for {n} cycles"),
        }
    }
}

/// Why the machine stopped, and where.
#[derive(Debug, Clone)]
pub struct StopInfo {
    pub pc: u16,
    pub cycle: u64,
    /// "breakpoint #1", "observer probe", "step", "jam", "budget" — printed as given.
    pub reason: String,
}

/// The answer to a resume. A host that drives its own machine returns `Stopped`; a host
/// whose machine is driven by something else returns `Resumed` at once and calls
/// [`MonitorHost::on_stop`] when the halt actually happens.
#[derive(Debug, Clone)]
pub enum Resumption {
    Stopped(StopInfo),
    Resumed { until: RunUntil },
}

/// What a reset does to the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetKind {
    Cold,
    Warm,
}

/// The host of a monitor.
///
/// One method has no default. Everything else answers "not available in this host" until
/// a host says otherwise, and the monitor prints that answer instead of pretending.
pub trait MonitorHost {
    /// The C64. The one thing every host of this monitor has.
    fn machine(&mut self) -> &mut Machine;

    // ── devices ──────────────────────────────────────────────────────────────

    /// A device's CPU view. The default answers nothing: the verbs that have moved
    /// reach the two 6502s through [`Self::machine`] and `machine().drive8`, exactly as
    /// they did before the extraction, and a host adds a view when it has a CPU those
    /// cannot reach — the second host's 32-bit firmware core is the case this exists
    /// for.
    fn cpu(&mut self, _dev: Device) -> Option<&mut dyn CpuView> {
        None
    }

    /// The devices `device` will accept, for the error message and for `help`.
    fn devices(&self) -> Vec<Device> {
        vec![Device::C64, Device::Drive8]
    }

    // ── running ──────────────────────────────────────────────────────────────

    /// Resume. A host that owns its own run loop drives the machine here; a host whose
    /// machine is driven by something else returns `Resumed` and calls
    /// [`Self::on_stop`] later.
    ///
    /// §3 wanted the DEFAULT to drive the machine. It cannot, and the reason is worth
    /// writing down rather than discovering twice: the daemon's step is
    /// `step_one_with_flow`, which classifies the step and pushes or pops a frame on
    /// the FlowTracker — and the FlowTracker lives in `MonitorSession`, which is the
    /// library's state, not the host's. A default here would either skip that (and the
    /// `flow` panel would go quietly wrong) or need the session passed in, which is a
    /// signature change nobody has earned yet. So the default is the same sentence
    /// every absent service gets, and a host that runs a machine says so.
    fn resume(&mut self, _until: RunUntil) -> Result<Resumption, String> {
        Err(self.unavailable("run control"))
    }

    /// Step `n` instructions, `over` skipping a JSR's body. Both hosts can do this
    /// synchronously: one instruction out of band moves no other clock.
    fn step(&mut self, _n: u64, _over: bool) -> Result<StopInfo, String> {
        Err(self.unavailable("run control"))
    }

    /// Halt or release the machine. A host implements it with whatever stop it has; the
    /// second host routes it through the firmware so the firmware stays consistent with
    /// the machine it is hosting.
    fn set_halted(&mut self, _halted: bool) -> Result<(), String> {
        Ok(())
    }

    /// A stop that arrived outside a command (§5.1). The library keeps it as the last
    /// stop; this is where the host surfaces it — a notification, or the next `status`.
    fn on_stop(&mut self, _stop: &StopInfo) {}

    // ── consequences ─────────────────────────────────────────────────────────

    /// What the command that just ran did to the machine. The library classifies; the
    /// host decides what follows. Default: nothing follows.
    fn on_effect(&mut self, _effect: MachineEffect) {}

    /// A write the monitor just performed, and the bank lens it went through ("ram",
    /// "io", "cpu", "rom", "cart"). The WRITE itself is the library's — it is the
    /// machine's own memory, and both hosts have the same `trx64_core::Machine`. What
    /// it MEANS is the host's.
    ///
    /// This is not `on_effect` with extra words. `on_effect` says what one COMMAND did
    /// to the timeline and is fired once, before the verb runs, because Spec 808's
    /// truncation has to happen before a `g` starts appending anchors. This fires per
    /// write, after it, and carries the lens, because the daemon's bus-selection gate
    /// latches `injected` and `io_injected` SEPARATELY — an `io` write means the VIC
    /// must be ticking, a `ram` write does not. One flag for both, or a notification
    /// without the lens, would silently drop a booted machine onto the isolated core.
    fn on_machine_write(&mut self, _lens: &str) {}

    /// Replace the machine. The default does the machine-level reset; a host that
    /// implements this is the only thing that runs, because a host whose firmware owns
    /// the reset line cannot be told about it afterwards.
    fn reset(&mut self, kind: ResetKind) -> Result<String, String> {
        match kind {
            ResetKind::Warm => {
                self.machine().warm_reset();
                Ok("warm reset".into())
            }
            ResetKind::Cold => Err("this host has no cold reset".into()),
        }
    }

    // ── services a host may not have ─────────────────────────────────────────

    /// Rewind, goto, frame, mark, rstep, the checkpoint ring.
    fn timeline(&mut self) -> Option<&mut dyn Timeline> {
        None
    }

    /// Mount, ls, load, save, cart, disk — and the working directory they resolve
    /// against, which belongs to the filesystem rather than to the monitor. The MEMORY
    /// half of `load`/`save`/`bload`/`bsave` is the library's and takes the same path as
    /// `wr`; only the file half is here.
    fn files(&mut self) -> Option<&mut dyn Files> {
        None
    }

    /// tracedb, traceindex, swimlane, taint, whowrote.
    fn traces(&mut self) -> Option<&mut dyn Traces> {
        None
    }

    /// What the library prints for a service this host does not have. One sentence, one
    /// place, so every refusal reads the same.
    fn unavailable(&self, what: &str) -> String {
        format!("{what} is not available in this host")
    }
}

/// The timeline services. Deliberately narrow: what the monitor's verbs need, not what a
/// host happens to have.
pub trait Timeline {
    fn rewind(&mut self, cycles: u64) -> Result<String, String>;
    fn goto_cycle(&mut self, cycle: u64) -> Result<String, String>;
    fn mark(&mut self, label: &str) -> Result<String, String>;
    fn list_marks(&mut self) -> Result<String, String>;
    /// An intervention while rewound truncates the future; the host reports how much
    /// went, so the monitor can print a number rather than a shrug.
    fn truncate(&mut self) -> Result<usize, String>;
    fn discard(&mut self, why: &str) -> Result<usize, String>;
}

/// The filesystem side of the media and file verbs.
pub trait Files {
    fn cwd(&self) -> String;
    fn set_cwd(&mut self, path: &str) -> Result<String, String>;
    fn list(&mut self, path: &str) -> Result<String, String>;
    fn read(&mut self, path: &str) -> Result<Vec<u8>, String>;
    fn write(&mut self, path: &str, bytes: &[u8]) -> Result<(), String>;
    fn mount(&mut self, path: &str, device: u8) -> Result<String, String>;
    fn unmount(&mut self, device: u8) -> Result<String, String>;
}

/// The trace sinks and readers.
pub trait Traces {
    fn status(&mut self) -> Result<String, String>;
    fn history(&mut self, from_cycle: Option<u64>, to_cycle: Option<u64>) -> Result<String, String>;
    fn who_wrote(&mut self, addr: u16, n: usize) -> Result<String, String>;
    fn taint(&mut self, addr: u16, cycle: Option<u64>) -> Result<String, String>;
    fn swimlane(&mut self, from_cycle: u64, to_cycle: u64) -> Result<String, String>;
}
