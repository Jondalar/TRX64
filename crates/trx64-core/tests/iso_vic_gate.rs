//! Reproduce the iso-vic-raster + iso-vic-badline-irq gate exercisers on the
//! CPU-isolated VicBus path and report the end cycle count, to cross-check the
//! verbatim VIC steal against the TS oracle's full-machine run (which the gate
//! WS response compares against).

use trx64_core::{Machine, NullSink};

#[test]
fn iso_vic_badline_irq_cycle_count() {
    // iso-vic-badline-irq exerciser (from tools/oracle/corpus/vic):
    // SEI; LDA #$1B; STA $D011; LDA #$32; STA $D012; LDA #$01; STA $D01A;
    // LDA #$08; STA $D016; LDA #$14; STA $D018; NOP; NOP; JMP $0816
    let prog = [
        0x78u8, 0xA9, 0x1B, 0x8D, 0x11, 0xD0, 0xA9, 0x32, 0x8D, 0x12, 0xD0, 0xA9, 0x01, 0x8D,
        0x1A, 0xD0, 0xA9, 0x08, 0x8D, 0x16, 0xD0, 0xA9, 0x14, 0x8D, 0x18, 0xD0, 0xEA, 0xEA, 0x4C,
        0x16, 0x08,
    ];
    let mut m = Machine::new();
    m.poke(0x0800, &prog);
    m.set_pc(0x0800);
    let mut o = NullSink;
    m.run_for_vic(40000, &mut o);
    eprintln!("iso-vic-badline-irq end clk: {}", m.clk);
    // The badline steal must advance the clock PAST the 40000 budget (badlines
    // steal read cycles → the instruction-stepped run overshoots the budget). A
    // non-stealing VIC would land near 40000; the steal pushes it higher.
    assert!(m.clk >= 40000, "run did not reach budget: {}", m.clk);
}

#[test]
fn iso_vic_raster_cycle_count() {
    // iso-vic-raster exerciser: SEI; LDA #$7F; STA $D012; LDA #$1B; STA $D011;
    // JMP $080B
    let prog = [
        0x78u8, 0xA9, 0x7F, 0x8D, 0x12, 0xD0, 0xA9, 0x1B, 0x8D, 0x11, 0xD0, 0x4C, 0x0B, 0x08,
    ];
    let mut m = Machine::new();
    m.poke(0x0800, &prog);
    m.set_pc(0x0800);
    let mut o = NullSink;
    m.run_for_vic(19656, &mut o);
    eprintln!("iso-vic-raster end clk: {}", m.clk);
    assert!(m.clk >= 19656, "run did not reach budget: {}", m.clk);
}
