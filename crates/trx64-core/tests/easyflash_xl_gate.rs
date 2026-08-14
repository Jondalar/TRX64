//! EasyFlash XL (cart type 232) — internal development vehicle for 4 MB CRT
//! simulation. Not hardware, not a VICE type.
//!
//! Two properties:
//!
//! 1. The bank register is eight bits, so all 256 banks are reachable and each
//!    serves its own bytes. Plain EasyFlash keeps six bits and 64 banks.
//! 2. A sector erase in a high bank must not wipe a low one. The AM29F040B's
//!    `sector_mask: 0x70000` is three bits — 512 KB — and on a 2 MB array it
//!    drops every address bit above A18, so an erase aimed at bank 200 lands on
//!    bank 8. Nothing fails at the time; the damage turns up later in a bank
//!    nobody touched.

use trx64_core::cart::{load_cartridge_from_bytes, BankInfo, CartMapper, MapperType};

fn build_crt(hw: u16, exrom: u8, game: u8, name: &str, chips: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   ");
    v.extend_from_slice(&0x40u32.to_be_bytes());
    v.extend_from_slice(&0x0100u16.to_be_bytes());
    v.extend_from_slice(&hw.to_be_bytes());
    v.push(exrom);
    v.push(game);
    v.extend_from_slice(&[0u8; 6]);
    let mut nm = [0u8; 32];
    let nb = name.as_bytes();
    nm[..nb.len().min(32)].copy_from_slice(&nb[..nb.len().min(32)]);
    v.extend_from_slice(&nm);
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
    }
}

/// Every bank carries a byte derived from its own number, so a read proves WHICH
/// bank answered rather than merely that something answered.
fn xl_cart(banks: u16) -> Vec<u8> {
    let chips: Vec<(u16, u16, Vec<u8>)> = (0..banks)
        .flat_map(|b| {
            let tag = (b & 0xff) as u8;
            [
                (b, 0x8000u16, vec![tag; 0x2000]),
                (b, 0xa000u16, vec![tag ^ 0xff; 0x2000]),
            ]
        })
        .collect();
    build_crt(232, 1, 0, "EFXL", &chips)
}

fn select(m: &mut Box<dyn CartMapper>, bank: u8) {
    m.write(0xde00, bank, &bi(), 0);
}

#[test]
fn the_type_id_is_its_own() {
    let (img, m) = load_cartridge_from_bytes(&xl_cart(1), "EFXL", None).expect("232 builds");
    assert_eq!(img.mapper_type, MapperType::EasyFlashXl);
    assert_eq!(m.mapper_type(), MapperType::EasyFlashXl);
    assert_ne!(MapperType::EasyFlashXl, MapperType::EasyFlash);

    // And 32 is still, exactly, EasyFlash.
    let ef = build_crt(32, 1, 0, "EF", &[(0, 0x8000, vec![0u8; 0x2000])]);
    let (i2, _) = load_cartridge_from_bytes(&ef, "EF", None).expect("32 builds");
    assert_eq!(i2.mapper_type, MapperType::EasyFlash);
}

#[test]
fn all_256_banks_are_reachable() {
    let (_i, mut m) = load_cartridge_from_bytes(&xl_cart(256), "EFXL", None).expect("builds");
    for bank in [0u8, 1, 63, 64, 65, 127, 200, 255] {
        select(&mut m, bank);
        assert_eq!(
            m.active_bank(0x8000),
            bank as u16,
            "bank {bank} must be selectable — six bits would fold it to {}",
            bank & 0x3f
        );
        assert_eq!(
            m.read(0x8000, &bi(), 0),
            Some(bank),
            "bank {bank} must serve its OWN bytes"
        );
    }
}

#[test]
fn plain_easyflash_still_stops_at_64_banks() {
    let ef = build_crt(32, 1, 0, "EF", &[(0, 0x8000, vec![7u8; 0x2000])]);
    let (_i, mut m) = load_cartridge_from_bytes(&ef, "EF", None).expect("builds");
    select(&mut m, 0x40);
    assert_eq!(m.active_bank(0x8000), 0, "$40 & $3f == 0, the hardware behaviour");
    select(&mut m, 0xff);
    assert_eq!(m.active_bank(0x8000), 0x3f, "$ff & $3f == 63");
}

/// The likely regression: reaching for FLASH040B instead of FLASH040B_XL, where
/// the sector mask truncates without any symptom at the time.
#[test]
fn erasing_a_high_bank_does_not_wipe_a_low_one() {
    let (_i, mut m) = load_cartridge_from_bytes(&xl_cart(256), "EFXL", None).expect("builds");

    // 8 and 200 are 192 banks — 1.5 MB — apart, but only three sector-mask bits
    // apart if the mask is left at the 512 KB value.
    let (low, high) = (8u8, 200u8);
    select(&mut m, low);
    assert_eq!(m.read(0x8000, &bi(), 0), Some(low), "bank 8 starts as itself");
    select(&mut m, high);
    assert_eq!(m.read(0x8000, &bi(), 0), Some(high), "bank 200 starts as itself");

    // EasyFlash programs only in ultimax; $DE02 bit 2 + the LED bits.
    m.write(0xde02, 0x86, &bi(), 0);
    select(&mut m, high);
    for (a, v) in [
        (0x8555u16, 0xaau8), (0x82aa, 0x55), (0x8555, 0x80),
        (0x8555, 0xaa), (0x82aa, 0x55), (0x8000, 0x30),
    ] {
        m.write(a, v, &bi(), 0);
    }
    let _ = m.read(0x8000, &bi(), 20_000_000); // let the erase alarm mature

    select(&mut m, low);
    assert_eq!(
        m.read(0x8000, &bi(), 20_000_000),
        Some(low),
        "bank 8 must survive an erase 192 banks away — with a 0x70000 sector mask \
         bank 200 aliases onto bank 8's sector"
    );
}
