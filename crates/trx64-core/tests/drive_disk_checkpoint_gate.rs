//! A checkpoint carries the disk as the drive has written it.
//!
//! The C64 SAVEs a file through the real KERNAL and DOS onto a blank disk. Once the
//! drive has written one track and moved its head on to the next, the machine is
//! captured, carried through a `.c64re` container and restored into a freshly booted
//! machine. The first track is then no longer marked dirty anywhere: the head move
//! folded it into the drive's write-back image, and only that image has it.
//!
//! - Restored and persisted at once, the file holds exactly what the captured
//!   machine would have written at that instant — the first track included.
//! - Restored, finished (the SAVE, then a second SAVE), and persisted, the file is
//!   byte for byte the one the machine that was never captured writes.
//! - The capture itself writes nothing to the disk file.
//!
//! D64 and G64 at drive position A (unit 8), and a D64 at position B (unit 9).
//!
//!   cargo test --release -p trx64-core --test drive_disk_checkpoint_gate -- --nocapture

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, restore_runtime_checkpoint};
use trx64_core::drive::{DiskImage, DiskKind, DrivePosition};
use trx64_core::drive_snapshot::{capture_drive1541, capture_drive_disk_image};
use trx64_core::gcr::{GcrImage, WritebackKind};
use trx64_core::native_snapshot::{read_native_snapshot, write_native_snapshot, WriteNativeSnapshotArgs};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const DOS_ROM: &str = "dos1541-325302-01+901229-05.bin";
const FRAME: u64 = 19_656;

fn roms_present() -> bool {
    let d = Path::new(ROM_DIR);
    d.join("kernal-901227-03.bin").exists() && d.join(DOS_ROM).exists()
}

// ── the media ───────────────────────────────────────────────────────────────────

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

/// A blank, formatted 35-track D64: BAM on 18/0, an empty directory on 18/1.
fn blank_d64() -> Vec<u8> {
    let mut d = vec![0u8; 174_848];
    let bam = sector_offset(18, 0);
    d[bam] = 18;
    d[bam + 1] = 1;
    d[bam + 2] = 0x41;
    for t in 1..=35u8 {
        let n = sectors_per_track(t);
        let e = bam + 4 + (t as usize - 1) * 4;
        let used: u32 = if t == 18 { 0b11 } else { 0 };
        let free: u32 = ((1u32 << n) - 1) & !used;
        d[e] = free.count_ones() as u8;
        d[e + 1] = free as u8;
        d[e + 2] = (free >> 8) as u8;
        d[e + 3] = (free >> 16) as u8;
    }
    for i in 0..16 {
        d[bam + 0x90 + i] = *b"CHECKPOINT".get(i).unwrap_or(&0xa0);
    }
    d[bam + 0xa0] = 0xa0;
    d[bam + 0xa1] = 0xa0;
    d[bam + 0xa2] = b'C';
    d[bam + 0xa3] = b'P';
    d[bam + 0xa4] = 0xa0;
    d[bam + 0xa5] = b'2';
    d[bam + 0xa6] = b'A';
    for i in 0xa7..0xab {
        d[bam + i] = 0xa0;
    }
    d[bam + 256 + 1] = 0xff;
    d
}

/// The same disk as a G64: an empty 84-half-track G64 header, then every track of
/// the D64's GCR encoding written into it by the drive's own G64 write-back (which
/// appends a track that has no data block yet).
fn blank_g64() -> Vec<u8> {
    let gcr = GcrImage::from_d64(&blank_d64());
    let mut g = b"GCR-1541".to_vec();
    g.push(0); // version
    g.push(84); // half-tracks
    g.extend_from_slice(&7928u16.to_le_bytes());
    g.resize(12 + 84 * 4 * 2, 0);
    for track in 1..=35usize {
        assert_eq!(gcr.write_half_track(WritebackKind::G64, &mut g, track * 2, false), 0, "G64 track {track}");
    }
    g
}

/// Sector 18/1 — the first directory sector — out of either image.
fn directory(kind: DiskKind, bytes: &[u8]) -> Vec<u8> {
    match kind {
        DiskKind::D64 => bytes[sector_offset(18, 1)..sector_offset(18, 2)].to_vec(),
        DiskKind::G64 => {
            let img = GcrImage::from_g64(bytes);
            let mut s = vec![0u8; 256];
            assert_eq!(trx64_core::gcr::gcr_read_sector(&img.tracks[34], &mut s, 1), trx64_core::gcr::CBMDOS_FDC_ERR_OK, "18/1 decodes");
            s
        }
    }
}

// ── the machine ─────────────────────────────────────────────────────────────────

fn frames(m: &mut Machine, n: u32) {
    let mut sink = NullSink;
    for _ in 0..n {
        m.run_for_full(FRAME, &mut sink, |_, _, _, _, _, _, _| {});
    }
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

/// SAVE `blocks` blocks of patterned memory from $0801 under `name`.
fn start_save(m: &mut Machine, name: &str, unit: u8, blocks: usize, seed: u8) {
    let len = blocks * 254 - 2;
    for i in 0..len {
        m.poke(0x0801 + i as u16, &[(i as u8).wrapping_mul(7).wrapping_add(seed)]);
    }
    let end = 0x0801 + len as u16;
    m.poke(0x002d, &[end as u8, (end >> 8) as u8]);
    type_in(m, format!("SAVE\"{name}\",{unit}\r").as_bytes());
}

#[derive(Clone, Debug)]
struct Case {
    kind: DiskKind,
    pos: DrivePosition,
}

impl Case {
    fn unit(&self) -> u8 {
        if self.pos == DrivePosition::A { 8 } else { 9 }
    }
    fn name(&self) -> String {
        format!("{:?}-{:?}", self.kind, self.pos)
    }
}

fn scratch_file(case: &Case, bytes: &[u8]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("trx64-disk-cp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ext = if matches!(case.kind, DiskKind::G64) { "g64" } else { "d64" };
    let p = dir.join(format!("{}.{ext}", case.name()));
    std::fs::write(&p, bytes).unwrap();
    p
}

/// Booted to READY with the blank disk (backed by `path`) in the case's drive.
fn booted(case: &Case, image: &[u8], path: &Path) -> Machine {
    let disk = DiskImage {
        kind: case.kind.clone(),
        bytes: image.to_vec(),
        backing_path: Some(path.to_string_lossy().into_owned()),
        read_only: false,
    };
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    if case.pos == DrivePosition::B {
        m.drive_b.attach_disk(disk);
        m.set_drive_power(DrivePosition::B, true).expect("B on at 9");
        frames(&mut m, 170);
    } else {
        frames(&mut m, 130);
        m.drive8.attach_disk(disk);
        frames(&mut m, 40);
    }
    m
}

/// Run until the drive has dirtied a second track — the first is then folded into
/// the write-back image by the head move and no longer dirty.
fn run_until_second_track(m: &mut Machine, pos: DrivePosition) -> Vec<u32> {
    let mut seen = BTreeSet::new();
    for _ in 0..4000 {
        frames(m, 1);
        let r = &m.drive(pos).rotation;
        if r.has_dirty_track() {
            seen.insert(r.dirty_half_track);
            if seen.len() >= 2 {
                return seen.into_iter().collect();
            }
        }
    }
    panic!("the drive never dirtied a second track (saw {seen:?})");
}

/// What a persist writes: the drive's pending write flushed, the image taken out.
fn persisted(m: &mut Machine, pos: DrivePosition) -> Vec<u8> {
    let d = m.drive_mut(pos);
    d.flush_disk_writeback();
    d.get_attached_disk().expect("disk attached").bytes.clone()
}

/// A persist to the disk's backing file, as the daemon's `media/persist` does.
fn persist_to_file(m: &mut Machine, pos: DrivePosition) -> Vec<u8> {
    let bytes = persisted(m, pos);
    let path = m.drive(pos).get_attached_disk().unwrap().backing_path.clone().expect("backed");
    std::fs::write(&path, &bytes).unwrap();
    bytes
}

/// The daemon's capture: the drive-8 blobs into the checkpoint, drive 8's medium
/// as the write-back form beside it (the `.c64re` media payload / the ring's
/// `_ringDriveDiskBytes`), position B inside the checkpoint. Through a `.c64re`.
fn capture(m: &mut Machine) -> (serde_json::Value, Option<DiskImage>) {
    let blob = capture_drive1541(&mut m.drive8);
    let overlay = capture_drive_disk_image(&m.drive8);
    let a_disk = m.drive8.disk_as_written();
    let fmt = match a_disk.as_ref().map(|d| &d.kind) {
        Some(DiskKind::G64) => "g64",
        Some(DiskKind::D64) => "d64",
        None => "",
    };
    let cp = capture_runtime_checkpoint(m, "", fmt, Some(&blob), overlay.as_deref(), None, None);
    let bytes = write_native_snapshot(WriteNativeSnapshotArgs {
        checkpoint: cp,
        schema_version: 1,
        media: vec![],
        runtime_version: "trx64/drive-disk-checkpoint-gate".into(),
        machine_model: "c64-pal".into(),
        provenance: None,
        pc: 0,
        cycle: 0,
    });
    (read_native_snapshot(&bytes).expect("read .c64re").checkpoint, a_disk)
}

/// A freshly booted machine with drive 8's medium re-attached (as the daemon does),
/// then the checkpoint restored over it.
fn restored(cp: &serde_json::Value, a_disk: Option<&DiskImage>) -> Machine {
    let mut r = Machine::new();
    r.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    if let Some(d) = a_disk {
        r.drive8.attach_disk(d.clone());
    }
    restore_runtime_checkpoint(&mut r, cp).expect("restore");
    r
}

fn mtime(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p).unwrap().modified().unwrap()
}

fn the_disk_rides_the_checkpoint_whole(case: Case) {
    let blank = if matches!(case.kind, DiskKind::G64) { blank_g64() } else { blank_d64() };
    let path = scratch_file(&case, &blank);
    let pos = case.pos;

    let mut m = booted(&case, &blank, &path);
    start_save(&mut m, "A", case.unit(), 24, 0x11);
    let tracks = run_until_second_track(&mut m, pos);
    eprintln!("[{}] captured with half-tracks {tracks:?} written", case.name());

    // What the captured machine would write to its file at this instant — on a
    // twin, so the machine itself is not flushed.
    let now = persisted(&mut m.clone(), pos);
    assert_ne!(now, blank, "the SAVE has put something on the disk by now");

    let (before, before_t) = (std::fs::read(&path).unwrap(), mtime(&path));
    let (cp, a_disk) = capture(&mut m);
    assert_eq!(std::fs::read(&path).unwrap(), before, "the capture wrote to the disk file");
    assert_eq!(mtime(&path), before_t, "the capture touched the disk file");
    assert_eq!(before, blank, "nothing has persisted the disk yet");

    // Restored and persisted at once: the tracks written before the capture are in.
    let mut r = restored(&cp, a_disk.as_ref());
    assert!(!r.drive(pos).rotation.has_dirty_track(), "a restore marks nothing dirty");
    let at_once = persist_to_file(&mut r, pos);
    assert!(at_once == now, "[{}] restored and persisted at once, the disk lost what was written before the capture", case.name());
    assert!(std::fs::read(&path).unwrap() == now, "the file holds it");

    // Restored, run to the end beside the machine that was never restored, persisted.
    let mut r = restored(&cp, a_disk.as_ref());
    for mm in [&mut m, &mut r] {
        frames(mm, 1500);
        start_save(mm, "B", case.unit(), 4, 0x5a);
        frames(mm, 900);
    }
    let straight = persisted(&mut m, pos);
    let dir = directory(case.kind.clone(), &straight);
    assert_eq!((dir[2] & 0x8f, &dir[5..6]), (0x82, &b"A"[..]), "[{}] file A is on the disk", case.name());
    assert_eq!((dir[34] & 0x8f, &dir[37..38]), (0x82, &b"B"[..]), "[{}] file B is on the disk", case.name());
    let via_restore = persist_to_file(&mut r, pos);
    assert!(
        via_restore == straight,
        "[{}] the restored machine's disk differs from the straight run's (first byte {:?})",
        case.name(),
        via_restore.iter().zip(&straight).position(|(a, b)| a != b)
    );
    assert!(std::fs::read(&path).unwrap() == straight, "the file holds it");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_d64_in_drive_8_rides_the_checkpoint_whole() {
    if !roms_present() {
        eprintln!("[skip] drive_disk_checkpoint_gate: ROMs absent at {ROM_DIR}");
        return;
    }
    the_disk_rides_the_checkpoint_whole(Case { kind: DiskKind::D64, pos: DrivePosition::A });
}

#[test]
fn a_g64_in_drive_8_rides_the_checkpoint_whole() {
    if !roms_present() {
        eprintln!("[skip] drive_disk_checkpoint_gate: ROMs absent at {ROM_DIR}");
        return;
    }
    the_disk_rides_the_checkpoint_whole(Case { kind: DiskKind::G64, pos: DrivePosition::A });
}

#[test]
fn a_d64_in_drive_b_at_unit_9_rides_the_checkpoint_whole() {
    if !roms_present() {
        eprintln!("[skip] drive_disk_checkpoint_gate: ROMs absent at {ROM_DIR}");
        return;
    }
    the_disk_rides_the_checkpoint_whole(Case { kind: DiskKind::D64, pos: DrivePosition::B });
}

#[test]
fn a_g64_in_drive_b_at_unit_9_rides_the_checkpoint_whole() {
    if !roms_present() {
        eprintln!("[skip] drive_disk_checkpoint_gate: ROMs absent at {ROM_DIR}");
        return;
    }
    the_disk_rides_the_checkpoint_whole(Case { kind: DiskKind::G64, pos: DrivePosition::B });
}

/// A head move folds the dirty track into the drive's write-back image; the next
/// flush must bring the drive's `DiskImage` — what a persist writes — up to it even
/// though nothing is dirty any more. It did not: the flush asked only "is a track
/// dirty?", so a track written and then left stayed out of the persisted file until
/// some later write happened to be pending at a flush.
#[test]
fn a_track_the_head_left_reaches_the_persisted_image() {
    let blank = blank_d64();
    let mut m = Machine::new();
    m.drive8.attach_disk(DiskImage { kind: DiskKind::D64, bytes: blank.clone(), backing_path: None, read_only: false });
    let sector: Vec<u8> = (0..256).map(|i| (i as u8) ^ 0xa5).collect();
    {
        let r = &mut m.drive8.rotation;
        let ht = r.current_half_track as usize;
        assert_eq!(ht, 36, "parked on track 18");
        let img = r.image.as_mut().unwrap();
        assert_eq!(trx64_core::gcr::gcr_write_sector(&mut img.tracks[ht - 2], &sector, 5), trx64_core::gcr::CBMDOS_FDC_ERR_OK);
        r.write_one_bit_for_test(1);
        assert!(r.has_dirty_track());
        r.move_head(2); // to track 19: track 18 is folded in, nothing is dirty
        assert!(!r.has_dirty_track());
    }
    let as_written = m.drive8.disk_as_written().unwrap().bytes;
    assert_eq!(&as_written[sector_offset(18, 5)..sector_offset(18, 6)], &sector[..], "the write-back image has it");
    assert!(m.drive8.flush_disk_writeback(), "the flush brings the disk image up to date");
    let persisted = &m.drive8.get_attached_disk().unwrap().bytes;
    assert_eq!(&persisted[sector_offset(18, 5)..sector_offset(18, 6)], &sector[..], "a persist writes it");
    assert!(!m.drive8.flush_disk_writeback(), "and only once");
}
