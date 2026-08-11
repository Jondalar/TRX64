//! trx64-cli (library face).
//!
//! The crate is primarily a binary (`src/main.rs`), but the verb/dispatch layer is
//! exposed here as a `[lib]` so integration tests can drive the `Engine` on a single
//! in-process machine (the binary's `mon` one-shot boots a fresh machine per call, so
//! a multi-step scripted check needs the lib). Additive — no second runtime path.

use std::path::Path;

pub mod audio;
pub mod boot_cmd;
pub mod convert_cmd;
pub mod diff_cmd;
pub mod disasm_cmd;
pub mod sandbox_cmd;
pub mod engine;
pub mod ftcolor;
pub mod keymap;
pub mod tui;
pub mod window;

pub use engine::{CmdResult, Engine, StateSnapshot};

/// Resolve the ROM directory, trying the likely locations in order and picking the
/// first that actually has the KERNAL. This makes the distributed binary work with a
/// `roms/` folder sitting next to it (the handout layout), while still honouring
/// `$C64RE_ROOT` for the in-tree dev setup. `--rom-dir` overrides this entirely.
pub fn default_rom_dir() -> std::path::PathBuf {
    // A self-contained handout (binary + roms/ beside it) seeds ~/.trx64/roms on its
    // first run, so every OTHER TRX64 binary on this machine — the brew-installed one
    // above all, whose own folder is wiped by each upgrade — finds ROMs afterwards
    // without anyone configuring anything. Once, never overwriting, and it says so.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Some(msg) = trx64_core::user_dir::seed_user_roms_from(&dir.join("roms")) {
                eprintln!("[trx64] {msg}");
            }
        }
    }
    let candidates = rom_dir_candidates();
    candidates
        .iter()
        .find(|p| p.join(KERNAL_FILE).exists())
        .cloned()
        .unwrap_or_else(|| {
            candidates.into_iter().next().unwrap_or_else(|| std::path::PathBuf::from("roms"))
        })
}

/// The file whose presence decides that a directory IS a ROM set.
const KERNAL_FILE: &str = "kernal-901227-03.bin";

/// The directories [`default_rom_dir`] probes, in order.
///
/// Split out from the resolver so a failure can print what was actually searched.
/// "not found" without the search list is the kind of message that sends people
/// reading source to find out where the program looked.
pub fn rom_dir_candidates() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let mut candidates: Vec<PathBuf> = Vec::new();
    // 1. Explicit C64RE checkout (dev / daemon parity).
    if let Ok(root) = std::env::var("C64RE_ROOT") {
        candidates.push(PathBuf::from(root).join("resources").join("roms"));
    }
    // 2. `roms/` next to the executable — the distributed handout (trx64cli + roms/).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("roms"));
        }
    }
    // 3. `~/.trx64/roms` — the set that survives a package upgrade. Under Homebrew the
    //    executable lives in the Cellar, so candidate 2 is deleted by the next upgrade;
    //    this one is not. Deliberately AFTER candidate 2, so a stale seeded copy can
    //    never shadow a set someone put next to the binary on purpose.
    if let Some(d) = trx64_core::user_dir::user_rom_dir() {
        candidates.push(d);
    }
    // 4. `roms/` in the current working directory.
    candidates.push(PathBuf::from("roms"));
    // 5. Dev fallback: the in-tree C64RE checkout.
    candidates.push(
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP"))
            .join("resources")
            .join("roms"),
    );
    candidates
}

/// Explain a boot that failed for want of ROMs, and say what to do about it.
///
/// This is the first thing a new user sees — `brew install trx64` then `trx64cli`
/// lands here — so it has to carry the whole story: that the ROMs are deliberately
/// absent, which files are wanted, and the two ways to point at them. The previous
/// message was the `io::Error`'s Debug form, `Io(Os { code: 2, kind: NotFound, .. })`,
/// which tells a stranger nothing and names no remedy.
pub fn rom_missing_help(tried: &Path, err: &dyn std::fmt::Display) -> String {
    let mut s = format!("no usable C64 ROMs in {}\n  {err}\n\n", tried.display());
    s.push_str(
        "TRX64 ships no ROMs and cannot: they are Commodore's property. Supply your own.\n\
         \n\
         Required (3 files, 20 KB):\n  \
           kernal-901227-03.bin   basic-901226-01.bin   chargen-901225-01.bin\n\
         Optional — without it the 1541 drive stays dead:\n  \
           dos1541-325302-01+901229-05.bin   (or the alias 1541.bin)\n\
         \n\
         Point trx64cli at them, either way:\n  \
           trx64cli --rom-dir /path/to/roms ...\n  \
           export C64RE_ROOT=/path/to/c64re      # uses <that>/resources/roms\n",
    );
    if let Some(d) = trx64_core::user_dir::user_rom_dir() {
        // The upgrade-proof spot, named so a first-time reader learns where the files go.
        s.push_str(&format!("  ...or drop them in {}\n", d.display()));
    }
    // Only worth printing when the path came from the search — with an explicit
    // --rom-dir, listing directories the user did not ask about is just noise.
    let candidates = rom_dir_candidates();
    if candidates.iter().any(|c| c == tried) {
        s.push_str("\nSearched, in order:\n");
        for c in &candidates {
            let why = if !c.exists() {
                "no such directory"
            } else if !c.join(KERNAL_FILE).exists() {
                "directory exists, but no kernal-901227-03.bin"
            } else {
                "ok"
            };
            s.push_str(&format!("  {} — {why}\n", c.display()));
        }
        if std::env::var_os("C64RE_ROOT").is_none() {
            s.push_str("  (C64RE_ROOT is not set, so its candidate is absent from this list)\n");
        }
    }
    s
}

/// Boot a fresh in-process machine from `rom_dir` and wrap it in an [`Engine`].
pub fn boot_engine(rom_dir: &Path) -> Result<Engine, String> {
    let state = trx64_daemon::create_embedded_state(rom_dir)
        .map_err(|e| rom_missing_help(rom_dir, &e))?;
    Ok(Engine::new(state))
}
