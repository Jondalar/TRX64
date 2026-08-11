//! The one directory TRX64 owns in a user's home: `~/.trx64`.
//!
//! It already held the cockpit's command history; it now also holds the ROM set, so a
//! binary that was not shipped as a self-contained folder — anything installed by a
//! package manager — has somewhere to find ROMs without the user configuring anything.
//!
//! WHY A SHARED HELPER: the history resolved `$HOME` and nothing else, which silently
//! does not exist on Windows (there it is `%USERPROFILE%`), so persistent history had
//! never worked there. Two callers with two copies of that bug is one too many.
//!
//! This module only READS and, on an explicit seed, writes into its own directory. The
//! daemon deliberately keeps global host state out of the user's `~/.config/c64re` store
//! (see `recent_media`), and that principle is intact: nothing here touches anything
//! outside `~/.trx64`.

use std::path::{Path, PathBuf};

/// `~/.trx64`, or `None` when the platform tells us nothing about a home directory.
///
/// `HOME` on Unix, `USERPROFILE` on Windows — checked in that order so a Unix-shell
/// environment on Windows (git-bash, MSYS) keeps working the way its user expects.
pub fn trx64_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| PathBuf::from(h).join(".trx64"))
}

/// `~/.trx64/roms` — the ROM set that survives a package upgrade.
///
/// Under Homebrew the executable lives in the Cellar, so a `roms/` folder next to it is
/// deleted by the next `brew upgrade`. This path is the answer to that.
pub fn user_rom_dir() -> Option<PathBuf> {
    trx64_home().map(|h| h.join("roms"))
}

/// The file whose presence means "this directory is a ROM set".
pub const KERNAL_FILE: &str = "kernal-901227-03.bin";

/// Does this directory hold a usable ROM set?
pub fn has_roms(dir: &Path) -> bool {
    dir.join(KERNAL_FILE).is_file()
}

/// Copy a ROM set into `~/.trx64/roms` so every other TRX64 binary on this machine can
/// find it — the self-contained handout folder seeding the package-installed one.
///
/// Returns a human-readable line when it copied something, `None` when it did not.
///
/// THREE RULES, and each of them prevents a specific silent failure:
///
/// 1. NEVER OVERWRITE. It seeds only when the target has no KERNAL. A handout unpacked
///    today must not quietly replace a set the user curated yesterday.
/// 2. NEVER SILENTLY. These are Commodore's ROMs — someone else's copyrighted files. A
///    tool does not duplicate those around a user's home directory without saying so,
///    even when the user owns the copy.
/// 3. THE SOURCE STILL WINS. The caller keeps `roms/`-next-to-the-executable ahead of
///    this directory in its search order, so a stale seeded copy can never shadow the
///    set someone deliberately placed beside the binary.
pub fn seed_user_roms_from(src: &Path) -> Option<String> {
    if !has_roms(src) {
        return None;
    }
    let dest = user_rom_dir()?;
    if has_roms(&dest) {
        return None; // rule 1
    }
    std::fs::create_dir_all(&dest).ok()?;
    let mut copied = 0usize;
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(src).ok()?.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("bin") {
            continue;
        }
        let Some(name) = p.file_name() else { continue };
        if let Ok(n) = std::fs::copy(&p, dest.join(name)) {
            copied += 1;
            bytes += n;
        }
    }
    if copied == 0 {
        return None;
    }
    Some(format!(
        "seeded {} ROM file(s) ({:.0} KB) into {} from {}",
        copied,
        bytes as f64 / 1024.0,
        dest.display(),
        src.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("trx64-userdir-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn home_falls_back_to_userprofile() {
        // The reason this helper exists: `$HOME` alone is unset on Windows, so the
        // cockpit's persistent history never worked there.
        let saved_home = std::env::var_os("HOME");
        let saved_up = std::env::var_os("USERPROFILE");
        unsafe {
            std::env::remove_var("HOME");
            std::env::set_var("USERPROFILE", "/tmp/pretend-windows-home");
        }
        assert_eq!(trx64_home(), Some(PathBuf::from("/tmp/pretend-windows-home/.trx64")));
        unsafe {
            std::env::remove_var("USERPROFILE");
            assert_eq!(trx64_home(), None, "no home at all → no path, not a guess");
            if let Some(h) = saved_home {
                std::env::set_var("HOME", h);
            }
            if let Some(u) = saved_up {
                std::env::set_var("USERPROFILE", u);
            }
        }
    }

    #[test]
    fn seeding_copies_once_and_never_overwrites() {
        let src = scratch("src");
        std::fs::write(src.join(KERNAL_FILE), b"original").unwrap();
        std::fs::write(src.join("basic-901226-01.bin"), b"basic").unwrap();
        std::fs::write(src.join("notes.txt"), b"not a rom").unwrap();

        let home = scratch("home");
        let saved = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &home) };

        let msg = seed_user_roms_from(&src).expect("first run seeds");
        assert!(msg.contains("2 ROM file(s)"), "only .bin files: {msg}");
        let dest = home.join(".trx64").join("roms");
        assert!(dest.join(KERNAL_FILE).is_file());
        assert!(!dest.join("notes.txt").exists(), "non-ROM files are not swept along");

        // A second run must not touch what is already there.
        std::fs::write(dest.join(KERNAL_FILE), b"curated").unwrap();
        assert!(seed_user_roms_from(&src).is_none(), "seeding is once, not every start");
        assert_eq!(std::fs::read(dest.join(KERNAL_FILE)).unwrap(), b"curated");

        unsafe {
            match saved {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_source_without_a_kernal_is_not_a_rom_set() {
        let src = scratch("empty");
        std::fs::write(src.join("basic-901226-01.bin"), b"basic").unwrap();
        assert!(seed_user_roms_from(&src).is_none());
        let _ = std::fs::remove_dir_all(&src);
    }
}
