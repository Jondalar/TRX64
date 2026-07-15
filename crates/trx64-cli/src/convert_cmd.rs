//! `trx64cli convert-vsf` — one-way VSF → `.c64re` import onramp (Spec 791.2).
//!
//! Load a VICE (or c64re-own) `.vsf` into a FRESH machine (this process = one
//! isolated scratch instance, no daemon, no shared/live session), then dump a
//! `.c64re` native snapshot (Spec 707). The runtime is untouched: it already reads
//! `.c64re` (`sandbox --seed` / `snapshot/undump`). Boris does `convert-vsf` then
//! `sandbox --seed` / `undump`.
//!
//! The load returns a Spec 791.3 fidelity report — `loaded` / `coarse` / `absent`
//! module lists + an explicit `faithful | partial | inspection-only` verdict —
//! which this command surfaces (text + `--json`). That REPLACES the old `errors=[]`
//! signal so a caller can never again mistake an inspection-only import for a
//! resumable machine.
//!
//! Slice 1 scope: the machine-state core (full 64-bit clock, CPU, RAM, CIA, SID, the
//! VIC register head) is restored; the drive, cartridge, and VIC micro-pipeline are
//! NOT (later slices — 791.1a/b/c). A converted disk-game / EF-cart VSF therefore
//! reports `partial` with those modules in `absent`.

use std::path::Path;

use serde_json::json;

use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, RUNTIME_CHECKPOINT_SCHEMA_VERSION};
use trx64_core::native_snapshot::{write_native_snapshot, WriteNativeSnapshotArgs};
use trx64_core::vsf::{load_vsf_report, VsfLoadReport};
use trx64_core::Machine;

/// Load `input` (.vsf) into a fresh machine booted from `rom_dir`, dump a `.c64re`
/// to `output`, and return the formatted report (text, or JSON when `json`).
pub fn run_convert(rom_dir: &Path, input: &str, output: &str, json: bool) -> Result<String, String> {
    let bytes = std::fs::read(input).map_err(|e| format!("read {input}: {e}"))?;

    // Fresh isolated machine (ROMs needed so the resumed state can execute against
    // KERNAL/BASIC/CHARGEN; the VSF's RAM image + port latches drive banking).
    let mut m = Machine::new();
    m.boot_from_dir(rom_dir)
        .map_err(|e| format!("boot ROMs from {}: {e:?}", rom_dir.display()))?;

    // Load the VSF with the fidelity report (Spec 791.3).
    let report: VsfLoadReport =
        load_vsf_report(&mut m, &bytes).map_err(|e| format!("load {input}: {e}"))?;

    // Capture the RuntimeCheckpoint (Spec 707). Slice 1 restores no drive/cart, so
    // those blobs are None (the c64re restore path tolerates null — drive stays cold).
    let checkpoint = capture_runtime_checkpoint(&m, "", "", None, None, None, None);
    let pc = m.c64_core.reg_pc as i64;
    let cycle = m.c64_core.clk as i64;

    let out_bytes = write_native_snapshot(WriteNativeSnapshotArgs {
        checkpoint,
        schema_version: RUNTIME_CHECKPOINT_SCHEMA_VERSION,
        media: Vec::new(),
        runtime_version: "trx64-runtime/1".to_string(),
        machine_model: "c64-pal".to_string(),
        provenance: None,
        pc,
        cycle,
    });

    if let Some(parent) = Path::new(output).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    std::fs::write(output, &out_bytes).map_err(|e| format!("write {output}: {e}"))?;

    if json {
        let out = json!({
            "input": input,
            "output": output,
            "source": report.source,
            "fidelity": report.fidelity.as_str(),
            "loaded": report.loaded,
            "coarse": report.coarse,
            "absent": report.absent,
            "pc": pc,
            "cycle": cycle,
            "fileBytes": out_bytes.len(),
        });
        serde_json::to_string(&out).map_err(|e| e.to_string())
    } else {
        Ok(format!(
            "convert-vsf: {input} → {output}\n  source={} fidelity={} pc=${:04x} cycle={} bytes={}\n  loaded=[{}]\n  coarse=[{}]\n  absent=[{}]",
            report.source,
            report.fidelity.as_str(),
            pc,
            cycle,
            out_bytes.len(),
            report.loaded.join(", "),
            report.coarse.join(", "),
            report.absent.join(", "),
        ))
    }
}
