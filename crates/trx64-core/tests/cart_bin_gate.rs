//! cart_bin_gate.rs — Spec 790 Slice 1 gate: raw `.bin` cartridge attach with a
//! typed geometry front-end (`cart::parse_bin` / `resolve_cart_type` /
//! `Machine::attach_cart_typed`).
//!
//! These assert the mechanical layer only (the DECIDED slice): the linear
//! `N*bank_unit` split builds byte-identical flash to the equivalent `.crt`, the
//! type resolver maps ids + mnemonics, the smart-attach door dispatches
//! `.crt` vs raw `.bin`, and the size gate rejects bad sizes. The runtime
//! self-configuring autodetect harness (S2) is out of scope here.

use trx64_core::cart::{
    self, load_cartridge_from_bin, mapper_from_image, parse_bin, resolve_cart_type, BankInfo,
    CartType, CrtError, MapperType,
};

// ── helpers ─────────────────────────────────────────────────────────────────

/// The same minimal CRT builder the mapper gate uses — a 0x40-byte header + N CHIP
/// packets — so a `.bin` split can be checked byte-identical to the `.crt` split.
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
    assert_eq!(v.len(), 0x40);
    for (bank, load, data) in chips {
        v.extend_from_slice(b"CHIP");
        let packet_len = 0x10 + data.len() as u32;
        v.extend_from_slice(&packet_len.to_be_bytes());
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

/// A raw 8K-bank ROML `.bin`: `banks` × 0x2000 bytes, bank N's byte 0 = N.
fn raw_8k_bin(banks: usize) -> Vec<u8> {
    let mut v = vec![0u8; banks * 0x2000];
    for n in 0..banks {
        v[n * 0x2000] = n as u8;
    }
    v
}

// ── 790.6: parse_bin builds the same image shape parse_crt does ───────────────

#[test]
fn parse_bin_8k_banks_split_and_read() {
    // 4 banks of 8K, ROML only, as a raw .bin (Megabyter geometry).
    let bin = raw_8k_bin(4);
    let img = parse_bin(&bin, "flash.bin", "flash", MapperType::MegaByter).expect("parse_bin");
    assert_eq!(img.mapper_type, MapperType::MegaByter);
    assert_eq!(img.name, "flash");
    assert_eq!(img.banks.len(), 4);
    for n in 0..4u16 {
        let b = img.banks.get(&n).expect("bank present");
        assert_eq!(b.roml.expect("roml")[0], n as u8);
        assert_eq!(b.romh_a000, None);
    }

    // Build the mapper and confirm bank switching reads the right bank.
    let mut m = mapper_from_image(&img).expect("mapper");
    assert_eq!(m.read(0x8000, &bi(), 0), Some(0)); // bank 0
    m.write(0xde00, 0x03, &bi(), 0); // Megabyter register_00 = bank 3
    assert_eq!(m.read(0x8000, &bi(), 0), Some(3));
}

#[test]
fn parse_bin_16k_banks_split_lo_hi() {
    // 2 banks of 16K (EasyFlash geometry): bank N ROML[0]=$10+N, ROMH@A000[0]=$20+N.
    let mut bin = vec![0xffu8; 2 * 0x4000];
    for n in 0..2usize {
        bin[n * 0x4000] = 0x10 + n as u8; // ROML[0]
        bin[n * 0x4000 + 0x2000] = 0x20 + n as u8; // ROMH@A000[0]
    }
    let img = parse_bin(&bin, "ef.bin", "ef", MapperType::EasyFlash).expect("parse_bin");
    assert_eq!(img.mapper_type, MapperType::EasyFlash);
    assert_eq!(img.banks.len(), 2);
    for n in 0..2u16 {
        let b = img.banks.get(&n).expect("bank present");
        assert_eq!(b.roml.expect("roml")[0], 0x10 + n as u8);
        assert_eq!(b.romh_a000.expect("romh_a000")[0], 0x20 + n as u8);
        assert_eq!(b.romh_e000, None);
    }
}

// ── 790.6: a .bin and the equivalent full .crt build byte-identical flash ──────

#[test]
fn bin_and_crt_build_byte_identical_8k() {
    // 4 banks, ROML only. .crt: 4 CHIP packets at $8000. .bin: 4 × 0x2000 linear.
    let chips: Vec<(u16, u16, Vec<u8>)> = (0..4u16)
        .map(|n| {
            let mut d = vec![0u8; 0x2000];
            d[0] = 0xa0 + n as u8;
            d[0x1fff] = 0xb0 + n as u8;
            (n, 0x8000, d)
        })
        .collect();
    let crt = build_crt(86, 0, 1, "MB", &chips);
    // The equivalent .bin = the same per-bank ROML laid out linearly.
    let mut bin = vec![0u8; 4 * 0x2000];
    for n in 0..4usize {
        bin[n * 0x2000] = 0xa0 + n as u8;
        bin[n * 0x2000 + 0x1fff] = 0xb0 + n as u8;
    }

    let (_ci, mut cm) = cart::load_cartridge_from_bytes(&crt, "MB", None).unwrap();
    let (_bi_img, mut bm) = load_cartridge_from_bin(&bin, "MB", MapperType::MegaByter).unwrap();
    assert_eq!(cm.mapper_type(), bm.mapper_type());
    // Every bank, both window edges, must read identically through both mappers.
    for n in 0..4u8 {
        cm.write(0xde00, n, &bi(), 0);
        bm.write(0xde00, n, &bi(), 0);
        assert_eq!(cm.read(0x8000, &bi(), 0), bm.read(0x8000, &bi(), 0));
        assert_eq!(cm.read(0x9fff, &bi(), 0), bm.read(0x9fff, &bi(), 0));
    }
}

#[test]
fn bin_and_crt_build_byte_identical_16k_easyflash() {
    // 2 banks of 16K. .crt: one $8000 CHIP of 0x4000 per bank (splits to ROML+ROMH).
    let chips: Vec<(u16, u16, Vec<u8>)> = (0..2u16)
        .map(|n| {
            let mut d = vec![0xffu8; 0x4000];
            d[0] = 0x10 + n as u8; // ROML[0]
            d[0x2000] = 0x20 + n as u8; // ROMH@A000[0]
            (n, 0x8000, d)
        })
        .collect();
    let crt = build_crt(32, 1, 0, "EF", &chips);
    let mut bin = vec![0xffu8; 2 * 0x4000];
    for n in 0..2usize {
        bin[n * 0x4000] = 0x10 + n as u8;
        bin[n * 0x4000 + 0x2000] = 0x20 + n as u8;
    }

    let (_ci, mut cm) = cart::load_cartridge_from_bytes(&crt, "EF", None).unwrap();
    let (_bi_img, mut bm) = load_cartridge_from_bin(&bin, "EF", MapperType::EasyFlash).unwrap();
    assert_eq!(cm.mapper_type(), MapperType::EasyFlash);
    assert_eq!(bm.mapper_type(), MapperType::EasyFlash);
    // 16K mode so both windows are live: register_02 = 2 → 16K, then bank-select.
    for n in 0..2u8 {
        cm.write(0xde02, 0x02, &bi(), 0);
        bm.write(0xde02, 0x02, &bi(), 0);
        cm.write(0xde00, n, &bi(), 0);
        bm.write(0xde00, n, &bi(), 0);
        assert_eq!(cm.read(0x8000, &bi(), 0), bm.read(0x8000, &bi(), 0)); // ROML
        assert_eq!(cm.read(0xa000, &bi(), 0), bm.read(0xa000, &bi(), 0)); // ROMH
    }
}

// ── 790.6: size rules ─────────────────────────────────────────────────────────

#[test]
fn parse_bin_bad_size_non_multiple() {
    // 0x2000 + 5 bytes is neither a bank multiple nor a +2 address strip → reject.
    let bin = vec![0u8; 0x2000 + 5];
    match parse_bin(&bin, "x.bin", "x", MapperType::MagicDesk) {
        Err(CrtError::BadBinSize { .. }) => {}
        Err(e) => panic!("expected BadBinSize, got {e:?}"),
        Ok(_) => panic!("expected BadBinSize, got Ok"),
    }
}

#[test]
fn parse_bin_bad_size_over_max_banks() {
    // Generic 8K has max_banks = 1; 2 banks of 8K exceeds it → BadBinSize.
    let bin = vec![0u8; 2 * 0x2000];
    match parse_bin(&bin, "x.bin", "x", MapperType::Normal8k) {
        Err(CrtError::BadBinSize { max_banks, .. }) => assert_eq!(max_banks, 1),
        Err(e) => panic!("expected BadBinSize, got {e:?}"),
        Ok(_) => panic!("expected BadBinSize, got Ok"),
    }
}

#[test]
fn parse_bin_two_byte_load_address_strip() {
    // 8K bank + a 2-byte prepended PRG-style load address → strip then 1 bank.
    let mut bin = vec![0x00u8, 0x80]; // load address $8000 (little-endian)
    bin.extend(std::iter::repeat(0u8).take(0x2000));
    bin[2] = 0x77; // first real ROML byte after the stripped address
    let img = parse_bin(&bin, "x.bin", "x", MapperType::MagicDesk).expect("strip + parse");
    assert_eq!(img.banks.len(), 1);
    assert_eq!(img.banks.get(&0).unwrap().roml.unwrap()[0], 0x77);
    // raw_bytes are the post-strip image (exactly one bank unit).
    assert_eq!(img.raw_bytes.len(), 0x2000);
}

// ── 790.6: resolve_cart_type ──────────────────────────────────────────────────

#[test]
fn resolve_cart_type_ids_and_mnemonics() {
    // Numeric VICE ids (positive + negative generics).
    assert_eq!(resolve_cart_type("32").unwrap(), CartType::Forced(MapperType::EasyFlash));
    assert_eq!(resolve_cart_type("86").unwrap(), CartType::Forced(MapperType::MegaByter));
    assert_eq!(resolve_cart_type("61").unwrap(), CartType::Forced(MapperType::C64MegaCart));
    assert_eq!(resolve_cart_type("-2").unwrap(), CartType::Forced(MapperType::Normal16k));
    assert_eq!(resolve_cart_type("-3").unwrap(), CartType::Forced(MapperType::Normal8k));
    assert_eq!(resolve_cart_type("-6").unwrap(), CartType::Forced(MapperType::Ultimax));
    // Mnemonics (case-insensitive).
    assert_eq!(resolve_cart_type("EasyFlash").unwrap(), CartType::Forced(MapperType::EasyFlash));
    assert_eq!(resolve_cart_type("ef").unwrap(), CartType::Forced(MapperType::EasyFlash));
    assert_eq!(resolve_cart_type("MB").unwrap(), CartType::Forced(MapperType::MegaByter));
    assert_eq!(resolve_cart_type("c64mc").unwrap(), CartType::Forced(MapperType::C64MegaCart));
    assert_eq!(resolve_cart_type("md").unwrap(), CartType::Forced(MapperType::MagicDesk));
    assert_eq!(resolve_cart_type("md16").unwrap(), CartType::Forced(MapperType::MagicDesk16));
    assert_eq!(resolve_cart_type("ocean").unwrap(), CartType::Forced(MapperType::Ocean));
    // Auto/detect sentinel.
    assert_eq!(resolve_cart_type("auto").unwrap(), CartType::Auto);
    assert_eq!(resolve_cart_type("crt").unwrap(), CartType::Auto);
    assert_eq!(resolve_cart_type("0").unwrap(), CartType::Auto);
    // Unknown.
    match resolve_cart_type("nonsense") {
        Err(CrtError::UnknownCartType(s)) => assert_eq!(s, "nonsense"),
        other => panic!("expected UnknownCartType, got {other:?}"),
    }
}

// ── 790.6: smart attach (Machine::attach_cart_typed) ──────────────────────────

#[test]
fn attach_typed_raw_bin_forced_attaches() {
    use trx64_core::Machine;
    let mut m = Machine::new();
    let bin = raw_8k_bin(4);
    let (name, ty) = m
        .attach_cart_typed(&bin, "flash", CartType::Forced(MapperType::MegaByter))
        .expect("forced raw-bin attach");
    assert_eq!(name, "flash");
    assert_eq!(ty, MapperType::MegaByter);
}

#[test]
fn attach_typed_raw_bin_auto_eapi_detects_easyflash() {
    use trx64_core::Machine;
    // One 16K bank; put "eapi" at bank-0 ROMH $1800 (file offset $3800).
    let mut bin = vec![0xffu8; 0x4000];
    bin[0x3800..0x3804].copy_from_slice(b"eapi");
    let mut m = Machine::new();
    let (_name, ty) = m
        .attach_cart_typed(&bin, "ef", CartType::Auto)
        .expect("auto eapi detect");
    assert_eq!(ty, MapperType::EasyFlash);
}

#[test]
fn attach_typed_raw_bin_auto_ambiguous_errors() {
    use trx64_core::Machine;
    // 4 banks of 8K, no eapi, no CBM80 → the S1 structural detect cannot settle.
    let bin = raw_8k_bin(4);
    let mut m = Machine::new();
    match m.attach_cart_typed(&bin, "x", CartType::Auto) {
        Err(CrtError::BinTypeAmbiguous) => {}
        other => panic!("expected BinTypeAmbiguous, got {other:?}"),
    }
}

#[test]
fn attach_typed_crt_still_header_driven() {
    use trx64_core::Machine;
    // A real .crt attaches header-driven under Auto (Magic Desk hw=19).
    let crt = build_crt(19, 0, 1, "MD", &[(0, 0x8000, vec![0xaau8; 0x2000])]);
    let mut m = Machine::new();
    let (_n, ty) = m.attach_cart_typed(&crt, "MD", CartType::Auto).expect("crt auto attach");
    assert_eq!(ty, MapperType::MagicDesk);

    // A Forced type on a .crt is a header OVERRIDE (parse still honours the CHIP
    // walk, but tags the image with the forced mapper).
    let mut m2 = Machine::new();
    let (_n2, ty2) = m2
        .attach_cart_typed(&crt, "MD", CartType::Forced(MapperType::Ocean))
        .expect("crt forced override");
    assert_eq!(ty2, MapperType::Ocean);
}

#[test]
fn attach_from_bytes_wrapper_matches_auto() {
    use trx64_core::Machine;
    // The legacy wrapper == attach_cart_typed(.., Auto): a .crt still attaches.
    let crt = build_crt(19, 0, 1, "MD", &[(0, 0x8000, vec![0x11u8; 0x2000])]);
    let mut m = Machine::new();
    let (_n, ty) = m.attach_cart_from_bytes(&crt, "MD").expect("wrapper attach");
    assert_eq!(ty, MapperType::MagicDesk);
}
