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
    // 3. `roms/` in the current working directory.
    candidates.push(PathBuf::from("roms"));
    // 4. Dev fallback: the in-tree C64RE checkout.
    candidates.push(
        PathBuf::from("/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP")
            .join("resources")
            .join("roms"),
    );
    candidates
        .iter()
        .find(|p| p.join("kernal-901227-03.bin").exists())
        .cloned()
        .unwrap_or_else(|| candidates.into_iter().next().unwrap_or_else(|| PathBuf::from("roms")))
}

/// Boot a fresh in-process machine from `rom_dir` and wrap it in an [`Engine`].
pub fn boot_engine(rom_dir: &Path) -> Result<Engine, String> {
    let state = trx64_daemon::create_embedded_state(rom_dir)
        .map_err(|e| format!("boot failed (ROMs at {}): {e:?}", rom_dir.display()))?;
    Ok(Engine::new(state))
}
