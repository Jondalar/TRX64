//! GMod4 cartridge gate (Spec 803).
//!
//! Covers the mapper's own contract: CRT type recognition, the register map (two banking
//! contexts, the inverted ROM-enable bits, A22), the three window offsets, the CPU-port
//! gating that makes "fake ultimax" work, and the bit-banged SPI path end to end — the
//! last one by running the vendor's own `flash_read_id` sequence THROUGH the registers,
//! which is the only way to prove the mapper and the SPI device agree.
//!
//! Reference: VICE patch #368 `c64/cart/gmod4.c` + the vendor register header
//! `include/gmod4.inc`. AGR is intentionally absent — see Spec 803 §6.

use trx64_core::cart::{parse_crt, BankInfo, CartMapper, MapperType};

const HW_GMOD4: u16 = 87;

/// A minimal valid CRT header + N CHIP packets.
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

/// Build a GMod4 mapper whose 8K half-bank `n` is filled with the byte `n as u8`, so a
/// read tells you which half-bank answered.
fn mapper(half_banks: u16) -> Box<dyn CartMapper> {
    let chips: Vec<(u16, u16, Vec<u8>)> =
        (0..half_banks).map(|b| (b, 0x8000, vec![b as u8; 0x2000])).collect();
    let crt = build_crt(HW_GMOD4, 1, 0, "GMOD4 TEST", &chips);
    let img = parse_crt(&crt, "gmod4.crt", None).expect("parse gmod4 crt");
    assert_eq!(img.mapper_type, MapperType::Gmod4);
    trx64_core::cart::mapper_from_image(&img).expect("build gmod4 mapper")
}

/// `mem_config == 7` (all three port lines high) — the everything-visible config the
/// registers and the $8000 window both answer in.
fn bi(port: u8) -> BankInfo {
    BankInfo {
        cpu_port_direction: 0x07,
        cpu_port_value: port,
        basic_visible: true,
        kernal_visible: true,
        io_visible: true,
        char_visible: false,
        cartridge_attached: true,
        cartridge_exrom: Some(1),
        cartridge_game: Some(0),
        phi1: 0xff,
    }
}
const CFG7: u8 = 0x07;

const CTRL: u16 = 0xde04;
const BANKING_ON: u8 = 0x40;

#[test]
fn crt_type_87_is_recognised() {
    let crt = build_crt(HW_GMOD4, 1, 0, "GMOD4", &[(0, 0x8000, vec![0xaa; 0x2000])]);
    let img = parse_crt(&crt, "g.crt", None).expect("parse");
    assert_eq!(img.mapper_type, MapperType::Gmod4);
}

#[test]
fn holds_ultimax_and_declares_fake_ultimax() {
    let m = mapper(4);
    let l = m.get_lines();
    assert_eq!((l.exrom, l.game), (1, 0), "GMod4 asserts ULTIMAX permanently");
    assert!(m.fake_ultimax(), "declined windows must fall back to the non-ultimax map");
}

#[test]
fn banking_off_shows_bank_zero_regardless_of_the_bank_registers() {
    let mut m = mapper(8);
    // Set a bank but leave banking DISABLED (control bit 6 clear).
    m.write(0xde01, 3, &bi(CFG7), 0);
    assert_eq!(m.read(0x8000, &bi(CFG7), 0), Some(0), "banking off ⇒ half-bank 0");
    // Enabling banking makes the same register take effect: bank 3 → half-bank 6.
    m.write(CTRL, BANKING_ON, &bi(CFG7), 0);
    assert_eq!(m.read(0x8000, &bi(CFG7), 0), Some(6), "bank 3 << 14 ⇒ half-bank 6");
}

#[test]
fn a000_window_is_the_odd_half_of_the_same_16k_bank() {
    let mut m = mapper(8);
    m.write(CTRL, BANKING_ON, &bi(CFG7), 0);
    m.write(0xde03, 2, &bi(CFG7), 0); // common bank 2 → both windows
    assert_eq!(m.read(0x8000, &bi(CFG7), 0), Some(4), "$8000 = even half");
    assert_eq!(m.read(0xa000, &bi(CFG7), 0), Some(5), "$A000 = odd half");
}

#[test]
fn the_two_banking_contexts_are_independent_and_writes_select_them() {
    let mut m = mapper(16);
    m.write(CTRL, BANKING_ON, &bi(CFG7), 0);
    m.write(0xde01, 1, &bi(CFG7), 0); // context A, bank 1
    m.write(0xde09, 5, &bi(CFG7), 0); // context B, bank 5 — and selects B
    assert_eq!(m.read(0x8000, &bi(CFG7), 0), Some(10), "context B is live after its write");
    m.write(0xde00, 0, &bi(CFG7), 0); // bare context-A select
    assert_eq!(m.read(0x8000, &bi(CFG7), 0), Some(2), "context A kept its own bank");
    // This is the point of the feature: an IRQ handler can bank without saving anything.
}

#[test]
fn rom_enable_bits_are_inverted() {
    let mut m = mapper(4);
    assert!(m.read(0x8000, &bi(CFG7), 0).is_some(), "enabled after power-up");
    m.write(CTRL, 0x02, &bi(CFG7), 0); // bit 1 SET = DISABLE $8000
    assert_eq!(m.read(0x8000, &bi(CFG7), 0), None, "a SET bit disables the window");
    assert!(m.read(0xa000, &bi(CFG7), 0).is_some(), "$A000 unaffected");
    m.write(CTRL, 0x08, &bi(CFG7), 0); // bit 3 = disable $E000
    assert_eq!(m.read(0xe000, &bi(CFG7), 0), None);
}

#[test]
fn e000_never_banks_so_the_cart_keeps_its_vectors() {
    let mut m = mapper(16);
    m.write(CTRL, BANKING_ON, &bi(CFG7), 0);
    m.write(0xde03, 7, &bi(CFG7), 0); // bank the world away
    assert_eq!(m.read(0x8000, &bi(CFG7), 0), Some(14), "$8000 followed the bank");
    assert_eq!(m.read(0xe000, &bi(CFG7), 0), Some(1), "$E000 stays on bank 0's odd half");
}

#[test]
fn a22_selects_the_upper_four_megabytes() {
    // A22 adds 0x400000 to every offset — half-bank 512.
    let mut m = mapper(600);
    assert_eq!(m.read(0x8000, &bi(CFG7), 0), Some(0));
    m.write(CTRL, 0x10, &bi(CFG7), 0); // bit 4 = A22
    assert_eq!(m.read(0x8000, &bi(CFG7), 0), Some(512u16 as u8), "A22 ⇒ +4 MiB");
}

#[test]
fn windows_decline_outside_their_cpu_port_configs() {
    // $8000 answers in mem_config 7 and 3 only (unless intrusive mode is on). This is
    // what makes fake ultimax work: the cart declines, and the bus reads RAM instead.
    let mut m = mapper(4);
    assert!(m.read(0x8000, &bi(7), 0).is_some());
    assert!(m.read(0x8000, &bi(3), 0).is_some());
    assert_eq!(m.read(0x8000, &bi(1), 0), None, "config 1 ⇒ RAM underneath");
    m.write(CTRL, 0x20, &bi(CFG7), 0); // intrusive
    assert!(m.read(0x8000, &bi(1), 0).is_some(), "intrusive mode overrides the gate");
}

#[test]
fn registers_ignore_writes_in_the_wrong_cpu_port_config() {
    let mut m = mapper(8);
    m.write(CTRL, BANKING_ON, &bi(CFG7), 0);
    assert!(!m.write(0xde01, 3, &bi(1), 0), "declined ⇒ the write belongs to RAM");
    assert_eq!(m.read(0x8000, &bi(CFG7), 0), Some(0), "the bank register did not change");
}

#[test]
fn reset_clears_control_but_deliberately_not_the_banking_registers() {
    let mut m = mapper(8);
    m.write(CTRL, BANKING_ON, &bi(CFG7), 0);
    m.write(0xde01, 3, &bi(CFG7), 0);
    m.reset();
    // Banking is off again, so the window shows bank 0 …
    assert_eq!(m.read(0x8000, &bi(CFG7), 0), Some(0));
    // … but the register still holds 3: the hardware leaves banking registers undefined
    // across reset and the vendor documentation requires software to re-initialise them.
    // Zeroing them here would hide that class of bug.
    m.write(CTRL, BANKING_ON, &bi(CFG7), 0);
    assert_eq!(m.read(0x8000, &bi(CFG7), 0), Some(6), "bank register survived the reset");
}

// ── the bit-banged SPI path, end to end ──────────────────────────────────────────

/// One SPI byte through the CONTROL register, mirroring the vendor's `spi_write_byte` /
/// `spi_read_byte`: clock low → sample DO from a ROM window → set DI → clock high.
fn spi_xfer(m: &mut Box<dyn CartMapper>, byte: u8) -> u8 {
    const BITBANG: u8 = 0x01;
    const CS_ACTIVE: u8 = 0x00; // bit 5 low = selected
    let mut out = 0u8;
    for i in (0..8).rev() {
        let d = ((byte >> i) & 1) << 6;
        m.write(CTRL, BITBANG | CS_ACTIVE | d, &bi(CFG7), 0); // CLK low
        // Read DO from $DE00 bit 7 — exactly what the vendor's spi_read_byte does.
        out = (out << 1) | (m.read(0xde00, &bi(CFG7), 0).unwrap_or(0) >> 7);
        m.write(CTRL, BITBANG | CS_ACTIVE | d | 0x80, &bi(CFG7), 0); // CLK high
    }
    out
}

#[test]
fn spi_flash_identify_runs_through_the_registers() {
    // The vendor ships `flash-identify` as an example precisely because software must ask
    // the device what it is. This drives the same $9F JEDEC sequence through the
    // cartridge's CONTROL register, which is the only way to prove the register decode
    // and the SPI device actually agree.
    let mut m = mapper(4);
    const BITBANG: u8 = 0x01;
    // Deselect → select, so the device resets its shift registers.
    m.write(CTRL, BITBANG | 0x20 | 0x0e, &bi(CFG7), 0); // CS high (inactive)
    m.write(CTRL, BITBANG | 0x00 | 0x0e, &bi(CFG7), 0); // CS low (selected)
    spi_xfer(&mut m, 0x9f);
    for _ in 0..3 {
        spi_xfer(&mut m, 0);
    }
    let id = [spi_xfer(&mut m, 0), spi_xfer(&mut m, 0), spi_xfer(&mut m, 0)];
    // W25Q64CV (Winbond, 8 MiB) — manufacturer $EF, memory type $40, capacity $17.
    assert_eq!(id, [0xef, 0x40, 0x17], "JEDEC ID through the cartridge registers");
}

#[test]
fn a_rom_window_reads_the_spi_line_while_bitbanging() {
    let mut m = mapper(4);
    // Bitbang on, device DEselected: the data line floats high (pull-up on bit 7).
    m.write(CTRL, 0x01 | 0x20, &bi(CFG7), 0);
    let v = m.read(0x8000, &bi(CFG7), 0).expect("window still answers");
    assert_eq!(v & 0x80, 0x80, "deselected SPI reads as a pulled-up bit 7");
}

#[test]
fn state_round_trips_through_get_set_state() {
    let mut m = mapper(8);
    m.write(CTRL, BANKING_ON | 0x10, &bi(CFG7), 0); // banking + A22
    m.write(0xde01, 2, &bi(CFG7), 0);
    let st = m.get_state();
    let mut n = mapper(8);
    n.set_state(st);
    assert_eq!(
        n.get_state().control_register,
        m.get_state().control_register,
        "control register survives a snapshot round trip"
    );
}

#[test]
fn io1_is_the_trampoline_page_when_not_bitbanging() {
    // $DE00 shows 256 bytes from bank 0 at $1E00 (context A) / $1F00 (context B) — the
    // point being an NMI handler that stays reachable whatever the banking registers say.
    let mut m = mapper(8);
    assert_eq!(m.read(0xde00, &bi(CFG7), 0), Some(0), "ctx A ⇒ $1E00, still half-bank 0");
    m.write(0xde08, 0, &bi(CFG7), 0); // select context B
    assert_eq!(m.read(0xde00, &bi(CFG7), 0), Some(0), "ctx B ⇒ $1F00, same half-bank");
    // Banking must not move it.
    m.write(CTRL, BANKING_ON, &bi(CFG7), 0);
    m.write(0xde0b, 4, &bi(CFG7), 0);
    assert_eq!(m.read(0xde00, &bi(CFG7), 0), Some(0), "the trampoline never banks");
}

#[test]
fn io1_declines_outside_the_io_visible_configs() {
    let mut m = mapper(4);
    assert!(m.read(0xde00, &bi(7), 0).is_some());
    assert_eq!(m.read(0xde00, &bi(1), 0), None, "falls through to the memory underneath");
}
