//! Parity probe: load the golden c64re-own VSF, re-save, and diff each module
//! byte-for-byte against the c64re-produced bytes. Any divergence = a field
//! mapping bug in vsf.rs vs c64re's module-mapping.ts.
//!
//! This is a DIAGNOSTIC (prints diffs); the hard assertion lives in the
//! round-trip unit test inside vsf.rs.

use std::collections::BTreeMap;

/// The golden c64re VSF carries copyrighted Commodore ROM data, so it is NOT
/// tracked in the repo (gitignored). Read it at runtime if present; the test
/// skips cleanly when it is absent (e.g. a fresh clone).
fn golden_bytes() -> Option<Vec<u8>> {
    std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/vsf/c64re-reset.vsf")).ok()
}

/// Parse a compact-format VSF into (name -> data bytes), in file order.
fn parse_modules(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut off = 19usize; // magic
    off += 2; // maj/min
    while bytes[off] != 0 { off += 1; }
    off += 1; // machine name null
    let mut out = Vec::new();
    while off < bytes.len() {
        let start = off;
        while off < bytes.len() && bytes[off] != 0 { off += 1; }
        let name = String::from_utf8_lossy(&bytes[start..off]).to_string();
        off += 1; // null
        off += 2; // maj/min
        let len = (bytes[off] as usize)
            | ((bytes[off + 1] as usize) << 8)
            | ((bytes[off + 2] as usize) << 16)
            | ((bytes[off + 3] as usize) << 24);
        off += 4;
        out.push((name, bytes[off..off + len].to_vec()));
        off += len;
    }
    out
}

#[test]
fn golden_c64re_roundtrip_diff() {
    let Some(golden) = golden_bytes() else {
        eprintln!("SKIP golden_c64re_roundtrip_diff: tests/fixtures/vsf/c64re-reset.vsf absent (copyrighted ROM data, not in repo)");
        return;
    };
    let golden: &[u8] = &golden;
    let mut m = trx64_core::Machine::new();
    // Load the golden c64re VSF into TRX64.
    let res = trx64_core::vsf::load_vsf(&mut m, golden).expect("load golden");
    eprintln!("loaded modules: {:?}", res.loaded_modules);
    eprintln!("ignored modules: {:?}", res.ignored_modules);
    eprintln!("errors: {:?}", res.errors);

    // Re-save and diff per module.
    let resaved = trx64_core::vsf::save_vsf(&mut m);
    let golden_mods: BTreeMap<_, _> = parse_modules(golden).into_iter().collect();
    let resaved_mods: BTreeMap<_, _> = parse_modules(&resaved).into_iter().collect();

    for (name, gdata) in &golden_mods {
        match resaved_mods.get(name) {
            None => eprintln!("MODULE {name}: MISSING in re-save"),
            Some(rdata) => {
                if gdata == rdata {
                    eprintln!("MODULE {name}: MATCH ({} bytes)", gdata.len());
                } else if gdata.len() != rdata.len() {
                    eprintln!(
                        "MODULE {name}: SIZE DIFF golden={} resaved={}",
                        gdata.len(),
                        rdata.len()
                    );
                } else {
                    let diffs: Vec<usize> = (0..gdata.len())
                        .filter(|&i| gdata[i] != rdata[i])
                        .collect();
                    eprintln!(
                        "MODULE {name}: {} byte diffs at offsets {:?}",
                        diffs.len(),
                        &diffs[..diffs.len().min(40)]
                    );
                    for &i in diffs.iter().take(24) {
                        eprintln!("   off {i:>3}: golden={:02x} resaved={:02x}", gdata[i], rdata[i]);
                    }
                }
            }
        }
    }
    for name in resaved_mods.keys() {
        if !golden_mods.contains_key(name) {
            eprintln!("MODULE {name}: EXTRA in re-save (not in golden)");
        }
    }
}
