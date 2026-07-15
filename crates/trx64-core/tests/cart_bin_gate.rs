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
    CartMapper, CartType, CrtError, MapperType,
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
fn attach_typed_raw_bin_auto_ambiguous_attaches_self_config_harness() {
    use trx64_core::Machine;
    // 4 banks of 8K, no eapi, no CBM80 → the S1 structural detect cannot settle.
    // Spec 790 S2: instead of `BinTypeAmbiguous`, the Auto path now attaches the
    // runtime self-configuring harness, which resolves the concrete type at runtime.
    let bin = raw_8k_bin(4);
    let mut m = Machine::new();
    let (name, ty) = m
        .attach_cart_typed(&bin, "x", CartType::Auto)
        .expect("ambiguous raw bin now attaches the self-config harness");
    assert_eq!(name, "x");
    // Unlocked until the loader touches a type-specific register at runtime.
    assert_eq!(ty, MapperType::SelfConfig);
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

// ══════════════════════════════════════════════════════════════════════════════
// Spec 790 Slice 2 — the runtime self-configuring cart harness.
//
// A raw multi-bank flash `.bin` (no reliable static type marker) attaches the
// `SelfConfigCartMapper`, which boots the image as a generic $DE00-banked 8K-game
// cart and LOCKS the concrete flash family in-place on the first type-specific
// register access. Two layers below:
//   1. SYNTHETIC (always run): drive each discriminator directly on the harness and
//      assert it locks the right type + post-lock reads match the concrete mapper.
//   2. REAL-DATA (gated on ROMs + local `.bin` fixtures, run with `--ignored`): boot
//      each raw fixture, run, and assert the harness locks a concrete type; two
//      fixtures of the same title in different formats must lock DIFFERENT types.
//      NEUTRAL: the dir is globbed, only `sample #N` is printed — no filenames.
// ══════════════════════════════════════════════════════════════════════════════

use trx64_core::cart::{SelfConfigCartMapper, SELFCONFIG_MAGICDESK_FALLBACK_WRITES};

/// A raw 8K-bank ROML `.bin` whose bank N's byte 0 = `0xA0+N` and byte $1FFF =
/// `0xB0+N` — so a bank-select is verifiable from the served ROML byte.
fn raw_8k_marked(banks: usize) -> Vec<u8> {
    let mut v = vec![0u8; banks * 0x2000];
    for n in 0..banks {
        v[n * 0x2000] = 0xa0 + n as u8;
        v[n * 0x2000 + 0x1fff] = 0xb0 + n as u8;
    }
    v
}

#[test]
fn self_config_pre_lock_serves_roml_and_banks_generically() {
    // Before any discriminator fires the harness is unresolved and serves ROML from
    // the $DE00-selected bank (the generic behaviour that lets a loader boot + run).
    let bin = raw_8k_marked(4);
    let mut h = SelfConfigCartMapper::new(&bin, "x");
    assert_eq!(h.mapper_type(), MapperType::SelfConfig);
    assert!(!h.is_resolved());
    assert_eq!(h.read(0x8000, &bi(), 0), Some(0xa0)); // bank 0
    assert_eq!(h.read(0x9fff, &bi(), 0), Some(0xb0));
    h.write(0xde00, 0x02, &bi(), 0); // select bank 2 (generic bank-low)
    assert_eq!(h.read(0x8000, &bi(), 0), Some(0xa2));
    assert_eq!(h.read(0x9fff, &bi(), 0), Some(0xb2));
    // Still unresolved: a plain $DE00 write is not a type-specific discriminator.
    assert_eq!(h.mapper_type(), MapperType::SelfConfig);
}

#[test]
fn self_config_locks_c64megacart_on_df00_write() {
    let bin = raw_8k_marked(8);
    let mut h = SelfConfigCartMapper::new(&bin, "x");
    h.write(0xde00, 0x03, &bi(), 0); // bank low = 3 (pre-lock)
    let consumed = h.write(0xdf00, 0x00, &bi(), 0); // IO2 control → LOCK C64MegaCart
    assert!(consumed);
    assert_eq!(h.mapper_type(), MapperType::C64MegaCart);
    assert!(h.is_resolved());

    // Post-lock behaviour must match a fresh concrete C64MegaCart driven the same way.
    let (_img, mut cc) =
        load_cartridge_from_bin(&bin, "x", MapperType::C64MegaCart).unwrap();
    cc.write(0xde00, 0x03, &bi(), 0);
    cc.write(0xdf00, 0x00, &bi(), 0);
    for bank in [0u8, 3, 5, 7] {
        h.write(0xde00, bank, &bi(), 0);
        cc.write(0xde00, bank, &bi(), 0);
        assert_eq!(h.read(0x8000, &bi(), 0), cc.read(0x8000, &bi(), 0), "bank {bank} ROML");
        assert_eq!(h.read(0x9fff, &bi(), 0), cc.read(0x9fff, &bi(), 0), "bank {bank} ROML end");
    }
    assert_eq!(h.get_lines().exrom, cc.get_lines().exrom);
    assert_eq!(h.get_lines().game, cc.get_lines().game);
}

#[test]
fn self_config_locks_megabyter_on_de02_write() {
    let bin = raw_8k_marked(8);
    let mut h = SelfConfigCartMapper::new(&bin, "x");
    h.write(0xde00, 0x02, &bi(), 0); // bank low = 2 (pre-lock)
    let consumed = h.write(0xde02, 0x00, &bi(), 0); // $DE02 mode → LOCK Megabyter
    assert!(consumed);
    assert_eq!(h.mapper_type(), MapperType::MegaByter);

    let (_img, mut mb) = load_cartridge_from_bin(&bin, "x", MapperType::MegaByter).unwrap();
    mb.write(0xde00, 0x02, &bi(), 0);
    mb.write(0xde02, 0x00, &bi(), 0);
    for bank in [0u8, 2, 4, 7] {
        h.write(0xde00, bank, &bi(), 0);
        mb.write(0xde00, bank, &bi(), 0);
        assert_eq!(h.read(0x8000, &bi(), 0), mb.read(0x8000, &bi(), 0), "bank {bank}");
    }
    assert_eq!(h.get_lines().game, mb.get_lines().game);
}

#[test]
fn self_config_locks_easyflash_on_de02_when_eapi_present() {
    // A 16K-interleaved image with the eapi signature at bank-0 ROMH $1800 ⇒ the
    // $DE02 mode-register discriminator resolves to EasyFlash (eapi tiebreak), not
    // Megabyter. (In the Auto attach path such an image is already caught by the S1
    // structural eapi detect; this asserts the harness's own tiebreak directly.)
    let mut bin = vec![0xffu8; 2 * 0x4000];
    bin[0x3800..0x3804].copy_from_slice(b"eapi"); // bank-0 ROMH $1800
    let mut h = SelfConfigCartMapper::new(&bin, "ef");
    // eapi ⇒ boot ultimax lines pre-lock.
    assert_eq!(h.get_lines().exrom, 1);
    assert_eq!(h.get_lines().game, 0);
    h.write(0xde02, 0x00, &bi(), 0); // mode register → LOCK EasyFlash (eapi)
    assert_eq!(h.mapper_type(), MapperType::EasyFlash);
}

#[test]
fn self_config_locks_magicdesk_on_de00_only_quiescence() {
    // A cart that only ever banks through $DE00 (no $DF00 / $DE02 / EEPROM read) is
    // the Magic Desk / Ocean residual: after the fallback threshold of $DE00-only
    // writes with no specific discriminator, the harness locks Magic Desk.
    let bin = raw_8k_marked(4);
    let mut h = SelfConfigCartMapper::new(&bin, "md");
    for i in 0..SELFCONFIG_MAGICDESK_FALLBACK_WRITES {
        // Keep bit1 clear (bank-low family) and CS/CLK quiet so only the $DE00
        // fallback path is exercised.
        h.write(0xde00, (i % 4) as u8, &bi(), 0);
    }
    assert_eq!(h.mapper_type(), MapperType::MagicDesk);

    // Post-lock behaviour matches a fresh Magic Desk driven with the same last bank.
    let (_img, mut md) = load_cartridge_from_bin(&bin, "md", MapperType::MagicDesk).unwrap();
    for bank in [0u8, 1, 2, 3] {
        h.write(0xde00, bank, &bi(), 0);
        md.write(0xde00, bank, &bi(), 0);
        assert_eq!(h.read(0x8000, &bi(), 0), md.read(0x8000, &bi(), 0), "bank {bank}");
    }
    // Magic Desk disable bit (bit7) releases the cart on both.
    h.write(0xde00, 0x80, &bi(), 0);
    md.write(0xde00, 0x80, &bi(), 0);
    assert_eq!(h.get_lines().exrom, md.get_lines().exrom);
    assert_eq!(h.get_lines().game, md.get_lines().game);
}

#[test]
fn self_config_specific_discriminator_preempts_magicdesk_fallback() {
    // Even after many $DE00 writes (short of the fallback), the first $DF00 write
    // still locks C64MegaCart — specific-first ordering.
    let bin = raw_8k_marked(8);
    let mut h = SelfConfigCartMapper::new(&bin, "x");
    for _ in 0..(SELFCONFIG_MAGICDESK_FALLBACK_WRITES - 1) {
        h.write(0xde00, 0x01, &bi(), 0);
    }
    assert_eq!(h.mapper_type(), MapperType::SelfConfig); // not yet fallen back
    h.write(0xdf00, 0x00, &bi(), 0);
    assert_eq!(h.mapper_type(), MapperType::C64MegaCart);
}

// ── REAL-DATA lock gate (gated on ROMs + local `.bin` fixtures) ───────────────

const ROM_DIR: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
const COMMERCIAL_BIN_DIR: &str =
    "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/commercial";

#[test]
#[ignore = "needs ROMs + local samples/commercial/*.bin; run with --ignored --nocapture"]
fn self_config_real_bins_lock_distinct_types() {
    use std::path::Path;
    use trx64_core::{Machine, NullSink};

    if !Path::new(ROM_DIR).join("kernal-901227-03.bin").exists() {
        eprintln!("SKIP: ROMs absent");
        return;
    }
    let mut bins: Vec<std::path::PathBuf> = match std::fs::read_dir(COMMERCIAL_BIN_DIR) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "bin").unwrap_or(false))
            .collect(),
        Err(_) => {
            eprintln!("SKIP: {COMMERCIAL_BIN_DIR} absent");
            return;
        }
    };
    if bins.is_empty() {
        eprintln!("SKIP: no *.bin fixtures in {COMMERCIAL_BIN_DIR}");
        return;
    }
    bins.sort(); // deterministic sample ordering; filenames are NOT surfaced

    // Generous window for the loader's init banking (observed discriminator ~1.8M
    // cycles into boot for these fixtures); run in chunks and early-exit on lock.
    const CHUNK: u64 = 200_000;
    const BUDGET: u64 = 8_000_000;

    let mut locked_types: Vec<MapperType> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    for (i, path) in bins.iter().enumerate() {
        let raw = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => {
                failures.push(format!("sample #{i}: unreadable"));
                continue;
            }
        };
        let mut m = Machine::new();
        m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
        let (_name, attach_ty) = m
            .attach_cart_typed(&raw, "sample", CartType::Auto)
            .expect("raw .bin attaches the self-config harness under Auto");
        // A raw flash dump with no static marker must attach UNRESOLVED (the harness).
        assert_eq!(attach_ty, MapperType::SelfConfig, "sample #{i} should attach the harness");
        m.cold_reset();

        let mut sink = NullSink;
        let mut ran: u64 = 0;
        let mut locked: Option<(MapperType, u64)> = None;
        while ran < BUDGET {
            m.run_for_full(CHUNK, &mut sink, |_, _, _, _, _, _, _| {});
            ran += CHUNK;
            let ty = m.cartridge.as_ref().map(|c| c.mapper_type()).unwrap_or(MapperType::SelfConfig);
            if ty != MapperType::SelfConfig {
                locked = Some((ty, ran));
                break;
            }
        }

        match locked {
            Some((ty, cyc)) => {
                eprintln!("sample #{i}: LOCKED {ty:?} after ~{cyc} cycles (final_pc=${:04X})", m.cpu.pc);
                locked_types.push(ty);
            }
            None => {
                // REPORT (per the gate contract) rather than fake a pass.
                let msg = format!(
                    "sample #{i}: did NOT lock within {BUDGET} cycles (banks-late / no register write in window, final_pc=${:04X})",
                    m.cpu.pc
                );
                eprintln!("{msg}");
                failures.push(msg);
            }
        }
    }

    assert!(
        failures.is_empty(),
        "self-config harness failed to lock on {} fixture(s): {:?}",
        failures.len(),
        failures
    );
    // Each fixture locked a concrete flash family.
    for (i, ty) in locked_types.iter().enumerate() {
        assert_ne!(*ty, MapperType::SelfConfig, "sample #{i} unresolved");
    }
    // Two fixtures of the SAME title in DIFFERENT cart formats must lock DIFFERENT
    // types (the whole point: the harness discriminates the formats by the register
    // each loader writes).
    if locked_types.len() >= 2 {
        let mut uniq = locked_types.clone();
        uniq.sort_by_key(|t| format!("{t:?}"));
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            locked_types.len(),
            "expected each fixture to lock a DISTINCT type, got {locked_types:?}"
        );
        eprintln!("PASS: {} fixtures locked distinct types {:?}", locked_types.len(), locked_types);
    }
}

// Real EasyFlash cart, re-linearized from a .crt into a raw 16K-per-bank .bin
// (ROML 8K ++ ROMH 8K) and attached under Auto → the harness must auto-lock
// EasyFlash. Neutral + env-gated (no fixture identity in code): set
// C64RE_EF_CRT_SAMPLE to any EasyFlash .crt. Run: `--ignored --nocapture`.
#[test]
#[ignore = "needs C64RE_EF_CRT_SAMPLE=<an EasyFlash .crt> + ROMs; run with --ignored"]
fn self_config_locks_easyflash_on_real_ef_crt() {
    use std::path::Path;
    use trx64_core::cart::parse_crt;
    use trx64_core::{Machine, NullSink};

    let crt_path = match std::env::var("C64RE_EF_CRT_SAMPLE") {
        Ok(p) => p,
        Err(_) => {
            eprintln!("SKIP: C64RE_EF_CRT_SAMPLE unset");
            return;
        }
    };
    if !Path::new(ROM_DIR).join("kernal-901227-03.bin").exists() {
        eprintln!("SKIP: ROMs absent");
        return;
    }
    let crt = match std::fs::read(&crt_path) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("SKIP: crt unreadable");
            return;
        }
    };
    let img = parse_crt(&crt, "efsample", None).expect("parse .crt");
    assert_eq!(img.mapper_type, MapperType::EasyFlash, "sample is not an EasyFlash .crt");

    // Re-linearize to the raw EF .bin layout: 16K per bank = ROML(8K) ++ ROMH(8K).
    let max_bank = *img.banks.keys().max().unwrap() as usize;
    let mut bin: Vec<u8> = Vec::with_capacity((max_bank + 1) * 0x4000);
    for n in 0..=max_bank {
        let b = img.banks.get(&(n as u16));
        let roml = b.and_then(|b| b.roml).unwrap_or([0xff; 0x2000]);
        let romh = b.and_then(|b| b.romh_a000.or(b.romh_e000)).unwrap_or([0xff; 0x2000]);
        bin.extend_from_slice(&roml);
        bin.extend_from_slice(&romh);
    }

    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let (_n, attach_ty) = m
        .attach_cart_typed(&bin, "efsample", CartType::Auto)
        .expect("attach EF .bin via Auto");

    // EF is detectable STRUCTURALLY (the eapi signature is a definitive static
    // marker) — so the Auto path may resolve EasyFlash immediately at attach,
    // without running. If it does not (attaches the harness), run until the
    // loader's first $DE02 write locks it. Either path must end at EasyFlash.
    if attach_ty != MapperType::SelfConfig {
        eprintln!("real EF .crt: EasyFlash detected STRUCTURALLY at attach (eapi)");
        assert_eq!(attach_ty, MapperType::EasyFlash, "structural detect must be EasyFlash");
        return;
    }

    m.cold_reset();
    let mut sink = NullSink;
    let mut ran: u64 = 0;
    let mut locked: Option<(MapperType, u64)> = None;
    const CHUNK: u64 = 200_000;
    const BUDGET: u64 = 16_000_000;
    while ran < BUDGET {
        m.run_for_full(CHUNK, &mut sink, |_, _, _, _, _, _, _| {});
        ran += CHUNK;
        let ty = m.cartridge.as_ref().map(|c| c.mapper_type()).unwrap_or(MapperType::SelfConfig);
        if ty != MapperType::SelfConfig {
            locked = Some((ty, ran));
            break;
        }
    }
    match locked {
        Some((ty, cyc)) => {
            eprintln!("real EF .crt: LOCKED {ty:?} after ~{cyc} cycles");
            assert_eq!(ty, MapperType::EasyFlash, "a real EasyFlash cart must auto-lock EasyFlash");
        }
        None => panic!(
            "real EF cart did NOT lock within {BUDGET} cycles (final_pc=${:04X})",
            m.cpu.pc
        ),
    }
}
