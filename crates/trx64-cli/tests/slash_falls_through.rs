//! The cockpit's `/` namespace and the daemon's verb set are the SAME set.
//!
//! `/run`, `/pause`, `/warp`, `/reset`, `/power` exist on both sides, and the
//! daemon's own help calls the `/` prefix input sugar for exactly that reason. The
//! cockpit nevertheless kept a hand-written list and answered "unknown command"
//! for everything else — so every verb the daemon grew had to be mirrored by hand
//! or was simply missing from this front-end. That is BUG-040's drift, and it bit
//! again the day `turbo` shipped: the daemon had the verb, the cockpit did not.
//!
//! Skips gracefully when the ROMs are absent (constructing the machine needs them).

use std::path::Path;

use trx64_cli::{boot_engine, default_rom_dir, Engine};

fn engine_or_skip() -> Option<Engine> {
    let rom_dir = default_rom_dir();
    if !Path::new(&rom_dir).join("kernal-901227-03.bin").exists() {
        eprintln!("[skip] slash_falls_through: ROMs absent at {}", rom_dir.display());
        return None;
    }
    match boot_engine(&rom_dir) {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("[skip] slash_falls_through: boot failed: {e}");
            None
        }
    }
}

/// A daemon verb the cockpit has no entry for still works through `/`.
#[test]
fn a_daemon_verb_the_cockpit_never_mirrored_still_works() {
    let Some(engine) = engine_or_skip() else { return };

    let out = engine.exec_line("/turbo").output;
    assert!(
        out.contains("machine=c64"),
        "/turbo must reach the daemon, not the cockpit's list: {out}"
    );
    assert!(
        !out.contains("unknown command"),
        "the cockpit answered instead of forwarding: {out}"
    );
}

/// Arguments survive the forward — a verb with a subcommand is the normal case.
#[test]
fn arguments_survive_the_forward() {
    let Some(engine) = engine_or_skip() else { return };

    let out = engine.exec_line("/turbo mode 128").output;
    assert!(out.contains("machine=128"), "the argument reached the daemon: {out}");

    let on = engine.exec_line("/turbo on").output;
    assert!(on.contains("speed bit=SET"), "and so did the next one: {on}");
}

/// A real typo still fails — from the daemon, which is the one place that can say.
#[test]
fn a_typo_still_fails_but_from_the_one_authority() {
    let Some(engine) = engine_or_skip() else { return };

    let out = engine.exec_line("/tubro").output;
    assert!(!out.is_empty(), "a typo must not answer with silence");
    assert!(
        out.to_lowercase().contains("unknown"),
        "the daemon says it does not know the verb: {out}"
    );
}

/// The cockpit's OWN verbs are unaffected — they are matched before the fallthrough.
#[test]
fn the_cockpits_own_verbs_still_win() {
    let Some(engine) = engine_or_skip() else { return };

    let out = engine.exec_line("/help").output;
    assert!(out.len() > 100, "/help is still the cockpit's own: {out}");
}
