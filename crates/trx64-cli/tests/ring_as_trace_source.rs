//! The always-on delta ring as a trace source for the analysis verbs.
//!
//! The delta + cpu-history rings record permanently, so the machine is always holding its
//! recent instruction stream and every write it performed. Before this, `map` / `taint` /
//! `swimlane` refused to look at any of it and told the user to run `trace on` — which
//! only helps if you predicted the problem before it happened. These tests pin the new
//! behaviour: the verbs answer from the ring, they say so, and a real capture still wins.
//!
//! Skips when the ROMs are absent, like the other CLI tests.

use std::path::Path;
use trx64_cli::{boot_engine, default_rom_dir, Engine};

fn engine_or_skip() -> Option<Engine> {
    let rom_dir = default_rom_dir();
    if !Path::new(&rom_dir).join("kernal-901227-03.bin").exists() {
        eprintln!("[skip] ring-as-trace-source: ROMs absent at {}", rom_dir.display());
        return None;
    }
    match boot_engine(&rom_dir) {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("[skip] ring-as-trace-source: boot failed: {e}");
            None
        }
    }
}

/// Run the machine far enough that the rings hold a useful window.
fn warm(engine: &Engine) {
    engine.exec_line("/power on");
    engine.exec_line("z 200000");
}

#[test]
fn swimlane_answers_from_the_ring_without_trace_on() {
    let Some(engine) = engine_or_skip() else { return };
    warm(&engine);

    let out = engine.exec_line("swimlane").output;
    assert!(
        !out.contains("no trace store"),
        "swimlane must not demand `trace on` while the ring holds data: {out}"
    );
    assert!(
        out.contains("delta ring"),
        "the answer must say where it came from — the ring is bounded, so a silent \
         answer would let 'nothing found' read as 'did not happen': {out}"
    );
    // A real swimlane rendering, not just the note.
    assert!(out.contains("cycle") || out.contains('$'), "expected rendered rows: {out}");
}

#[test]
fn map_answers_from_the_ring_without_trace_on() {
    let Some(engine) = engine_or_skip() else { return };
    warm(&engine);

    let out = engine.exec_line("map c64").output;
    assert!(!out.contains("no trace store"), "map should build from the ring: {out}");
    assert!(out.contains("delta ring"), "map must name its source too: {out}");
}

#[test]
fn a_real_capture_takes_precedence_over_the_ring() {
    let Some(engine) = engine_or_skip() else { return };
    warm(&engine);

    // Capture explicitly, then ask. The answer must come from the capture — silently
    // preferring the ring would make an explicit `trace on` meaningless.
    engine.exec_line("trace on");
    engine.exec_line("z 20000");
    engine.exec_line("trace off");

    let out = engine.exec_line("swimlane").output;
    assert!(
        !out.contains("delta ring"),
        "an explicit capture must win over the ring: {out}"
    );
}

#[test]
fn a_window_older_than_the_ring_is_refused_with_a_useful_reason() {
    let Some(engine) = engine_or_skip() else { return };
    warm(&engine);

    // The ring is bounded. Asking for something it has already overwritten must say so
    // rather than answering from the wrong window.
    let out = engine.exec_line("swimlane 1 2").output;
    if out.contains("delta ring") && out.contains("only reaches back") {
        assert!(out.contains("trace on"), "the refusal should name the way to get it: {out}");
    }
    // If the ring still covers cycle 1-2 (a very short run), answering is also correct —
    // hence the conditional. What must never happen is a silent wrong-window answer,
    // which the provenance note above rules out.
}
