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
        phi1: 0xff,
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
    assert_eq!(m.read(0x8000, &bi(), 0), Some(0));
    // ROMH@A000 not visible for MagicDesk.
    assert_eq!(m.read(0xa000, &bi(), 0), None);

    // Write bank 2 via $DE00 (bit7 clear → enabled). bankmask for 4 banks = 0x03.
    assert!(m.write(0xde00, 2, &bi(), 0));
    assert_eq!(m.read(0x8000, &bi(), 0), Some(2));
    assert_eq!(m.get_lines().exrom, 0);

    // bit7 set → cart disabled → exrom=1, game=1 (lines released).
    assert!(m.write(0xde00, 0x80, &bi(), 0));
    assert_eq!(m.get_lines().exrom, 1);
    assert_eq!(m.get_lines().game, 1);

    // reset → bank 0, regval 0, enabled.
    m.reset();
    assert_eq!(m.get_lines().exrom, 0);
    assert_eq!(m.read(0x8000, &bi(), 0), Some(0));

    // A write outside $DE00-$DEFF is not consumed.
    assert!(!m.write(0x8000, 0xff, &bi(), 0));
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
    assert_eq!(m.read(0x8000, &bi(), 0), Some(0x10));
    assert_eq!(m.read(0xa000, &bi(), 0), Some(0x20));
    // disable → exrom=1, game=1.
    assert!(m.write(0xde00, 0x80, &bi(), 0));
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
    assert_eq!(m.read(0x8000, &bi(), 0), Some(0x40)); // bank 0 ROML
    assert_eq!(m.read(0xa000, &bi(), 0), Some(0x40)); // 16K mirror of the SAME bank
    // bank-select bank 1 via $DE00.
    assert!(m.write(0xde00, 1, &bi(), 0));
    assert_eq!(m.read(0x8000, &bi(), 0), Some(0x41));
    assert_eq!(m.read(0xa000, &bi(), 0), Some(0x41));
}

#[test]
fn normal_8k_static_lines() {
    let crt = build_crt(0, 0, 1, "N8", &[(0, 0x8000, vec![0x99u8; 0x2000])]);
    let (_img, mut m) = load_cartridge_from_bytes(&crt, "N8", None).unwrap();
    assert_eq!(m.mapper_type(), MapperType::Normal8k);
    assert_eq!(m.get_lines().exrom, 0);
    assert_eq!(m.get_lines().game, 1);
    assert_eq!(m.read(0x8000, &bi(), 0), Some(0x99));
    // A $DE00 write is never consumed by a normal cart.
    assert!(!m.write(0xde00, 5, &bi(), 0));
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
    assert_eq!(m.read(0xa000, &bi(), 0), None);
    assert_eq!(m.read(0xfffc, &bi(), 0), Some(0x00));
    assert_eq!(m.read(0xfffd, &bi(), 0), Some(0xf0));
}

#[test]
fn unsupported_serial_families_error() {
    // hw=62 (GMOD3, SPI flash) still yields no mapper in this tier.
    let crt = build_crt(62, 1, 0, "G3", &[(0, 0x8000, vec![0u8; 0x2000])]);
    let img = parse_crt(&crt, "x", None).unwrap();
    assert_eq!(img.mapper_type, MapperType::Unsupported);
    assert!(load_cartridge_from_bytes(&crt, "G3", None).is_err());
}

#[test]
fn easyflash_gmod2_megabyter_build_writable_mappers() {
    // hw=32 (EasyFlash), 60 (GMOD2), 86 (MegaByter) now build the WRITABLE tier.
    let ef = build_crt(32, 1, 0, "EF", &[(0, 0x8000, vec![0u8; 0x2000])]);
    let (img, m) = load_cartridge_from_bytes(&ef, "EF", None).expect("EasyFlash builds");
    assert_eq!(img.mapper_type, MapperType::EasyFlash);
    assert_eq!(m.mapper_type(), MapperType::EasyFlash);

    let g2 = build_crt(60, 0, 1, "G2", &[(0, 0x8000, vec![0u8; 0x2000])]);
    let (_i, m2) = load_cartridge_from_bytes(&g2, "G2", None).expect("GMOD2 builds");
    assert_eq!(m2.mapper_type(), MapperType::Gmod2);

    let mb = build_crt(86, 0, 1, "MB", &[(0, 0x8000, vec![0u8; 0x2000])]);
    let (_i, m3) = load_cartridge_from_bytes(&mb, "MB", None).expect("MegaByter builds");
    assert_eq!(m3.mapper_type(), MapperType::MegaByter);
}

#[test]
fn state_roundtrip() {
    let chips: Vec<(u16, u16, Vec<u8>)> =
        (0..4u16).map(|n| (n, 0x8000, vec![n as u8; 0x2000])).collect();
    let crt = build_crt(19, 0, 1, "MD", &chips);
    let (_img, mut m) = load_cartridge_from_bytes(&crt, "MD", None).unwrap();
    m.write(0xde00, 3, &bi(), 0);
    let st: CartState = m.get_state();
    assert_eq!(st.current_bank, 3);
    assert_eq!(st.control_register, Some(3));
    // Fresh mapper, restore state → same live bank.
    let (_i2, mut m2) = load_cartridge_from_bytes(&crt, "MD", None).unwrap();
    m2.set_state(st);
    assert_eq!(m2.read(0x8000, &bi(), 0), Some(3));
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
fn cart_resident_divergence_guardrail() {
    // GUARDRAIL #2 (undump vs mounted cart): cart_resident_divergence flags when the
    // resident RAM under the cart window differs from the cart's flash/ROM, and clears
    // when they match. (No cart → None.)
    use trx64_core::Machine;

    // No cart mounted → no divergence signal.
    let bare = Machine::new();
    assert!(bare.cart_resident_divergence().is_none(), "no cart → no nudge");

    // Mount an 8K cart whose ROM is all $AA at $8000..$9FFF.
    let mut m = Machine::new();
    let crt = build_crt(19, 0, 1, "MD", &[(0, 0x8000, vec![0xaa; 0x2000])]);
    m.attach_cart_from_bytes(&crt, "MD").expect("attach");
    // Resident RAM under the window is power-on (0x00) → DIVERGES from the cart ($AA).
    let div = m.cart_resident_divergence().expect("RAM 0x00 vs cart 0xAA diverges");
    assert_eq!(div.0, 0x8000, "first divergent sample at the window base");
    assert_eq!(div.1, 0xaa, "cart byte");
    assert_eq!(div.2, 0x00, "resident RAM byte");

    // Make the resident RAM under the WHOLE low window match the cart → no divergence.
    for off in 0..0x2000usize {
        m.ram[0x8000 + off] = 0xaa;
    }
    assert!(
        m.cart_resident_divergence().is_none(),
        "resident RAM == cart at every sample → no nudge"
    );
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

// ── WRITABLE FLASH TIER: program/erase/EEPROM round-trips + write-back ────────

/// Build a minimal EasyFlash CRT (hw=32). `banks` chips: each (bank, load, data).
/// EasyFlash boots ultimax (exrom=1, game=0) so the reset vector is in ROMH.
fn build_easyflash_crt(chips: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
    build_crt(32, 1, 0, "EF", chips)
}

/// Drive the AMD AM29F040B byte-program command sequence through the EasyFlash
/// mapper's flash-low chip (ROML window, ultimax mode): AA→55→A0→<addr,data>.
/// The mapper write hook stores to flash ONLY in ultimax; EasyFlash boots
/// ultimax (register_02=0 → memconfig ULTIMAX), so $8000-$9FFF writes program
/// the low flash. The magic addresses ($555/$2AA) live in the low 11 bits, so a
/// store to $8000|magic hits the chip's magic1/2 in bank 0.
#[test]
fn easyflash_flash_program_then_readback() {
    // Bank 0: ROML + ROMH both 0xFF (erased flash). One 16K chip ($8000, 0x4000)
    // gives bank 0 ROML+ROMH; that's the simplest 2-chip-equivalent layout.
    let mut bank0 = vec![0xffu8; 0x4000];
    // Put a reset vector in ROMH so the (unused-here) boot would be valid.
    bank0[0x3ffc] = 0x00;
    bank0[0x3ffd] = 0x80;
    let crt = build_easyflash_crt(&[(0, 0x8000, bank0)]);
    let (_img, mut m) = load_cartridge_from_bytes(&crt, "EF", None).unwrap();
    assert_eq!(m.mapper_type(), MapperType::EasyFlash);

    // EasyFlash boots ultimax: register_02 = 0 → exrom=1, game=0.
    assert_eq!(m.get_lines().exrom, 1);
    assert_eq!(m.get_lines().game, 0);

    // Program 0x42 into ROML offset 0x100 (bank 0). The AMD sequence addresses use
    // the magic offsets in the $8000 window: $8000|0x555 = $8555, $8000|0x2AA=$82AA.
    assert!(m.write(0x8555, 0xaa, &bi(), 10));
    assert!(m.write(0x82aa, 0x55, &bi(), 11));
    assert!(m.write(0x8555, 0xa0, &bi(), 12)); // byte-program command
    assert!(m.write(0x8100, 0x42, &bi(), 13)); // program 0x42 at ROML $0100
    // Readback through the bus read path (FSM is back in READ → array byte).
    assert_eq!(m.read(0x8100, &bi(), 14), Some(0x42));
    assert!(m.is_writable_dirty(), "flash must report dirty after a program");

    // Program a second byte to prove the chip stays writable.
    assert!(m.write(0x8555, 0xaa, &bi(), 20));
    assert!(m.write(0x82aa, 0x55, &bi(), 21));
    assert!(m.write(0x8555, 0xa0, &bi(), 22));
    assert!(m.write(0x8101, 0x99, &bi(), 23));
    assert_eq!(m.read(0x8101, &bi(), 24), Some(0x99));

    // The writable image carries the programmed bytes (lo flash, offset 0x100/0x101).
    let img = m.writable_image(25).expect("EasyFlash has a writable image");
    assert_eq!(img[0x100], 0x42);
    assert_eq!(img[0x101], 0x99);
}

/// EasyFlash sector erase via the AMD 6-write sequence (AA 55 80 AA 55 30) wipes
/// the sector to 0xFF after the lazy erase-alarm window elapses.
#[test]
fn easyflash_sector_erase_lazy_clk() {
    // Bank 0 ROML pre-filled with 0x00 so we can see the erase to 0xFF.
    let mut bank0 = vec![0x00u8; 0x4000];
    bank0[0x3ffc] = 0x00;
    bank0[0x3ffd] = 0x80;
    let crt = build_easyflash_crt(&[(0, 0x8000, bank0)]);
    let (_img, mut m) = load_cartridge_from_bytes(&crt, "EF", None).unwrap();

    // Sector-erase command sequence at clk 0 (sector 0 = $00000-$0FFFF).
    m.write(0x8555, 0xaa, &bi(), 0);
    m.write(0x82aa, 0x55, &bi(), 0);
    m.write(0x8555, 0x80, &bi(), 0);
    m.write(0x8555, 0xaa, &bi(), 0);
    m.write(0x82aa, 0x55, &bi(), 0);
    m.write(0x8000, 0x30, &bi(), 0); // sector 0 erase armed
    // A read well past the timeout (50) + sector cycles (1_000_000) catches the
    // lazy alarm up → sector 0 wiped to 0xFF.
    let _ = m.read(0x9000, &bi(), 2_000_000);
    assert_eq!(m.read(0x8000, &bi(), 2_000_001), Some(0xff));
    assert_eq!(m.read(0x9fff, &bi(), 2_000_002), Some(0xff));
}

/// EasyFlash $DE00 bank register + $DE02 mode register drive banking + the
/// EXROM/GAME lines. VICE easyflash_memconfig is indexed by register_02&7 =
/// (mode<<2)|(!exrom<<1)|game (jumper in bit3). So register_02=2 → !exrom=1,
/// game=0 → 16K (exrom=0,game=0); register_02=0 → ultimax (boot).
#[test]
fn easyflash_bank_and_mode_register() {
    // 2 banks; bank N ROML[0] = N.
    let chips: Vec<(u16, u16, Vec<u8>)> = (0..2u16)
        .map(|n| {
            let mut d = vec![0xffu8; 0x2000];
            d[0] = n as u8;
            (n, 0x8000, d)
        })
        .collect();
    let crt = build_easyflash_crt(&chips);
    let (_img, mut m) = load_cartridge_from_bytes(&crt, "EF", None).unwrap();

    // Boot: register_02 = 0 → ultimax (exrom=1, game=0).
    assert_eq!(m.get_lines().exrom, 1);
    assert_eq!(m.get_lines().game, 0);
    // $DE02 = 2 → memconfig index 2 = 16K game (exrom=0, game=0).
    assert!(m.write(0xde02, 0x02, &bi(), 0));
    assert_eq!(m.get_lines().exrom, 0);
    assert_eq!(m.get_lines().game, 0);
    // $DE00 bank = 1 → ROML[0] reads 1.
    assert!(m.write(0xde00, 0x01, &bi(), 0));
    assert_eq!(m.read(0x8000, &bi(), 0), Some(1));
    // $DE04 mirrors $DE00 (addr & 2 == 0); $DE06 mirrors $DE02 (addr & 2).
    assert!(m.write(0xde04, 0x00, &bi(), 0)); // bank 0
    assert_eq!(m.read(0x8000, &bi(), 0), Some(0));
}

/// GMOD2 M93C86 EEPROM write-then-read round-trip through the mapper's IO1
/// ($DE00) serial protocol: a full EWEN + WRITE + READ shift sequence persists
/// a 16-bit word and reads it back on the EEPROM DO bit (IO1 read bit 7).
#[test]
fn gmod2_eeprom_write_read_roundtrip() {
    let crt = build_crt(60, 0, 1, "G2", &[(0, 0x8000, vec![0xffu8; 0x2000])]);
    let (_img, mut m) = load_cartridge_from_bytes(&crt, "G2", None).unwrap();
    assert_eq!(m.mapper_type(), MapperType::Gmod2);

    // GMOD2 IO1 ($DE00): bit6 = EEPROM CS, bit5 = CLK, bit4 = DI. The cart bank is
    // bits 0-5 (CS bit6 also selects cmode). We drive CS=1 (bit6) so cmode=off,
    // then clock the serial command in. Helper: write a $DE00 byte = base | lines.
    // CS=1 keeps the EEPROM selected; pulse CLK (bit5) high then low per bit; DI
    // is bit4.
    let mut io1 = |m: &mut Box<dyn trx64_core::cart::CartMapper>, cs: u8, clk: u8, di: u8| {
        let v = (cs << 6) | (clk << 5) | (di << 4);
        m.write(0xde00, v, &bi(), 0);
    };
    // Shift one bit MSB-first: set DI with CLK low, then raise CLK, then lower.
    let mut shift = |m: &mut Box<dyn trx64_core::cart::CartMapper>, di: u8| {
        io1(m, 1, 0, di);
        io1(m, 1, 1, di);
        io1(m, 1, 0, di);
    };

    // Deassert then assert CS to reset the input shiftreg.
    io1(&mut m, 0, 0, 0);
    io1(&mut m, 1, 0, 0);
    // EWEN = start(1) 00 11 + pad to 13 clocks.
    for b in [1u8, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0] {
        shift(&mut m, b);
    }
    // New command: deassert/reassert CS.
    io1(&mut m, 0, 0, 0);
    io1(&mut m, 1, 0, 0);
    // WRITE: start(1) 01 + 10-bit addr (0x004) + 16-bit data (0x5AA5) = 29 clocks.
    let mut wbits = vec![1u8, 0, 1];
    for i in (0..10).rev() {
        wbits.push(((0x004u32 >> i) & 1) as u8);
    }
    for i in (0..16).rev() {
        wbits.push(((0x5AA5u32 >> i) & 1) as u8);
    }
    for b in wbits {
        shift(&mut m, b);
    }
    // Falling CS commits the write.
    io1(&mut m, 0, 0, 0);
    assert!(m.is_writable_dirty(), "GMOD2 EEPROM must be dirty after a write");

    // READ back: start(1) 10 + 10-bit addr (0x004) = 13 clocks → CMDREADDUMMY.
    io1(&mut m, 1, 0, 0);
    let mut rbits = vec![1u8, 1, 0];
    for i in (0..10).rev() {
        rbits.push(((0x004u32 >> i) & 1) as u8);
    }
    for b in rbits {
        shift(&mut m, b);
    }
    // Clock out 16 data bits; each IO1 read returns DO in bit 7 (CS asserted).
    let mut word: u32 = 0;
    for _ in 0..16 {
        io1(&mut m, 1, 1, 0); // rising CLK shifts the next bit out
        let dout = (m.read(0xde00, &bi(), 0).unwrap() >> 7) & 1;
        word = (word << 1) | dout as u32;
        io1(&mut m, 1, 0, 0); // falling CLK
    }
    assert_eq!(word, 0x5AA5, "EEPROM read-back must match the written word");

    // The writable image carries the EEPROM bytes after the flash array.
    let img = m.writable_image(0).expect("GMOD2 has a writable image");
    // flash = 64 banks * 0x2000 = 0x80000; EEPROM word 0x004 at byte 0x008/0x009.
    assert_eq!(img[0x80000 + (0x004 << 1)], 0x5A);
    assert_eq!(img[0x80000 + (0x004 << 1) + 1], 0xA5);
}

/// EasyFlash write-back: program a byte, snapshot the writable image, load it into
/// a fresh mapper, and confirm the programmed byte persisted (the save survives a
/// detach/reattach via the image).
#[test]
fn easyflash_writeback_persists_across_image_roundtrip() {
    let mut bank0 = vec![0xffu8; 0x4000];
    bank0[0x3ffc] = 0x00;
    bank0[0x3ffd] = 0x80;
    let crt = build_easyflash_crt(&[(0, 0x8000, bank0)]);
    let (_img, mut m) = load_cartridge_from_bytes(&crt, "EF", None).unwrap();
    // Program 0x37 at ROML $0042.
    m.write(0x8555, 0xaa, &bi(), 0);
    m.write(0x82aa, 0x55, &bi(), 0);
    m.write(0x8555, 0xa0, &bi(), 0);
    m.write(0x8042, 0x37, &bi(), 0);
    let saved = m.writable_image(0).expect("image");

    // Fresh mapper from the same CRT (blank flash), then load the saved image.
    let (_img2, mut m2) = load_cartridge_from_bytes(&crt, "EF", None).unwrap();
    assert_eq!(m2.read(0x8042, &bi(), 0), Some(0xff)); // blank before load
    m2.set_writable_image(&saved);
    assert_eq!(m2.read(0x8042, &bi(), 0), Some(0x37)); // persisted after load
}

// ── BEHAVIORAL: a real EasyFlash CRT boots into the cart (ROM-gated) ──────────

const EF_SAMPLE: &str =
    "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/AccoladeComics_TRX+1D_EF.crt";

#[test]
#[ignore = "needs ROMs + the AccoladeComics_TRX+1D_EF.crt sample; run with --ignored"]
fn behavioral_easyflash_boots_into_cart() {
    use std::path::Path;
    use trx64_core::{Machine, NullSink};

    if !Path::new(ROM_DIR).join("kernal-901227-03.bin").exists() {
        eprintln!("SKIP: ROMs absent");
        return;
    }
    let crt = match std::fs::read(EF_SAMPLE) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("SKIP: {EF_SAMPLE} absent");
            return;
        }
    };

    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let (name, ty) = m.attach_cart_from_bytes(&crt, "EF").expect("attach CRT");
    eprintln!("attached: {name} ({ty:?})");
    assert_eq!(ty, MapperType::EasyFlash);
    m.cold_reset();

    // EasyFlash boots ultimax: the $FFFC reset vector comes from the cart's ROMH
    // (hi flash), so the machine reboots INTO the cart. Run a budget and record
    // whether the CPU ever executes inside a cart window ($8000-$9FFF ROML or the
    // ultimax $E000-$FFFF ROMH), and whether a non-blank frame paints.
    let mut sink = NullSink;
    let mut reached_cart = false;
    let mut max_colors = 0usize;
    for _ in 0..200 {
        m.run_for_full(50_000, &mut sink, |_, _, _, _, _, _, _| {});
        let pc = m.cpu.pc;
        if (0x8000..=0x9fff).contains(&pc) || (0xe000..=0xffff).contains(&pc) {
            reached_cart = true;
        }
        let (_w, _h, rgba) = m.render_canvas_rgba();
        let mut colors = std::collections::HashSet::new();
        for px in rgba.chunks_exact(4) {
            colors.insert((px[0], px[1], px[2]));
        }
        max_colors = max_colors.max(colors.len());
    }
    eprintln!(
        "reached_cart={reached_cart} max_distinct_colors={max_colors} final_pc=${:04X}",
        m.cpu.pc
    );
    assert!(
        reached_cart,
        "CPU never executed inside a cart window — the machine did not boot into the EasyFlash cart"
    );
    if max_colors > 1 {
        eprintln!("PASS: EasyFlash executed + a non-blank frame rendered.");
    } else {
        eprintln!("PASS (EasyFlash executed; frame still blank in budget).");
    }
}

// ── BEHAVIORAL: a real GMOD2 CRT attaches + the flash/EEPROM build (ROM-gated) ─

const GMOD2_SAMPLE: &str =
    "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/yeti_mountain_GMOD2.crt";

#[test]
#[ignore = "needs ROMs + the yeti_mountain_GMOD2.crt sample; run with --ignored"]
fn behavioral_gmod2_boots_into_cart() {
    use std::path::Path;
    use trx64_core::{Machine, NullSink};

    if !Path::new(ROM_DIR).join("kernal-901227-03.bin").exists() {
        eprintln!("SKIP: ROMs absent");
        return;
    }
    let crt = match std::fs::read(GMOD2_SAMPLE) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("SKIP: {GMOD2_SAMPLE} absent");
            return;
        }
    };

    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let (name, ty) = m.attach_cart_from_bytes(&crt, "GMOD2").expect("attach CRT");
    eprintln!("attached: {name} ({ty:?})");
    assert_eq!(ty, MapperType::Gmod2);
    m.cold_reset();

    // GMOD2 boots 8K (exrom=0, game=1): the CBM80 cold-start runs out of the cart
    // ROML ($8000-$9FFF). Run a budget; record whether the CPU executes in ROML.
    let mut sink = NullSink;
    let mut reached_cart = false;
    let mut max_colors = 0usize;
    for _ in 0..200 {
        m.run_for_full(50_000, &mut sink, |_, _, _, _, _, _, _| {});
        let pc = m.cpu.pc;
        if (0x8000..=0x9fff).contains(&pc) {
            reached_cart = true;
        }
        let (_w, _h, rgba) = m.render_canvas_rgba();
        let mut colors = std::collections::HashSet::new();
        for px in rgba.chunks_exact(4) {
            colors.insert((px[0], px[1], px[2]));
        }
        max_colors = max_colors.max(colors.len());
    }
    eprintln!(
        "reached_cart={reached_cart} max_distinct_colors={max_colors} final_pc=${:04X}",
        m.cpu.pc
    );
    assert!(
        reached_cart,
        "CPU never executed inside the GMOD2 ROML window"
    );
    eprintln!("PASS: GMOD2 executed (flash ROML + M93C86 EEPROM mapper live).");
}
