//! Spec 872 — a 1581 drive.
//!
//! Every test boots the real machine with the real KERNAL and the real 1581 DOS and
//! asks its question through the KERNAL: `LOAD"$",n`, `LOAD`, `SAVE`, `OPEN15` and a
//! BASIC loop. The D81s are built here and read back here with a static reader after
//! the drive's write-back — nothing pokes the drive to make a point.
//!
//! The 1581 DOS (`dos1581-318045-02.bin`) is Commodore IP and not bundled. It is looked
//! for in `$TRX64_1581_ROM_DIR`, the C64 ROM directory, and VICE's `data/DRIVES`.
//!
//!   cargo test --release -p trx64-core --test drive1581_gate -- --nocapture

use std::path::{Path, PathBuf};

use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, prepare_drive_types, restore_runtime_checkpoint};
use trx64_core::drive::{DiskImage, DiskKind, DrivePosition};
use trx64_core::drive_snapshot::{capture_drive1541, capture_drive_disk_image};
use trx64_core::native_snapshot::{read_native_snapshot, write_native_snapshot, WriteNativeSnapshotArgs};
use trx64_core::iec::DriveType;
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const VICE_DRIVES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../vice/vice/data/DRIVES");
const DOS_1541: &str = "dos1541-325302-01+901229-05.bin";
const DOS_1581: &str = "dos1581-318045-02.bin";
const FRAME: u64 = 19_656;

fn rom_1581() -> Option<Vec<u8>> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(d) = std::env::var("TRX64_1581_ROM_DIR") {
        dirs.push(d.into());
    }
    dirs.push(ROM_DIR.into());
    dirs.push(VICE_DRIVES.into());
    dirs.iter().find_map(|d| std::fs::read(d.join(DOS_1581)).ok())
}

fn roms_present() -> bool {
    let d = Path::new(ROM_DIR);
    d.join("kernal-901227-03.bin").exists() && d.join(DOS_1541).exists() && rom_1581().is_some()
}

macro_rules! need_roms {
    () => {
        if !roms_present() {
            eprintln!("[skip] drive1581_gate: ROMs absent (C64 at {ROM_DIR}, 1581 DOS)");
            return;
        }
    };
}

// ── D81: build and read back ────────────────────────────────────────────────────

const D81_LEN: usize = 819_200;

fn off(t: u8, s: u8) -> usize {
    ((t as usize - 1) * 40 + s as usize) * 256
}

/// A formatted 80-track D81: header 40/0, BAM 40/1-2, directory from 40/3.
#[derive(Clone)]
struct D81 {
    bytes: Vec<u8>,
    dir_slot: usize,
}

impl D81 {
    fn new(name: &[u8], id: [u8; 2]) -> Self {
        let mut d = vec![0u8; D81_LEN];
        let h = off(40, 0);
        d[h] = 40;
        d[h + 1] = 3;
        d[h + 2] = 0x44;
        for i in 0..16 {
            d[h + 4 + i] = *name.get(i).unwrap_or(&0xa0);
        }
        d[h + 0x14] = 0xa0;
        d[h + 0x15] = 0xa0;
        d[h + 0x16] = id[0];
        d[h + 0x17] = id[1];
        d[h + 0x18] = 0xa0;
        d[h + 0x19] = b'3';
        d[h + 0x1a] = b'D';
        d[h + 0x1b] = 0xa0;
        d[h + 0x1c] = 0xa0;
        for (k, sec) in [(0usize, 1u8), (1, 2)] {
            let b = off(40, sec);
            d[b] = if k == 0 { 40 } else { 0 };
            d[b + 1] = if k == 0 { 2 } else { 0xff };
            d[b + 2] = 0x44;
            d[b + 3] = 0xbb;
            d[b + 4] = id[0];
            d[b + 5] = id[1];
            d[b + 6] = 0xc0;
            d[b + 7] = 0x00;
            for i in 0..40usize {
                let t = (k * 40 + i + 1) as u8;
                let e = b + 0x10 + i * 6;
                let used: u64 = if t == 40 { 0b1111 } else { 0 };
                let free: u64 = ((1u64 << 40) - 1) & !used;
                d[e] = free.count_ones() as u8;
                for j in 0..5 {
                    d[e + 1 + j] = (free >> (8 * j)) as u8;
                }
            }
        }
        let dir = off(40, 3);
        d[dir] = 0;
        d[dir + 1] = 0xff;
        D81 { bytes: d, dir_slot: 0 }
    }

    fn bam_entry(&self, t: u8) -> usize {
        let b = if t <= 40 { off(40, 1) } else { off(40, 2) };
        b + 0x10 + ((t as usize - 1) % 40) * 6
    }

    fn mark_used(&mut self, t: u8, s: u8) {
        let e = self.bam_entry(t);
        let byte = e + 1 + (s as usize / 8);
        let bit = 1u8 << (s % 8);
        assert!(self.bytes[byte] & bit != 0, "{t}/{s} already used");
        self.bytes[byte] &= !bit;
        self.bytes[e] -= 1;
    }

    /// Add a closed file (`ftype` 0x82 PRG, 0x81 SEQ) on the sectors `secs`.
    fn add_at(mut self, name: &[u8], ftype: u8, data: &[u8], secs: &[(u8, u8)]) -> Self {
        let chunks: Vec<&[u8]> = data.chunks(254).collect();
        assert_eq!(chunks.len(), secs.len(), "one sector per 254 bytes");
        for &(t, s) in secs {
            self.mark_used(t, s);
        }
        for (i, c) in chunks.iter().enumerate() {
            let o = off(secs[i].0, secs[i].1);
            if let Some(&(nt, ns)) = secs.get(i + 1) {
                self.bytes[o] = nt;
                self.bytes[o + 1] = ns;
            } else {
                self.bytes[o] = 0;
                self.bytes[o + 1] = (c.len() + 1) as u8;
            }
            self.bytes[o + 2..o + 2 + c.len()].copy_from_slice(c);
        }
        assert!(self.dir_slot < 8);
        let e = off(40, 3) + self.dir_slot * 32;
        self.dir_slot += 1;
        self.bytes[e + 2] = ftype;
        self.bytes[e + 3] = secs[0].0;
        self.bytes[e + 4] = secs[0].1;
        for i in 0..16 {
            self.bytes[e + 5 + i] = *name.get(i).unwrap_or(&0xa0);
        }
        self.bytes[e + 30] = secs.len() as u8;
        self.bytes[e + 31] = (secs.len() >> 8) as u8;
        self
    }

    /// Add a file laid down from `(t, s)` upwards through the sectors and downwards
    /// through the tracks.
    fn add(self, name: &[u8], ftype: u8, data: &[u8], start: (u8, u8)) -> Self {
        let n = data.chunks(254).count();
        let (mut t, mut s) = start;
        let mut secs = Vec::new();
        for _ in 0..n {
            secs.push((t, s));
            if s == 39 {
                s = 0;
                t -= 1;
            } else {
                s += 1;
            }
        }
        self.add_at(name, ftype, data, &secs)
    }
}

/// A static D81 directory read: header name, id, the entries, blocks free.
struct StaticDir {
    name: String,
    id: String,
    files: Vec<(String, u8, u16)>,
    blocks_free: u32,
}

fn petscii(b: &[u8]) -> String {
    b.iter().take_while(|&&c| c != 0xa0).map(|&c| c as char).collect()
}

fn static_dir(img: &[u8]) -> StaticDir {
    let h = off(40, 0);
    let name = petscii(&img[h + 4..h + 20]);
    let id = petscii(&img[h + 0x16..h + 0x18]);
    let mut files = Vec::new();
    let (mut t, mut s) = (img[h], img[h + 1]);
    let mut guard = 0;
    while t != 0 && guard < 40 {
        guard += 1;
        let o = off(t, s);
        for k in 0..8 {
            let e = o + k * 32;
            let ft = img[e + 2];
            if ft & 0x80 == 0 {
                continue;
            }
            let blocks = img[e + 30] as u16 | (img[e + 31] as u16) << 8;
            files.push((petscii(&img[e + 5..e + 21]), ft, blocks));
        }
        t = img[o];
        s = img[o + 1];
    }
    let mut blocks_free = 0u32;
    for t in 1..=80u8 {
        if t == 40 {
            continue;
        }
        let b = if t <= 40 { off(40, 1) } else { off(40, 2) };
        blocks_free += img[b + 0x10 + ((t as usize - 1) % 40) * 6] as u32;
    }
    StaticDir { name, id, files, blocks_free }
}

/// The contents of the closed file `name`, following the directory and the chain.
fn read_file(img: &[u8], name: &[u8]) -> Option<(u8, Vec<u8>)> {
    let h = off(40, 0);
    let (mut t, mut s) = (img[h], img[h + 1]);
    let mut guard = 0;
    while t != 0 && guard < 40 {
        guard += 1;
        let o = off(t, s);
        for k in 0..8 {
            let e = o + k * 32;
            let ftype = img[e + 2];
            if ftype & 0x80 == 0 {
                continue;
            }
            let n: Vec<u8> = img[e + 5..e + 21].iter().copied().take_while(|&b| b != 0xa0).collect();
            if n == name {
                let (mut ft, mut fs) = (img[e + 3], img[e + 4]);
                let mut out = Vec::new();
                let mut g = 0;
                while ft != 0 && g < 3200 {
                    g += 1;
                    let d = off(ft, fs);
                    if img[d] == 0 {
                        out.extend_from_slice(&img[d + 2..=d + img[d + 1] as usize]);
                        break;
                    }
                    out.extend_from_slice(&img[d + 2..d + 256]);
                    ft = img[d];
                    fs = img[d + 1];
                }
                return Some((ftype, out));
            }
        }
        t = img[o];
        s = img[o + 1];
    }
    None
}

/// A tokenized BASIC program of REM lines, as a PRG.
fn basic_prg(lines: usize) -> Vec<u8> {
    let mut prg = vec![0x01, 0x08];
    let mut addr: u16 = 0x0801;
    for i in 0..lines {
        let mut body = Vec::new();
        let num = 10 * (i as u16 + 1);
        body.extend_from_slice(&num.to_le_bytes());
        body.push(0x8f);
        for k in 0..60u32 {
            body.push(0x21 + ((i as u32 * 7 + k * 3) % 90) as u8);
        }
        body.push(0);
        let next = addr + 2 + body.len() as u16;
        prg.extend_from_slice(&next.to_le_bytes());
        prg.extend_from_slice(&body);
        addr = next;
    }
    prg.extend_from_slice(&[0, 0]);
    prg
}

fn d81(bytes: Vec<u8>) -> DiskImage {
    DiskImage { kind: DiskKind::D81, bytes, backing_path: None, read_only: false }
}

// ── the machine through the keyboard ────────────────────────────────────────────

fn frames(m: &mut Machine, n: u32) {
    let mut sink = NullSink;
    for _ in 0..n {
        m.run_for_full(FRAME, &mut sink, |_, _, _, _, _, _, _| {});
    }
}

/// Booted to READY with a 1581 in position A at unit 8 holding `a`.
fn booted_1581(a: Option<Vec<u8>>) -> Machine {
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    m.set_drive_power(DrivePosition::A, false).unwrap();
    m.set_drive_type(DrivePosition::A, DriveType::Drive1581).expect("A off: type change");
    m.drive8.set_rom_1581(&rom_1581().unwrap()).unwrap();
    if let Some(a) = a {
        m.drive8.mount(d81(a)).expect("a D81 fits a 1581");
    }
    m.set_drive_power(DrivePosition::A, true).unwrap();
    frames(&mut m, 200);
    m
}

fn screen(m: &Machine) -> String {
    let mut s = String::new();
    for row in 0..25u16 {
        let mut line = String::new();
        for col in 0..40u16 {
            let v = m.read_full(0x0400 + row * 40 + col) & 0x7f;
            line.push(match v {
                1..=26 => (b'A' + (v - 1)) as char,
                48..=57 => (b'0' + (v - 48)) as char,
                0x2e => '.',
                0x24 => '$',
                0x22 => '"',
                0x3f => '?',
                0x2c => ',',
                _ => ' ',
            });
        }
        s.push_str(line.trim_end());
        s.push('\n');
    }
    s
}

fn type_in(m: &mut Machine, s: &[u8]) {
    for chunk in s.chunks(10) {
        let mut waited = 0;
        while m.read_full(0x00c6) != 0 && waited < 200 {
            frames(m, 1);
            waited += 1;
        }
        for (i, b) in chunk.iter().enumerate() {
            m.poke(0x0277 + i as u16, &[*b]);
        }
        m.poke(0x00c6, &[chunk.len() as u8]);
        frames(m, 2);
    }
}

fn command(m: &mut Machine, cmd: &[u8], max_frames: u32) -> String {
    type_in(m, b"\x93");
    frames(m, 3);
    type_in(m, cmd);
    for _ in 0..max_frames {
        frames(m, 1);
        if screen(m).contains("READY.") {
            break;
        }
    }
    screen(m)
}

fn load_dir(m: &mut Machine, unit: u8) -> String {
    let out = command(m, format!("LOAD\"$\",{unit}\r").as_bytes(), 3000);
    if out.contains("ERROR") {
        return out;
    }
    let list = command(m, b"LIST\r", 300);
    format!("{out}{list}")
}

// ── D64 (for the 1541 beside the 1581) ──────────────────────────────────────────

fn d64_spt(t: u8) -> usize {
    match t {
        1..=17 => 21,
        18..=24 => 19,
        25..=30 => 18,
        _ => 17,
    }
}

fn d64_off(t: u8, s: u8) -> usize {
    ((1..t).map(d64_spt).sum::<usize>() + s as usize) * 256
}

/// A formatted 35-track D64 with files laid down from track 17 downwards (the 871
/// gate's builder).
fn d64_with(name: &[u8], files: &[(&[u8], u8, Vec<u8>)]) -> Vec<u8> {
    let mut d = vec![0u8; 174_848];
    let bam = d64_off(18, 0);
    d[bam] = 18;
    d[bam + 1] = 1;
    d[bam + 2] = 0x41;
    for t in 1..=35u8 {
        let n = d64_spt(t);
        let e = bam + 4 + (t as usize - 1) * 4;
        let used: u32 = if t == 18 { 0b11 } else { 0 };
        let free: u32 = ((1u32 << n) - 1) & !used;
        d[e] = free.count_ones() as u8;
        d[e + 1] = free as u8;
        d[e + 2] = (free >> 8) as u8;
        d[e + 3] = (free >> 16) as u8;
    }
    for i in 0..16 {
        d[bam + 0x90 + i] = *name.get(i).unwrap_or(&0xa0);
    }
    for (i, b) in [0xa0, 0xa0, b'2', b'D', 0xa0, b'2', b'A', 0xa0, 0xa0, 0xa0, 0xa0].iter().enumerate() {
        d[bam + 0xa0 + i] = *b;
    }
    let dir = d64_off(18, 1);
    d[dir + 1] = 0xff;
    let mut next = (17u8, 0u8);
    for (slot, (fname, ftype, data)) in files.iter().enumerate() {
        let chunks: Vec<&[u8]> = data.chunks(254).collect();
        let mut secs = Vec::new();
        for _ in 0..chunks.len() {
            let (t, s) = next;
            next = if (s as usize) + 1 < d64_spt(t) { (t, s + 1) } else { (t - 1, 0) };
            let e = bam + 4 + (t as usize - 1) * 4;
            let mut map = d[e + 1] as u32 | (d[e + 2] as u32) << 8 | (d[e + 3] as u32) << 16;
            map &= !(1 << s);
            d[e] -= 1;
            d[e + 1] = map as u8;
            d[e + 2] = (map >> 8) as u8;
            d[e + 3] = (map >> 16) as u8;
            secs.push((t, s));
        }
        for (i, c) in chunks.iter().enumerate() {
            let o = d64_off(secs[i].0, secs[i].1);
            if let Some(&(nt, ns)) = secs.get(i + 1) {
                d[o] = nt;
                d[o + 1] = ns;
            } else {
                d[o] = 0;
                d[o + 1] = (c.len() + 1) as u8;
            }
            d[o + 2..o + 2 + c.len()].copy_from_slice(c);
        }
        let e = dir + slot * 32;
        d[e + 2] = *ftype;
        d[e + 3] = secs[0].0;
        d[e + 4] = secs[0].1;
        for i in 0..16 {
            d[e + 5 + i] = *fname.get(i).unwrap_or(&0xa0);
        }
        d[e + 30] = secs.len() as u8;
    }
    d
}

fn d64_read_file(img: &[u8], name: &[u8]) -> Option<(u8, Vec<u8>)> {
    let (mut t, mut s) = (18u8, 1u8);
    let mut guard = 0;
    while t != 0 && guard < 20 {
        guard += 1;
        let o = d64_off(t, s);
        for k in 0..8 {
            let e = o + k * 32;
            let ftype = img[e + 2];
            if ftype & 0x80 == 0 {
                continue;
            }
            let n: Vec<u8> = img[e + 5..e + 21].iter().copied().take_while(|&b| b != 0xa0).collect();
            if n == name {
                let (mut ft, mut fs) = (img[e + 3], img[e + 4]);
                let mut out = Vec::new();
                let mut g = 0;
                while ft != 0 && g < 700 {
                    g += 1;
                    let d = d64_off(ft, fs);
                    if img[d] == 0 {
                        out.extend_from_slice(&img[d + 2..=d + img[d + 1] as usize]);
                        break;
                    }
                    out.extend_from_slice(&img[d + 2..d + 256]);
                    ft = img[d];
                    fs = img[d + 1];
                }
                return Some((ftype, out));
            }
        }
        t = img[o];
        s = img[o + 1];
    }
    None
}

// ── static D81 checks ───────────────────────────────────────────────────────────

/// The chain of sectors a file (or the directory) occupies.
fn chain(img: &[u8], t: u8, s: u8) -> Vec<(u8, u8)> {
    let (mut t, mut s) = (t, s);
    let mut out = Vec::new();
    while t != 0 && out.len() < 3200 {
        out.push((t, s));
        let o = off(t, s);
        let (nt, ns) = (img[o], img[o + 1]);
        t = nt;
        s = ns;
    }
    out
}

/// A static check that the BAM and the directory agree: every track's free count is its
/// bitmap's, and the sectors marked used are exactly the header, the two BAM sectors,
/// the directory chain and every file's chain.
fn bam_consistent(img: &[u8]) -> Result<(), String> {
    let mut used = std::collections::BTreeSet::new();
    used.insert((40u8, 0u8));
    used.insert((40, 1));
    used.insert((40, 2));
    let h = off(40, 0);
    for ts in chain(img, img[h], img[h + 1]) {
        used.insert(ts);
    }
    let (mut t, mut s) = (img[h], img[h + 1]);
    let mut guard = 0;
    while t != 0 && guard < 40 {
        guard += 1;
        let o = off(t, s);
        for k in 0..8 {
            let e = o + k * 32;
            if img[e + 2] & 0x80 != 0 {
                for ts in chain(img, img[e + 3], img[e + 4]) {
                    if !used.insert(ts) {
                        return Err(format!("sector {}/{} in two chains", ts.0, ts.1));
                    }
                }
            }
        }
        t = img[o];
        s = img[o + 1];
    }
    for t in 1..=80u8 {
        let b = if t <= 40 { off(40, 1) } else { off(40, 2) };
        let e = b + 0x10 + ((t as usize - 1) % 40) * 6;
        let mut map = 0u64;
        for j in 0..5 {
            map |= (img[e + 1 + j] as u64) << (8 * j);
        }
        if map >> 40 != 0 {
            return Err(format!("track {t}: bits above sector 39"));
        }
        if img[e] as u32 != map.count_ones() {
            return Err(format!("track {t}: free count {} vs bitmap {}", img[e], map.count_ones()));
        }
        for s in 0..40u8 {
            let free = map & (1 << s) != 0;
            if free == used.contains(&(t, s)) {
                return Err(format!("sector {t}/{s}: BAM says {}", if free { "free, but it is in use" } else { "used, but nothing holds it" }));
            }
        }
    }
    Ok(())
}

/// The sectors that differ between two images.
fn changed_sectors(a: &[u8], b: &[u8]) -> Vec<(u8, u8)> {
    (0..3200usize)
        .filter(|&i| a[i * 256..(i + 1) * 256] != b[i * 256..(i + 1) * 256])
        .map(|i| ((i / 40 + 1) as u8, (i % 40) as u8))
        .collect()
}

// ── machine helpers ─────────────────────────────────────────────────────────────

/// Run until the 1581 in `pos` has let go of the disk: its DOS keeps a track cache and
/// writes it back on its own schedule (the real drive's LED goes out then), so a host
/// that persists at `READY.` persists what the drive has not written yet.
fn settle(m: &mut Machine, pos: DrivePosition) {
    for _ in 0..3000 {
        let idle = match m.drive(pos).board_1581() {
            Some(b) => !b.ports().motor_on && !b.wd().busy,
            None => true,
        };
        if idle {
            return;
        }
        frames(m, 1);
    }
    panic!("the 1581 in {pos:?} never went idle");
}

fn persisted(m: &mut Machine, pos: DrivePosition) -> Vec<u8> {
    settle(m, pos);
    let d = m.drive_mut(pos);
    d.flush_disk_writeback();
    d.get_attached_disk().expect("a disk").bytes.clone()
}

/// Spec 872 §9.10 — a stock C64 never clocks SRQ, so the DOS never turns its serial
/// port toward the bus to send. What the ROM does with PB5 (read, 2026-09-23): the
/// burst send paths (`$ACD4`, `$ADAD`) are gated by `$76` bit 5, which only an SDR
/// byte arriving over SRQ sets; but `$DBC7`, reached from `$ACBB` at the end of every
/// ATN sequence, resets the shift register by toggling CRA's SP mode and raises PB5 for
/// exactly the 32 cycles from `$DBCF` to `$ACCA` (`STA $4001` … `STA $4001`), with no
/// byte in the SDR. So: every PB5 high is that 32-cycle pulse, and no byte is ever
/// shifted out. A burst send would hold PB5 for a whole byte and count one.
fn assert_no_fast_serial(m: &Machine, pos: DrivePosition) {
    if let Some(b) = m.drive(pos).board_1581() {
        let ch = &b.fast_log.pb5_changes;
        assert!(!ch.last().is_some_and(|c| c.1), "PB5 left high: {ch:?}");
        for w in ch.windows(2) {
            if w[0].1 {
                assert!(!w[1].1 && w[1].0 - w[0].0 <= 32, "PB5 high for more than the $DBC7 pulse: {w:?}");
            }
        }
        assert_eq!(b.fast_log.bytes_out, 0, "the serial port shifted bytes out");
    }
}

/// A parsed directory listing: the header, the entries (blocks, name, type), blocks free.
type Listing = (Option<String>, Vec<(u16, String, String)>, Option<u32>);

/// Parse a BASIC `LIST` of a directory: the header line and each entry line.
fn parse_listing(out: &str) -> Listing {
    let mut header = None;
    let mut files = Vec::new();
    let mut free = None;
    for line in out.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_suffix("BLOCKS FREE.") {
            free = rest.trim().parse().ok();
            continue;
        }
        let Some(q1) = l.find('"') else { continue };
        let Some(q2) = l[q1 + 1..].find('"').map(|x| x + q1 + 1) else { continue };
        let n: u16 = match l[..q1].trim().parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let name = l[q1 + 1..q2].to_string();
        let tail = l[q2 + 1..].trim().to_string();
        if header.is_none() && n == 0 && files.is_empty() {
            header = Some(format!("{name}|{tail}"));
        } else {
            files.push((n, name.trim_end().to_string(), tail));
        }
    }
    (header, files, free)
}

/// The drive's error channel, read by a program (`INPUT#` is illegal in direct mode).
fn error_channel(m: &mut Machine, unit: u8) -> String {
    type_in(m, b"NEW\r");
    frames(m, 10);
    type_in(m, format!("10 OPEN15,{unit},15:INPUT#15,A,B$,C,D:PRINTA;B$;C;D:CLOSE15\r").as_bytes());
    frames(m, 5);
    command(m, b"RUN\r", 1200)
}

// ── §9.1 — directory ────────────────────────────────────────────────────────────

fn directory_image() -> Vec<u8> {
    D81::new(b"THE1581DISK", *b"81")
        .add(b"FIRST", 0x82, &basic_prg(3), (39, 0))
        .add(b"SECOND FILE", 0x81, &(0..700u32).map(|i| i as u8).collect::<Vec<_>>(), (41, 0))
        .add(b"THIRD", 0x82, &basic_prg(40), (80, 10))
        .bytes
}

#[test]
fn a_1581_lists_the_directory_a_static_read_gives() {
    need_roms!();
    let img = directory_image();
    let st = static_dir(&img);
    let mut m = booted_1581(Some(img.clone()));
    let out = load_dir(&mut m, 8);
    let (header, files, free) = parse_listing(&out);
    let want_header = format!("{:<16}|{} 3D", st.name, st.id);
    assert_eq!(header.as_deref(), Some(want_header.as_str()), "header and ID:\n{out}");
    let want: Vec<(u16, String, String)> = st
        .files
        .iter()
        .map(|(n, t, b)| (*b, n.clone(), match t & 7 { 1 => "SEQ", 2 => "PRG", _ => "?" }.to_string()))
        .collect();
    assert_eq!(files, want, "entries:\n{out}");
    assert_eq!(free, Some(st.blocks_free), "blocks free:\n{out}");
    assert_eq!(persisted(&mut m, DrivePosition::A), img, "a directory read writes nothing");
    assert_no_fast_serial(&m, DrivePosition::A);
}

// ── §9.2 — KERNAL LOAD ──────────────────────────────────────────────────────────

/// A PRG of 15 blocks laid down from 39/30: ten sectors on side 1 of track 39
/// (logical sectors 20-39), then five on side 0 of track 38.
fn load_image() -> (Vec<u8>, Vec<u8>) {
    let prg = basic_prg(58);
    let blocks = prg.chunks(254).count();
    assert!(blocks >= 12, "the file must leave track 39: {blocks} blocks");
    (D81::new(b"LOADTEST", *b"LT").add(b"FILE", 0x82, &prg, (39, 30)).bytes, prg)
}

fn loaded(m: &Machine, prg: &[u8]) -> Vec<u8> {
    let end = 0x0801 + (prg.len() - 2) as u16;
    (0x0801..end).map(|a| m.read_full(a)).collect()
}

#[test]
fn kernal_load_across_a_track_and_both_sides() {
    need_roms!();
    let (img, prg) = load_image();
    let secs = chain(&img, 39, 30);
    assert!(secs.iter().any(|s| s.0 == 39 && s.1 >= 20) && secs.iter().any(|s| s.0 == 38 && s.1 < 20));
    let mut m = booted_1581(Some(img));
    let out = command(&mut m, b"LOAD\"FILE\",8,1\r", 6000);
    assert!(out.contains("LOADING") && !out.contains("ERROR"), "LOAD:\n{out}");
    assert_eq!(loaded(&m, &prg), prg[2..], "the PRG in RAM is the file");
    let end = 0x0801 + (prg.len() - 2) as u16;
    assert_eq!(m.read_full(0x2d) as u16 | (m.read_full(0x2e) as u16) << 8, end, "end of load");
    assert_no_fast_serial(&m, DrivePosition::A);
}

// ── §9.3 — KERNAL SAVE ──────────────────────────────────────────────────────────

#[test]
fn kernal_save_writes_a_consistent_d81() {
    need_roms!();
    let (img, prg) = load_image();
    let mut m = booted_1581(Some(img.clone()));
    let out = command(&mut m, b"LOAD\"FILE\",8,1\r", 6000);
    assert!(!out.contains("ERROR"), "{out}");
    let out = command(&mut m, b"SAVE\"NEW\",8\r", 8000);
    assert!(out.contains("SAVING") && !out.contains("ERROR"), "SAVE:\n{out}");
    let after = persisted(&mut m, DrivePosition::A);
    let (ftype, saved) = read_file(&after, b"NEW").expect("NEW in the image");
    assert_eq!(ftype, 0x82, "a closed PRG");
    assert_eq!(saved, prg, "NEW is byte-identical to the saved RAM range");
    bam_consistent(&after).expect("BAM and directory agree");
    // Every sector that changed is NEW's, the BAM's or the directory's.
    let e = off(40, 3);
    let new_entry = (0..8).map(|k| e + k * 32).find(|&x| &after[x + 5..x + 8] == b"NEW").expect("NEW's entry");
    let mut allowed: std::collections::BTreeSet<(u8, u8)> = chain(&after, after[new_entry + 3], after[new_entry + 4]).into_iter().collect();
    allowed.extend([(40u8, 1u8), (40, 2)]);
    allowed.extend(chain(&after, 40, 3));
    for s in changed_sectors(&img, &after) {
        assert!(allowed.contains(&s), "sector {}/{} changed and is not NEW's, the BAM or the directory", s.0, s.1);
    }
    assert_no_fast_serial(&m, DrivePosition::A);

    // The same image in a fresh machine loads NEW back.
    let mut r = booted_1581(Some(after));
    let out = command(&mut r, b"LOAD\"NEW\",8,1\r", 6000);
    assert!(out.contains("LOADING") && !out.contains("ERROR"), "LOAD NEW:\n{out}");
    assert_eq!(loaded(&r, &prg), prg[2..], "NEW loads back");
}

// ── §9.4 — format ───────────────────────────────────────────────────────────────

#[test]
fn format_writes_every_physical_track_and_a_valid_d81() {
    need_roms!();
    // Blank-filled: every byte a value the DOS's format does not write.
    let blank = vec![0xe5u8; D81_LEN];
    let mut m = booted_1581(Some(blank));
    let out = command(&mut m, b"OPEN15,8,15,\"N:TEST,72\":CLOSE15\r", 20_000);
    assert!(out.contains("READY.") && !out.contains("ERROR"), "format:\n{out}");
    // Wait until the drive has let go of the disk.
    for _ in 0..600 {
        if !m.drive8.board_1581().unwrap().wd().busy && !m.drive8.board_1581().unwrap().ports().activity_led {
            break;
        }
        frames(&mut m, 1);
    }
    let log = m.drive8.board_1581().unwrap().wd.write_track_log.clone();
    let mut seen = std::collections::BTreeSet::new();
    seen.extend(log.iter().copied());
    for t in 0..80u8 {
        for h in 0..2u8 {
            assert!(seen.contains(&(t, h)), "physical track {t} side {h} was never written by WRITE TRACK ({} commands)", log.len());
        }
    }
    let img = persisted(&mut m, DrivePosition::A);
    assert_eq!(log.len(), 160, "one WRITE TRACK per physical track and side");
    // Every sector off the directory track was rewritten from the surface the WRITE
    // TRACKs laid down. Track 40 is the DOS's own: after the format it writes its track
    // cache back, and the cache still holds what it read there before (a D81 always
    // presents a readable surface, blank-filled or not) — what a real 1581 does with a
    // disk that carried data. Its header, BAM and directory sectors are checked below.
    let untouched: Vec<(usize, usize)> = (0..3200usize)
        .filter(|&i| i / 40 + 1 != 40 && img[i * 256..(i + 1) * 256].iter().all(|&b| b == 0xe5))
        .map(|i| (i / 40 + 1, i % 40))
        .collect();
    assert!(untouched.is_empty(), "sectors still holding the blank fill: {untouched:?}");
    let st = static_dir(&img);
    assert_eq!((st.name.as_str(), st.id.as_str()), ("TEST", "72"), "header at 40/0");
    assert_eq!(&img[off(40, 1) + 2..off(40, 1) + 4], &[0x44, 0xbb], "BAM at 40/1");
    assert_eq!(&img[off(40, 2) + 2..off(40, 2) + 4], &[0x44, 0xbb], "BAM at 40/2");
    assert_eq!(st.blocks_free, 3160, "blocks free");
    bam_consistent(&img).expect("the formatted BAM");
    assert_no_fast_serial(&m, DrivePosition::A);
    // Mounted again it lists TEST.
    let mut r = booted_1581(Some(img));
    let out = load_dir(&mut r, 8);
    let (header, files, free) = parse_listing(&out);
    assert!(header.is_some_and(|h| h.starts_with("TEST ")), "{out}");
    assert!(files.is_empty() && free == Some(3160), "{out}");
}

// ── §9.5 — a 1541 and a 1581 on one bus ─────────────────────────────────────────

const COPY_8_TO_9: &[&[u8]] = &[
    b"10 OPEN2,8,2,\"F,S,R\"\r",
    b"20 OPEN3,9,3,\"F,S,W\"\r",
    b"30 GET#2,A$:S=ST\r",
    b"40 IFA$=\"\"THENA$=CHR$(0)\r",
    b"50 PRINT#3,A$;\r",
    b"60 IFS=0THEN30\r",
    b"70 CLOSE3:CLOSE2\r",
];

fn seq_data() -> Vec<u8> {
    (0..300u32).map(|i| ((i * 37 + 11) % 256) as u8).collect()
}

/// A 1541 at 8 (A) with `a`, a 1581 at 9 (B) with `b`, both on with the machine.
fn booted_mixed(a: Vec<u8>, b: Vec<u8>) -> Machine {
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    m.set_drive_type(DrivePosition::B, DriveType::Drive1581).expect("B is off");
    m.drive_b.set_rom_1581(&rom_1581().unwrap()).unwrap();
    m.drive_b.mount(d81(b)).expect("a D81 fits B");
    m.set_drive_power(DrivePosition::B, true).expect("B on at 9");
    m.drive8.attach_disk(DiskImage { kind: DiskKind::D64, bytes: a, backing_path: None, read_only: false });
    frames(&mut m, 200);
    m
}

fn run_copy(m: &mut Machine, from: u8, to: u8) {
    type_in(m, b"NEW\r");
    frames(m, 10);
    for line in COPY_8_TO_9 {
        let l = String::from_utf8(line.to_vec()).unwrap().replace("2,8,2", &format!("2,{from},2")).replace("3,9,3", &format!("3,{to},3"));
        type_in(m, l.as_bytes());
        frames(m, 5);
    }
    let out = command(m, b"RUN\r", 30_000);
    assert!(out.contains("READY.") && !out.contains("ERROR"), "the copy {from} -> {to} ran:\n{out}");
}

#[test]
fn a_1541_and_a_1581_copy_to_each_other() {
    need_roms!();
    // 8 → 9: from the 1541's D64 to the 1581's D81.
    let a = d64_with(b"SOURCE", &[(b"F", 0x81, seq_data())]);
    let b = D81::new(b"TARGET", *b"TG").bytes;
    let mut m = booted_mixed(a.clone(), b.clone());
    assert_eq!(m.iec.unit_type[9], DriveType::Drive1581, "the IEC core knows the 1581 at 9");
    run_copy(&mut m, 8, 9);
    let b_img = persisted(&mut m, DrivePosition::B);
    let (ft, copied) = read_file(&b_img, b"F").expect("F on the 1581");
    assert_eq!((ft, copied), (0x81, seq_data()), "8 -> 9 is byte-identical");
    bam_consistent(&b_img).expect("the 1581's BAM");
    assert_eq!(persisted(&mut m, DrivePosition::A), a, "the 1541's source image is unchanged");
    assert_no_fast_serial(&m, DrivePosition::B);

    // 9 → 8: from the 1581's D81 to the 1541's D64.
    let a = d64_with(b"TARGET", &[]);
    let b = D81::new(b"SOURCE", *b"SC").add(b"F", 0x81, &seq_data(), (39, 5)).bytes;
    let mut m = booted_mixed(a, b.clone());
    run_copy(&mut m, 9, 8);
    let a_img = persisted(&mut m, DrivePosition::A);
    let (ft, copied) = d64_read_file(&a_img, b"F").expect("F on the 1541");
    assert_eq!((ft, copied), (0x81, seq_data()), "9 -> 8 is byte-identical");
    assert_eq!(persisted(&mut m, DrivePosition::B), b, "the 1581's source image is unchanged");
    assert_no_fast_serial(&m, DrivePosition::B);
}

// ── §9.6 — disk change ──────────────────────────────────────────────────────────

#[test]
fn a_swapped_d81_is_seen_and_no_disk_is_drive_not_ready() {
    need_roms!();
    let x = D81::new(b"DISKX", *b"XX").add(b"ONX", 0x82, &basic_prg(2), (39, 0)).bytes;
    let y = D81::new(b"DISKY", *b"YY").add(b"ONY", 0x82, &basic_prg(2), (41, 0)).bytes;
    let mut m = booted_1581(Some(x));
    let out = load_dir(&mut m, 8);
    assert!(out.contains("DISKX") && out.contains("ONX"), "{out}");
    // Swap while powered.
    m.drive8.detach_disk();
    frames(&mut m, 5);
    m.drive8.mount(d81(y)).unwrap();
    frames(&mut m, 5);
    let out = load_dir(&mut m, 8);
    assert!(out.contains("DISKY") && out.contains("ONY") && !out.contains("ONX"), "the new disk:\n{out}");

    // No disk: the drive fails the job with $03 and reports 74, DRIVE NOT READY.
    m.drive8.detach_disk();
    frames(&mut m, 5);
    type_in(&mut m, b"\x93");
    frames(&mut m, 3);
    type_in(&mut m, b"LOAD\"$\",8\r");
    let mut job3 = false;
    let mut out = String::new();
    for _ in 0..6000 {
        frames(&mut m, 1);
        let ram = m.drive8.ram();
        // The job queue: one job code per buffer at $02-$0A; a finished job leaves
        // its result there.
        if ram[0x02..0x0b].contains(&0x03) {
            job3 = true;
        }
        out = screen(&m);
        if out.contains("READY.") {
            break;
        }
    }
    eprintln!("[§9.6] the C64 prints for LOAD\"$\",8 with no disk:\n{}", out.trim_end());
    assert!(job3, "the drive's job queue never held job code $03");
    let out = error_channel(&mut m, 8);
    assert!(out.contains("74") && out.contains("DRIVE NOT READY"), "error channel:\n{out}");
}

// ── §9.7 — the 870 rules on a 1581 ──────────────────────────────────────────────

#[test]
fn the_870_part_rules_hold_for_a_1581() {
    need_roms!();
    let img = D81::new(b"PARTS", *b"PT").add(b"FILE", 0x82, &basic_prg(2), (39, 0)).bytes;
    let mut m = booted_1581(Some(img.clone()));

    // Off: DEVICE NOT PRESENT.
    m.set_drive_power(DrivePosition::A, false).unwrap();
    assert_eq!(m.iec.drive_slot, None, "off: not on the bus");
    let out = command(&mut m, b"LOAD\"$\",8\r", 600);
    assert!(out.contains("DEVICE NOT PRESENT"), "off:\n{out}");
    // set_drive_type while powered is refused, naming the position — and allowed off.
    m.set_drive_power(DrivePosition::A, true).unwrap();
    let e = m.set_drive_type(DrivePosition::A, DriveType::Drive1541).err().expect("powered");
    assert!(e.contains("position A") && e.contains("switch it off"), "{e}");
    // Off → on is a power-on, the D81 kept.
    frames(&mut m, 200);
    assert!(m.drive8.get_attached_disk().is_some(), "the medium stays");
    assert!(load_dir(&mut m, 8).contains("PARTS"), "after the power-on");

    // Held: nothing on the bus while held; released it is freshly reset.
    m.drive8.set_reset_held(true);
    m.sync_drive_slots();
    assert_eq!(m.iec.drive_slot, None, "held: drives nothing");
    let clk = m.drive8.drive_clk;
    frames(&mut m, 50);
    assert_eq!(m.drive8.drive_clk, clk, "held: no cycle ran");
    m.drive8.set_reset_held(false);
    m.sync_drive_slots();
    assert!(m.drive8.drive_clk < 1000, "released: a fresh reset");
    frames(&mut m, 200);
    assert!(load_dir(&mut m, 8).contains("PARTS"), "after the hold");

    // Stopped mid-transfer for a second: same PC and state on release, the load completes.
    let prg = basic_prg(58);
    let big = D81::new(b"STOP", *b"SP").add(b"BIG", 0x82, &prg, (39, 30)).bytes;
    m.drive8.detach_disk();
    m.drive8.mount(d81(big)).unwrap();
    frames(&mut m, 5);
    type_in(&mut m, b"\x93LOAD\"BIG\",8,1\r");
    frames(&mut m, 120);
    let pc = m.drive8.cpu().reg_pc;
    let clk = m.drive8.drive_clk;
    let head = m.drive8.board_1581().unwrap().head();
    m.drive8.set_stopped(true);
    let bus = m.iec.iecbus.drv_bus[8];
    frames(&mut m, 50);
    assert_eq!((m.drive8.cpu().reg_pc, m.drive8.drive_clk), (pc, clk), "stopped: nothing ran");
    assert_eq!(m.drive8.board_1581().unwrap().head(), head);
    assert_eq!(m.iec.iecbus.drv_bus[8], bus, "stopped: its outputs stand");
    m.drive8.set_stopped(false);
    for _ in 0..6000 {
        frames(&mut m, 1);
        if screen(&m).contains("READY.") {
            break;
        }
    }
    assert_eq!(loaded(&m, &prg), prg[2..], "the transfer completed");

    // The reset line: connected, a C64 warm reset resets the drive; cut, it does not.
    frames(&mut m, 50);
    m.drive8.set_reset_line_connected(false);
    let pc = m.drive8.cpu().reg_pc;
    let ram = m.drive8.ram().to_vec();
    let clk = m.drive8.drive_clk;
    m.warm_reset();
    assert_eq!((m.drive8.cpu().reg_pc, m.drive8.drive_clk), (pc, clk), "cut: the drive carries on");
    assert_eq!(m.drive8.ram(), &ram[..], "cut: RAM as it was");
    m.drive8.set_reset_line_connected(true);
    m.warm_reset();
    assert!(m.drive8.drive_clk < 1000, "connected: the drive reset");
    frames(&mut m, 250);
    assert!(load_dir(&mut m, 8).contains("STOP"), "after the warm reset");

    // ROM: 32 KiB from bytes boots (every test does); 16 KiB and other sizes are refused
    // naming size and type; a ROM given while powered waits for the power-on.
    let rom = rom_1581().unwrap();
    for bad in [0x4000usize, 1000] {
        let e = m.drive8.set_rom(&vec![0u8; bad]).expect_err("wrong size").to_string();
        assert!(e.contains(&bad.to_string()) && e.contains("1581"), "{e}");
        let e = m.drive8.board_1581_mut().unwrap().set_rom(&vec![0u8; bad]).expect_err("wrong size");
        assert!(e.contains(&bad.to_string()) && e.contains("1581"), "{e}");
    }
    let mut marked = rom.clone();
    marked[0] ^= 0xff;
    m.drive8.set_rom(&marked).unwrap();
    assert_eq!(m.drive8.drive_peek(0x8000), rom[0], "given while powered: not yet");
    m.drive8.reset();
    assert_eq!(m.drive8.drive_peek(0x8000), rom[0], "a reset keeps the ROM");
    m.set_drive_power(DrivePosition::A, false).unwrap();
    m.set_drive_power(DrivePosition::A, true).unwrap();
    assert_eq!(m.drive8.drive_peek(0x8000), marked[0], "in force from the power-on");
}


// ── §9.8 — checkpoints ──────────────────────────────────────────────────────────

fn capture(m: &mut Machine) -> serde_json::Value {
    let blob = capture_drive1541(&mut m.drive8);
    let overlay = capture_drive_disk_image(&m.drive8);
    capture_runtime_checkpoint(m, "", "d81", Some(&blob), overlay.as_deref(), None, None)
}

fn through_c64re(cp: &serde_json::Value) -> serde_json::Value {
    let bytes = write_native_snapshot(WriteNativeSnapshotArgs {
        checkpoint: cp.clone(),
        schema_version: 1,
        media: vec![],
        runtime_version: "trx64/872-gate".into(),
        machine_model: "c64-pal".into(),
        provenance: None,
        pc: 0,
        cycle: 0,
    });
    read_native_snapshot(&bytes).expect("read .c64re").checkpoint
}

/// A freshly booted machine the host has given the 1581 DOS, into which `cp` is
/// restored the way the daemon does it: board types first, then the media as written,
/// then the checkpoint.
fn restored(cp: &serde_json::Value, media: &[(DrivePosition, DiskImage)]) -> Machine {
    let mut r = Machine::new();
    r.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let rom = rom_1581().unwrap();
    r.drive8.set_rom_1581(&rom).unwrap();
    r.drive_b.set_rom_1581(&rom).unwrap();
    prepare_drive_types(&mut r, cp);
    for (pos, d) in media {
        r.drive_mut(*pos).mount(d.clone()).expect("the medium fits the restored board");
    }
    restore_runtime_checkpoint(&mut r, cp).expect("restore");
    r
}

/// What differs between two machines that should be the same, briefly; empty = same.
fn state_diff(a: &Machine, b: &Machine) -> String {
    let mut out = Vec::new();
    let first = |x: &[u8], y: &[u8]| x.iter().zip(y).position(|(p, q)| p != q);
    if let Some(i) = first(&a.ram[..], &b.ram[..]) {
        out.push(format!("C64 RAM from ${i:04X}"));
    }
    for pos in [DrivePosition::A, DrivePosition::B] {
        if let Some(i) = first(a.drive(pos).ram(), b.drive(pos).ram()) {
            out.push(format!("drive {pos:?} RAM from ${i:04X}"));
        }
    }
    let clocks = |m: &Machine| (m.c64_core.clk, m.drive8.drive_clk, m.drive_b.drive_clk);
    if clocks(a) != clocks(b) {
        out.push(format!("clocks {:?} vs {:?}", clocks(a), clocks(b)));
    }
    let pcs = |m: &Machine| (m.cpu6510.reg_pc, m.drive8.cpu().reg_pc, m.drive_b.cpu().reg_pc);
    if pcs(a) != pcs(b) {
        out.push(format!("PCs {:04X?} vs {:04X?}", pcs(a), pcs(b)));
    }
    let bus = |m: &Machine| (m.iec.drive_slot, m.iec.drive_slot_b, m.iec.iecbus_callback, m.iec.iecbus.cpu_port, m.iec.iecbus.drv_port);
    if bus(a) != bus(b) {
        out.push(format!("bus {:?} vs {:?}", bus(a), bus(b)));
    }
    out.join("; ")
}

/// The whole 1581 as its checkpoint modules describe it — CPU, RAM, interrupt status,
/// CIA (timers, IFR and SDR delay lines, TOD), WD, FDD and the resident track — taken
/// from a copy, so comparing never touches either machine.
fn board_modules(m: &Machine, pos: DrivePosition) -> Vec<u8> {
    let mut d = m.drive(pos).clone();
    capture_drive1541(&mut d)
}

fn run_until(m: &mut Machine, max: u32, pred: impl Fn(&Machine) -> bool) -> bool {
    for _ in 0..max {
        if pred(m) {
            return true;
        }
        frames(m, 1);
    }
    pred(m)
}

/// Capture now, restore into a fresh machine, and hold the two in cycle-for-cycle
/// lockstep for 500 frames; then run both until the drives are idle and compare the
/// disks they persist.
fn round_trip(m: &mut Machine, what: &str) {
    let media: Vec<(DrivePosition, DiskImage)> = [DrivePosition::A, DrivePosition::B]
        .into_iter()
        .filter_map(|p| m.drive(p).disk_as_written().map(|d| (p, d)))
        .filter(|(p, _)| *p == DrivePosition::A)
        .collect();
    let cp = through_c64re(&capture(m));
    let r = restored(&cp, &media);
    let d = state_diff(&r, m);
    assert!(d.is_empty(), "{what}: restored ≠ captured: {d}");
    for pos in [DrivePosition::A, DrivePosition::B] {
        if m.drive(pos).board_1581().is_some() {
            assert_eq!(board_modules(&r, pos), board_modules(m, pos), "{what}: the 1581 in {pos:?} (CPU, CIA, WD, FDD) restored whole");
        }
    }
    let (mut a, mut b) = (m.clone(), r);
    for f in 1..=500 {
        frames(&mut a, 1);
        frames(&mut b, 1);
        let d = state_diff(&a, &b);
        assert!(d.is_empty(), "{what}: restored and straight apart after frame {f}: {d}");
    }
    for pos in [DrivePosition::A, DrivePosition::B] {
        if a.drive(pos).board_1581().is_some() {
            assert_eq!(board_modules(&a, pos), board_modules(&b, pos), "{what}: the 1581 in {pos:?} after 500 frames");
        }
    }
    for mach in [&mut a, &mut b] {
        run_until(mach, 20_000, |m| screen(m).contains("READY."));
        for pos in [DrivePosition::A, DrivePosition::B] {
            if mach.drive(pos).get_attached_disk().is_some() {
                settle(mach, pos);
            }
        }
    }
    for pos in [DrivePosition::A, DrivePosition::B] {
        if a.drive(pos).get_attached_disk().is_some() {
            assert!(persisted(&mut a, pos) == persisted(&mut b, pos), "{what}: the persisted disk in {pos:?} differs from the straight run's");
        }
    }
}

#[test]
fn a_1581_round_trips_a_checkpoint_mid_load_and_mid_save() {
    need_roms!();
    let (img, prg) = load_image();

    // Mid-LOAD.
    let mut m = booted_1581(Some(img.clone()));
    type_in(&mut m, b"\x93LOAD\"FILE\",8,1\r");
    assert!(run_until(&mut m, 600, |m| m.drive8.board_1581().unwrap().wd().busy), "the WD at work");
    frames(&mut m, 30);
    round_trip(&mut m, "mid-LOAD");
    assert_eq!(loaded(&m, &prg), loaded(&m, &prg));

    // Mid-SAVE, with the resident track written and not yet flushed.
    let mut m = booted_1581(Some(img));
    let out = command(&mut m, b"LOAD\"FILE\",8,1\r", 6000);
    assert!(!out.contains("ERROR"), "{out}");
    type_in(&mut m, b"\x93SAVE\"NEW\",8\r");
    assert!(
        run_until(&mut m, 3000, |m| m.drive8.board_1581().unwrap().wd.fdd.raw.dirty),
        "the resident track goes dirty during the SAVE"
    );
    round_trip(&mut m, "mid-SAVE");
}

#[test]
fn a_1581_beside_a_1541_round_trips_a_checkpoint_mid_copy() {
    need_roms!();
    let a = d64_with(b"SOURCE", &[(b"F", 0x81, seq_data())]);
    let b = D81::new(b"TARGET", *b"TG").bytes;
    let mut m = booted_mixed(a, b);
    type_in(&mut m, b"NEW\r");
    frames(&mut m, 10);
    for line in COPY_8_TO_9 {
        type_in(&mut m, line);
        frames(&mut m, 5);
    }
    type_in(&mut m, b"\x93RUN\r");
    frames(&mut m, 250);
    let cp = through_c64re(&capture(&mut m));
    assert_eq!(cp["driveB"]["drivePart"]["boardType"], 1581, "B rides as a 1581");
    assert_eq!(cp["driveB"]["disk"]["kind"], "d81");
    let a_disk = m.drive8.disk_as_written().unwrap();
    let r = restored(&cp, &[(DrivePosition::A, a_disk)]);
    assert_eq!(r.drive_b.board_type(), DriveType::Drive1581);
    let d = state_diff(&r, &m);
    assert!(d.is_empty(), "restored ≠ captured: {d}");
    assert_eq!(board_modules(&r, DrivePosition::B), board_modules(&m, DrivePosition::B));
    let (mut x, mut y) = (m.clone(), r);
    for f in 1..=500 {
        frames(&mut x, 1);
        frames(&mut y, 1);
        let d = state_diff(&x, &y);
        assert!(d.is_empty(), "apart after frame {f}: {d}");
    }
}

#[test]
fn a_checkpoint_from_before_872_restores_both_positions_as_1541s() {
    need_roms!();
    // A stock two-1541 machine's checkpoint carries no board type.
    let mut old = Machine::new();
    old.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    frames(&mut old, 20);
    let blob = capture_drive1541(&mut old.drive8);
    let cp = through_c64re(&capture_runtime_checkpoint(&old, "", "d64", Some(&blob), None, None, None));
    assert!(cp["drivePart"].get("boardType").is_none(), "a 1541 is not written");
    // Into a machine whose A is a 1581 and whose B is a 1581.
    let mut m = booted_1581(None);
    m.set_drive_type(DrivePosition::B, DriveType::Drive1581).unwrap();
    restore_runtime_checkpoint(&mut m, &cp).expect("restore");
    assert_eq!(m.drive8.board_type(), DriveType::Drive1541, "A back to a 1541");
    assert_eq!(m.drive_b.board_type(), DriveType::Drive1541, "B back to a 1541");
    let d = state_diff(&m, &old);
    assert!(d.is_empty(), "{d}");
}

// ── §9.9 — WD timing as the drive CPU sees it ───────────────────────────────────

/// Drive code at $0500, run by `M-E`: motor on (PA2 low), RESTORE (h = 1, r1r0 = 01),
/// then CIA Timer A loaded with $FFFF and started, SEEK to the track in $0582, wait
/// for BUSY to rise and fall, stop the timer and leave its count at $0580/$0581, put
/// the DOS's Timer A back (latch 6, running). Timer A counts the 2 MHz drive clock.
fn seek_probe_code() -> Vec<u8> {
    let mut c = vec![
        0x78, // SEI
        0xad, 0x00, 0x40, 0x29, 0xfb, 0x8d, 0x00, 0x40, // motor on
        0xa9, 0x09, 0x8d, 0x00, 0x60, // RESTORE h r1r0=01
        0x20, 0x60, 0x05, // JSR wait
        0xad, 0x82, 0x05, 0x8d, 0x03, 0x60, // data = target
        0xa9, 0xff, 0x8d, 0x04, 0x40, 0x8d, 0x05, 0x40, // TA latch $FFFF
        0xa9, 0x11, 0x8d, 0x0e, 0x40, // force load + start
        0xa9, 0x19, 0x8d, 0x00, 0x60, // SEEK h r1r0=01
        0x20, 0x60, 0x05, // JSR wait
        0xa9, 0x00, 0x8d, 0x0e, 0x40, // stop TA
        0xad, 0x04, 0x40, 0x8d, 0x80, 0x05, 0xad, 0x05, 0x40, 0x8d, 0x81, 0x05,
        0xa9, 0x06, 0x8d, 0x04, 0x40, 0xa9, 0x00, 0x8d, 0x05, 0x40, 0xa9, 0x11, 0x8d, 0x0e, 0x40,
        0x58, 0x60, // CLI RTS
    ];
    c.resize(0x60, 0xea);
    c.extend_from_slice(&[
        0xad, 0x00, 0x60, 0x29, 0x01, 0xf0, 0xf9, // BUSY high?
        0xad, 0x00, 0x60, 0x29, 0x01, 0xd0, 0xf9, // BUSY low?
        0x60,
    ]);
    c
}

#[test]
fn a_seek_measured_by_the_drive_through_its_cia_timer() {
    need_roms!();
    let img = D81::new(b"TIMING", *b"TM").bytes;
    let mut m = booted_1581(Some(img));
    let code = seek_probe_code();
    let data: Vec<String> = code.iter().map(|b| b.to_string()).collect();
    let mut prog: Vec<String> = vec![
        format!("10 OPEN15,8,15:FORI=0TO{}:READA", code.len() - 1),
        "20 PRINT#15,\"M-W\"CHR$(I)CHR$(5)CHR$(1)CHR$(A):NEXT".into(),
        "30 K=0:FORT=2TO1STEP-1".into(),
        "40 PRINT#15,\"M-W\"CHR$(130)CHR$(5)CHR$(1)CHR$(T)".into(),
        "50 PRINT#15,\"M-E\"CHR$(0)CHR$(5)".into(),
        "60 PRINT#15,\"M-R\"CHR$(128)CHR$(5)CHR$(2):GET#15,L$:GET#15,H$".into(),
        "70 POKE49152+K,ASC(L$+CHR$(0)):POKE49153+K,ASC(H$+CHR$(0)):K=K+2:NEXT:CLOSE15".into(),
    ];
    for (n, chunk) in data.chunks(14).enumerate() {
        prog.push(format!("{} DATA{}", 100 + n, chunk.join(",")));
    }
    type_in(&mut m, b"NEW\r");
    frames(&mut m, 10);
    for line in &prog {
        type_in(&mut m, format!("{line}\r").as_bytes());
        frames(&mut m, 5);
    }
    m.poke(0xc000, &[0xaa; 4]);
    let out = command(&mut m, b"RUN\r", 6000);
    assert!(out.contains("READY.") && !out.contains("ERROR"), "the probe ran:\n{out}");
    let count = |k: u16| m.read_full(0xc000 + k) as u64 | (m.read_full(0xc001 + k) as u64) << 8;
    let (two, one) = (0xffff - count(0), 0xffff - count(2));
    eprintln!("[§9.9] SEEK 0→2 took {two} drive cycles, 0→1 took {one}; one step = {}", two - one);
    // The step: r1r0 = 01 on the WD1772 is 24 000 cycles (12 ms at 2 MHz). The two
    // seeks differ by one step, give or take where each lands on the byte grid (the
    // controller idles in 64-cycle bytes) and the 9-cycle BUSY poll.
    assert!((two - one).abs_diff(24_000) <= 64 + 9, "one step: {}", two - one);
    // One seek of one step: PREPARE (48) + the step + the idle PREPARE (48) before BUSY
    // falls, less up to a byte of grid alignment, plus the probe's fixed path (the SEEK
    // store, JSR, poll, RTS and the timer stop) — under 64 cycles.
    let over = one as i64 - 24_000 - 96;
    assert!((-64..64).contains(&over), "one step at r1r0 = 01 costs {one} cycles ({over:+} off 24 096)");
}
