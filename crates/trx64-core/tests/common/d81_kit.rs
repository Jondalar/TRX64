// Spec 875 — the D81 kit of drive1581_gate.rs (Spec 872), copied so that gate stays
// unchanged: build and read back D81s, drive the machine through the keyboard.
// Pulled into fdc_controller_gate.rs with include!().

use std::path::{Path, PathBuf};

use trx64_core::drive::{DiskImage, DiskKind, DrivePosition};
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

// ── the images of 872 §9.1 and §9.2 ────────────────────────────────────────────────────────────

fn directory_image() -> Vec<u8> {
    D81::new(b"THE1581DISK", *b"81")
        .add(b"FIRST", 0x82, &basic_prg(3), (39, 0))
        .add(b"SECOND FILE", 0x81, &(0..700u32).map(|i| i as u8).collect::<Vec<_>>(), (41, 0))
        .add(b"THIRD", 0x82, &basic_prg(40), (80, 10))
        .bytes
}

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

