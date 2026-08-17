//! Can the drive read back what it just wrote?
//!
//! SAVE a program, power-cycle, LOAD it again. The plainest thing a 1541 does,
//! and it did not work: the drive reported SAVING and then READY, and the very
//! next LOAD said FILE NOT FOUND.
//!
//! The cause was one line. `via2d.c:187` stores the byte the DOS puts on $1C01
//! into `GCR_write_value`, and the rotation engine latches it as the byte to
//! write at each byte boundary (`last_write_data = GCR_write_value`, in BOTH
//! engines' write branches). Our via2 `store_pra` dropped it — under a comment
//! saying the write value "is unused on the read-only LOAD path" — so every byte
//! the drive ever wrote came from a field nothing had ever set.
//!
//! What that cost beyond SAVE: a title that saves mid-run writes, reads back
//! immediately, gets a data-block error, and executes whatever is in the buffer.
//! One did exactly that — a JAM in the drive and the C64 waiting forever on a
//! CLK nobody would pull again.
//!
//!   cargo test -p trx64-core --test drive_write_readback_gate -- --nocapture

use std::path::Path;

use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");

fn sectors_per_track(t: u8) -> usize {
    match t {
        1..=17 => 21,
        18..=24 => 19,
        25..=30 => 18,
        _ => 17,
    }
}

/// A blank but FORMATTED 35-track D64: a valid BAM and an empty directory.
///
/// Built here rather than carried as a fixture, and built rather than formatted
/// in the emulator, because a real `N0:` format writes all 35 tracks and would
/// dominate the test. The GCR image the drive sees is synthesised from these
/// bytes with proper headers either way — what this supplies is the DOS-level
/// bookkeeping a SAVE needs in order to allocate a block.
fn blank_formatted_d64(name: &[u8], id: [u8; 2]) -> Vec<u8> {
    let mut d = vec![0u8; 174_848];
    let mut off = 0usize;
    for t in 1..18 {
        off += sectors_per_track(t) * 256;
    }
    let bam = off; // track 18 sector 0

    d[bam] = 18;
    d[bam + 1] = 1; // first directory sector
    d[bam + 2] = 0x41; // 'A' — DOS version
    d[bam + 3] = 0x00;

    for t in 1..=35u8 {
        let n = sectors_per_track(t);
        let e = bam + 4 + (t as usize - 1) * 4;
        // Track 18 keeps the BAM and the first directory sector for itself.
        let used: u32 = if t == 18 { 0b11 } else { 0 };
        let free_mask: u32 = ((1u32 << n) - 1) & !used;
        d[e] = free_mask.count_ones() as u8;
        d[e + 1] = (free_mask & 0xff) as u8;
        d[e + 2] = ((free_mask >> 8) & 0xff) as u8;
        d[e + 3] = ((free_mask >> 16) & 0xff) as u8;
    }

    for i in 0..16 {
        d[bam + 0x90 + i] = *name.get(i).unwrap_or(&0xa0);
    }
    d[bam + 0xa0] = 0xa0;
    d[bam + 0xa1] = 0xa0;
    d[bam + 0xa2] = id[0];
    d[bam + 0xa3] = id[1];
    d[bam + 0xa4] = 0xa0;
    d[bam + 0xa5] = b'2';
    d[bam + 0xa6] = b'A';
    for i in 0xa7..0xab {
        d[bam + i] = 0xa0;
    }

    // Directory sector 18/1: an empty chain.
    d[bam + 256] = 0x00;
    d[bam + 257] = 0xff;
    d
}

/// Put a line into the BASIC keyboard buffer and let the editor read it.
fn line(m: &mut Machine, s: &[u8], settle_frames: u32) {
    for (i, b) in s.iter().enumerate() {
        m.poke(0x0277 + i as u16, &[*b]);
    }
    m.poke(0x00c6, &[s.len() as u8]);
    let mut sink = NullSink;
    for _ in 0..settle_frames {
        m.run_for_full(19_656, &mut sink, |_, _, _, _, _, _, _| {});
    }
}

/// Wait for the drive to finish working — the activity LED, never the motor
/// (a 1541 keeps spinning after a job completes).
fn wait_drive_idle(m: &mut Machine, max_frames: u32) {
    let mut sink = NullSink;
    let mut ever_busy = false;
    for _ in 0..max_frames {
        let busy = m.drive8.led_on();
        if busy {
            ever_busy = true;
        } else if ever_busy {
            return;
        }
        m.run_for_full(19_656, &mut sink, |_, _, _, _, _, _, _| {});
    }
}

/// Read the 40×25 screen as plain text (screen codes → ASCII, letters + digits).
fn screen_text(m: &Machine) -> String {
    let mut s = String::new();
    for row in 0..25 {
        let mut line = String::new();
        for col in 0..40 {
            let v = m.read_full(0x0400 + row * 40 + col) & 0x3f;
            line.push(match v {
                0 => '@',
                1..=26 => (b'A' + (v as u8 - 1)) as char,
                48..=57 => (b'0' + (v as u8 - 48)) as char,
                _ => ' ',
            });
        }
        s.push_str(line.trim_end());
        s.push('\n');
    }
    s
}

#[test]
fn the_drive_can_read_back_what_it_wrote() {
    let rom_dir = Path::new(ROM_DIR);
    if !rom_dir.join("kernal-901227-03.bin").exists() {
        eprintln!("[skip] drive_write_readback_gate: ROMs absent at {}", rom_dir.display());
        return;
    }

    let disk = blank_formatted_d64(b"TESTDISK", [b'T', b'D']);
    let mut sink = NullSink;

    // ── SAVE ────────────────────────────────────────────────────────────────
    let mut m = Machine::new();
    m.boot_from_dir(rom_dir).expect("boot ROMs");
    m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: disk.clone(),
        backing_path: None,
        read_only: false,
    });
    m.run_for_full(800_000, &mut sink, |_, _, _, _, _, _, _| {});

    line(&mut m, b"10 PRINT\"HELLO\"\r", 60);
    line(&mut m, b"SAVE\"PROG\",8\r", 30);
    wait_drive_idle(&mut m, 900);
    m.run_for_full(1_000_000, &mut sink, |_, _, _, _, _, _, _| {});

    let after_save = screen_text(&m);
    assert!(
        after_save.contains("SAVING"),
        "the SAVE never started:\n{after_save}"
    );
    assert!(
        !after_save.contains("ERROR"),
        "the SAVE reported an error:\n{after_save}"
    );

    // Take the image back out of the drive — this is what a persist writes.
    m.drive8.flush_disk_writeback();
    let written = m.drive8.disk.as_ref().expect("disk still attached").bytes.clone();
    assert_eq!(written.len(), disk.len(), "the image kept its size");
    assert_ne!(written, disk, "a SAVE must change the image");

    // The directory must name the file. A drive that wrote a constant produced a
    // structurally intact directory too — this is the cheap half of the check.
    let mut dir_off = 0usize;
    for t in 1..18 {
        dir_off += sectors_per_track(t) * 256;
    }
    dir_off += 256; // 18/1
    let entry = &written[dir_off + 2..dir_off + 32];
    assert_eq!(entry[0] & 0x8f, 0x82, "a closed PRG entry, got type {:02x}", entry[0]);
    assert_eq!(&entry[3..7], b"PROG", "the directory names the file");

    // ── LOAD, on a machine that has never seen the program ───────────────────
    let mut m2 = Machine::new();
    m2.boot_from_dir(rom_dir).expect("boot ROMs");
    m2.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});
    m2.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: written,
        backing_path: None,
        read_only: false,
    });
    m2.run_for_full(800_000, &mut sink, |_, _, _, _, _, _, _| {});

    line(&mut m2, b"LOAD\"PROG\",8\r", 30);
    wait_drive_idle(&mut m2, 1200);
    m2.run_for_full(2_000_000, &mut sink, |_, _, _, _, _, _, _| {});

    let after_load = screen_text(&m2);
    assert!(
        !after_load.contains("FILE NOT FOUND"),
        "the drive could not find what it had just written:\n{after_load}"
    );
    assert!(
        !after_load.contains("ERROR"),
        "the LOAD reported an error:\n{after_load}"
    );

    // And the program is really there: BASIC's start-of-variables pointer moved
    // past the line, and the line's text is in RAM at $0801.
    let vartab = m2.read_full(0x002d) as u16 | ((m2.read_full(0x002e) as u16) << 8);
    assert!(vartab > 0x0801, "BASIC still empty after LOAD (vartab ${vartab:04X})");
    let mut prog = Vec::new();
    for a in 0x0801..vartab.min(0x0830) {
        prog.push(m2.read_full(a));
    }
    assert!(
        prog.windows(5).any(|w| w == b"HELLO"),
        "the loaded program is not the one that was saved: {prog:02x?}"
    );
}
