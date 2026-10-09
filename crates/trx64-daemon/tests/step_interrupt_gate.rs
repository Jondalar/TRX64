//! C64RE #49 — the monitor's stepping around hardware interrupts is VICE's.
//!
//! VICE (`6510core.c` DO_INTERRUPT, `monitor.c` `monitor_check_icount` /
//! `monitor_check_icount_interrupt`): an IRQ/NMI taken at an instruction boundary ends a
//! step-into right after the 7-cycle entry, on the handler's first opcode ($FF48 / $FE43);
//! under step-over (`n`, `ret`) it is a new level like a JSR, runs through its RTI, and the
//! step ends back in the interrupted code.
//!
//! Drives the real `monitor/exec` dispatch on a cold-reset machine. A CIA timer raises the
//! interrupt at a fixed distance from the program's start; the vectors at $0314/$0318 point
//! at a handler that bumps a counter at $C300, so "the handler ran" is a number.
//!
//! ROMs: `TRX64_ROM_DIR`, `~/.trx64/roms`, or the sibling workbench; without them the tests
//! say so and skip.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use trx64_daemon::{create_embedded_state, dispatch, Request, SharedState};

fn rom_dir() -> Option<PathBuf> {
    let has = |p: &Path| p.join("kernal-901227-03.bin").exists();
    if let Ok(d) = std::env::var("TRX64_ROM_DIR") {
        let p = PathBuf::from(d);
        if has(&p) {
            return Some(p);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join(".trx64").join("roms");
        if has(&p) {
            return Some(p);
        }
    }
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for up in [4, 5] {
        let mut p = here.clone();
        for _ in 0..up {
            p = p.parent()?.to_path_buf();
        }
        let p = p.join("C64ReverseEngineeringMCP").join("resources").join("roms");
        if has(&p) {
            return Some(p);
        }
    }
    None
}

struct Rig {
    state: SharedState,
    id: u64,
    /// Address of the spin loop's NOP; its JMP (back to here) follows.
    spin: u16,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Regs {
    pc: u16,
    a: u8,
    sp: u8,
}

const MAIN: u16 = 0xc100;
const HANDLER: u16 = 0xc200;
const COUNTER: u16 = 0xc300;

impl Rig {
    fn new(cia: u16) -> Option<Rig> {
        let roms = rom_dir().or_else(|| {
            eprintln!("SKIP step_interrupt_gate: no ROM set (TRX64_ROM_DIR). NOT green, NOT run.");
            None
        })?;
        let state = create_embedded_state(&roms).expect("boot the machine from ROMs");
        let mut rig = Rig { state, id: 0, spin: 0 };
        let nmi = cia == 0xdd00;
        let icr = cia + 0x0d;
        let (lo, hi) = (icr as u8, (icr >> 8) as u8);
        // Main: SEI, program the timer for an interrupt $1000 cycles out, CLI, then a spin of
        // NOP + JMP (two instructions, so "the next instruction" is not the same address).
        let mut main: Vec<u8> = vec![
            0x78, // SEI
            0xa9, 0x7f, 0x8d, lo, hi, // mask everything
            0xad, lo, hi, // acknowledge
            0xa9, 0x00, 0x8d, (cia + 4) as u8, (cia >> 8) as u8, // TA lo
            0xa9, 0x10, 0x8d, (cia + 5) as u8, (cia >> 8) as u8, // TA hi = $1000
            0xa9, 0x81, 0x8d, lo, hi, // TA interrupt on
            0xa9, 0x11, 0x8d, (cia + 0x0e) as u8, (cia >> 8) as u8, // force load + start
        ];
        if !nmi {
            main.push(0x58); // CLI (an NMI needs no CLI)
        }
        let spin = MAIN + main.len() as u16;
        main.extend([0xea, 0x4c, spin as u8, (spin >> 8) as u8]); // spin: NOP / JMP spin
        rig.spin = spin;
        rig.wr(MAIN, &main);
        // Handler: INC counter, acknowledge the CIA, return. IRQ leaves through the KERNAL's
        // pull-registers tail $EA81 (which RTIs), NMI RTIs itself.
        let mut h = vec![0xee, COUNTER as u8, (COUNTER >> 8) as u8, 0xad, lo, hi];
        if nmi {
            h.push(0x40);
        } else {
            h.extend([0x4c, 0x81, 0xea]);
        }
        rig.wr(HANDLER, &h);
        let vec_at = if nmi { 0x0318 } else { 0x0314 };
        rig.wr(vec_at, &[HANDLER as u8, (HANDLER >> 8) as u8]);
        rig.wr(COUNTER, &[0]);
        rig.cmd("r sp=ff");
        rig.cmd(&format!("r pc={MAIN:04x}"));
        // Run into the spin loop (SEI..CLI), so every later step is a spin step.
        for _ in 0..40 {
            if rig.regs().pc >= spin {
                break;
            }
            rig.cmd("z");
        }
        assert!(rig.regs().pc >= spin, "reached the spin loop");
        Some(rig)
    }

    fn cmd(&mut self, c: &str) -> String {
        self.id += 1;
        let req = Request { jsonrpc: "2.0".into(), id: json!(self.id), method: "monitor/exec".into(), params: json!({ "command": c }) };
        let res = dispatch(req, &self.state);
        match (res.result, res.error) {
            (Some(v), _) => v.get("output").and_then(Value::as_str).unwrap_or("").to_string(),
            (None, Some(e)) => panic!("monitor/exec {c:?}: {}", e.message),
            _ => panic!("monitor/exec {c:?}: no result"),
        }
    }

    fn wr(&mut self, at: u16, bytes: &[u8]) {
        let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
        self.cmd(&format!("wr {at:04x} {}", hex.join(" ")));
    }

    fn regs(&mut self) -> Regs {
        let out = self.cmd("r");
        let line = out.lines().find(|l| l.starts_with(".;")).unwrap_or_else(|| panic!("no register line in {out:?}"));
        let f: Vec<&str> = line[2..].split_whitespace().collect();
        let h = |s: &str| u32::from_str_radix(s, 16).unwrap();
        Regs { pc: h(f[0]) as u16, a: h(f[1]) as u8, sp: h(f[4]) as u8 }
    }

    fn counter(&mut self) -> u8 {
        let out = self.cmd(&format!("m {COUNTER:04x} {COUNTER:04x}"));
        let tok = out.split_whitespace().nth(1).unwrap_or_else(|| panic!("memory line {out:?}"));
        u8::from_str_radix(tok, 16).unwrap()
    }

    fn flow(&mut self) -> String {
        self.cmd("flow")
    }
}

const GUARD: usize = 3000;

/// `z` until the landing line shows `target`; returns the registers before that last `z`.
fn z_until(rig: &mut Rig, target: u16) -> Regs {
    let needle = format!("${target:04x}");
    for _ in 0..GUARD {
        let before = rig.regs();
        let out = rig.cmd("z");
        if out.starts_with(&needle) {
            return before;
        }
    }
    panic!("`z` never landed on ${target:04x} in {GUARD} steps");
}

fn step_into(cia: u16, entry: u16, first_opcode_pushes: bool) {
    let Some(mut rig) = Rig::new(cia) else { return };
    let sp_main = rig.regs().sp;
    let a_main = rig.regs().a;
    let before = z_until(&mut rig, entry);
    assert!(before.pc >= MAIN && before.pc < HANDLER, "the step started in main code, was ${:04x}", before.pc);
    let r = rig.regs();
    assert_eq!(r.pc, entry, "the step ends ON the handler's first opcode");
    assert_eq!(r.sp, sp_main.wrapping_sub(3), "entry only: PCH, PCL, P pushed, SP-3");
    assert_eq!(r.a, a_main, "the first opcode has not run, A untouched");
    assert_eq!(rig.counter(), 0, "the handler has not run");
    let fl = rig.flow();
    assert!(fl.contains("current=irq") || fl.contains("current=nmi"), "flow has the interrupt frame: {fl}");
    assert!(fl.contains(&format!("enter=${entry:04X}")), "frame entered at the handler start: {fl}");
    // The next step runs the first opcode.
    rig.cmd("z");
    let r2 = rig.regs();
    assert_eq!(r2.pc, entry + 1, "one more step executes the first opcode");
    let want = if first_opcode_pushes { sp_main.wrapping_sub(4) } else { sp_main.wrapping_sub(3) };
    assert_eq!(r2.sp, want);
}

#[test]
fn step_into_stops_on_the_irq_entry() {
    step_into(0xdc00, 0xff48, true);
}

#[test]
fn step_into_stops_on_the_nmi_entry() {
    step_into(0xdd00, 0xfe43, false);
}

/// `n` until the handler's counter moves; returns (registers before, landing line, after).
fn n_until_handler_ran(rig: &mut Rig) -> (Regs, String, Regs) {
    let c0 = rig.counter();
    for _ in 0..GUARD {
        let before = rig.regs();
        let out = rig.cmd("n");
        if rig.counter() != c0 {
            let after = rig.regs();
            return (before, out, after);
        }
    }
    panic!("the interrupt never arrived during `n` in {GUARD} steps");
}

fn step_over(cia: u16, entry: u16) {
    let Some(mut rig) = Rig::new(cia) else { return };
    let (before, line, after) = n_until_handler_ran(&mut rig);
    assert!(before.pc >= MAIN && before.pc < HANDLER, "stepping in main code");
    // The spin is NOP ($EA) then JMP: the step's instruction is stepped, the interrupt that
    // follows it runs through its RTI, and the step ends on the NEXT main instruction.
    let next = if before.pc == rig.spin { rig.spin + 1 } else { rig.spin };
    assert_eq!(after.pc, next, "lands on the next main-code instruction ({line})");
    assert_eq!(after.sp, before.sp, "SP is back at the main level");
    assert!(!line.contains(&format!("${entry:04x}")), "never reports the handler: {line}");
    assert_eq!(rig.counter(), 1, "the handler ran exactly once, fully");
    let fl = rig.flow();
    assert!(fl.contains("current=main"), "back in the main flow: {fl}");
    assert!(fl.contains("no interrupt/trap frame active"), "the run-through is balanced: {fl}");
}

#[test]
fn step_over_runs_the_irq_through_its_rti() {
    step_over(0xdc00, 0xff48);
}

#[test]
fn step_over_runs_the_nmi_through_its_rti() {
    step_over(0xdd00, 0xfe43);
}

/// A user breakpoint inside the handler stops the run-through there.
#[test]
fn step_over_stops_on_a_breakpoint_inside_the_handler() {
    for (cia, name) in [(0xdc00u16, "irq"), (0xdd00, "nmi")] {
        let Some(mut rig) = Rig::new(cia) else { return };
        rig.cmd(&format!("bk {HANDLER:04x}"));
        let mut hit = None;
        for _ in 0..GUARD {
            let out = rig.cmd("n");
            if out.contains("hit user bp") {
                hit = Some(out);
                break;
            }
        }
        let out = hit.unwrap_or_else(|| panic!("{name}: `n` never stopped on the handler breakpoint"));
        assert_eq!(rig.regs().pc, HANDLER, "{name}: stopped ON the breakpoint ({out})");
        assert_eq!(rig.counter(), 0, "{name}: stopped before the handler's first instruction ran");
    }
}

/// `ret` out of a handler: the RTI is the return, the flow frame is popped.
#[test]
fn return_from_the_handler_pops_the_flow_frame() {
    let Some(mut rig) = Rig::new(0xdc00) else { return };
    z_until(&mut rig, 0xff48);
    assert!(rig.flow().contains("current=irq"));
    rig.cmd("ret");
    let r = rig.regs();
    assert!(r.pc >= MAIN && r.pc < HANDLER, "ret ends back in main code, at ${:04x}", r.pc);
    assert_eq!(rig.counter(), 1, "the handler ran to its RTI");
    let fl = rig.flow();
    assert!(fl.contains("no interrupt/trap frame active"), "RTI popped the frame: {fl}");
}
