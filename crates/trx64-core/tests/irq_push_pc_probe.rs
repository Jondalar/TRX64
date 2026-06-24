//! Validate that a HARDWARE interrupt dispatch on the verbatim SC core emits the
//! stack-push WRITE records with the `pc` field = the RETURN ADDRESS (= cpu.rs
//! `service_interrupt`, which pushed `self.reg_pc`), NOT the post-fetch synthetic
//! pc. The 50k/2M trace gates' windows do not reach a HW IRQ before the
//! pre-existing $E4DD VIC BA-steal skew, so this probe validates the
//! interrupt-dispatch trace-pc path directly off the verbatim core's own stream.

use std::path::Path;
use trx64_core::{BusKind, Machine, Observer};

const ROM_DIR: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";

#[derive(Default)]
struct PushSink {
    /// Stack WRITE records (addr, value, pc) captured in order.
    stack_writes: Vec<(u16, u8, u16)>,
    /// $EA31 IRQ-handler entry count (proves a HW IRQ dispatched).
    irq_entries: usize,
}

impl Observer for PushSink {
    #[allow(clippy::too_many_arguments)]
    fn on_instruction(
        &mut self,
        pc: u16,
        _op: u8,
        _b1: u8,
        _b2: u8,
        _a: u8,
        _x: u8,
        _y: u8,
        _sp: u8,
        _p: u8,
        _clk: u64,
    ) {
        if pc == 0xEA31 {
            self.irq_entries += 1;
        }
    }
    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, pc: u16, _clk: u64, _old: u8) {
        if kind == BusKind::Write && (0x0100..=0x01FF).contains(&addr) {
            self.stack_writes.push((addr, value, pc));
        }
    }
    fn on_interrupt(&mut self, _v: u16, _c: u64) {}
}

#[test]
fn hw_irq_push_pc_is_return_address() {
    let d = Path::new(ROM_DIR);
    if !d.join("kernal-901227-03.bin").exists() {
        eprintln!("ROMs absent; skipping IRQ push-pc probe");
        return;
    }
    let mut m = Machine::new();
    m.boot_from_dir(d).expect("boot ROMs");
    let mut sink = PushSink::default();
    // Run far enough that the CIA1 Timer-A cursor IRQ fires through $EA31.
    m.run_for_full(2_300_000, &mut sink, |_, _, _, _, _, _, _| {});

    assert!(sink.irq_entries > 0, "no $EA31 HW IRQ dispatched in window");
    eprintln!("HW IRQ ($EA31) dispatches: {}", sink.irq_entries);

    // A HW IRQ pushes PCH, PCL, P as three consecutive stack writes. The PCH+PCL
    // values reconstruct the return address; their pc fields must EQUAL that return
    // address (cpu.rs pushed self.reg_pc). Scan for a PCH/PCL pair (descending SP,
    // P-byte with B clear, bit5 set) and assert.
    let w = &sink.stack_writes;
    let mut checked = 0;
    for i in 0..w.len().saturating_sub(2) {
        let (a0, v0, pc0) = w[i]; // PCH
        let (a1, v1, pc1) = w[i + 1]; // PCL
        let (a2, v2, _pc2) = w[i + 2]; // P
        // Consecutive descending stack addresses + a P byte with B(0x10) clear and
        // UNUSED(0x20) set = a hardware (not BRK) interrupt frame.
        if a0.wrapping_sub(1) == a1
            && a1.wrapping_sub(1) == a2
            && (v2 & 0x10) == 0
            && (v2 & 0x20) != 0
        {
            let ret = ((v0 as u16) << 8) | v1 as u16;
            // The PCH/PCL push records' pc field must be the return address.
            if pc0 == ret && pc1 == ret {
                checked += 1;
                eprintln!(
                    "HW IRQ frame: ret=${ret:04X} push_pc_field=${pc0:04X} (OK — equals return addr)"
                );
                if checked >= 3 {
                    break;
                }
            } else if (v2 & 0x10) == 0 && (v2 & 0x20) != 0 && a0 >= 0x0102 {
                // A genuine IRQ frame whose push pc does NOT match the return addr
                // is the bug this probe guards against.
                panic!(
                    "HW IRQ push pc mismatch: ret=${ret:04X} but push_pc_field=${pc0:04X}/${pc1:04X}"
                );
            }
        }
    }
    assert!(checked > 0, "found no HW IRQ stack frame to validate");
}
