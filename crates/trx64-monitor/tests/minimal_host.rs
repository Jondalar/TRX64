//! Spec 864 §11.4 — a second host, in the test suite.
//!
//! The daemon is a big host: it has a timeline, media, traces, a transport and sixty
//! fields of its own. This one has a `Machine` and nothing else, which is the whole
//! point — it proves that everything a host may not have is a sentence rather than a
//! panic, and that the verbs which only need the machine work against any host that has
//! one.
//!
//! It is also the standing check on the trait's shape. The second real host (the UE2
//! emulator, whose C64 is `trx64-core` behind an unmodified U64 firmware) builds against
//! exactly these defaults, and if implementing `machine()` alone ever stops being enough
//! to get a monitor, this test stops compiling here rather than in their tree.

use trx64_core::Machine;
use trx64_monitor::host::MonitorHost;
use trx64_monitor::session::MonitorSession;
use trx64_monitor::verbs::{classify, try_exec};
use trx64_monitor::MachineEffect;

/// Everything a host must bring: one machine.
struct BareHost {
    machine: Machine,
    /// What the library told us happened, so the test can assert on the classification
    /// a host would act on.
    effects: Vec<MachineEffect>,
    writes: Vec<String>,
}

impl BareHost {
    fn new() -> Self {
        BareHost { machine: Machine::new(), effects: Vec::new(), writes: Vec::new() }
    }
}

impl MonitorHost for BareHost {
    fn machine(&mut self) -> &mut Machine {
        &mut self.machine
    }

    fn on_effect(&mut self, effect: MachineEffect) {
        self.effects.push(effect);
    }

    fn on_machine_write(&mut self, lens: &str) {
        self.writes.push(lens.to_string());
    }
}

fn run(mon: &mut MonitorSession, host: &mut BareHost, line: &str) -> Result<String, String> {
    try_exec(mon, host, line).unwrap_or_else(|| Err(format!("not a library verb: {line}")))
}

#[test]
fn a_host_with_only_a_machine_gets_a_monitor() {
    let mut mon = MonitorSession::new();
    let mut host = BareHost::new();

    // Registers, against a machine nobody booted. The point is that it answers at all.
    let r = run(&mut mon, &mut host, "r").expect("registers");
    assert!(r.contains("ADDR"), "the register panel: {r}");

    // Write and read back through the library's own memory path.
    run(&mut mon, &mut host, "wr 0400 de ad be ef").expect("wr");
    let m = run(&mut mon, &mut host, "m 0400 0403").expect("m");
    assert!(m.contains("de ad be ef"), "the bytes just written: {m}");
    assert_eq!(host.writes.last().map(String::as_str), Some("cpu"));

    // Disassembly of the bytes we put there.
    let d = run(&mut mon, &mut host, "d 0400 0400").expect("d");
    assert!(!d.is_empty());

    // The debug surfaces list without a run loop anywhere in sight.
    assert!(run(&mut mon, &mut host, "bk").is_ok());
    assert!(run(&mut mon, &mut host, "flow").is_ok());
    assert!(run(&mut mon, &mut host, "bt").is_ok());

    // And the help text, which is the port audit's list of what must answer.
    let h = run(&mut mon, &mut host, "help").expect("help");
    assert!(h.contains("monitor (VICE-superset)"));
}

#[test]
fn the_run_control_verbs_are_not_this_hosts_business() {
    // `g`, `until`, `z`, `n` and `ret` have not moved into the library yet, so they are
    // not the library's to answer: `try_exec` says so by returning `None` rather than
    // inventing a refusal. What IS already true is the trait's default — a host that
    // never implements run control gets the standard sentence, not a panic.
    let mut mon = MonitorSession::new();
    let mut host = BareHost::new();
    assert!(try_exec(&mut mon, &mut host, "g").is_none());

    let before = host.machine.clk;
    let err = host.resume(trx64_monitor::RunUntil::Forever).unwrap_err();
    assert!(err.contains("not available"), "the standard sentence: {err}");
    assert_eq!(host.machine.clk, before, "a refused resume moved no cycles");

    let err = host.step(1, false).unwrap_err();
    assert!(err.contains("not available"), "the standard sentence: {err}");
    assert_eq!(host.machine.clk, before, "a refused step moved no cycles");
}

#[test]
fn the_services_a_host_may_not_have_answer_instead_of_failing() {
    let mut host = BareHost::new();
    assert!(host.timeline().is_none());
    assert!(host.files().is_none());
    assert!(host.traces().is_none());
    assert!(host.cpu(trx64_monitor::Device::C64).is_none());
    assert_eq!(host.unavailable("rewind"), "rewind is not available in this host");
}

#[test]
fn the_effect_is_classified_by_the_parse_not_by_a_verb_list() {
    // The `r` special case is the whole reason this lives in the library: a host that
    // held its own list of mutating verbs could not express it, because the difference
    // between reading the registers and writing one is a `=`.
    assert_eq!(classify("r"), MachineEffect::Observes);
    assert_eq!(classify("r a=42"), MachineEffect::Mutates);
    assert_eq!(classify("  R   A=42 "), MachineEffect::Mutates);
    assert_eq!(classify("wr 0400 01"), MachineEffect::Mutates);
    assert_eq!(classify("m e000 e00f"), MachineEffect::Observes);
    assert_eq!(classify("reset"), MachineEffect::Replaces);
    assert_eq!(classify("reset cold"), MachineEffect::Replaces);
    assert_eq!(classify(""), MachineEffect::Observes);
}

#[test]
fn the_assemble_mode_belongs_to_the_library() {
    // The monitor is modal, and the mode is what a second copy of the dispatch would get
    // wrong first: `a c100` swallows every following line until an empty one.
    let mut mon = MonitorSession::new();
    let mut host = BareHost::new();

    run(&mut mon, &mut host, "a c100").expect("enter assemble mode");
    assert_eq!(mon.state.asm_cursor, Some(0xc100));

    run(&mut mon, &mut host, "inx").expect("assemble INX");
    assert_eq!(mon.state.asm_cursor, Some(0xc101));
    // The line was assembled, not dispatched as a verb.
    assert_eq!(host.machine.peek_lens(0xc100, "ram"), 0xe8);

    // An empty line leaves the mode, and the next line is a verb again.
    run(&mut mon, &mut host, "").expect("leave assemble mode");
    assert_eq!(mon.state.asm_cursor, None);
    assert!(run(&mut mon, &mut host, "bank").is_ok());
}

#[test]
fn the_selected_device_gates_the_verbs_the_library_does_not_own() {
    // `device drive8` makes the monitor read-inspect only, and that gate has to apply to
    // a HOST's verbs too — otherwise a verb the library does not own would quietly act on
    // the C64 while the user is looking at the drive.
    let mut mon = MonitorSession::new();
    let mut host = BareHost::new();

    run(&mut mon, &mut host, "device drive8").expect("select the drive");
    assert!(run(&mut mon, &mut host, "r").is_ok(), "r is read-inspect");

    let blocked = try_exec(&mut mon, &mut host, "swapcrt")
        .expect("the gate answers for a verb the library does not own");
    assert!(blocked.unwrap_err().contains("read-inspect only"));

    run(&mut mon, &mut host, "device c64").expect("back to the C64");
    assert!(try_exec(&mut mon, &mut host, "swapcrt").is_none(), "now it falls through");
}

// ── Spec 864, the second host's first finding ───────────────────────────────────
//
// `device` used to compare the argument against the literals "c64" and "drive8", so a
// host that offered a third device could never be switched to it: the UE2 emulator's
// `device fw` answered the usage line, and `devices()` was called by nothing at all. The
// read-inspect gate under it was keyed on the literal too, so a host device would have
// fallen through it as if it were the C64 — the one case where the wrong answer is
// silent rather than loud.

/// A host with a machine and one device of its own, which is exactly the shape the
/// second host has.
struct HostWithOwnDevice {
    machine: Machine,
}

impl MonitorHost for HostWithOwnDevice {
    fn machine(&mut self) -> &mut Machine {
        &mut self.machine
    }
    fn devices(&self) -> Vec<trx64_monitor::Device> {
        vec![
            trx64_monitor::Device::C64,
            trx64_monitor::Device::Drive8,
            trx64_monitor::Device::Host("fw"),
        ]
    }
}

fn run_own(
    mon: &mut MonitorSession,
    host: &mut HostWithOwnDevice,
    line: &str,
) -> Result<String, String> {
    try_exec(mon, host, line).unwrap_or_else(|| Err(format!("unknown verb: {line}")))
}

#[test]
fn a_host_device_can_be_selected_and_is_listed() {
    let mut host = HostWithOwnDevice { machine: Machine::new() };
    let mut mon = MonitorSession::default();

    let listed = run_own(&mut mon, &mut host, "device").expect("device lists");
    assert!(
        listed.contains("fw"),
        "the host's own device must appear in the list it prints: {listed}"
    );

    let picked = run_own(&mut mon, &mut host, "device fw").expect("device fw is accepted");
    assert!(picked.contains("fw"), "selecting the host's device answers with it: {picked}");
    assert_eq!(mon.state.device, "fw");
}

#[test]
fn a_device_the_host_does_not_offer_is_refused_with_the_ones_it_does() {
    let mut host = HostWithOwnDevice { machine: Machine::new() };
    let mut mon = MonitorSession::default();

    let err = run_own(&mut mon, &mut host, "device nosuch").unwrap_err();
    assert!(err.contains("c64"), "the refusal names what this host HAS: {err}");
    assert!(err.contains("fw"), "including the host's own device: {err}");
    assert_eq!(mon.state.device, "c64", "a refused selection changes nothing");
}

#[test]
fn a_host_device_is_read_inspect_like_every_device_that_is_not_the_c64() {
    let mut host = HostWithOwnDevice { machine: Machine::new() };
    let mut mon = MonitorSession::default();

    run_own(&mut mon, &mut host, "device fw").expect("select it");
    let err = run_own(&mut mon, &mut host, "wr 0400 01").unwrap_err();
    assert!(
        err.contains("read-inspect"),
        "a write under a host device must be blocked, not applied to the C64: {err}"
    );
}
