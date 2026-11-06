//! Protovision TwoMegabyter (CRT hardware type 89): 2 MiB MX29F016 flash, 128 banks of
//! 16 KiB (ROML + ROMH), bank register at $DE00, mode register at $DE02 (IO1, decoded on
//! A1). Scenarios S1-S7 of the cartridge's spec. All 6502 here is our own bytes.

use std::path::PathBuf;
use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, restore_runtime_checkpoint};
use trx64_core::cart::{
    load_cartridge_from_bytes, resolve_cart_type, BankInfo, CartMapper, CartType, CrtError, MapperType,
};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");

fn rom_dir() -> Option<PathBuf> {
    let d = std::env::var("TRX64_ROM_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(ROM_DIR));
    d.join("kernal-901227-03.bin").exists().then_some(d)
}

fn bi() -> BankInfo {
    BankInfo {
        cpu_port_direction: 0x2f,
        cpu_port_value: 0x37,
        basic_visible: true,
        kernal_visible: true,
        io_visible: true,
        char_visible: false,
        cartridge_attached: true,
        cartridge_exrom: None,
        cartridge_game: None,
        phi1: 0xff,
        ram_under: 0,
    }
}

/// A type-89 CRT: header EXROM=0 GAME=0, one CHIP packet per (bank, load address, data).
fn crt(chips: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   ");
    v.extend_from_slice(&0x40u32.to_be_bytes());
    v.extend_from_slice(&0x0100u16.to_be_bytes());
    v.extend_from_slice(&89u16.to_be_bytes());
    v.push(0);
    v.push(0);
    v.extend_from_slice(&[0u8; 6]);
    let mut nm = [0u8; 32];
    nm[..3].copy_from_slice(b"TMB");
    v.extend_from_slice(&nm);
    for (bank, load, data) in chips {
        v.extend_from_slice(b"CHIP");
        v.extend_from_slice(&(0x10 + data.len() as u32).to_be_bytes());
        v.extend_from_slice(&2u16.to_be_bytes());
        v.extend_from_slice(&bank.to_be_bytes());
        v.extend_from_slice(&load.to_be_bytes());
        v.extend_from_slice(&(data.len() as u16).to_be_bytes());
        v.extend_from_slice(data);
    }
    v
}

/// Banks 0, 1, 37, 64, 123..127 present; ROML of bank n = n, ROMH of bank n = n ^ $FF.
fn tagged(banks: &[u16]) -> Vec<u8> {
    let chips: Vec<_> = banks
        .iter()
        .flat_map(|&b| [(b, 0x8000u16, vec![b as u8; 0x2000]), (b, 0xa000u16, vec![!(b as u8); 0x2000])])
        .collect();
    crt(&chips)
}

fn load(bytes: &[u8]) -> Box<dyn CartMapper> {
    load_cartridge_from_bytes(bytes, "tmb", None).expect("type 89 builds").1
}

fn rd(m: &mut Box<dyn CartMapper>, a: u16) -> Option<u8> {
    m.read(a, &bi(), 0)
}
fn wr(m: &mut Box<dyn CartMapper>, a: u16, v: u8) -> bool {
    m.write(a, v, &bi(), 0)
}
fn bank(m: &mut Box<dyn CartMapper>, n: u8) {
    wr(m, 0xde00, n);
}
fn mode(m: &mut Box<dyn CartMapper>, v: u8) {
    wr(m, 0xde02, v);
}

/// AMD unlock + command, our own sequence: AA->$8555, 55->$82AA, cmd->$8555.
fn cmd(m: &mut Box<dyn CartMapper>, c: u8) {
    wr(m, 0x8555, 0xaa);
    wr(m, 0x82aa, 0x55);
    wr(m, 0x8555, c);
}

// ── S1 ───────────────────────────────────────────────────────────────────────────────

#[test]
fn s1_type_89_is_known_and_boots_in_16k_bank_0() {
    let (img, mut m) = load_cartridge_from_bytes(&tagged(&[0, 1]), "tmb", None).expect("89 is accepted");
    assert_eq!(img.mapper_type, MapperType::TwoMegabyter);
    assert_eq!(m.mapper_type(), MapperType::TwoMegabyter);
    let l = m.get_lines();
    assert_eq!((l.exrom, l.game), (0, 0), "16K at power-on");
    assert_eq!(rd(&mut m, 0x8000), Some(0x00));
    assert_eq!(rd(&mut m, 0xa000), Some(0xff), "ROMH of bank 0");
    assert_eq!(rd(&mut m, 0xc000), None);
}

#[test]
fn s1_cart_type_ids_and_mnemonics() {
    for s in ["89", "twomegabyter", "tmb", "TMB"] {
        assert_eq!(resolve_cart_type(s).unwrap(), CartType::Forced(MapperType::TwoMegabyter), "{s}");
    }
}

#[test]
fn s1_a_bank_beyond_127_is_refused_not_dropped() {
    match load_cartridge_from_bytes(&crt(&[(128, 0x8000, vec![1; 0x2000])]), "tmb", None) {
        Err(CrtError::BankOutOfRange { bank: 128, limit: 128 }) => {}
        other => panic!("expected BankOutOfRange, got {:?}", other.map(|_| ())),
    }
}

/// The CPU runs our code out of $8000: KERNAL finds CBM80, jumps through ($8000), the
/// code stores a marker into screen RAM.
#[test]
fn s1_the_cpu_boots_from_roml() {
    let Some(rd_) = rom_dir() else {
        eprintln!("SKIP: ROMs absent");
        return;
    };
    let mut rom = vec![0xffu8; 0x2000];
    rom[0..2].copy_from_slice(&[0x09, 0x80]); // cold start -> $8009
    rom[2..4].copy_from_slice(&[0x09, 0x80]); // warm start
    rom[4..9].copy_from_slice(&[0xc3, 0xc2, 0xcd, 0x38, 0x30]); // CBM80
    // LDA #$5A / STA $0400 / JMP $8010
    rom[9..].iter_mut().for_each(|b| *b = 0xea);
    rom[9..18].copy_from_slice(&[0xa9, 0x5a, 0x8d, 0x00, 0x04, 0x4c, 0x0e, 0x80, 0xea]);
    rom[0x0e..0x11].copy_from_slice(&[0x4c, 0x0e, 0x80]);
    let bytes = crt(&[(0, 0x8000, rom), (0, 0xa000, vec![0x77; 0x2000])]);
    let mut m = Machine::new();
    m.boot_from_dir(&rd_).expect("boot ROMs");
    let (_, ty) = m.attach_cart_from_bytes(&bytes, "tmb").expect("attach");
    assert_eq!(ty, MapperType::TwoMegabyter);
    m.warm_reset();
    m.run_for_full_capped(3_000_000, 400_000, &mut NullSink, |_, _, _, _, _, _, _| {});
    assert_eq!(m.read_full(0x0400), 0x5a, "the code at $8009 ran");
    assert_eq!(m.read_full(0xa000), 0x77, "$A000 reads ROMH of bank 0 in mode 0");
}

// ── S2 ───────────────────────────────────────────────────────────────────────────────

#[test]
fn s2_banking_reaches_every_bank_and_ignores_bit_7() {
    let mut m = load(&tagged(&[0, 1, 64, 127]));
    for n in [0u8, 1, 64, 127] {
        bank(&mut m, n);
        assert_eq!(rd(&mut m, 0x8000), Some(n), "ROML of bank {n}");
        assert_eq!(rd(&mut m, 0x9fff), Some(n));
        assert_eq!(rd(&mut m, 0xa000), Some(!n), "ROMH of bank {n}");
        assert_eq!(rd(&mut m, 0xbfff), Some(!n));
        assert_eq!(m.active_bank(0x8000), n as u16);
    }
    bank(&mut m, 0x80 | 64);
    assert_eq!(rd(&mut m, 0x8000), Some(64), "bit 7 of $DE00 is ignored");
    assert_eq!(rd(&mut m, 0xa000), Some(!64u8));
    bank(&mut m, 0x80);
    assert_eq!(rd(&mut m, 0x8000), Some(0), "$80 is bank 0");
}

#[test]
fn s2_a1_decodes_the_register_and_io_reads_are_open_bus() {
    let mut m = load(&tagged(&[0, 1, 64]));
    assert!(wr(&mut m, 0xde04, 64), "$DE04 (A1 clear) is the bank register");
    assert_eq!(rd(&mut m, 0x8000), Some(64));
    assert!(wr(&mut m, 0xde06, 1), "$DE06 (A1 set) is the mode register");
    let l = m.get_lines();
    assert_eq!((l.exrom, l.game), (1, 0), "Ultimax");
    wr(&mut m, 0xde0e, 0);
    assert_eq!(m.get_lines().exrom, 0);
    for a in [0xde00u16, 0xde02, 0xde04, 0xdeff, 0xdf00, 0xdfff] {
        assert_eq!(rd(&mut m, a), None, "IO read ${a:04X} is open bus");
    }
    assert!(!wr(&mut m, 0xdf00, 5), "IO2 is not a register");
    assert_eq!(rd(&mut m, 0x8000), Some(64), "IO2 write changed nothing");
}

// ── S3 ───────────────────────────────────────────────────────────────────────────────

#[test]
fn s3_modes_drive_the_lines() {
    let mut m = load(&tagged(&[0]));
    for (v, exrom, game) in [(0u8, 0u8, 0u8), (1, 1, 0), (2, 0, 1), (3, 1, 1)] {
        mode(&mut m, v);
        let l = m.get_lines();
        assert_eq!((l.exrom, l.game), (exrom, game), "mode {v}");
    }
    mode(&mut m, 0xfc);
    assert_eq!(m.get_lines().exrom, 0, "only the low two bits of $DE02 count");
}

#[test]
fn s3_modes_through_the_pla() {
    let Some(rd_) = rom_dir() else {
        eprintln!("SKIP: ROMs absent");
        return;
    };
    let mut m = Machine::new();
    m.boot_from_dir(&rd_).expect("boot ROMs");
    // ROML = $11, ROMH = $22
    m.attach_cart_from_bytes(&crt(&[(0, 0x8000, vec![0x11; 0x2000]), (0, 0xa000, vec![0x22; 0x2000])]), "tmb")
        .expect("attach");
    m.write_full(0x0001, 0x37);
    let basic = std::fs::read(rd_.join("basic-901226-01.bin")).unwrap()[0];
    let kernal = std::fs::read(rd_.join("kernal-901227-03.bin")).unwrap()[0];
    assert_ne!(basic, 0x22);
    assert_ne!(kernal, 0x22);
    let probe = |m: &mut Machine| (m.read_full_live(0x8000), m.read_full_live(0xa000), m.read_full_live(0xe000));

    m.write_full(0xde02, 0);
    assert_eq!(probe(&mut m), (0x11, 0x22, kernal), "16K: $8000 and $A000 from the cart");
    m.write_full(0xde02, 1);
    let (lo, mid, hi) = probe(&mut m);
    assert_eq!(lo, 0x11, "Ultimax: ROML");
    assert_eq!(hi, 0x22, "Ultimax: ROMH at $E000");
    assert_ne!(mid, 0x22, "Ultimax: $A000 is open");
    assert_ne!(mid, basic, "Ultimax: $A000 is not BASIC");
    m.write_full(0xde02, 2);
    assert_eq!(probe(&mut m), (0x11, basic, kernal), "8K: only $8000");
    m.write_full(0xde02, 3);
    let (lo, mid, hi) = probe(&mut m);
    assert_ne!(lo, 0x11, "off: RAM at $8000");
    assert_eq!((mid, hi), (basic, kernal), "off: BASIC and KERNAL");
}

// ── S4 ───────────────────────────────────────────────────────────────────────────────

fn programmed_cart() -> Box<dyn CartMapper> {
    // bank 37: both halves $F0 everywhere
    load(&crt(&[(37, 0x8000, vec![0xf0; 0x2000]), (37, 0xa000, vec![0xf0; 0x2000])]))
}

#[test]
fn s4_program_through_roml_and_ultimax_romh() {
    let mut m = programmed_cart();
    bank(&mut m, 37);
    mode(&mut m, 1);
    cmd(&mut m, 0xa0);
    assert!(wr(&mut m, 0x8123, 0x30));
    assert_eq!(rd(&mut m, 0x8123).unwrap() & 0x80, 0x30 & 0x80, "DATA# polling ends at once");
    cmd(&mut m, 0xa0);
    assert!(wr(&mut m, 0xe345, 0x90));
    assert_eq!(rd(&mut m, 0xe345).unwrap() & 0x80, 0x90 & 0x80);
    mode(&mut m, 0);
    assert_eq!(rd(&mut m, 0x8123), Some(0x30), "ROML offset $0123");
    assert_eq!(rd(&mut m, 0xa345), Some(0x90), "ROMH offset $2345 is $A345 in 16K");
    assert_eq!(rd(&mut m, 0x8124), Some(0xf0));
    assert_eq!(rd(&mut m, 0xa123), Some(0xf0), "ROML and ROMH are separate");
    assert!(m.is_writable_dirty());
}

#[test]
fn s4_a_program_cannot_set_bits_back() {
    let mut m = programmed_cart();
    bank(&mut m, 37);
    mode(&mut m, 1);
    cmd(&mut m, 0xa0);
    wr(&mut m, 0x8010, 0x3c); // asks for bits 2 and 3 back: the chip keeps $F0 & $3C
    assert_eq!(rd(&mut m, 0x8010).unwrap() & 0x80, 0x80, "DQ7 is the complement of the data: the program failed");
    wr(&mut m, 0x8000, 0xf0); // reset to read
    mode(&mut m, 0);
    assert_eq!(rd(&mut m, 0x8010), Some(0x30), "$F0 & $3C");
}

#[test]
fn s4_flash_is_written_only_in_ultimax() {
    for md in [0u8, 2, 3] {
        let mut m = programmed_cart();
        bank(&mut m, 37);
        mode(&mut m, md);
        assert!(!wr(&mut m, 0x8555, 0xaa), "mode {md}: the write goes to C64 RAM");
        wr(&mut m, 0x82aa, 0x55);
        wr(&mut m, 0x8555, 0xa0);
        assert!(!wr(&mut m, 0x8123, 0x00));
        assert!(!m.is_writable_dirty(), "mode {md}");
        mode(&mut m, 0);
        assert_eq!(rd(&mut m, 0x8123), Some(0xf0), "mode {md}");
    }
    // Ultimax: only ROML and ROMH at $E000 reach the flash, $A000 does not.
    let mut m = programmed_cart();
    mode(&mut m, 1);
    assert!(!wr(&mut m, 0xa555, 0xaa));
    assert!(wr(&mut m, 0xe555, 0xaa));
}

// ── S5 ───────────────────────────────────────────────────────────────────────────────

fn poll_clk() -> u64 {
    20_000_000
}

#[test]
fn s5_sector_erase_clears_exactly_one_64k_sector() {
    let mut chips = Vec::new();
    for b in [122u16, 123, 124, 125, 126, 127] {
        chips.push((b, 0x8000u16, vec![0x00u8; 0x2000]));
        chips.push((b, 0xa000u16, vec![0x00u8; 0x2000]));
    }
    let mut m = load(&crt(&chips));
    mode(&mut m, 1);
    bank(&mut m, 124);
    cmd(&mut m, 0x80);
    wr(&mut m, 0x8555, 0xaa);
    wr(&mut m, 0x82aa, 0x55);
    wr(&mut m, 0x8000, 0x30); // sector address: bank 124 = sector 31
    assert_eq!(m.read(0x8000, &bi(), poll_clk()).unwrap() & 0x80, 0x80, "erase done: DQ7 = 1");
    let img = m.writable_image(poll_clk()).unwrap();
    assert_eq!(img.len(), 0x20_0000);
    let sector = 31 * 0x10000;
    assert!(img[sector..sector + 0x10000].iter().all(|&b| b == 0xff), "banks 124-127 erased");
    assert!(img[sector - 0x4000..sector].iter().all(|&b| b == 0x00), "bank 123 untouched");
    assert!(img[..122 * 0x4000].iter().all(|&b| b == 0xff), "everything else was already erased");
}

#[test]
fn s5_chip_erase_clears_all_2_mib() {
    let mut m = load(&tagged(&[0, 1, 37, 64, 127]));
    mode(&mut m, 1);
    cmd(&mut m, 0x80);
    cmd(&mut m, 0x10);
    assert_eq!(m.read(0x8000, &bi(), poll_clk()).unwrap() & 0x80, 0x80);
    let img = m.writable_image(poll_clk()).unwrap();
    assert_eq!(img.len(), 0x20_0000);
    assert!(img.iter().all(|&b| b == 0xff));
}

// ── S6 ───────────────────────────────────────────────────────────────────────────────

#[test]
fn s6_autoselect_reports_c2_ad() {
    let mut m = load(&tagged(&[0, 37]));
    bank(&mut m, 37);
    mode(&mut m, 1);
    cmd(&mut m, 0x90);
    assert_eq!(rd(&mut m, 0x8000), Some(0xc2), "manufacturer");
    assert_eq!(rd(&mut m, 0x8001), Some(0xad), "device");
    wr(&mut m, 0x8000, 0xf0); // reset to read
    assert_eq!(rd(&mut m, 0x8000), Some(37));
}

// ── S7 ───────────────────────────────────────────────────────────────────────────────

#[test]
fn s7_a_bank_without_a_packet_is_written_back_into_the_crt() {
    let src = tagged(&[0, 1]);
    let mut m = load(&src);
    bank(&mut m, 99);
    mode(&mut m, 1);
    cmd(&mut m, 0xa0);
    wr(&mut m, 0x8200, 0x5a);
    cmd(&mut m, 0xa0);
    wr(&mut m, 0xe300, 0xa5);
    let saved = m.crt_image(0).expect("crt_image");
    assert_eq!(&saved[0x16..0x18], &89u16.to_be_bytes());
    let mut again = load(&saved);
    bank(&mut again, 99);
    assert_eq!(rd(&mut again, 0x8200), Some(0x5a), "ROML byte of a bank the CRT had no packet for");
    assert_eq!(rd(&mut again, 0xa300), Some(0xa5), "ROMH byte");
    assert_eq!(rd(&mut again, 0x8201), Some(0xff));
    bank(&mut again, 1);
    assert_eq!(rd(&mut again, 0x8000), Some(1), "the packets the CRT had are kept");
    assert_eq!(rd(&mut again, 0xa000), Some(!1u8));
}

#[test]
fn s7_reset_keeps_the_flash_and_restores_bank_0_mode_0() {
    let mut m = programmed_cart();
    bank(&mut m, 37);
    mode(&mut m, 1);
    cmd(&mut m, 0xa0);
    wr(&mut m, 0x8123, 0x00);
    let before = m.writable_image(0).unwrap();
    m.reset();
    assert_eq!(m.active_bank(0x8000), 0);
    let l = m.get_lines();
    assert_eq!((l.exrom, l.game), (0, 0));
    assert_eq!(m.writable_image(0).unwrap(), before);
    bank(&mut m, 37);
    assert_eq!(rd(&mut m, 0x8123), Some(0x00));
}

#[test]
fn s7_a_checkpoint_round_trip_keeps_flash_bank_and_mode() {
    let bytes = tagged(&[0, 37]);
    let mut m = Machine::new();
    if let Some(d) = rom_dir() {
        m.boot_from_dir(&d).expect("boot ROMs");
    }
    m.attach_cart_from_bytes(&bytes, "tmb").expect("attach");
    {
        let c = m.cartridge.as_mut().unwrap();
        c.write(0xde00, 37, &bi(), 0);
        c.write(0xde02, 1, &bi(), 0);
        for (a, v) in [(0x8555u16, 0xaau8), (0x82aa, 0x55), (0x8555, 0xa0), (0x8abc, 0x12)] {
            c.write(a, v, &bi(), 0);
        }
    }
    let clk = m.c64_core.clk;
    let live = m.cartridge.as_mut().unwrap().writable_image(clk).unwrap();
    let crt_bytes = m.cartridge_image.as_ref().unwrap().raw_bytes.clone();
    let cp = capture_runtime_checkpoint(&m, "", "", None, None, Some(&crt_bytes), Some(&live));
    let cp: serde_json::Value = serde_json::from_str(&serde_json::to_string(&cp).unwrap()).unwrap();
    let mut back = Machine::new();
    if let Some(d) = rom_dir() {
        back.boot_from_dir(&d).expect("boot ROMs");
    }
    restore_runtime_checkpoint(&mut back, &cp).expect("restore");
    let c = back.cartridge.as_mut().expect("the cartridge came back");
    assert_eq!(c.mapper_type(), MapperType::TwoMegabyter);
    assert_eq!(c.writable_image(back.c64_core.clk).unwrap(), live, "flash round-trips");
    assert_eq!(c.active_bank(0x8000), 37);
    let l = c.get_lines();
    assert_eq!((l.exrom, l.game), (1, 0));
    assert_eq!(c.peek(0x8abc, &bi()), Some(37 & 0x12));
}
