//! Which rotation engine actually runs, per game of the 7-game gate?
//!
//! VICE has two implementations of the rotating disk and picks one per image type
//! (`driveimage.c:222`):
//!
//!   complicated_image_loaded = (P64 || G64 || G71)
//!     0 → rotation_1541_simple  — "very simple and fast emulation for perfect
//!                                  images like those coming from dxx files"
//!     1 → rotation_1541_gcr     — the read-channel circuit: UE7/UF4 counters, the
//!                                  2.5 µs flux filter, and random flux reversals
//!                                  18 µs after the last real one
//!
//! `docs/vice-1541-arch.md:231` writes that rule down, and our attach does not
//! implement it. This probe reports, per gate title, which engine the machine is
//! actually on — before the boot, after the mount, and after the LOAD.
//!
//!   cargo test -p trx64-core --test engine_per_game_probe -- --ignored --nocapture

use std::path::Path;

use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const SAMPLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/samples");

fn roms_present() -> bool {
    Path::new(ROM_DIR).join("kernal-901227-03.bin").exists()
}

fn engine(m: &Machine) -> &'static str {
    if m.drive8.rotation.complicated_image_loaded != 0 {
        "rotation_1541_gcr (circuit)"
    } else {
        "rotation_1541_simple"
    }
}

/// What VICE would be on for this image type, per driveimage.c:222.
fn vice_engine(kind: &DiskKind) -> &'static str {
    match kind {
        DiskKind::G64 => "rotation_1541_gcr (circuit)",
        DiskKind::D64 => "rotation_1541_simple",
    }
}

fn probe(file: &str, kind: DiskKind, name: &str) {
    if !roms_present() {
        eprintln!("skip {name}: ROMs absent");
        return;
    }
    let path = format!("{SAMPLES}/{file}");
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("skip {name}: sample absent ({path})");
        return;
    };

    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut sink = NullSink;
    m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});
    let at_boot = engine(&m);

    m.drive8.attach_disk(DiskImage {
        kind: kind.clone(),
        bytes,
        backing_path: Some(path.clone()),
        read_only: false,
    });
    let after_mount = engine(&m);

    m.run_for_full(800_000, &mut sink, |_, _, _, _, _, _, _| {});
    // LOAD"*",8,1 — injected into the BASIC keyboard buffer, exactly as the gate
    // does it (seven_game_gate.rs:36-41).
    let keys = b"LOAD\"*\",8,1\r";
    for (i, b) in keys.iter().enumerate() {
        m.poke(0x0277 + i as u16, &[*b]);
    }
    m.poke(0x00c6, &[keys.len() as u8]);
    for _ in 0..200 {
        m.run_for_full(50_000, &mut sink, |_, _, _, _, _, _, _| {});
    }
    let after_load = engine(&m);

    let want = vice_engine(&kind);
    let verdict = if after_load == want { "  " } else { "!!" };
    println!(
        "{verdict} {name:<12} {kind:?}  boot={at_boot:<26} mount={after_mount:<26} load={after_load:<26} VICE={want}"
    );
}

#[test]
#[ignore = "probe; run with --ignored --nocapture"]
fn which_engine_runs_per_gate_title() {
    println!("\n  title        image  engine at each stage                                                             what VICE runs");
    println!("  {}", "-".repeat(120));
    probe("scramble_infinity.d64", DiskKind::D64, "scramble");
    probe("POLARBEAR.d64", DiskKind::D64, "polarbear");
    probe("motm.g64", DiskKind::G64, "motm");
    probe("green_beret[ocean_1986](!).g64", DiskKind::G64, "greenberet");
    probe("impossible_mission_ii[epyx_1987](!).g64", DiskKind::G64, "impossible2");
    probe("last_ninja_remix_s1[system3_1991].g64", DiskKind::G64, "lastninja");
    probe("maniac_mansion_s1[activision_1987](german)(manual)(!).g64", DiskKind::G64, "maniac");
    println!("  {}", "-".repeat(120));
    println!("  '!!' = we are on a different engine than VICE would be.\n");
}
