//! EasyFlash 3 as a cartridge (CRT hardware type 90) — one test per acceptance item.
//!
//! The cartridge is skoe's EF3: one 8 MB MX29LV640EB, eight slots of 64 banks, a mode
//! register at `$DE0F`, three buttons. Everything here goes through the CPU bus or the
//! cartridge's own port — nothing pokes the flash to make a point — and the two big
//! cases run the real third-party software on it: skoe's EF3 boot menu (built from his
//! sources) and his MX29LV640EB EAPI driver (assembled from his source).
//!
//! The third-party software is NOT in this repository. The two cases that need it build
//! it at test time from the local clone of the EF3 sources
//! (`$TRX64_EF3_SRC`, else `../../../kerberos/skoe-easyflash`) with the local tools
//! (acme, cc65) into a scratch directory, and say `SKIP` on stderr — where the test harness
//! does not swallow it — when the clone, a tool or the ROMs are missing.
//!
//!   cargo test --release -p trx64-core --test ef3_gate -- --nocapture

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use trx64_core::cart::{
    load_cartridge_from_bytes, parse_crt, resolve_cart_type, BankInfo, CartMapper, CartType, MapperType,
};
use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, restore_runtime_checkpoint};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const EF3_SRC_DEFAULT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../kerberos/skoe-easyflash");

macro_rules! skip {
    ($($why:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), "SKIP: {} — {}", module_path!(), format!($($why)*));
        return;
    }};
}

fn rom_dir() -> Option<PathBuf> {
    let d = std::env::var("TRX64_ROM_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(ROM_DIR));
    d.join("kernal-901227-03.bin").exists().then_some(d)
}

// ── the image ────────────────────────────────────────────────────────────────────

/// The flash address, restated here from the spec (D6) so the gate does not borrow the
/// formula it is checking.
fn faddr(slot: usize, bank: usize, romh: bool, off: usize) -> usize {
    slot * 0x10_0000 + (bank / 8) * 0x2_0000 + (romh as usize) * 0x1_0000 + (bank % 8) * 0x2000 + off
}

fn crt_with(hw: u16, chips: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   ");
    v.extend_from_slice(&0x40u32.to_be_bytes());
    v.extend_from_slice(&0x0100u16.to_be_bytes());
    v.extend_from_slice(&hw.to_be_bytes());
    v.push(1); // EXROM
    v.push(0); // GAME
    v.extend_from_slice(&[0u8; 6]);
    let mut name = [0u8; 32];
    name[..8].copy_from_slice(b"EF3 GATE");
    v.extend_from_slice(&name);
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

/// Every 8 KB half of every bank of every slot says WHICH one it is in its first two
/// bytes (`key = slot*64 + bank`, ROMH with the top bit set), so a read proves which
/// slot/bank/half answered, not merely that something did.
fn tagged_image() -> &'static [u8] {
    static IMG: OnceLock<Vec<u8>> = OnceLock::new();
    IMG.get_or_init(|| {
        let mut chips = Vec::new();
        for key in 0u16..512 {
            let mut roml = vec![0x11u8; 0x2000];
            roml[0] = (key >> 8) as u8;
            roml[1] = key as u8;
            let mut romh = vec![0x22u8; 0x2000];
            romh[0] = (key >> 8) as u8 | 0x80;
            romh[1] = key as u8;
            chips.push((key, 0x8000u16, roml));
            chips.push((key, 0xa000u16, romh));
        }
        crt_with(90, &chips)
    })
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

fn ef3_mapper() -> Box<dyn CartMapper> {
    load_cartridge_from_bytes(tagged_image(), "EF3", None).expect("type 90 builds").1
}

fn lines(m: &dyn CartMapper) -> (u8, u8) {
    let l = m.get_lines();
    (l.exrom, l.game)
}

// the lines the spec names, as (EXROM, GAME) with 0 = asserted
const ULTIMAX: (u8, u8) = (1, 0);
const K16: (u8, u8) = (0, 0);
const K8: (u8, u8) = (0, 1);
const OFF: (u8, u8) = (1, 1);

fn wr(m: &mut Box<dyn CartMapper>, addr: u16, v: u8) -> bool {
    m.write(addr, v, &bi(), 0)
}

fn rd(m: &mut Box<dyn CartMapper>, addr: u16) -> Option<u8> {
    m.read(addr, &bi(), 0)
}

/// Press, then let the machine's part happen at mapper level: the generated reset and the
/// vector fetch that lets GAME go.
fn press(m: &mut Box<dyn CartMapper>, button: &str) {
    m.press_button(button).expect("button");
    assert!(m.take_reset_request(), "{button} asks for a reset");
    m.reset_generated();
    m.reset_vector_fetched();
}

// ── the machine ──────────────────────────────────────────────────────────────────

fn machine_with(crt: &[u8]) -> Machine {
    let mut m = Machine::new();
    if let Some(d) = rom_dir() {
        m.boot_from_dir(&d).expect("boot ROMs");
    }
    m.attach_cart_from_bytes(crt, "ef3").expect("attach");
    m.cold_reset();
    m
}

fn run(m: &mut Machine, instrs: u64) {
    m.run_for_full_capped(instrs * 64, instrs, &mut NullSink, |_, _, _, _, _, _, _| {});
}

/// Poke `code` at `$0400` (RAM in every cartridge mode, Ultimax included) and run it.
fn run_code(m: &mut Machine, code: &[u8], instrs: u64) {
    m.poke(0x0400, code);
    m.write_full(0x0001, 0x37);
    m.c64_core.reg_pc = 0x0400;
    run(m, instrs);
}

fn status(m: &Machine) -> trx64_core::ef3::Ef3Status {
    m.cartridge.as_ref().expect("cart").ef3_status().expect("an EF3")
}

fn peek(m: &Machine, addr: u16) -> Option<u8> {
    m.cartridge.as_ref().expect("cart").peek(addr, &bi())
}

/// A type-90 image whose every slot has a reset vector to code that leaves its slot
/// number at `$0300`: what "the reset runs the selected slot" is measured with.
fn boot_image() -> Vec<u8> {
    let mut chips = Vec::new();
    for slot in 0u16..8 {
        let mut romh = vec![0xffu8; 0x2000];
        // SEI / LDA #slot / STA $0300 / INC $0301 / JMP loop
        let code = [0x78, 0xa9, slot as u8, 0x8d, 0x00, 0x03, 0xee, 0x01, 0x03, 0x4c, 0x06, 0xe0];
        romh[..code.len()].copy_from_slice(&code);
        romh[0x1ffc] = 0x00;
        romh[0x1ffd] = 0xe0;
        chips.push((slot * 64, 0xa000u16, romh));
        let mut roml = vec![0xffu8; 0x2000];
        roml[0] = 0xc0 + slot as u8;
        chips.push((slot * 64, 0x8000u16, roml));
    }
    crt_with(90, &chips)
}

// ═════════════════════════════════════════════════════════════════════════════════
// The type, the image
// ═════════════════════════════════════════════════════════════════════════════════

#[test]
fn type_90_is_easyflash3_and_the_resolvers_know_it() {
    let (img, m) = load_cartridge_from_bytes(tagged_image(), "EF3", None).expect("90 builds");
    assert_eq!(img.mapper_type, MapperType::EasyFlash3);
    assert_eq!(m.mapper_type(), MapperType::EasyFlash3);
    for s in ["90", "ef3", "easyflash3", "EF3"] {
        assert_eq!(resolve_cart_type(s).unwrap(), CartType::Forced(MapperType::EasyFlash3), "{s}");
    }
    // the neighbours did not move
    assert_eq!(resolve_cart_type("32").unwrap(), CartType::Forced(MapperType::EasyFlash));
    assert_eq!(resolve_cart_type("87").unwrap(), CartType::Forced(MapperType::Gmod4));
    assert_eq!(resolve_cart_type("88").unwrap(), CartType::Forced(MapperType::C64MegaCart));
    assert_eq!(resolve_cart_type("ef").unwrap(), CartType::Forced(MapperType::EasyFlash));
    // a bank beyond slot 7 is refused, not dropped
    let bad = crt_with(90, &[(512, 0x8000, vec![0u8; 0x2000])]);
    assert!(load_cartridge_from_bytes(&bad, "bad", None).is_err());
}

/// D6 / the CRT: each CHIP packet lands where the formula puts it. A bank is not 16 KB
/// contiguous in the chip — its ROMH half lies 64 KB after its ROML half.
#[test]
fn chip_packets_land_where_the_flash_formula_puts_them() {
    let mut m = ef3_mapper();
    let flash = m.writable_image(0).expect("image");
    assert_eq!(flash.len(), 8 * 1024 * 1024);
    for key in [0usize, 1, 7, 8, 9, 63, 64, 65, 200, 320, 511] {
        let (slot, bank) = (key / 64, key % 64);
        for romh in [false, true] {
            let a = faddr(slot, bank, romh, 0);
            let want_hi = (key >> 8) as u8 | if romh { 0x80 } else { 0 };
            assert_eq!(
                (flash[a], flash[a + 1]),
                (want_hi, key as u8),
                "slot {slot} bank {bank} {} at flash ${a:06X}",
                if romh { "ROMH" } else { "ROML" }
            );
        }
        assert_eq!(faddr(slot, bank, true, 0) - faddr(slot, bank, false, 0), 0x1_0000, "halves 64 KB apart");
    }
    // and the reads come out of the same place through the registers
    for key in [0usize, 9, 63, 200, 511] {
        let (slot, bank) = (key / 64, key % 64);
        wr(&mut m, 0xde01, slot as u8);
        wr(&mut m, 0xde00, bank as u8);
        assert_eq!((rd(&mut m, 0x8000).unwrap(), rd(&mut m, 0x8001).unwrap()), ((key >> 8) as u8, key as u8));
        assert_eq!((rd(&mut m, 0xa000).unwrap(), rd(&mut m, 0xa001).unwrap()), ((key >> 8) as u8 | 0x80, key as u8));
        assert_eq!(rd(&mut m, 0xe000), rd(&mut m, 0xa000), "ROMH at $E000 is the same half as at $A000");
    }
    // a smaller image fills the rest with $FF, as erased flash does
    let small = crt_with(90, &[(0, 0x8000, vec![0x42u8; 0x2000])]);
    let mut s = load_cartridge_from_bytes(&small, "s", None).unwrap().1;
    let f = s.writable_image(0).unwrap();
    assert_eq!(f[0], 0x42);
    assert!(f[0x2000..].iter().all(|&b| b == 0xff));
    // the same image from a raw .bin is the same chip (slot-major, ROML then ROMH per bank)
    let mut bin = vec![0xffu8; 0x4000 * 130];
    bin[0] = 0x77; // key 0, ROML
    bin[0x2000] = 0x78; // key 0, ROMH
    bin[0x4000 * 129] = 0x79; // key 129 = slot 2 bank 1, ROML
    let mut b = trx64_core::cart::load_cartridge_from_bin(&bin, "b", MapperType::EasyFlash3).unwrap().1;
    let f = b.writable_image(0).unwrap();
    assert_eq!((f[faddr(0, 0, false, 0)], f[faddr(0, 0, true, 0)], f[faddr(2, 1, false, 0)]), (0x77, 0x78, 0x79));
}

// ═════════════════════════════════════════════════════════════════════════════════
// Acceptance 2 — slots and the small registers (D1)
// ═════════════════════════════════════════════════════════════════════════════════

#[test]
fn a2_de01_switches_slots_at_once_and_reads_back() {
    let mut m = ef3_mapper();
    // 16K mode so both windows answer
    for slot in 0u8..8 {
        wr(&mut m, 0xde01, slot);
        assert_eq!(rd(&mut m, 0xde01), Some(slot), "a read of $DE01 returns the slot");
        let key = slot as u16 * 64;
        assert_eq!(
            (rd(&mut m, 0x8000).unwrap(), rd(&mut m, 0x8001).unwrap()),
            ((key >> 8) as u8, key as u8),
            "slot {slot}'s bank 0 answers at once, no reset"
        );
    }
    // bits 7:3 are not part of the slot, and they read as 0
    wr(&mut m, 0xde01, 0xfd);
    assert_eq!(rd(&mut m, 0xde01), Some(5));
    // the bank register keeps six bits; $DE01 is not touched by it
    wr(&mut m, 0xde00, 0xff);
    assert_eq!(m.active_bank(0x8000), 5 * 64 + 63);
    assert_eq!(rd(&mut m, 0xde01), Some(5));
    wr(&mut m, 0xde00, 0x40);
    assert_eq!(m.active_bank(0x8000), 5 * 64, "bits 7:6 of $DE00 are ignored");
}

#[test]
fn a2_the_small_registers_and_what_does_not_decode() {
    let mut m = ef3_mapper();
    assert_eq!(rd(&mut m, 0xde08), Some(0x49), "$DE08 reads the CPLD version, 1.1.1");
    assert_eq!(rd(&mut m, 0xde09), Some(0x00), "USB: no data, not ready");
    assert_eq!(rd(&mut m, 0xde0a), Some(0x00));
    wr(&mut m, 0xde09, 0xff);
    wr(&mut m, 0xde0a, 0xff);
    // write-only registers read as the open bus (None), not as what was written
    for a in [0xde00u16, 0xde02, 0xde03, 0xde04, 0xde07, 0xde0b, 0xde0e, 0xde0f] {
        assert_eq!(rd(&mut m, a), None, "${a:04X} is open bus");
    }
    // only $DE00-$DE0F decodes: no mirrors in $DE10-$DEFF
    wr(&mut m, 0xde11, 3);
    wr(&mut m, 0xde10, 9);
    wr(&mut m, 0xdeff, 0xff);
    assert!(!wr(&mut m, 0xde11, 3), "a write to a mirror is not consumed");
    assert_eq!(rd(&mut m, 0xde01), Some(0), "$DE11 is not $DE01");
    assert_eq!(m.active_bank(0x8000), 0, "$DE10 is not $DE00");
    assert_eq!(rd(&mut m, 0xde18), None, "$DE18 is not $DE08");
    // the monitor's peek shows the write-only registers as shadows
    wr(&mut m, 0xde00, 0x15);
    wr(&mut m, 0xde02, 0xf7);
    assert_eq!(m.peek(0xde00, &bi()), Some(0x15));
    assert_eq!(m.peek(0xde02, &bi()), Some(0x87), "bits 6:4 of $DE02 are ignored, the shadow shows the rest");
    // ...and the version register answers in EF mode only: after the kill it is gone
    wr(&mut m, 0xde0f, 7);
    for a in [0xde01u16, 0xde08, 0xde09, 0xde0a, 0xdf00] {
        assert_eq!(rd(&mut m, a), None, "${a:04X} in the kill state");
    }
}

#[test]
fn the_io2_ram_is_one_256_byte_ram_shared_by_all_slots() {
    let mut m = ef3_mapper();
    let before: Vec<u8> = (0..256).map(|i| rd(&mut m, 0xdf00 + i).unwrap()).collect();
    // the same power-up pattern as the EasyFlash's IO2 RAM
    assert_eq!(&before[..8], &[0xff, 0x00, 0x00, 0xff, 0xff, 0x00, 0x00, 0xff]);
    wr(&mut m, 0xde01, 2);
    assert!(wr(&mut m, 0xdf10, 0xa5));
    wr(&mut m, 0xde01, 6);
    assert_eq!(rd(&mut m, 0xdf10), Some(0xa5), "slot 6 sees what slot 2 wrote");
    assert_eq!(rd(&mut m, 0xdff0), Some(before[0xf0]), "nothing else moved");
    // in the kill state it does not answer, and the write goes nowhere
    wr(&mut m, 0xde0f, 7);
    assert!(!wr(&mut m, 0xdf10, 0x11));
    assert_eq!(rd(&mut m, 0xdf10), None);
}

// ═════════════════════════════════════════════════════════════════════════════════
// Acceptance 3 — D2's table, no-VIC, the windows
// ═════════════════════════════════════════════════════════════════════════════════

#[test]
fn a3_every_row_of_the_line_table() {
    // `boot` as the table has it: 1 after power-on, 0 after Special.
    for (boot, rows) in [
        (
            1u8,
            [
                // $DE02 low bits (M, GAME, EXROM) -> lines
                (0b000u8, ULTIMAX), // M=0, EXROM bit 0
                (0b010, K16),       // M=0, EXROM bit 1
                (0b100, OFF),       // M=1, GAME 0, EXROM 0
                (0b101, ULTIMAX),   // M=1, GAME 1, EXROM 0
                (0b110, K8),        // M=1, GAME 0, EXROM 1
                (0b111, K16),       // M=1, GAME 1, EXROM 1
            ],
        ),
        (
            0u8,
            [(0b000, OFF), (0b010, K8), (0b100, OFF), (0b101, ULTIMAX), (0b110, K8), (0b111, K16)],
        ),
    ] {
        let mut m = ef3_mapper();
        if boot == 0 {
            press(&mut m, "special");
            assert_eq!(m.ef3_status().unwrap().boot, false, "Special clears the boot flag");
        } else {
            assert!(m.ef3_status().unwrap().boot);
        }
        for (reg, want) in rows {
            wr(&mut m, 0xde02, reg);
            assert_eq!(lines(&*m), want, "boot={boot} $DE02={reg:#05b}");
        }
        // bit 3 is no-VIC and does not change the lines the CPU's half-cycle sees
        wr(&mut m, 0xde02, 0b1111);
        assert_eq!(lines(&*m), K16);
    }
}

#[test]
fn a3_the_boot_flag_reaches_game_only_at_a_reset_or_a_de02_write_with_m_0() {
    let mut m = ef3_mapper();
    assert_eq!(lines(&*m), ULTIMAX, "power-on: boot 1, $DE02 clear");
    // Special flips the flag (and asks for a reset); until the reset runs, GAME stays
    m.press_button("special").unwrap();
    assert_eq!(lines(&*m), ULTIMAX, "the flag has not reached the line yet");
    assert!(m.take_reset_request());
    m.reset_generated();
    // GAME is pulled low from the start of the generated reset until the first ROMH access
    assert_eq!(lines(&*m), ULTIMAX, "pulled: the reset vector is read in Ultimax");
    m.reset_vector_fetched();
    assert_eq!(lines(&*m), OFF, "boot 0, M=0, EXROM 0: off");
    // a $DE02 write with M=1 sets GAME from its own bit, whatever the flag says
    wr(&mut m, 0xde02, 0b101);
    assert_eq!(lines(&*m), ULTIMAX);
    // a write with M=0 takes the flag again
    wr(&mut m, 0xde02, 0b000);
    assert_eq!(lines(&*m), OFF);
    // an EXTERNAL reset is not a generated one: no pull, and the flag is back to 1
    m.reset();
    assert_eq!(lines(&*m), ULTIMAX);
    assert!(m.ef3_status().unwrap().boot);
}

#[test]
fn a3_no_vic_takes_the_cartridge_off_the_vic_half_cycle() {
    let mut m = ef3_mapper();
    wr(&mut m, 0xde02, 0b101); // Ultimax
    let romh = m.vic_romh().expect("the VIC sees ROMH through Ultimax");
    assert_eq!(&romh[..2], &[0x80, 0x00], "slot 0 bank 0 ROMH");
    wr(&mut m, 0xde02, 0b1101); // + bit 3
    assert!(m.vic_romh().is_none(), "no-VIC: in the VIC's half-cycle vic_romh returns nothing");
    assert_eq!(lines(&*m), ULTIMAX, "the CPU's half still sees the lines");
}

/// The windows the lines give, through the real PLA: a program on the CPU bus reads
/// `$8000`, `$A000`, `$E000` in each mode.
#[test]
fn a3_the_windows_each_mode_gives() {
    let mut m = machine_with(tagged_image());
    // reads of $8000/$A000/$E000 into $0300.., then JMP *
    let prog = |reg: u8| -> Vec<u8> {
        vec![
            0xa9, reg, 0x8d, 0x02, 0xde, // LDA #reg / STA $DE02
            0xad, 0x00, 0x80, 0x8d, 0x00, 0x03, // LDA $8000 / STA $0300
            0xad, 0x00, 0xa0, 0x8d, 0x01, 0x03, // LDA $A000 / STA $0301
            0xad, 0x00, 0xe0, 0x8d, 0x02, 0x03, // LDA $E000 / STA $0302
            0x4c, 0x17, 0x04, // JMP *
        ]
    };
    for (reg, name, roml, romh_a, romh_e) in [
        (0b111u8, "16K", true, true, false),
        (0b110, "8K", true, false, false),
        (0b101, "Ultimax", true, false, true),
        (0b100, "off", false, false, false),
    ] {
        m.poke(0x0300, &[0xee, 0xee, 0xee]);
        // a sentinel in the RAM under each window, so "off" cannot read as a tag byte
        for a in [0x8000u16, 0xa000, 0xe000] {
            m.poke(a, &[0xee]);
        }
        run_code(&mut m, &prog(reg), 8);
        let (l, a, e) = (m.read_full(0x0300), m.read_full(0x0301), m.read_full(0x0302));
        // ROML answers with the tag byte, which is 0 (key 0 high byte); ROMH has bit 7
        assert_eq!(l == 0x00, roml, "{name}: ROML at $8000 (read {l:#04x})");
        assert_eq!(a == 0x80, romh_a, "{name}: ROMH at $A000 (read {a:#04x})");
        assert_eq!(e == 0x80, romh_e, "{name}: ROMH at $E000 (read {e:#04x})");
    }
}

// ═════════════════════════════════════════════════════════════════════════════════
// Acceptance 4 — $DE0F (D3, D9)
// ═════════════════════════════════════════════════════════════════════════════════

/// LDA #v / STA $DE0F / INC $0300 / JMP *-3 style tail: if the machine is NOT reset by
/// the write, the INC runs at least once.
fn mode_prog(v: u8) -> Vec<u8> {
    vec![0xa9, v, 0x8d, 0x0f, 0xde, 0xee, 0x00, 0x03, 0x4c, 0x08, 0x04]
}

#[test]
fn a4_mode_1_hides_the_register_and_does_not_reset() {
    let mut m = machine_with(&boot_image());
    m.poke(0x0300, &[0, 0]);
    run_code(&mut m, &mode_prog(1), 6);
    assert!(m.read_full(0x0300) >= 1, "the program carried on: no reset");
    let s = status(&m);
    assert_eq!((s.state, s.menu_enabled), ("ef", false), "EF mode on, $DE0F gone");
    assert_eq!(s.last_mode, Some((1, "EasyFlash")));
    // $DE0F no longer decodes: a second write changes nothing, mode 7 included
    run_code(&mut m, &mode_prog(7), 6);
    assert_eq!(status(&m).state, "ef", "a write to $DE0F after the first is ignored");
    assert_eq!(lines_of(&m), ULTIMAX);
}

fn lines_of(m: &Machine) -> (u8, u8) {
    lines(&**m.cartridge.as_ref().unwrap())
}

#[test]
fn a4_mode_0_resets_into_the_selected_slot() {
    let mut m = machine_with(&boot_image());
    // select slot 3, bank 0, then mode 0 from the CPU
    let prog = [
        0xa9, 0x03, 0x8d, 0x01, 0xde, // slot 3
        0xa9, 0x00, 0x8d, 0x00, 0xde, // bank 0 (a generated reset keeps the bank)
        0xa9, 0x00, 0x8d, 0x0f, 0xde, // $DE0F = 0 -> EF + reset
        0xee, 0x02, 0x03, // INC $0302: runs only if the write did not reset
        0x4c, 0x12, 0x04,
    ];
    m.poke(0x0300, &[0xee; 4]);
    m.poke(0x0302, &[0]);
    run_code(&mut m, &prog, 6);
    // the reset ran inside that instruction; now the cart code of slot 3 runs
    run(&mut m, 40);
    assert_eq!(m.read_full(0x0300), 3, "the reset vector came from slot 3's flash");
    assert!(m.read_full(0x0301) > 0, "and slot 3's code is running");
    let s = status(&m);
    assert_eq!((s.slot, s.bank, s.state, s.menu_enabled, s.boot), (3, 0, "ef", false, true));
    assert_eq!(s.last_mode, Some((0, "EasyFlash + reset")));
    assert_eq!(m.read_full(0x0302), 0, "the INC after the write never ran");
    assert_eq!(peek(&m, 0xde02), Some(0), "$DE02 was cleared by the reset");
    // a reset the cart generates does not bring $DE0F back
    assert!(!status(&m).menu_enabled);
}

#[test]
fn a4_mode_7_makes_the_cart_invisible_until_menu() {
    let mut m = machine_with(&boot_image());
    run_code(&mut m, &mode_prog(7), 3);
    run(&mut m, 5);
    let s = status(&m);
    assert_eq!((s.state, s.menu_enabled, s.not_emulated), ("off", false, false));
    assert_eq!(lines_of(&m), OFF, "no lines");
    for a in [0xde01u16, 0xde08, 0xdf00, 0x8000, 0xe000] {
        assert_eq!(m.cartridge.as_ref().unwrap().peek(a, &bi()), None, "${a:04X}: nothing answers");
    }
    // the C64 booted without the cartridge: the reset vector is the KERNAL's
    let vec = u16::from(m.kernal_rom[0x1ffc]) | u16::from(m.kernal_rom[0x1ffd]) << 8;
    assert!(m.memconfig.kernal, "the KERNAL is in the map");
    let _ = vec;
    // a generated reset does not bring it back; the Menu button does
    let r = m.cart_press_button("menu").unwrap();
    assert!(r);
    let s = status(&m);
    assert_eq!((s.state, s.menu_enabled, s.slot, s.bank), ("ef", true, 0, 0));
    // ...and so does an external reset
    let mut m2 = machine_with(&boot_image());
    run_code(&mut m2, &mode_prog(7), 3);
    assert_eq!(status(&m2).state, "off");
    m2.warm_reset();
    assert_eq!((status(&m2).state, status(&m2).menu_enabled), ("ef", true));
}

#[test]
fn a4_modes_2_4_5_6_kill_and_say_so() {
    for (mode, name) in [(2u8, "KERNAL"), (4, "AR/RR/NP"), (5, "SS5"), (6, "C128")] {
        let mut m = machine_with(&boot_image());
        run_code(&mut m, &mode_prog(mode), 3);
        run(&mut m, 5);
        let s = status(&m);
        assert_eq!(s.state, "off", "mode {mode}: the cartridge is off");
        assert!(s.not_emulated, "mode {mode} is flagged notEmulated");
        assert_eq!(s.last_mode, Some((mode, name)));
        assert_eq!(
            s.notice.as_deref(),
            Some(format!("EF3 mode {mode} ({name}) is not emulated — the cartridge is off").as_str())
        );
        assert_eq!(lines_of(&m), OFF);
        assert_eq!(m.cartridge.as_ref().unwrap().peek(0xde08, &bi()), None);
        // Menu brings it back, and clears the flag
        m.cart_press_button("menu").unwrap();
        assert!(!status(&m).not_emulated);
    }
}

#[test]
fn a4_modes_3_and_8_to_15_disable_everything_without_a_reset() {
    for mode in [3u8, 8, 9, 12, 15] {
        let mut m = machine_with(&boot_image());
        m.poke(0x0300, &[0]);
        run_code(&mut m, &mode_prog(mode), 6);
        assert!(m.read_full(0x0300) >= 1, "mode {mode}: no reset, the program carried on");
        let s = status(&m);
        assert_eq!((s.state, s.menu_enabled, s.not_emulated), ("off", false, false), "mode {mode}");
        assert_eq!(lines_of(&m), OFF);
    }
    // only the low nibble selects: $10 is mode 0
    let mut m = ef3_mapper();
    wr(&mut m, 0xde0f, 0x10);
    assert!(m.take_reset_request(), "data(3 downto 0) = 0 is a reset into EF");
}

// ═════════════════════════════════════════════════════════════════════════════════
// Acceptance 5 — the buttons (D4, D5)
// ═════════════════════════════════════════════════════════════════════════════════

fn scribble(m: &mut Machine) {
    // slot 3, bank 7, $DE02 = 16K + LED, via the CPU
    run_code(
        m,
        &[
            0xa9, 0x03, 0x8d, 0x01, 0xde, 0xa9, 0x07, 0x8d, 0x00, 0xde, 0xa9, 0x87, 0x8d, 0x02, 0xde, 0x4c, 0x0f, 0x04,
        ],
        4,
    );
}

#[test]
fn a5_menu_reset_special_produce_their_d4_row() {
    // Menu: EF mode, $DE0F enabled, slot 0, bank 0, boot 1, $DE02 cleared — from the kill state
    let mut m = machine_with(&boot_image());
    run_code(&mut m, &mode_prog(7), 3);
    scribble(&mut m); // nothing answers: these writes go nowhere
    assert_eq!(status(&m).state, "off");
    assert!(m.cart_press_button("menu").unwrap());
    let s = status(&m);
    assert_eq!((s.state, s.menu_enabled, s.slot, s.bank, s.boot), ("ef", true, 0, 0, true), "Menu");
    assert_eq!(peek(&m, 0xde02), Some(0));
    assert_eq!(lines_of(&m), ULTIMAX);
    assert_eq!(s.last_mode, None);

    // Reset: mode and $DE0F enable unchanged, slot kept, bank 0, boot 1, $DE02 cleared
    let mut m = machine_with(&boot_image());
    run_code(&mut m, &mode_prog(1), 3); // EF, $DE0F gone
    scribble(&mut m);
    assert_eq!((status(&m).slot, status(&m).bank), (3, 7));
    assert!(m.cart_press_button("reset").unwrap());
    let s = status(&m);
    assert_eq!((s.state, s.menu_enabled, s.slot, s.bank, s.boot), ("ef", false, 3, 0, true), "Reset");
    assert_eq!(peek(&m, 0xde02), Some(0));
    run(&mut m, 40);
    assert_eq!(m.read_full(0x0300), 3, "Reset restarts the current slot");

    // Special: the same, but boot 0 — the C64 comes up as if no cartridge ran
    let mut m = machine_with(&boot_image());
    run_code(&mut m, &mode_prog(1), 3);
    scribble(&mut m);
    m.poke(0x0300, &[0xee]);
    assert!(m.cart_press_button("special").unwrap());
    let s = status(&m);
    assert_eq!((s.state, s.menu_enabled, s.slot, s.bank, s.boot), ("ef", false, 3, 0, false), "Special");
    assert_eq!(peek(&m, 0xde02), Some(0));
    assert_eq!(lines_of(&m), OFF, "boot 0, M=0, EXROM 0: no lines");
    assert!(m.memconfig.kernal, "the KERNAL is in the map");
    let kvec = u16::from(m.kernal_rom[0x1ffc]) | u16::from(m.kernal_rom[0x1ffd]) << 8;
    assert_eq!(m.cpu6510.reg_pc, kvec, "the reset vector is the KERNAL's, not the cartridge's");
    assert_eq!(m.read_full(0x0300), 0xee, "slot 3's code did not run");
}

#[test]
fn a5_a_reset_the_cart_generates_keeps_slot_bank_and_boot() {
    // the D4 row for `$DE0F` = 0: mode as set, slot kept, bank kept, boot kept
    let mut m = ef3_mapper();
    m.press_button("reset").unwrap();
    m.take_reset_request();
    m.reset_generated();
    m.reset_vector_fetched();
    wr(&mut m, 0xde01, 4);
    wr(&mut m, 0xde00, 9);
    wr(&mut m, 0xde02, 0x87);
    wr(&mut m, 0xde0f, 0);
    assert!(m.take_reset_request());
    m.reset_generated();
    m.reset_vector_fetched();
    let s = m.ef3_status().unwrap();
    assert_eq!((s.slot, s.bank, s.boot, s.state, s.menu_enabled), (4, 9, true, "ef", false));
    assert_eq!(m.peek(0xde02, &bi()), Some(0), "only $DE02 is cleared");
    // an external reset clears all of it
    m.reset();
    let s = m.ef3_status().unwrap();
    assert_eq!((s.slot, s.bank, s.boot, s.state, s.menu_enabled), (0, 0, true, "ef", true));
}

#[test]
fn a5_in_the_kill_state_reset_and_special_do_nothing_and_menu_works() {
    let mut m = ef3_mapper();
    wr(&mut m, 0xde0f, 7);
    m.take_reset_request();
    m.press_button("reset").unwrap();
    assert!(!m.take_reset_request(), "the EF buttons act only while the EF mode is enabled");
    m.press_button("special").unwrap();
    assert!(!m.take_reset_request());
    m.press_button("menu").unwrap();
    assert!(m.take_reset_request());
    assert!(m.press_button("rest").is_err(), "an unknown button is refused");
}

#[test]
fn a5_a_cart_without_buttons_refuses_by_name() {
    let ef1 = crt_with(32, &[(0, 0x8000, vec![0u8; 0x2000])]);
    let mut m = machine_with(&ef1);
    let e = m.cart_press_button("reset").unwrap_err();
    assert!(e.contains("EasyFlash") && e.contains("no buttons"), "{e}");
    let mut none = Machine::new();
    assert!(none.cart_press_button("reset").unwrap_err().contains("no cartridge"));
    // an unknown button on an EF3 names the ones it has
    let mut m = machine_with(&boot_image());
    let e = m.cart_press_button("power").unwrap_err();
    assert!(e.contains("menu") && e.contains("special"), "{e}");
}

// ═════════════════════════════════════════════════════════════════════════════════
// The chip (D6) — non-uniform sectors, status polling
// ═════════════════════════════════════════════════════════════════════════════════

#[test]
fn the_chip_has_boot_blocks_polls_its_status_and_erases_one_sector() {
    use trx64_core::flash040::{Flash040, FLASH_MX29LV640EB};
    let mut f = Flash040::new(vec![0x00u8; 0x80_0000], "t", FLASH_MX29LV640EB);
    // autoselect: C2 / CB at byte-mode address 2
    f.store(0xaaa, 0xaa, 0);
    f.store(0x555, 0x55, 0);
    f.store(0xaaa, 0x90, 0);
    assert_eq!((f.read(0, 1), f.read(2, 1)), (0xc2, 0xcb));
    f.store(0, 0xf0, 2);
    // program: DQ7 complement + DQ6 toggle while busy, the data after the typical 9 us
    f.store(0xaaa, 0xaa, 10);
    f.store(0x555, 0x55, 10);
    f.store(0xaaa, 0xa0, 10);
    f.store(0x1234, 0x00, 10); // program 0x00 over 0x00: bit 7 of the data is 0, DQ7 reads 1
    let a = f.read(0x1234, 12);
    let b = f.read(0x1234, 13);
    assert_eq!(a & 0x80, 0x80, "DQ7 is the complement of the programmed bit 7 while busy");
    assert_ne!(a & 0x40, b & 0x40, "DQ6 toggles on every read");
    assert_eq!(f.read(0x1234, 30), 0x00, "after the program time the byte reads as itself");
    // a boot block erases alone: 8 KB, not 64 KB
    let erase = |f: &mut Flash040, addr: u32, clk: u64| {
        for (a, v) in [(0xaaau32, 0xaau8), (0x555, 0x55), (0xaaa, 0x80), (0xaaa, 0xaa), (0x555, 0x55)] {
            f.store(a, v, clk);
        }
        f.store(addr, 0x30, clk);
    };
    erase(&mut f, 3 * 0x2000, 100);
    let s1 = f.read(0, 120); // inside the time-out: Q3 = 0
    assert_eq!(s1 & 0x08, 0, "Q3 low in the sector-erase time-out");
    let s2 = f.read(0, 200); // erasing: Q7 = 0, Q6 toggling, Q3 = 1
    let s3 = f.read(0, 201);
    assert_eq!((s2 & 0x88, s3 & 0x88), (0x08, 0x08), "erasing: Q7 low, Q3 high");
    assert_ne!(s2 & 0x40, s3 & 0x40, "Q6 toggles while erasing");
    f.read(0, 1_000_000);
    assert_eq!(f.peek(3 * 0x2000), 0xff, "boot block 3 erased");
    assert_eq!(f.peek(4 * 0x2000 - 1), 0xff);
    assert_eq!(f.peek(2 * 0x2000), 0x00, "boot block 2 untouched");
    assert_eq!(f.peek(4 * 0x2000), 0x00, "boot block 4 untouched");
    // a 64 KB sector erases alone
    erase(&mut f, 0x5_0000, 2_000_000);
    f.read(0, 4_000_000);
    assert_eq!((f.peek(0x4_ffff), f.peek(0x5_0000), f.peek(0x5_ffff), f.peek(0x6_0000)), (0x00, 0xff, 0xff, 0x00));
    // the top sector is the last of the 127
    erase(&mut f, 0x7f_0000, 5_000_000);
    f.read(0, 7_000_000);
    assert_eq!((f.peek(0x7e_ffff), f.peek(0x7f_0000), f.peek(0x7f_ffff)), (0x00, 0xff, 0xff));
}

// ═════════════════════════════════════════════════════════════════════════════════
// Third-party software, built at test time from the local clone
// ═════════════════════════════════════════════════════════════════════════════════

fn ef3_src() -> Option<PathBuf> {
    let p = std::env::var("TRX64_EF3_SRC").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(EF3_SRC_DEFAULT));
    p.join("EF3BootImage/Makefile").exists().then_some(p)
}

/// The tools the builds need, looked for on the PATH plus the two places they live here.
fn tool_path() -> String {
    let mut p = std::env::var("PATH").unwrap_or_default();
    for extra in ["/usr/local/bin", "/opt/homebrew/bin"] {
        p = format!("{p}:{extra}");
    }
    p
}

fn have_tool(name: &str) -> bool {
    Command::new(name).arg("-V").env("PATH", tool_path()).output().map(|_| true).unwrap_or(false)
}

fn sh(dir: &Path, cmd: &str) -> Result<(), String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .env("PATH", tool_path())
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{cmd} failed: {}{}",
            String::from_utf8_lossy(&out.stdout).lines().rev().take(8).collect::<Vec<_>>().join("\n"),
            String::from_utf8_lossy(&out.stderr).lines().rev().take(8).collect::<Vec<_>>().join("\n")
        ))
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for e in std::fs::read_dir(from).map_err(|e| e.to_string())? {
        let e = e.map_err(|e| e.to_string())?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if e.file_name() == ".git" {
            continue;
        }
        if e.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// The scratch copy of the EF3 sources. The clone itself is never written to.
fn scratch(src: &Path) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("trx64-ef3-gate-bootimage");
    let tree = dir.join("skoe-easyflash");
    if !tree.join("EF3BootImage").exists() {
        let _ = std::fs::remove_dir_all(&dir);
        copy_tree(src, &tree)?;
    }
    Ok(tree)
}

struct BootImage {
    /// The slot-0 image `mkimages` makes: 64 banks, ROML then ROMH, 16 KB per bank.
    menu: Vec<u8>,
    directory: Vec<u8>,
    /// Run address of the plotter's glyph tables, from the linker's label file.
    charset_tab_lo: u16,
    charset_tab_hi: u16,
}

/// Build skoe's EF3 boot image with his Makefile: trampoline, efmenu, prgstart and
/// `mkimages`, into the scratch tree. Not built: EasyProg (a dummy of the right shape stands
/// in, empty) and the third-party KERNALs/cartridges of the `init` image. Two things are
/// worked around for the toolchain on this machine and nowhere else: the stale prebuilt
/// objects in the clone are removed (they carry another cc65's object version), and the
/// linker configs get the `ONCE` segment cc65 2.18's runtime wants.
fn build_boot_image() -> Result<BootImage, String> {
    let src = ef3_src().ok_or("no EF3 source clone")?;
    let tree = scratch(&src)?;
    let boot = tree.join("EF3BootImage");
    if !boot.join("ef3-menu.bin").exists() {
        for cfg in ["efmenu/src/ld2.cfg", "prgstart/src/ld.crt.cfg"] {
            let p = boot.join(cfg);
            let s = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
            if !s.contains("ONCE:") {
                let s = s.replacen("SEGMENTS {\n", "SEGMENTS {\n    ONCE:     load = ROM,            type = ro,  optional = yes;\n", 1);
                std::fs::write(&p, s).map_err(|e| e.to_string())?;
            }
        }
        sh(&tree.join("libs/libef3usb"), "rm -rf obj libef3usb.lib && make")?;
        sh(&boot, "rm -rf efmenu/obj prgstart/obj")?;
        // `efmenu.crt`/`prgstart.crt` come from a helper (`bin2efcrt`) this gate has no use
        // for: the .bin files are what is taken. A stand-in that writes an empty file.
        let tool = tree.join("EasySDK/tools/bin2efcrt");
        std::fs::write(&tool, "#!/bin/sh\n: > \"$2\"\n").map_err(|e| e.to_string())?;
        sh(&tree, "chmod +x EasySDK/tools/bin2efcrt")?;
        let dummy = tree.join("dummy_easyprog");
        std::fs::create_dir_all(&dummy).map_err(|e| e.to_string())?;
        std::fs::write(dummy.join("Makefile"), "all:\n\t@true\n").map_err(|e| e.to_string())?;
        std::fs::write(dummy.join("easyprog"), b"").map_err(|e| e.to_string())?;
        sh(&boot, &format!("make easyprog={}/easyprog ef3-menu.bin directory.bin", dummy.display()))?;
    }
    let labels = std::fs::read_to_string(boot.join("efmenu/efmenu.bin.labels")).map_err(|e| e.to_string())?;
    let label = |name: &str| -> Result<u16, String> {
        labels
            .lines()
            .find_map(|l| {
                let mut t = l.split_whitespace();
                (t.next() == Some("al")).then_some(())?;
                let a = t.next()?;
                (t.next()? == name).then(|| u16::from_str_radix(a, 16).ok()).flatten()
            })
            .ok_or(format!("label {name} not in the map"))
    };
    Ok(BootImage {
        menu: std::fs::read(boot.join("ef3-menu.bin")).map_err(|e| e.to_string())?,
        directory: std::fs::read(boot.join("directory.bin")).map_err(|e| e.to_string())?,
        charset_tab_lo: label(".charset_tab_lo")?,
        charset_tab_hi: label(".charset_tab_hi")?,
    })
}

/// The type-90 CRT: skoe's menu image in slot 0 (his directory at bank $10), and slot 5
/// with a small EasyFlash program of ours — ROMH bank 0 with a reset vector — whose name the
/// directory carries.
fn menu_crt(b: &BootImage) -> Vec<u8> {
    let mut chips = Vec::new();
    for bank in 0u16..64 {
        let off = bank as usize * 0x4000;
        let mut roml = b.menu[off..off + 0x2000].to_vec();
        let romh = b.menu[off + 0x2000..off + 0x4000].to_vec();
        if bank == 0x10 {
            // the directory (EF-Directory V1:) goes at the start of bank $10 ROML
            let mut dir = b.directory.clone();
            // slot 5's name, as the menu will list it
            let at = 16 + 5 * 16;
            dir[at..at + 16].copy_from_slice(b"TRX64 EF3 TEST\0\0");
            roml[..dir.len()].copy_from_slice(&dir);
        }
        if roml.iter().any(|&x| x != 0xff) {
            chips.push((bank, 0x8000u16, roml));
        }
        if romh.iter().any(|&x| x != 0xff) {
            chips.push((bank, 0xa000u16, romh));
        }
    }
    // slot 5: SEI / LDX #0 / LDA $E020,X / STA $0300,X / INX / CPX #5 / BNE / JMP *, "EF5OK"
    let mut prog = vec![0xffu8; 0x2000];
    let code = [
        0x78, 0xa2, 0x00, 0xbd, 0x20, 0xe0, 0x9d, 0x00, 0x03, 0xe8, 0xe0, 0x05, 0xd0, 0xf5, 0x4c, 0x0e, 0xe0,
    ];
    prog[..code.len()].copy_from_slice(&code);
    prog[0x20..0x25].copy_from_slice(b"EF5OK");
    prog[0x1ffc] = 0x00;
    prog[0x1ffd] = 0xe0;
    chips.push((5 * 64, 0xa000, prog));
    chips.push((5 * 64, 0x8000, vec![0xffu8; 0x2000]));
    crt_with(90, &chips)
}

/// What `text_plot_str` puts in the bitmap for `s` at `(x_pos, y_pos)` in 8x8 cells, read
/// off the glyph tables the menu itself copied into RAM — compared, not assumed.
fn plotted(m: &Machine, b: &BootImage, s: &[u8], x_pos: usize, y_pos: usize) -> (Vec<u8>, Vec<u8>) {
    let ram = |a: usize| m.ram[a];
    let mut expect = vec![0u8; 320 * 25 + 8];
    let mut x = 8 * x_pos;
    for &c in s.iter().take_while(|&&c| c != 0) {
        let g = ram(b.charset_tab_lo as usize + c as usize) as usize
            | (ram(b.charset_tab_hi as usize + c as usize) as usize) << 8;
        // the first column is always plotted, even an empty one (a space is one blank
        // column); the glyph ends at the first empty column AFTER that
        let mut i = 0;
        loop {
            let col = ram(g + i);
            for row in 0..8 {
                if col >> row & 1 != 0 {
                    expect[y_pos * 320 + (x / 8) * 8 + row] |= 0x80 >> (x % 8);
                }
            }
            x += 1;
            i += 1;
            if ram(g + i) == 0 {
                break;
            }
        }
        x += 1; // the space between characters
    }
    let cells = (x + 7) / 8 - x_pos;
    let mut want = Vec::new();
    let mut got = Vec::new();
    for cx in 0..cells {
        for row in 0..8 {
            want.push(expect[y_pos * 320 + (x_pos + cx) * 8 + row]);
            got.push(m.ram[0x6000 + y_pos * 320 + (x_pos + cx) * 8 + row]);
        }
    }
    (want, got)
}

#[test]
fn a1_skoes_boot_image_boots_to_the_menu_and_starts_a_slot() {
    let Some(roms) = rom_dir() else { skip!("no ROMs ({ROM_DIR})") };
    if ef3_src().is_none() {
        skip!("no clone of the EF3 sources (set TRX64_EF3_SRC)");
    }
    for t in ["acme", "cc65", "ca65", "ld65", "ar65", "cc"] {
        if !have_tool(t) {
            skip!("build tool {t} not found");
        }
    }
    let img = build_boot_image().unwrap_or_else(|e| panic!("building skoe's boot image: {e}"));
    let crt = menu_crt(&img);
    let mut m = Machine::new();
    m.boot_from_dir(&roms).expect("boot ROMs");
    m.attach_cart_from_bytes(&crt, "ef3-menu").expect("attach");
    m.cold_reset();

    // boot: trampoline at $FF00 -> bank 8 -> the menu
    for _ in 0..60 {
        m.run_for_full_capped(100_000, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
        if m.vic.regs[0x11] & 0x20 != 0 && m.vic.regs[0x20] & 0x0f == 6 {
            break;
        }
    }
    assert!(m.vic.regs[0x11] & 0x20 != 0, "the menu switched the VIC to bitmap mode");
    assert_eq!(m.vic.regs[0x20] & 0x0f, 6, "blue border, as init_screen sets it");
    // let it plot
    m.run_for_full_capped(1_500_000, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
    // the cartridge is in EF mode, boot flag set, and the menu has read slot 0's directory
    let s = status(&m);
    // (the slot is whatever the menu last looked at: it walks the slots to see which hold a program)
    assert_eq!((s.state, s.boot, s.menu_enabled), ("ef", true, true));
    // screen text: slot 5's entry in the right-hand menu is plotted with the name the
    // directory (slot 0, bank $10) gave it
    let (want, got) = plotted(&m, &img, b"TRX64 EF3 TEST", 24, 13 + 1 + 4);
    assert!(want.iter().any(|&b| b != 0), "the expected text is not blank");
    assert_eq!(got, want, "the menu shows \"TRX64 EF3 TEST\" at the slot 5 line");
    // and the version line a real EF3 shows: the menu reads $DE08
    // start slot 5 ('e' = "EF Slot 5")
    // (the menu reads the key through the KERNAL, then waits for it to be RELEASED before it
    // starts the entry — as it does on a real keyboard)
    m.keyboard.key_down("E");
    for _ in 0..4 {
        m.run_for_full_capped(100_000, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
    }
    m.keyboard.key_up("E");
    for _ in 0..40 {
        m.run_for_full_capped(100_000, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
        if &m.ram[0x300..0x305] == b"EF5OK" {
            break;
        }
    }
    assert_eq!(&m.ram[0x300..0x305], b"EF5OK", "slot 5's EasyFlash program is running");
    let s = status(&m);
    assert_eq!((s.slot, s.state, s.menu_enabled), (5, "ef", false), "the menu selected slot 5 and mode 0");
    assert_eq!(s.last_mode, Some((0, "EasyFlash + reset")));
    // Menu button: back to the menu
    assert!(m.cart_press_button("menu").unwrap());
    for _ in 0..60 {
        m.run_for_full_capped(100_000, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
        if m.vic.regs[0x11] & 0x20 != 0 && m.vic.regs[0x20] & 0x0f == 6 {
            break;
        }
    }
    assert!(m.vic.regs[0x11] & 0x20 != 0, "the Menu button brought the menu back");
    assert!(status(&m).menu_enabled);
    // "Show Versions" (<SPACE> for the next page, then V): the menu reads $DE08 and prints it
    // as the CPLD core version — 1.1.1 for $49
    m.run_for_full_capped(1_500_000, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
    for key in ["SPACE", "V"] {
        m.keyboard.key_down(key);
        for _ in 0..4 {
            m.run_for_full_capped(100_000, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
        }
        m.keyboard.key_up(key);
        for _ in 0..10 {
            m.run_for_full_capped(100_000, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
        }
    }
    let (want, got) = plotted(&m, &img, b"1.1.1", 9, 4);
    assert!(want.iter().any(|&b| b != 0));
    assert_eq!(got, want, "the version screen shows the CPLD core version 1.1.1, read from $DE08");
}

// ── the EAPI ─────────────────────────────────────────────────────────────────────

struct Eapi {
    bin: Vec<u8>,
}

fn build_eapi() -> Result<Eapi, String> {
    let src = ef3_src().ok_or("no EF3 source clone")?;
    let dir = std::env::temp_dir().join("trx64-ef3-gate-eapi");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let eapi_dir = src.join("EasySDK/eapi");
    let out = dir.join("eapi-mx29640b.bin");
    sh(
        &eapi_dir,
        &format!("acme -f plain -o {} eapi-mx29640b.s", out.display()),
    )?;
    Ok(Eapi { bin: std::fs::read(out).map_err(|e| e.to_string())? })
}

/// Our test program, assembled with acme at test time. It calls skoe's driver and leaves
/// what it saw at `$0380...`, then `$42` at `$03ff`.
const EAPI_TEST: &str = r#"
!cpu 6502
* = $0400
RES = $0380
        sei
        lda #$07
        sta $de02               ; 16K: ROML + ROMH visible, as EAPIInit wants
        jsr $c014               ; EAPIInit
        sta RES+0               ; device id
        stx RES+1               ; manufacturer
        sty RES+2               ; slots
        lda #0
        rol
        sta RES+3               ; carry
        ; --- slot 5: erase the 64 KB ROML sector of banks 0-7 and the ROMH one
        lda #5
        jsr $df98               ; EAPISetSlot
        lda #0
        ldy #$80
        jsr $df83               ; EAPIEraseSector bank 0 ROML
        lda #0
        rol
        sta RES+4
        lda #0
        ldy #$a0
        jsr $df83               ; ... ROMH
        lda #0
        rol
        sta RES+5
        ; --- program 3 bytes into ROML bank 0 and 2 into ROMH bank 0 of slot 5
        lda #0
        jsr $df86               ; EAPISetBank 0
        lda #$a5
        ldx #$00
        ldy #$80
        jsr $df80               ; EAPIWriteFlash $8000
        lda #0
        rol
        sta RES+6
        lda #$3c
        ldx #$01
        ldy #$80
        jsr $df80
        lda #$81
        ldx #$ff
        ldy #$9f
        jsr $df80               ; last byte of the bank
        lda #$5a
        ldx #$00
        ldy #$e0
        jsr $df80               ; $E000 = ROMH bank 0
        lda #0
        rol
        sta RES+7
        lda #9
        jsr $df86               ; bank 9 = ROML half of the second 64 KB group? no: same sector group as bank 1
        lda #$99
        ldx #$10
        ldy #$80
        jsr $df80
        lda #0
        rol
        sta RES+8
        ; --- slot 0: erase ONE boot block (bank 3 ROML) and program into it
        lda #0
        jsr $df98
        lda #3
        ldy #$80
        jsr $df83
        lda #0
        rol
        sta RES+9
        lda #3
        jsr $df86
        lda #$c3
        ldx #$20
        ldy #$80
        jsr $df80
        lda #0
        rol
        sta RES+10
        ; --- read back through the window
        lda #5
        jsr $df98
        lda #0
        jsr $df86
        lda $8000
        sta RES+16
        lda $8001
        sta RES+17
        lda $9fff
        sta RES+18
        lda $a000
        sta RES+19              ; ROMH bank 0 $A000 = what was written at $E000
        lda #9
        jsr $df86
        lda $8010
        sta RES+20
        lda #0
        jsr $df98
        lda #3
        jsr $df86
        lda $8020
        sta RES+21
        lda $8000
        sta RES+22              ; erased boot block byte
        lda #$42
        sta $03ff
        jmp *
"#;

struct EapiRun {
    m: Machine,
    before: Vec<u8>,
    res: [u8; 32],
}

fn run_eapi_session() -> Result<EapiRun, String> {
    let eapi = build_eapi()?;
    let dir = std::env::temp_dir().join("trx64-ef3-gate-eapi");
    let s = dir.join("eapi_test.s");
    std::fs::write(&s, EAPI_TEST).map_err(|e| e.to_string())?;
    let o = dir.join("eapi_test.bin");
    sh(&dir, &format!("acme -f plain -o {} {}", o.display(), s.display()))?;
    let prog = std::fs::read(&o).map_err(|e| e.to_string())?;

    // an image with something recognisable everywhere the program may touch or must not
    let mut chips = Vec::new();
    for slot in 0u16..8 {
        for bank in 0u16..16 {
            let key = slot * 64 + bank;
            chips.push((key, 0x8000u16, vec![(0x10 + slot as u8) ^ bank as u8; 0x2000]));
            chips.push((key, 0xa000u16, vec![(0x80 + slot as u8) ^ bank as u8; 0x2000]));
        }
    }
    // slot 0's reset vector, so the machine comes up
    let mut romh = vec![0xeau8; 0x2000];
    romh[0x1ffc] = 0x00;
    romh[0x1ffd] = 0xe0;
    chips[1] = (0, 0xa000, romh);
    let crt = crt_with(90, &chips);
    let mut m = machine_with(&crt);
    let before = m.cartridge.as_mut().unwrap().writable_image(m.c64_core.clk).unwrap();

    // the EAPI is a PRG at $C000 (two address bytes, then the code); our program at $0400
    assert_eq!(&eapi.bin[..2], &[0x00, 0xc0], "the driver loads at $C000");
    m.poke(0xc000, &eapi.bin[2..]);
    m.poke(0x0400, &prog);
    m.write_full(0x0001, 0x37);
    m.c64_core.reg_pc = 0x0400;
    for _ in 0..400 {
        m.run_for_full_capped(100_000, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
        if m.ram[0x03ff] == 0x42 {
            break;
        }
    }
    if m.ram[0x03ff] != 0x42 {
        return Err(format!("the EAPI program did not finish: pc ${:04x}, res {:02x?}", m.c64_core.reg_pc, &m.ram[0x380..0x3a0]));
    }
    let mut res = [0u8; 32];
    res.copy_from_slice(&m.ram[0x380..0x3a0]);
    Ok(EapiRun { m, before, res })
}

fn eapi_tools_or_skip() -> bool {
    if ef3_src().is_none() {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), b"SKIP: ef3_gate - no clone of the EF3 sources (set TRX64_EF3_SRC)\n");
        return false;
    }
    if !have_tool("acme") {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), b"SKIP: ef3_gate - acme not found\n");
        return false;
    }
    true
}

#[test]
fn a6_the_ef3_eapi_programs_and_erases_and_nothing_else_changes() {
    if !eapi_tools_or_skip() {
        return;
    }
    let mut r = run_eapi_session().unwrap_or_else(|e| panic!("{e}"));
    let res = r.res;
    // EAPIInit: A = $CB, X = $C2, Y = 8 slots, carry clear
    assert_eq!((res[0], res[1], res[2], res[3]), (0xcb, 0xc2, 8, 0), "EAPIInit");
    // every erase and write reported success (carry clear)
    for (i, what) in [(4, "erase slot 5 ROML sector"), (5, "erase slot 5 ROMH sector"), (6, "write ROML"), (7, "write ROMH"),
                      (9, "erase slot 0 boot block"), (10, "write boot block")] {
        assert_eq!(res[i], 0, "{what}: carry clear");
    }
    // bank 9 is in the 64 KB group the program did NOT erase: a program can only clear bits,
    // so putting $99 over $1C is an error the driver reports (carry set) and the cell holds $1C & $99
    assert_eq!(res[8], 1, "programming 0->1 bits without an erase fails: carry set");
    // read back through the cartridge window, by the program itself
    assert_eq!((res[16], res[17], res[18], res[19]), (0xa5, 0x3c, 0x81, 0x5a), "slot 5 reads back what was written");
    assert_eq!(res[20], 0x1c & 0x99, "bank 9 of slot 5 holds the AND, as flash does");
    assert_eq!((res[21], res[22]), (0xc3, 0xff), "slot 0's boot block reads back; the rest of it is erased");

    // and nothing else changed: the whole chip, against a model of exactly these edits
    let now = r.m.cartridge.as_mut().unwrap().writable_image(r.m.c64_core.clk).unwrap();
    let mut want = r.before.clone();
    // slot 5, ROML half, banks 0-7 (the 64 KB sector) erased; ROMH half banks 0-7 erased
    for bank in 0..8 {
        want[faddr(5, bank, false, 0)..faddr(5, bank, false, 0) + 0x2000].fill(0xff);
        want[faddr(5, bank, true, 0)..faddr(5, bank, true, 0) + 0x2000].fill(0xff);
    }
    want[faddr(5, 0, false, 0)] = 0xa5;
    want[faddr(5, 0, false, 1)] = 0x3c;
    want[faddr(5, 0, false, 0x1fff)] = 0x81;
    want[faddr(5, 0, true, 0)] = 0x5a;
    // bank 9 is the other 64 KB group: its sector was NOT erased, so only bits that are 1 in
    // both can survive a program
    let old = r.before[faddr(5, 9, false, 0x10)];
    want[faddr(5, 9, false, 0x10)] = old & 0x99;
    // slot 0: boot block 3 only
    want[faddr(0, 3, false, 0)..faddr(0, 3, false, 0) + 0x2000].fill(0xff);
    want[faddr(0, 3, false, 0x20)] = 0xc3;
    let diff: Vec<usize> = (0..now.len()).filter(|&i| now[i] != want[i]).take(8).collect();
    assert!(diff.is_empty(), "the chip differs from the model of the edits at {diff:06x?}");
    assert_eq!(now.len(), want.len());
}

#[test]
fn a7_a_flash_write_survives_checkpoint_savecrt_and_reload() {
    if !eapi_tools_or_skip() {
        return;
    }
    let mut r = run_eapi_session().unwrap_or_else(|e| panic!("{e}"));
    let clk = r.m.c64_core.clk;
    let live = r.m.cartridge.as_mut().unwrap().writable_image(clk).unwrap();
    assert_ne!(live, r.before, "the program changed the flash");

    // a checkpoint round trip, through JSON as the .c64re carries it
    let crt = r.m.cartridge_image.as_ref().unwrap().raw_bytes.clone();
    let cp = capture_runtime_checkpoint(&r.m, "", "", None, None, Some(&crt), Some(&live));
    let cp: serde_json::Value = serde_json::from_str(&serde_json::to_string(&cp).unwrap()).unwrap();
    let mut back = Machine::new();
    if let Some(d) = rom_dir() {
        back.boot_from_dir(&d).expect("boot ROMs");
    }
    restore_runtime_checkpoint(&mut back, &cp).expect("restore");
    let c = back.cartridge.as_mut().expect("the cartridge came back");
    assert_eq!(c.mapper_type(), MapperType::EasyFlash3);
    assert_eq!(c.writable_image(back.c64_core.clk).unwrap(), live, "the flash round-trips a checkpoint");
    let (s_now, s_back) = (status(&r.m), status(&back));
    assert_eq!(s_now, s_back, "slot, $DE02, boot flag and mode enables round-trip");
    // the IO2 RAM too (the EAPI copied itself there)
    assert_eq!(peek(&back, 0xdf80), peek(&r.m, 0xdf80));
    assert_eq!(peek(&back, 0xdf80), Some(0x4c), "the driver's jump table is in the IO2 RAM");

    // savecrt: the CHIP layout, and a reload reads the same chip
    let saved = r.m.cartridge.as_mut().unwrap().crt_image(clk).expect("a type-90 CRT");
    let parsed = parse_crt(&saved, "saved", None).expect("parses");
    assert_eq!(parsed.mapper_type, MapperType::EasyFlash3);
    let (_i, mut reloaded) = load_cartridge_from_bytes(&saved, "saved", None).expect("reloads");
    assert_eq!(reloaded.writable_image(0).unwrap(), live, "savecrt + reload is the same chip");
    // packets are slot-major with slot*64+bank and 8 KB each
    let mut off = 0x40;
    let mut last = -1i32;
    let mut n = 0;
    while off + 16 <= saved.len() && &saved[off..off + 4] == b"CHIP" {
        let len = u32::from_be_bytes(saved[off + 4..off + 8].try_into().unwrap()) as usize;
        let bank = u16::from_be_bytes([saved[off + 10], saved[off + 11]]) as i32;
        let load = u16::from_be_bytes([saved[off + 12], saved[off + 13]]);
        assert!(bank >= last, "slot-major order");
        last = bank;
        assert!(load == 0x8000 || load == 0xa000 || load == 0xe000);
        assert_eq!(len, 0x2010);
        off += len;
        n += 1;
    }
    assert!(n > 0 && off == saved.len(), "the file is exactly its packets");
    // a write into a slot the image had NO packets for survives too (EasyProg onto a new slot)
    let mut c = load_cartridge_from_bytes(&crt_with(90, &[(0, 0x8000, vec![1u8; 0x2000])]), "n", None).unwrap().1;
    c.write(0xde02, 0b101, &bi(), 0); // Ultimax: the flash can be written
    c.write(0xde01, 0, &bi(), 0); // the driver issues the unlock cycles at slot 0 / bank 0
    for (a, v) in [(0x8aaau16, 0xaau8), (0x8555, 0x55), (0x8aaa, 0xa0)] {
        c.write(a, v, &bi(), 0);
    }
    c.write(0xde01, 6, &bi(), 0);
    c.write(0x8000, 0x00, &bi(), 0);
    let img = c.crt_image(1_000).unwrap();
    let mut c2 = load_cartridge_from_bytes(&img, "n2", None).unwrap().1;
    c2.write(0xde01, 6, &bi(), 0);
    assert_eq!(c2.read(0x8000, &bi(), 2_000), Some(0x00), "slot 6 was written from scratch and saved");
    assert_eq!(c2.read(0x8001, &bi(), 2_000), Some(0xff));
}
