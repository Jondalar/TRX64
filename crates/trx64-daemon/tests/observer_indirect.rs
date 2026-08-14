//! An observer watches an ADDRESS, not an instruction — so it must fire when the
//! address is reached indirectly, through a pointer, an index, or a computed jump.
//!
//! This is the property that makes observers usable on real software, where almost
//! nothing touches an interesting address with an absolute operand. A loader writes
//! through `($fb),y`, a game dispatches through a jump table, the KERNAL enters its
//! IRQ through `($0314)`. A watch that only matched literal operands would be silent
//! through all of it, and silent in the way that looks like "nothing happened here".
//!
//! It holds for a structural reason worth stating: the hooks sit AFTER the addressing
//! mode is resolved. A load/store observer is on the bus access, where the address is
//! the effective address the CPU actually drove; an exec observer is on the program
//! counter at an instruction boundary, whatever branched there.

use trx64_core::{AccessCtx, BusKind, Machine, Observer};

#[derive(Default)]
struct Hits {
    writes: Vec<(u16, u8)>, // addr, value
    reads: Vec<u16>,
}

impl Observer for Hits {
    #[allow(clippy::too_many_arguments)]
    fn on_instruction(
        &mut self, _pc: u16, _op: u8, _b1: u8, _b2: u8,
        _a: u8, _x: u8, _y: u8, _sp: u8, _p: u8, _clk: u64,
    ) {
    }
    fn on_bus(&mut self, _k: BusKind, _a: u16, _v: u8, _pc: u16, _clk: u64, _old: u8) {}
    fn on_interrupt(&mut self, _vector: u16, _clk: u64) {}
    fn on_access(&mut self, kind: BusKind, addr: u16, value: u8, _cx: AccessCtx) -> bool {
        match kind {
            BusKind::Write => self.writes.push((addr, value)),
            BusKind::Read => self.reads.push(addr),
            _ => {}
        }
        false
    }
}

fn watch(range: std::ops::RangeInclusive<usize>) -> Box<[u8; 0x10000]> {
    let mut w = Box::new([0u8; 0x10000]);
    for a in range {
        w[a] = 1;
    }
    w
}

/// `sta ($fb),y` — the address exists nowhere in the instruction. It is assembled at
/// run time from a zero-page pointer plus Y, which is how essentially every loader
/// and depacker writes its output.
#[test]
fn a_write_through_a_zero_page_pointer_is_seen() {
    let mut m = Machine::new();
    m.poke(
        0xC000,
        &[
            0xA9, 0x00, // lda #$00
            0x85, 0xFB, // sta $fb        ptr lo
            0xA9, 0x40, // lda #$40
            0x85, 0xFC, // sta $fc        ptr hi   -> ($fb) = $4000
            0xA0, 0x05, // ldy #$05
            0xA9, 0x7E, // lda #$7e
            0x91, 0xFB, // sta ($fb),y    -> writes $7e to $4005
            0x00, // brk
        ],
    );
    m.c64_core.reg_pc = 0xC000;

    // Watch ONLY the effective address. $fb/$fc are deliberately outside the window,
    // so a hit can only come from the resolved access.
    let w = watch(0x4000..=0x40FF);
    let mut hits = Hits::default();
    m.run_for_full_capped_dbg(2_000, 64, None, None, Some(&w), &mut hits, |_, _, _, _, _, _, _| {});

    assert_eq!(
        hits.writes,
        vec![(0x4005, 0x7E)],
        "the observer must see the RESOLVED address $4005, not the operand $fb"
    );
}

/// `lda ($fb,x)` — the pointer itself is picked by X. Same question from the read side.
#[test]
fn a_read_through_an_indexed_indirect_pointer_is_seen() {
    let mut m = Machine::new();
    // Pointer table at $f0: entry 2 (x=4) points at $4123.
    m.poke(0x00F4, &[0x23, 0x41]);
    m.poke(0x4123, &[0x99]);
    m.poke(
        0xC000,
        &[
            0xA2, 0x04, // ldx #$04
            0xA1, 0xF0, // lda ($f0,x)   -> reads $4123
            0x00, // brk
        ],
    );
    m.c64_core.reg_pc = 0xC000;

    let w = watch(0x4100..=0x41FF);
    let mut hits = Hits::default();
    m.run_for_full_capped_dbg(2_000, 64, None, None, Some(&w), &mut hits, |_, _, _, _, _, _, _| {});

    assert!(
        hits.reads.contains(&0x4123),
        "the observer must see the resolved read at $4123; saw {:04X?}",
        hits.reads
    );
}

/// Absolute-indexed, the ordinary case, kept alongside so a regression cannot pass by
/// breaking only one addressing family.
#[test]
fn an_absolute_indexed_write_is_seen() {
    let mut m = Machine::new();
    m.poke(
        0xC000,
        &[
            0xA2, 0x10, // ldx #$10
            0xA9, 0x2A, // lda #$2a
            0x9D, 0x00, 0x40, // sta $4000,x  -> $4010
            0x00, // brk
        ],
    );
    m.c64_core.reg_pc = 0xC000;

    let w = watch(0x4000..=0x40FF);
    let mut hits = Hits::default();
    m.run_for_full_capped_dbg(2_000, 64, None, None, Some(&w), &mut hits, |_, _, _, _, _, _, _| {});

    assert_eq!(hits.writes, vec![(0x4010, 0x2A)]);
}

/// An operand byte that merely LOOKS like a watched address must not fire it. The
/// fetch path is separate from the access path on purpose; without that split, code
/// running inside a watched window would drown a data watch in its own opcodes.
#[test]
fn fetching_an_operand_is_not_an_access() {
    let mut m = Machine::new();
    // `lda #$00 / lda #$40` — the bytes $00 and $40 are fetched, never accessed.
    m.poke(0xC000, &[0xA9, 0x00, 0xA9, 0x40, 0x00]);
    m.c64_core.reg_pc = 0xC000;

    let mut w = watch(0x0000..=0x0000);
    w[0x0040] = 1;
    w[0xC001] = 1; // the operand byte's own address, fetched every pass
    let mut hits = Hits::default();
    m.run_for_full_capped_dbg(2_000, 64, None, None, Some(&w), &mut hits, |_, _, _, _, _, _, _| {});

    assert!(
        hits.reads.is_empty() && hits.writes.is_empty(),
        "a fetch is not a data access; saw reads {:04X?} writes {:04X?}",
        hits.reads,
        hits.writes
    );
}

/// `jmp ($3ffe)` — the destination is in memory, not in the instruction. An exec
/// observer watches the PROGRAM COUNTER, so it fires on arrival however control got
/// there: a vector, a jump table, an RTS trampoline, or self-modified code.
#[test]
fn a_computed_jump_reaches_a_watched_pc() {
    let mut m = Machine::new();
    m.poke(0x3FFE, &[0x00, 0x45]); // vector -> $4500
    m.poke(0x4500, &[0xEA, 0x00]); // nop / brk at the destination
    m.poke(0xC000, &[0x6C, 0xFE, 0x3F, 0x00]); // jmp ($3ffe)
    m.c64_core.reg_pc = 0xC000;

    // The core's exec gate is the breakpoint set: it halts AT the pc, before it runs.
    let bp: std::collections::HashSet<u16> = [0x4500u16].into_iter().collect();

    let mut hits = Hits::default();
    let stop = m.run_for_full_capped_dbg(
        2_000, 64, Some(&bp), None, None, &mut hits, |_, _, _, _, _, _, _| {},
    );

    match stop {
        trx64_core::RunStop::Breakpoint(pc) => assert_eq!(
            pc, 0x4500,
            "an exec watch fires on the PC that was reached, not on the jump's operand"
        ),
        other => panic!("expected to halt at the indirect destination, got {other:?}"),
    }
}
