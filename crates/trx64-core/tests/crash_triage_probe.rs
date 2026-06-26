//! reverse-debug Phase 2 — live integration probe for the guided crash-triage.
//!
//! Crafts a DELIBERATE stack-smash through the REAL emulator core (no ROMs — the code
//! lives at $C000, always RAM): a routine sets SP, writes a CORRUPTED return address
//! onto the 6502 stack via two `STA`s, then `RTS` pops it and jumps into the wild → the
//! wild PC holds a JAM ($02) opcode → the CPU jams. We then run `Machine::crash_triage`
//! on the jammed state and assert the chain (i) names the JAM/wild PC, (ii) identifies
//! the RTS stack pop, and (iii) `who_wrote` PINS the smashing instruction's PC — the
//! exact "assert the corruptor PC == the known smashing instruction" the spec asks for,
//! end-to-end through the actual core (the pure heuristic is unit-tested separately).

use trx64_core::crash_triage::TransferKind;
use trx64_core::Machine;

/// Poke a program at `$C000`, point the SC core's PC at it, and run up to `instrs`
/// instructions on the full-machine path (or until the CPU jams).
fn run_program(m: &mut Machine, code: &[u8], instrs: u64) {
    let base = 0xc000u16;
    m.poke(base, code);
    m.c64_core.reg_pc = base;
    m.run_for_full_capped(instrs * 16, instrs, &mut trx64_core::NullSink, |_, _, _, _, _, _, _| {});
}

#[test]
fn triage_pins_the_stack_smashing_instruction() {
    let mut m = Machine::new();
    m.delta_ring.set_enabled(true);
    m.cpu_history.set_enabled(true);

    // ── The stack-smash program @ $C000 ──────────────────────────────────────────
    //   LDX #$FC       A2 FC        ; SP target
    //   TXS            9A           ; SP = $FC → RTS pops $01FD(lo)/$01FE(hi)
    //   LDA #$DD       A9 DD
    //   STA $01FD      8D FD 01     ; CORRUPTOR (ret-lo) — the smashing STA @ $C005
    //   LDA #$C0       A9 C0
    //   STA $01FE      8D FE 01     ; CORRUPTOR (ret-hi) — the smashing STA @ $C00A
    //   RTS            60           ; pops $C0DD → PC = $C0DE (the wild PC)  @ $C00D
    // and at the WILD PC $C0DE: a JAM ($02) opcode.
    let code = [
        0xa2, 0xfc, // LDX #$FC      @ $C000
        0x9a, //       TXS           @ $C002
        0xa9, 0xdd, // LDA #$DD      @ $C003
        0x8d, 0xfd, 0x01, // STA $01FD  @ $C005  (corruptor ret-lo)
        0xa9, 0xc0, // LDA #$C0      @ $C008
        0x8d, 0xfe, 0x01, // STA $01FE  @ $C00A  (corruptor ret-hi)
        0x60, //       RTS           @ $C00D
    ];
    m.poke(0xc0de, &[0x02]); // JAM at the wild target.

    // Run enough instructions to execute the program AND hit the JAM (the JAM keeps
    // re-fetching, so the instruction cap bounds the storm).
    run_program(&mut m, &code, 40);

    // The CPU must have JAMmed at the wild PC.
    assert!(m.c64_core.is_jammed, "the RTS into bad bytes jammed the CPU");
    assert_eq!(m.c64_core.reg_pc, 0xc0de, "jammed at the wild PC the RTS produced");

    // ── Triage the jammed state ──────────────────────────────────────────────────
    let chain = m.crash_triage(None);

    // (i) names the JAM / wild PC + opcode.
    assert_eq!(chain.crash.pc, 0xc0de, "crash PC = wild PC");
    assert_eq!(chain.crash.opcode, 0x02, "crash opcode = the JAM byte");

    // (ii) identifies the RTS stack pop.
    assert_eq!(chain.transfer.kind, TransferKind::Rts, "wild transfer is an RTS");
    assert_eq!(chain.transfer.at_pc, 0xc00d, "the RTS that derailed is @ $C00D");
    assert_eq!(chain.transfer.pre_sp, Some(0xfc), "SP before the RTS pop");
    assert!(chain.transfer.kind.is_stack_pop());

    // (iii) who_wrote PINS the smashing instructions. Two popped slots: $01FD (ret-lo,
    // written by the STA @ $C005) and $01FE (ret-hi, written by the STA @ $C00A).
    assert!(chain.pinned_corruptor, "the corruptor was pinned");
    assert_eq!(chain.corruptor_slots.len(), 2, "ret-lo + ret-hi slots");

    let lo = &chain.corruptor_slots[0];
    assert_eq!(lo.addr, 0x01fd, "ret-lo slot address");
    assert_eq!(lo.value, 0xdd, "ret-lo popped byte");
    assert_eq!(lo.writer_pc, Some(0xc005), "ret-lo smashed by the STA @ $C005");
    assert_eq!(lo.writer_new, Some(0xdd));

    let hi = &chain.corruptor_slots[1];
    assert_eq!(hi.addr, 0x01fe, "ret-hi slot address");
    assert_eq!(hi.value, 0xc0, "ret-hi popped byte");
    assert_eq!(hi.writer_pc, Some(0xc00a), "ret-hi smashed by the STA @ $C00A");
    assert_eq!(hi.writer_new, Some(0xc0));

    // The compact summary names the wild address + cites a corruptor PC.
    assert!(chain.summary.contains("$C0DE"), "summary names the JAM PC: {}", chain.summary);
    assert!(chain.summary.contains("RTS"), "summary names the RTS: {}", chain.summary);
    assert!(
        chain.summary.contains("$C005") || chain.summary.contains("$C00A"),
        "summary cites a corruptor PC: {}",
        chain.summary
    );
}

#[test]
fn triage_is_honest_when_not_a_stack_smash() {
    // A direct JMP into a wild JAM is NOT a stack smash — the triage must report the
    // JMP and refuse to invent a stack corruptor.
    let mut m = Machine::new();
    m.delta_ring.set_enabled(true);
    m.cpu_history.set_enabled(true);

    //   NOP            EA
    //   JMP $C0DE      4C DE C0   ; direct jump into the wild
    let code = [0xea, 0x4c, 0xde, 0xc0];
    m.poke(0xc0de, &[0x02]); // JAM at the target.
    run_program(&mut m, &code, 40);

    assert!(m.c64_core.is_jammed);
    let chain = m.crash_triage(None);
    assert_eq!(chain.crash.pc, 0xc0de);
    assert_eq!(chain.transfer.kind, TransferKind::JmpAbs, "direct JMP, not a pop");
    assert!(chain.corruptor_slots.is_empty(), "no fabricated stack corruptor");
    assert!(!chain.pinned_corruptor);
}
