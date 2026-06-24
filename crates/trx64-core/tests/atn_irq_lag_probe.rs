//! atn_irq_lag_probe.rs — diagnose the scramble custom $DD00 loader desync.
//!
//! Reproduces the c64re reference sequence EXACTLY: boot to BASIC ready, mount
//! scramble, settle, LOAD"*",8,1, run to BASIC ready, verify the loaded program
//! matches the reference, type RUN, then trace the custom-loader handshake (C64 PC
//! + drive PC + IEC lines). The behavioral bar is the SCRAMBLE INFINITY title.
//!
//! Run with:
//!   cargo test -p trx64-core --test atn_irq_lag_probe -- --ignored --nocapture

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

fn inject_keys(m: &mut Machine, s: &[u8]) {
    for (i, b) in s.iter().enumerate() {
        m.poke(0x0277 + i as u16, &[*b]);
    }
    m.poke(0x00c6, &[s.len() as u8]);
}

#[test]
#[ignore = "scramble custom-loader trace; run explicitly with --ignored --nocapture"]
fn scramble_run_trace() {
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

    // Boot to BASIC ready.
    m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});
    // Mount scramble + settle.
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: d64.clone(),
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    m.run_for_full(500_000, &mut sink, |_, _, _, _, _, _, _| {});
    // LOAD"*",8,1 + RETURN.
    inject_keys(&mut m, b"LOAD\"*\",8,1\r");

    // Run the LOAD to completion: watch for BASIC editor idle ($E5CD/$E5D1 loop)
    // with no pending keys. With a `,1` non-relocating load VARTAB is NOT updated,
    // so detect ready by the editor main-loop PC + a settled idle (PC stable in the
    // editor across two checks).
    let mut load_done = false;
    let mut load_done_clk = 0u64;
    let mut ready_streak = 0u32;
    use std::collections::HashMap;
    let mut load_c64_hist: HashMap<u16, u64> = HashMap::new();
    for _ in 0..600 {
        m.run_for_full(50_000, &mut sink, |_, _, _, _, _, _, _| {});
        let pc = m.cpu6510.reg_pc;
        *load_c64_hist.entry(pc).or_insert(0) += 1;
        if (0xE5C0..=0xE5F0).contains(&pc) && m.read_full(0x00c6) == 0 {
            ready_streak += 1;
            if ready_streak >= 3 {
                load_done = true;
                load_done_clk = m.cpu6510.clk;
                break;
            }
        } else {
            ready_streak = 0;
        }
    }
    // Where did the LOAD spend its time?
    let mut ltop: Vec<_> = load_c64_hist.iter().collect();
    ltop.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("== top C64 boundary PCs DURING LOAD ==");
    for (pc, n) in ltop.iter().take(10) {
        eprintln!("  ${:04X}: {}", pc, n);
    }
    let st = m.read_full(0x0090);
    let txttab = m.read_full(0x002B) as u16 | ((m.read_full(0x002C) as u16) << 8);
    let vartab = m.read_full(0x002D) as u16 | ((m.read_full(0x002E) as u16) << 8);
    eprintln!(
        "post-LOAD: done={load_done} clk={load_done_clk} PC=${:04X} ST=${st:02X} TXTTAB=${txttab:04X} VARTAB=${vartab:04X}",
        m.cpu6510.reg_pc
    );
    // The loaded BASIC stub at $0801 (reference: 0B 08 00 00 9E 32 30 36 31 = SYS 2061).
    eprintln!(
        "$0801..$0811: {:02X?}",
        (0x0801..0x0811).map(|a| m.read_full(a)).collect::<Vec<_>>()
    );
    // The loader entry at $080D (reference: 78 A9 34 85 01 = SEI/LDA #$34/STA $01).
    eprintln!(
        "$080D..$0820: {:02X?}",
        (0x080D..0x0820).map(|a| m.read_full(a)).collect::<Vec<_>>()
    );

    if !load_done {
        eprintln!("LOAD did not return to BASIC ready — NOT typing RUN. Stuck in serial.");
        return;
    }

    // Type RUN.
    inject_keys(&mut m, b"RUN\r");

    // Fine trace: the FIRST distinct C64 PCs after RUN, stepping one C64 instruction
    // at a time, until we either reach $080D or hit a serial routine and loop. We log
    // the run-to-program transition ($A7AE BASIC exec, $E000+ KERNAL, $0800+ program).
    {
        let mut seq: Vec<u16> = Vec::new();
        let mut last = 0xFFFFu16;
        let mut steps = 0u64;
        let mut seen_a000 = false; // entered BASIC interpreter (RUN dispatch)
        while steps < 400_000 {
            m.run_for_full(1, &mut sink, |_, _, _, _, _, _, _| {});
            let pc = m.cpu6510.reg_pc;
            if pc != last {
                if (0xA000..0xC000).contains(&pc) {
                    seen_a000 = true;
                }
                // Record program-area + entry to serial + the SYS dispatch.
                if seq.len() < 200
                    && (pc < 0x0800
                        || (0x0800..0x0900).contains(&pc)
                        || (0xA000..0xA900).contains(&pc)
                        || (0xE000..0xF000).contains(&pc))
                {
                    seq.push(pc);
                }
                last = pc;
            }
            steps += 1;
            if (0x080D..=0x0820).contains(&pc) {
                eprintln!("RUN reached loader $080D after {steps} instrs");
                break;
            }
        }
        eprintln!("seen BASIC interp ($A000-$BFFF)={seen_a000}");
        eprintln!("== first distinct C64 PCs after RUN (filtered) ==");
        let s: Vec<String> = seq.iter().map(|p| format!("{:04X}", p)).collect();
        eprintln!("  {}", s.join(" "));

        // Continue tracing past $080D into the custom loader. Capture the distinct
        // PC stream in the $0400-$0900 loader region + the FIRST entry to the $DD00
        // bit-bang loop ($04xx), and the IEC line state once it gets there.
        // Capture the FULL distinct PC stream (all regions) after the copy loop, to
        // see where the loader JMPs and where it hangs. Also snapshot the last drive
        // PC and IEC state at a periodic interval.
        let mut full_seq: Vec<u16> = Vec::new();
        let mut last2 = 0xFFFFu16;
        let mut steps2 = 0u64;
        let mut reached_dd00 = false;
        let mut dd00_clk = 0u64;
        let mut last_drv_pc = 0u16;
        use std::collections::HashMap as HM;
        let mut hang_hist: HM<u16, u64> = HM::new();
        while steps2 < 3_000_000 {
            m.run_for_full(1, &mut sink, |pc, _, _, _, _, _, _| {
                last_drv_pc = pc;
            });
            let pc = m.cpu6510.reg_pc;
            if pc != last2 {
                // Record the distinct stream but COLLAPSE the $0814-$081F copy loop
                // to a single marker so we see what comes AFTER it.
                let collapsed = (0x0814..=0x081F).contains(&pc);
                if full_seq.len() < 160 && !(collapsed && full_seq.last() == Some(&0x0814)) {
                    full_seq.push(if collapsed { 0x0814 } else { pc });
                }
                if !reached_dd00 && (0x0400..=0x04FF).contains(&pc) {
                    reached_dd00 = true;
                    dd00_clk = m.cpu6510.clk;
                }
                last2 = pc;
            }
            // After 1.5M steps, histogram the steady-state hang PCs.
            if steps2 > 1_500_000 {
                *hang_hist.entry(pc).or_insert(0) += 1;
            }
            steps2 += 1;
        }
        eprintln!("reached $04xx DD00 loop={reached_dd00} (clk={dd00_clk})");
        eprintln!("== distinct loader PC stream after $080D (copy loop collapsed to 0814) ==");
        let ls: Vec<String> = full_seq.iter().map(|p| format!("{:04X}", p)).collect();
        eprintln!("  {}", ls.join(" "));
        let mut htop: Vec<_> = hang_hist.iter().collect();
        htop.sort_by(|a, b| b.1.cmp(a.1));
        eprintln!("== steady-state C64 hang PCs ==");
        for (pc, n) in htop.iter().take(10) {
            eprintln!("  ${:04X}: {}", pc, n);
        }
        eprintln!("last drive PC at hang=${:04X}", last_drv_pc);
        eprintln!(
            "loader IEC state: cpu_port=${:02X} drv_port=${:02X} cpu_bus=${:02X} drvPB=${:02X} C64_PC=${:04X}",
            m.iec.iecbus.cpu_port, m.iec.iecbus.drv_port, m.iec.iecbus.cpu_bus, m.drive8.via1_pb_iec_output(), m.cpu6510.reg_pc
        );
        let (ifr, ier, pcr, irq_active, irq_stamp) = m.drive8.via1_irq_debug();
        eprintln!(
            "drive VIA1 at hang: IFR=${ifr:02X} IER=${ier:02X} PCR=${pcr:02X} CA1ctrl(pcr&1)={} irq_active={irq_active} irq_stamp={irq_stamp} | iec.iec_old_atn=${:02X}",
            pcr & 1, m.iec.iec_old_atn
        );
        eprintln!(
            "  IFR&IER&7F=${:02X} (CA1=0x02 set in IFR={})",
            ifr & ier & 0x7f, ifr & 0x02
        );
        eprintln!(
            "drive IntStatus: global_pending=${:08X} nirq={} irq_clk={} pending_int={:?} | drive reg_p=${:02X} (I-flag={}) drive_clk={} drive_PC=${:04X}",
            m.drive8.int.global_pending_int,
            m.drive8.int.nirq,
            m.drive8.int.irq_clk,
            m.drive8.int.pending_int,
            m.drive8.core.reg_p,
            (m.drive8.core.reg_p >> 2) & 1,
            m.drive8.drive_clk,
            m.drive8.core.reg_pc,
        );
        eprintln!(
            "loader RAM $0400..$0410: {:02X?}",
            (0x0400..0x0410).map(|a| m.read_full(a)).collect::<Vec<_>>()
        );

        // Run the loader, rendering snapshots at several time points (the reference
        // renders a CLEAN title ~8M cycles after RUN, so check early + late).
        let mut last_d020 = m.read_full(0xD020);
        let mut d020_changes = 0u64;
        for shot in 0..40 {
            m.run_for_full(1_000_000, &mut sink, |_, _, _, _, _, _, _| {});
            let d020 = m.read_full(0xD020);
            if d020 != last_d020 {
                d020_changes += 1;
                last_d020 = d020;
            }
            // Snapshot frames at +5M, +8M, +12M, +20M after RUN.
            if [5, 8, 12, 20].contains(&(shot + 1)) {
                let (w, h, rgba) = m.render_canvas_rgba();
                let mut dc = std::collections::HashSet::new();
                for px in rgba.chunks(4) {
                    dc.insert((px[0], px[1], px[2]));
                }
                let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
                for px in rgba.chunks(4) {
                    ppm.extend_from_slice(&px[..3]);
                }
                let p = format!("/Users/alex/Development/C64/Tools/TRX64/traces/scramble_trx64_+{}M.ppm", shot + 1);
                std::fs::write(&p, &ppm).ok();
                eprintln!("  snapshot +{}M: PC=${:04X} D011=${:02X} distinct_colors={}", shot + 1, m.cpu6510.reg_pc, m.read_full(0xD011), dc.len());
            }
        }
        eprintln!(
            "after +40M cyc: C64 PC=${:04X} clk={} D020=${:02X} D021=${:02X} D011=${:02X} D016=${:02X} d020_changes={d020_changes}",
            m.cpu6510.reg_pc, m.cpu6510.clk,
            m.read_full(0xD020), m.read_full(0xD021), m.read_full(0xD011), m.read_full(0xD016)
        );
        // Dump the VIC screen-RAM text (default $0400 screen) to spot title chars.
        let vic_bank = m.vic_bank_base();
        let vm = ((m.vic.regs[0x18] >> 4) & 0x0f) as u16; // video matrix base
        let screen = vic_bank.wrapping_add(vm * 0x0400);
        eprintln!("VIC bank=${vic_bank:04X} screen-RAM base=${screen:04X}");
        let scr: Vec<u8> = (0..1000).map(|i| m.read_full(screen.wrapping_add(i))).collect();
        let nonblank = scr.iter().filter(|&&c| c != 0x20 && c != 0x00).count();
        eprintln!("screen non-blank chars: {nonblank}/1000");
        // Render to RGBA and write a PPM (P6) so the image can be inspected.
        let (w, h, rgba) = m.render_canvas_rgba();
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        for px in rgba.chunks(4) {
            ppm.push(px[0]);
            ppm.push(px[1]);
            ppm.push(px[2]);
        }
        let out = "/Users/alex/Development/C64/Tools/TRX64/traces/scramble_trx64.ppm";
        std::fs::write(out, &ppm).expect("write ppm");
        // Distinct non-grey pixels = the title art rendered (not a blank screen).
        let mut distinct_colors = std::collections::HashSet::new();
        for px in rgba.chunks(4) {
            distinct_colors.insert((px[0], px[1], px[2]));
        }
        eprintln!(
            "rendered {w}x{h} → {out}  distinct_colors={} (blank screen ~= 2-3)",
            distinct_colors.len()
        );
        return; // stop here; the loader region is the focus
    }
    #[allow(unreachable_code)]
    {

    // Trace the post-RUN window. Record the C64 PC histogram + drive PC histogram,
    // and the FIRST time the C64 reaches the loader ($080D) and the first $DD00
    // bit-bang loop ($04xx). Also track the loader bar at $05FD.
    let mut c64_hist: HashMap<u16, u64> = HashMap::new();
    let mut drv_hist: HashMap<u16, u64> = HashMap::new();
    let mut reached_080d = false;
    let mut reached_04xx = false;
    let mut first_080d_clk = 0u64;
    let mut max_bar = m.read_full(0x05FD);
    let mut bar_changes: Vec<(u64, u8)> = Vec::new();
    let mut last_bar = max_bar;
    let run_clk = m.cpu6510.clk;

    let chunk = 20_000u64;
    let mut total = 0u64;
    while total < 25_000_000 {
        m.run_for_full(chunk, &mut sink, |pc, _, _, _, _, _, _| {
            *drv_hist.entry(pc).or_insert(0) += 1;
        });
        total += chunk;
        let pc = m.cpu6510.reg_pc;
        *c64_hist.entry(pc).or_insert(0) += 1;
        if !reached_080d && (0x080D..=0x0820).contains(&pc) {
            reached_080d = true;
            first_080d_clk = m.cpu6510.clk;
        }
        if !reached_04xx && (0x0400..=0x04FF).contains(&pc) {
            reached_04xx = true;
        }
        let bar = m.read_full(0x05FD);
        if bar != last_bar {
            bar_changes.push((m.cpu6510.clk - run_clk, bar));
            last_bar = bar;
        }
        if bar > max_bar && bar < 0xF0 {
            max_bar = bar;
        }
    }

    eprintln!(
        "post-RUN: reached $080D={reached_080d} (clk={first_080d_clk}) reached $04xx={reached_04xx}"
    );
    eprintln!("bar post-LOAD=${:02X} MAX=${max_bar:02X} changes={bar_changes:02X?}", m.read_full(0x05FD).min(max_bar));
    let mut ctop: Vec<_> = c64_hist.iter().collect();
    ctop.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("== top C64 boundary PCs post-RUN ==");
    for (pc, n) in ctop.iter().take(14) {
        eprintln!("  ${:04X}: {}", pc, n);
    }
    let mut dtop: Vec<_> = drv_hist.iter().collect();
    dtop.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("== top drive PCs post-RUN ==");
    for (pc, n) in dtop.iter().take(14) {
        eprintln!("  ${:04X}: {}", pc, n);
    }
    eprintln!(
        "final: C64 PC=${:04X} clk={} drive RAM $0400..$0410={:02X?}",
        m.cpu6510.reg_pc, m.cpu6510.clk,
        (0x0400..0x0410).map(|a| m.drive8.drive_ram_read(a)).collect::<Vec<_>>()
    );
    }
}
