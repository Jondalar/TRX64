//! Spec 875 §12.9 — a host's controller in the 1581 lives through the C64's power cycle
//! in the session: the same box, `power(false)` as the machine goes, then `rebase`,
//! `power(true)` and `drive_reset` as it comes back, and the DOS lists through it.
//!
//!   cargo test --release -p trx64-session --test fdc_power_cycle

#![allow(dead_code, unused_imports)]

#[path = "../../trx64-core/tests/common/sector_fdc.rs"]
mod sector_fdc;

include!("../../trx64-core/tests/common/d81_kit.rs");

use sector_fdc::{Ev, SectorFdc};
use trx64_session::Session;

/// A ROM directory with the C64 set and the 1581 DOS, as `boot_from_dir` wants it.
fn rom_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("trx64-875-roms-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    for f in ["kernal-901227-03.bin", "basic-901226-01.bin", "chargen-901225-01.bin", DOS_1541] {
        std::fs::copy(Path::new(ROM_DIR).join(f), d.join(f)).unwrap();
    }
    std::fs::write(d.join(DOS_1581), rom_1581().unwrap()).unwrap();
    d
}

fn fdc(m: &Machine) -> &SectorFdc {
    m.fdc_controller_as::<SectorFdc>(DrivePosition::A).expect("SectorFdc at A")
}

#[test]
fn a_host_controller_survives_the_c64_power_cycle() {
    need_roms!();
    let roms = rom_dir();
    let mut s = Session::new("875");
    s.power_on(&roms).expect("power on");
    let m = &mut s.machine;
    m.set_drive_power(DrivePosition::A, false).unwrap();
    m.set_drive_type(DrivePosition::A, DriveType::Drive1581).unwrap();
    m.attach_fdc_controller(DrivePosition::A, Box::new(SectorFdc::new("SectorFdc", directory_image()))).unwrap();
    m.set_drive_power(DrivePosition::A, true).unwrap();
    frames(m, 200);
    assert!(load_dir(m, 8).contains("THE1581DISK"), "before the power cycle");
    let identity = fdc(&s.machine) as *const SectorFdc as usize;
    let k = fdc(&s.machine).log.len();

    s.power_off();
    s.power_on(&roms).expect("power on");
    let m = &mut s.machine;
    frames(m, 200);
    assert_eq!(fdc(m) as *const SectorFdc as usize, identity, "the same box");
    let ev: Vec<Ev> = fdc(m).log[k..].iter().filter(|e| !matches!(e, Ev::Store { .. } | Ev::Served { .. } | Ev::BoardOut { .. })).cloned().collect();
    assert!(
        matches!(ev[..], [Ev::Power { on: false }, Ev::Rebase { .. }, Ev::Power { on: true }, Ev::DriveReset { clk: 0 }, Ev::FirstClockTo { .. }]),
        "{ev:?}"
    );
    assert!(m.drive8.get_attached_disk().is_none(), "no medium of TRX64's");
    let out = load_dir(m, 8);
    assert!(out.contains("THE1581DISK") && out.contains("THIRD"), "LOAD\"$\",8 lists afterwards:\n{out}");
    let _ = std::fs::remove_dir_all(roms);
}
