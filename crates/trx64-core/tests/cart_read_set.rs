//! Spec 785 C1 — the CART_READ lane: reads SERVED out of cart space, attributed
//! to the bank that was live while they happened.
//!
//! Two layers, mirroring `loader_head_trace.rs` (the disk lane's proof):
//!
//!   1. SYNTHETIC (runs in the gate, ROM-gated skip): a hand-assembled program in
//!      RAM banks an EasyFlash cart, reads, banks away, reads, banks BACK and
//!      reads. Proves the whole chain — `Machine::arm_cart_reads` → the FullBus
//!      `cart_read` hook → `CartMapper::active_bank` → one record per residency —
//!      including that a bank switch really does close a residency, and that
//!      writes to RAM under the banked-in ROM do not fabricate reads.
//!   2. REAL CARTRIDGE (`--ignored`): boot an actual multi-bank cartridge and
//!      report the bank walk it performs. The image is THIRD-PARTY PROPERTY: it is
//!      never committed, never fixtured, and its path comes from the environment
//!      (`TRX64_CART_READ_SET_CRT`), so this file names no title and no user path.
//!
//!   cargo test -p trx64-core --test cart_read_set
//!   TRX64_CART_READ_SET_CRT=/path/to/x.crt \
//!     cargo test -p trx64-core --test cart_read_set real_cart -- --ignored --nocapture

use std::path::Path;
use trx64_core::cart::{CartSlot, MapperType};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");

fn roms_present() -> bool {
    Path::new(ROM_DIR).join("kernal-901227-03.bin").exists()
}

/// A minimal valid CRT: 0x40-byte header + N CHIP packets.
fn build_crt(hw: u16, exrom: u8, game: u8, chips: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   ");
    v.extend_from_slice(&0x40u32.to_be_bytes()); // headerLen
    v.extend_from_slice(&0x0100u16.to_be_bytes()); // version
    v.extend_from_slice(&hw.to_be_bytes()); // hardwareType
    v.push(exrom);
    v.push(game);
    v.extend_from_slice(&[0u8; 6]);
    v.extend_from_slice(&[0u8; 32]); // name
    assert_eq!(v.len(), 0x40);
    for (bank, load, data) in chips {
        v.extend_from_slice(b"CHIP");
        v.extend_from_slice(&(0x10 + data.len() as u32).to_be_bytes());
        v.extend_from_slice(&0u16.to_be_bytes()); // chipType = ROM
        v.extend_from_slice(&bank.to_be_bytes());
        v.extend_from_slice(&load.to_be_bytes());
        v.extend_from_slice(&(data.len() as u16).to_be_bytes());
        v.extend_from_slice(data);
    }
    v
}

// ── 1. SYNTHETIC: the bank walk a hand-written program performs ───────────────

#[test]
fn armed_lane_follows_a_real_easyflash_bank_switch() {
    if !roms_present() {
        eprintln!("SKIP: ROMs absent ({ROM_DIR})");
        return;
    }

    // Three EasyFlash banks, each filled with its own byte so a wrong bank shows
    // up as a wrong VALUE too, not only as a wrong record.
    let chips: Vec<(u16, u16, Vec<u8>)> = [(0u16, 0xa0u8), (3, 0xb3), (7, 0xc7)]
        .iter()
        .map(|&(bank, fill)| (bank, 0x8000u16, vec![fill; 0x2000]))
        .collect();
    let crt = build_crt(32, 1, 0, &chips); // hw 32 = EasyFlash

    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let (_name, ty) = m.attach_cart_from_bytes(&crt, "synthetic").expect("attach CRT");
    assert_eq!(ty, MapperType::EasyFlash);

    // The program runs from $0200 — RAM in every memconfig, ultimax included.
    // $DE02 = 6 leaves the ultimax boot config for 8K game mode, so $8000-$9FFF is
    // ROML and $E000 is the KERNAL again.
    #[rustfmt::skip]
    let prog: &[u8] = &[
        0x78,                     // SEI              — no KERNAL IRQ mid-measurement
        0xa9, 0x06, 0x8d, 0x02, 0xde, // LDA #$06 / STA $DE02  → 8K game mode
        0xad, 0x00, 0x80,         // LDA $8000        — bank 0, offset $0000
        0xa9, 0x03, 0x8d, 0x00, 0xde, // LDA #$03 / STA $DE00  → bank 3
        0xad, 0x00, 0x80,         // LDA $8000        — bank 3, offset $0000
        0xad, 0x23, 0x81,         // LDA $8123        — bank 3, offset $0123
        0x8d, 0x00, 0x90,         // STA $9000        — RAM UNDER the ROM: not a read
        0xa9, 0x07, 0x8d, 0x00, 0xde, // LDA #$07 / STA $DE00  → bank 7
        0xad, 0xff, 0x9f,         // LDA $9FFF        — bank 7, offset $1FFF
        0xa9, 0x00, 0x8d, 0x00, 0xde, // LDA #$00 / STA $DE00  → back to bank 0
        0xad, 0x10, 0x80,         // LDA $8010        — bank 0 AGAIN, offset $0010
        0x4c, 0x2b, 0x02,         // JMP * (spin in RAM — no further cart reads)
    ];
    m.poke(0x0200, prog);
    m.write_full(0x0001, 0x37); // LORAM+HIRAM+CHAREN — the standard config
    m.c64_core.reg_pc = 0x0200;

    m.arm_cart_reads(true);
    let mut sink = NullSink;
    m.run_for_full(2_000, &mut sink, |_, _, _, _, _, _, _| {});
    let recs = m.drain_cart_reads();

    for r in &recs {
        eprintln!(
            "clk {:>6}  bank {:>3}  slot {}  off ${:04X}-${:04X}  bytes {}",
            r.cycle, r.bank, r.slot, r.off_lo, r.off_hi, r.bytes
        );
    }

    // FOUR residencies: 0 → 3 → 7 → 0. Reading a bank, leaving it and coming back
    // is two records, never one merged record.
    let banks: Vec<u16> = recs.iter().map(|r| r.bank).collect();
    assert_eq!(banks, vec![0, 3, 7, 0], "the lane must follow the $DE00 walk exactly");
    assert!(recs.iter().all(|r| r.slot == CartSlot::Roml as u8), "all reads were ROML");

    // Each residency's offsets are the ones the program actually touched. The
    // instruction fetches run from RAM, so the counts are the LDAs alone.
    assert_eq!((recs[0].off_lo, recs[0].off_hi, recs[0].bytes), (0x0000, 0x0000, 1));
    assert_eq!((recs[1].off_lo, recs[1].off_hi, recs[1].bytes), (0x0000, 0x0123, 2));
    assert_eq!((recs[2].off_lo, recs[2].off_hi, recs[2].bytes), (0x1fff, 0x1fff, 1));
    assert_eq!((recs[3].off_lo, recs[3].off_hi, recs[3].bytes), (0x0010, 0x0010, 1));
    // ...and in particular the `STA $9000` did NOT show up: the pre-write old-byte
    // read is instrumentation, not a bus cycle (else bank 3 would report 3 bytes
    // and an off_hi of $1000).
    assert!(recs.iter().all(|r| r.off_hi != 0x1000), "a store must not fabricate a cart read");

    // Cycle-ordered, and the drain emptied the set.
    assert!(recs.windows(2).all(|w| w[0].cycle <= w[1].cycle), "records are cycle-ordered");
    assert!(m.drain_cart_reads().is_empty(), "drained");
}

/// Armed-on-command: a disarmed run accumulates nothing, and arming clears.
#[test]
fn disarmed_lane_never_accumulates() {
    if !roms_present() {
        eprintln!("SKIP: ROMs absent ({ROM_DIR})");
        return;
    }
    let crt = build_crt(32, 1, 0, &[(0u16, 0x8000u16, vec![0xa0u8; 0x2000])]);
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    m.attach_cart_from_bytes(&crt, "synthetic").expect("attach CRT");

    #[rustfmt::skip]
    let prog: &[u8] = &[
        0x78,
        0xa9, 0x06, 0x8d, 0x02, 0xde, // 8K game mode
        0xad, 0x00, 0x80,             // LDA $8000
        0x4c, 0x09, 0x02,             // JMP *
    ];
    m.poke(0x0200, prog);
    m.write_full(0x0001, 0x37);
    m.c64_core.reg_pc = 0x0200;

    let mut sink = NullSink;
    m.run_for_full(2_000, &mut sink, |_, _, _, _, _, _, _| {});
    assert!(m.drain_cart_reads().is_empty(), "disarmed → nothing accumulates");

    // Arming mid-flight clears whatever a previous arm left behind.
    m.arm_cart_reads(true);
    m.c64_core.reg_pc = 0x0200;
    m.run_for_full(2_000, &mut sink, |_, _, _, _, _, _, _| {});
    assert!(!m.drain_cart_reads().is_empty(), "armed → the lane records");
    m.arm_cart_reads(true);
    assert!(m.drain_cart_reads().is_empty(), "arming clears");
}

/// A monitor read of a cart window is not the title reading: it must never enter
/// the read-set.
#[test]
fn monitor_reads_stay_out_of_the_read_set() {
    if !roms_present() {
        eprintln!("SKIP: ROMs absent ({ROM_DIR})");
        return;
    }
    let crt = build_crt(32, 1, 0, &[(0u16, 0x8000u16, vec![0xa0u8; 0x2000])]);
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    m.attach_cart_from_bytes(&crt, "synthetic").expect("attach CRT");
    m.write_full(0x0001, 0x37);

    m.arm_cart_reads(true);
    for a in 0x8000u16..0x8100 {
        let _ = m.read_full_live(a); // `sidefx on` monitor lane
        let _ = m.read_full(a); // peek lane
    }
    assert!(m.drain_cart_reads().is_empty(), "monitor accesses are not the title's read-set");
}

// ── 2. REAL CARTRIDGE: the bank walk an actual title performs ─────────────────

/// Boot a real cartridge and report the read-set. The image is third-party
/// property — path from `TRX64_CART_READ_SET_CRT`, never committed here.
///
/// Optional knobs: `TRX64_CART_READ_SET_CYCLES` (default 30M ≈ 30 s PAL),
/// `TRX64_CART_READ_SET_MIN_BANKS` (assert at least this many DISTINCT banks were
/// read — the streaming-loader shape).
#[test]
#[ignore = "third-party cartridge: set TRX64_CART_READ_SET_CRT; run with --ignored --nocapture"]
fn real_cart_bank_walk() {
    if !roms_present() {
        eprintln!("SKIP: ROMs absent ({ROM_DIR})");
        return;
    }
    let path = match std::env::var("TRX64_CART_READ_SET_CRT") {
        Ok(p) => p,
        Err(_) => {
            eprintln!("SKIP: TRX64_CART_READ_SET_CRT unset");
            return;
        }
    };
    let crt = std::fs::read(&path).expect("read the cartridge image");
    let budget: u64 = std::env::var("TRX64_CART_READ_SET_CYCLES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000_000);
    let min_banks: usize = std::env::var("TRX64_CART_READ_SET_MIN_BANKS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);

    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let (name, ty) = m.attach_cart_from_bytes(&crt, "cart").expect("attach CRT");
    eprintln!("attached {name:?} ({ty:?}), {} bytes", crt.len());
    m.cold_reset();

    m.arm_cart_reads(true);
    let mut sink = NullSink;
    let mut done = 0u64;
    let mut recs = Vec::new();
    while done < budget {
        m.run_for_full(500_000, &mut sink, |_, _, _, _, _, _, _| {});
        done += 500_000;
        recs.extend(m.drain_cart_reads());
    }
    recs.extend(m.drain_cart_reads());

    assert!(!recs.is_empty(), "a booting cartridge must serve reads out of cart space");

    // The walk, in order, deduplicated for readability.
    let mut walk: Vec<(u16, u8)> = Vec::new();
    for r in &recs {
        if walk.last() != Some(&(r.bank, r.slot)) {
            walk.push((r.bank, r.slot));
        }
    }
    let mut distinct: Vec<(u16, u8)> = walk.clone();
    distinct.sort_unstable();
    distinct.dedup();
    let total: u64 = recs.iter().map(|r| r.bytes as u64).sum();
    eprintln!(
        "{} records, {} distinct (bank,slot), {total} bytes served, over {done} cycles",
        recs.len(),
        distinct.len()
    );
    eprintln!(
        "walk (bank/slot, consecutive duplicates collapsed): {}",
        walk.iter()
            .map(|(b, s)| format!("{b}{}", if *s == 0 { "L" } else { "H" }))
            .collect::<Vec<_>>()
            .join(" ")
    );
    // Per (bank, slot) totals — what a manifest slot span is validated against.
    let mut by_bank: std::collections::BTreeMap<(u16, u8), (u16, u16, u64)> = Default::default();
    for r in &recs {
        let e = by_bank.entry((r.bank, r.slot)).or_insert((0xffff, 0, 0));
        e.0 = e.0.min(r.off_lo);
        e.1 = e.1.max(r.off_hi);
        e.2 += r.bytes as u64;
    }
    for ((bank, slot), (lo, hi, bytes)) in &by_bank {
        eprintln!(
            "  bank {bank:>3} {}  ${lo:04X}-${hi:04X}  {bytes} bytes",
            if *slot == 0 { "ROML" } else { "ROMH" }
        );
    }

    for r in &recs {
        assert!(r.off_lo <= r.off_hi, "offset range ordered");
        assert!(r.off_hi <= 0x1fff, "offsets live inside the 8K window");
        assert!(r.bytes > 0, "a record always attributes >0 served reads");
    }
    let distinct_banks: std::collections::BTreeSet<u16> = recs.iter().map(|r| r.bank).collect();
    assert!(
        distinct_banks.len() >= min_banks,
        "expected reads from >= {min_banks} distinct banks, saw {:?}",
        distinct_banks
    );
}
