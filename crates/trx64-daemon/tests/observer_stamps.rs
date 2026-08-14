//! BUG-042 — an observer event must carry the cyc/PC/A of the instruction that caused
//! it, not of whatever the run segment started with.
//!
//! The reported symptom: ~130 consecutive hits all stamped `cyc=24578787 pc=$093C a=$06`,
//! then the next block sharing another. A 6502 `sta` costs at least three cycles, so
//! hundreds of accesses at one cycle count is impossible on its face.
//!
//! THE GATE HAS TO BE A REAL LOOP. A test that stores twice would pass with the old
//! per-segment snapshot, because two events inside one segment can legitimately share a
//! cycle. What only holds when the stamps are per-event is that consecutive events have
//! DISTINCT, INCREASING cycles across hundreds of accesses.

use trx64_core::{AccessCtx, BusKind, Machine, NullSink, Observer};

/// Records what each access reported, so the test can check the sequence rather than
/// one sample.
#[derive(Default)]
struct StampRecorder {
    seen: Vec<(u16, u64, u8)>, // pc, clk, a
}

impl Observer for StampRecorder {
    #[allow(clippy::too_many_arguments)]
    fn on_instruction(
        &mut self, _pc: u16, _op: u8, _b1: u8, _b2: u8,
        _a: u8, _x: u8, _y: u8, _sp: u8, _p: u8, _clk: u64,
    ) {
    }
    fn on_bus(&mut self, _k: BusKind, _a: u16, _v: u8, _pc: u16, _clk: u64, _old: u8) {}
    fn on_interrupt(&mut self, _vector: u16, _clk: u64) {}
    fn on_access(&mut self, _kind: BusKind, _addr: u16, _value: u8, cx: AccessCtx) -> bool {
        self.seen.push((cx.pc, cx.clk, cx.a));
        false
    }
}

/// A store loop over a 256-byte window: `lda #v / sta $4000,x / inx / bne`.
/// Every iteration stores once, several cycles apart, with a changing X and A.
fn store_loop() -> Vec<u8> {
    vec![
        0xA2, 0x00, // ldx #$00
        0xA9, 0x01, // lda #$01
        // loop:
        0x9D, 0x00, 0x40, // sta $4000,x
        0x69, 0x01, // adc #$01      (A changes every iteration)
        0xE8, // inx
        0xD0, 0xF8, // bne loop
        0x00, // brk
    ]
}

#[test]
fn every_access_carries_its_own_cycle_pc_and_accumulator() {
    let mut m = Machine::new();
    let code = store_loop();
    m.poke(0xC000, &code);
    m.c64_core.reg_pc = 0xC000;

    // Watch the whole store window.
    let mut watch = Box::new([0u8; 0x10000]);
    for a in 0x4000..=0x40FF {
        watch[a] = 1;
    }

    let mut rec = StampRecorder::default();
    m.run_for_full_capped_dbg(
        200_000,
        4000,
        None,
        None,
        Some(&watch),
        &mut rec,
        |_, _, _, _, _, _, _| {},
    );

    let stores: Vec<_> = rec.seen.iter().filter(|(_, _, _)| true).cloned().collect();
    assert!(
        stores.len() >= 200,
        "the loop must actually run — only {} accesses recorded",
        stores.len()
    );

    // THE property. Not "the stamps differ somewhere" — every consecutive pair must
    // advance, across the whole run.
    let mut prev = stores[0].1;
    let mut identical_runs = 0usize;
    let mut worst_run = 0usize;
    for (_, clk, _) in stores.iter().skip(1) {
        if *clk == prev {
            identical_runs += 1;
            worst_run = worst_run.max(identical_runs);
        } else {
            assert!(
                *clk > prev,
                "cycles must increase: {} after {}",
                clk,
                prev
            );
            identical_runs = 0;
        }
        prev = *clk;
    }
    assert_eq!(
        worst_run, 0,
        "found {} consecutive accesses sharing one cycle — that is the per-segment \
         stamping of BUG-042 (a 6502 store costs >= 3 cycles, so this cannot happen)",
        worst_run + 1
    );

    // And the accumulator moves with the program, rather than being one frozen value.
    let distinct_a: std::collections::HashSet<u8> = stores.iter().map(|(_, _, a)| *a).collect();
    assert!(
        distinct_a.len() > 8,
        "the accumulator changes every iteration; saw only {} distinct values — the \
         stamps are still coming from a snapshot",
        distinct_a.len()
    );
}
