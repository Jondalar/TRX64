//! `trx64cli diff A.c64re B.c64re` — Spec 794 whitebox component-diff.
//!
//! Reads two `.c64re` snapshots, extracts their `checkpoint` trees, and runs the
//! `trx64_core::checkpoint_diff` compute with a caller exclusion mask → a per-
//! component equivalence verdict. No machine, no daemon, no live session — a pure
//! file→file capability, the diff step of the sandbox fan-out (score N candidates
//! against a baseline). Meaning / provenance lands in C64RE.

use serde_json::json;

use trx64_core::checkpoint_diff::{diff_checkpoints, format_component_diff, ExcludeMask};
use trx64_core::native_snapshot::read_native_snapshot;

/// Read a `.c64re` file and return its `checkpoint` JSON tree.
fn read_checkpoint(path: &str) -> Result<serde_json::Value, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    let read = read_native_snapshot(&bytes).map_err(|e| format!("parse {path}: {e}"))?;
    Ok(read.checkpoint)
}

/// Parse a `--exclude` value `space:from-to` (e.g. `driveram:0x0000-0x07FF`) into the
/// `{ space, from, to }` shape `ExcludeMask::from_json` understands. Returns None (the
/// entry is skipped) on a malformed spec.
fn parse_exclude_range(s: &str) -> Option<serde_json::Value> {
    let (space, rng) = s.split_once(':')?;
    let (from, to) = rng.split_once('-')?;
    Some(json!({ "space": space.trim(), "from": from.trim(), "to": to.trim() }))
}

#[allow(clippy::too_many_arguments)]
pub fn run_diff(
    a_path: &str,
    b_path: &str,
    exclude: &[String],
    components: &[String],
    lanes: &[String],
    presets: &[String],
    json_out: bool,
) -> Result<String, String> {
    let a = read_checkpoint(a_path)?;
    let b = read_checkpoint(b_path)?;

    let ranges: Vec<serde_json::Value> = exclude.iter().filter_map(|s| parse_exclude_range(s)).collect();
    let mask_json = json!({
        "components": components,
        "lanes": lanes,
        "presets": presets,
        "ranges": ranges,
    });
    let mask = ExcludeMask::from_json(&mask_json);

    let diff = diff_checkpoints(&a, &b, &mask);
    if json_out {
        Ok(serde_json::to_string_pretty(&diff).unwrap_or_else(|_| diff.to_string()))
    } else {
        Ok(format_component_diff(&diff))
    }
}
