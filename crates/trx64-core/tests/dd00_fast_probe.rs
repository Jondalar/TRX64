//! dd00_fast_probe.rs — diagnose the scramble custom 2-bit $DD00 fast transfer.
//!
//! Boots, mounts scramble, LOAD"*",8,1, RUN, then captures every C64 $DD00 READ
//! (value + sampled CLK/DATA bits + C64 clk + PC) and the drive's VIA1 PB output
//! during the fast-transfer window, so the sampled bits can be diffed byte-by-byte
//! against the c64re reference (traces/scramble_run_ref.duckdb).
//!
//! Run with:
//!   cargo test -p trx64-core --test dd00_fast_probe -- --ignored --nocapture

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

/// Captures $DD00 reads. We only log within an arming window to keep it bounded.
#[derive(Default)]
struct Dd00Sink {
    armed: bool,
    // (clk, pc, value)
    reads: Vec<(u64, u16, u8)>,
    tags: Vec<u32>,
    max: usize,
    cap_instr: bool,
    cap_target: usize,
    instrs: Vec<(u16, u8, u8, u8)>,
}

impl Observer for Dd00Sink {
    fn on_instruction(
        &mut self,
        pc: u16,
        _op: u8,
        _b1: u8,
        _b2: u8,
        a: u8,
        x: u8,
        y: u8,
        _sp: u8,
        _p: u8,
        _clk: u64,
    ) {
        if self.cap_instr && self.instrs.len() < self.cap_target {
            self.instrs.push((pc, a, x, y));
        }
    }
    #[inline]
    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, pc: u16, clk: u64, _old: u8) {
        if !self.armed {
            return;
        }
        // Capture $DD00 reads from the KERNAL serial debounce loop ($EEA9/$EEAC) and
        // its callers ($EExx). These are the cross-domain reads that diverge.
        let is_dd = addr == 0xdd00;
        let is_r = matches!(kind, BusKind::Read);
        // TRX64 divergence is near clk ~25895938 (ref 36544945 − offset 10649007).
        if is_dd
            && is_r
            && (0xEE00..=0xEEC0).contains(&pc)
            && (25_894_000..=25_898_000).contains(&clk)
        {
            self.reads.push((clk, pc, value));
            self.tags.push(0);
        }
    }
    fn on_interrupt(&mut self, _vector: u16, _clk: u64) {}
}

#[test]
#[ignore = "scramble fast-transfer $DD00 probe; run explicitly with --ignored --nocapture"]
fn dd00_fast_probe() {
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
    let mut sink = Dd00Sink {
        armed: false,
        reads: Vec::new(),
        tags: Vec::new(),
        max: 4000,
        cap_instr: false,
        cap_target: 4000,
        instrs: Vec::new(),
    };

    // Boot to BASIC ready.
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
    eprintln!(
        "$0801..$0811: {:02X?}",
        (0x0801..0x0811).map(|a| m.read_full(a)).collect::<Vec<_>>()
    );
    if !load_done {
        eprintln!("LOAD did not finish; abort");
        return;
    }

    // Type RUN, run until the C64 first reaches the loader entry $080D, then capture
    // the C64 PC+A+X+Y instruction stream for the first ~4000 instructions and dump
    // it so it can be diffed against the c64re reference (which is byte-deterministic
    // until the first wrong transferred byte). The FIRST divergent instruction =
    // the first wrong byte.
    inject_keys(&mut m, b"RUN\r");
    let mut reached = false;
    let run_clk = m.cpu6510.clk;
    // Step to $080D.
    let mut at_080d = false;
    for _ in 0..3_000_000 {
        m.run_for_full(1, &mut sink, |_, _, _, _, _, _, _| {});
        if m.cpu6510.reg_pc == 0x080D {
            at_080d = true;
            break;
        }
    }
    eprintln!("reached $080D={at_080d} clk={}", m.cpu6510.clk);
    if at_080d {
        // Capture a LONG C64 instruction stream (PC only) from $080D so we can diff
        // the full control-flow against the reference and find the first PC-level
        // divergence (the harmless transient-A differences are filtered by matching
        // only the PC stream).
        // Run to ~instr 323000 (just before the first divergence at idx 323841),
        // capturing the C64 instruction stream so we can find the divergence index,
        // AND arm $DD00 capture so we record the KERNAL $EEA9 debounce reads.
        sink.cap_instr = true;
        sink.cap_target = 1_500_000;
        sink.armed = true;
        // Capture the DRIVE PC stream (pc + drive_clk) in the divergence clk window
        // so we can compare the drive phase against the reference drive_pc channel
        // (reference divergence: C64-cycle 36544945, drive at PC $EC12-$EC44).
        let mut drv_trace: Vec<(u16, u64)> = Vec::new();
        let mut guard = 0u64;
        while sink.instrs.len() < sink.cap_target && guard < 12_000_000 {
            m.run_for_full(1, &mut sink, |pc, _a, _x, _y, _sp, _p, dclk| {
                if (25_894_000..=25_896_500).contains(&dclk) && drv_trace.len() < 120 {
                    drv_trace.push((pc, dclk));
                }
            });
            guard += 1;
        }
        sink.cap_instr = false;
        sink.armed = false;
        eprintln!("== TRX64 DRIVE PC stream in divergence window (pc, drive_clk) ==");
        for (pc, dclk) in drv_trace.iter() {
            eprintln!("  drvPC=${pc:04X} drive_clk={dclk}");
        }
        let line: Vec<String> = sink
            .instrs
            .iter()
            .map(|(pc, a, x, y)| format!("{pc:04X}:{a:02X}:{x:02X}:{y:02X}"))
            .collect();
        std::fs::write("/tmp/trx64_full.txt", line.join("\n")).ok();
        eprintln!("wrote {} full records to /tmp/trx64_full.txt", sink.instrs.len());
        // Dump the first $EExx $DD00 debounce reads (the divergence is at the FIRST
        // $EEA9 read that fails to transition). Show all captured.
        eprintln!("== KERNAL $EExx $DD00 reads near clk 25.896M (the divergence) ==");
        for (i, ((clk, pc, val), _t)) in sink.reads.iter().zip(sink.tags.iter()).enumerate() {
            eprintln!("  [{i}] clk={clk} PC=${pc:04X} $DD00=${val:02X}");
        }
        eprintln!("total reads in window: {}", sink.reads.len());
        return; // stop here — this dump is the focus
    }
    // Arm $DD00/$DD02 capture across the WHOLE setup so we see the C64 establishing
    // the IEC handshake before $04E2.
    sink.armed = true;
    // Track the drive PC stream from RUN: record if it ever reaches $07xx, and the
    // FIRST time the drive PC leaves the $07xx region into $00xx (the divergence).
    let mut drv_reached_07xx = false;
    let mut first_07xx_clk = 0u64;
    let mut first_00xx_after_07xx: Option<(u16, u16)> = None;
    let mut last_drv = 0u16;
    // Capture the drive PC trail right around the $07xx → $00xx divergence.
    let mut div_trail: Vec<(u16, u8, u8, u8, u8, u8, u64)> = Vec::new();
    let mut capturing_trail = false;
    // Step ONE C64 instruction at a time. Track the FIRST clk the drive enters $07xx,
    // the LAST clk it is still in $07xx (residency), and count drive instrs in $07xx.
    // ALSO histogram the C64 boundary PC AFTER the drive enters $07xx until the C64
    // reaches $04E2 — this is the "C64 setup before the handshake wait".
    let mut substeps = 0u64;
    let mut last_07xx_clk = 0u64;
    let mut drv_07xx_instrs = 0u64;
    let _ = (&mut capturing_trail, &mut div_trail, &mut last_drv, &mut first_00xx_after_07xx);
    use std::collections::HashMap as HM;
    let mut c64_setup_hist: HM<u16, u64> = HM::new();
    while substeps < 8_000_000 {
        let mut in07 = false;
        m.run_for_full(1, &mut sink, |pc, _a, _x, _y, _sp, _p, _dclk| {
            if (0x0790..=0x07FF).contains(&pc) {
                if !drv_reached_07xx {
                    drv_reached_07xx = true;
                }
                in07 = true;
                drv_07xx_instrs += 1;
            }
        });
        if drv_reached_07xx && first_07xx_clk == 0 {
            first_07xx_clk = m.cpu6510.clk;
        }
        if in07 {
            last_07xx_clk = m.cpu6510.clk;
        }
        if drv_reached_07xx {
            *c64_setup_hist.entry(m.cpu6510.reg_pc).or_insert(0) += 1;
        }
        substeps += 1;
        if m.cpu6510.reg_pc == 0x04E2 {
            reached = true;
            break;
        }
    }
    eprintln!(
        "drive $07xx residency: first_clk={first_07xx_clk} last_clk={last_07xx_clk} span={} instrs_in_07xx={drv_07xx_instrs}",
        last_07xx_clk.saturating_sub(first_07xx_clk)
    );
    let mut ctop: Vec<_> = c64_setup_hist.iter().collect();
    ctop.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("== C64 boundary PCs from drive-$07xx-entry to $04E2 (where the C64 spends its time) ==");
    for (pc, n) in ctop.iter().take(16) {
        eprintln!("  ${:04X}: {}", pc, n);
    }
    sink.armed = false;
    // Dump the C64's $DD00/$DD02 access sequence from RUN to $04E2 (the setup +
    // first part of the handshake). $DD02 writes = DDRA toggles driving CLK/DATA out.
    eprintln!("== C64 $DD00/$DD02 accesses RUN→$04E2 (first 80) ==");
    for (i, ((clk, pc, val), tag)) in sink.reads.iter().zip(sink.tags.iter()).enumerate() {
        if i >= 80 {
            break;
        }
        let is_w = tag & 0x10000 != 0;
        let addr = (tag >> 1) & 0xFFFF;
        eprintln!(
            "  clk={clk} PC=${pc:04X} {} ${addr:04X} = ${val:02X}",
            if is_w { "WR" } else { "rd" }
        );
    }
    sink.reads.clear();
    sink.tags.clear();
    eprintln!(
        "drive reached $07xx={drv_reached_07xx} first_00xx_after_07xx={:04X?}",
        first_00xx_after_07xx
    );
    eprintln!(
        "reached loader $04xx={reached} at PC=${:04X} clk={} (+{} after RUN)",
        m.cpu6510.reg_pc,
        m.cpu6510.clk,
        m.cpu6510.clk - run_clk
    );
    if !reached {
        eprintln!("never reached loader; abort");
        return;
    }

    use std::collections::HashMap;
    // ARM the $DD00 capture and run the transfer for a bounded window.
    sink.armed = true;
    let arm_clk = m.cpu6510.clk;
    let mut drv_hist: HashMap<u16, u64> = HashMap::new();
    let mut budget = 0u64;
    while budget < 800_000 && sink.reads.len() < sink.max {
        m.run_for_full(20_000, &mut sink, |pc, _, _, _, _, _, _| {
            *drv_hist.entry(pc).or_insert(0) += 1;
        });
        budget += 20_000;
    }
    sink.armed = false;

    let mut dtop: Vec<_> = drv_hist.iter().collect();
    dtop.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("== top DRIVE PCs during window ==");
    for (pc, n) in dtop.iter().take(14) {
        eprintln!("  ${:04X}: {}", pc, n);
    }
    eprintln!(
        "drive at window end: PC=${:04X} drive_clk={} PB_out=${:02X} | iec: cpu_port=${:02X} drv_port=${:02X} cpu_bus=${:02X}",
        m.drive8.core.reg_pc,
        m.drive8.drive_clk,
        m.drive8.via1_pb_iec_output(),
        m.iec.cpu_port,
        m.iec.drv_port,
        m.iec.cpu_bus,
    );
    // Drive RAM where the fast-loader transfer code was uploaded ($0700-$0730).
    eprintln!(
        "drive RAM $0700..$0730: {:02X?}",
        (0x0700..0x0730).map(|a| m.drive8.drive_ram_read(a)).collect::<Vec<_>>()
    );
    eprintln!(
        "drive RAM $07A0..$07C0: {:02X?}",
        (0x07A0..0x07C0).map(|a| m.drive8.drive_ram_read(a)).collect::<Vec<_>>()
    );
    eprintln!(
        "drive RAM $0060..$00C0: {:02X?}",
        (0x0060..0x00C0).map(|a| m.drive8.drive_ram_read(a)).collect::<Vec<_>>()
    );
    eprintln!(
        "drive RAM $0790..$07C0: {:02X?}",
        (0x0790..0x07C0).map(|a| m.drive8.drive_ram_read(a)).collect::<Vec<_>>()
    );
    let (ifr, ier, pcr, irqa, irqs) = m.drive8.via1_irq_debug();
    eprintln!(
        "drive VIA1: IFR=${ifr:02X} IER=${ier:02X} PCR=${pcr:02X} irq_active={irqa} irq_stamp={irqs} | iec_old_atn=${:02X}",
        m.iec.iec_old_atn
    );

    eprintln!(
        "captured {} $DD00 reads in window arm_clk={} .. {}",
        sink.reads.len(),
        arm_clk,
        m.cpu6510.clk
    );

    // Summarise: print the first 120 $DD00 reads with the decoded bits.
    // $DD00 bit6 = CLK_IN, bit7 = DATA_IN (the sampled 2 bits). Print transitions
    // where the sampled (bit6,bit7) changes, plus the PC.
    eprintln!("== first 160 $DD00 reads (clk, PC, val, CLKin=bit6, DATAin=bit7) ==");
    let mut last_bits = 0xFFu8;
    let mut shown = 0;
    for (clk, pc, val) in sink.reads.iter() {
        let bits = val & 0xC0;
        let clkin = (val >> 6) & 1;
        let datin = (val >> 7) & 1;
        // Show every read for the first 60, then only transitions.
        if shown < 60 || bits != last_bits {
            eprintln!(
                "  clk={clk} PC=${pc:04X} val=${val:02X} CLKin={clkin} DATAin={datin}",
            );
            shown += 1;
        }
        last_bits = bits;
        if shown > 220 {
            break;
        }
    }

    // Also dump the loader RAM so we can confirm the transfer code matches reference.
    eprintln!(
        "loader $04E0..$0520: {:02X?}",
        (0x04E0..0x0520).map(|a| m.read_full(a)).collect::<Vec<_>>()
    );
    eprintln!(
        "loader $0400..$0420: {:02X?}",
        (0x0400..0x0420).map(|a| m.read_full(a)).collect::<Vec<_>>()
    );
    eprintln!(
        "C64 RAM $77D0..$77F8: {:02X?}",
        (0x77D0..0x77F8).map(|a| m.read_full(a)).collect::<Vec<_>>()
    );
    eprintln!(
        "C64 RAM $40F0..$4110: {:02X?}",
        (0x40F0..0x4110).map(|a| m.read_full(a)).collect::<Vec<_>>()
    );
}
