//! drive_stream_probe.rs — capture TRX64's drive8 PC+clk stream through the
//! scramble load, for alignment against the c64re reference (drive8 stream in
//! traces/scramble_run_ref.duckdb).
//!
//! Boots, mounts scramble, LOAD"*",8,1, RUN, then captures the deduplicated
//! drive-PC stream (pc, a, x, y, sp, p, drive_clk + the C64 clk at the sample)
//! into /tmp/trx64_drive.txt so the FIRST divergence vs the reference can be
//! pinned.
//!
//! Run with:
//!   cargo test -p trx64-core --test drive_stream_probe -- --ignored --nocapture

use std::path::Path;
use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::{BusKind, Machine, Observer};

const ROM_DIR: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
const SAMPLE: &str =
    "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/scramble_infinity.d64";

fn roms_present() -> bool {
    let d = Path::new(ROM_DIR);
    d.join("kernal-901227-03.bin").exists()
        && (d.join("dos1541-325302-01+901229-05.bin").exists() || d.join("1541.bin").exists())
}

fn inject_keys(m: &mut Machine, s: &[u8]) {
    for (i, b) in s.iter().enumerate() {
        m.poke(0x0277 + i as u16, &[*b]);
    }
    m.poke(0x00c6, &[s.len() as u8]);
}

#[derive(Default)]
struct NopSink;
impl Observer for NopSink {
    fn on_instruction(
        &mut self,
        _pc: u16,
        _op: u8,
        _b1: u8,
        _b2: u8,
        _a: u8,
        _x: u8,
        _y: u8,
        _sp: u8,
        _p: u8,
        _clk: u64,
    ) {
    }
    fn on_bus(&mut self, _k: BusKind, _a: u16, _v: u8, _pc: u16, _clk: u64, _o: u8) {}
    fn on_interrupt(&mut self, _vector: u16, _clk: u64) {}
}

#[test]
#[ignore = "scramble drive-stream probe; run explicitly with --ignored --nocapture"]
fn drive_stream_probe() {
    if !roms_present() {
        eprintln!("skip: ROMs absent");
        return;
    }
    let d64 = match std::fs::read(SAMPLE) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("skip: sample disk absent");
            return;
        }
    };

    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let mut sink = NopSink;

    m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: d64.clone(),
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    m.run_for_full(500_000, &mut sink, |_, _, _, _, _, _, _| {});

    // LOAD"*",8,1
    inject_keys(&mut m, b"LOAD\"*\",8,1\r");
    let mut load_done = false;
    for _ in 0..600 {
        m.run_for_full(50_000, &mut sink, |_, _, _, _, _, _, _| {});
        let pc = m.cpu6510.reg_pc;
        if (0xE5C0..=0xE5F0).contains(&pc) && m.read_full(0x00c6) == 0 {
            load_done = true;
            break;
        }
    }
    eprintln!("LOAD done={load_done} PC=${:04X}", m.cpu6510.reg_pc);
    if !load_done {
        eprintln!("LOAD did not finish; abort");
        return;
    }

    // RUN, then capture the drive stream for a large window. Record each
    // deduplicated drive-PC sample as: drive_clk, c64_clk, pc, a, x, y, sp, p.
    inject_keys(&mut m, b"RUN\r");

    // Capture format: one record per drive-PC change.
    let mut recs: Vec<(u64, u64, u16, u8, u8, u8, u8, u8)> = Vec::with_capacity(4_000_000);
    let cap_target = 3_000_000usize;
    let mut guard = 0u64;
    // ~30M C64 cycles is plenty to reach the give-up window (~25.9M) and beyond.
    while recs.len() < cap_target && guard < 32_000_000 {
        // Run one C64 instruction; capture any drive-PC change(s) it produced.
        // The drive_clk in the sample is the alignment key (matches the
        // reference trace's data_json 'clk'). Stamp the post-instruction C64 clk.
        let mut pending: Vec<(u64, u16, u8, u8, u8, u8, u8)> = Vec::new();
        m.run_for_full(1, &mut sink, |pc, a, x, y, sp, p, dclk| {
            pending.push((dclk, pc, a, x, y, sp, p));
        });
        let c64_clk = m.cpu6510.clk;
        for (dclk, pc, a, x, y, sp, p) in pending {
            recs.push((dclk, c64_clk, pc, a, x, y, sp, p));
        }
        guard += 1;
        if let Some(last) = recs.last() {
            // After we are well past the give-up window, stop.
            if last.1 > 27_500_000 {
                break;
            }
        }
    }
    eprintln!("captured {} drive records (guard={guard})", recs.len());

    let lines: Vec<String> = recs
        .iter()
        .map(|(dclk, c64clk, pc, a, x, y, sp, p)| {
            format!("{dclk}\t{c64clk}\t{pc:04X}\t{a:02X}\t{x:02X}\t{y:02X}\t{sp:02X}\t{p:02X}")
        })
        .collect();
    std::fs::write("/tmp/trx64_drive.txt", lines.join("\n")).ok();
    eprintln!("wrote /tmp/trx64_drive.txt");

    // Quick summary: where the drive ends, and a histogram of PCs.
    use std::collections::HashMap;
    let mut hist: HashMap<u16, u64> = HashMap::new();
    for r in &recs {
        *hist.entry(r.2).or_insert(0) += 1;
    }
    let mut top: Vec<_> = hist.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("== top TRX64 drive PCs ==");
    for (pc, n) in top.iter().take(20) {
        eprintln!("  ${:04X}: {}", pc, n);
    }
    // First time the drive reaches the give-up region ($E87B / $E9Cx).
    if let Some(r) = recs.iter().find(|r| (0xE870..=0xE9D0).contains(&r.2)) {
        eprintln!(
            "FIRST give-up region hit: drive_clk={} c64_clk={} pc=${:04X}",
            r.0, r.1, r.2
        );
    } else {
        eprintln!("drive never reached the give-up region in window");
    }
}
