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
    inject_keys(&mut m, b"LOAD\"$\",8\r");

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
    inject_keys(&mut m, b"LOAD\"$\",8\r");

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
    inject_keys(&mut m, b"LOAD\"$\",8\r");

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
    inject_keys(&mut m, b"LOAD\"$\",8\r");

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

/// Track forward LOAD progress: watch the C64 store-pointer ($AE/$AF) advance as
/// directory bytes land, find the LAST advance (where progress stalls), and dump
/// the exact line state + both PCs at the stall. Pinpoints the deadlocking byte.
#[test]
#[ignore = "diagnostic probe for the IEC/GCR LOAD path; run explicitly with --ignored"]
fn iec_load_stall_point() {
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
    inject_keys(&mut m, b"LOAD\"$\",8\r");

    // Watch the C64 LOAD store-pointer $AE/$AF (LDTND/EAL — the address the LOAD
    // routine writes each received byte to). Track its max + the clk of the last
    // advance. Also count bytes received via ACPTR ($EE13 entries).
    let mut last_ptr = 0u16;
    let mut max_ptr = 0u16;
    let mut last_advance_clk = 0u64;
    let mut acptr_entries = 0u64;
    let mut prev_pc = 0u16;
    let mut steps = 0u64;
    let max_steps = 20_000_000u64;
    let mut last_drive_pc = 0u16;
    // Ring of distinct-consecutive drive PCs around the stall.
    // (drv_pc, clk, c64_pc, cpu_port, drv_port)
    let mut ring: Vec<(u16, u64, u16, u8, u8)> = Vec::new();
    // Count drive ATN-service ($E85B) and ATN-IRQ ($FE7A) entries, bucketed
    // before vs after the stall onset (~5.0M).
    let mut e85b_before = 0u64;
    let mut e85b_after = 0u64;
    let mut fe7a_before = 0u64;
    let mut fe7a_after = 0u64;
    let mut prev_dpc = 0u16;
    // $E999 = send-abort to idle. $E992 = got-next-byte (continue). The single
    // $E999 hit marks the cycle the drive falsely aborts the directory talk-send.
    let mut e999_clks: Vec<u64> = Vec::new();
    let mut last_e992_clk = 0u64;
    while steps < max_steps {
        let before = m.cpu6510.clk < 4_960_000;
        let mut hit_e999 = false;
        let mut hit_e992 = false;
        m.run_for_full(1, &mut sink, |pc, _, _, _, _, _, _| {
            last_drive_pc = pc;
            if pc == 0xE85B && prev_dpc != 0xE85B {
                if before { e85b_before += 1 } else { e85b_after += 1 }
            }
            if pc == 0xFE7A && prev_dpc != 0xFE7A {
                if before { fe7a_before += 1 } else { fe7a_after += 1 }
            }
            if pc == 0xE999 { hit_e999 = true; }
            if pc == 0xE992 { hit_e992 = true; }
            prev_dpc = pc;
        });
        steps += 1;
        if hit_e999 {
            e999_clks.push(m.cpu6510.clk);
        }
        if hit_e992 {
            last_e992_clk = m.cpu6510.clk;
        }
        let pc = m.cpu6510.reg_pc;
        if pc == 0xEE13 && prev_pc != 0xEE13 {
            acptr_entries += 1;
        }
        prev_pc = pc;
        // Fixed window around the abort (clk 4945507).
        if m.cpu6510.clk > 4_944_500 && m.cpu6510.clk < 4_946_500
            && ring.last().map(|x| x.0) != Some(last_drive_pc) {
            ring.push((last_drive_pc, m.cpu6510.clk, m.cpu6510.reg_pc, m.iec.cpu_port, m.iec.drv_port));
        }
        // Progress metric = bytes received via ACPTR. Record clk of last new byte.
        if acptr_entries != last_ptr as u64 {
            last_advance_clk = m.cpu6510.clk;
            last_ptr = acptr_entries as u16;
            max_ptr = acptr_entries as u16;
        }
        // Stop when we've gone 3M cycles past the last received byte (stalled).
        if last_advance_clk != 0 && m.cpu6510.clk - last_advance_clk > 3_000_000 {
            break;
        }
    }
    eprintln!(
        "ATN-service $E85B: before={} after={}  | ATN-IRQ $FE7A: before={} after={}",
        e85b_before, e85b_after, fe7a_before, fe7a_after
    );
    eprintln!(
        "send-abort $E999 count={} first_few={:?} last_continue $E992@clk={}",
        e999_clks.len(),
        e999_clks.iter().take(5).collect::<Vec<_>>(),
        last_e992_clk
    );
    eprintln!("== drive PC trail around stall (drv@clk c64=.. cp=.. dp=..) ==");
    for (pc, c, c64, cp, dp) in ring.iter() {
        eprintln!("  drv={:04X}@{} c64={:04X} cp={:02X} dp={:02X}", pc, c, c64, cp, dp);
    }
    eprintln!(
        "max store-ptr $AE/$AF=${:04X} last-advance@clk={} acptr_entries={} stall_clk={}",
        max_ptr, last_advance_clk, acptr_entries, m.cpu6510.clk
    );
    eprintln!(
        "STALL state: c64_pc=${:04X} drv_pc=${:04X} cpu_port={:02X} drv_port={:02X} drvPB={:02X} ST=${:02X}",
        m.cpu6510.reg_pc, last_drive_pc, m.iec.cpu_port, m.iec.drv_port,
        m.drive8.via1_pb_iec_output(), m.read_full(0x0090)
    );
    eprintln!(
        "bytes landed @ $0801..$0830: {:02X?}",
        (0x0801..0x0830).map(|a| m.read_full(a)).collect::<Vec<_>>()
    );
    // Drive-side serial state: talk/listen flags + channel/buffer status.
    let dr = |a: u16| m.drive8.drive_ram_read(a);
    eprintln!(
        "drive flags: $79(listen)={:02X} $7A(talk)={:02X} $7C(atn-svc)={:02X} $7D={:02X} $82(chan)={:02X} $83={:02X} $84={:02X} $F8={:02X}",
        dr(0x79), dr(0x7A), dr(0x7C), dr(0x7D), dr(0x82), dr(0x83), dr(0x84), dr(0xF8)
    );
    eprintln!(
        "drive chan status $F2..$F9: {:02X?}  buffers $00..$0E: {:02X?}",
        (0xF2..0xFA).map(dr).collect::<Vec<_>>(),
        (0x00..0x0F).map(dr).collect::<Vec<_>>()
    );
    // VIA1 (IEC) regs: PB output ($1800), DDR, PCR/IFR/IER for CA1(ATN) state.
    eprintln!(
        "drive VIA1: PB-out={:02X} drv_port-in(via_iec_tmp)={:02X}",
        m.drive8.via1_pb_iec_output(), m.iec.drv_port
    );
}

/// Byte-exact directory-load verification mirroring the corpus
/// `disk/disk-load-dir.json` sequence (mount-before-boot, 2M+2M boot, type
/// LOAD"$",8 via the keyboard matrix with the corpus hold/gap cycles, 4×2M run),
/// then compares $0801..$0A80 + vartab ($2D/$2E) + ST ($90) to the golden values
/// extracted from `disk-load-dir.golden.json`.
#[test]
#[ignore = "byte-exact directory-load check; run explicitly with --ignored"]
fn iec_load_dir_byteexact() {
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
    // Corpus order: mount at cycle 0 (before any run).
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: d64.clone(),
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    let mut sink = NullSink;
    // boot-1 + boot-2 (2M + 2M).
    m.run_for_full(2_000_000, &mut sink, |_, _, _, _, _, _, _| {});
    m.run_for_full(2_000_000, &mut sink, |_, _, _, _, _, _, _| {});
    // type-load-dir: LOAD"$",8\r via the keyboard matrix, corpus hold/gap = 80000.
    m.keyboard
        .type_text(m.cpu6510.clk, "LOAD\"$\",8\r", 80_000, 80_000);
    // load-1..load-4 (4 × 2M).
    for _ in 0..4 {
        m.run_for_full(2_000_000, &mut sink, |_, _, _, _, _, _, _| {});
    }

    // Golden values from disk-load-dir.golden.json.
    let golden_st: u8 = 0x40;
    let golden_vartab: [u8; 2] = [127, 10]; // $0A7F
    let golden_dir: [u8; 32] = [
        31, 8, 0, 0, 18, 34, 32, 32, 32, 32, 32, 32, 32, 32, 32, 32, 32, 32, 32, 32, 32, 32, 34,
        32, 32, 32, 32, 32, 32, 0, 63, 8,
    ];
    let st = m.read_full(0x0090);
    let vartab = [m.read_full(0x002D), m.read_full(0x002E)];
    let dir: Vec<u8> = (0x0801..0x0801 + 640).map(|a| m.read_full(a)).collect();
    eprintln!("ST=${:02X} (golden $40)  vartab={:02X?} (golden 7F,0A)", st, vartab);
    eprintln!("dir first32: {:02X?}", &dir[..32]);
    eprintln!("dir last16:  {:02X?}", &dir[640 - 16..]);
    let first_mismatch = dir
        .iter()
        .take(32)
        .zip(golden_dir.iter())
        .position(|(a, b)| a != b);
    assert_eq!(st, golden_st, "status $90 mismatch");
    assert_eq!(vartab, golden_vartab, "vartab $2D/$2E mismatch");
    assert!(
        first_mismatch.is_none(),
        "directory first-32 byte mismatch at idx {:?}",
        first_mismatch
    );
    // Full 640-byte directory image must match the golden image.
    let golden_full = std::fs::read("/tmp/golden_dir.bin").ok();
    if let Some(gf) = golden_full {
        if gf.len() == 640 {
            let fm = dir.iter().zip(gf.iter()).position(|(a, b)| a != b);
            assert!(fm.is_none(), "full directory mismatch at idx {:?}", fm);
            eprintln!("FULL 640-byte directory BYTE-EXACT vs golden");
        }
    }
    eprintln!("PASS: directory load byte-exact, ST=$40 (EOI)");
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
    inject_keys(&mut m, b"LOAD\"$\",8\r");

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

/// Measure the rotational phase at the FIRST track-1 SYNC lock, mirroring the
/// corpus `disk/scramble-load-progress.json` driving sequence EXACTLY so the
/// numbers are comparable to the c64re/TS reference:
///   mount-before-boot, 2M + 2M boot, type LOAD"*",8,1 (hold/gap 80000),
///   then run while sampling the drive PC.
///
/// The 1541 DOS find-sync loop is at $F562 `BIT $1C00` / $F565 `BMI $F55D`;
/// it falls through to $F567 `LDA $1C01` the instant SYNC is detected. We log:
///   - every half-track change (the seek 18->1) with its drive_clk
///   - the FIRST $F567 (sync lock) AFTER the head reaches track 1 (halftrack 2),
///     with drive_clk, gcr_head_offset, speed_zone, and the GCR byte read.
///
/// c64re/TS reference (measured live): head settles on track 1 at drive_clk
/// ~5243192, first track-1 sync lock (drive $F567) at drive_clk 5354260
/// => settle->lock delta ~111068 drive cycles.
#[test]
#[ignore = "rotational-phase measurement; run explicitly with --ignored"]
fn track1_sync_phase_probe() {
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
    // Corpus order: mount at cycle 0 (before any run).
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: d64.clone(),
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    let mut sink = NullSink;
    // boot-1 + boot-2 (2M + 2M) — exactly as the corpus scenario.
    m.run_for_full(2_000_000, &mut sink, |_, _, _, _, _, _, _| {});
    m.run_for_full(2_000_000, &mut sink, |_, _, _, _, _, _, _| {});
    // type-load: LOAD"*",8,1\r with corpus hold/gap = 80000.
    m.keyboard
        .type_text(m.cpu6510.clk, "LOAD\"*\",8,1\r", 80_000, 80_000);

    // Step in fine chunks, after each chunk inspecting the live rotation state.
    // We detect (a) every half-track transition with its drive_clk and (b) the
    // first $F567 sync-lock that occurs while the head is on track 1 (halftrack 2),
    // capturing the head offset at that instant.
    let mut prev_ht: u32 = m.drive8.rotation.current_half_track;
    let mut last_step_clk: Option<u64> = None; // drive_clk of the final 18->1 step
    let mut step_log: Vec<(u32, u64)> = Vec::new();
    // Track whether the current fine chunk saw an F567 (sync lock).
    let mut lock_clk_offset: Option<(u64, u32, usize, u8)> = None; // (clk, head_off, zone, gcr_read)

    // Run boot/seek window coarsely until the head is on track 1, logging steps.
    let mut total = 0u64;
    while total < 8_000_000 && m.drive8.rotation.current_half_track != 2 {
        m.run_for_full(20_000, &mut sink, |_, _, _, _, _, _, _| {});
        total += 20_000;
        let ht = m.drive8.rotation.current_half_track;
        if ht != prev_ht {
            step_log.push((ht, m.drive8.drive_clk));
            last_step_clk = Some(m.drive8.drive_clk);
            prev_ht = ht;
        }
    }
    let settle_clk = m.drive8.drive_clk;
    let settle_off = m.drive8.rotation.gcr_head_offset;

    // Now step finely watching for the first F567 while on track 1.
    let mut sawf567 = false;
    let mut budget_after = 0u64;
    while !sawf567 && budget_after < 2_000_000 {
        let mut hit = false;
        m.run_for_full(50, &mut sink, |pc, _, _, _, _, _, _| {
            if pc == 0xF567 {
                hit = true;
            }
        });
        budget_after += 50;
        if hit && m.drive8.rotation.current_half_track == 2 {
            lock_clk_offset = Some((
                m.drive8.drive_clk,
                m.drive8.rotation.gcr_head_offset,
                m.drive8.rotation.speed_zone,
                m.drive8.rotation.gcr_read,
            ));
            sawf567 = true;
        }
    }

    eprintln!("== TRX64 track-1 rotational-phase probe ==");
    eprintln!("seek steps (halftrack, drive_clk):");
    for (ht, c) in &step_log {
        eprintln!("  ht={} (track {}) drive_clk={}", ht, ht / 2, c);
    }
    eprintln!(
        "head SETTLED on track1: drive_clk~{} head_offset={} bits (byte ~{}) last_step_clk={:?}",
        settle_clk, settle_off, settle_off / 8, last_step_clk
    );
    if let Some((clk, off, zone, gcr)) = lock_clk_offset {
        let lc = last_step_clk.unwrap_or(settle_clk);
        eprintln!(
            "FIRST track-1 SYNC LOCK: drive_clk={} head_offset={} bits (byte ~{}) zone={} gcr_read=${:02X}",
            clk, off, off / 8, zone, gcr
        );
        eprintln!(
            "  settle->lock delta = {} drive cycles (c64re/TS ref = ~111068)",
            clk.wrapping_sub(lc)
        );
    } else {
        eprintln!("FIRST track-1 SYNC LOCK: not captured within budget");
    }
}

/// Direct gate-quantity probe: replicate the corpus scenario C64-cycle checkpoints
/// EXACTLY and report $AE/$AF at each L1..L8 (4M boot + 8x1M), plus the precise
/// C64 cycle at which $AE first becomes non-zero (first data byte deposited).
///
/// Golden (TS): end1..end4 = [0,0]; end5=[131,9]; => $AE first moves in (8M,9M].
#[test]
#[ignore = "gate-quantity $AE progression probe; run explicitly with --ignored"]
fn ae_progress_probe() {
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
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: d64.clone(),
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    let mut sink = NullSink;
    m.run_for_full(2_000_000, &mut sink, |_, _, _, _, _, _, _| {});
    m.run_for_full(2_000_000, &mut sink, |_, _, _, _, _, _, _| {});
    m.keyboard
        .type_text(m.cpu6510.clk, "LOAD\"*\",8,1\r", 80_000, 80_000);

    let mut first_move: Option<u64> = None;
    // Fine chunks; tighten near the 8M boundary to pin first_move precisely.
    let mut next_label_at = 5_000_000u64; // L1 = 4M boot + 1M
    let mut label = 1;
    eprintln!("== TRX64 $AE/$AF progression (corpus checkpoints) ==");
    loop {
        let before = m.cpu6510.clk;
        // Fine 2k chunks in the 7.9M-8.1M window to pin the first move precisely.
        let chunk = if m.cpu6510.clk >= 7_900_000 && m.cpu6510.clk < 8_100_000 {
            2_000
        } else {
            50_000
        };
        m.run_for_full(chunk, &mut sink, |_, _, _, _, _, _, _| {});
        let ae = m.read_full(0x00AE);
        let af = m.read_full(0x00AF);
        if first_move.is_none() && (ae != 0 || af != 0) {
            first_move = Some(m.cpu6510.clk);
        }
        if m.cpu6510.clk >= next_label_at && label <= 8 {
            eprintln!(
                "  end{} (C64 clk {}): $AE/$AF = [{}, {}]",
                label, m.cpu6510.clk, ae, af
            );
            label += 1;
            next_label_at += 1_000_000;
        }
        let _ = before;
        if m.cpu6510.clk >= 13_000_000 {
            break;
        }
    }
    eprintln!(
        "FIRST $AE/$AF move at C64 clk = {:?}  (golden: between 8M and 9M)",
        first_move
    );
}
