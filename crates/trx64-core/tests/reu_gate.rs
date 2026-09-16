//! Spec 853 gate — the REU and GeoRAM as port devices.
//!
//! Every case runs as a real 6502 program through the CPU bus. 815's lesson: a gate that
//! pokes the chip proves the wrong door — the first UCI gate passed while the detection a
//! program actually performs failed.
//!
//! No ROMs are needed: every program is poked at `$C000` with `$01 = $37`.

use trx64_core::{Machine, NullSink, RunStop};

// ── the REU's C64-side registers ──────────────────────────────────────────────────
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
    assert_eq!(m.reu().unwrap().ram_slice(0, 64), &src[..]);

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
    m.reu_mut().unwrap().write_ram(0, &[0x11, 0x22]);

    transfer(&mut m, 0x2000, 0, 2, TYPE_SWAP);
    assert_eq!((m.read_full(0x2000), m.read_full(0x2001)), (0x11, 0x22));
    assert_eq!(m.reu().unwrap().ram_slice(0, 2), &[0xAA, 0xBB]);
}

#[test]
fn verify_sets_end_of_block_when_equal_and_the_error_bit_when_not() {
    let mut m = machine_with_reu(512);
    let data: Vec<u8> = (0..16u8).collect();
    fill(&mut m, 0x3000, &data);
    m.reu_mut().unwrap().write_ram(0, &data);

    transfer(&mut m, 0x3000, 0, 16, TYPE_VERIFY);
    let s = m.reu().unwrap().status();
    assert_eq!(s.status & ST_VERIFY_ERROR, 0, "equal data does not fail");
    assert_ne!(s.status & ST_END_OF_BLOCK, 0);

    // A mismatch in the middle: the error bit, and NO end-of-block.
    let mut m = machine_with_reu(512);
    fill(&mut m, 0x3000, &data);
    m.reu_mut().unwrap().write_ram(0, &data);
    m.reu_mut().unwrap().set_ram_byte(4, 0xFF);
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
    assert_eq!(m.reu().unwrap().ram_slice(0, 4), &[0x42, 0x42, 0x42, 0x42]);
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
    assert_eq!(m.reu().unwrap().ram_byte(0), 0x77, "the $FF00 write ran it");
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
    m.reu_mut().unwrap().set_ram_byte(1, 0xEE);
    armed_then(&mut m, &[0xEE, 0x00, 0xFF], 2); // INC $FF00
    assert_eq!(m.reu().unwrap().ram_byte(0), 0x77);
    assert_eq!(m.reu().unwrap().ram_byte(1), 0xEE, "a second transfer would have moved this");
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
    assert_eq!(m.reu().unwrap().ram_byte(0), 0x00, "nothing was armed, so nothing ran");
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
    assert_eq!(g.ram_byte(3 * 16384 + 2 * 256 + 0x10), 0x5A);
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


// ── Spec 853 D6/D7 — the ring and the dump are two different things ────────────────

use trx64_core::c64re_snapshot::{
    capture_runtime_checkpoint, capture_runtime_checkpoint_with, restore_runtime_checkpoint,
    CaptureOpts,
};

fn dump(m: &Machine) -> serde_json::Value {
    capture_runtime_checkpoint(m, "", "", None, None, None, None)
}

fn ring_entry(m: &Machine) -> serde_json::Value {
    capture_runtime_checkpoint_with(
        m,
        "",
        "",
        None,
        None,
        None,
        None,
        CaptureOpts { omit_framebuffer: true, omit_expansion_ram: true },
    )
}

#[test]
fn a_dump_carries_the_reu_ram_and_an_undump_brings_it_back() {
    let mut m = machine_with_reu(512);
    m.reu_mut().unwrap().write_ram(0, &[0xDE, 0xAD, 0xBE, 0xEF]);
    let cp = dump(&m);

    let mut fresh = Machine::new();
    restore_runtime_checkpoint(&mut fresh, &cp).expect("undump");
    assert_eq!(fresh.reu().map(|r| r.size_kb()), Some(512), "the undump re-attached it");
    assert_eq!(fresh.reu().unwrap().ram_slice(0, 4), &[0xDE, 0xAD, 0xBE, 0xEF]);
    assert!(!fresh.expansion_ram_uncovered(), "a dump covers the RAM");
}

#[test]
fn a_ring_entry_carries_the_registers_but_not_the_ram() {
    let mut m = machine_with_reu(512);
    m.reu_mut().unwrap().set_ram_byte(0, 0x11);
    transfer(&mut m, 0x1000, 0x40, 4, TYPE_STASH); // leaves the registers somewhere known
    let base_after = m.reu().unwrap().status().base_computer;
    let cp = ring_entry(&m);

    // The ring entry must not be carrying 512 KiB.
    let ram_node = cp.get("expansion").and_then(|e| e.get("ram")).cloned().unwrap();
    assert!(ram_node.is_null(), "the ring omits the expansion RAM");

    // Restoring it leaves the RAM alone — and says the restore was partial.
    m.reu_mut().unwrap().set_ram_byte(0, 0x99);
    restore_runtime_checkpoint(&mut m, &cp).expect("restore");
    assert_eq!(m.reu().unwrap().ram_byte(0), 0x99, "the RAM was not touched");
    assert_eq!(m.reu().unwrap().status().base_computer, base_after, "the registers came back");
    assert!(m.expansion_ram_uncovered(), "and the machine says the RAM is not covered");
}

#[test]
fn a_checkpoint_without_the_node_does_not_eject_the_device() {
    // The cartridge's rule is "no node means detach". For a device that is not a
    // cartridge that would be an ejection nobody asked for.
    let mut m = machine_with_reu(512);
    m.reu_mut().unwrap().set_ram_byte(0, 0x5A);
    let mut cp = dump(&m);
    cp.as_object_mut().unwrap().remove("expansion"); // a pre-853 .c64re

    restore_runtime_checkpoint(&mut m, &cp).expect("restore");
    assert!(m.reu().is_some(), "still attached");
    assert_eq!(m.reu().unwrap().ram_byte(0), 0x5A, "and untouched");
    assert!(m.expansion_ram_uncovered(), "but the restore covered nothing of it");
}

#[test]
fn a_georam_round_trips_through_a_dump() {
    let mut m = Machine::new();
    assert!(m.attach_georam(512));
    m.georam_mut().unwrap().restore_registers(2, 3);
    m.georam_mut().unwrap().set_ram_byte(3 * 16384 + 2 * 256, 0x7E);
    let cp = dump(&m);

    let mut fresh = Machine::new();
    restore_runtime_checkpoint(&mut fresh, &cp).expect("undump");
    let g = fresh.georam().expect("re-attached");
    assert_eq!((g.window(), g.bank(), g.size_kb()), (2, 3, 512));
    assert_eq!(g.ram_byte(3 * 16384 + 2 * 256), 0x7E);
}

// ── Spec 853 D1 — several devices in one place ────────────────────────────────────

use std::sync::{Arc, Mutex};
use trx64_core::{Access, ExpansionChain, ExpansionDevice, PortLines};

/// A device that answers one address and records everything it is shown.
struct Tap {
    addr: u16,
    answer: Option<u8>,
    log: Arc<Mutex<Vec<(u16, u8)>>>,
    snooped: Arc<Mutex<Vec<(u16, u8)>>>,
}

impl ExpansionDevice for Tap {
    fn read(&mut self, a: Access, _cart: Option<u8>) -> Option<u8> {
        if a.addr == self.addr { self.answer } else { None }
    }
    fn peek(&self, addr: u16, _cart: Option<u8>) -> Option<u8> {
        if addr == self.addr { self.answer } else { None }
    }
    fn write(&mut self, a: Access, value: u8) {
        self.log.lock().unwrap().push((a.addr, value));
    }
    fn snoop_addresses(&self) -> &[u16] {
        &[0xD020]
    }
    fn snoop_write(&mut self, a: Access, value: u8) {
        self.snooped.lock().unwrap().push((a.addr, value));
    }
    fn lines(&self) -> PortLines {
        PortLines::default()
    }
}

fn tap(addr: u16, answer: Option<u8>) -> (Box<Tap>, Arc<Mutex<Vec<(u16, u8)>>>, Arc<Mutex<Vec<(u16, u8)>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let snooped = Arc::new(Mutex::new(Vec::new()));
    (Box::new(Tap { addr, answer, log: log.clone(), snooped: snooped.clone() }), log, snooped)
}

#[test]
fn a_chain_lets_every_device_see_a_write_and_the_first_answer_win() {
    let mut m = Machine::new();
    let (a, a_log, _) = tap(0xDE00, Some(0x11));
    let (b, b_log, _) = tap(0xDE00, Some(0x22));
    m.attach_expansion(Box::new(ExpansionChain::new().with(a).with(b)));

    let mut p = Vec::new();
    lda_sta(&mut p, 0x5A, 0xDE00);
    p.extend_from_slice(&[0xAD, 0x00, 0xDE, 0x8D, 0x00, 0x04]);
    jmp_self(&mut p);
    run_at(&mut m, &p, 4);

    assert_eq!(m.read_full(0x0400), 0x11, "the FIRST device's answer stands");
    assert_eq!(a_log.lock().unwrap().len(), 1, "and both saw the write");
    assert_eq!(b_log.lock().unwrap().len(), 1);
}

#[test]
fn a_chain_unions_the_snoop_addresses_of_its_members() {
    let mut m = machine_with_reu(512);
    // The REU snoops $FF00; the tap snoops $D020. Both must still be reported.
    let reu = m.detach_expansion().expect("attached above");
    let (t, _, snooped) = tap(0xDE00, None);
    m.attach_expansion(Box::new(ExpansionChain::new().with(reu).with(t)));

    fill(&mut m, 0x4000, &[0x77]);
    let mut p = setup(0x4000, 0, 1);
    lda_sta(&mut p, TYPE_STASH | EXECUTE, COMMAND); // armed for $FF00
    lda_sta(&mut p, 0x0E, 0xD020); // the tap's address
    p.extend_from_slice(&[0xA9, 0x37, 0x8D, 0x00, 0xFF]); // STA $FF00
    jmp_self(&mut p);
    let n = instrs(&p);
    run_at(&mut m, &p, n);

    // Both snooped addresses reach BOTH members: 850's `port_snoop` already hands every
    // snooped write to every place, and on the real port every device sees every bus
    // cycle. So the tap is shown the $FF00 write as well as its own $D020 — what matters
    // is that registering an address still WORKS through the chain.
    let seen = snooped.lock().unwrap().clone();
    assert!(seen.iter().any(|(a, v)| *a == 0xD020 && *v == 0x0E), "the tap's own address: {seen:?}");
    assert_eq!(m.reu().unwrap().ram_byte(0), 0x77, "and the REU's $FF00 trigger still ran");
}

#[test]
fn a_transfer_runs_from_inside_a_chain() {
    // dma_pending has to survive the fan-out, or the run loop never asks the REU.
    let mut m = Machine::new();
    let (t, _, _) = tap(0xDE00, None);
    let mut chain = ExpansionChain::new().with(t);
    chain.push(Box::new(trx64_core::Reu::new(512).unwrap()));
    m.attach_expansion(Box::new(chain));

    fill(&mut m, 0x1000, &[0xC5, 0xC6]);
    transfer(&mut m, 0x1000, 0, 2, TYPE_STASH);
    assert_eq!(m.reu().unwrap().ram_slice(0, 2), &[0xC5, 0xC6]);
}

#[test]
fn attaching_also_keeps_what_was_already_there() {
    let mut m = machine_with_reu(512);
    let (t, log, _) = tap(0xDE00, Some(0x99));
    m.attach_expansion_also(t);

    // Both are reachable: the REU through its accessor, the tap by answering.
    assert!(m.reu().is_some(), "the REU was not evicted");
    let mut p = Vec::new();
    p.extend_from_slice(&[0xAD, 0x00, 0xDE, 0x8D, 0x00, 0x04]);
    lda_sta(&mut p, 0x01, 0xDE00);
    jmp_self(&mut p);
    run_at(&mut m, &p, 4);
    assert_eq!(m.read_full(0x0400), 0x99, "the added device answers");
    assert_eq!(log.lock().unwrap().len(), 1);

    // And a third one lands in the SAME chain rather than nesting.
    let (t2, _, _) = tap(0xDE01, Some(0x88));
    m.attach_expansion_also(t2);
    assert!(m.reu().is_some(), "still there after a third");
}

// ── Spec 853 D9/D11 — preload, and looking at the RAM ──────────────────────────────

#[test]
fn an_image_preloads_the_ram_and_is_never_written_back() {
    let mut m = machine_with_reu(512);
    let img: Vec<u8> = (0..=255u8).collect();
    assert_eq!(m.load_expansion_image(&img).unwrap(), 256);
    assert_eq!(m.reu().unwrap().ram_slice(0, 256), &img[..]);
    // The rest is untouched, not padded from the image.
    assert_eq!(m.reu().unwrap().ram_byte(256), 0);

    // An image larger than the device is truncated rather than refused.
    let big = vec![0xAB; 1024 * 1024];
    let mut small = machine_with_reu(128);
    assert_eq!(small.load_expansion_image(&big).unwrap(), 128 * 1024);
}

#[test]
fn loading_an_image_without_a_device_is_an_error_not_a_silent_no_op() {
    let mut m = Machine::new();
    assert!(m.load_expansion_image(&[1, 2, 3]).is_err());
}

#[test]
fn the_ram_window_reads_without_touching_anything() {
    let mut m = machine_with_reu(512);
    m.reu_mut().unwrap().write_ram(0x1000, &[1, 2, 3, 4]);

    assert_eq!(m.expansion_ram_slice(0x1000, 4).unwrap(), vec![1, 2, 3, 4]);
    // Past the end is empty, not a panic and not a wrap.
    assert!(m.expansion_ram_slice(0x0800_0000, 16).unwrap().is_empty());
    // A window that runs off the end is clamped.
    let tail = m.expansion_ram_slice(512 * 1024 - 2, 64).unwrap();
    assert_eq!(tail.len(), 2);

    // And it is a peek: a hundred reads change nothing.
    let before = m.reu().unwrap().status();
    for _ in 0..100 {
        m.expansion_ram_slice(0, 16);
    }
    assert_eq!(m.reu().unwrap().status(), before);
}

#[test]
fn the_window_works_for_a_georam_too() {
    let mut m = Machine::new();
    assert!(m.attach_georam(512));
    m.georam_mut().unwrap().set_ram_byte(0, 0x7F);
    assert_eq!(m.expansion_ram_slice(0, 1).unwrap(), vec![0x7F]);
    assert!(Machine::new().expansion_ram_slice(0, 1).is_none(), "no device, no window");
}

// ── Spec 854 — the expansion RAM the host owns ─────────────────────────────────────

use trx64_core::ExpansionRam;

/// A store that belongs to the "host": the test can read the very bytes the device
/// wrote, which is the whole point — on a U64 those bytes are the firmware's DDR.
#[derive(Clone)]
struct HostRam(Arc<Mutex<Vec<u8>>>);

impl HostRam {
    fn new(bytes: usize) -> Self {
        HostRam(Arc::new(Mutex::new(vec![0; bytes])))
    }
    fn at(&self, off: usize) -> u8 {
        self.0.lock().unwrap()[off]
    }
    fn set(&self, off: usize, v: u8) {
        self.0.lock().unwrap()[off] = v;
    }
}

impl ExpansionRam for HostRam {
    fn len(&self) -> u32 {
        self.0.lock().unwrap().len() as u32
    }
    fn read(&self, off: u32) -> u8 {
        self.0.lock().unwrap().get(off as usize).copied().unwrap_or(0)
    }
    fn write(&mut self, off: u32, value: u8) {
        if let Some(b) = self.0.lock().unwrap().get_mut(off as usize) {
            *b = value;
        }
    }
    // No clone_ram and no is_owned: the bytes belong to the host.
}

#[test]
fn a_transfer_lands_in_the_hosts_own_memory() {
    let host = HostRam::new(512 * 1024);
    let mut m = Machine::new();
    assert!(m.attach_reu_borrowed(512, Box::new(host.clone())));

    fill(&mut m, 0x1000, &[0xDE, 0xAD, 0xBE, 0xEF]);
    transfer(&mut m, 0x1000, 0, 4, TYPE_STASH);

    // The host reads its OWN buffer — not a copy the device kept.
    assert_eq!([host.at(0), host.at(1), host.at(2), host.at(3)], [0xDE, 0xAD, 0xBE, 0xEF]);

    // And what the host writes is what the C64 fetches back.
    host.set(0, 0x11);
    transfer(&mut m, 0x2000, 0, 1, TYPE_FETCH);
    assert_eq!(m.read_full(0x2000), 0x11, "the firmware's preload is what the C64 sees");
}

#[test]
fn nothing_lent_is_the_no_dram_case_not_a_panic() {
    let mut m = Machine::new();
    assert!(m.attach_reu(512));
    m.set_expansion_ram(None);

    fill(&mut m, 0x1000, &[0x5A, 0x5A]);
    transfer(&mut m, 0x1000, 0, 2, TYPE_STASH); // writes go nowhere
    transfer(&mut m, 0x3000, 0, 2, TYPE_FETCH); // reads give the latch

    // The transfer still COMPLETED: end-of-block is set and the registers moved.
    let s = m.reu().unwrap().status();
    assert_ne!(s.status & ST_END_OF_BLOCK, 0);
    assert_eq!(s.base_computer, 0x3002);
    assert_eq!(m.reu().unwrap().ram_len(), 0, "no store, no RAM");
}

#[test]
fn a_store_can_be_swapped_without_disturbing_a_register() {
    let a = HostRam::new(512 * 1024);
    let b = HostRam::new(512 * 1024);
    b.set(0, 0x42);

    let mut m = Machine::new();
    assert!(m.attach_reu_borrowed(512, Box::new(a.clone())));
    let before = m.reu().unwrap().status();

    let returned = m.set_expansion_ram(Some(Box::new(b.clone())));
    assert!(returned.is_some(), "the old store comes back");
    assert_eq!(m.reu().unwrap().status(), before, "and no register moved");

    transfer(&mut m, 0x4000, 0, 1, TYPE_FETCH);
    assert_eq!(m.read_full(0x4000), 0x42, "the NEW store is what the next transfer sees");
}

#[test]
fn the_size_moves_at_runtime_without_dropping_the_contents() {
    let host = HostRam::new(512 * 1024);
    host.set(0x100, 0x77);
    let mut m = Machine::new();
    assert!(m.attach_reu_borrowed(512, Box::new(host.clone())));

    // The firmware writes C64_REU_SIZE: 512 KiB -> 128 KiB.
    assert!(m.reu_mut().unwrap().set_size_kb(128));
    assert_eq!(m.reu().unwrap().status().size_kb, 128);
    assert_eq!(host.at(0x100), 0x77, "the store was not dropped");

    // And the REC now reports 64K chips, as a 1700 does.
    assert_eq!(m.reu().unwrap().status().status & 0x10, 0, "64K chips after the shrink");
    assert!(!m.reu_mut().unwrap().set_size_kb(384), "a size no REU had is refused");
}

#[test]
fn a_georam_over_the_same_store_sees_the_same_bytes() {
    let host = HostRam::new(512 * 1024);
    let mut m = Machine::new();
    assert!(m.attach_reu_borrowed(512, Box::new(host.clone())));
    fill(&mut m, 0x1000, &[0x9A]);
    transfer(&mut m, 0x1000, 0, 1, TYPE_STASH);

    // Same setting, same region on a U64: swap the device, keep the store.
    assert!(m.attach_georam_borrowed(512, Box::new(host.clone())));
    assert_eq!(m.georam().unwrap().ram_byte(0), 0x9A, "one region, two devices");
}

#[test]
fn a_borrowed_store_is_in_no_snapshot() {
    let host = HostRam::new(512 * 1024);
    host.set(0, 0xC3);
    let mut m = Machine::new();
    assert!(m.attach_reu_borrowed(512, Box::new(host.clone())));

    let cp = dump(&m);
    let ram_node = cp.get("expansion").and_then(|e| e.get("ram")).cloned().unwrap();
    assert!(ram_node.is_null(), "the host's memory image is not ours to carry");

    // Restoring it leaves the host's bytes alone and reports the gap.
    host.set(0, 0xD4);
    restore_runtime_checkpoint(&mut m, &cp).expect("restore");
    assert_eq!(host.at(0), 0xD4, "the host's memory was not overwritten");
    assert!(m.expansion_ram_uncovered(), "and the machine says so");
}

#[test]
fn an_owned_store_still_rides_a_dump() {
    // The standalone case must be untouched by all of the above.
    let mut m = machine_with_reu(512);
    m.reu_mut().unwrap().set_ram_byte(0, 0xEE);
    let cp = dump(&m);
    assert!(!cp["expansion"]["ram"].is_null(), "an owned store is still carried");

    let mut fresh = Machine::new();
    restore_runtime_checkpoint(&mut fresh, &cp).expect("undump");
    assert_eq!(fresh.reu().unwrap().ram_byte(0), 0xEE);
    assert!(!fresh.expansion_ram_uncovered());
}
