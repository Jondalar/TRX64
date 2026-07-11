//! `trx64cli boot` — boot a disk/cart in an ISOLATED process to a state, then dump
//! a `.c64re` snapshot.
//!
//! This is a Spec 787 scratch instance: its own in-process machine
//! (`create_embedded_state` via `boot_engine`), no daemon, no shared/live session —
//! so it never touches the human's `integrated-1`. Used to mint seeds/fixtures for
//! `trx64cli sandbox --seed`. Drives the SAME dispatch the daemon uses, in-process.
//!
//! Timing matters twice:
//!  - keys are scheduled from the CURRENT clock, so we run to READY (`--warmup`)
//!    BEFORE the first `--type`, else the keys are presented during the cold boot.
//!  - a `LOAD` and its `RUN` must be SEPARATE `--type`s: typing `RUN` while `LOAD`
//!    is still loading loses it (the editor is not reading the keyboard mid-LOAD).
//!    Each `--type` is therefore followed by a `--type-gap` run so its command
//!    completes before the next keys arrive.

use std::path::Path;

use base64::Engine as _;
use serde_json::json;

use crate::boot_engine;
use crate::engine::Engine;

fn compact(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// Advance `total` cycles in `chunk`-sized session/run calls (the controller stays
/// paused across calls; session/run is a bounded drive, not a resume).
fn run_cycles(engine: &Engine, total: u64, chunk: u64) -> Result<(), String> {
    let mut done: u64 = 0;
    while done < total {
        let step = chunk.min(total - done);
        engine.rpc("session/run", json!({ "cycles": step }))?;
        done += step;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn run_boot(
    rom_dir: &Path,
    disk: &str,
    warmup: u64,
    types: &[String],
    type_gap: u64,
    cycles: u64,
    chunk: u64,
    dump: &str,
    render: Option<&str>,
) -> Result<String, String> {
    let engine = boot_engine(rom_dir).map_err(|e| format!("{e}"))?;
    let mut log: Vec<String> = Vec::new();

    // Mount the medium — power-cycles THIS process's machine only (cold boot).
    let m = engine.rpc("media/mount", json!({ "path": disk }))?;
    log.push(format!("mount {disk}: {}", compact(&m)));

    // Force the controller paused so session/run may drive cycles directly (it
    // refuses while running, and a mount can flip it on).
    let _ = engine.rpc("debug/pause", json!({ "source": "cli-boot" }));

    // Warm up to the READY prompt BEFORE typing (keys are scheduled from now on).
    run_cycles(&engine, warmup, chunk)?;
    log.push(format!("warmup {warmup} cycles (~{}s → READY)", warmup / 985_248));

    // Each --type: queue keys (raw control chars — session/type does NOT decode
    // escapes, so convert literal \r/\n → RETURN here), then run --type-gap so the
    // command finishes before the next keys arrive.
    for t in types {
        let decoded = t.replace("\\r", "\r").replace("\\n", "\r");
        let r = engine.rpc("session/type", json!({ "text": decoded }))?;
        log.push(format!("type {t:?}: {}", compact(&r)));
        run_cycles(&engine, type_gap, chunk)?;
        log.push(format!("  gap {type_gap} cycles"));
    }

    // Final settle (game boots / reaches an in-play state).
    run_cycles(&engine, cycles, chunk)?;
    log.push(format!("settle {cycles} cycles (~{}s PAL)", cycles / 985_248));

    // Optional screenshot (verify what actually booted) — decode the render_screen
    // data-URL PNG and write it.
    if let Some(png_path) = render {
        let r = engine.rpc("runtime/render_screen", json!({ "scale": 2 }))?;
        let url = r.get("dataUrl").and_then(|v| v.as_str()).unwrap_or("");
        let b64 = url.strip_prefix("data:image/png;base64,").unwrap_or(url);
        let png = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| format!("render base64 decode: {e}"))?;
        std::fs::write(png_path, &png).map_err(|e| format!("write {png_path}: {e}"))?;
        log.push(format!("render → {png_path} ({} bytes)", png.len()));
    }

    // Dump the .c64re snapshot (write_native_snapshot on this process's machine).
    let d = engine.rpc("snapshot/dump", json!({ "path": dump }))?;
    log.push(format!("dump {dump}: {}", compact(&d)));

    Ok(log.join("\n"))
}
