//! Spec 852 gate — the Ultimate Command Interface as U64 hardware.
//!
//! The C64 side runs as real programs through the CPU bus (815's lesson: a gate that pokes
//! the chip proves the wrong door). The firmware side is driven the way `command_intf.cc`
//! drives the block, with its own `HANDSHAKE_*` values (`command_intf.h:45-53`).
//!
//! No ROMs are needed except for the cartridge case, which skips loudly without them.

use std::path::Path;
use std::sync::{Arc, Mutex};
use trx64_core::c64_6510core::{IK_IRQ, INT_SRC_EXPANSION};
use trx64_core::vic::SpeedProfile;
use trx64_core::{Access, ExpansionDevice, Hold, Machine, NullSink, RunStop, Uci, UciEvents};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
/// PAL: 63 cycles × 312 lines.
const FRAME: u64 = 19_656;

// command_intf.h:14-33 — offsets from CMD_IF_BASE.
const SLOT_BASE: u16 = 0x0;
const SLOT_ENABLE: u16 = 0x1;
const HANDSHAKE_OUT: u16 = 0x2;
const STATUSBYTE: u16 = 0x3;
const IRQMASK_SET: u16 = 0x4;
const IRQMASK_CLEAR: u16 = 0x5;
const STATUS_LENGTH: u16 = 0xA;
const IRQMASK: u16 = 0xB;
const RESPONSE_LEN_L: u16 = 0xC;
const RESPONSE_LEN_H: u16 = 0xD;
const COMMAND_LEN_L: u16 = 0xE;
const COMMAND_LEN_H: u16 = 0xF;
const RAM: u16 = 0x800;
const RESPONSE_RAM: u16 = RAM + 896;
const STATUS_RAM: u16 = RAM + 1792;

// command_intf.h:45-53.
const HANDSHAKE_RESET: u8 = 0x87;
const HANDSHAKE_ACCEPT_COMMAND: u8 = 0x01;
const HANDSHAKE_ACCEPT_NEXTDATA: u8 = 0x02;
const HANDSHAKE_VALIDATE_LAST: u8 = 0x10;
const HANDSHAKE_VALIDATE_MORE: u8 = 0x30;
const CMD_NEW_COMMAND: u8 = 0x01;
const CMD_DATA_ACCEPTED: u8 = 0x02;
const CMD_HS_DMA_ACTIVE: u8 = 0x80;

fn u64_machine() -> Machine {
    let mut m = Machine::new();
    m.set_machine_profile(SpeedProfile::U64);
    m
}

fn uci(m: &mut Machine) -> &mut Uci {
    m.uci_mut().expect("the u64 profile carries the block")
}

/// `CommandInterface`'s constructor and `run_task` prologue, plus the enable that the menu
/// (or `unlock_irq`) writes: the block at `$DF1C`.
fn firmware_init(m: &mut Machine) {
    let u = uci(m);
    u.fw_write(SLOT_BASE, 0x47);
    u.fw_write(HANDSHAKE_OUT, HANDSHAKE_RESET);
    u.fw_write(IRQMASK_CLEAR, 7);
    u.fw_write(SLOT_ENABLE, 1);
}

/// `command_interface_irq` (mask what fired) and the task's pick-up of a new command.
fn firmware_take_command(m: &mut Machine) -> Vec<u8> {
    let u = uci(m);
    assert!(u.fw_irq(), "a pushed command interrupts the firmware");
    let flags = u.fw_read(STATUSBYTE) & !u.fw_read(IRQMASK);
    assert_ne!(flags & CMD_NEW_COMMAND, 0);
    u.fw_write(IRQMASK_SET, flags);
    assert!(!u.fw_irq(), "the ISR masks it");
    let len = u16::from(u.fw_read(COMMAND_LEN_L)) | (u16::from(u.fw_read(COMMAND_LEN_H)) << 8);
    let cmd = (0..len).map(|i| u.fw_read(RAM + i)).collect();
    u.fw_write(HANDSHAKE_OUT, HANDSHAKE_ACCEPT_COMMAND);
    cmd
}

/// `copy_result` (`command_intf.cc:188-206`).
fn firmware_copy_result(m: &mut Machine, data: &[u8], status: &[u8], last: bool) {
    let u = uci(m);
    for (i, b) in data.iter().enumerate() {
        u.fw_write(RESPONSE_RAM + i as u16, *b);
    }
    for (i, b) in status.iter().enumerate() {
        u.fw_write(STATUS_RAM + i as u16, *b);
    }
    u.fw_write(RESPONSE_LEN_H, (data.len() >> 8) as u8);
    u.fw_write(RESPONSE_LEN_L, data.len() as u8);
    u.fw_write(STATUS_LENGTH, status.len() as u8);
    u.fw_write(HANDSHAKE_OUT, if last { HANDSHAKE_VALIDATE_LAST } else { HANDSHAKE_VALIDATE_MORE });
}

/// Poke `code` at `origin`, bank `$01`, point the SC core at it and run `instrs` instructions.
fn run_at(m: &mut Machine, origin: u16, port01: u8, code: &[u8], instrs: u64) -> RunStop {
    m.poke(origin, code);
    m.write_full(0x0001, port01);
    m.c64_core.reg_pc = origin;
    run_on(m, instrs)
}

/// Run `instrs` more instructions from wherever the CPU is.
fn run_on(m: &mut Machine, instrs: u64) -> RunStop {
    m.run_for_full_capped(instrs * 64, instrs, &mut NullSink, |_, _, _, _, _, _, _| {})
}

/// `LDA $DF1B` … `LDA $DF1F`, each stored at `$0400+n`.
const PROBE: [u8; 30] = [
    0xad, 0x1b, 0xdf, 0x8d, 0x00, 0x04, 0xad, 0x1c, 0xdf, 0x8d, 0x01, 0x04, 0xad, 0x1d, 0xdf, 0x8d, 0x02,
    0x04, 0xad, 0x1e, 0xdf, 0x8d, 0x03, 0x04, 0xad, 0x1f, 0xdf, 0x8d, 0x04, 0x04,
];

/// $C000: write `01 02 03` to `$DF1D`, PUSH_CMD with `control`, then `JMP *`.
fn push_program(control: u8) -> Vec<u8> {
    vec![
        0xa9, 0x01, 0x8d, 0x1d, 0xdf, 0xa9, 0x02, 0x8d, 0x1d, 0xdf, 0xa9, 0x03, 0x8d, 0x1d, 0xdf, 0xa9, control, 0x8d,
        0x1c, 0xdf, 0x4c, 0x14, 0xc0,
    ]
}

/// $C100, `print_result` from `cmd_test_rom.tas`: the response into `$0400,X` while b7, its
/// count to `$04F0`; the status into `$0410,X` while b6, its count to `$04F1`; then
/// `$02` (NEXT_DATA) to `$DF1C` and `JMP *`.
const READ_PROGRAM: [u8; 48] = [
    0xa2, 0x00, // LDX #$00
    0xad, 0x1c, 0xdf, // LDA $DF1C
    0x10, 0x09, // BPL +9
    0xad, 0x1e, 0xdf, // LDA $DF1E
    0x9d, 0x00, 0x04, // STA $0400,X
    0xe8, // INX
    0xd0, 0xf2, // BNE $C102
    0x8e, 0xf0, 0x04, // STX $04F0
    0xa2, 0x00, // LDX #$00
    0xad, 0x1c, 0xdf, // LDA $DF1C
    0x29, 0x40, // AND #$40
    0xf0, 0x09, // BEQ +9
    0xad, 0x1f, 0xdf, // LDA $DF1F
    0x9d, 0x10, 0x04, // STA $0410,X
    0xe8, // INX
    0xd0, 0xf0, // BNE $C115
    0x8e, 0xf1, 0x04, // STX $04F1
    0xa9, 0x02, // LDA #$02
    0x8d, 0x1c, 0xdf, // STA $DF1C
    0x4c, 0x2d, 0xc1, // JMP $C12D
];

fn bytes(m: &Machine, addr: u16, len: u16) -> Vec<u8> {
    (0..len).map(|i| m.read_full(addr + i)).collect()
}

// ── Disabled ──────────────────────────────────────────────────────────────────────────

#[test]
fn at_power_on_the_block_is_disabled_and_the_window_reads_the_open_bus() {
    let mut plain = Machine::new();
    assert!(plain.uci().is_none(), "the c64 profile has no block");
    assert!(plain.port_profile.is_none());
    run_at(&mut plain, 0xc000, 0x37, &PROBE, 10);

    let mut m = u64_machine();
    let s = m.uci_status().expect("the u64 profile has one");
    assert!(!s.enabled && s.slot_base == 0 && s.state == 0 && !s.error && s.irq_mask == 7);
    assert_eq!((s.command_length, s.response_pointer, s.status_pointer), (0, 896, 1792));
    run_at(&mut m, 0xc000, 0x37, &PROBE, 10);
    assert_eq!(bytes(&m, 0x0400, 5), bytes(&plain, 0x0400, 5), "open bus, byte for byte");
    assert_eq!(m.c64_core.clk, plain.c64_core.clk, "and the same timing");
    assert_eq!(m.uci_status().unwrap().response_pointer, 896, "a disabled block has no read side effects");

    m.set_machine_profile(SpeedProfile::C64);
    assert!(m.uci().is_none(), "leaving the u64 profile removes the block");
}

// ── Identify, and the firmware's register quirks ─────────────────────────────────────

#[test]
fn identify_answers_c9_once_the_firmware_enables_it() {
    let mut m = u64_machine();
    uci(&mut m).fw_write(SLOT_ENABLE, 1);
    uci(&mut m).fw_write(SLOT_BASE, 0x47);
    run_at(&mut m, 0xc000, 0x37, &PROBE, 10);
    assert_eq!(m.read_full(0x0402), 0xc9, "$DF1D identifies");
    assert_eq!(m.read_full(0x0401) & 0x30, 0x00, "$DF1C: state 00");
    assert_eq!(m.read_full(0x0400), 0x00, "$DF1B: the bus id");
    assert_eq!(m.read_full(0x0403), 0x00, "$DF1E: no response");
    assert_eq!(m.read_full(0xdf18), 0xff, "registers 0-2 read $FF");

    // docs/hw/11-drives-iec-periph.md §UltiCommand, from command_protocol.vhd:252-289.
    let u = m.uci().unwrap();
    assert_eq!(u.fw_read(SLOT_BASE), 0x46, "slot base reads back bits 6:1");
    let bounds: Vec<u8> = (0x4..=0x9).map(|o| u.fw_read(o)).collect();
    assert_eq!(bounds, vec![0x00, 0x6f, 0x70, 0xdf, 0xe0, 0xff], "buffer bounds >> 3");
    // The probe's `LDA $DF1E` and `LDA $DF1F` advanced both pointers by one although no
    // byte was available — and these two registers show the pointers, not the lengths.
    assert_eq!(u.fw_read(STATUS_LENGTH), 0x01, "STATUS_LENGTH reads the status pointer's low byte ($701)");
    assert_eq!((u.fw_read(RESPONSE_LEN_L), u.fw_read(RESPONSE_LEN_H)), (0x81, 0x03), "RESPONSE_LEN reads the pointer ($381)");
    assert_eq!(u.fw_read(IRQMASK), 0x07);
    assert_eq!(u.fw_read(0x13), u.fw_read(STATUSBYTE), "the registers repeat every 16 bytes");

    uci(&mut m).fw_write(SLOT_ENABLE, 0x80 | 0x2b);
    assert_eq!(m.read_full(0xdf1b), 0x0b, "bit 7 set: bits 4:0 are the bus id");
    assert!(m.uci_status().unwrap().enabled, "and enable is untouched");
}

// ── A command round trip ─────────────────────────────────────────────────────────────

#[test]
fn a_command_round_trip_driven_like_command_intf_cc() {
    let mut m = u64_machine();
    firmware_init(&mut m);
    run_at(&mut m, 0xc000, 0x37, &push_program(0x01), 9);
    assert_eq!(m.read_full(0xdf1c), 0x11, "state 01 with new-command set");

    assert_eq!(firmware_take_command(&mut m), vec![1, 2, 3], "command length 3, the RAM holds it");
    firmware_copy_result(&mut m, b"OK!", b"00,OK", true);
    uci(&mut m).fw_write(IRQMASK_CLEAR, CMD_NEW_COMMAND);
    assert_eq!(m.read_full(0xdf1c), 0xe0, "b7 and b6 set, state 10");
    for _ in 0..100 {
        m.read_full(0xdf1e);
    }
    assert_eq!(m.uci_status().unwrap().response_pointer, 896, "a peek does not read");

    run_at(&mut m, 0xc100, 0x37, &READ_PROGRAM, 200);
    assert_eq!(bytes(&m, 0x0400, 3), b"OK!".to_vec(), "the response, read while b7");
    assert_eq!(m.read_full(0x04f0), 3);
    assert_eq!(bytes(&m, 0x0410, 5), b"00,OK".to_vec(), "the status, read while b6");
    assert_eq!(m.read_full(0x04f1), 5);
    assert_eq!(m.read_full(0xdf1c), 0x00, "NEXT_DATA on the last data: state 00");
    assert!(!m.uci().unwrap().fw_irq(), "nothing for the firmware to do");
}

#[test]
fn more_data_goes_back_to_the_firmware_with_data_accepted() {
    let mut m = u64_machine();
    firmware_init(&mut m);
    run_at(&mut m, 0xc000, 0x37, &push_program(0x01), 9);
    firmware_take_command(&mut m);
    firmware_copy_result(&mut m, b"AB", b"", false);
    uci(&mut m).fw_write(IRQMASK_CLEAR, CMD_NEW_COMMAND);
    assert_eq!(m.read_full(0xdf1c) & 0x30, 0x30, "state 11: more to come");

    run_at(&mut m, 0xc100, 0x37, &READ_PROGRAM, 200);
    assert_eq!(bytes(&m, 0x0400, 2), b"AB".to_vec());
    assert_eq!(m.read_full(0xdf1c), 0x12, "DATA_ACC: state 01, data accepted");
    let u = uci(&mut m);
    assert!(u.fw_irq(), "the firmware is interrupted for the next block");
    let flags = u.fw_read(STATUSBYTE) & !u.fw_read(IRQMASK);
    assert_eq!(flags & 7, CMD_DATA_ACCEPTED, "the handshake bits the ISR queues");
    u.fw_write(IRQMASK_SET, flags);

    // get_more_data → copy_result, then ACCEPT_NEXTDATA (command_intf.cc:145-154).
    firmware_copy_result(&mut m, b"CD", b"00,OK", true);
    uci(&mut m).fw_write(HANDSHAKE_OUT, HANDSHAKE_ACCEPT_NEXTDATA);
    uci(&mut m).fw_write(IRQMASK_CLEAR, CMD_DATA_ACCEPTED);
    run_at(&mut m, 0xc100, 0x37, &READ_PROGRAM, 200);
    assert_eq!(bytes(&m, 0x0400, 2), b"CD".to_vec());
    assert_eq!(bytes(&m, 0x0410, 5), b"00,OK".to_vec());
    assert_eq!(m.read_full(0xdf1c), 0x00);
}

#[test]
fn a_push_while_busy_sets_error_and_08_clears_it() {
    let mut m = u64_machine();
    firmware_init(&mut m);
    // LDA #$01 / STA $DF1C / STA $DF1C / LDA $DF1C / STA $0400 / LDA #$08 / STA $DF1C /
    // LDA $DF1C / STA $0401 / JMP *
    let prog = [
        0xa9, 0x01, 0x8d, 0x1c, 0xdf, 0x8d, 0x1c, 0xdf, 0xad, 0x1c, 0xdf, 0x8d, 0x00, 0x04, 0xa9, 0x08, 0x8d,
        0x1c, 0xdf, 0xad, 0x1c, 0xdf, 0x8d, 0x01, 0x04, 0x4c, 0x19, 0xc0,
    ];
    run_at(&mut m, 0xc000, 0x37, &prog, 10);
    assert_eq!(m.read_full(0x0400), 0x19, "state 01, ERROR, new command");
    assert_eq!(m.read_full(0x0401), 0x11, "$08 cleared ERROR and pushed nothing");
}

// ── Clamps ───────────────────────────────────────────────────────────────────────────

#[test]
fn nine_hundred_command_bytes_stop_at_895_and_response_reads_stop_at_1791() {
    let mut m = u64_machine();
    firmware_init(&mut m);
    // Y = 0..255 three times, then 0..131, each written to $DF1D: 900 bytes.
    let writes = [
        0xa2, 0x03, 0xa0, 0x00, // LDX #3 / LDY #0
        0x98, 0x8d, 0x1d, 0xdf, 0xc8, 0xd0, 0xf9, // TYA / STA $DF1D / INY / BNE
        0xca, 0xd0, 0xf6, // DEX / BNE
        0x98, 0x8d, 0x1d, 0xdf, 0xc8, 0xc0, 0x84, 0xd0, 0xf7, // TYA / STA / INY / CPY #$84 / BNE
        0x4c, 0x17, 0xc0,
    ];
    run_at(&mut m, 0xc000, 0x37, &writes, 5000);
    let u = m.uci().unwrap();
    assert_eq!((u.fw_read(COMMAND_LEN_L), u.fw_read(COMMAND_LEN_H)), (0x7f, 0x03), "length 895");
    assert_eq!(u.fw_read(RAM + 894), 126, "byte 894 in place");
    assert_eq!(u.fw_read(RAM + 895), 131, "the clamped slot takes every byte after it; the last wins");

    uci(&mut m).fw_write(HANDSHAKE_OUT, HANDSHAKE_ACCEPT_COMMAND);
    let data: Vec<u8> = (0..896u32).map(|i| (i as u8) ^ 0x5a).collect();
    firmware_copy_result(&mut m, &data, b"", true);
    uci(&mut m).fw_write(RAM + 2047 - 256, 0xa5); // RAM[1791], the response buffer's last byte
    // LDA $DF1E 900 times, the last byte to $0400.
    let reads = [
        0xa2, 0x03, 0xa0, 0x00, // LDX #3 / LDY #0
        0xad, 0x1e, 0xdf, 0xc8, 0xd0, 0xfa, // LDA $DF1E / INY / BNE
        0xca, 0xd0, 0xf7, // DEX / BNE
        0xad, 0x1e, 0xdf, 0xc8, 0xc0, 0x84, 0xd0, 0xf8, // LDA / INY / CPY #$84 / BNE
        0x8d, 0x00, 0x04, 0x4c, 0x18, 0xc0,
    ];
    run_at(&mut m, 0xc000, 0x37, &reads, 5000);
    let u = m.uci().unwrap();
    assert_eq!((u.fw_read(RESPONSE_LEN_L), u.fw_read(RESPONSE_LEN_H)), (0xff, 0x06), "the pointer stays at 1791");
    assert_eq!(m.read_full(0x0400), 0xa5, "and keeps answering its byte");
    assert_ne!(m.read_full(0xdf1c) & 0x80, 0, "1791 is still inside a 896-byte response");
}

// ── IRQ ──────────────────────────────────────────────────────────────────────────────

#[test]
fn the_command_irq_is_taken_and_a_handler_read_drops_it_for_good() {
    let mut m = u64_machine();
    firmware_init(&mut m);
    // $C000: SEI / vector $C100 / CLI / LDA #$21 (PUSH, IRQ enable) / STA $DF1C / JMP *
    let main = [
        0x78, 0xa9, 0x00, 0x8d, 0xfe, 0xff, 0xa9, 0xc1, 0x8d, 0xff, 0xff, 0x58, 0xa9, 0x21, 0x8d, 0x1c, 0xdf,
        0x4c, 0x11, 0xc0,
    ];
    // $C100: LDA $DF1E / INC $0400 / RTI
    m.poke(0xc100, &[0xad, 0x1e, 0xdf, 0xee, 0x00, 0x04, 0x40]);
    run_at(&mut m, 0xc000, 0x35, &main, 20);
    assert!(!m.uci_status().unwrap().irq, "no IRQ while the firmware is busy");
    assert_eq!(m.read_full(0x0400), 0);

    firmware_take_command(&mut m);
    firmware_copy_result(&mut m, b"X", b"", true);
    assert_eq!(m.read_full(0xdf1d), 0x49, "$DF1D says $49 while the IRQ is active");
    run_on(&mut m, 400);
    assert_eq!(m.read_full(0x0400), 1, "the handler ran exactly once");
    assert!((0xc011..=0xc013).contains(&m.c64_core.reg_pc), "back in the main loop");
    assert_eq!(m.c64_int.pending_int[INT_SRC_EXPANSION] & IK_IRQ, 0, "the line is released");
    assert_eq!(m.read_full(0xdf1d), 0xc9);
}

// ── Freeze and the $FF00 trigger ─────────────────────────────────────────────────────

/// One frame in 21-cycle slices; the raster lines seen.
fn held_frame(m: &mut Machine) -> std::collections::BTreeSet<u16> {
    let mut lines = std::collections::BTreeSet::new();
    for _ in 0..(FRAME / 21) {
        assert_eq!(m.run_for_full_capped(21, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {}), RunStop::CycleBudget);
        lines.insert(m.vic.raster_line);
    }
    lines
}

#[test]
fn freeze_holds_the_cpu_and_validate_releases_it_at_the_same_pc() {
    let mut m = u64_machine();
    firmware_init(&mut m);
    // LDA #$81 (PUSH, freeze) / STA $DF1C / INC $0400 / JMP *
    run_at(&mut m, 0xc000, 0x37, &[0xa9, 0x81, 0x8d, 0x1c, 0xdf, 0xee, 0x00, 0x04, 0x4c, 0x08, 0xc0], 2);
    assert_eq!(m.c64_core.reg_pc, 0xc005);
    assert_eq!(m.effective_hold(), Some(Hold::Cpu), "freeze is the hold line");
    assert_ne!(m.uci().unwrap().fw_read(HANDSHAKE_OUT) & CMD_HS_DMA_ACTIVE, 0, "the firmware sees it");
    let lines = held_frame(&mut m);
    assert_eq!(lines.len(), 312, "$D012 runs");
    assert_eq!((m.c64_core.reg_pc, m.read_full(0x0400)), (0xc005, 0), "the CPU does not");

    firmware_take_command(&mut m);
    firmware_copy_result(&mut m, b"", b"", true);
    assert_eq!(m.effective_hold(), None);
    run_on(&mut m, 1);
    assert_eq!((m.c64_core.reg_pc, m.read_full(0x0400)), (0xc008, 1), "released where it stopped");
}

#[test]
fn the_ff00_trigger_freezes_on_sta_and_on_inc() {
    // LDA #$41 (PUSH, trigger) / STA $DF1C / INC $0400 / STA $FF00 / INC $0401 / JMP *
    let sta = [
        0xa9, 0x41, 0x8d, 0x1c, 0xdf, 0xee, 0x00, 0x04, 0x8d, 0x00, 0xff, 0xee, 0x01, 0x04, 0x4c, 0x0e, 0xc0,
    ];
    let mut m = u64_machine();
    firmware_init(&mut m);
    run_at(&mut m, 0xc000, 0x37, &sta, 3);
    assert_eq!(m.uci().unwrap().fw_read(HANDSHAKE_OUT), 0x50, "trigger armed, state 01, no freeze");
    // A host write to $FF00 is the firmware's DMA or a debugger, not a C64 cycle.
    m.write_full(0xff00, 0x00);
    assert!(!m.uci_status().unwrap().freeze, "a host write does not trigger");
    run_on(&mut m, 10);
    assert_eq!(m.c64_core.reg_pc, 0xc00b, "held after the STA");
    assert_eq!((m.read_full(0x0400), m.read_full(0x0401)), (1, 0));
    assert_eq!(m.uci().unwrap().fw_read(HANDSHAKE_OUT), 0x90, "freeze set, trigger spent");

    // LDA #$41 / STA $DF1C / INC $FF00 / INC $0401 / JMP *
    let inc = [0xa9, 0x41, 0x8d, 0x1c, 0xdf, 0xee, 0x00, 0xff, 0xee, 0x01, 0x04, 0x4c, 0x0b, 0xc0];
    let mut m = u64_machine();
    firmware_init(&mut m);
    run_at(&mut m, 0xc000, 0x37, &inc, 10);
    assert_eq!(m.c64_core.reg_pc, 0xc008, "INC $FF00 triggers too");
    assert_eq!(m.read_full(0x0401), 0);

    // Without the trigger bit a write to $FF00 is just a write.
    let mut m = u64_machine();
    firmware_init(&mut m);
    let mut plain = sta;
    plain[1] = 0x01;
    run_at(&mut m, 0xc000, 0x37, &plain, 6);
    assert!(!m.uci_status().unwrap().freeze);
    assert_eq!(m.read_full(0x0401), 1);
}

// ── Windows and routing ──────────────────────────────────────────────────────────────

#[test]
fn slot_base_07_answers_at_de1c_and_7f_at_dffc() {
    for (base, control) in [(0x07u8, 0xde1cu16), (0x7f, 0xdffc), (0x47, 0xdf1c)] {
        let [ctl_lo, ctl_hi] = control.to_le_bytes();
        let [id_lo, id_hi] = (control + 1).to_le_bytes();
        // LDA control+1 / STA $0400 / LDA control / STA $0401 / LDA $DF1D / STA $0402 / JMP *
        let prog = [
            0xad, id_lo, id_hi, 0x8d, 0x00, 0x04, 0xad, ctl_lo, ctl_hi, 0x8d, 0x01, 0x04, 0xad, 0x1d, 0xdf, 0x8d,
            0x02, 0x04, 0x4c, 0x12, 0xc0,
        ];
        let mut off = u64_machine();
        run_at(&mut off, 0xc000, 0x37, &prog, 7);

        let mut m = u64_machine();
        uci(&mut m).fw_write(SLOT_ENABLE, 1);
        uci(&mut m).fw_write(SLOT_BASE, base);
        run_at(&mut m, 0xc000, 0x37, &prog, 7);
        assert_eq!(m.read_full(0x0400), 0xc9, "base ${base:02X}: ${:04X} identifies", control + 1);
        assert_eq!(m.read_full(0x0401), 0x00, "base ${base:02X}: ${control:04X} is the control register");
        if control != 0xdf1c {
            assert_eq!(m.read_full(0x0402), off.read_full(0x0402), "base ${base:02X}: $DF1D is open bus");
        }
    }

    // Routing gates each range on its own (Spec 852 §5, C64_BUS_INTERNAL bits 0/1).
    let mut m = u64_machine();
    uci(&mut m).fw_write(SLOT_ENABLE, 1);
    uci(&mut m).fw_write(SLOT_BASE, 0x07);
    m.write_full(0x0001, 0x37);
    uci(&mut m).set_routed(false, true);
    assert_ne!(m.read_full(0xde1d), 0xc9, "an unrouted IO1 range is not on the bus");
    uci(&mut m).set_routed(true, false);
    assert_eq!(m.read_full(0xde1d), 0xc9);
    uci(&mut m).fw_write(SLOT_BASE, 0x47);
    assert_ne!(m.read_full(0xdf1d), 0xc9, "nor an unrouted IO2 range");
}

// ── A cartridge beside it ────────────────────────────────────────────────────────────

#[test]
fn a_cartridge_serving_the_window_loses_the_read_and_both_see_the_write() {
    if !Path::new(ROM_DIR).join("kernal-901227-03.bin").exists() {
        eprintln!("SKIP: ROMs absent ({ROM_DIR})");
        return;
    }
    let mut m = u64_machine();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    m.attach_cart_from_bytes(&easyflash_crt(), "synthetic").expect("attach CRT");
    firmware_init(&mut m);
    // LDA #$06 / STA $DE02 / LDA #$77 / STA $DF1D (EasyFlash RAM and the UCI) / LDA $DF1D /
    // STA $0400 / LDA #$55 / STA $DF10 / LDA $DF10 / STA $0401 / JMP *
    let prog = [
        0xa9, 0x06, 0x8d, 0x02, 0xde, 0xa9, 0x77, 0x8d, 0x1d, 0xdf, 0xad, 0x1d, 0xdf, 0x8d, 0x00, 0x04, 0xa9,
        0x55, 0x8d, 0x10, 0xdf, 0xad, 0x10, 0xdf, 0x8d, 0x01, 0x04, 0x4c, 0x1b, 0x02,
    ];
    run_at(&mut m, 0x0200, 0x37, &prog, 11);
    assert!(m.cartridge.is_some());
    assert_eq!(m.read_full(0x0400), 0xc9, "the UCI's register answer wins over the cartridge's RAM");
    assert_eq!(m.read_full(0x0401), 0x55, "outside the window the cartridge answers");
    let u = m.uci().unwrap();
    assert_eq!((u.fw_read(COMMAND_LEN_L), u.fw_read(RAM)), (1, 0x77), "the UCI took the write");

    uci(&mut m).fw_write(SLOT_ENABLE, 0);
    // LDA $DF1D / STA $0402 / JMP *
    run_at(&mut m, 0x0300, 0x37, &[0xad, 0x1d, 0xdf, 0x8d, 0x02, 0x04, 0x4c, 0x06, 0x03], 3);
    assert_eq!(m.read_full(0x0402), 0x77, "and so did the cartridge");
}

/// A one-bank EasyFlash CRT: header + one 8K CHIP at $8000 (as in `expansion_port_gate`).
fn easyflash_crt() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   ");
    v.extend_from_slice(&0x40u32.to_be_bytes());
    v.extend_from_slice(&0x0100u16.to_be_bytes());
    v.extend_from_slice(&32u16.to_be_bytes()); // EasyFlash
    v.push(1); // EXROM
    v.push(0); // GAME
    v.extend_from_slice(&[0u8; 6]);
    let mut name = [0u8; 32];
    name[..9].copy_from_slice(b"GATE 852 ");
    v.extend_from_slice(&name);
    v.extend_from_slice(b"CHIP");
    v.extend_from_slice(&(0x10u32 + 0x2000).to_be_bytes());
    v.extend_from_slice(&0u16.to_be_bytes()); // ROM
    v.extend_from_slice(&0u16.to_be_bytes()); // bank 0
    v.extend_from_slice(&0x8000u16.to_be_bytes());
    v.extend_from_slice(&0x2000u16.to_be_bytes());
    v.extend(std::iter::repeat_n(0xeau8, 0x2000));
    v
}

// ── Reset, power, restore, clone ─────────────────────────────────────────────────────

#[test]
fn a_c64_reset_keeps_the_block_and_its_error_and_tells_the_firmware() {
    let mut m = u64_machine();
    firmware_init(&mut m);
    // PUSH twice: a pending command and ERROR.
    run_at(&mut m, 0xc000, 0x37, &[0xa9, 0x01, 0x8d, 0x1d, 0xdf, 0xa9, 0x01, 0x8d, 0x1c, 0xdf, 0x8d, 0x1c, 0xdf], 5);
    assert_eq!(uci(&mut m).take_events(), UciEvents::default(), "nothing happened yet");
    let before = m.uci_status().unwrap();
    assert!(before.enabled && before.state == 1 && before.error && before.command_length == 1);

    m.warm_reset();
    let after = m.uci_status().unwrap();
    assert_eq!(
        (after.enabled, after.state, after.error, after.command_length, after.slot_base),
        (true, 1, true, 1, before.slot_base),
        "only the FPGA reset clears the block"
    );
    assert!(after.pending.c64_reset, "the status shows the reset the firmware has not taken");
    assert_eq!(uci(&mut m).take_events(), UciEvents { c64_reset: true, unlock: false });
    assert_eq!(uci(&mut m).take_events(), UciEvents::default(), "reported once");
    m.cold_reset();
    assert!(uci(&mut m).take_events().c64_reset, "a cold reset is a C64 reset too");
}

#[test]
fn a_new_machine_a_profile_change_and_a_restore_give_a_power_on_block() {
    let fresh = |m: &Machine| {
        let s = m.uci_status().unwrap();
        assert!(!s.enabled && s.slot_base == 0 && s.state == 0 && !s.error && s.irq_mask == 7 && !s.freeze);
        assert_eq!((s.command_length, s.response_pointer, s.status_pointer), (0, 896, 1792));
        assert_eq!(s.pending, UciEvents::default());
    };
    let busy = || {
        let mut m = u64_machine();
        firmware_init(&mut m);
        run_at(&mut m, 0xc000, 0x37, &push_program(0x01), 9);
        m.cold_reset();
        m
    };
    fresh(&u64_machine());

    let mut m = busy();
    m.set_machine_profile(SpeedProfile::U64);
    assert_eq!(m.uci_status().unwrap().state, 1, "entering u64 again keeps the block");
    let copy = m.clone();
    assert_eq!(copy.uci_status(), m.uci_status(), "a cloned machine carries a copy");
    m.set_machine_profile(SpeedProfile::C64);
    m.set_machine_profile(SpeedProfile::U64);
    fresh(&m);

    let mut m = busy();
    uci(&mut m).set_routed(true, false);
    let cp = trx64_core::c64re_snapshot::capture_runtime_checkpoint(&m, "", "", None, None, None, None);
    trx64_core::c64re_snapshot::restore_runtime_checkpoint(&mut m, &cp).expect("restore");
    fresh(&m);
    let s = m.uci_status().unwrap();
    assert_eq!((s.routed_io1, s.routed_io2), (true, false), "the host's routing is not block state");
}

// ── Unlock ───────────────────────────────────────────────────────────────────────────

#[test]
fn ab_to_d038_then_cd_to_d036_unlocks_and_nothing_may_come_between() {
    // LDA #$AB / STA $D038 / LDA #$CD / STA $D036
    let unlock = [0xa9, 0xab, 0x8d, 0x38, 0xd0, 0xa9, 0xcd, 0x8d, 0x36, 0xd0];
    let mut m = u64_machine();
    run_at(&mut m, 0xc000, 0x37, &unlock, 4);
    assert_eq!(uci(&mut m).take_events(), UciEvents { c64_reset: false, unlock: true });
    assert!(!m.uci_status().unwrap().enabled, "standalone the event is all: no firmware enables the block");

    let between: [&[u8]; 2] = [
        // LDA #$AB / STA $D038 / STA $D036 / LDA #$CD / STA $D036
        &[0xa9, 0xab, 0x8d, 0x38, 0xd0, 0x8d, 0x36, 0xd0, 0xa9, 0xcd, 0x8d, 0x36, 0xd0],
        // LDA #$AB / STA $D038 / STA $DF1D / LDA #$CD / STA $D036
        &[0xa9, 0xab, 0x8d, 0x38, 0xd0, 0x8d, 0x1d, 0xdf, 0xa9, 0xcd, 0x8d, 0x36, 0xd0],
    ];
    for prog in between {
        let mut m = u64_machine();
        run_at(&mut m, 0xc000, 0x37, prog, 5);
        assert!(!uci(&mut m).take_events().unlock, "a C64 write in between re-arms: {prog:02x?}");
    }

    // A host write is not on the C64 bus and does not count either way.
    let mut m = u64_machine();
    run_at(&mut m, 0xc000, 0x37, &unlock[..5], 2);
    m.write_full(0xd036, 0x00);
    run_at(&mut m, 0xc005, 0x37, &unlock[5..], 2);
    assert!(uci(&mut m).take_events().unlock, "the host write in between is invisible to the sequence");
}

// ── Stretched reads ──────────────────────────────────────────────────────────────────

/// Sees every port read with its stall, beside the UCI.
struct Tap(Arc<Mutex<Vec<Access>>>);

impl ExpansionDevice for Tap {
    fn read(&mut self, a: Access, _cart: Option<u8>) -> Option<u8> {
        self.0.lock().unwrap().push(a);
        None
    }
    fn peek(&self, _addr: u16, _cart: Option<u8>) -> Option<u8> {
        None
    }
    fn write(&mut self, _a: Access, _value: u8) {}
}

#[test]
fn a_read_stretched_by_a_badline_advances_the_pointer_once_per_cycle_on_the_bus() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut m = u64_machine();
    firmware_init(&mut m);
    m.attach_expansion(Box::new(Tap(log.clone())));
    m.write_full(0xd011, 0x1b); // display on: badlines steal cycles
    // $C000: LDA $DF1E / JMP $C000
    m.poke(0xc000, &[0xad, 0x1e, 0xdf, 0x4c, 0x00, 0xc0]);
    m.write_full(0x0001, 0x37);
    m.c64_core.reg_pc = 0xc000;

    let mut stalls = std::collections::BTreeMap::<(u32, u32), usize>::new();
    let start = m.c64_core.clk;
    while m.c64_core.clk - start < 2 * FRAME {
        log.lock().unwrap().clear();
        let before = m.uci_status().unwrap().response_pointer;
        run_on(&mut m, 1); // LDA $DF1E
        let after = m.uci_status().unwrap().response_pointer;
        let reads: Vec<Access> = log.lock().unwrap().iter().copied().filter(|a| a.addr == 0xdf1e).collect();
        assert_eq!(reads.len(), 1);
        let a = reads[0];
        assert_eq!(u32::from(after - before), a.stalled_on_bus + 1, "one advance per cycle on the bus: {a:?}");
        assert!(a.stalled == 0 || a.stalled_on_bus < a.stalled, "never once per stolen cycle: {a:?}");
        *stalls.entry((a.stalled, a.stalled_on_bus)).or_default() += 1;
        if after > 1700 {
            uci(&mut m).fw_write(RESPONSE_LEN_L, 0); // rewinds the response pointer
        }
        run_on(&mut m, 1); // JMP
    }
    assert!(stalls.keys().any(|(s, on_bus)| *s > 1 && *on_bus > 0), "some reads landed on a badline: {stalls:?}");
    eprintln!("(stalled, on the bus) → reads: {stalls:?}");
}
