//! Issue #4 — the 1581 DOS is looked up per file in every ROM directory, not only in the
//! one the KERNAL came from. A machine booted from a directory with the C64 set and the
//! 1541 DOS, the 1581 DOS only in a second directory: the 1581 boots and lists a D81.
//!
//!   cargo test --release -p trx64-session --test issue4_1581_dos

#![allow(dead_code, unused_imports)]

include!("../../trx64-core/tests/common/d81_kit.rs");

use trx64_session::Session;

/// Two temp ROM directories: `a` with the C64 set and the 1541 DOS, `b` with only the
/// 1581 DOS.
fn split_rom_dirs(tag: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("trx64-issue4-{tag}-{}", std::process::id()));
    let (a, b) = (base.join("a"), base.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    for f in ["kernal-901227-03.bin", "basic-901226-01.bin", "chargen-901225-01.bin", DOS_1541] {
        std::fs::copy(Path::new(ROM_DIR).join(f), a.join(f)).unwrap();
    }
    std::fs::write(b.join(DOS_1581), rom_1581().unwrap()).unwrap();
    (a, b)
}

#[test]
fn the_1581_dos_from_another_rom_directory_boots_the_1581() {
    need_roms!();
    let (a, b) = split_rom_dirs("found");
    let mut s = Session::new("issue4");
    s.rom_dirs = vec![a.clone(), b.clone()];
    s.power_on(&a).expect("power on from the KERNAL directory");
    let m = &mut s.machine;
    assert!(m.drive8.has_rom_1581() && m.drive_b.has_rom_1581(), "the DOS came from {}", b.display());
    assert!(m.no_1581_dos(DrivePosition::A).is_none());
    m.set_drive_power(DrivePosition::A, false).unwrap();
    m.set_drive_type(DrivePosition::A, DriveType::Drive1581).unwrap();
    m.drive8.mount(d81(directory_image())).expect("a D81 fits a 1581");
    m.set_drive_power(DrivePosition::A, true).unwrap();
    frames(m, 200);
    let out = load_dir(m, 8);
    assert!(out.contains("THE1581DISK") && out.contains("THIRD"), "LOAD\"$\",8 lists the D81:\n{out}");

    // And again after the C64's power cycle: the lookup belongs to the session's boot.
    s.power_off();
    s.power_on(&a).expect("power on");
    assert!(s.machine.drive8.has_rom_1581(), "a power cycle finds it again");
}

#[test]
fn without_a_1581_dos_anywhere_the_refusal_names_the_file_and_the_directories() {
    need_roms!();
    let (a, b) = split_rom_dirs("absent");
    std::fs::remove_file(b.join(DOS_1581)).unwrap();
    let mut s = Session::new("issue4");
    s.rom_dirs = vec![a.clone(), b.clone()];
    s.power_on(&a).expect("power on");
    let why = s.machine.no_1581_dos(DrivePosition::A).expect("no DOS anywhere");
    assert!(why.starts_with("no 1581 DOS (dos1581-318045-02.bin"), "{why}");
    assert!(why.contains(&a.display().to_string()) && why.contains(&b.display().to_string()), "both directories named: {why}");
}
