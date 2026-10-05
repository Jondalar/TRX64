//! The undo ring records the byte a store OVERWRITES, not the ROM above it.
//!
//! Every case plants a sentinel ($5A) in the RAM array through a RAM-direct poke, runs
//! a real store from RAM-resident code at $0200 (visible in every banking, ultimax
//! included), `reverse_step`s it and reads the RAM array back directly (`m.ram`, never
//! the CPU view, which shows the ROM). Needs the system ROMs; says `SKIP` without them.

use trx64_core::{AccessCtx, BusKind, Machine, NullSink, Observer};

const ROM_DIR: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const SENTINEL: u8 = 0x5a;
const CODE: u16 = 0x0200;

fn rom_dir() -> std::path::PathBuf {
    std::env::var_os("TRX64_ROM_DIR").map(Into::into).unwrap_or_else(|| ROM_DIR.into())
}

fn roms_present() -> bool {
    let ok = rom_dir().join("kernal-901227-03.bin").exists();
    if !ok {
        eprintln!("SKIP: ROMs absent ({})", rom_dir().display());
    }
    ok
}

fn machine() -> Machine {
    let mut m = Machine::new();
    m.boot_from_dir(&rom_dir()).expect("boot ROMs");
    m
}

fn build_crt(hw: u16, exrom: u8, game: u8, chips: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   ");
    v.extend_from_slice(&0x40u32.to_be_bytes());
    v.extend_from_slice(&0x0100u16.to_be_bytes());
    v.extend_from_slice(&hw.to_be_bytes());
    v.push(exrom);
    v.push(game);
    v.extend_from_slice(&[0u8; 6]);
    v.extend_from_slice(&[0u8; 32]);
    for (bank, load, data) in chips {
        v.extend_from_slice(b"CHIP");
        v.extend_from_slice(&(0x10 + data.len() as u32).to_be_bytes());
        v.extend_from_slice(&0u16.to_be_bytes());
        v.extend_from_slice(&bank.to_be_bytes());
        v.extend_from_slice(&load.to_be_bytes());
        v.extend_from_slice(&(data.len() as u16).to_be_bytes());
        v.extend_from_slice(data);
    }
    v
}

#[derive(Default)]
struct Writes(Vec<(u16, u8, u8)>); // (addr, value, old)

impl Observer for Writes {
    fn on_instruction(&mut self, _: u16, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u64) {}
    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, _: u16, _: u64, old: u8) {
        if matches!(kind, BusKind::Write) {
            self.0.push((addr, value, old));
        }
    }
    fn on_interrupt(&mut self, _: u16, _: u64) {}
    fn on_access(&mut self, _: BusKind, _: u16, _: u8, _: AccessCtx) -> bool {
        false
    }
}

/// Arm the rings, plant the code at $0200 and run `instrs` instructions.
fn run<O: Observer>(m: &mut Machine, code: &[u8], instrs: u64, obs: &mut O) {
    m.delta_ring.set_enabled(true);
    m.cpu_history.set_enabled(true);
    m.poke(CODE, code);
    m.c64_core.reg_pc = CODE;
    m.run_for_full_capped(instrs * 16, instrs, obs, |_, _, _, _, _, _, _| {});
}

/// Undo the last instruction (the store) and assert the RAM byte is the sentinel again.
fn undo_store_and_check(m: &mut Machine, addr: u16, store_pc: u16) {
    let r = m.reverse_step(1).expect("reverse_step");
    assert_eq!(r.steps_taken, 1);
    assert_eq!(m.c64_core.reg_pc, store_pc, "PC back on the store");
    assert_eq!(m.ram[addr as usize], SENTINEL, "RAM ${addr:04X} after rstep");
}

#[test]
fn basic_rom_in_sta_a000() {
    if !roms_present() {
        return;
    }
    let mut m = machine();
    assert!(m.memconfig.basic, "BASIC is banked in");
    m.poke(0xa000, &[SENTINEL]);
    let rom_byte = m.read_full(0xa000);
    assert_ne!(rom_byte, SENTINEL, "the CPU view shows BASIC, not the RAM beneath");
    let mut w = Writes::default();
    // LDA #$11 ; STA $A000
    run(&mut m, &[0xa9, 0x11, 0x8d, 0x00, 0xa0], 2, &mut w);
    assert_eq!(m.ram[0xa000], 0x11, "the store landed in the RAM under BASIC");
    // The trace/on_bus `old` is the RAM byte the store overwrote, not the BASIC ROM byte.
    assert_eq!(w.0.last().copied(), Some((0xa000, 0x11, SENTINEL)));
    undo_store_and_check(&mut m, 0xa000, CODE + 2);
}

#[test]
fn readonly_cart_rom_at_8000() {
    if !roms_present() {
        return;
    }
    let mut m = machine();
    let crt = build_crt(0, 0, 1, &[(0, 0x8000, vec![0xc3; 0x2000])]); // Normal 8K cart
    m.attach_cart_from_bytes(&crt, "ro8k").expect("attach");
    m.cold_reset();
    assert!(matches!(m.memconfig.bank8, trx64_core::Bank8::CartLo));
    m.poke(0x8000, &[SENTINEL]);
    assert_eq!(m.read_full(0x8000), 0xc3, "the CPU view shows the cart ROM");
    run(&mut m, &[0xa9, 0x11, 0x8d, 0x00, 0x80], 2, &mut NullSink);
    assert_eq!(m.ram[0x8000], 0x11, "a read-only mapper lets the store fall to RAM");
    undo_store_and_check(&mut m, 0x8000, CODE + 2);
}

#[test]
fn kernal_in_sta_e000() {
    if !roms_present() {
        return;
    }
    let mut m = machine();
    assert!(m.memconfig.kernal, "KERNAL is banked in");
    m.poke(0xe000, &[SENTINEL]);
    let mut w = Writes::default();
    run(&mut m, &[0xa9, 0x11, 0x8d, 0x00, 0xe0], 2, &mut w);
    assert_eq!(m.ram[0xe000], 0x11);
    undo_store_and_check(&mut m, 0xe000, CODE + 2);
}

#[test]
fn d000_io_off_char_rom_visible() {
    if !roms_present() {
        return;
    }
    let mut m = machine();
    m.poke(0xd000, &[SENTINEL]);
    // LDA #$33 ; STA $01 (CHAREN=0: char ROM at $D000) ; LDA #$77 ; STA $D000
    run(&mut m, &[0xa9, 0x33, 0x85, 0x01, 0xa9, 0x77, 0x8d, 0x00, 0xd0], 4, &mut NullSink);
    assert!(!m.memconfig.io && m.memconfig.char_rom, "char ROM in, I/O off");
    assert_eq!(m.ram[0xd000], 0x77, "the store landed in the RAM beneath");
    undo_store_and_check(&mut m, 0xd000, CODE + 6);
}

#[test]
fn easyflash_flash_store_is_not_a_ram_store() {
    if !roms_present() {
        return;
    }
    let mut m = machine();
    let chips = [(0u16, 0x8000u16, vec![0xa0u8; 0x2000]), (0, 0xa000, vec![0xb0; 0x2000])];
    let crt = build_crt(32, 1, 0, &chips); // EasyFlash, boots ultimax
    m.attach_cart_from_bytes(&crt, "ef").expect("attach");
    m.cold_reset();
    assert!(m.memconfig.ultimax, "EasyFlash in ultimax: ROML flash takes $8000 stores");
    m.poke(0x8000, &[SENTINEL]);
    run(&mut m, &[0xa9, 0xaa, 0x8d, 0x00, 0x80], 2, &mut NullSink);
    assert_eq!(m.ram[0x8000], SENTINEL, "the flash consumed the store; RAM untouched");
    undo_store_and_check(&mut m, 0x8000, CODE + 2);
}
