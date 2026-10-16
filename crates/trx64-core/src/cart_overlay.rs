//! Banked cart overlay helpers shared by the `CartMapper::overlay_bank_*` impls of the
//! read-only (per-bank window) and flat-image (flash) families.
//!
//! An overlay is a what-if patch of the STORED bank image. It never goes through a
//! mapper's write path (no flash command state machine, no dirty flag, no persist
//! generation), so it is neither a program nor an erase.

use crate::cart::{cart_overlay_target, CrtBank};
use std::collections::BTreeMap;

/// Which of a bank's three stored windows an overlay address lands in.
#[derive(Clone, Copy)]
enum Win {
    Roml,
    RomhA000,
    RomhE000,
}

/// Which windows a per-bank-window mapper actually serves (its visibility predicates).
#[derive(Clone, Copy)]
pub(crate) struct Served {
    pub roml: bool,
    pub romh_a000: bool,
    pub romh_e000: bool,
}

fn locate(
    banks: &BTreeMap<u16, CrtBank>,
    served: Served,
    space: &str,
    bank: u16,
    addr: u16,
) -> Result<(Win, usize), String> {
    let (_, base) = cart_overlay_target(space, addr)?;
    let (win, ok, name) = match base {
        0x8000 => (Win::Roml, served.roml, "ROML"),
        0xa000 => (Win::RomhA000, served.romh_a000, "ROMH at $A000"),
        _ => (Win::RomhE000, served.romh_e000, "ROMH at $E000"),
    };
    if !ok {
        return Err(format!("overlay {space} ${addr:04X}: this cartridge does not serve {name}"));
    }
    let Some(b) = banks.get(&bank) else {
        return Err(format!("overlay bank {bank} is not in this cartridge image"));
    };
    let present = match win {
        Win::Roml => b.roml.is_some(),
        Win::RomhA000 => b.romh_a000.is_some(),
        Win::RomhE000 => b.romh_e000.is_some(),
    };
    if !present {
        return Err(format!("overlay bank {bank} has no {name} image"));
    }
    Ok((win, (addr - base) as usize))
}

/// Write one byte into the stored window of a per-bank-window mapper.
pub(crate) fn banked_write(
    banks: &mut BTreeMap<u16, CrtBank>,
    served: Served,
    space: &str,
    bank: u16,
    addr: u16,
    byte: u8,
) -> Result<(), String> {
    let (win, off) = locate(banks, served, space, bank, addr)?;
    let b = banks.get_mut(&bank).expect("locate checked the bank");
    let w = match win {
        Win::Roml => b.roml.as_mut(),
        Win::RomhA000 => b.romh_a000.as_mut(),
        Win::RomhE000 => b.romh_e000.as_mut(),
    };
    w.expect("locate checked the window")[off] = byte;
    Ok(())
}

/// Read one byte back from the stored window of a per-bank-window mapper.
pub(crate) fn banked_read(
    banks: &BTreeMap<u16, CrtBank>,
    served: Served,
    space: &str,
    bank: u16,
    addr: u16,
) -> Result<u8, String> {
    let (win, off) = locate(banks, served, space, bank, addr)?;
    let b = &banks[&bank];
    let w = match win {
        Win::Roml => b.roml.as_ref(),
        Win::RomhA000 => b.romh_a000.as_ref(),
        Win::RomhE000 => b.romh_e000.as_ref(),
    };
    Ok(w.expect("locate checked the window")[off])
}

/// Index into a flat linear flash image of 8 KiB banks (`bank << 13 | window offset`).
/// `romh_e000_alias`: the family serves the SAME bank image at the ultimax `$E000`
/// window (C64MegaCart), so `romh $E000-$FFFF` addresses the ROML image.
pub(crate) fn flat_offset(
    len: usize,
    romh_e000_alias: bool,
    space: &str,
    bank: u16,
    addr: u16,
) -> Result<usize, String> {
    let (_, base) = cart_overlay_target(space, addr)?;
    let ok = match base {
        0x8000 => true,
        0xe000 => romh_e000_alias,
        _ => false,
    };
    if !ok {
        return Err(format!("overlay {space} ${addr:04X}: this cartridge does not serve that window"));
    }
    let off = ((bank as usize) << 13) | (addr & 0x1fff) as usize;
    if off >= len {
        return Err(format!(
            "overlay bank {bank} {space} ${addr:04X} → offset ${off:X} exceeds the image (${len:X})"
        ));
    }
    Ok(off)
}

#[cfg(test)]
mod tests {
    use crate::cart::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// One bank: (bank number, ROML fill, ROMH $A000 fill, ROMH $E000 fill).
    type Bank = (u16, Option<u8>, Option<u8>, Option<u8>);

    fn image(t: MapperType, banks: &[Bank]) -> ParsedCartridgeImage {
        let mut map: BTreeMap<u16, CrtBank> = BTreeMap::new();
        for &(n, l, a, e) in banks {
            map.insert(
                n,
                CrtBank { roml: l.map(|f| [f; 0x2000]), romh_a000: a.map(|f| [f; 0x2000]), romh_e000: e.map(|f| [f; 0x2000]) },
            );
        }
        ParsedCartridgeImage {
            name: "overlay".into(),
            mapper_type: t,
            exrom: 0,
            game: 1,
            banks: map,
            profiles: BTreeSet::new(),
            raw_bytes: Vec::new(),
        }
    }

    fn bi() -> BankInfo {
        BankInfo {
            cpu_port_direction: 0x2f,
            cpu_port_value: 0x37,
            basic_visible: false,
            kernal_visible: false,
            io_visible: true,
            char_visible: false,
            cartridge_attached: true,
            cartridge_exrom: Some(0),
            cartridge_game: Some(1),
            phi1: 0xff,
        }
    }

    fn mk(t: MapperType, banks: &[Bank]) -> Box<dyn CartMapper> {
        mapper_from_image(&image(t, banks)).unwrap()
    }

    fn live(m: &mut Box<dyn CartMapper>, addr: u16) -> Option<u8> {
        m.read(addr, &bi(), 0)
    }

    fn refused(r: Result<(), String>, why: &str) {
        let e = r.expect_err(why);
        assert!(e.contains("overlay"), "{why}: {e}");
    }

    /// The refusals every family shares: unknown space, a window the space does not
    /// cover, a bank the image does not hold.
    fn common_refusals(m: &mut Box<dyn CartMapper>, absent_bank: u16) {
        refused(m.overlay_bank_write("bogus", 0, 0x8000, 1), "unknown space");
        refused(m.overlay_bank_write("roml", 0, 0xa000, 1), "roml outside $8000-$9FFF");
        refused(m.overlay_bank_write("romh", 0, 0x8000, 1), "romh outside its windows");
        refused(m.overlay_bank_write("roml", absent_bank, 0x8000, 1), "bank out of range");
        assert!(m.overlay_bank_read("roml", absent_bank, 0x8000).is_err());
    }

    #[test]
    fn normal_8k_16k_and_ultimax() {
        let mut m = mk(MapperType::Normal16k, &[(0, Some(0x11), Some(0x22), None)]);
        m.overlay_bank_write("roml", 0, 0x9f00, 0xa1).unwrap();
        m.overlay_bank_write("romh", 0, 0xa010, 0xa2).unwrap();
        assert_eq!(m.overlay_bank_read("roml", 0, 0x9f00).unwrap(), 0xa1);
        assert_eq!(m.overlay_bank_read("romh", 0, 0xa010).unwrap(), 0xa2);
        assert_eq!(live(&mut m, 0x9f00), Some(0xa1));
        assert_eq!(live(&mut m, 0xa010), Some(0xa2));
        assert_eq!(live(&mut m, 0x9f01), Some(0x11), "neighbour untouched");
        refused(m.overlay_bank_write("romh", 0, 0xe000, 1), "a 16K cart has no ROMH at $E000");
        common_refusals(&mut m, 1);

        let mut m = mk(MapperType::Normal8k, &[(0, Some(0x11), None, None)]);
        m.overlay_bank_write("roml", 0, 0x8000, 0x77).unwrap();
        assert_eq!(live(&mut m, 0x8000), Some(0x77));
        refused(m.overlay_bank_write("romh", 0, 0xa000, 1), "an 8K cart has no ROMH");

        let mut m = mk(MapperType::Ultimax, &[(0, Some(0x33), None, Some(0x44))]);
        m.overlay_bank_write("romh", 0, 0xfffc, 0x09).unwrap();
        assert_eq!(live(&mut m, 0xfffc), Some(0x09));
        assert_eq!(m.overlay_bank_read("romh", 0, 0xfffd).unwrap(), 0x44);
        refused(m.overlay_bank_write("romh", 0, 0xa000, 1), "ultimax serves ROMH at $E000 only");

        let mut m = mk(MapperType::Ultimax, &[(0, None, None, Some(0x44))]);
        refused(m.overlay_bank_write("roml", 0, 0x8000, 1), "ultimax image with no ROML");
        m.overlay_bank_write("romh", 0, 0xe000, 1).unwrap();
    }

    #[test]
    fn magic_desk_and_magic_desk_16() {
        for (t, has_romh) in [(MapperType::MagicDesk, false), (MapperType::MagicDesk16, true)] {
            let a = if has_romh { Some(0xb0) } else { None };
            let mut m = mk(t, &[(0, Some(0x10), a, None), (5, Some(0x50), a, None)]);
            m.overlay_bank_write("roml", 5, 0x9f00, 0xee).unwrap();
            assert_eq!(m.overlay_bank_read("roml", 5, 0x9f00).unwrap(), 0xee);
            assert_eq!(m.overlay_bank_read("roml", 0, 0x9f00).unwrap(), 0x10, "bank 0 untouched");
            assert_eq!(live(&mut m, 0x9f00), Some(0x10), "bank 0 is mapped");
            m.write(0xde00, 5, &bi(), 0);
            assert_eq!(live(&mut m, 0x9f00), Some(0xee), "switching to bank 5 shows the patch");
            m.write(0xde00, 0, &bi(), 0);
            assert_eq!(live(&mut m, 0x9f00), Some(0x10));
            if has_romh {
                m.overlay_bank_write("romh", 5, 0xbffe, 0xd0).unwrap();
                m.write(0xde00, 5, &bi(), 0);
                assert_eq!(live(&mut m, 0xbffe), Some(0xd0));
                assert_eq!(m.overlay_bank_read("romh", 0, 0xbffe).unwrap(), 0xb0);
            } else {
                refused(m.overlay_bank_write("romh", 0, 0xa000, 1), "Magic Desk serves ROML only");
            }
            common_refusals(&mut m, 9);
        }
    }

    #[test]
    fn ocean_16k_mirrors_romh_onto_roml_and_8k_has_no_romh() {
        let mut m = mk(MapperType::Ocean, &[(0, Some(0x10), None, None), (3, Some(0x30), None, None)]);
        m.overlay_bank_write("romh", 3, 0xa010, 0xcc).unwrap();
        assert_eq!(m.overlay_bank_read("roml", 3, 0x8010).unwrap(), 0xcc, "one stored image");
        assert_eq!(m.overlay_bank_read("romh", 3, 0xa010).unwrap(), 0xcc);
        assert_eq!(m.overlay_bank_read("roml", 0, 0x8010).unwrap(), 0x10);
        m.write(0xde00, 3, &bi(), 0);
        assert_eq!(live(&mut m, 0x8010), Some(0xcc));
        assert_eq!(live(&mut m, 0xa010), Some(0xcc));
        refused(m.overlay_bank_write("romh", 3, 0xe000, 1), "Ocean has no $E000 window");
        common_refusals(&mut m, 9);

        // 512 KB: the 8K-game config, ROML only.
        let mut m = mk(MapperType::Ocean, &[(0, Some(0x10), None, None), (63, Some(0x63), None, None)]);
        m.overlay_bank_write("roml", 63, 0x8001, 0x99).unwrap();
        m.write(0xde00, 63, &bi(), 0);
        assert_eq!(live(&mut m, 0x8001), Some(0x99));
        refused(m.overlay_bank_write("romh", 63, 0xa000, 1), "8K Ocean has no ROMH");
    }

    /// Flash families: patch lands in the image and the live window, and is neither a
    /// program (dirty / generation) nor a write the persist path would see.
    fn flash_checks(t: MapperType, banks: &[Bank], bank: u16, select: impl Fn(&mut Box<dyn CartMapper>, u16), absent: u16) {
        let mut m = mk(t, banks);
        let (dirty, gen) = (m.is_writable_dirty(), m.writable_generation());
        m.overlay_bank_write("roml", bank, 0x9f00, 0xab).unwrap();
        assert_eq!(m.overlay_bank_read("roml", bank, 0x9f00).unwrap(), 0xab);
        assert_eq!(m.overlay_bank_read("roml", 0, 0x9f00).unwrap(), 0x00, "bank 0 untouched");
        assert_eq!(live(&mut m, 0x9f00), Some(0x00));
        select(&mut m, bank);
        assert_eq!(live(&mut m, 0x9f00), Some(0xab), "{t:?}: live window after the bank switch");
        assert_eq!(live(&mut m, 0x9eff), Some(bank as u8), "{t:?}: neighbour byte is the bank fill");
        assert_eq!((m.is_writable_dirty(), m.writable_generation()), (dirty, gen), "{t:?}: not a program");
        refused(m.overlay_bank_write("romh", bank, 0xa000, 1), "no ROMH at $A000");
        common_refusals(&mut m, absent);
    }

    #[test]
    fn gmod2() {
        flash_checks(MapperType::Gmod2, &[(0, Some(0), None, None), (5, Some(5), None, None)], 5, |m, b| {
            m.write(0xde00, b as u8, &bi(), 0);
        }, 64);
    }

    #[test]
    fn megabyter() {
        flash_checks(MapperType::MegaByter, &[(0, Some(0), None, None), (100, Some(100), None, None)], 100, |m, b| {
            m.write(0xde00, b as u8, &bi(), 0);
        }, 128);
    }

    #[test]
    fn c64megacart_including_the_ultimax_romh_alias() {
        let sel = |m: &mut Box<dyn CartMapper>, b: u16| {
            m.write(0xde00, (b & 0xff) as u8, &bi(), 0);
            m.write(0xdf00, (b >> 8) as u8, &bi(), 0);
        };
        flash_checks(MapperType::C64MegaCart, &[(0, Some(0), None, None), (200, Some(200), None, None)], 200, sel, 256);
        // ROMH $E000 is the same stored bank image as ROML, live in ultimax.
        let mut m = mk(MapperType::C64MegaCart, &[(0, Some(0), None, None), (200, Some(200), None, None)]);
        m.overlay_bank_write("romh", 200, 0xfff0, 0x5a).unwrap();
        assert_eq!(m.overlay_bank_read("roml", 200, 0x9ff0).unwrap(), 0x5a);
        sel(&mut m, 200);
        m.write(0xdf00, 0xc0, &bi(), 0); // ultimax, bank high bits 0
        assert_eq!(live(&mut m, 0xfff0), Some(0x5a));
        refused(m.overlay_bank_write("romh", 200, 0xa000, 1), "no ROMH at $A000");
    }

    #[test]
    fn gmod4_banks_are_16k_with_roml_even_and_romh_odd() {
        // 16K bank 3 = 8K images 6 (ROML half) and 7 (ROMH half).
        let banks: Vec<Bank> = (0..8).map(|n| (n, Some(n as u8), None, None)).collect();
        let mut m = mk(MapperType::Gmod4, &banks);
        let dirty = m.is_writable_dirty();
        m.overlay_bank_write("roml", 3, 0x8010, 0xaa).unwrap();
        m.overlay_bank_write("romh", 3, 0xa010, 0xbb).unwrap();
        assert_eq!(m.overlay_bank_read("roml", 3, 0x8010).unwrap(), 0xaa);
        assert_eq!(m.overlay_bank_read("romh", 3, 0xa010).unwrap(), 0xbb);
        assert_eq!(m.overlay_bank_read("roml", 2, 0x8010).unwrap(), 4, "neighbour bank untouched");
        // Banking on (CONTROL bit 6), both windows to bank 3 ($DE03).
        m.write(0xde04, 0x40, &bi(), 0);
        m.write(0xde03, 3, &bi(), 0);
        assert_eq!(live(&mut m, 0x8010), Some(0xaa));
        assert_eq!(live(&mut m, 0xa010), Some(0xbb));
        assert_eq!(m.is_writable_dirty(), dirty);
        refused(m.overlay_bank_write("roml", 512, 0x8000, 1), "past the 8 MiB image");
        refused(m.overlay_bank_write("bogus", 0, 0x8000, 1), "unknown space");
        refused(m.overlay_bank_write("roml", 0, 0xa000, 1), "roml outside its window");
        // $E000 is the cart's pinned half of bank 0.
        m.overlay_bank_write("romh", 0, 0xfffc, 0x07).unwrap();
        assert_eq!(live(&mut m, 0xfffc), Some(0x07));
    }

    #[test]
    fn self_config_refuses_until_locked_then_delegates() {
        let raw = vec![0u8; 0x20_0000];
        let mut m: Box<dyn CartMapper> = Box::new(SelfConfigCartMapper::new(&raw, "raw"));
        refused(m.overlay_bank_write("roml", 0, 0x8000, 1), "unlocked");
        assert!(m.overlay_bank_read("roml", 0, 0x8000).unwrap_err().contains("not locked"));
        m.write(0xdf00, 0, &bi(), 0); // an $DF00 write locks C64MegaCart
        assert_eq!(m.mapper_type(), MapperType::C64MegaCart);
        m.overlay_bank_write("roml", 3, 0x8000, 0x42).unwrap();
        assert_eq!(m.overlay_bank_read("roml", 3, 0x8000).unwrap(), 0x42);
        m.write(0xde00, 3, &bi(), 0);
        assert_eq!(live(&mut m, 0x8000), Some(0x42));
    }
}
