//! IEC LOAD probe (observability scaffold for the iec-serial milestone).
//!
//! Boots the full C64, runs to BASIC ready, mounts scramble_infinity.d64, injects
//! `LOAD"*",8` + RETURN into the keyboard buffer, and runs while sampling the C64
//! PC, the IEC line state, the drive PC, and the KERNAL status byte ST ($90).
//!
//! This is a DIAGNOSTIC test (run with `--nocapture`), not a gate. It exists to
//! locate the precise per-bit divergence in the IEC serial byte transfer.

use std::path::Path;
use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
const SAMPLE: &str =
    "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/scramble_infinity.d64";

fn roms_present() -> bool {
    let d = Path::new(ROM_DIR);
    d.join("kernal-901227-03.bin").exists()
        && (d.join("dos1541-325302-01+901229-05.bin").exists() || d.join("1541.bin").exists())
}

/// Inject a PETSCII string into the C64 keyboard buffer ($0277..) and set the
/// pending-key count ($00C6). The KERNAL main loop drains it as if typed.
fn inject_keys(m: &mut Machine, s: &[u8]) {
    for (i, b) in s.iter().enumerate() {
        m.poke(0x0277 + i as u16, &[*b]);
    }
    m.poke(0x00c6, &[s.len() as u8]);
}

#[test]
#[ignore = "diagnostic probe for the IEC/GCR LOAD path; run explicitly with --ignored"]
fn iec_load_probe() {
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

    // Run to BASIC "READY." (the editor main loop). ~2M cycles is plenty.
    let mut sink = NullSink;
    m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});

    // Mount the disk now (full machine: drive8 already booted to idle).
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: d64.clone(),
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    // Let the drive settle after attach.
    m.run_for_full(500_000, &mut sink, |_, _, _, _, _, _, _| {});

    // Inject LOAD"*",8 + RETURN  →  L O A D " * " , 8 RETURN
    inject_keys(&mut m, b"LOAD\"*\",8\r");

    // Run with instrumentation. Sample at instruction boundaries via a PC
    // histogram of the most-visited PCs, plus the IEC line snapshot.
    use std::collections::HashMap;
    let mut pc_hist: HashMap<u16, u64> = HashMap::new();
    let mut drive_pc_hist: HashMap<u16, u64> = HashMap::new();

    // We need per-instruction sampling. run_for_full samples drive PCs via the
    // callback; for C64 PC we step in small budget chunks.
    let chunk = 20_000u64;
    let mut total = 0u64;
    let budget = 30_000_000u64;
    let mut last_st = 0u8;
    let mut st_changes: Vec<(u64, u8)> = Vec::new();
    let mut eea9_seen = false;

    while total < budget {
        m.run_for_full(chunk, &mut sink, |pc, _, _, _, _, _, _| {
            *drive_pc_hist.entry(pc).or_insert(0) += 1;
        });
        total += chunk;
        let pc = m.cpu6510.reg_pc;
        *pc_hist.entry(pc).or_insert(0) += 1;
        if pc == 0xEEA9 || (0xEE00..=0xEEC0).contains(&pc) {
            eea9_seen = true;
        }
        let st = m.read_full(0x0090);
        if st != last_st {
            st_changes.push((total, st));
            last_st = st;
        }
    }

    // Report top C64 PCs.
    let mut top: Vec<_> = pc_hist.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("== Top C64 boundary PCs (chunk samples) ==");
    for (pc, n) in top.iter().take(15) {
        eprintln!("  ${:04X}: {}", pc, n);
    }
    let mut dtop: Vec<_> = drive_pc_hist.iter().collect();
    dtop.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("== Top drive PCs ==");
    for (pc, n) in dtop.iter().take(15) {
        eprintln!("  ${:04X}: {}", pc, n);
    }
    eprintln!("== ST ($90) changes (cycle, st) ==");
    for (c, st) in &st_changes {
        eprintln!("  @{}: ${:02X}", c, st);
    }
    eprintln!("eea9_seen={}", eea9_seen);

    // Final state: program landing area $0801.. and pointers.
    let txttab = m.read_full(0x002B) as u16 | ((m.read_full(0x002C) as u16) << 8);
    let vartab = m.read_full(0x002D) as u16 | ((m.read_full(0x002E) as u16) << 8);
    eprintln!("TXTTAB=${:04X} VARTAB(load-end)=${:04X}", txttab, vartab);
    eprintln!(
        "first 16 bytes @ $0801: {:02X?}",
        (0x0801..0x0811).map(|a| m.read_full(a)).collect::<Vec<_>>()
    );
    eprintln!("final ST=${:02X}", m.read_full(0x0090));
}

/// Fine-grained instruction trace of the FIRST LOAD attempt. Steps one C64
/// instruction at a time and logs the IEC line state + drive PC whenever the
/// C64 PC is in the serial-protocol routines ($ED00-$EF00). Bounded window so
/// we can see the exact handshake sequence and where it stalls.
#[test]
#[ignore = "diagnostic probe for the IEC/GCR LOAD path; run explicitly with --ignored"]
fn iec_load_trace() {
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
    let mut sink = NullSink;
    m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: d64.clone(),
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    m.run_for_full(500_000, &mut sink, |_, _, _, _, _, _, _| {});
    inject_keys(&mut m, b"LOAD\"*\",8\r");

    // Step one instruction at a time. Log transitions of the C64 PC region of
    // interest + IEC lines + drive PC. Stop after a bounded number of logged
    // serial-protocol events or when ST=$42 appears.
    let mut logged = 0u64;
    let mut last_drive_pc = 0u16;
    let mut prev_line = (0xFFu8, 0xFFu8); // (cpu_port, drv_port)
    let mut steps = 0u64;
    let max_steps = 6_000_000u64;
    let mut in_serial_since = false;
    while steps < max_steps {
        m.run_for_full(1, &mut sink, |pc, _, _, _, _, _, _| {
            last_drive_pc = pc;
        });
        steps += 1;
        let pc = m.cpu6510.reg_pc;
        let serial = (0xED00..=0xEFFF).contains(&pc);
        if serial {
            let cp = m.iec.cpu_port;
            let dp = m.iec.drv_port;
            let line = (cp, dp);
            // Log only when entering serial routines OR the line/dpc changed.
            if !in_serial_since || line != prev_line {
                let atn = if cp & 0x10 != 0 { 'r' } else { 'A' }; // from cpu_bus actually
                let clk = if cp & 0x40 != 0 { 'r' } else { 'C' };
                let dat = if cp & 0x80 != 0 { 'r' } else { 'D' };
                let dclk = m.drive8.via1_pb_iec_output();
                eprintln!(
                    "c64=${:04X} cpu_port={:02X}[clk={} dat={} atn={}] drv_port={:02X} drvPB={:02X} drvPC=${:04X} clk={}",
                    pc, cp, clk, dat, atn, dp, dclk, last_drive_pc, m.cpu6510.clk
                );
                logged += 1;
                prev_line = line;
            }
            in_serial_since = true;
        } else {
            in_serial_since = false;
        }
        if m.read_full(0x0090) == 0x42 {
            eprintln!("ST=$42 reached at step {} clk={}", steps, m.cpu6510.clk);
            break;
        }
        if logged > 400 {
            eprintln!("(log cap reached)");
            break;
        }
    }
    eprintln!("done: steps={} logged={}", steps, logged);
}

/// Trace the DRIVE-side PC stream in the window right after the C64 sends TALK,
/// to see whether the drive enters its file-send routine or stalls in command
/// processing. Logs every distinct drive PC with its clock.
#[test]
#[ignore = "diagnostic probe for the IEC/GCR LOAD path; run explicitly with --ignored"]
fn iec_drive_talk_trace() {
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
    let mut sink = NullSink;
    m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: d64.clone(),
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    m.run_for_full(500_000, &mut sink, |_, _, _, _, _, _, _| {});
    inject_keys(&mut m, b"LOAD\"*\",8\r");

    // Run up to ~clk where TALK is sent (~+540k from here), capturing every
    // distinct drive PC after the C64 reaches $ED09 (TALK). We log distinct
    // drive PCs (deduped) along with the count of how many times each appears,
    // plus the running sequence (capped).
    use std::collections::HashMap;
    let mut seen_talk = false;
    let mut seq: Vec<(u16, u64)> = Vec::new();
    let mut counts: HashMap<u16, u64> = HashMap::new();
    let mut steps = 0u64;
    let max_steps = 8_000_000u64;
    while steps < max_steps {
        let mut last_dpc = 0u16;
        m.run_for_full(1, &mut sink, |pc, _, _, _, _, _, _| {
            *counts.entry(pc).or_insert(0) += 1;
            last_dpc = pc;
            if seen_talk && seq.len() < 800 {
                // record distinct consecutive
                if seq.last().map(|x| x.0) != Some(pc) {
                    seq.push((pc, 0));
                }
            }
        });
        steps += 1;
        let _ = last_dpc;
        let pc = m.cpu6510.reg_pc;
        if pc == 0xED09 {
            seen_talk = true;
        }
        if m.read_full(0x0090) == 0x42 {
            eprintln!("ST=$42 at clk={}", m.cpu6510.clk);
            break;
        }
        if seq.len() >= 700 {
            break;
        }
    }
    eprintln!("== drive PC sequence after TALK (distinct consecutive) ==");
    for (pc, _) in seq.iter().take(200) {
        eprint!("{:04X} ", pc);
    }
    eprintln!();
    let mut top: Vec<_> = counts.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("== drive PC counts (top 25) ==");
    for (pc, n) in top.iter().take(25) {
        eprintln!("  ${:04X}: {}", pc, n);
    }
}

/// Detect whether the drive EVER reaches its TALK send-byte routine ($E909
/// region) during a long run, and if so, capture the C64+drive bit-handshake
/// around the first send. Reports the max drive PC region reached.
#[test]
#[ignore = "diagnostic probe for the IEC/GCR LOAD path; run explicitly with --ignored"]
fn iec_drive_send_reached() {
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
    let mut sink = NullSink;
    m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: d64.clone(),
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    m.run_for_full(500_000, &mut sink, |_, _, _, _, _, _, _| {});
    inject_keys(&mut m, b"LOAD\"*\",8\r");

    let mut send_hits = 0u64; // drive in $E909-$E999
    let mut sendloop_hits = 0u64; // drive in $E95C-$E985 (bit loop)
    let mut first_send_clk = 0u64;
    let mut steps = 0u64;
    let max_steps = 12_000_000u64;
    let mut st42_clk = 0u64;
    while steps < max_steps {
        m.run_for_full(1, &mut sink, |pc, _, _, _, _, _, _| {
            if (0xE909..=0xE999).contains(&pc) {
                send_hits += 1;
                if first_send_clk == 0 {
                    first_send_clk = 1;
                }
            }
            if (0xE95C..=0xE985).contains(&pc) {
                sendloop_hits += 1;
            }
        });
        steps += 1;
        if first_send_clk == 1 {
            first_send_clk = m.cpu6510.clk;
        }
        if m.read_full(0x0090) == 0x42 && st42_clk == 0 {
            st42_clk = m.cpu6510.clk;
            break;
        }
    }
    eprintln!(
        "send_hits(E909-E999)={} sendloop_hits(E95C-E985)={} first_send_clk={} st42_clk={} final_c64clk={}",
        send_hits, sendloop_hits, first_send_clk, st42_clk, m.cpu6510.clk
    );
}

/// Profile the drive during the "lost" window (after the C64 sends TALK, while
/// it waits ~7M cycles for the drive to start sending). Buckets drive PCs by
/// 256-byte page to see where the drive spends the 7M cycles.
#[test]
#[ignore = "diagnostic probe for the IEC/GCR LOAD path; run explicitly with --ignored"]
fn iec_drive_lost_window() {
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
    let mut sink = NullSink;
    m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: d64.clone(),
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    m.run_for_full(500_000, &mut sink, |_, _, _, _, _, _, _| {});
    inject_keys(&mut m, b"LOAD\"*\",8\r");

    use std::collections::HashMap;
    let mut page_counts: HashMap<u16, u64> = HashMap::new();
    let mut fine: HashMap<u16, u64> = HashMap::new();
    let mut steps = 0u64;
    // Run until well past the TALK send (~3M) into the lost window.
    let start_clk = m.cpu6510.clk;
    while m.cpu6510.clk - start_clk < 9_000_000 {
        m.run_for_full(2000, &mut sink, |pc, _, _, _, _, _, _| {
            *page_counts.entry(pc >> 8).or_insert(0) += 1;
            *fine.entry(pc).or_insert(0) += 1;
        });
        steps += 1;
        let _ = steps;
    }
    let mut pages: Vec<_> = page_counts.iter().collect();
    pages.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("== drive PC page histogram (whole load window) ==");
    for (pg, n) in pages.iter().take(20) {
        eprintln!("  ${:02X}xx: {}", pg, n);
    }
    let mut f: Vec<_> = fine.iter().collect();
    f.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("== top fine drive PCs ==");
    for (pc, n) in f.iter().take(30) {
        eprintln!("  ${:04X}: {}", pc, n);
    }
    // Drive job queue state (buffer 0 job code + track/sector).
    eprintln!(
        "drive job: $00={:02X} $06(trk)={:02X} $07(sec)={:02X} half_track={}",
        m.drive8.drive_ram_read(0x00),
        m.drive8.drive_ram_read(0x06),
        m.drive8.drive_ram_read(0x07),
        m.drive8.rotation.current_half_track
    );
    // The command buffer ($0200+) holds the received filename. If the IEC LISTEN
    // path worked, "*" (PETSCII $2A) should be there.
    let cmdbuf: Vec<u8> = (0x0200..0x0210).map(|a| m.drive8.drive_ram_read(a)).collect();
    eprintln!("drive cmd buffer $0200: {:02X?}", cmdbuf);
}
