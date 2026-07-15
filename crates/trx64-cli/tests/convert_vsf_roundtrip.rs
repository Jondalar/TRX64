//! Spec 791 — `convert-vsf` round-trip integration test.
//!
//! Convert a real (gitignored, local) VSF sample → `.c64re`, load the `.c64re` back
//! into a fresh machine, and assert the resumed state matches the VSF-loaded machine
//! (CPU regs + a RAM spot-check) and CONTINUES (a short run does not land in the
//! BASIC warm-start/idle loop). Skips cleanly when the ROMs or a sample are absent —
//! there is no CI here (see project memory: no GitHub CI/CD).
//!
//! NEUTRALITY: no title/person names. The sample is resolved from `TRX64_SAMPLE_VSF`
//! or a `tests/fixtures/*.vsf` glob (both local/gitignored).

use std::path::PathBuf;

use trx64_cli::{convert_cmd, default_rom_dir};
use trx64_core::c64re_snapshot::restore_runtime_checkpoint;
use trx64_core::native_snapshot::read_native_snapshot;
use trx64_core::vsf::load_vsf_report;
use trx64_core::{Machine, NullSink};

fn rom_dir_or_skip() -> Option<PathBuf> {
    let d = default_rom_dir();
    if d.join("kernal-901227-03.bin").exists() {
        Some(d)
    } else {
        eprintln!("[skip] no C64 ROMs at {} — convert-vsf round-trip skipped", d.display());
        None
    }
}

/// Resolve a sample `.vsf`: the `TRX64_SAMPLE_VSF` env var, else the first
/// `tests/fixtures/*.vsf`. Both are local / gitignored — no title names committed.
fn sample_vsf_or_skip() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("TRX64_SAMPLE_VSF") {
        let pb = PathBuf::from(&p);
        if pb.exists() {
            return Some(pb);
        }
        eprintln!("[skip] TRX64_SAMPLE_VSF={p} does not exist");
        return None;
    }
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures");
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "vsf").unwrap_or(false) {
                return Some(p);
            }
        }
    }
    eprintln!(
        "[skip] no sample .vsf (set TRX64_SAMPLE_VSF or drop one in {}/)",
        dir.display()
    );
    None
}

#[test]
fn convert_vsf_roundtrip_resumes() {
    let Some(rom_dir) = rom_dir_or_skip() else { return };
    let Some(sample) = sample_vsf_or_skip() else { return };
    let sample_str = sample.to_string_lossy().to_string();
    let bytes = std::fs::read(&sample).expect("read sample vsf");

    // Reference machine A: load the VSF directly (the fidelity-report entry).
    let mut a = Machine::new();
    a.boot_from_dir(&rom_dir).expect("boot A");
    let rep_a = load_vsf_report(&mut a, &bytes).expect("load A");
    eprintln!(
        "sample source={} fidelity={} absent={:?}",
        rep_a.source,
        rep_a.fidelity.as_str(),
        rep_a.absent
    );
    // The load returns an explicit fidelity verdict — never the old errors=[] signal.
    assert!(!rep_a.fidelity.as_str().is_empty(), "an explicit fidelity is reported");

    // Convert the SAME VSF → .c64re via the CLI code path.
    let out = std::env::temp_dir().join(format!("trx64-convert-{}.c64re", std::process::id()));
    let out_str = out.to_string_lossy().to_string();
    let text = convert_cmd::run_convert(&rom_dir, &sample_str, &out_str, false, None).expect("convert-vsf");
    eprintln!("{text}");

    // Machine B: load the produced .c64re back.
    let snap = std::fs::read(&out).expect("read .c64re");
    let read = read_native_snapshot(&snap).expect("read native snapshot");
    let mut b = Machine::new();
    b.boot_from_dir(&rom_dir).expect("boot B");
    restore_runtime_checkpoint(&mut b, &read.checkpoint).expect("restore B");

    // CPU regs match the VSF-loaded reference.
    assert_eq!(b.c64_core.reg_pc, a.c64_core.reg_pc, "PC");
    assert_eq!(b.c64_core.reg_a, a.c64_core.reg_a, "A");
    assert_eq!(b.c64_core.reg_x, a.c64_core.reg_x, "X");
    assert_eq!(b.c64_core.reg_y, a.c64_core.reg_y, "Y");
    assert_eq!(b.c64_core.reg_sp, a.c64_core.reg_sp, "SP");
    assert_eq!(b.c64_core.clk, a.c64_core.clk, "clk (full 64-bit)");

    // RAM spot-check across low/mid/high image.
    for &addr in &[0x0000usize, 0x0100, 0x0400, 0x0801, 0x1000, 0x8000, 0xc000, 0xfffe] {
        assert_eq!(b.ram[addr], a.ram[addr], "ram[{addr:#06x}] mismatch");
    }

    // The restored state CONTINUES: a short run does not sit in the BASIC editor
    // warm-start / idle loop ($E5CD..$E5D5, which includes $E5D1).
    let pc0 = b.c64_core.reg_pc;
    let clk0 = b.c64_core.clk;
    let mut sink = NullSink;
    b.run_for_full(200_000, &mut sink, |_, _, _, _, _, _, _| {});
    assert!(b.c64_core.clk > clk0, "clock advanced on resume");
    let pc = b.c64_core.reg_pc;
    assert!(
        !(0xE5CDu16..=0xE5D5).contains(&pc),
        "resumed into BASIC warm-start ${pc:04x} (pc0=${pc0:04x}) — not continuing"
    );

    let _ = std::fs::remove_file(&out);
}
