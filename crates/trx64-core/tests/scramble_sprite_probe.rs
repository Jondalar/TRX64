//! scramble_sprite_probe.rs — dump the scramble title sprite state for the
//! garbled-sprite diagnosis (DATA vs RENDERING).
//!
//! Boots, mounts scramble, LOAD"*",8,1, RUN, runs to the stable title, then dumps:
//!   * the active VIC bank base + screen base (so we know where the sprite pointers
//!     live — $screen+$3F8..$3FF),
//!   * the 8 sprite POINTERS (the data-block index per sprite),
//!   * the 63 DATA bytes of each enabled sprite's definition (what the pointer ×64
//!     resolves to, as the VIC reads it),
//!   * the VIC sprite registers $D000-$D02E (enable/x/y/exp/pri/mc/colors/x-msb).
//! Also writes the rendered framebuffer to traces/scramble_sprite_probe.ppm so the
//! image can be eyeballed and pixel-diffed against traces/scramble_ref_screen1.png.
//!
//! Run with:
//!   cargo test -p trx64-core --test scramble_sprite_probe -- --ignored --nocapture

use std::path::Path;
use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
const SAMPLE: &str =
    "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/samples/scramble_infinity.d64";

fn roms_present() -> bool {
    let d = Path::new(ROM_DIR);
    d.join("kernal-901227-03.bin").exists()
}

fn inject_keys(m: &mut Machine, s: &[u8]) {
    for (i, b) in s.iter().enumerate() {
        m.poke(0x0277 + i as u16, &[*b]);
    }
    m.poke(0x00c6, &[s.len() as u8]);
}

#[test]
#[ignore = "scramble title sprite-state dump; run with --ignored --nocapture"]
fn scramble_sprite_probe() {
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
    let mut obs = NullSink;

    // Boot to BASIC ready.
    m.run_for_full(2_500_000, &mut obs, |_, _, _, _, _, _, _| {});
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: d64.clone(),
        backing_path: Some(SAMPLE.to_string()),
        read_only: false,
    });
    m.run_for_full(500_000, &mut obs, |_, _, _, _, _, _, _| {});

    // LOAD"*",8,1 then run until LOAD returns to BASIC.
    inject_keys(&mut m, b"LOAD\"*\",8,1\r");
    let mut load_done = false;
    for _ in 0..800 {
        m.run_for_full(50_000, &mut obs, |_, _, _, _, _, _, _| {});
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

    // RUN. The title comes up after the fast loader; sweep cycle marks so we can
    // find the STABLE title window (bank/D018 + a small PPM at each), because the
    // game advances past the title later (different bank → full-screen garble).
    inject_keys(&mut m, b"RUN\r");
    let run_clk = m.cpu6510.clk;
    let trace_root = format!("{}/../../traces", env!("CARGO_MANIFEST_DIR"));
    // Marks (cycles after RUN) to probe.
    let marks: [u64; 12] = [
        5_000_000, 8_000_000, 10_000_000, 12_000_000, 14_000_000, 16_000_000, 18_000_000,
        20_000_000, 24_000_000, 28_000_000, 34_000_000, 40_000_000,
    ];
    let mut next = 0usize;
    let mut budget = 0u64;
    while next < marks.len() {
        m.run_for_full(100_000, &mut obs, |_, _, _, _, _, _, _| {});
        budget += 100_000;
        if budget >= marks[next] {
            let bb = m.vic_bank_base();
            let d018 = m.vic.regs[0x18];
            let d011 = m.vic.regs[0x11];
            let en = m.vic.regs[0x15];
            eprintln!(
                "  +{:>9} clk={} PC=${:04X} bank=${bb:04X} D018=${d018:02X} D011=${d011:02X} D015=${en:02X}",
                marks[next], m.cpu6510.clk, m.cpu6510.reg_pc
            );
            let (w, h, rgba) = m.render_canvas_rgba();
            let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
            for px in rgba.chunks_exact(4) {
                ppm.extend_from_slice(&px[..3]);
            }
            std::fs::write(format!("{trace_root}/scramble_mark_{:02}M.ppm", marks[next] / 1_000_000), &ppm).ok();
            next += 1;
        }
    }
    eprintln!("after RUN: PC=${:04X} clk={} (+{} after RUN)", m.cpu6510.reg_pc, m.cpu6510.clk, m.cpu6510.clk - run_clk);

    // ── VIC config ──────────────────────────────────────────────────────────
    let bank_base = m.vic_bank_base();
    let d018 = m.vic.regs[0x18];
    let screen_base = bank_base.wrapping_add(((d018 as u16 & 0xf0) << 6) as u16);
    eprintln!(
        "VIC: bank_base=${bank_base:04X} D018=${d018:02X} screen_base=${screen_base:04X} \
         D011=${:02X} D016=${:02X}",
        m.vic.regs[0x11], m.vic.regs[0x16]
    );

    // ── VIC sprite registers $D000-$D02E ────────────────────────────────────
    let r = |off: u8| m.vic.regs[(off & 0x3f) as usize];
    eprintln!("== VIC sprite registers ==");
    for s in 0..8 {
        eprintln!(
            "  spr{s}: X=${:02X} Y=${:02X} color(D02{:X})=${:02X}",
            r(0x00 + 2 * s),
            r(0x01 + 2 * s),
            7 + s,
            r(0x27 + s) & 0x0f
        );
    }
    eprintln!(
        "  D015 enable=${:02X} D010 x-msb=${:02X} D017 y-exp=${:02X} D01D x-exp=${:02X}",
        r(0x15), r(0x10), r(0x17), r(0x1d)
    );
    eprintln!(
        "  D01B pri=${:02X} D01C mc=${:02X} D025 mc0=${:02X} D026 mc1=${:02X}",
        r(0x1b), r(0x1c), r(0x25) & 0x0f, r(0x26) & 0x0f
    );

    // ── Sprite POINTERS (screen_base + $3F8 .. +$3FF) ───────────────────────
    eprintln!("== sprite pointers (screen_base+$3F8) ==");
    let mut ptrs = [0u8; 8];
    for s in 0..8u16 {
        let p = m.ram[screen_base.wrapping_add(0x3f8 + s) as usize];
        ptrs[s as usize] = p;
        eprintln!(
            "  spr{s}: ptr=${p:02X} -> data @ ${:04X}",
            (p as u16).wrapping_mul(64)
        );
    }

    // ── Sprite DATA (63 bytes per enabled sprite) as the VIC reads it ───────
    // The VIC reads via the bank/char-ROM-shadow rules; for scramble the sprite
    // data lives in RAM, so raw RAM is what the VIC sees (no $1000 shadow here),
    // but dump both the absolute address and the bytes.
    let enable = r(0x15);
    eprintln!("== sprite data: WRONG addr (ptr*64, no bank) vs CORRECT addr (bank|ptr*64) ==");
    for s in 0..8usize {
        if enable & (1 << s) == 0 {
            continue;
        }
        let wrong = (ptrs[s] as u16).wrapping_mul(64);
        let correct = bank_base.wrapping_add(wrong);
        let wbytes: Vec<u8> = (0..63).map(|i| m.ram[wrong.wrapping_add(i) as usize]).collect();
        let cbytes: Vec<u8> = (0..63).map(|i| m.ram[correct.wrapping_add(i) as usize]).collect();
        eprintln!("  spr{s} WRONG @ ${wrong:04X}: {:02X?}", &wbytes[..12]);
        eprintln!("  spr{s} RIGHT @ ${correct:04X}: {:02X?}", &cbytes[..12]);
    }

    // ── Dump the rendered framebuffer to a PPM (P6) for eyeballing ──────────
    let (w, h, rgba) = m.render_canvas_rgba();
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in rgba.chunks_exact(4) {
        ppm.push(px[0]);
        ppm.push(px[1]);
        ppm.push(px[2]);
    }
    let out = format!(
        "{}/../../traces/scramble_sprite_probe.ppm",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::write(&out, &ppm).expect("write ppm");
    eprintln!("wrote framebuffer {w}x{h} to {out}");
}
