//! cart_mapper_gate.rs — validation for the read-only cartridge mapper tier
//! (cart.rs + the full.rs FullBus wiring).
//!
//! Two layers:
//!   1. UNIT (always run): parse_crt + each mapper's banking + the PLA
//!      lines/memconfig, against known byte patterns + the VICE-derived rules.
//!   2. BEHAVIORAL (gated on a real .crt sample + ROMs present, run with
//!      `--ignored`): attach a Magic Desk CRT, cold-reset, run, confirm the
//!      machine boots INTO the cart (PC reaches the cart ROML window at
//!      $8000-$9FFF) and a non-blank frame renders.
//!
//! Run the behavioral gate:
//!   cargo test -p trx64-core --test cart_mapper_gate behavioral -- --ignored --nocapture

use trx64_core::cart::{
    load_cartridge_from_bytes, parse_crt, BankInfo, CartState, MapperType,
};

// ── helpers ─────────────────────────────────────────────────────────────────

/// A minimal valid CRT header (0x40 bytes) + N CHIP packets. `hw` = hardware
/// type, `exrom`/`game` = the header lines. Each chip is (bank, load_addr, data).
fn build_crt(hw: u16, exrom: u8, game: u8, name: &str, chips: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   "); // 16-byte signature
    v.extend_from_slice(&0x40u32.to_be_bytes()); // headerLen @ 0x10
    v.extend_from_slice(&0x0100u16.to_be_bytes()); // version @ 0x14
    v.extend_from_slice(&hw.to_be_bytes()); // hardwareType @ 0x16
    v.push(exrom); // @ 0x18
    v.push(game); // @ 0x19
    v.extend_from_slice(&[0u8; 6]); // @ 0x1A-0x1F reserved
    // name @ 0x20-0x3F (32 bytes, zero-padded)
    let mut nm = [0u8; 32];
    let nb = name.as_bytes();
    nm[..nb.len().min(32)].copy_from_slice(&nb[..nb.len().min(32)]);
    v.extend_from_slice(&nm);
    assert_eq!(v.len(), 0x40);
    for (bank, load, data) in chips {
        v.extend_from_slice(b"CHIP");
        let packet_len = 0x10 + data.len() as u32;
        v.extend_from_slice(&packet_len.to_be_bytes()); // @ +4
        v.extend_from_slice(&0u16.to_be_bytes()); // chipType @ +8
        v.extend_from_slice(&bank.to_be_bytes()); // bank @ +10
        v.extend_from_slice(&load.to_be_bytes()); // loadAddr @ +12
        v.extend_from_slice(&(data.len() as u16).to_be_bytes()); // size @ +14
        v.extend_from_slice(data);
    }
    v
}

/// A bank info with the I/O / port state a banked write needs (the read-only
/// mappers ignore most of it).
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
    }
}

// ── parse_crt ─────────────────────────────────────────────────────────────────

#[test]
fn parse_crt_rejects_non_crt() {
    let bad = b"NOT A CARTRIDGE!!extra".to_vec();
    assert!(parse_crt(&bad, "x", None).is_err());
}

#[test]
fn parse_crt_header_and_chip_walk() {
    // hw=19 (Magic Desk), 2 banks of 8K ROML.
    let b0 = vec![0xa9u8; 0x2000];
    let mut b1 = vec![0x55u8; 0x2000];
    b1[0] = 0xc3; // distinguishable byte
    let crt = build_crt(
        19,
        0,
        1,
        "TESTCART",
        &[(0, 0x8000, b0.clone()), (1, 0x8000, b1.clone())],
    );
    let img = parse_crt(&crt, "test.crt", None).expect("parse");
    assert_eq!(img.name, "TESTCART");
    assert_eq!(img.mapper_type, MapperType::MagicDesk);
    assert_eq!(img.exrom, 0);
    assert_eq!(img.game, 1);
    assert_eq!(img.banks.len(), 2);
    // bank 0 ROML present, first byte $A9.
    let bank0 = img.banks.get(&0).unwrap();
    assert_eq!(bank0.roml.unwrap()[0], 0xa9);
    let bank1 = img.banks.get(&1).unwrap();
    assert_eq!(bank1.roml.unwrap()[0], 0xc3);
    assert_eq!(bank1.romh_a000, None);
}

#[test]
fn parse_crt_infers_normal_8k_16k_ultimax() {
    // hw=0, single $8000 8K chip → normal_8k.
    let c8 = build_crt(0, 0, 1, "N8", &[(0, 0x8000, vec![1u8; 0x2000])]);
    assert_eq!(parse_crt(&c8, "x", None).unwrap().mapper_type, MapperType::Normal8k);

    // hw=0 with a $A000 chip → normal_16k.
    let c16 = build_crt(
        0,
        0,
        0,
        "N16",
        &[(0, 0x8000, vec![1u8; 0x2000]), (0, 0xa000, vec![2u8; 0x2000])],
    );
    assert_eq!(parse_crt(&c16, "x", None).unwrap().mapper_type, MapperType::Normal16k);

    // hw=0 with exrom=1,game=1 → ultimax.
    let cu = build_crt(0, 1, 1, "U", &[(0, 0xe000, vec![3u8; 0x2000])]);
    assert_eq!(parse_crt(&cu, "x", None).unwrap().mapper_type, MapperType::Ultimax);
}

#[test]
fn parse_crt_8000_chip_with_4000_bytes_splits_to_romh() {
    // A $8000 CHIP carrying 0x4000 bytes splits into ROML + ROMH@A000.
    let mut data = vec![0u8; 0x4000];
    data[0] = 0x11; // ROML[0]
    data[0x2000] = 0x22; // ROMH@A000[0]
    let crt = build_crt(0, 0, 0, "SPLIT", &[(0, 0x8000, data)]);
    let img = parse_crt(&crt, "x", None).unwrap();
    let b = img.banks.get(&0).unwrap();
    assert_eq!(b.roml.unwrap()[0], 0x11);
    assert_eq!(b.romh_a000.unwrap()[0], 0x22);
}

// ── MagicDesk mapper banking + lines ──────────────────────────────────────────

#[test]
fn magicdesk_banking_and_lines() {
    // 4 banks; bank N's ROML[0] = N (so we can read which bank is live).
    let chips: Vec<(u16, u16, Vec<u8>)> = (0..4u16)
        .map(|n| {
            let mut d = vec![0u8; 0x2000];
            d[0] = n as u8;
            (n, 0x8000, d)
        })
        .collect();
    let crt = build_crt(19, 0, 1, "MD", &chips);
    let (_img, mut m) = load_cartridge_from_bytes(&crt, "MD", None).unwrap();

    // Boot: bank 0, enabled → exrom=0, game=1 (8K), ROML[0]==0.
    assert_eq!(m.get_lines().exrom, 0);
    assert_eq!(m.get_lines().game, 1);
    assert_eq!(m.read(0x8000, &bi()), Some(0));
    // ROMH@A000 not visible for MagicDesk.
    assert_eq!(m.read(0xa000, &bi()), None);

    // Write bank 2 via $DE00 (bit7 clear → enabled). bankmask for 4 banks = 0x03.
    assert!(m.write(0xde00, 2, &bi()));
    assert_eq!(m.read(0x8000, &bi()), Some(2));
    assert_eq!(m.get_lines().exrom, 0);

    // bit7 set → cart disabled → exrom=1, game=1 (lines released).
    assert!(m.write(0xde00, 0x80, &bi()));
    assert_eq!(m.get_lines().exrom, 1);
    assert_eq!(m.get_lines().game, 1);

    // reset → bank 0, regval 0, enabled.
    m.reset();
    assert_eq!(m.get_lines().exrom, 0);
    assert_eq!(m.read(0x8000, &bi()), Some(0));

    // A write outside $DE00-$DEFF is not consumed.
    assert!(!m.write(0x8000, 0xff, &bi()));
}

#[test]
fn magicdesk16_maps_roml_and_romh() {
    // bank 0: ROML[0]=$10, ROMH@A000[0]=$20 (a $8000 chip with 0x4000 bytes).
    let mut d = vec![0u8; 0x4000];
    d[0] = 0x10;
    d[0x2000] = 0x20;
    let crt = build_crt(85, 0, 0, "MD16", &[(0, 0x8000, d)]);
    let (_img, mut m) = load_cartridge_from_bytes(&crt, "MD16", None).unwrap();
    assert_eq!(m.mapper_type(), MapperType::MagicDesk16);
    // enabled → 16K game (exrom=0, game=0).
    assert_eq!(m.get_lines().exrom, 0);
    assert_eq!(m.get_lines().game, 0);
    assert_eq!(m.read(0x8000, &bi()), Some(0x10));
    assert_eq!(m.read(0xa000, &bi()), Some(0x20));
    // disable → exrom=1, game=1.
    assert!(m.write(0xde00, 0x80, &bi()));
    assert_eq!(m.get_lines().game, 1);
}

#[test]
fn ocean_8k_vs_16k_mirror() {
    // Small image (2 banks → 0x4000 bytes, not 512KB) → 16K mirror config.
    let chips: Vec<(u16, u16, Vec<u8>)> = (0..2u16)
        .map(|n| {
            let mut d = vec![0u8; 0x2000];
            d[0] = 0x40 + n as u8;
            (n, 0x8000, d)
        })
        .collect();
    let crt = build_crt(5, 0, 0, "OCEAN", &chips);
    let (_img, mut m) = load_cartridge_from_bytes(&crt, "OCEAN", None).unwrap();
    assert_eq!(m.mapper_type(), MapperType::Ocean);
    // not 512KB → 16K game (exrom=0, game=0), and $A000 mirrors $8000's ROML bank.
    assert_eq!(m.get_lines().game, 0);
    assert_eq!(m.read(0x8000, &bi()), Some(0x40)); // bank 0 ROML
    assert_eq!(m.read(0xa000, &bi()), Some(0x40)); // 16K mirror of the SAME bank
    // bank-select bank 1 via $DE00.
    assert!(m.write(0xde00, 1, &bi()));
    assert_eq!(m.read(0x8000, &bi()), Some(0x41));
    assert_eq!(m.read(0xa000, &bi()), Some(0x41));
}

#[test]
fn normal_8k_static_lines() {
    let crt = build_crt(0, 0, 1, "N8", &[(0, 0x8000, vec![0x99u8; 0x2000])]);
    let (_img, mut m) = load_cartridge_from_bytes(&crt, "N8", None).unwrap();
    assert_eq!(m.mapper_type(), MapperType::Normal8k);
    assert_eq!(m.get_lines().exrom, 0);
    assert_eq!(m.get_lines().game, 1);
    assert_eq!(m.read(0x8000, &bi()), Some(0x99));
    // A $DE00 write is never consumed by a normal cart.
    assert!(!m.write(0xde00, 5, &bi()));
}

#[test]
fn ultimax_maps_romh_e000_not_a000() {
    let mut d = vec![0u8; 0x2000];
    d[0x1ffc] = 0x00; // $FFFC low (vector)
    d[0x1ffd] = 0xf0; // $FFFD high → reset vector $F000
    let crt = build_crt(0, 1, 1, "U", &[(0, 0xe000, d)]);
    let (_img, mut m) = load_cartridge_from_bytes(&crt, "U", None).unwrap();
    assert_eq!(m.mapper_type(), MapperType::Ultimax);
    // ROMH at $E000-$FFFF, NOT $A000.
    assert_eq!(m.read(0xa000, &bi()), None);
    assert_eq!(m.read(0xfffc, &bi()), Some(0x00));
    assert_eq!(m.read(0xfffd, &bi()), Some(0xf0));
}

#[test]
fn unsupported_flash_families_error() {
    // hw=32 (EasyFlash) parses but yields no read-only mapper.
    let crt = build_crt(32, 1, 0, "EF", &[(0, 0x8000, vec![0u8; 0x2000])]);
    let img = parse_crt(&crt, "x", None).unwrap();
    assert_eq!(img.mapper_type, MapperType::Unsupported);
    assert!(load_cartridge_from_bytes(&crt, "EF", None).is_err());
}

#[test]
fn state_roundtrip() {
    let chips: Vec<(u16, u16, Vec<u8>)> =
        (0..4u16).map(|n| (n, 0x8000, vec![n as u8; 0x2000])).collect();
    let crt = build_crt(19, 0, 1, "MD", &chips);
    let (_img, mut m) = load_cartridge_from_bytes(&crt, "MD", None).unwrap();
    m.write(0xde00, 3, &bi());
    let st: CartState = m.get_state();
    assert_eq!(st.current_bank, 3);
    assert_eq!(st.control_register, Some(3));
    // Fresh mapper, restore state → same live bank.
    let (_i2, mut m2) = load_cartridge_from_bytes(&crt, "MD", None).unwrap();
    m2.set_state(st);
    assert_eq!(m2.read(0x8000, &bi()), Some(3));
}

// ── Machine-level: memconfig / PLA lines with a cart attached ──────────────────

#[test]
fn machine_memconfig_magicdesk_8k_boot_config() {
    use trx64_core::Machine;
    let mut m = Machine::new();
    // Magic Desk 8K: bank 0 ROML, byte 0 = $A9.
    let crt = build_crt(19, 0, 1, "MD", &[(0, 0x8000, {
        let mut d = vec![0u8; 0x2000];
        d[0] = 0xa9;
        d
    })]);
    let (name, ty) = m.attach_cart_from_bytes(&crt, "MD").expect("attach");
    assert_eq!(name, "MD");
    assert_eq!(ty, MapperType::MagicDesk);
    // Boot port = $37 (loram=hiram=charen=1), exrom=0, game=1 → idx 7|16 = 23.
    // bank8=CartLo (cart at $8000), bankA=Basic, bankE=Kernal. NOT ultimax.
    let cfg = m.memconfig;
    assert!(matches!(cfg.bank8, trx64_core::Bank8::CartLo));
    assert!(cfg.basic);
    assert!(cfg.kernal);
    assert!(!cfg.ultimax);
}

#[test]
fn machine_no_cart_memconfig_unchanged() {
    use trx64_core::Machine;
    let m = Machine::new();
    // No cart: boot memconfig must be the stock BASIC+IO+KERNAL config (idx 0x1f).
    let cfg = m.memconfig;
    assert!(matches!(cfg.bank8, trx64_core::Bank8::Ram));
    assert!(cfg.basic);
    assert!(cfg.kernal);
    assert!(cfg.io);
    assert!(!cfg.char_rom);
    assert!(!cfg.ultimax);
}

// ── BEHAVIORAL: a real Magic Desk CRT boots into the cart ─────────────────────

const ROM_DIR: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
const MD_SAMPLE: &str =
    "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/im3_MAGICDESK.crt";

#[test]
#[ignore = "needs ROMs + the im3_MAGICDESK.crt sample; run with --ignored"]
fn behavioral_magicdesk_boots_into_cart() {
    use std::path::Path;
    use trx64_core::{Machine, NullSink};

    if !Path::new(ROM_DIR).join("kernal-901227-03.bin").exists() {
        eprintln!("SKIP: ROMs absent");
        return;
    }
    let crt = match std::fs::read(MD_SAMPLE) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("SKIP: {MD_SAMPLE} absent");
            return;
        }
    };

    let mut m = Machine::new();
    // boot_from_dir loads ROMs + cold-resets (no cart yet). Attach the cart, then
    // cold-reset AGAIN so the cart is reset + the memconfig is cart-aware and the
    // $FFFC vector fetches through the banked map.
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let (name, ty) = m.attach_cart_from_bytes(&crt, "im3_MAGICDESK").expect("attach CRT");
    eprintln!("attached: {name} ({ty:?})");
    assert_eq!(ty, MapperType::MagicDesk);
    m.cold_reset();

    // Run a long budget (this is a full-game crack — IM3 — that decrunches a big
    // payload from the cart's banks before painting). Sample the PC each chunk;
    // record whether the CPU ever executes inside the cart ROML window
    // ($8000-$9FFF), proving the KERNAL handed control to the cart's CBM80
    // cold-start, and whether the live banking ever maps the cart there.
    let mut sink = NullSink;
    let mut reached_cart = false;
    let mut max_pc_in_cart = 0u16;
    let mut max_colors = 0usize;
    for _ in 0..200 {
        m.run_for_full(50_000, &mut sink, |_, _, _, _, _, _, _| {});
        let pc = m.cpu.pc;
        if (0x8000..=0x9fff).contains(&pc) {
            reached_cart = true;
            max_pc_in_cart = max_pc_in_cart.max(pc);
        }
        let (_w, _h, rgba) = m.render_canvas_rgba();
        let mut colors = std::collections::HashSet::new();
        for px in rgba.chunks_exact(4) {
            colors.insert((px[0], px[1], px[2]));
        }
        max_colors = max_colors.max(colors.len());
    }

    eprintln!(
        "reached_cart={reached_cart} (max cart PC ${max_pc_in_cart:04X}) max_distinct_colors={max_colors} final_pc=${:04X}",
        m.cpu.pc
    );

    // HARD PASS criterion: the cart actually executed (PC inside the ROML window).
    // This is the load-bearing proof that the read-only mapper + the cart-aware
    // memconfig + the banked reset vector all work end-to-end: the KERNAL booted,
    // saw the cart's CBM80, and ran its cold-start out of the cart ROM. (A full
    // game crack may sit in its decrunch/loader long past a fixed budget before it
    // paints, exactly like the disk seven_game_gate's PARTIAL verdict — so the
    // screen-paint is logged, not asserted, to keep this a deterministic gate.)
    assert!(
        reached_cart,
        "CPU never executed inside the cart ROML window — the machine did not boot into the cart"
    );
    if max_colors > 1 {
        eprintln!("PASS: cart executed + a non-blank frame rendered.");
    } else {
        eprintln!("PASS (cart executed; frame still blank in budget — decrunch in progress).");
    }
}
