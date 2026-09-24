//! The drive positions, the drive type and the folder device through the typed FFI
//! (Specs 870–873): the calls a Swift host makes, against a real booted machine.
//!
//! ROMs: as in `smoke.rs` (`C64RE_ROOT`, default the C64RE checkout); skipped with a
//! note when absent. The 1581 part also needs `dos1581-318045-02.bin` (VICE's
//! `data/DRIVES`, Commodore IP, not bundled) and is skipped without it.

use trx64_ffi::{Runtime, Trx64Error};

fn rom_dir() -> std::path::PathBuf {
    let root = std::env::var("C64RE_ROOT")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP").to_string());
    std::path::PathBuf::from(root).join("resources").join("roms")
}

fn rom_1581() -> Option<std::path::PathBuf> {
    let p = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../vice/vice/data/DRIVES"))
        .join("dos1581-318045-02.bin");
    p.exists().then_some(p)
}

/// A scratch directory, removed on drop.
struct TmpDir(std::path::PathBuf);
impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        let p = std::env::temp_dir().join(format!("trx64-ffi-drives-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn file(&self, name: &str, bytes: &[u8]) -> String {
        let p = self.0.join(name);
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().to_string()
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn runtime(roms: &std::path::Path) -> Option<std::sync::Arc<Runtime>> {
    if !roms.join("kernal-901227-03.bin").exists() {
        eprintln!("[drives] ROMs not found at {} — skipping", roms.display());
        return None;
    }
    let rt = Runtime::new(roms.to_string_lossy().to_string()).expect("Runtime::new");
    rt.create_session(true).expect("create_session");
    Some(rt)
}

const D64_BYTES: usize = 174_848;
const D81_BYTES: usize = 819_200;

#[test]
fn position_b_powers_on_at_9_and_takes_a_d64() {
    let Some(rt) = runtime(&rom_dir()) else { return };
    let tmp = TmpDir::new("b");

    let d = rt.drives().expect("drives");
    assert_eq!(d.len(), 2);
    assert_eq!((d[0].position.as_str(), d[1].position.as_str()), ("A", "B"));
    assert!(d[0].powered && d[0].device == 8 && d[0].kind == "1541");
    assert!(!d[1].powered, "B comes up off");
    assert_eq!(d[1].unit_jumpers, 9);
    assert!(d[0].disk.is_none() && d[1].disk.is_none());

    let p = rt.drive_power(9, true).expect("B on");
    assert!(p.powered && p.device == 9);
    let d64 = tmp.file("b.d64", &vec![0u8; D64_BYTES]);
    let m = rt.mount_at(d64.clone(), 9).expect("mount at 9");
    assert_eq!((m.kind.as_str(), m.slot), ("d64", Some(9)));

    let d = rt.drives().expect("drives");
    assert!(d[0].disk.is_none(), "A is still empty");
    let disk = d[1].disk.as_ref().expect("B has the disk");
    assert_eq!((disk.path.as_str(), disk.format.as_str()), (d64.as_str(), "d64"));
    let b = rt.drive_status(9).expect("status 9");
    assert_eq!((b.position.as_str(), b.device, b.powered), ("B", 9, true));
    assert_eq!(b.side, 0, "a 1541 has one side");

    // Stopped and reset-held are reported and released.
    assert!(rt.drive_stop(9, true).expect("stop").stopped);
    assert!(rt.drive_status(9).unwrap().stopped);
    assert!(!rt.drive_stop(9, false).expect("run on").stopped);
    assert!(rt.drive_reset(9, Some(true)).expect("hold").reset_held);
    assert!(rt.drives().unwrap()[1].reset_held);
    assert!(!rt.drive_reset(9, Some(false)).expect("release").reset_held);
    rt.drive_reset(9, None).expect("pulse");
    rt.drive_power_cycle(9).expect("power cycle");

    // The jumpers: B moved to 10 is in force at its next reset.
    let u = rt.set_drive_unit(9, 10).expect("jumpers");
    assert_eq!((u.device, u.jumpers, u.in_force), (9, 10, false));
    let e = rt.set_drive_unit(9, 8).expect_err("A is at 8");
    assert!(matches!(&e, Trx64Error::Dispatch { message, .. } if message.contains("position A")), "{e}");

    rt.unmount_at(9).expect("eject B");
    assert!(rt.drive_status(9).unwrap().disk.is_none());
    let e = rt.drive_status(11).expect_err("nothing at 11");
    assert!(matches!(&e, Trx64Error::Dispatch { message, .. } if message.contains("no drive at unit 11")), "{e}");
}

#[test]
fn the_existing_media_calls_still_address_unit_8() {
    let Some(rt) = runtime(&rom_dir()) else { return };
    let tmp = TmpDir::new("compat");
    rt.drive_power(9, true).expect("B on");
    let d64 = tmp.file("a.d64", &vec![0u8; D64_BYTES]);
    // `slot` is not sent: 9 here still mounts into the drive at 8, as before.
    let m = rt.mount(d64.clone(), 9).expect("mount");
    assert_eq!(m.slot, Some(8));
    let d = rt.drives().unwrap();
    assert!(d[0].disk.is_some() && d[1].disk.is_none());
    rt.swap(d64).expect("swap");
    assert!(rt.unmount(9).expect("unmount").ok);
    assert!(rt.drives().unwrap()[0].disk.is_none(), "the disk at 8 came out");
}

#[test]
fn a_type_change_is_refused_while_powered_and_a_d81_goes_into_a_1581() {
    // A ROM directory with the 1581 DOS in it, when there is one: the runtime loads it
    // at boot, and the 1581 then powers on into its own DOS.
    let base = rom_dir();
    let tmp = TmpDir::new("1581");
    let roms = match rom_1581() {
        Some(dos) if base.join("kernal-901227-03.bin").exists() => {
            let dir = tmp.0.join("roms");
            std::fs::create_dir_all(&dir).unwrap();
            for e in std::fs::read_dir(&base).unwrap().flatten() {
                std::fs::copy(e.path(), dir.join(e.file_name())).unwrap();
            }
            std::fs::copy(dos, dir.join("dos1581-318045-02.bin")).unwrap();
            Some(dir)
        }
        _ => {
            eprintln!("[drives] no 1581 DOS — the type change runs, the power-on is skipped");
            None
        }
    };
    let Some(rt) = runtime(roms.as_deref().unwrap_or(&base)) else { return };

    // Powered → refused, naming the position; nothing changed.
    rt.drive_power(9, true).expect("B on");
    let e = rt.set_drive_type(9, "1581".into()).expect_err("B is powered");
    assert!(
        matches!(&e, Trx64Error::Dispatch { message, .. } if message.contains("position B") && message.contains("switch it off")),
        "{e}"
    );
    assert_eq!(rt.drive_status(9).unwrap().kind, "1541");
    let e = rt.set_drive_type(8, "1571".into()).expect_err("no such board");
    assert!(matches!(&e, Trx64Error::Dispatch { message, .. } if message.contains("1571")), "{e}");

    // Off → allowed.
    assert!(!rt.drive_power(9, false).expect("B off").powered);
    let t = rt.set_drive_type(9, "1581".into()).expect("type while off");
    assert_eq!((t.kind.as_str(), t.device), ("1581", 9));
    assert!(t.ejected.is_none());

    // A D81 goes in (a 1541 refuses it).
    let d81 = tmp.file("blank.d81", &vec![0u8; D81_BYTES]);
    let e = rt.mount_at(d81.clone(), 8).expect_err("A is a 1541");
    assert!(matches!(&e, Trx64Error::Dispatch { message, .. } if message.contains("D81")), "{e}");
    let m = rt.mount_at(d81.clone(), 9).expect("D81 into the 1581");
    assert_eq!(m.kind, "d81");
    let b = &rt.drives().unwrap()[1];
    assert_eq!(b.kind, "1581");
    assert_eq!(b.disk.as_ref().map(|d| d.format.as_str()), Some("d81"));

    if roms.is_some() {
        rt.drive_power(9, true).expect("the 1581 on");
        rt.run_cycles(2_000_000).expect("run");
        let s = rt.drive_status(9).unwrap();
        assert!(s.powered && s.kind == "1581");
        assert!(s.drive_pc >= 0x8000, "the 1581 runs its DOS: pc ${:04X}", s.drive_pc);
        rt.drive_power(9, false).expect("off again");
    }

    // Back to a 1541: the D81 does not fit and comes out, named.
    let t = rt.set_drive_type(9, "1541".into()).expect("back to 1541");
    assert_eq!(t.ejected.as_ref().map(|e| e.format.as_str()), Some("d81"));
    assert!(rt.drives().unwrap()[1].disk.is_none());
}

#[test]
fn a_folder_attaches_lists_and_detaches() {
    let Some(rt) = runtime(&rom_dir()) else { return };
    let tmp = TmpDir::new("folder");
    let dir = tmp.0.join("disk");
    std::fs::create_dir_all(dir.join("games")).unwrap();
    std::fs::write(dir.join("games/start.prg"), [0x01, 0x08, 0, 0]).unwrap();
    let path = dir.to_string_lossy().to_string();

    assert!(rt.folders().expect("folders").is_empty());
    let e = rt.attach_folder(8, path.clone(), false, None, None).expect_err("8 is drive A's");
    assert!(matches!(&e, Trx64Error::Dispatch { message, .. } if message.contains("position A")), "{e}");

    let f = rt
        .attach_folder(10, path.clone(), true, Some("games/start.prg".into()), None)
        .expect("attach at 10");
    assert_eq!((f.unit, f.read_only, f.profile.as_str()), (10, true, "ultimate"));
    assert_eq!(f.boot.as_deref(), Some("games/start.prg"));
    let g = rt.attach_folder(11, path.clone(), false, None, Some("vice".into())).expect("attach at 11");
    assert_eq!((g.profile.as_str(), g.boot), ("vice", None));

    let list = rt.folders().expect("folders");
    assert_eq!(list.iter().map(|f| f.unit).collect::<Vec<_>>(), vec![10, 11]);
    assert!(list[0].path.ends_with("disk"), "{}", list[0].path);

    rt.detach_folder(10).expect("detach 10");
    rt.detach_folder(11).expect("detach 11");
    assert!(rt.folders().unwrap().is_empty());
    let e = rt.detach_folder(10).expect_err("already gone");
    assert!(matches!(&e, Trx64Error::Dispatch { message, .. } if message.contains("no folder device")), "{e}");
}
