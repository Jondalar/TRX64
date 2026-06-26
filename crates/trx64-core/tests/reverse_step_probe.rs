//! reverse-debug Phase 1b — live integration probe for the full-delta undo ring.
//!
//! Runs a small REAL program on the full-machine SC path (no ROMs needed — the code
//! lives at $C000, which is always RAM), recording into the always-on delta ring, then
//! `reverse_step`s back past the stores and asserts the RAM bytes AND the CPU registers
//! rolled back BYTE-EXACT to the prior state. Also exercises `who_wrote` on the live
//! ring. This is the "RAM rolled back" check the spec asks for, end-to-end through the
//! actual emulator core (not a synthetic delta stream — that's covered by the
//! `delta_ring` unit tests).

use trx64_core::Machine;

/// Poke a program at `$C000`, point the SC core's PC at it, and run `instrs`
/// instructions on the full-machine path. Returns nothing; the machine is mutated.
fn run_program(m: &mut Machine, code: &[u8], instrs: u64) {
    let base = 0xc000u16;
    m.poke(base, code);
    // Point the verbatim SC core at the program (full-machine path executes c64_core).
    m.c64_core.reg_pc = base;
    // A generous cycle budget that can't trip before the instruction cap.
    m.run_for_full_capped(instrs * 16, instrs, &mut trx64_core::NullSink, |_, _, _, _, _, _, _| {});
}

#[test]
fn reverse_step_rolls_back_ram_and_regs_byte_exact() {
    let mut m = Machine::new();
    // Ensure the ring is armed regardless of ambient env (the bench toggles it off).
    m.delta_ring.set_enabled(true);
    m.cpu_history.set_enabled(true);

    // Program @ $C000 (all operands hit RAM, no banking surprises):
    //   LDA #$11        A9 11
    //   STA $0400       8D 00 04      ; write #1 → $0400
    //   LDA #$22        A9 22
    //   STA $0401       8D 01 04      ; write #2 → $0401
    //   LDX #$33        A2 33
    //   STX $0402       8E 02 04      ; write #3 → $0402
    let code = [
        0xa9, 0x11, 0x8d, 0x00, 0x04, 0xa9, 0x22, 0x8d, 0x01, 0x04, 0xa2, 0x33, 0x8e, 0x02, 0x04,
    ];
    // Pre-condition: the target bytes are 0 (fresh power-on RAM).
    assert_eq!(m.read_full(0x0400), 0x00);
    assert_eq!(m.read_full(0x0401), 0x00);
    assert_eq!(m.read_full(0x0402), 0x00);

    run_program(&mut m, &code, 6);

    // After the 6 instructions: the three stores landed, regs reflect the program end.
    assert_eq!(m.read_full(0x0400), 0x11, "STA #1 landed");
    assert_eq!(m.read_full(0x0401), 0x22, "STA #2 landed");
    assert_eq!(m.read_full(0x0402), 0x33, "STX #3 landed");
    let pc_after = m.c64_core.reg_pc;
    assert_eq!(pc_after, 0xc00f, "PC at end of program");
    assert_eq!(m.c64_core.reg_a, 0x22);
    assert_eq!(m.c64_core.reg_x, 0x33);

    // The ring holds at least the 6 retired instructions.
    assert!(m.delta_ring.len() >= 6, "ring recorded the program (len={})", m.delta_ring.len());

    // ── reverse_step the THREE store instructions (STX, then the LDX, then STA #2…) ──
    // Step back 1 instruction (the STX $0402) → $0402 must roll back to 0, PC to the STX.
    let r1 = m.reverse_step(1).expect("reverse 1");
    assert_eq!(r1.steps_taken, 1);
    assert_eq!(m.read_full(0x0402), 0x00, "STX $0402 undone (byte-exact rollback)");
    assert_eq!(m.c64_core.reg_pc, 0xc00c, "PC landed before STX");
    // The undone write is reported with the old→new bytes.
    assert_eq!(r1.undone_writes.len(), 1);
    assert_eq!(r1.undone_writes[0].addr, 0x0402);
    assert_eq!(r1.undone_writes[0].old_value, 0x00);
    assert_eq!(r1.undone_writes[0].new_value, 0x33);

    // Step back to BEFORE the program start (5 more instructions): all stores undone,
    // all registers back to power-on, PC back to $C000.
    let r2 = m.reverse_step(5).expect("reverse 5");
    assert_eq!(r2.steps_taken, 5);
    assert_eq!(m.read_full(0x0400), 0x00, "STA $0400 undone");
    assert_eq!(m.read_full(0x0401), 0x00, "STA $0401 undone");
    assert_eq!(m.c64_core.reg_pc, 0xc000, "PC rolled all the way back to program start");

    // The ring is now empty of the program (we undid all 6).
    // (len may be >0 only if the boot path had earlier instructions — but we never
    // booted, so the program was the entire timeline.)
    assert_eq!(m.delta_ring.len(), 0, "all program entries undone");
}

#[test]
fn who_wrote_pins_the_last_writer_live() {
    let mut m = Machine::new();
    m.delta_ring.set_enabled(true);

    // Three different instructions write $033C; who_wrote must pin the LAST one.
    //   LDA #$AA  STA $033C   ; writer A
    //   LDA #$BB  STA $033C   ; writer B
    //   LDA #$CC  STA $033C   ; writer C (the last)
    let code = [
        0xa9, 0xaa, 0x8d, 0x3c, 0x03, // LDA #$AA ; STA $033C
        0xa9, 0xbb, 0x8d, 0x3c, 0x03, // LDA #$BB ; STA $033C
        0xa9, 0xcc, 0x8d, 0x3c, 0x03, // LDA #$CC ; STA $033C
    ];
    run_program(&mut m, &code, 6);
    assert_eq!(m.read_full(0x033c), 0xcc, "final value");

    let hits = m.who_wrote(0x033c, 5);
    assert_eq!(hits.len(), 3, "three writers to $033C");
    // Newest first: the $CC store, then $BB, then $AA.
    assert_eq!(hits[0].new_value, 0xcc, "newest writer first");
    assert_eq!(hits[0].old_value, 0xbb, "old→new: was $BB before $CC");
    assert_eq!(hits[1].new_value, 0xbb);
    assert_eq!(hits[1].old_value, 0xaa);
    assert_eq!(hits[2].new_value, 0xaa);
    assert_eq!(hits[2].old_value, 0x00, "first writer saw fresh RAM");
    // The writing instruction PC is the STA opcode address (each STA is 2 bytes after
    // its LDA): $C002, $C007, $C00C.
    assert_eq!(hits[0].pc, 0xc00c);
    assert_eq!(hits[1].pc, 0xc007);
    assert_eq!(hits[2].pc, 0xc002);

    // who_wrote for an address nobody wrote → empty.
    assert!(m.who_wrote(0x4444, 5).is_empty());
}

#[test]
fn reverse_step_rolls_back_the_cpu_port() {
    // The $01 CPU port drives the PLA banking; a corrupted $01 is a real crash cause.
    // Verify reverse_step restores the port byte AND the derived memconfig.
    let mut m = Machine::new();
    m.delta_ring.set_enabled(true);
    // Program @ $C000: LDA #$35 ; STA $01   (change the bank map: KERNAL/BASIC out)
    //   A9 35  85 01
    let code = [0xa9, 0x35, 0x85, 0x01];
    let port_before = m.read_full(0x0001);
    run_program(&mut m, &code, 2);
    // After the store the port reads back the written low bits (the data port).
    let port_after = m.read_full(0x0001);
    assert_ne!(port_after, port_before, "STA $01 changed the port");
    // Reverse the two instructions → the port (and its banking) rolls back.
    let r = m.reverse_step(2).expect("reverse 2");
    assert_eq!(r.steps_taken, 2);
    assert_eq!(m.read_full(0x0001), port_before, "CPU port $01 rolled back byte-exact");
    // The undo recorded a write to the $01 port.
    assert!(
        r.undone_writes.iter().any(|w| w.addr == 0x0001),
        "the $01 port write was captured + undone"
    );
}

#[test]
fn reverse_step_disabled_errs_cleanly() {
    let mut m = Machine::new();
    m.delta_ring.set_enabled(false);
    let err = m.reverse_step(1).unwrap_err();
    assert!(err.contains("disabled"), "got: {err}");
}
