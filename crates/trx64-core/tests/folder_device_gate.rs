//! Spec 873 — a folder on the bus.
//!
//! Every test boots the real machine with the real KERNAL (and the real DOS in drive 8)
//! and asks its question through the KERNAL: `LOAD"$",9`, `LOAD`, `SAVE`, `OPEN`/`PRINT#`/
//! `GET#`, the command channel. The folder is a temp directory made here and read back
//! here; nothing pokes the IEC core to make a point.
//!
//!   cargo test --release -p trx64-core --test folder_device_gate -- --nocapture
//!
//! The seven games from a folder (§11.11) and the cost (§11.12) run only when asked
//! (`--ignored`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, restore_runtime_checkpoint};
use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::drive_snapshot::{capture_drive1541, capture_drive_disk_image};
use trx64_core::folder_device::{FolderOpts, FolderSource, HostFolder, TimingProfile};
use trx64_core::iec::IecbusCallback;
use trx64_core::native_snapshot::{read_native_snapshot, write_native_snapshot, WriteNativeSnapshotArgs};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const SAMPLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/samples");
const DOS_ROM: &str = "dos1541-325302-01+901229-05.bin";
const FRAME: u64 = 19_656;

fn roms_present() -> bool {
    let d = Path::new(ROM_DIR);
    d.join("kernal-901227-03.bin").exists() && d.join(DOS_ROM).exists()
}

macro_rules! need_roms {
    () => {
        if !roms_present() {
            eprintln!("[skip] folder_device_gate: ROMs absent at {ROM_DIR}");
            return;
        }
    };
}

// ── a temp folder, removed on drop ──────────────────────────────────────────────────

struct TempFolder(PathBuf);

impl TempFolder {
    fn new(tag: &str) -> TempFolder {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "trx64-873-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempFolder(p)
    }
    fn put(&self, name: &str, bytes: &[u8]) -> &Self {
        let p = self.0.join(name);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(p, bytes).unwrap();
        self
    }
    fn get(&self, name: &str) -> Option<Vec<u8>> {
        std::fs::read(self.0.join(name)).ok()
    }
    fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.0).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        v.sort();
        v
    }
    fn source(&self) -> Arc<dyn FolderSource> {
        Arc::new(HostFolder::new(&self.0).unwrap())
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ── D64 (drive 8's disk) ────────────────────────────────────────────────────────────

fn sectors_per_track(t: u8) -> usize {
    match t {
        1..=17 => 21,
        18..=24 => 19,
        25..=30 => 18,
        _ => 17,
    }
}

fn sector_offset(t: u8, s: u8) -> usize {
    ((1..t).map(sectors_per_track).sum::<usize>() + s as usize) * 256
}

/// A formatted D64 holding one file `name` of type `ftype` at track 17.
fn d64_with(disk_name: &[u8], file: Option<(&[u8], u8, &[u8])>) -> Vec<u8> {
    let mut d = vec![0u8; 174_848];
    let bam = sector_offset(18, 0);
    d[bam] = 18;
    d[bam + 1] = 1;
    d[bam + 2] = 0x41;
    let mut used: Vec<(u8, u8)> = vec![(18, 0), (18, 1)];
    let mut chain = Vec::new();
    if let Some((_, _, data)) = file {
        let n = data.len().div_ceil(254).max(1);
        for i in 0..n {
            chain.push((17u8, i as u8));
            used.push((17, i as u8));
        }
    }
    for t in 1..=35u8 {
        let n = sectors_per_track(t);
        let e = bam + 4 + (t as usize - 1) * 4;
        let mut free: u32 = (1u32 << n) - 1;
        for &(ut, us) in &used {
            if ut == t {
                free &= !(1 << us);
            }
        }
        d[e] = free.count_ones() as u8;
        d[e + 1] = (free & 0xff) as u8;
        d[e + 2] = ((free >> 8) & 0xff) as u8;
        d[e + 3] = ((free >> 16) & 0xff) as u8;
    }
    for i in 0..16 {
        d[bam + 0x90 + i] = *disk_name.get(i).unwrap_or(&0xa0);
    }
    d[bam + 0xa0] = 0xa0;
    d[bam + 0xa1] = 0xa0;
    d[bam + 0xa2] = b'2';
    d[bam + 0xa3] = b'D';
    d[bam + 0xa4] = 0xa0;
    d[bam + 0xa5] = b'2';
    d[bam + 0xa6] = b'A';
    for i in 0xa7..0xab {
        d[bam + i] = 0xa0;
    }
    let dir = sector_offset(18, 1);
    d[dir] = 0x00;
    d[dir + 1] = 0xff;
    if let Some((name, ftype, data)) = file {
        let chunks: Vec<&[u8]> = data.chunks(254).collect();
        for (i, c) in chunks.iter().enumerate() {
            let o = sector_offset(chain[i].0, chain[i].1);
            if let Some(&(nt, ns)) = chain.get(i + 1) {
                d[o] = nt;
                d[o + 1] = ns;
            } else {
                d[o] = 0;
                d[o + 1] = (c.len() + 1) as u8;
            }
            d[o + 2..o + 2 + c.len()].copy_from_slice(c);
        }
        let e = dir;
        d[e + 2] = ftype;
        d[e + 3] = chain[0].0;
        d[e + 4] = chain[0].1;
        for i in 0..16 {
            d[e + 5 + i] = *name.get(i).unwrap_or(&0xa0);
        }
        d[e + 30] = chain.len() as u8;
    }
    d
}

fn disk(bytes: Vec<u8>) -> DiskImage {
    DiskImage { kind: DiskKind::D64, bytes, backing_path: None, read_only: false }
}

/// A tokenized BASIC program of REM lines, linked for `$0801`, as a PRG.
fn basic_prg(lines: usize) -> Vec<u8> {
    let mut prg = vec![0x01, 0x08];
    let mut addr: u16 = 0x0801;
    for i in 0..lines {
        let mut body = Vec::new();
        let num = 10 * (i as u16 + 1);
        body.extend_from_slice(&num.to_le_bytes());
        body.push(0x8f); // REM
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

/// Raw bytes for `$C000`, as a PRG.
fn data_prg(len: usize, seed: u32) -> Vec<u8> {
    let mut v = vec![0x00, 0xc0];
    v.extend((0..len as u32).map(|i| ((i * 31 + seed * 7 + 3) % 256) as u8));
    v
}

// ── the machine through the keyboard ────────────────────────────────────────────────

fn frames(m: &mut Machine, n: u32) {
    let mut sink = NullSink;
    for _ in 0..n {
        m.run_for_full(FRAME, &mut sink, |_, _, _, _, _, _, _| {});
    }
}

/// Booted to READY with `d8` in drive 8 (or drive 8 switched off) and the folder at
/// `unit` when given.
fn booted(d8: Option<Vec<u8>>, folder: Option<(&TempFolder, u8, FolderOpts)>) -> Machine {
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    if d8.is_none() {
        m.drive8.set_power(false);
        m.sync_drive_slots();
    }
    if let Some((f, unit, opts)) = folder {
        m.attach_folder(unit, f.source(), opts).expect("attach folder");
    }
    frames(&mut m, 130);
    if let Some(d) = d8 {
        m.drive8.attach_disk(disk(d));
    }
    frames(&mut m, 40);
    m
}

fn at9(f: &TempFolder) -> Option<(&TempFolder, u8, FolderOpts)> {
    Some((f, 9, FolderOpts::default()))
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
                0x20 => ' ',
                0x21..=0x2f | 0x3a..=0x3f => v as char,
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

/// Clear the screen, type `cmd`, run until the editor prints READY again.
fn command(m: &mut Machine, cmd: &[u8], max_frames: u32) -> String {
    if let Some(rest) = cmd.strip_prefix(b"NEW\r").filter(|r| !r.is_empty()) {
        command(m, b"NEW\r", 100);
        return command(m, rest, max_frames);
    }
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

/// The bytes `LOAD"$",unit` put into BASIC memory (at `$0801`: secondary 0 relocates),
/// and the screen.
fn load_dir_bytes(m: &mut Machine, unit: u8) -> (String, Vec<u8>) {
    let out = command(m, format!("NEW\rLOAD\"$\",{unit}\r").as_bytes(), 1500);
    let end = m.read_full(0x2d) as u16 | (m.read_full(0x2e) as u16) << 8;
    let bytes: Vec<u8> = (0x0801..end).map(|a| m.read_full(a)).collect();
    (out, bytes)
}

fn load_dir(m: &mut Machine, unit: u8) -> String {
    let out = command(m, format!("LOAD\"$\",{unit}\r").as_bytes(), 1500);
    if out.contains("ERROR") {
        return out;
    }
    let list = command(m, b"LIST\r", 200);
    format!("{out}{list}")
}

/// The status channel, read through BASIC and printed as `code*text*track`.
fn status(m: &mut Machine, unit: u8) -> String {
    type_in(m, b"NEW\r");
    frames(m, 5);
    for line in [
        format!("10 OPEN15,{unit},15\r"),
        "20 INPUT#15,A,B$,C,D\r".to_string(),
        "30 PRINTA\"*\"B$\"*\"C\r".to_string(),
        "40 CLOSE15\r".to_string(),
    ] {
        type_in(m, line.as_bytes());
        frames(m, 3);
    }
    let out = command(m, b"RUN\r", 1500);
    let line = out.lines().find(|l| l.contains('*') && !l.contains("PRINT")).unwrap_or("").to_string();
    line.split('*').map(|p| p.trim()).collect::<Vec<_>>().join("*")
}

/// Send a command built by a BASIC string expression down the command channel.
fn send_expr(m: &mut Machine, unit: u8, expr: &str) {
    type_in(m, b"NEW\r");
    frames(m, 5);
    type_in(m, format!("10 OPEN15,{unit},15,{expr}:CLOSE15\r").as_bytes());
    frames(m, 3);
    let out = command(m, b"RUN\r", 1500);
    assert!(!out.contains("ERROR"), "sending {expr}:\n{out}");
}

/// Send `cmd` down the command channel.
fn send_command(m: &mut Machine, unit: u8, cmd: &str) {
    type_in(m, b"NEW\r");
    frames(m, 5);
    type_in(m, format!("10 OPEN15,{unit},15,\"{cmd}\":CLOSE15\r").as_bytes());
    frames(m, 3);
    let out = command(m, b"RUN\r", 1500);
    assert!(!out.contains("ERROR"), "sending {cmd}:\n{out}");
}

// ── §11.1 — directory ───────────────────────────────────────────────────────────────

/// The 1541 listing the spec describes, built here from the folder the test made.
fn expected_listing(header: &str, entries: &[(u16, &[u8], &str, bool)], free: u16) -> Vec<u8> {
    let mut v = vec![1, 1, 0, 0, 0x12, b'"'];
    let h = header.as_bytes();
    let h = &h[h.len().saturating_sub(16)..];
    for i in 0..16 {
        v.push(*h.get(i).unwrap_or(&b' '));
    }
    v.extend_from_slice(b"\" 00 2A\0");
    for &(blocks, name, ty, lock) in entries {
        v.extend_from_slice(&[1, 1, blocks as u8, (blocks >> 8) as u8]);
        let mut text = [b' '; 27];
        let q = 1 + (blocks < 10) as usize + (blocks < 100) as usize;
        text[q] = b'"';
        text[q + 1..q + 1 + name.len()].copy_from_slice(name);
        text[q + 1 + name.len()] = b'"';
        text[q + 19..q + 22].copy_from_slice(ty.as_bytes());
        text[q + 22] = if lock { b'<' } else { b' ' };
        v.extend_from_slice(&text);
        v.push(0);
    }
    v.extend_from_slice(&[1, 1, free as u8, (free >> 8) as u8]);
    v.extend_from_slice(b"BLOCKS FREE.");
    v.extend_from_slice(&[b' '; 13]);
    v.extend_from_slice(&[0, 0, 0]);
    v
}

#[test]
fn directory_lists_the_folder_as_a_1541_would() {
    need_roms!();
    let f = TempFolder::new("dir");
    f.put("game.prg", &vec![1u8; 600]) // 3 blocks
        .put("Notes.SEQ", &[2u8; 10]) // 1 block, SEQ, name mixed case
        .put("title.bin", &vec![3u8; 254 * 12 + 1]) // 13 blocks, PRG with its extension
        .put(".hidden", b"x")
        .put("a_very_long_file_name_indeed.prg", b"yy")
        .put("sub/inner.prg", b"z");
    let mut m = booted(Some(d64_with(b"EIGHT", None)), at9(&f));
    assert_eq!(m.iec.iecbus_callback, IecbusCallback::Conf3, "a folder device puts the bus on conf3");
    let (out, bytes) = load_dir_bytes(&mut m, 9);
    assert!(out.contains("LOADING") && !out.contains("ERROR"), "LOAD\"$\",9:\n{out}");
    let free = (f.source().free_bytes().unwrap() / 254).min(65535) as u16;
    let folder = f.0.file_name().unwrap().to_str().unwrap().to_ascii_uppercase();
    // Sorted by host name bytes: "Notes.SEQ" < "a_very…" < "game.prg" < "sub" < "title.bin".
    let long: Vec<u8> = b"A_VERY_LONG_FILE".iter().map(|&c| if c == b'_' { 0x5f } else { c }).collect();
    let expected = expected_listing(
        &folder,
        &[
            (1, &[0xce, 0x4f, 0x54, 0x45, 0x53], "SEQ", false),
            (1, &long, "PRG", false),
            (3, b"GAME", "PRG", false),
            (0, b"SUB", "DIR", false),
            (13, b"TITLE.BIN", "PRG", false),
        ],
        free,
    );
    // In memory the listing sits at $0801 without its load address, and BASIC has
    // relinked it: compare every byte but the line links. The free count is sampled
    // when the listing is built; the host may have moved since, so it is compared
    // loosely.
    let mut want = expected.clone();
    let mut got = bytes.clone();
    assert_eq!(got.len(), want.len(), "listing length:\n got {}\nwant {}", hex(&got), hex(&want));
    let mut starts = vec![0usize];
    let mut o = 30;
    while o < want.len() {
        starts.push(o);
        o += 32;
    }
    for &st in &starts {
        for k in 0..2 {
            if st + k < want.len() {
                want[st + k] = 0;
                got[st + k] = 0;
            }
        }
    }
    let fr = *starts.last().unwrap() + 2;
    let got_free = got[fr] as i32 | (got[fr + 1] as i32) << 8;
    assert!((got_free - free as i32).abs() <= 64, "BLOCKS FREE {got_free} vs host {free}");
    got[fr] = want[fr];
    got[fr + 1] = want[fr + 1];
    assert_eq!(got, want, "listing bytes differ:\n got {}\nwant {}", hex(&got), hex(&want));
    // And as the user sees it.
    let shown = load_dir(&mut m, 9);
    assert!(shown.contains("GAME") && shown.contains("TITLE.BIN") && shown.contains("DIR") && !shown.contains("HIDDEN"), "{shown}");
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

// ── §11.2 — KERNAL load ─────────────────────────────────────────────────────────────

fn ram(m: &Machine, from: u16, len: usize) -> Vec<u8> {
    (0..len).map(|i| m.read_full(from + i as u16)).collect()
}

#[test]
fn kernal_load_from_the_folder() {
    need_roms!();
    let f = TempFolder::new("load");
    let file = data_prg(3000, 1);
    let other = data_prg(700, 2);
    f.put("file.prg", &file).put("zz_other.prg", &other).put("readme.txt", b"hello");
    let mut m = booted(Some(d64_with(b"EIGHT", None)), at9(&f));
    let out = command(&mut m, b"LOAD\"FILE\",9,1\r", 3000);
    assert!(out.contains("LOADING") && !out.contains("ERROR"), "LOAD\"FILE\",9,1:\n{out}");
    assert_eq!(ram(&m, 0xc000, file.len() - 2), file[2..], "byte-identical to the host file");
    let end = m.read_full(0xae) as u16 | (m.read_full(0xaf) as u16) << 8;
    assert_eq!(end, 0xc000 + (file.len() - 2) as u16, "end of load");

    // No boot file: `*` is the first PRG in listing order — "file.prg".
    m.poke(0xc000, &[0; 16]);
    let out = command(&mut m, b"NEW\rLOAD\"*\",9,1\r", 3000);
    assert!(!out.contains("ERROR"), "{out}");
    assert_eq!(ram(&m, 0xc000, 16), file[2..18], "LOAD\"*\" without a boot file loads the first PRG");

    // A missing name.
    let out = command(&mut m, b"NEW\rLOAD\"NOPE\",9,1\r", 3000);
    assert!(out.contains("FILE NOT FOUND"), "missing name:\n{out}");

    // With a boot file set at attach.
    let mut m = booted(
        Some(d64_with(b"EIGHT", None)),
        Some((&f, 9, FolderOpts { boot: Some("zz_other.prg".into()), ..Default::default() })),
    );
    let out = command(&mut m, b"LOAD\"*\",9,1\r", 3000);
    assert!(!out.contains("ERROR"), "{out}");
    assert_eq!(ram(&m, 0xc000, other.len() - 2), other[2..], "LOAD\"*\" loads the boot file");
    // A raw host file by its name with the extension.
    let out = command(&mut m, b"NEW\rLOAD\"README.TXT\",9,1\r", 3000);
    assert!(!out.contains("ERROR"), "{out}");
}

/// §11.9 — load and save again with VICE's profile.
///
/// With the screen on, VICE's 60 µs half-bit is shorter than the KERNAL's worst poll
/// gap while it waits for the first bit (a 35-cycle loop plus a 43-cycle badline
/// stall, see `timing_against_the_kernal`): a free-running device then shows CLK low
/// for less time than the C64 can be away, the C64 misses bit 0 and takes the byte one
/// bit late. VICE's own device never hits it because it moves one state per C64 `$DD00`
/// access. So VICE's numbers are run here with the display blanked (`$D011` bit 4
/// clear, no badlines) — the condition under which they hold on this clock.
#[test]
fn kernal_load_and_save_with_the_vice_profile() {
    need_roms!();
    let f = TempFolder::new("vice");
    let file = data_prg(2000, 5);
    f.put("file.prg", &file);
    let opts = FolderOpts { profile: TimingProfile::VICE, ..Default::default() };
    let mut m = booted(Some(d64_with(b"EIGHT", None)), Some((&f, 9, opts)));
    m.write_full(0xd011, 0x0b);
    let out = command(&mut m, b"LOAD\"FILE\",9,1\r", 3000);
    assert!(out.contains("LOADING") && !out.contains("ERROR"), "VICE profile LOAD:\n{out}");
    assert_eq!(ram(&m, 0xc000, file.len() - 2), file[2..]);
    save_round(&mut m, &f);
    let (out, _) = load_dir_bytes(&mut m, 9);
    assert!(!out.contains("ERROR"), "VICE profile directory:\n{out}");
}

// ── §11.3 — KERNAL save ─────────────────────────────────────────────────────────────

/// Put a BASIC program in memory the way `LOAD` from drive 8 would, then SAVE it to 9;
/// SAVE over it (63); SAVE with `@0:` (replaces).
fn save_round(m: &mut Machine, f: &TempFolder) {
    let prg = basic_prg(40);
    command(m, b"NEW\r", 100);
    m.poke(0x0801, &prg[2..]);
    let end = 0x0801 + (prg.len() - 2) as u16;
    m.poke(0x2d, &[end as u8, (end >> 8) as u8]);
    m.poke(0x2f, &[end as u8, (end >> 8) as u8]);
    m.poke(0x31, &[end as u8, (end >> 8) as u8]);
    let _ = std::fs::remove_file(f.0.join("new.prg"));
    let out = command(m, b"SAVE\"NEW\",9\r", 6000);
    assert!(out.contains("SAVING") && !out.contains("ERROR"), "SAVE\"NEW\",9:\n{out}");
    assert_eq!(f.get("new.prg").as_deref(), Some(&prg[..]), "new.prg byte-identical after CLOSE");
    assert_eq!(status(m, 9), "0*OK*0", "status after the save");

    // Over an existing name: refused with 63, the file untouched.
    let before = f.get("new.prg");
    let _ = command(m, b"SAVE\"NEW\",9\r", 6000);
    assert_eq!(status(m, 9), "63*FILE EXISTS*0", "SAVE over an existing name");
    assert_eq!(f.get("new.prg"), before, "the existing file is untouched");

    // `@0:` replaces: a different program this time.
    let prg2 = basic_prg(12);
    command(m, b"NEW\r", 100);
    m.poke(0x0801, &prg2[2..]);
    let end = 0x0801 + (prg2.len() - 2) as u16;
    for zp in [0x2d, 0x2f, 0x31] {
        m.poke(zp, &[end as u8, (end >> 8) as u8]);
    }
    let out = command(m, b"SAVE\"@0:NEW\",9\r", 6000);
    assert!(!out.contains("ERROR"), "SAVE\"@0:NEW\",9:\n{out}");
    assert_eq!(f.get("new.prg").as_deref(), Some(&prg2[..]), "@0: replaced new.prg");
}

#[test]
fn kernal_save_to_the_folder() {
    need_roms!();
    let f = TempFolder::new("save");
    let mut m = booted(Some(d64_with(b"EIGHT", None)), at9(&f));
    save_round(&mut m, &f);
    // And it loads back through the KERNAL.
    let out = command(&mut m, b"NEW\rLOAD\"NEW\",9\r", 6000);
    assert!(!out.contains("ERROR"), "{out}");
    assert_eq!(ram(&m, 0x0801, 16), basic_prg(12)[2..18]);
}

// ── §11.4 — the command channel ─────────────────────────────────────────────────────

#[test]
fn the_command_channel() {
    need_roms!();
    let f = TempFolder::new("cmd");
    f.put("one.prg", b"\x01\x08aa").put("two.prg", b"\x01\x08bb").put("keep.seq", b"k");
    let mut m = booted(Some(d64_with(b"EIGHT", None)), at9(&f));
    assert_eq!(status(&mut m, 9), "73*TRX64 FOLDER DOS V1.0*0", "power-on status");
    assert_eq!(status(&mut m, 9), "0*OK*0", "read once, it resets");

    // Scratch with a wildcard and a list.
    send_command(&mut m, 9, "S:ONE,TW*");
    assert_eq!(status(&mut m, 9), "1*FILES SCRATCHED*2", "two scratched");
    assert_eq!(f.names(), vec!["keep.seq"]);
    // Rename.
    send_command(&mut m, 9, "R:KEPT=KEEP");
    assert_eq!(status(&mut m, 9), "0*OK*0");
    assert_eq!(f.names(), vec!["kept.seq"]);
    send_command(&mut m, 9, "R:X=NOTHERE");
    assert_eq!(status(&mut m, 9), "62*FILE NOT FOUND*0");
    f.put("x.prg", b"xx");
    send_command(&mut m, 9, "R:X=KEPT");
    assert_eq!(status(&mut m, 9), "63*FILE EXISTS*0");
    // Directories.
    send_command(&mut m, 9, "MD:SUB");
    assert_eq!(status(&mut m, 9), "0*OK*0");
    assert!(f.0.join("sub").is_dir());
    send_command(&mut m, 9, "MD:SUB");
    assert_eq!(status(&mut m, 9), "63*FILE EXISTS*0");
    send_command(&mut m, 9, "CD:SUB");
    assert_eq!(status(&mut m, 9), "0*OK*0");
    let prg = basic_prg(3);
    command(&mut m, b"NEW\r", 100);
    m.poke(0x0801, &prg[2..]);
    let end = 0x0801 + (prg.len() - 2) as u16;
    for zp in [0x2d, 0x2f, 0x31] {
        m.poke(zp, &[end as u8, (end >> 8) as u8]);
    }
    let out = command(&mut m, b"SAVE\"INSIDE\",9\r", 6000);
    assert!(!out.contains("ERROR"), "{out}");
    assert_eq!(std::fs::read(f.0.join("sub/inside.prg")).ok().as_deref(), Some(&prg[..]), "saved into the subdirectory");
    send_command(&mut m, 9, "CD\x5f");
    assert_eq!(status(&mut m, 9), "0*OK*0", "CD← to the parent");
    send_command(&mut m, 9, "CD\x5f");
    assert_eq!(status(&mut m, 9), "39*SYNTAX ERROR*0", "CD← at the root");
    send_command(&mut m, 9, "CD:NOPE");
    assert_eq!(status(&mut m, 9), "39*SYNTAX ERROR*0");
    send_command(&mut m, 9, "RD:SUB");
    assert_eq!(status(&mut m, 9), "63*FILE EXISTS*0", "RD of a directory that is not empty");
    send_command(&mut m, 9, "CD/SUB/");
    send_command(&mut m, 9, "S:INSIDE");
    send_command(&mut m, 9, "CD//");
    assert_eq!(status(&mut m, 9), "0*OK*0");
    send_command(&mut m, 9, "RD:SUB");
    assert_eq!(status(&mut m, 9), "0*OK*0");
    assert!(!f.0.join("sub").exists());

    // Memory commands are refused and nothing is stored or run; block commands too.
    let names = f.names();
    send_expr(&mut m, 9, "\"M-W\"+CHR$(0)+CHR$(5)+CHR$(3)+\"ABC\"");
    assert_eq!(status(&mut m, 9), "33*SYNTAX ERROR*0", "M-W refused");
    send_expr(&mut m, 9, "\"M-E\"+CHR$(0)+CHR$(5)");
    assert_eq!(status(&mut m, 9), "33*SYNTAX ERROR*0", "M-E refused");
    let ev: Vec<String> = m.folder_mut(9).unwrap().take_events().into_iter().map(|e| e.text).collect();
    assert!(ev.iter().any(|e| e == "refused M-W $0500") && ev.iter().any(|e| e == "refused M-E $0500"), "{ev:?}");
    send_command(&mut m, 9, "B-R 2 0 18 0");
    assert_eq!(status(&mut m, 9), "78*BLOCK ACCESS DENIED*0", "B-R refused");
    send_command(&mut m, 9, "U1 2 0 18 0");
    assert_eq!(status(&mut m, 9), "78*BLOCK ACCESS DENIED*0", "U1 refused");
    send_command(&mut m, 9, "U3");
    assert_eq!(status(&mut m, 9), "33*SYNTAX ERROR*0", "U3 refused");
    send_command(&mut m, 9, "N:DISK,ID");
    assert_eq!(status(&mut m, 9), "31*SYNTAX ERROR*0", "N: refused");
    send_command(&mut m, 9, "P");
    assert_eq!(status(&mut m, 9), "33*SYNTAX ERROR*0", "P: unknown");
    assert_eq!(f.names(), names, "nothing stored on the host");
    // Still usable.
    send_command(&mut m, 9, "UI");
    assert_eq!(status(&mut m, 9), "73*TRX64 FOLDER DOS V1.0*0", "UI answers 73");
    let out = load_dir(&mut m, 9);
    assert!(out.contains("KEPT") && !out.contains("ERROR"), "still usable:\n{out}");
}

// ── §11.5 — SEQ, and a copy from drive 8 ────────────────────────────────────────────

fn enter(m: &mut Machine, lines: &[&[u8]]) {
    type_in(m, b"NEW\r");
    frames(m, 10);
    for line in lines {
        type_in(m, line);
        frames(m, 5);
    }
}

#[test]
fn seq_every_byte_and_a_copy_from_eight() {
    need_roms!();
    let f = TempFolder::new("seq");
    let mut m = booted(Some(d64_with(b"EIGHT", Some((b"F", 0x81, &copy_data())))), at9(&f));
    // Write all 256 byte values with PRINT#, read them back with GET#.
    enter(
        &mut m,
        &[
            b"10 OPEN2,9,2,\"ALL,S,W\"\r",
            b"20 FORI=0TO255:PRINT#2,CHR$(I);:NEXT\r",
            b"30 CLOSE2\r",
            b"40 OPEN2,9,2,\"ALL,S,R\"\r",
            b"50 FORI=0TO255:GET#2,A$:IFA$=\"\"THENA$=CHR$(0)\r",
            b"60 POKE49152+I,ASC(A$):NEXT:S=ST\r",
            b"70 CLOSE2:PRINT\"ST\"S\r",
        ],
    );
    let out = command(&mut m, b"RUN\r", 30_000);
    assert!(out.contains("READY.") && !out.contains("ERROR"), "{out}");
    let all: Vec<u8> = (0..=255u8).collect();
    assert_eq!(f.get("all.seq"), Some(all.clone()), "all.seq on the host");
    assert_eq!(ram(&m, 0xc000, 256), all, "read back with GET#");
    assert!(out.contains("ST 64"), "EOI on the last byte:\n{out}");

    // The 871 copy, 8 → 9.
    enter(
        &mut m,
        &[
            b"10 OPEN2,8,2,\"F,S,R\"\r",
            b"20 OPEN3,9,3,\"F,S,W\"\r",
            b"30 GET#2,A$:S=ST\r",
            b"40 IFA$=\"\"THENA$=CHR$(0)\r",
            b"50 PRINT#3,A$;\r",
            b"60 IFS=0THEN30\r",
            b"70 CLOSE3:CLOSE2\r",
        ],
    );
    let out = command(&mut m, b"RUN\r", 30_000);
    assert!(out.contains("READY.") && !out.contains("ERROR"), "the copy ran:\n{out}");
    assert_eq!(f.get("f.seq"), Some(copy_data()), "byte-identical on the host");
}

fn copy_data() -> Vec<u8> {
    (0..300u32).map(|i| ((i * 37 + 11) % 256) as u8).collect()
}

// ── §11.6 — not present, not in the way ─────────────────────────────────────────────

#[test]
fn not_present_and_not_in_the_way() {
    need_roms!();
    let f = TempFolder::new("away");
    f.put("x.prg", b"\x01\x08x");
    let mut m = booted(Some(d64_with(b"EIGHT", Some((b"ONEIGHT", 0x82, &basic_prg(2))))), None);
    let d8_alone = load_dir_bytes(&mut m, 8).1;
    assert!(load_dir(&mut m, 9).contains("DEVICE NOT PRESENT"), "nothing at 9");
    m.attach_folder(9, f.source(), FolderOpts::default()).unwrap();
    assert!(!load_dir(&mut m, 9).contains("ERROR"), "attached");
    // Drive 8 talks exactly as it did alone, and after every transaction on 8 the
    // device's slot is released.
    let mut worst = 0xffu8;
    let mut sink = NullSink;
    type_in(&mut m, b"\x93LOAD\"$\",8\r");
    for _ in 0..(1500 * 100) {
        m.run_for_full_capped(1_000_000, 1, &mut sink, |_, _, _, _, _, _, _| {});
        let fl = &m.folder(9).unwrap().line;
        if fl.flags == 0 && m.iec.iecbus.cpu_bus & 0x10 != 0 {
            worst &= m.iec.iecbus.drv_bus[9];
        }
        if screen(&m).contains("READY.") && m.read_full(0x00c6) == 0 && fl.flags == 0 {
            break;
        }
    }
    assert_eq!(worst & 0xc0, 0xc0, "the folder's slot held a line while it was not addressed");
    frames(&mut m, 30);
    let end = m.read_full(0x2d) as u16 | (m.read_full(0x2e) as u16) << 8;
    let d8_beside: Vec<u8> = (0x0801..end).map(|a| m.read_full(a)).collect();
    assert_eq!(d8_beside, d8_alone, "LOAD\"$\",8 unchanged with the folder at 9");
    m.detach_folder(9).unwrap();
    assert!(load_dir(&mut m, 9).contains("DEVICE NOT PRESENT"), "detached");
    assert_eq!(m.iec.iecbus_callback, IecbusCallback::Conf1, "back to the one-drive map");
    assert_eq!(m.iec.iecbus.drv_bus[9], 0xff, "slot released");
}

#[test]
fn a_folder_and_a_drive_at_one_unit_are_refused() {
    need_roms!();
    let f = TempFolder::new("refuse");
    let mut m = booted(Some(d64_with(b"EIGHT", None)), None);
    let e = m.attach_folder(8, f.source(), FolderOpts::default()).expect_err("drive A is at 8");
    assert!(e.contains("position A") && e.contains("unit 8"), "{e}");
    assert!(m.attach_folder(12, f.source(), FolderOpts::default()).is_err(), "12 is not offered");
    m.attach_folder(9, f.source(), FolderOpts::default()).unwrap();
    let e = m.attach_folder(9, f.source(), FolderOpts::default()).expect_err("a second at 9");
    assert!(e.contains("unit 9"), "{e}");
    let e = m.set_drive_power(trx64_core::drive::DrivePosition::B, true).expect_err("B at 9 beside the folder");
    assert!(e.contains("folder") && e.contains("unit 9"), "{e}");
    // A folder at 8 with drive A off: the device alone on the bus.
    let mut m = booted(None, Some((&f, 8, FolderOpts::default())));
    assert_eq!(m.iec.iecbus_callback, IecbusCallback::Conf3);
    assert!(!load_dir(&mut m, 8).contains("ERROR"), "folder alone at 8");
}

#[test]
fn read_only_attach_refuses_every_write() {
    need_roms!();
    let f = TempFolder::new("ro");
    f.put("a.prg", b"\x01\x08a");
    let mut m = booted(Some(d64_with(b"EIGHT", None)), Some((&f, 9, FolderOpts { read_only: true, ..Default::default() })));
    status(&mut m, 9);
    for cmd in ["S:A", "R:B=A", "MD:D"] {
        send_command(&mut m, 9, cmd);
        assert_eq!(status(&mut m, 9), "26*WRITE PROTECT ON*0", "{cmd}");
    }
    let _ = command(&mut m, b"SAVE\"NEW\",9\r", 3000);
    assert_eq!(status(&mut m, 9), "26*WRITE PROTECT ON*0", "SAVE");
    assert_eq!(f.names(), vec!["a.prg"]);
}

// ── §11.8 — checkpoints ─────────────────────────────────────────────────────────────

fn capture(m: &mut Machine) -> serde_json::Value {
    let blob = capture_drive1541(&mut m.drive8);
    let overlay = capture_drive_disk_image(&m.drive8);
    capture_runtime_checkpoint(m, "", "d64", Some(&blob), overlay.as_deref(), None, None)
}

fn through_c64re(cp: &serde_json::Value) -> serde_json::Value {
    let bytes = write_native_snapshot(WriteNativeSnapshotArgs {
        checkpoint: cp.clone(),
        schema_version: 1,
        media: vec![],
        runtime_version: "trx64/873-gate".into(),
        machine_model: "c64-pal".into(),
        provenance: None,
        pc: 0,
        cycle: 0,
    });
    read_native_snapshot(&bytes).expect("read .c64re").checkpoint
}

fn state_diff(a: &Machine, b: &Machine) -> String {
    let mut out = Vec::new();
    let first = |x: &[u8], y: &[u8]| x.iter().zip(y).position(|(p, q)| p != q);
    if let Some(i) = first(&a.ram[..], &b.ram[..]) {
        out.push(format!("C64 RAM from ${i:04X}"));
    }
    if let Some(i) = first(a.drive8.ram(), b.drive8.ram()) {
        out.push(format!("drive RAM from ${i:04X}"));
    }
    let clocks = |m: &Machine| (m.c64_core.clk, m.drive8.drive_clk);
    if clocks(a) != clocks(b) {
        out.push(format!("clocks {:?} vs {:?}", clocks(a), clocks(b)));
    }
    let pcs = |m: &Machine| (m.cpu6510.reg_pc, m.drive8.core.reg_pc);
    if pcs(a) != pcs(b) {
        out.push(format!("PCs {:04X?} vs {:04X?}", pcs(a), pcs(b)));
    }
    let dev = |m: &Machine| serde_json::to_value(&m.folders).unwrap();
    if dev(a) != dev(b) {
        out.push(format!("folder device {} vs {}", dev(a), dev(b)));
    }
    if a.iec.iecbus.drv_bus != b.iec.iecbus.drv_bus || a.iec.iecbus.cpu_port != b.iec.iecbus.cpu_port {
        out.push("IEC lines".into());
    }
    out.join("; ")
}

fn restored_from(cp: &serde_json::Value, d8: DiskImage) -> Machine {
    let mut r = Machine::new();
    r.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    r.drive8.attach_disk(d8);
    restore_runtime_checkpoint(&mut r, cp).expect("restore");
    r
}

fn lockstep(m: &Machine, r: &Machine, n: u32) {
    let (mut a, mut b) = (m.clone(), r.clone());
    for fr in 1..=n {
        frames(&mut a, 1);
        frames(&mut b, 1);
        let d = state_diff(&a, &b);
        assert!(d.is_empty(), "restored and straight apart after frame {fr}: {d}");
    }
}

fn finish(m: &mut Machine) {
    for _ in 0..20_000 {
        frames(m, 1);
        if screen(m).contains("READY.") {
            break;
        }
    }
    assert!(!screen(m).contains("ERROR"), "finished cleanly:\n{}", screen(m));
}

#[test]
fn checkpoints_mid_load_and_mid_save() {
    need_roms!();
    let f = TempFolder::new("cp");
    let big = data_prg(3500, 9);
    f.put("big.prg", &big);
    // A large file beside it: whatever its size, the checkpoint holds none of it.
    f.put("huge.bin", &vec![0x5a; 2 << 20]);
    let d8 = d64_with(b"EIGHT", None);

    // Drive 8 lists its disk once first. A 1541 that has not run its motor since its
    // power-on carries a rotation that believes the motor is on (the reset value) while
    // VIA2 says off; a restore re-derives motor and zone from VIA2 (VICE's undump order)
    // and the restored drive then reads SYNC differently from the straight one. That is
    // the drive's checkpoint, not the folder's, and the same without a folder attached.
    let spun = |m: &mut Machine| {
        assert!(!load_dir(m, 8).contains("ERROR"));
    };

    // Mid-LOAD from 9.
    let mut m = booted(Some(d8.clone()), at9(&f));
    spun(&mut m);
    command(&mut m, b"NEW\r", 100);
    type_in(&mut m, b"\x93LOAD\"BIG\",9,1\r");
    frames(&mut m, 120);
    assert!(m.folder(9).unwrap().line.flags & 0x20 != 0, "mid-LOAD: the folder is talking");
    let cp = through_c64re(&capture(&mut m));
    let node = cp.get("folders").expect("the folder rides the checkpoint").to_string();
    assert!(node.len() < 16 * 1024 && !node.contains("5a5a5a5a"), "no file contents in the checkpoint ({} bytes)", node.len());
    let mut r = restored_from(&cp, disk(d8.clone()));
    let d = state_diff(&r, &m);
    assert!(d.is_empty(), "restored ≠ captured: {d}");
    lockstep(&m, &r, 500);
    finish(&mut m);
    finish(&mut r);
    assert_eq!(ram(&m, 0xc000, big.len() - 2), big[2..], "straight run loaded");
    assert_eq!(ram(&r, 0xc000, big.len() - 2), big[2..], "restored run loaded");

    // Mid-SAVE to 9.
    let prg = basic_prg(60);
    let mut m = booted(Some(d8.clone()), at9(&f));
    spun(&mut m);
    let before_save = through_c64re(&capture(&mut m));
    command(&mut m, b"NEW\r", 100);
    m.poke(0x0801, &prg[2..]);
    let end = 0x0801 + (prg.len() - 2) as u16;
    for zp in [0x2d, 0x2f, 0x31] {
        m.poke(zp, &[end as u8, (end >> 8) as u8]);
    }
    type_in(&mut m, b"\x93SAVE\"NEW\",9\r");
    frames(&mut m, 150);
    assert!(m.folder(9).unwrap().line.flags & 0x40 != 0, "mid-SAVE: the folder is listening");
    let cp = through_c64re(&capture(&mut m));
    let mut r = restored_from(&cp, disk(d8.clone()));
    let d = state_diff(&r, &m);
    assert!(d.is_empty(), "restored ≠ captured: {d}");
    lockstep(&m, &r, 500);
    finish(&mut m);
    finish(&mut r);
    assert_eq!(f.get("new.prg").as_deref(), Some(&prg[..]), "the save finished byte-identical");

    // A restore to before the SAVE leaves the host file in place.
    let mut r = restored_from(&before_save, disk(d8));
    frames(&mut r, 10);
    assert_eq!(f.get("new.prg").as_deref(), Some(&prg[..]), "rewinding does not touch the host");
    assert!(load_dir(&mut r, 9).contains("NEW"), "and the rewound machine sees it");
}

#[test]
fn a_checkpoint_without_folders_restores_with_none() {
    need_roms!();
    let f = TempFolder::new("nocp");
    let d8 = d64_with(b"EIGHT", None);
    let mut plain = booted(Some(d8.clone()), None);
    let cp = capture(&mut plain);
    assert!(cp.get("folders").is_none(), "a machine without folders writes no node");
    let mut r = booted(Some(d8), at9(&f));
    restore_runtime_checkpoint(&mut r, &cp).unwrap();
    assert!(r.folders.is_empty());
    assert_eq!(r.iec.iecbus_callback, IecbusCallback::Conf1);
    assert!(load_dir(&mut r, 9).contains("DEVICE NOT PRESENT"));
}

// ── §11.9 — timing against the KERNAL ───────────────────────────────────────────────

/// The margins the KERNAL gives a device, read off the ROM: the instruction bytes are
/// checked, so a different KERNAL fails here instead of passing on numbers it lacks.
struct KernalMargins {
    /// ATN asserted (`$ED33 STA $DD00`) → the DATA sample in `$EEA9` that decides
    /// DEVICE NOT PRESENT: W1MS at `$EEB3` (`LDX #n`, 5-cycle loop) plus the path.
    device_present: u64,
    /// C64 as talker, the last bit sent → no frame acknowledge = error: CIA1 timer B,
    /// `$ED92 LDA #hi` → `$DC07`, low latch `$FF` (the KERNAL never writes `$DC06`
    /// outside the tape code at `$FBB1`).
    frame_ack: u64,
    /// C64 as listener, DATA released → no CLK from the talker = EOI: timer B, `$EE20
    /// LDA #hi`.
    eoi_timeout: u64,
    /// C64 as listener, waiting for the first bit's CLK fall: the longest time between
    /// two samples of the line — the `$EE30` loop (`LDA $DC0D / AND # / BNE / JSR $EEA9
    /// / BMI`, `$EEA9` = `LDA $DD00 / CMP $DD00 / BNE / ASL / RTS`) plus a badline's
    /// 43-cycle stall. A talker's first CLK-low phase shorter than this can be missed.
    first_bit_poll_gap: u64,
}

fn kernal_margins() -> KernalMargins {
    let k = std::fs::read(Path::new(ROM_DIR).join("kernal-901227-03.bin")).unwrap();
    let at = |a: u16| k[(a - 0xe000) as usize];
    assert_eq!((at(0xed33), at(0xed34), at(0xed35)), (0x8d, 0x00, 0xdd), "$ED33 STA $DD00 (ATN)");
    assert_eq!((at(0xeeb3), at(0xeeb4), at(0xeeb6), at(0xeeb7)), (0x8a, 0xa2, 0xca, 0xd0), "$EEB3 W1MS");
    let n = at(0xeeb5) as u64;
    assert_eq!((at(0xed92), at(0xed94), at(0xed95), at(0xed96)), (0xa9, 0x8d, 0x07, 0xdc), "$ED92 LDA # / STA $DC07");
    assert_eq!((at(0xee20), at(0xee22), at(0xee23), at(0xee24)), (0xa9, 0x8d, 0x07, 0xdc), "$EE20 LDA # / STA $DC07");
    // SEI 2, JSR $EE8E 22, JSR $EE97 22, JSR $EEB3 (6+2+2+5n-1+2+6), SEI 2, JSR $EE97 22,
    // JSR $EEA9 6 + LDA $DD00 4.
    let device_present = 2 + 22 + 22 + (17 + 5 * n) + 2 + 22 + 6 + 4;
    assert_eq!(
        (at(0xee30), at(0xee33), at(0xee35), at(0xee37), at(0xee3a)),
        (0xad, 0x29, 0xd0, 0x20, 0x30),
        "$EE30 LDA $DC0D / AND / BNE / JSR / BMI"
    );
    assert_eq!((at(0xeea9), at(0xeeac), at(0xeeaf), at(0xeeb1), at(0xeeb2)), (0xad, 0xcd, 0xd0, 0x0a, 0x60), "$EEA9 DEBPIA");
    let poll_loop = 4 + 2 + 2 + 6 + (4 + 4 + 2 + 2 + 6) + 3;
    KernalMargins {
        first_bit_poll_gap: poll_loop + 43,
        device_present,
        frame_ack: ((at(0xed93) as u64) << 8) | 0xff,
        eoi_timeout: ((at(0xee21) as u64) << 8) | 0xff,
    }
}

/// The device's reaction times, measured instruction by instruction over a LOAD and a
/// SAVE through the real KERNAL: ATN → DATA ("I am here"), the C64's last CLK fall of a
/// byte → frame acknowledge, and ready-for-data → first CLK fall as talker.
fn measure(profile: TimingProfile) -> (u64, u64, u64) {
    let f = TempFolder::new("measure");
    f.put("file.prg", &data_prg(600, 3));
    let mut m = booted(Some(d64_with(b"EIGHT", None)), Some((&f, 9, FolderOpts { profile, ..Default::default() })));
    if profile == TimingProfile::VICE {
        m.write_full(0xd011, 0x0b);
    }
    command(&mut m, b"NEW\r", 100);
    let mut sink = NullSink;
    let (mut atn_ack, mut rx_ack, mut tx_lead) = (0u64, 0u64, 0u64);
    let mut run = |m: &mut Machine, typed: &[u8]| {
        type_in(m, typed);
        let mut atn_fell: Option<u64> = None;
        let mut clk_fell: Option<u64> = None;
        let mut data_rose: Option<u64> = None;
        let mut prev_cpu = m.iec.iecbus.cpu_bus;
        for i in 0..40_000_000u64 {
            m.run_for_full_capped(1_000_000, 1, &mut sink, |_, _, _, _, _, _, _| {});
            let clk = m.c64_core.clk;
            let cpu = m.iec.iecbus.cpu_bus;
            let dev = m.iec.iecbus.drv_bus[9];
            let line = m.folder(9).unwrap().line.clone();
            if prev_cpu & 0x10 != 0 && cpu & 0x10 == 0 {
                atn_fell = Some(clk);
            }
            if let Some(t) = atn_fell {
                if dev & 0x80 == 0 {
                    atn_ack = atn_ack.max(clk - t);
                    atn_fell = None;
                }
            }
            // Listener: the C64 pulls CLK after the 8th bit.
            if prev_cpu & 0x40 != 0 && cpu & 0x40 == 0 && line.flags & 0xc0 != 0 && (line.state == 21 || line.state == 26) {
                clk_fell = Some(clk);
            }
            if let Some(t) = clk_fell {
                if dev & 0x80 == 0 {
                    rx_ack = rx_ack.max(clk - t);
                    clk_fell = None;
                }
            }
            // Talker: the C64 releases DATA (ready-for-data) → the first bit's CLK fall.
            // (Not the deliberate EOI: then the device holds CLK released on purpose,
            // P_EOI, until the C64 has timed out and acknowledged.)
            if line.flags == 0x20 && prev_cpu & 0x80 == 0 && cpu & 0x80 != 0 && dev & 0x40 != 0 {
                data_rose = if line.state == 4 { None } else { Some(clk) };
            }
            if let Some(t) = data_rose {
                if dev & 0x40 == 0 {
                    tx_lead = tx_lead.max(clk - t);
                    data_rose = None;
                }
            }
            prev_cpu = cpu;
            if i % 20_000 == 0 && i > 0 && screen(m).contains("READY.") && m.read_full(0xc6) == 0 && line.flags == 0 {
                break;
            }
        }
    };
    run(&mut m, b"\x93LOAD\"FILE\",9,1\r");
    command(&mut m, b"NEW\r", 100);
    let prg = basic_prg(4);
    m.poke(0x0801, &prg[2..]);
    let end = 0x0801 + (prg.len() - 2) as u16;
    for zp in [0x2d, 0x2f, 0x31] {
        m.poke(zp, &[end as u8, (end >> 8) as u8]);
    }
    run(&mut m, b"\x93SAVE\"OUT\",9\r");
    assert!(f.get("out.prg").is_some(), "the measured SAVE reached the host");
    (atn_ack, rx_ack, tx_lead)
}

#[test]
fn timing_against_the_kernal() {
    need_roms!();
    let k = kernal_margins();
    eprintln!(
        "KERNAL margins (C64 cycles): device present {} ; talker frame-ack wait {} ; listener EOI timeout {}",
        k.device_present, k.frame_ack, k.eoi_timeout
    );
    assert_eq!((k.device_present, k.frame_ack, k.eoi_timeout, k.first_bit_poll_gap), (1017, 1279, 511, 78), "the stock KERNAL's numbers");
    let hz = 985_248.0f64;
    let cyc = |us: u32| (us as f64 * hz / 1e6 + 0.5) as u64;
    eprintln!(
        "first-bit CLK-low phase vs the KERNAL's poll gap {}: ultimate {} cycles, vice {} cycles",
        k.first_bit_poll_gap,
        cyc(TimingProfile::ULTIMATE.t_bit_low),
        cyc(TimingProfile::VICE.t_bit_low)
    );
    assert!(cyc(TimingProfile::ULTIMATE.t_bit_low) > k.first_bit_poll_gap, "the default profile outlasts the poll gap");
    // VICE's 60 µs does not — recorded, see `kernal_load_and_save_with_the_vice_profile`.
    assert!(cyc(TimingProfile::VICE.t_bit_low) < k.first_bit_poll_gap);
    for (name, p) in [("ultimate", TimingProfile::ULTIMATE), ("vice", TimingProfile::VICE)] {
        let (atn_ack, rx_ack, tx_lead) = measure(p);
        eprintln!("{name}: measured worst case ATN→DATA {atn_ack}, frame ack {rx_ack}, ready→first bit {tx_lead} cycles");
        assert!(atn_ack < k.device_present, "{name}: ATN answered in {atn_ack} ≥ {}", k.device_present);
        assert!(rx_ack < k.frame_ack, "{name}: frame acknowledged in {rx_ack} ≥ {}", k.frame_ack);
        assert!(tx_lead < k.eoi_timeout, "{name}: first bit after {tx_lead} ≥ {} would read as EOI", k.eoi_timeout);
    }
}


#[test]
fn checkpoint_with_the_folder_alone_on_the_bus() {
    need_roms!();
    let f = TempFolder::new("alone");
    let big = data_prg(3000, 4);
    f.put("big.prg", &big);
    let mut m = booted(None, Some((&f, 8, FolderOpts::default())));
    command(&mut m, b"NEW\r", 100);
    type_in(&mut m, b"\x93LOAD\"BIG\",8,1\r");
    frames(&mut m, 100);
    assert!(m.folder(8).unwrap().line.flags & 0x20 != 0, "mid-LOAD");
    let cp = through_c64re(&capture(&mut m));
    let mut r = Machine::new();
    r.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    restore_runtime_checkpoint(&mut r, &cp).expect("restore");
    assert!(!r.drive8.powered() && r.folder(8).is_some());
    let d = state_diff(&r, &m);
    assert!(d.is_empty(), "restored ≠ captured: {d}");
    lockstep(&m, &r, 500);
    finish(&mut r);
    assert_eq!(ram(&r, 0xc000, big.len() - 2), big[2..], "restored run loaded");
}
