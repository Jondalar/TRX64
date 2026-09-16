//! Spec 853 gate — the REU and GeoRAM as port devices.
//!
//! Every case runs as a real 6502 program through the CPU bus. 815's lesson: a gate that
//! pokes the chip proves the wrong door — the first UCI gate passed while the detection a
//! program actually performs failed.
//!
//! No ROMs are needed: every program is poked at `$C000` with `$01 = $37`.

use trx64_core::{Machine, NullSink, RunStop};

// ── the REU's C64-side registers ──────────────────────────────────────────────────
const STATUS: u16 = 0xDF00;
const COMMAND: u16 = 0xDF01;
const BASE_LO: u16 = 0xDF02;
const BASE_HI: u16 = 0xDF03;
const RAM_LO: u16 = 0xDF04;
const RAM_HI: u16 = 0xDF05;
const BANK: u16 = 0xDF06;
const LEN_LO: u16 = 0xDF07;
const LEN_HI: u16 = 0xDF08;
const INTERRUPT: u16 = 0xDF09;
const ADDR_CONTROL: u16 = 0xDF0A;

const TYPE_STASH: u8 = 0x00;
const TYPE_FETCH: u8 = 0x01;
const TYPE_SWAP: u8 = 0x02;
const TYPE_VERIFY: u8 = 0x03;
const FF00_DISABLED: u8 = 0x10;
const AUTOLOAD: u8 = 0x20;
const EXECUTE: u8 = 0x80;

const ST_VERIFY_ERROR: u8 = 0x20;
const ST_END_OF_BLOCK: u8 = 0x40;
const ST_INT_PENDING: u8 = 0x80;

const ORIGIN: u16 = 0xC000;

// ── building 6502 by hand ─────────────────────────────────────────────────────────

fn lda_sta(out: &mut Vec<u8>, value: u8, addr: u16) {
    out.extend_from_slice(&[0xA9, value, 0x8D, addr as u8, (addr >> 8) as u8]);
}

/// The seven address/length registers. Two instructions each.
fn setup(host: u16, reu_addr: u32, len: u16) -> Vec<u8> {
    let mut p = Vec::new();
    lda_sta(&mut p, host as u8, BASE_LO);
    lda_sta(&mut p, (host >> 8) as u8, BASE_HI);
    lda_sta(&mut p, reu_addr as u8, RAM_LO);
    lda_sta(&mut p, (reu_addr >> 8) as u8, RAM_HI);
    lda_sta(&mut p, (reu_addr >> 16) as u8, BANK);
    lda_sta(&mut p, len as u8, LEN_LO);
    lda_sta(&mut p, (len >> 8) as u8, LEN_HI);
    p
}

/// How many instructions a program of `lda_sta` pairs holds, plus the trailing `JMP *`.
fn instrs(prog: &[u8]) -> u64 {
    // Every builder here emits only 5-byte LDA/STA pairs and one 3-byte JMP.
    ((prog.len() - 3) / 5 * 2 + 1) as u64
}

fn jmp_self(prog: &mut Vec<u8>) {
    let here = ORIGIN + prog.len() as u16;
    prog.extend_from_slice(&[0x4C, here as u8, (here >> 8) as u8]);
}

fn run_at(m: &mut Machine, code: &[u8], n: u64) -> RunStop {
    m.poke(ORIGIN, code);
    m.write_full(0x0001, 0x37);
    m.c64_core.reg_pc = ORIGIN;
    m.run_for_full_capped(n * 64, n, &mut NullSink, |_, _, _, _, _, _, _| {})
}

fn machine_with_reu(size_kb: u32) -> Machine {
    let mut m = Machine::new();
    assert!(m.attach_reu(size_kb), "{size_kb} KiB is a real REU size");
    m
}

/// Fill C64 RAM directly — the gate's fixture, not the thing under test.
fn fill(m: &mut Machine, addr: u16, bytes: &[u8]) {
    m.poke(addr, bytes);
}

// ── detection ─────────────────────────────────────────────────────────────────────

#[test]
fn an_empty_port_does_not_answer_where_the_reu_would() {
    // Spec 840: with nothing attached the window is the open bus, not RAM. A probe that
    // writes and reads back must NOT find a device.
    let mut m = Machine::new();
    let mut p = Vec::new();
    lda_sta(&mut p, 0x55, BASE_LO);
    p.extend_from_slice(&[0xAD, BASE_LO as u8, (BASE_LO >> 8) as u8, 0x8D, 0x00, 0x04]);
    jmp_self(&mut p);
    run_at(&mut m, &p, 4);
    assert_ne!(m.read_full(0x0400), 0x55, "an empty port must not echo the write back");
}

#[test]
fn the_registers_answer_and_are_mirrored_up_to_dfff() {
    let mut m = machine_with_reu(512);
    let mut p = Vec::new();
    lda_sta(&mut p, 0x34, BASE_LO);
    // Read it back through a MIRROR: $DF22 is the same register as $DF02.
    p.extend_from_slice(&[0xAD, 0x22, 0xDF, 0x8D, 0x00, 0x04]);
    jmp_self(&mut p);
    run_at(&mut m, &p, 4);
    assert_eq!(m.read_full(0x0400), 0x34);
}

// ── the four transfers ────────────────────────────────────────────────────────────

fn transfer(m: &mut Machine, host: u16, reu_addr: u32, len: u16, cmd: u8) {
    let mut p = setup(host, reu_addr, len);
    lda_sta(&mut p, cmd | EXECUTE | FF00_DISABLED, COMMAND);
    jmp_self(&mut p);
    let n = instrs(&p);
    run_at(m, &p, n);
}

#[test]
fn stash_then_fetch_round_trips_through_the_bus() {
    let mut m = machine_with_reu(512);
    let src: Vec<u8> = (0..64u16).map(|i| (i as u8) ^ 0x5A).collect();
    fill(&mut m, 0x1000, &src);

    transfer(&mut m, 0x1000, 0, 64, TYPE_STASH);
    assert_eq!(&m.reu().unwrap().ram()[0..64], &src[..]);

    fill(&mut m, 0x1000, &[0u8; 64]);
    transfer(&mut m, 0x1000, 0, 64, TYPE_FETCH);
    for (i, want) in src.iter().enumerate() {
        assert_eq!(m.read_full(0x1000 + i as u16), *want, "byte {i}");
    }
}

#[test]
fn swap_exchanges_both_sides() {
    let mut m = machine_with_reu(512);
    fill(&mut m, 0x2000, &[0xAA, 0xBB]);
    m.reu_mut().unwrap().ram_mut()[0..2].copy_from_slice(&[0x11, 0x22]);

    transfer(&mut m, 0x2000, 0, 2, TYPE_SWAP);
    assert_eq!((m.read_full(0x2000), m.read_full(0x2001)), (0x11, 0x22));
    assert_eq!(&m.reu().unwrap().ram()[0..2], &[0xAA, 0xBB]);
}

#[test]
fn verify_sets_end_of_block_when_equal_and_the_error_bit_when_not() {
    let mut m = machine_with_reu(512);
    let data: Vec<u8> = (0..16u8).collect();
    fill(&mut m, 0x3000, &data);
    m.reu_mut().unwrap().ram_mut()[0..16].copy_from_slice(&data);

    transfer(&mut m, 0x3000, 0, 16, TYPE_VERIFY);
    let s = m.reu().unwrap().status();
    assert_eq!(s.status & ST_VERIFY_ERROR, 0, "equal data does not fail");
    assert_ne!(s.status & ST_END_OF_BLOCK, 0);

    // A mismatch in the middle: the error bit, and NO end-of-block.
    let mut m = machine_with_reu(512);
    fill(&mut m, 0x3000, &data);
    m.reu_mut().unwrap().ram_mut()[0..16].copy_from_slice(&data);
    m.reu_mut().unwrap().ram_mut()[4] = 0xFF;
    transfer(&mut m, 0x3000, 0, 16, TYPE_VERIFY);
    let s = m.reu().unwrap().status();
    assert_ne!(s.status & ST_VERIFY_ERROR, 0);
    assert_eq!(s.status & ST_END_OF_BLOCK, 0);
}

// ── the register file's own behaviour, seen from the C64 ───────────────────────────

#[test]
fn the_status_clears_its_top_bits_when_the_cpu_reads_it() {
    let mut m = machine_with_reu(512);
    transfer(&mut m, 0x1000, 0, 4, TYPE_STASH);
    assert_ne!(m.reu().unwrap().status().status & ST_END_OF_BLOCK, 0);

    // Two LDA $DF00, each stored away.
    let mut p = Vec::new();
    p.extend_from_slice(&[0xAD, 0x00, 0xDF, 0x8D, 0x00, 0x04]);
    p.extend_from_slice(&[0xAD, 0x00, 0xDF, 0x8D, 0x01, 0x04]);
    jmp_self(&mut p);
    run_at(&mut m, &p, 5);
    assert_ne!(m.read_full(0x0400) & ST_END_OF_BLOCK, 0, "the first read still sees it");
    assert_eq!(m.read_full(0x0401) & ST_END_OF_BLOCK, 0, "and takes it away");
}

#[test]
fn autoload_restores_the_registers_and_without_it_they_advance() {
    let mut m = machine_with_reu(512);
    let mut p = setup(0x5000, 0, 8);
    lda_sta(&mut p, TYPE_STASH | AUTOLOAD | EXECUTE | FF00_DISABLED, COMMAND);
    jmp_self(&mut p);
    let n = instrs(&p);
    run_at(&mut m, &p, n);
    let s = m.reu().unwrap().status();
    assert_eq!((s.base_computer, s.transfer_length), (0x5000, 8), "autoload puts them back");

    let mut m = machine_with_reu(512);
    transfer(&mut m, 0x5000, 0, 8, TYPE_STASH);
    let s = m.reu().unwrap().status();
    assert_eq!(s.base_computer, 0x5008, "without it the C64 address ends past the block");
    assert_eq!(s.transfer_length, 1);
}

#[test]
fn a_fixed_c64_address_reads_the_same_byte_every_time() {
    let mut m = machine_with_reu(512);
    fill(&mut m, 0x6000, &[0x42]);
    let mut p = setup(0x6000, 0, 4);
    lda_sta(&mut p, 0x80, ADDR_CONTROL); // FIX_C64
    lda_sta(&mut p, TYPE_STASH | EXECUTE | FF00_DISABLED, COMMAND);
    jmp_self(&mut p);
    let n = instrs(&p);
    run_at(&mut m, &p, n);
    assert_eq!(&m.reu().unwrap().ram()[0..4], &[0x42, 0x42, 0x42, 0x42]);
}

// ── the $FF00 trigger ─────────────────────────────────────────────────────────────

/// Arm a transfer with the `$FF00` trigger ENABLED, then let `tail` run.
fn armed_then(m: &mut Machine, tail: &[u8], extra_instrs: u64) {
    let mut p = setup(0x4000, 0, 1);
    lda_sta(&mut p, TYPE_STASH | EXECUTE, COMMAND); // trigger NOT disabled
    let armed_instrs = instrs(&{
        let mut q = p.clone();
        jmp_self(&mut q);
        q
    }) - 1;
    p.extend_from_slice(tail);
    jmp_self(&mut p);
    m.poke(ORIGIN, &p);
    m.write_full(0x0001, 0x37);
    m.c64_core.reg_pc = ORIGIN;
    let n = armed_instrs + extra_instrs;
    m.run_for_full_capped(n * 64, n, &mut NullSink, |_, _, _, _, _, _, _| {});
}

#[test]
fn an_armed_transfer_waits_for_ff00_and_then_runs_once() {
    let mut m = machine_with_reu(512);
    fill(&mut m, 0x4000, &[0x77]);
    // STA $FF00 — one write cycle.
    armed_then(&mut m, &[0xA9, 0x37, 0x8D, 0x00, 0xFF], 2);
    assert_eq!(m.reu().unwrap().ram()[0], 0x77, "the $FF00 write ran it");
    let s = m.reu().unwrap().status();
    assert!(!s.armed_for_ff00 && !s.dma_pending, "and it is not armed any more");
}

#[test]
fn an_rmw_write_to_ff00_runs_exactly_one_transfer() {
    // 850's snoop reports BOTH write cycles of `INC $FF00`. If that fired twice, the
    // second transfer would run with the registers the first one left behind — the REU
    // address would have advanced, so byte 0 would stay and byte 1 would be written.
    let mut m = machine_with_reu(512);
    fill(&mut m, 0x4000, &[0x77]);
    m.reu_mut().unwrap().ram_mut()[1] = 0xEE;
    armed_then(&mut m, &[0xEE, 0x00, 0xFF], 2); // INC $FF00
    assert_eq!(m.reu().unwrap().ram()[0], 0x77);
    assert_eq!(m.reu().unwrap().ram()[1], 0xEE, "a second transfer would have moved this");
}

#[test]
fn with_the_trigger_disabled_an_ff00_write_does_nothing() {
    let mut m = machine_with_reu(512);
    fill(&mut m, 0x4000, &[0x77]);
    // EXECUTE is not set at all: $FF00 has nothing to release.
    let mut p = setup(0x4000, 0, 1);
    lda_sta(&mut p, TYPE_STASH | FF00_DISABLED, COMMAND);
    p.extend_from_slice(&[0xA9, 0x37, 0x8D, 0x00, 0xFF]);
    jmp_self(&mut p);
    let n = instrs(&p);
    run_at(&mut m, &p, n);
    assert_eq!(m.reu().unwrap().ram()[0], 0x00, "nothing was armed, so nothing ran");
}

// ── what a transfer costs, and what it leaves alone ───────────────────────────────

#[test]
fn a_transfer_costs_about_a_cycle_a_byte_and_more_across_a_badline() {
    let mut m = machine_with_reu(512);
    let mut p = setup(0x1000, 0, 256);
    lda_sta(&mut p, TYPE_STASH | EXECUTE | FF00_DISABLED, COMMAND);
    jmp_self(&mut p);
    let n = instrs(&p);
    m.poke(ORIGIN, &p);
    m.write_full(0x0001, 0x37);
    m.c64_core.reg_pc = ORIGIN;
    // Everything up to (not including) the arming STA.
    m.run_for_full_capped((n - 2) * 64, n - 2, &mut NullSink, |_, _, _, _, _, _, _| {});
    let before = m.c64_core.clk;
    // The arming STA, after which the transfer runs at the boundary.
    m.run_for_full_capped(64 * 1024, 1, &mut NullSink, |_, _, _, _, _, _, _| {});
    let cost = m.c64_core.clk - before;
    // 256 bytes, one cycle each, plus the STA's own four.
    assert!(cost >= 256 + 4, "a 256-byte transfer cost {cost} cycles");
    // And the VIC steals on top; it never comes out cheaper than the byte count.
    assert!(cost < 256 + 4 + 2000, "unexpectedly expensive: {cost}");
}

#[test]
fn the_cpu_comes_out_of_a_transfer_untouched() {
    let mut m = machine_with_reu(512);
    let mut p = setup(0x1000, 0, 64);
    lda_sta(&mut p, TYPE_STASH | EXECUTE | FF00_DISABLED, COMMAND);
    jmp_self(&mut p);
    let n = instrs(&p);
    m.poke(ORIGIN, &p);
    m.write_full(0x0001, 0x37);
    m.c64_core.reg_pc = ORIGIN;
    m.run_for_full_capped((n - 2) * 64, n - 2, &mut NullSink, |_, _, _, _, _, _, _| {});
    let (x, y, sp) = (m.c64_core.reg_x, m.c64_core.reg_y, m.c64_core.reg_sp);
    m.run_for_full_capped(64 * 1024, 1, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert_eq!((m.c64_core.reg_x, m.c64_core.reg_y, m.c64_core.reg_sp), (x, y, sp));
    // A holds the command byte the STA wrote, and the transfer did not disturb it.
    assert_eq!(m.c64_core.reg_a, TYPE_STASH | EXECUTE | FF00_DISABLED);
}

#[test]
fn a_stock_machine_is_bit_identical_without_a_device() {
    // The same program, with and without an REU attached: the port must cost nothing.
    let mut p = Vec::new();
    for i in 0..8u8 {
        lda_sta(&mut p, i, 0x0400 + i as u16);
    }
    jmp_self(&mut p);
    let n = instrs(&p);

    let mut plain = Machine::new();
    run_at(&mut plain, &p, n);

    let mut with_reu = machine_with_reu(512);
    run_at(&mut with_reu, &p, n);

    assert_eq!(plain.c64_core.clk, with_reu.c64_core.clk, "an idle REU costs no cycles");
    for i in 0..8u16 {
        assert_eq!(plain.read_full(0x0400 + i), with_reu.read_full(0x0400 + i));
    }
}

// ── the interrupt ─────────────────────────────────────────────────────────────────

#[test]
fn end_of_block_drives_the_irq_line_only_with_both_mask_bits() {
    let mut m = machine_with_reu(512);
    // Interrupts enabled + end-of-block enabled.
    let mut p = setup(0x1000, 0, 4);
    lda_sta(&mut p, 0xC0, INTERRUPT);
    lda_sta(&mut p, TYPE_STASH | EXECUTE | FF00_DISABLED, COMMAND);
    jmp_self(&mut p);
    let n = instrs(&p);
    run_at(&mut m, &p, n);
    assert!(m.expansion_lines().irq, "the REU pulls IRQ on end of block");
    assert_ne!(m.reu().unwrap().status().status & ST_INT_PENDING, 0);

    // Reading the status through the bus drops the line again. The I flag has to be set
    // FIRST, and from outside: the REU's own interrupt is pending at the next boundary,
    // and an `SEI` in the program would never get to run — this gate has no ROMs, so the
    // CPU would vector through $0000 and the probe would never execute. (Whether the CPU
    // TAKES an expansion interrupt, and with what delay, is 850's R5 case.)
    m.c64_core.reg_p |= 0x04;
    let mut q = Vec::new();
    q.extend_from_slice(&[0xAD, 0x00, 0xDF]);
    jmp_self(&mut q);
    m.poke(ORIGIN, &q);
    m.c64_core.reg_pc = ORIGIN;
    m.run_for_full_capped(128, 2, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert_eq!(m.reu().unwrap().status().status & ST_INT_PENDING, 0, "the read cleared it");
    assert!(!m.expansion_lines().irq, "and the read takes it away");
}

#[test]
fn without_the_mask_bits_nothing_is_driven() {
    let mut m = machine_with_reu(512);
    transfer(&mut m, 0x1000, 0, 4, TYPE_STASH);
    assert!(!m.expansion_lines().irq);
}

// ── GeoRAM ────────────────────────────────────────────────────────────────────────

#[test]
fn the_georam_window_reads_and_writes_through_the_bus() {
    let mut m = Machine::new();
    assert!(m.attach_georam(512));
    let mut p = Vec::new();
    lda_sta(&mut p, 3, 0xDFFF); // bank 3
    lda_sta(&mut p, 2, 0xDFFE); // window 2
    lda_sta(&mut p, 0x5A, 0xDE10);
    p.extend_from_slice(&[0xAD, 0x10, 0xDE, 0x8D, 0x00, 0x04]);
    jmp_self(&mut p);
    run_at(&mut m, &p, 8);
    assert_eq!(m.read_full(0x0400), 0x5A);
    let g = m.georam().unwrap();
    assert_eq!((g.bank(), g.window()), (3, 2));
    assert_eq!(g.ram()[3 * 16384 + 2 * 256 + 0x10], 0x5A);
}

#[test]
fn the_georam_registers_are_write_only() {
    let mut m = Machine::new();
    assert!(m.attach_georam(512));
    let mut p = Vec::new();
    lda_sta(&mut p, 0x07, 0xDFFF);
    p.extend_from_slice(&[0xAD, 0xFF, 0xDF, 0x8D, 0x00, 0x04]);
    jmp_self(&mut p);
    run_at(&mut m, &p, 4);
    assert_ne!(m.read_full(0x0400), 0x07, "a register read is never valid");
    assert_eq!(m.georam().unwrap().bank(), 7, "but the write landed");
}

#[test]
fn georam_steals_no_cycles() {
    let mut p = Vec::new();
    lda_sta(&mut p, 0x11, 0xDE00);
    lda_sta(&mut p, 0x22, 0xDE01);
    jmp_self(&mut p);
    let n = instrs(&p);

    let mut plain = Machine::new();
    run_at(&mut plain, &p, n);
    let mut with_geo = Machine::new();
    assert!(with_geo.attach_georam(512));
    run_at(&mut with_geo, &p, n);

    assert_eq!(plain.c64_core.clk, with_geo.c64_core.clk, "no DMA means no cycles");
}

