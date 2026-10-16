//! Action Replay V5 / V6 (CRT hardware type 1) — the synthetic cases: mounting, the
//! register and the memory map behind it, a peek that does nothing, the freeze on the
//! cycle of the press, and a checkpoint taken while frozen. Hermetic: the images are built
//! here; the cases that run a machine need the C64 ROMs and say `SKIP` when they are absent.
//! The real images are `action_replay_corpus.rs`.
//!
//!   cargo test --release -p trx64-core --test action_replay_gate -- --nocapture

use std::path::PathBuf;

use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, restore_runtime_checkpoint};
use trx64_core::cart::{
    load_cartridge_from_bin, load_cartridge_from_bytes, parse_crt, resolve_cart_type, BankInfo, CartMapper,
    CartType, CrtError, MapperType,
};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");

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

/// What bank `n` holds at offset `i`: different in every bank and at every offset, so a
/// read proves which bank and which byte answered.
fn tag(n: usize, i: usize) -> u8 {
    ((n * 0x41) as u8) ^ (i as u8) ^ ((i >> 8) as u8).wrapping_mul(7) ^ 0x5a
}

fn plain_banks() -> [Vec<u8>; 4] {
    std::array::from_fn(|n| (0..0x2000).map(|i| tag(n, i)).collect())
}

fn crt_of(chips: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   ");
    v.extend_from_slice(&0x40u32.to_be_bytes());
    v.extend_from_slice(&0x0100u16.to_be_bytes());
    v.extend_from_slice(&1u16.to_be_bytes()); // Action Replay
    v.push(0);
    v.push(0);
    v.extend_from_slice(&[0u8; 6]);
    let mut name = [0u8; 32];
    name[..9].copy_from_slice(b"AR GATE 5");
    v.extend_from_slice(&name);
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

fn ar_crt(banks: &[Vec<u8>; 4]) -> Vec<u8> {
    let chips: Vec<_> = banks.iter().enumerate().map(|(n, b)| (n as u16, 0x8000u16, b.clone())).collect();
    crt_of(&chips)
}

fn ar_bin(banks: &[Vec<u8>; 4]) -> Vec<u8> {
    banks.iter().flatten().copied().collect()
}

fn bi(phi1: u8, ram_under: u8) -> BankInfo {
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
        phi1,
        ram_under,
    }
}

fn mapper() -> Box<dyn CartMapper> {
    load_cartridge_from_bytes(&ar_crt(&plain_banks()), "AR", None).expect("type 1 builds").1
}

fn lines(m: &dyn CartMapper) -> (u8, u8) {
    let l = m.get_lines();
    (l.exrom, l.game)
}

// the lines, as (EXROM, GAME) with 0 = asserted
const K8: (u8, u8) = (0, 1);
const K16: (u8, u8) = (0, 0);
const OFF: (u8, u8) = (1, 1);
const ULTIMAX: (u8, u8) = (1, 0);

fn wr(m: &mut Box<dyn CartMapper>, addr: u16, v: u8) -> bool {
    m.write(addr, v, &bi(0xff, 0), 0)
}

fn rd(m: &mut Box<dyn CartMapper>, addr: u16) -> Option<u8> {
    m.read(addr, &bi(0xff, 0), 0)
}

fn pk(m: &dyn CartMapper, addr: u16) -> Option<u8> {
    m.peek(addr, &bi(0xff, 0))
}

fn freezer(m: &dyn CartMapper) -> trx64_core::cart::FreezerCartState {
    m.get_state().freezer.expect("a freezer")
}

// ── S1: a synthetic image mounts as an Action Replay ─────────────────────────────

#[test]
fn s1_crt_type_1_mounts_with_four_banks() {
    let crt = ar_crt(&plain_banks());
    let image = parse_crt(&crt, "ar.crt", None).expect("parses");
    assert_eq!(image.mapper_type, MapperType::ActionReplay);
    assert_eq!(image.banks.len(), 4);
    let (img, m) = load_cartridge_from_bytes(&crt, "ar.crt", None).expect("builds");
    assert_eq!(img.mapper_type, MapperType::ActionReplay);
    assert_eq!(m.mapper_type(), MapperType::ActionReplay);
    assert_eq!(m.buttons(), &["freeze"]);
}

#[test]
fn s1_bin_is_32_kib_in_four_banks() {
    let banks = plain_banks();
    let (img, mut m) = load_cartridge_from_bin(&ar_bin(&banks), "ar.bin", MapperType::ActionReplay).expect("32 KiB");
    assert_eq!((img.mapper_type, img.banks.len()), (MapperType::ActionReplay, 4));
    assert_eq!(rd(&mut m, 0x8000), Some(tag(0, 0)));
    // the same bytes as the CRT of the same banks, bank by bank
    wr(&mut m, 0xde00, 0x18);
    assert_eq!(rd(&mut m, 0x8123), Some(tag(3, 0x123)));
    // Anything but 32 KiB is another cartridge
    for len in [0x2000usize, 0x4000, 0x6000, 0xa000] {
        let e = load_cartridge_from_bin(&vec![0u8; len], "x.bin", MapperType::ActionReplay).err();
        assert!(matches!(e, Some(CrtError::BadBinSize { .. })), "{len:#x}: {e:?}");
    }
}

#[test]
fn s1_the_type_has_names() {
    for s in ["1", "ar", "AR", "action-replay", "actionreplay"] {
        assert_eq!(resolve_cart_type(s).unwrap(), CartType::Forced(MapperType::ActionReplay), "{s}");
    }
}

#[test]
fn s1_a_crt_that_is_not_four_8k_banks_is_refused() {
    let b = plain_banks();
    let three = crt_of(&[(0, 0x8000, b[0].clone()), (1, 0x8000, b[1].clone()), (2, 0x8000, b[2].clone())]);
    assert!(matches!(load_cartridge_from_bytes(&three, "x", None).err(), Some(CrtError::BadLayout(_))));
    let mut chips: Vec<_> = (0..4).map(|n| (n as u16, 0x8000u16, b[n].clone())).collect();
    chips.push((4, 0x8000, vec![0; 0x2000]));
    assert!(matches!(
        load_cartridge_from_bytes(&crt_of(&chips), "x", None).err(),
        Some(CrtError::BankOutOfRange { bank: 4, .. })
    ));
    // a 16 KiB chip at $8000 is not an Action Replay bank
    let wide = crt_of(&[
        (0, 0x8000, vec![0; 0x4000]),
        (1, 0x8000, b[1].clone()),
        (2, 0x8000, b[2].clone()),
        (3, 0x8000, b[3].clone()),
    ]);
    assert!(matches!(load_cartridge_from_bytes(&wide, "x", None).err(), Some(CrtError::BadLayout(_))));
}

// ── S3: the register, the banks, the memory map ──────────────────────────────────

#[test]
fn s3_power_on_is_bank_0_8k_with_the_register_clear() {
    let mut m = mapper();
    assert_eq!(lines(m.as_ref()), K8);
    assert_eq!(rd(&mut m, 0x8000), Some(tag(0, 0)));
    assert_eq!(rd(&mut m, 0x9fff), Some(tag(0, 0x1fff)));
    let f = freezer(m.as_ref());
    assert!(f.active && !f.export_ram && !f.nmi);
    assert_eq!((f.regvalue, f.mode, f.bank), (0, 0, 0));
    assert_eq!((f.freeze_at, f.nmi_at), (None, None));
}

#[test]
fn s3_bits_0_to_4_are_the_lines_and_the_bank() {
    let mut m = mapper();
    // value & 3: 0 = 8K, 1 = 16K, 2 = off, 3 = ultimax; bits 3-4 the 8 KiB bank
    for (value, want, bank) in [
        (0x00u8, K8, 0usize),
        (0x01, K16, 0),
        (0x02, OFF, 0),
        (0x03, ULTIMAX, 0),
        (0x08, K8, 1),
        (0x11, K16, 2),
        (0x18, K8, 3),
        (0x1b, ULTIMAX, 3),
    ] {
        assert!(wr(&mut m, 0xde00, value), "{value:#x}: the active cartridge takes the store");
        assert_eq!(lines(m.as_ref()), want, "{value:#x}");
        assert_eq!(rd(&mut m, 0x8000 + 0x0345), Some(tag(bank, 0x345)), "{value:#x}: ROML bank");
        assert_eq!(rd(&mut m, 0xa000 + 0x0345), Some(tag(bank, 0x345)), "{value:#x}: the same bank at ROMH");
        assert_eq!(rd(&mut m, 0xe000 + 0x1ffa), Some(tag(bank, 0x1ffa)), "{value:#x}: and at $E000");
        assert_eq!(m.active_bank(0x8000), bank as u16);
    }
    // the register mirrors through $DE00-$DEFF
    wr(&mut m, 0xde00, 0);
    wr(&mut m, 0xde7f, 0x10);
    assert_eq!(freezer(m.as_ref()).bank, 2);
    assert_eq!(pk(m.as_ref(), 0xde00), Some(0x10), "a peek shows the register, at every mirror");
    assert_eq!(pk(m.as_ref(), 0xdeff), Some(0x10));
}

#[test]
fn s3_kill_turns_the_register_off_but_leaves_the_map() {
    let mut m = mapper();
    // 0x04 | mode 3 | bank 1: killed, with the configuration it was killed with
    assert!(wr(&mut m, 0xde00, 0x04 | 0x03 | 0x08));
    assert!(!freezer(m.as_ref()).active);
    assert_eq!(lines(m.as_ref()), ULTIMAX, "the configuration of the killing store stands");
    assert_eq!(rd(&mut m, 0x8000), Some(tag(1, 0)));
    // $DE00 and $DF00 no longer answer, and a store no longer changes anything
    assert!(!wr(&mut m, 0xde00, 0x00));
    assert_eq!(lines(m.as_ref()), ULTIMAX);
    assert_eq!(freezer(m.as_ref()).regvalue, 0x0f);
    assert_eq!(rd(&mut m, 0xde00), None, "a read of a killed IO1 is the open bus");
    assert_eq!(rd(&mut m, 0xdf00), None);
    assert_eq!(pk(m.as_ref(), 0xdf00), Some(0), "VICE's peek of a killed IO2 is 0");
    // a reset brings it back
    m.reset();
    assert!(freezer(m.as_ref()).active);
    assert_eq!(lines(m.as_ref()), K8);
}

#[test]
fn s3_ram_at_roml_and_io2() {
    let mut m = mapper();
    // 0x20: RAM at ROML and IO2, 8K game
    wr(&mut m, 0xde00, 0x20);
    assert_eq!(lines(m.as_ref()), K8);
    assert!(freezer(m.as_ref()).export_ram);
    // 8K game: a store to ROML lands in cartridge RAM AND in C64 RAM — the C64 part is the
    // bus's, the cartridge's part is `write_under_roml`; the cartridge does not consume it
    assert!(!wr(&mut m, 0x8100, 0xa5));
    m.write_under_roml(0x8100, 0xa5);
    assert_eq!(rd(&mut m, 0x8100), Some(0xa5), "ROML now reads the RAM");
    assert_eq!(rd(&mut m, 0x9f10), Some(freezer(m.as_ref()).ram[0x1f10]));
    // IO2 is the top page of the same RAM
    assert!(wr(&mut m, 0xdf10, 0x77));
    assert_eq!(freezer(m.as_ref()).ram[0x1f10], 0x77);
    assert_eq!(rd(&mut m, 0xdf10), Some(0x77));
    assert_eq!(rd(&mut m, 0x9f10), Some(0x77), "$9F00 and $DF00 are the same page");
    assert_eq!(rd(&mut m, 0xdf90), Some(rd(&mut m, 0x9f90).unwrap()));
    // RAM off: IO2 and the page at $9F00 are the ROM bank again
    wr(&mut m, 0xde00, 0x08);
    assert_eq!(rd(&mut m, 0xdf10), Some(tag(1, 0x1f10)));
    assert_eq!(rd(&mut m, 0x9f10), Some(tag(1, 0x1f10)));
    assert!(!wr(&mut m, 0xdf10, 0x33), "IO2 is not writable with RAM off");
    assert_eq!(freezer(m.as_ref()).ram[0x1f10], 0x77, "and the RAM did not take it");
    // ultimax with RAM: the cartridge RAM is the only RAM at ROML, so it takes the store
    wr(&mut m, 0xde00, 0x23);
    assert_eq!(lines(m.as_ref()), ULTIMAX);
    assert!(wr(&mut m, 0x8002, 0x42));
    assert_eq!(freezer(m.as_ref()).ram[2], 0x42);
    assert_eq!(rd(&mut m, 0x8002), Some(0x42));
    // ultimax, RAM off: the store is consumed and goes nowhere
    wr(&mut m, 0xde00, 0x03);
    assert!(wr(&mut m, 0x8002, 0x99));
    assert_eq!(freezer(m.as_ref()).ram[2], 0x42);
}

#[test]
fn s3_mode_0x22_reads_c64_ram_or_cartridge_ram() {
    let mut m = mapper();
    wr(&mut m, 0xde00, 0x20);
    for (i, b) in [(0x10usize, 0x0fu8), (0x11, 0xf0), (0x12, 0x55)] {
        m.write_under_roml(0x8000 + i as u16, b);
    }
    // 0x22: RAM on, EXROM high, GAME high — the lines are 8K, the read is both buses
    wr(&mut m, 0xde00, 0x22);
    assert_eq!(lines(m.as_ref()), K8, "mode $22 is configured as an 8K game");
    let r = |m: &mut Box<dyn CartMapper>, a: u16, under: u8| m.read(a, &bi(0xff, under), 0).unwrap();
    assert_eq!(r(&mut m, 0x8010, 0xf0), 0xff, "C64 RAM $f0 OR cartridge RAM $0f");
    assert_eq!(r(&mut m, 0x8011, 0x0f), 0xff);
    assert_eq!(r(&mut m, 0x8012, 0x00), 0x55);
    assert_eq!(r(&mut m, 0x8012, 0xaa), 0xff);
    assert_eq!(m.peek(0x8012, &bi(0xff, 0x0a)), Some(0x5f), "the peek shows the same OR");
    // 0x23 is not the broken mode: ultimax with RAM
    wr(&mut m, 0xde00, 0x23);
    assert_eq!(r(&mut m, 0x8012, 0xaa), 0x55);
}

#[test]
fn s3_a_read_of_io1_rewrites_the_register_with_the_bus_byte() {
    let mut m = mapper();
    assert_eq!(freezer(m.as_ref()).regvalue, 0);
    // The R/W line is not decoded: the read stores whatever phi1 held. 0x58 = bank 3 bit
    // 4... 0101 1000: bit 6 release, bits 4-3 = 3 bank, bits 1-0 = 0 (8K)
    let got = m.read(0xde40, &bi(0x58, 0), 0);
    assert_eq!(got, Some(0x58), "the byte read is the byte stored");
    let f = freezer(m.as_ref());
    assert_eq!((f.regvalue, f.bank), (0x58, 3));
    assert!(m.take_config_changed(), "the bus is told to re-run the PLA");
    assert!(!m.take_config_changed(), "once");
    // a read that turns the cartridge into an ultimax one moves the lines
    m.read(0xde00, &bi(0x03, 0), 0);
    assert_eq!(lines(m.as_ref()), ULTIMAX);
    // a read of a bus byte with the kill bit kills it, like a store
    m.read(0xde00, &bi(0x04, 0), 0);
    assert!(!freezer(m.as_ref()).active);
    assert_eq!(m.read(0xde00, &bi(0x00, 0), 0), None);
    assert_eq!(freezer(m.as_ref()).regvalue, 0x04, "and a killed cartridge is not rewritten again");
}

// ── S6: a peek does nothing ──────────────────────────────────────────────────────

#[test]
fn s6_a_peek_changes_nothing_at_the_mapper() {
    let mut m = mapper();
    wr(&mut m, 0xde00, 0x2b);
    let before = freezer(m.as_ref());
    let l = lines(m.as_ref());
    for addr in (0xde00u16..=0xdfff).chain(0x8000..=0x8010).chain(0xe000..=0xe010) {
        m.peek(addr, &bi(0x03, 0));
    }
    assert_eq!(freezer(m.as_ref()), before);
    assert_eq!(lines(m.as_ref()), l);
    assert!(!m.take_config_changed());
}

fn bare(m: &mut Machine) {
    m.attach_cart_from_bytes(&ar_crt(&plain_banks()), "ar").expect("attach");
    m.cold_reset();
}

#[test]
fn s6_the_monitor_and_state_reads_fire_no_side_effect_but_a_live_read_does() {
    let mut m = Machine::new();
    bare(&mut m);
    m.write_full(0xde00, 0x28); // RAM on, 8K, bank 1
    let cart = |m: &Machine| m.cartridge.as_ref().unwrap().get_state();
    let before = cart(&m).freezer.unwrap();
    let memconfig_ultimax = m.memconfig.ultimax;
    // the VIC's last phi1 byte is a mode-changing one: 0x03 would make it an ultimax cart
    m.vic.last_read_phi1 = 0x03;
    for addr in 0xde00u16..=0xdfff {
        m.read_full(addr);
        m.peek_lens(addr, "cart");
        m.peek_lens(addr, "cpu");
        m.peek_lens(addr, "io");
    }
    for addr in [0x8000u16, 0x9fff, 0xa000, 0xe000, 0xfffa] {
        m.read_full(addr);
    }
    assert_eq!(cart(&m).freezer.unwrap(), before, "no peek moved the register, the lines or the RAM");
    assert_eq!(m.memconfig.ultimax, memconfig_ultimax, "and the memory map is as it was");
    assert_eq!(m.read_full(0xde00), 0x28, "the monitor shows the register");

    // The contrast that makes the test mean something: the CPU's read of $DE00 stores the
    // phi1 byte, and the memory map follows it at once.
    let v = m.read_full_live(0xde00);
    assert_eq!(v, 0x03, "the read returns the bus byte");
    assert_eq!(cart(&m).freezer.unwrap().regvalue, 0x03);
    assert!(m.memconfig.ultimax, "and the PLA was re-run after the read, not only after a write");
}

#[test]
fn s6_the_cartridge_sees_the_real_phi1_byte() {
    let mut m = Machine::new();
    bare(&mut m);
    m.vic.last_read_phi1 = 0x10;
    m.read_full_live(0xde00);
    assert_eq!(m.cartridge.as_ref().unwrap().get_state().freezer.unwrap().regvalue, 0x10, "not $ff");
}

// ── the cartridge RAM is not flash ───────────────────────────────────────────────

#[test]
fn the_ram_is_not_a_writable_image_and_not_dirty() {
    let mut m = mapper();
    wr(&mut m, 0xde00, 0x23);
    wr(&mut m, 0x8000, 0xaa);
    assert!(!m.is_writable_dirty(), "writing the RAM does not dirty a cartridge");
    assert!(!m.persists_writable_state());
    assert!(m.writable_image(0).is_none(), "nothing to write back into the .crt");
    assert!(m.crt_image(0).is_none());
}

#[test]
fn power_up_ram_is_vices_pattern_and_a_reset_leaves_it() {
    let mut m = mapper();
    let ram = freezer(m.as_ref()).ram;
    // start 255, inverted every 2 bytes from offset 1, the pattern inverted every $100
    assert_eq!(&ram[..6], &[0xff, 0x00, 0x00, 0xff, 0xff, 0x00]);
    assert_eq!(ram[0x100], 0x00);
    assert_eq!(ram[0x101], 0xff);
    wr(&mut m, 0xde00, 0x23);
    wr(&mut m, 0x8000, 0x12);
    m.reset();
    assert_eq!(freezer(m.as_ref()).ram[0], 0x12, "a reset is not a power cycle");
    m.power_up();
    assert_eq!(freezer(m.as_ref()).ram, ram, "a power cycle is");
}

// ── the freeze, on a machine ─────────────────────────────────────────────────────

/// An Action Replay whose bank 0 holds a program and the freezer's NMI handler.
///
/// $8000 CBM80 autostart → init → a loop counting in $0334. The handler (reached through the
/// NMI vector at $FFFA, which in the freeze's ultimax config is this same bank 0 at $E000)
/// counts in $0335, builds `LDA #$40 / STA $DE00 / RTI` in RAM, and runs it: the store
/// releases /NMI and puts the cartridge back to an 8K game, the RTI resumes the program.
fn freezer_rom() -> [Vec<u8>; 4] {
    let mut banks = plain_banks();
    let b = &mut banks[0];
    b.iter_mut().for_each(|x| *x = 0xea);
    let put = |b: &mut Vec<u8>, at: usize, code: &[u8]| b[at..at + code.len()].copy_from_slice(code);
    put(b, 0, &[0x09, 0x80, 0x09, 0x80, 0xc3, 0xc2, 0xcd, 0x38, 0x30]);
    put(
        b,
        9,
        &[
            0x8e, 0x16, 0xd0, // STX $D016
            0x20, 0xa3, 0xfd, // JSR $FDA3
            0x20, 0x50, 0xfd, // JSR $FD50
            0x20, 0x15, 0xfd, // JSR $FD15
            0x20, 0x5b, 0xff, // JSR $FF5B
            0xee, 0x34, 0x03, // loop: INC $0334   ($8018)
            0x4c, 0x18, 0x80, // JMP loop
        ],
    );
    put(
        b,
        0x1000,
        &[
            0xa9, 0xa9, 0x8d, 0x40, 0x03, // LDA #$A9   STA $0340   (LDA #imm)
            0xa9, 0x40, 0x8d, 0x41, 0x03, // LDA #$40   STA $0341   (the value)
            0xa9, 0x8d, 0x8d, 0x42, 0x03, // LDA #$8D   STA $0342   (STA abs)
            0xa9, 0x00, 0x8d, 0x43, 0x03, // LDA #$00   STA $0343
            0xa9, 0xde, 0x8d, 0x44, 0x03, // LDA #$DE   STA $0344
            0xa9, 0x40, 0x8d, 0x45, 0x03, // LDA #$40   STA $0345   (RTI)
            0xee, 0x35, 0x03, // INC $0335
            0x4c, 0x40, 0x03, // JMP $0340
        ],
    );
    put(b, 0x1ffa, &[0x00, 0xf0, 0x09, 0x80, 0x00, 0xf0]); // NMI, RESET, IRQ
    banks
}

fn boot(crt: &[u8]) -> Option<Machine> {
    let d = rom_dir()?;
    let mut m = Machine::new();
    m.boot_from_dir(&d).expect("boot ROMs");
    m.attach_cart_from_bytes(crt, "ar").expect("attach");
    m.cold_reset();
    Some(m)
}

fn run(m: &mut Machine, cycles: u64) {
    m.run_for_full_capped(cycles, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
}

fn frozen_state(m: &Machine) -> trx64_core::cart::FreezerCartState {
    m.cartridge.as_ref().unwrap().get_state().freezer.unwrap()
}

#[test]
fn s4_synthetic_freeze_enters_the_handler_and_returns() {
    let Some(mut m) = boot(&ar_crt(&freezer_rom())) else { skip!("no C64 ROMs") };
    run(&mut m, 2_000_000);
    let counter = m.read_full(0x0334);
    assert_ne!(counter, 0, "the program runs");
    assert_eq!((m.read_full(0x0335), frozen_state(&m).mode), (0, 0));

    let at = m.c64_core.clk;
    assert_eq!(m.cart_press_button("freeze"), Ok(false), "a freeze is not a reset");
    // The press is held until its cycle: nothing is up yet.
    assert_eq!(frozen_state(&m).freeze_at, Some(at));
    assert!(!frozen_state(&m).nmi);

    // One instruction on: the press has been taken at the cycle it was made — /NMI is up
    // and the cartridge's freeze is exactly three cycles after it.
    m.run_for_full_capped(2, 1, &mut NullSink, |_, _, _, _, _, _, _| {});
    let f = frozen_state(&m);
    assert!(f.nmi, "NMI asserted");
    assert_eq!((f.freeze_at, f.nmi_at), (None, Some(at)), "NMI up on the cycle of the press; the freeze waits for the CPU to take it");
    assert_eq!(f.mode, 0, "and the map has not moved under the instruction in flight");

    // run on: the handler ran once, the program carries on from where it was frozen
    run(&mut m, 2_000);
    assert_eq!(m.read_full(0x0335), 1, "the NMI handler ran exactly once");
    let f = frozen_state(&m);
    assert!(!f.nmi, "the handler released /NMI through the register");
    assert_eq!((f.mode, f.export_ram, f.bank), (0, false, 0), "and put the cartridge back to an 8K game");
    assert!(!m.memconfig.ultimax);
    let after = m.read_full(0x0334);
    run(&mut m, 50_000);
    assert_ne!(m.read_full(0x0334), after, "the counter runs on");
    assert_eq!(m.read_full(0x0335), 1, "and nothing froze again");
}

#[test]
fn s4_the_freeze_is_ultimax_with_ram_while_the_handler_runs() {
    let Some(mut m) = boot(&ar_crt(&freezer_rom())) else { skip!("no C64 ROMs") };
    run(&mut m, 2_000_000);
    m.cart_press_button("freeze").unwrap();
    // stop inside the handler ($F000.. in the freeze's ultimax map)
    let mut seen = false;
    for _ in 0..200 {
        m.run_for_full_capped(1, 1, &mut NullSink, |_, _, _, _, _, _, _| {});
        if (0xf000..0xf030).contains(&m.c64_core.reg_pc) {
            seen = true;
            break;
        }
    }
    assert!(seen, "the CPU took the NMI through the cartridge's vector");
    let f = frozen_state(&m);
    assert_eq!((f.mode, f.export_ram, f.bank, f.active), (3, true, 0, true), "ultimax, RAM at ROML, bank 0");
    assert!(m.memconfig.ultimax);
    assert_eq!(m.read_full(0xfffa), 0x00);
    assert_eq!(m.read_full(0xfffb), 0xf0);
}

#[test]
fn s4_a_press_at_a_later_cycle_is_held_across_the_run() {
    let Some(mut m) = boot(&ar_crt(&freezer_rom())) else { skip!("no C64 ROMs") };
    run(&mut m, 2_000_000);
    let now = m.c64_core.clk;
    assert!(m.cart_press_button_at("freeze", Some(now - 1)).is_err(), "not in the past");
    assert!(m.cart_press_button_at("freeze", Some(now + 10_000)).is_ok());
    run(&mut m, 9_000);
    assert_eq!(m.read_full(0x0335), 0, "not yet");
    assert_eq!(frozen_state(&m).freeze_at, Some(now + 10_000));
    run(&mut m, 3_000);
    assert_eq!(m.read_full(0x0335), 1, "taken in the middle of a run, at its cycle");
}

#[test]
fn s4_a_press_is_the_same_run_every_time() {
    let Some(mut a) = boot(&ar_crt(&freezer_rom())) else { skip!("no C64 ROMs") };
    let mut b = boot(&ar_crt(&freezer_rom())).unwrap();
    let sig = |m: &mut Machine| {
        run(m, 400_000);
        let at = m.c64_core.clk + 5_000;
        m.cart_press_button_at("freeze", Some(at)).unwrap();
        run(m, 20_000);
        (m.c64_core.clk, m.c64_core.reg_pc, m.read_full(0x0334), m.read_full(0x0335), frozen_state(m))
    };
    assert_eq!(sig(&mut a), sig(&mut b));
}

#[test]
fn s4_a_cartridge_without_a_freeze_button_still_refuses() {
    let mut m = Machine::new();
    assert!(m.cart_press_button("freeze").is_err(), "no cartridge");
    bare(&mut m);
    assert!(m.cart_press_button("menu").unwrap_err().contains("freeze"), "names what it has");
}

#[test]
fn s4_freeze_revives_a_killed_cartridge() {
    let mut m = Machine::new();
    bare(&mut m);
    m.write_full(0xde00, 0x04);
    assert!(!frozen_state(&m).active);
    let c = m.cartridge.as_mut().unwrap();
    c.press_button_at("freeze", 100).unwrap();
    assert!(!c.run_alarms(100), "the press arms the NMI first");
    assert!(c.nmi_line());
    assert!(!c.run_alarms(110), "and nothing else happens with the clock");
    assert!(c.nmi_vector_fetch(), "the cartridge freezes when the CPU is at the vector fetch");
    let f = frozen_state(&m);
    assert_eq!((f.active, f.mode, f.export_ram, f.bank), (true, 3, true, 0));
}

#[test]
fn s4_register_write_with_bit_6_releases_nmi() {
    let mut m = mapper();
    m.press_button_at("freeze", 10).unwrap();
    m.run_alarms(10);
    m.nmi_vector_fetch();
    assert!(m.nmi_line());
    wr(&mut m, 0xde00, 0x23); // no release bit: still held
    assert!(m.nmi_line());
    wr(&mut m, 0xde00, 0x43);
    assert!(!m.nmi_line(), "bit 6 lets go");
}

// ── S5: a checkpoint taken while frozen ──────────────────────────────────────────

#[test]
fn s5_checkpoint_while_frozen_round_trips_through_json() {
    let Some(mut m) = boot(&ar_crt(&freezer_rom())) else { skip!("no C64 ROMs") };
    run(&mut m, 2_000_000);
    m.cart_press_button("freeze").unwrap();
    for _ in 0..200 {
        m.run_for_full_capped(1, 1, &mut NullSink, |_, _, _, _, _, _, _| {});
        if (0xf000..0xf030).contains(&m.c64_core.reg_pc) {
            break;
        }
    }
    let frozen_pc = m.c64_core.reg_pc;
    // write the cartridge RAM from the frozen state, as the menu would
    m.write_full(0x8000, 0xc3);
    m.write_full(0x9f01, 0x3c);
    let frozen = frozen_state(&m);
    assert_eq!((frozen.mode, frozen.ram[0], frozen.ram[0x1f01]), (3, 0xc3, 0x3c));

    let crt = m.cartridge_image.as_ref().unwrap().raw_bytes.clone();
    let cp = capture_runtime_checkpoint(&m, "", "", None, None, Some(&crt), None);
    let cp: serde_json::Value = serde_json::from_str(&serde_json::to_string(&cp).unwrap()).unwrap();
    let ram_node = &cp["cartState"]["freezerState"]["ram"];
    assert!(ram_node.get("b64").is_some(), "the RAM is a byte node, not a list of numbers: {ram_node}");

    // run on, return to the program, then come back
    run(&mut m, 5_000);
    assert_eq!(frozen_state(&m).mode, 0);

    let mut back = Machine::new();
    back.boot_from_dir(&rom_dir().unwrap()).unwrap();
    restore_runtime_checkpoint(&mut back, &cp).expect("restore");
    assert_eq!(frozen_state(&back), frozen, "registers, lines, NMI and the RAM are the checkpoint's");
    assert!(back.memconfig.ultimax, "and so is the memory map");
    assert_eq!(back.c64_core.reg_pc, frozen_pc);
    // it continues exactly as the original did from the same point
    run(&mut back, 5_000);
    assert_eq!(frozen_state(&back).mode, 0);
    assert_eq!(back.read_full(0x0335), 1);
}

#[test]
fn s5_a_pending_freeze_is_carried_by_the_checkpoint() {
    let Some(mut m) = boot(&ar_crt(&freezer_rom())) else { skip!("no C64 ROMs") };
    run(&mut m, 2_000_000);
    let at = m.c64_core.clk + 50_000;
    m.cart_press_button_at("freeze", Some(at)).unwrap();
    let crt = m.cartridge_image.as_ref().unwrap().raw_bytes.clone();
    let cp = capture_runtime_checkpoint(&m, "", "", None, None, Some(&crt), None);
    let cp: serde_json::Value = serde_json::from_str(&serde_json::to_string(&cp).unwrap()).unwrap();
    let mut back = Machine::new();
    back.boot_from_dir(&rom_dir().unwrap()).unwrap();
    restore_runtime_checkpoint(&mut back, &cp).expect("restore");
    assert_eq!(frozen_state(&back).freeze_at, Some(at));
    run(&mut back, 60_000);
    assert_eq!(back.read_full(0x0335), 1, "the press still happens, at its cycle");
}

#[test]
fn s5_a_checkpoint_that_predates_the_freezer_part_still_loads() {
    let Some(m) = boot(&ar_crt(&freezer_rom())) else { skip!("no C64 ROMs") };
    let crt = m.cartridge_image.as_ref().unwrap().raw_bytes.clone();
    let mut cp = capture_runtime_checkpoint(&m, "", "", None, None, Some(&crt), None);
    cp["cartState"].as_object_mut().unwrap().remove("freezerState");
    let mut back = Machine::new();
    back.boot_from_dir(&rom_dir().unwrap()).unwrap();
    restore_runtime_checkpoint(&mut back, &cp).expect("restore");
    assert_eq!(back.cartridge.as_ref().unwrap().mapper_type(), MapperType::ActionReplay);
}

#[test]
fn s5_a_bin_mount_comes_back_as_an_action_replay() {
    let Some(mut m) = (|| {
        let d = rom_dir()?;
        let mut m = Machine::new();
        m.boot_from_dir(&d).ok()?;
        m.attach_cart_typed(&ar_bin(&freezer_rom()), "ar.bin", CartType::Forced(MapperType::ActionReplay)).ok()?;
        m.cold_reset();
        Some(m)
    })() else {
        skip!("no C64 ROMs")
    };
    run(&mut m, 100_000);
    let bytes = m.cartridge_image.as_ref().unwrap().raw_bytes.clone();
    let cp = capture_runtime_checkpoint(&m, "", "", None, None, Some(&bytes), None);
    let mut back = Machine::new();
    back.boot_from_dir(&rom_dir().unwrap()).unwrap();
    restore_runtime_checkpoint(&mut back, &cp).expect("restore");
    assert_eq!(back.cartridge.as_ref().unwrap().mapper_type(), MapperType::ActionReplay);
}

// ── the read-set lane ────────────────────────────────────────────────────────────

#[test]
fn the_read_set_does_not_count_cartridge_ram_as_rom() {
    let mut m = mapper();
    assert!(m.read_is_rom(0x8000));
    wr(&mut m, 0xde00, 0x20);
    assert!(!m.read_is_rom(0x8000), "ROML now reads RAM");
    assert!(m.read_is_rom(0xa000), "ROMH is still ROM");
    wr(&mut m, 0xde00, 0x00);
    wr(&mut m, 0xde00, 0x22);
    assert!(!m.read_is_rom(0x9000), "mode $22 is RAM too");
}

// ── a press never breaks the instruction in flight ───────────────────────────────

/// Like [`freezer_rom`], with a loop whose instructions read ROM at their operands:
/// `JSR $FFE4` (KERNAL code at $E000+), `LDA $A123` (BASIC ROM), `EOR $E456` (KERNAL ROM),
/// `STA $0336`, `INC $0334`. IRQs are on (CLI), so the KERNAL's IRQ runs in the same loop.
fn rom_reading_rom() -> [Vec<u8>; 4] {
    let mut banks = freezer_rom();
    let b = &mut banks[0];
    let code: &[u8] = &[
        0x58, // CLI                      ($8018)
        0x20, 0xe4, 0xff, // loop: JSR $FFE4    ($8019)
        0xad, 0x23, 0xa1, // LDA $A123
        0x4d, 0x56, 0xe4, // EOR $E456
        0x8d, 0x36, 0x03, // STA $0336
        0xee, 0x34, 0x03, // INC $0334
        0x4c, 0x19, 0x80, // JMP loop
    ];
    b[0x18..0x18 + code.len()].copy_from_slice(code);
    banks
}

fn boot_loop() -> Option<Machine> {
    let mut m = boot(&ar_crt(&rom_reading_rom()))?;
    run(&mut m, 2_000_000);
    assert_ne!(m.read_full(0x0334), 0, "the loop runs");
    Some(m)
}

/// Press `k` cycles into the clone of `base` and run on: the program must survive it.
fn press_and_check(base: &Machine, k: u64, what: &str) {
    let mut m = base.clone();
    let expect = m.read_full(0xa123) ^ m.read_full(0xe456);
    run(&mut m, k);
    m.cart_press_button("freeze").unwrap();
    run(&mut m, 4_000);
    assert_eq!(m.read_full(0x0335), 1, "{what} +{k}: the handler ran, from the cartridge's vector");
    let f = frozen_state(&m);
    assert_eq!((f.mode, f.nmi, f.nmi_at), (0, false, None), "{what} +{k}: and the cartridge is back to an 8K game");
    let c = m.read_full(0x0334);
    assert_eq!(m.read_full(0x0336), expect, "{what} +{k}: the instruction read the ROM it names");
    run(&mut m, 4_000);
    assert_ne!(m.read_full(0x0334), c, "{what} +{k}: the loop runs on");
    assert!(!m.c64_core.is_jammed, "{what} +{k}: no JAM");
}

#[test]
fn s4_a_press_on_every_cycle_of_a_window_never_breaks_the_instruction_in_flight() {
    let Some(base) = boot_loop() else { skip!("no C64 ROMs") };
    // 3 passes of the loop (~55 cycles each, plus the KERNAL's GETIN): every cycle
    for k in 0..200 {
        press_and_check(&base, k, "loop");
    }
}

#[test]
fn s4_a_press_around_an_irq_entry_never_breaks_the_program() {
    let Some(mut m) = boot_loop() else { skip!("no C64 ROMs") };
    // find the next IRQ entry ($FF48 is the KERNAL's IRQ vector target), then take a state
    // some 80 cycles before it (the run is deterministic: the same IRQ comes again)
    let mut probe = m.clone();
    let mut irq_at = None;
    for _ in 0..40_000 {
        probe.run_for_full_capped(1, 1, &mut NullSink, |_, _, _, _, _, _, _| {});
        if (0xff48..0xff50).contains(&probe.c64_core.reg_pc) {
            irq_at = Some(probe.c64_core.clk);
            break;
        }
    }
    let irq_at = irq_at.expect("an IRQ comes");
    let ahead = irq_at - m.c64_core.clk - 80;
    run(&mut m, ahead);
    let base = m;
    let span = irq_at - base.c64_core.clk;
    assert!(span >= 60, "a state far enough ahead of the IRQ: {span}");
    // presses from before the IRQ sequence starts to after its vector fetch
    for k in 0..span + 12 {
        press_and_check(&base, k, "irq");
    }
}
