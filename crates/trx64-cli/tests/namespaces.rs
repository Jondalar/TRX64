//! CLI-FEEL S1 — `!` filesystem namespace + verb aliases (umount/undump/settings) +
//! the bare-FS-verb cockpit nudge. Drives the `Engine` verb layer on a single
//! in-process machine (like smoke.rs) and asserts the routing.
//!
//! GUARDRAIL under test: `!` is a COCKPIT routing layer only — `!ls` routes to the
//! monitor's bare `ls`; the shared `run_monitor` verbs stay bare-callable (unchanged).
//!
//! Skips gracefully when the ROMs are absent (constructing the machine needs them).

use std::path::Path;

use trx64_cli::{boot_engine, default_rom_dir, Engine};

fn engine_or_skip() -> Option<Engine> {
    let rom_dir = default_rom_dir();
    if !Path::new(&rom_dir).join("kernal-901227-03.bin").exists() {
        eprintln!("[skip] namespaces: ROMs absent at {}", rom_dir.display());
        return None;
    }
    match boot_engine(&rom_dir) {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("[skip] namespaces: boot failed: {e}");
            None
        }
    }
}

/// `!ls` routes to the monitor's `ls` (the FS verb), listing the cwd. We `!cd` into a
/// temp dir with a known marker file, then `!ls` must list it — proving the `!`
/// prefix reaches `run_monitor` and did NOT get intercepted as a cockpit hint.
#[test]
fn bang_ls_routes_to_monitor_fs() {
    let Some(engine) = engine_or_skip() else { return };

    let dir = std::env::temp_dir().join(format!("trx64_s1_bangls_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("marker.prg"), b"\x01\x08marker").unwrap();

    // `!cd` is itself an FS verb routed through `!`.
    let cd = engine.exec_line(&format!("!cd {}", dir.display()));
    assert!(!cd.output.contains("unknown"), "!cd routed to monitor: {}", cd.output);

    let out = engine.exec_line("!ls").output;
    assert!(
        out.contains("marker.prg"),
        "!ls lists the cwd via the monitor FS verb, got: {out}"
    );
    // It must be the real listing, not the cockpit hint.
    assert!(
        !out.contains("filesystem commands live behind"),
        "!ls must not emit the bare-verb hint, got: {out}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A BARE FS verb (no `!`) is nudged toward the `!` namespace — the cockpit prints a
/// one-line hint instead of silently running the monitor's copy.
#[test]
fn bare_fs_verb_returns_hint() {
    let Some(engine) = engine_or_skip() else { return };

    let out = engine.exec_line("ls").output;
    assert!(
        out.contains("filesystem commands live behind '!'"),
        "bare `ls` returns the cockpit hint, got: {out}"
    );
    assert!(out.contains("!ls"), "hint points at the `!` form, got: {out}");

    // The hint is verb-specific: bare `load` → `!load` (not the VM `/load`).
    let load = engine.exec_line("load").output;
    assert!(load.contains("!load"), "bare `load` hints `!load`, got: {load}");
}

/// A BARE non-FS verb still passes straight through to the monitor (guardrail: the
/// nudge only fires for the FS-verb set, not the whole monitor surface).
#[test]
fn bare_non_fs_verb_still_passthrough() {
    let Some(engine) = engine_or_skip() else { return };
    engine.exec_line("/power on");

    // `r` = monitor registers; must NOT be hinted.
    let r = engine.exec_line("r").output;
    assert!(
        !r.contains("filesystem commands live behind"),
        "bare `r` is a monitor verb, not FS — no hint, got: {r}"
    );
    assert!(r.contains("ADDR") || r.contains("AC") || r.contains("PC"), "registers: {r}");
}

/// `/umount` is an alias of `/eject` — same code path, identical output.
#[test]
fn umount_aliases_eject() {
    let Some(engine) = engine_or_skip() else { return };

    let eject = engine.exec_line("/eject").output;
    let umount = engine.exec_line("/umount").output;
    assert_eq!(eject, umount, "/umount and /eject take the same path");
    assert!(eject.contains("EJECT"), "eject output: {eject}");
}

/// `/undump` is an alias of `/restore` — same code path (usage message on no-arg).
#[test]
fn undump_aliases_restore() {
    let Some(engine) = engine_or_skip() else { return };

    let restore = engine.exec_line("/restore").output;
    let undump = engine.exec_line("/undump").output;
    assert_eq!(restore, undump, "/undump and /restore take the same path");
    let loadsnapshot = engine.exec_line("/loadsnapshot").output;
    assert_eq!(restore, loadsnapshot, "/loadsnapshot and /restore take the same path");
}

/// `/snapshot` is an alias of `/dump` — our runtime snapshot IS the .c64re dump, so an
/// agent reaching for VICE's "snapshot" gets the capability instead of unknown-command.
#[test]
fn snapshot_aliases_dump() {
    let Some(engine) = engine_or_skip() else { return };

    let dump = engine.exec_line("/dump").output;
    let snapshot = engine.exec_line("/snapshot").output;
    assert_eq!(dump, snapshot, "/snapshot and /dump take the same path");
    assert!(!dump.contains("unknown"), "/dump is a known verb: {dump}");
    assert!(!snapshot.contains("unknown"), "/snapshot is a known verb: {snapshot}");
}

/// `/settings` returns a non-empty read-only status summary.
#[test]
fn settings_returns_summary() {
    let Some(engine) = engine_or_skip() else { return };
    engine.exec_line("/power on");

    let out = engine.exec_line("/settings").output;
    assert!(!out.trim().is_empty(), "/settings is non-empty");
    assert!(out.contains("settings"), "settings header: {out}");
    assert!(out.contains("pacing"), "settings lists pacing: {out}");
    assert!(out.contains("cart"), "settings lists cart: {out}");
}

/// Bare `!` prints the FS help (not an error, not empty).
#[test]
fn bang_only_prints_fs_help() {
    let Some(engine) = engine_or_skip() else { return };
    let out = engine.exec_line("!").output;
    assert!(out.contains("!-commands"), "bare `!` → FS help, got: {out}");
    assert!(out.contains("!ls"), "FS help lists !ls, got: {out}");
}
