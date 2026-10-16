//! An overlay patch must not outlive the anchor restore, for EVERY banked family.
//! The restore re-attaches the cartridge from its original `.crt` bytes and lays the
//! anchor's writable image over it (flash families), so a patch made after the capture is
//! gone — exactly what the daemon's `runtime/overlay_run` relies on between variant runs.
//! The checkpoint is captured the way the daemon captures it (`cartridge_image.raw_bytes`
//! + `writable_image`), and restored with the same `restore_runtime_checkpoint`.

use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, restore_runtime_checkpoint};
use trx64_core::cart::{CartType, MapperType};
use trx64_core::Machine;

const ORIGINAL: u8 = 0x5a;
const PATCH: u8 = 0xa5;

/// A CRT whose every chip byte is `ORIGINAL`: `(bank, load address, size)` per CHIP packet.
fn crt(hw: u16, exrom: u8, game: u8, chips: &[(u16, u16, u16)]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   ");
    v.extend_from_slice(&0x40u32.to_be_bytes());
    v.extend_from_slice(&0x0100u16.to_be_bytes());
    v.extend_from_slice(&hw.to_be_bytes());
    v.extend_from_slice(&[exrom, game, 0, 0, 0, 0, 0, 0]);
    v.extend_from_slice(&[0u8; 32]);
    for &(bank, load, size) in chips {
        v.extend_from_slice(b"CHIP");
        v.extend_from_slice(&(0x10u32 + size as u32).to_be_bytes());
        v.extend_from_slice(&0u16.to_be_bytes());
        v.extend_from_slice(&bank.to_be_bytes());
        v.extend_from_slice(&load.to_be_bytes());
        v.extend_from_slice(&size.to_be_bytes());
        v.extend(std::iter::repeat_n(ORIGINAL, size as usize));
    }
    v
}

fn rolls_back(label: &str, expect: MapperType, bytes: Vec<u8>, space: &str, bank: u16, addr: u16) {
    let mut m = Machine::new();
    let (_, t) = m.attach_cart_typed(&bytes, label, CartType::Auto).expect("attach");
    assert_eq!(t, expect, "{label}: mapper");
    let clk = m.c64_core.clk;
    let raw = m.cartridge_image.as_ref().unwrap().raw_bytes.clone();
    let flash = m.cartridge.as_mut().and_then(|c| c.writable_image(clk));
    let cp = capture_runtime_checkpoint(&m, "", "", None, None, Some(&raw), flash.as_deref());

    let (dirty, gen) = {
        let c = m.cartridge.as_ref().unwrap();
        (c.is_writable_dirty(), c.writable_generation())
    };
    let cart = m.cartridge.as_mut().unwrap();
    assert_eq!(cart.overlay_bank_read(space, bank, addr).unwrap(), ORIGINAL, "{label}: image starts original");
    cart.overlay_bank_write(space, bank, addr, PATCH).unwrap();
    assert_eq!(cart.overlay_bank_read(space, bank, addr).unwrap(), PATCH, "{label}: patch applied");
    assert_eq!((cart.is_writable_dirty(), cart.writable_generation()), (dirty, gen), "{label}: overlay is not a program");
    let persisted = cart.writable_image(clk);
    assert_eq!(persisted.is_some(), flash.is_some(), "{label}");

    restore_runtime_checkpoint(&mut m, &cp).expect("restore");
    let cart = m.cartridge.as_ref().expect("cart re-attached");
    assert_eq!(cart.mapper_type(), expect, "{label}: same family after restore");
    assert_eq!(cart.overlay_bank_read(space, bank, addr).unwrap(), ORIGINAL, "{label}: patch gone after restore");
    assert_eq!(cart.is_writable_dirty(), dirty, "{label}: dirty state restored");
}

#[test]
fn a_restore_drops_the_overlay_in_every_banked_family() {
    let l8 = |b| (b, 0x8000, 0x2000);
    rolls_back("normal8k", MapperType::Normal8k, crt(0, 0, 1, &[l8(0)]), "roml", 0, 0x8123);
    rolls_back("normal16k", MapperType::Normal16k, crt(0, 0, 0, &[(0, 0x8000, 0x4000)]), "romh", 0, 0xa123);
    rolls_back("ultimax", MapperType::Ultimax, crt(0, 1, 0, &[(0, 0xe000, 0x2000)]), "romh", 0, 0xe123);
    rolls_back("magicdesk", MapperType::MagicDesk, crt(19, 0, 1, &[l8(0), l8(5)]), "roml", 5, 0x9f00);
    rolls_back("magicdesk16", MapperType::MagicDesk16, crt(85, 0, 0, &[(0, 0x8000, 0x4000), (5, 0x8000, 0x4000)]), "romh", 5, 0xbf00);
    rolls_back("ocean", MapperType::Ocean, crt(5, 0, 0, &[l8(0), l8(5)]), "roml", 5, 0x9f00);
    rolls_back("easyflash", MapperType::EasyFlash, crt(32, 1, 0, &[l8(0), l8(5)]), "roml", 5, 0x9f00);
    rolls_back("gmod2", MapperType::Gmod2, crt(60, 0, 1, &[l8(0), l8(5)]), "roml", 5, 0x9f00);
    rolls_back("megabyter", MapperType::MegaByter, crt(86, 0, 1, &[l8(0), l8(100)]), "roml", 100, 0x9f00);
    rolls_back("c64megacart", MapperType::C64MegaCart, crt(88, 0, 1, &[l8(0), l8(200)]), "roml", 200, 0x9f00);
    rolls_back("gmod4", MapperType::Gmod4, crt(87, 1, 0, &[l8(0), l8(1), l8(6), l8(7)]), "romh", 3, 0xa123);
}
