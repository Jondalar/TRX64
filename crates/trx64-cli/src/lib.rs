//! trx64-cli (library face).
//!
//! The crate is primarily a binary (`src/main.rs`), but the verb/dispatch layer is
//! exposed here as a `[lib]` so integration tests can drive the `Engine` on a single
//! in-process machine (the binary's `mon` one-shot boots a fresh machine per call, so
//! a multi-step scripted check needs the lib). Additive — no second runtime path.

use std::path::Path;

pub mod audio;
pub mod engine;
pub mod keymap;
pub mod tui;
pub mod window;

pub use engine::{CmdResult, Engine, StateSnapshot};

/// Mirror the daemon's `rom_dir()` resolution: $C64RE_ROOT/resources/roms with the
/// daemon's default root.
pub fn default_rom_dir() -> std::path::PathBuf {
    let root = std::env::var("C64RE_ROOT")
        .unwrap_or_else(|_| "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP".to_string());
    std::path::PathBuf::from(root).join("resources").join("roms")
}

/// Boot a fresh in-process machine from `rom_dir` and wrap it in an [`Engine`].
pub fn boot_engine(rom_dir: &Path) -> Result<Engine, String> {
    let state = trx64_daemon::create_embedded_state(rom_dir)
        .map_err(|e| format!("boot failed (ROMs at {}): {e:?}", rom_dir.display()))?;
    Ok(Engine::new(state))
}
