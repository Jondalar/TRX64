//! The drive CPU's interrupt status rides the checkpoint.
//!
//! A machine in the middle of a KERNAL file load (real KERNAL, real DOS) is captured,
//! carried through a `.c64re` container, restored into a freshly booted machine, and
//! then the machine that was captured and the restored one run side by side for 500
//! frames. They must stay the same machine cycle for cycle: after every frame the C64
//! clock and PC and every powered drive's clock and PC are equal, and at the end the
//! C64 RAM and the drive RAMs are. Once with one drive, once with two (the load then
//! comes from position B at unit 9, drive 8 idle beside it).
//!
//! What it takes, each shown red by taking it out: the VIAs' IRQ levels restored
//! into the drive's interrupt status (VICE's `restore_int` → `interrupt_restore_irq`;
//! a no-op here before, one drive apart after frame 4), the head placed before the
//! VIA read so the undump's rotation advances it (two drives apart after frame 26),
//! and the rotation engine carried in GCRIMAGE0 instead of forced to the GCR circuit
//! (one drive apart after frame 1). The DRIVECPU interrupt block (irq_clk,
//! irq_pending_clk, nirq, global_pending_int, …) is written and read too, as VICE
//! does; no capture instant tried here needs it — with it left unread the gate stays
//! green (the status re-derived from the VIA levels gives the same machine).
//!
//! Older blobs still restore: DRIVECPU minor 3 (no interrupt block) with the drive's
//! interrupt status reset, GCRIMAGE minor 1 on the GCR circuit engine (VICE's rule).
//!
//!   cargo test --release -p trx64-core --test drive_int_snapshot_gate -- --nocapture

use std::path::Path;

use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, restore_runtime_checkpoint};
use trx64_core::drive::{DiskImage, DiskKind, DrivePosition};
use trx64_core::drive_snapshot::{capture_drive1541, capture_drive_disk_image, restore_drive1541};
use trx64_core::native_snapshot::{read_native_snapshot, write_native_snapshot, WriteNativeSnapshotArgs};
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const DOS_ROM: &str = "dos1541-325302-01+901229-05.bin";
const FRAME: u64 = 19_656;
const LOCKSTEP_FRAMES: u32 = 500;

fn roms_present() -> bool {
    let d = Path::new(ROM_DIR);
    d.join("kernal-901227-03.bin").exists() && d.join(DOS_ROM).exists()
}

macro_rules! need_roms {
    () => {
        if !roms_present() {
            eprintln!("[skip] drive_int_snapshot_gate: ROMs absent at {ROM_DIR}");
            return;
        }
    };
}

// ── a D64 with one long PRG ─────────────────────────────────────────────────────

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

/// A formatted 35-track D64 holding the PRG "BIG": load address $2000, `blocks`
/// sectors of patterned data, laid down on track 17 onwards (sector interleave 1 —
/// the DOS does not care, it follows the chain). The BAM is left all-free apart from
/// track 18; nothing here writes to the disk.
fn disk_with_big_prg(blocks: usize) -> Vec<u8> {
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
        d[bam + 0x90 + i] = *b"INTGATE".get(i).unwrap_or(&0xa0);
    }
    d[bam + 0xa2] = b'2';
    d[bam + 0xa3] = b'D';
    d[bam + 0xa5] = b'2';
    d[bam + 0xa6] = b'A';
    // The data sectors: track 17 down, then 16, … (each track fully).
    let mut secs = Vec::new();
    let mut t = 17u8;
    while secs.len() < blocks {
        for s in 0..sectors_per_track(t) as u8 {
            if secs.len() < blocks {
                secs.push((t, s));
            }
        }
        t -= 1;
    }
    let mut data = vec![0x00, 0x20];
    data.extend((0..blocks * 254 - 2).map(|i| (i * 37 + i / 251) as u8));
    for (i, chunk) in data.chunks(254).enumerate() {
        let o = sector_offset(secs[i].0, secs[i].1);
        if let Some(&(nt, ns)) = secs.get(i + 1) {
            d[o] = nt;
            d[o + 1] = ns;
        } else {
            d[o] = 0;
            d[o + 1] = (chunk.len() + 1) as u8;
        }
        d[o + 2..o + 2 + chunk.len()].copy_from_slice(chunk);
    }
    let dir = sector_offset(18, 1);
    d[dir + 1] = 0xff;
    d[dir + 2] = 0x82;
    d[dir + 3] = secs[0].0;
    d[dir + 4] = secs[0].1;
    for i in 0..16 {
        d[dir + 5 + i] = *b"BIG".get(i).unwrap_or(&0xa0);
    }
    d[dir + 30] = blocks as u8;
    d
}

fn disk(bytes: Vec<u8>) -> DiskImage {
    DiskImage { kind: DiskKind::D64, bytes, backing_path: None, read_only: false }
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

/// Booted to READY with the load disk in drive 8 — or, `two`, in drive B at unit 9
/// (switched on with the C64) and a second copy in drive 8 — then `LOAD"BIG",<unit>,1`
/// typed and run until the transfer is well under way.
fn mid_load(two: bool) -> (Machine, Vec<u8>) {
    let img = disk_with_big_prg(60);
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    if two {
        m.drive_b.attach_disk(disk(img.clone()));
        m.set_drive_power(DrivePosition::B, true).expect("B on at 9");
    }
    frames(&mut m, 130);
    m.drive8.attach_disk(disk(img.clone()));
    frames(&mut m, 40);
    let unit = if two { 9 } else { 8 };
    type_in(&mut m, format!("LOAD\"BIG\",{unit},1\r").as_bytes());
    frames(&mut m, 250);
    let loading = if two { &m.drive_b } else { &m.drive8 };
    assert!(loading.ports().motor_on, "mid-transfer: the loading drive's motor runs");
    (m, img)
}

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
        runtime_version: "trx64/drive-int-gate".into(),
        machine_model: "c64-pal".into(),
        provenance: None,
        pc: 0,
        cycle: 0,
    });
    read_native_snapshot(&bytes).expect("read .c64re").checkpoint
}

/// A freshly booted machine with drive 8's medium re-attached (as the daemon does),
/// then the checkpoint restored over it.
fn restored(cp: &serde_json::Value, a_disk: Vec<u8>) -> Machine {
    let mut r = Machine::new();
    r.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    r.drive8.attach_disk(disk(a_disk));
    restore_runtime_checkpoint(&mut r, cp).expect("restore");
    r
}

/// (C64 clk, C64 PC, [(drive clk, drive PC) per powered position]).
type Beat = (u64, u16, Vec<(u64, u16)>);

fn beat(m: &Machine) -> Beat {
    let mut drives = vec![(m.drive8.core.clk, m.drive8.core.reg_pc)];
    if m.drive_b.powered() {
        drives.push((m.drive_b.core.clk, m.drive_b.core.reg_pc));
    }
    (m.c64_core.clk, m.cpu6510.reg_pc, drives)
}

/// Run both `LOCKSTEP_FRAMES` frames; `Err` names the first frame they part.
fn lockstep(a: &mut Machine, b: &mut Machine) -> Result<(), String> {
    let (ba, bb) = (beat(a), beat(b));
    if ba != bb {
        return Err(format!("already apart at the restore: {ba:?} vs {bb:?}"));
    }
    for f in 1..=LOCKSTEP_FRAMES {
        frames(a, 1);
        frames(b, 1);
        let (ba, bb) = (beat(a), beat(b));
        if ba != bb {
            return Err(format!("apart after frame {f}: straight {ba:?} vs restored {bb:?}"));
        }
    }
    let first = |x: &[u8], y: &[u8]| x.iter().zip(y).position(|(p, q)| p != q);
    if let Some(i) = first(&a.ram[..], &b.ram[..]) {
        return Err(format!("C64 RAM differs from ${i:04X} after {LOCKSTEP_FRAMES} frames"));
    }
    if let Some(i) = first(a.drive8.ram(), b.drive8.ram()) {
        return Err(format!("drive 8 RAM differs from ${i:04X} after {LOCKSTEP_FRAMES} frames"));
    }
    if let Some(i) = first(a.drive_b.ram(), b.drive_b.ram()) {
        return Err(format!("drive B RAM differs from ${i:04X} after {LOCKSTEP_FRAMES} frames"));
    }
    Ok(())
}

fn round_trip_runs_in_lockstep(two: bool) {
    let (mut m, img) = mid_load(two);
    let cp = through_c64re(&capture(&mut m));
    assert_eq!(cp.get("driveB").is_some(), two, "B rides the checkpoint exactly when on");
    let mut r = restored(&cp, img);
    let loading = if two { &r.drive_b } else { &r.drive8 };
    assert!(loading.ports().motor_on, "the restored machine is mid-transfer too");
    if let Err(e) = lockstep(&mut m, &mut r) {
        panic!("{} drive(s): the restored machine left the straight run — {e}", if two { 2 } else { 1 });
    }
}

#[test]
fn one_drive_mid_load_restores_cycle_for_cycle() {
    need_roms!();
    round_trip_runs_in_lockstep(false);
}

#[test]
fn two_drives_mid_load_restore_cycle_for_cycle() {
    need_roms!();
    round_trip_runs_in_lockstep(true);
}

// ── a blob from before the interrupt block ─────────────────────────────────────

/// Rewrite a `drive1541` blob as a DRIVECPU 1.3 writer made it: the module's minor
/// back to 3 and the trailing interrupt block (5 clocks + 3 dwords = 52 bytes) cut,
/// the module size patched to match. Module frame: 16-byte name, major, minor,
/// dword size (which counts the 22-byte header).
fn as_drivecpu_1_3(blob: &[u8]) -> Vec<u8> {
    const INT_BLOCK: usize = 5 * 8 + 3 * 4;
    let name = b"DRIVECPU0";
    let pos = blob
        .windows(16)
        .position(|w| w.starts_with(name) && w[name.len()..].iter().all(|&b| b == 0))
        .expect("DRIVECPU0 module in the blob");
    assert_eq!((blob[pos + 16], blob[pos + 17]), (1, 4), "the blob is DRIVECPU 1.4");
    let size = u32::from_le_bytes(blob[pos + 18..pos + 22].try_into().unwrap()) as usize;
    let end = pos + size;
    let mut out = blob[..end - INT_BLOCK].to_vec();
    out.extend_from_slice(&blob[end..]);
    out[pos + 17] = 3;
    out[pos + 18..pos + 22].copy_from_slice(&((size - INT_BLOCK) as u32).to_le_bytes());
    out
}

#[test]
fn a_drivecpu_1_3_blob_restores_with_the_interrupt_status_reset() {
    need_roms!();
    let (mut m, img) = mid_load(false);
    let new_blob = capture_drive1541(&mut m.drive8.clone());
    let old_blob = as_drivecpu_1_3(&new_blob);

    // The drive side of the restore takes it, and lands on the captured CPU.
    let mut d = m.drive8.clone();
    restore_drive1541(&mut d, &old_blob).expect("a 1.3 blob restores");
    assert_eq!((d.core.clk, d.core.reg_pc), (m.drive8.core.clk, m.drive8.core.reg_pc));
    assert_eq!(d.ram(), m.drive8.ram());
    let int = &d.int;
    assert_eq!(int.nirq, 0, "reset: no source counted");
    assert_eq!(int.pending_int, [0, 0], "reset: no source pending");
    assert_eq!(int.nnmi, 0);
    assert_eq!(int.irq_pending_clk, u64::MAX, "reset: no IRQ tail armed");

    // A whole checkpoint carrying the old blob restores, and the machine goes on
    // loading: the transfer finishes and the file is in memory.
    let mut cp = capture(&mut m);
    cp.as_object_mut().unwrap().insert("drive1541".into(), trx64_core::native_snapshot::ta_u8(&old_blob));
    let mut r = restored(&cp, img.clone());
    let mut done = false;
    for _ in 0..3000 {
        frames(&mut r, 1);
        if (0xE5CD..=0xE5D4).contains(&r.cpu6510.reg_pc) && r.read_full(0x00c6) == 0 && !r.drive8.ports().motor_on {
            done = true;
            break;
        }
    }
    assert!(done, "the load finished after restoring a 1.3 blob");
    let want: Vec<u8> = (0..60 * 254 - 2).map(|i: usize| (i * 37 + i / 251) as u8).collect();
    let got: Vec<u8> = (0..want.len()).map(|i| r.read_full(0x2000 + i as u16)).collect();
    assert!(got == want, "the file arrived whole at $2000");
}

/// Rewrite a `driveDiskImage` blob as a GCRIMAGE 3.1 writer made it: minor back to 1,
/// the trailing engine byte cut.
fn as_gcrimage_3_1(blob: &[u8]) -> Vec<u8> {
    let name = b"GCRIMAGE0";
    let pos = blob
        .windows(16)
        .position(|w| w.starts_with(name) && w[name.len()..].iter().all(|&b| b == 0))
        .expect("GCRIMAGE0 module in the blob");
    assert_eq!((blob[pos + 16], blob[pos + 17]), (3, 2), "the blob is GCRIMAGE 3.2");
    let size = u32::from_le_bytes(blob[pos + 18..pos + 22].try_into().unwrap()) as usize;
    let end = pos + size;
    let mut out = blob[..end - 1].to_vec();
    out.extend_from_slice(&blob[end..]);
    out[pos + 17] = 1;
    out[pos + 18..pos + 22].copy_from_slice(&((size - 1) as u32).to_le_bytes());
    out
}

#[test]
fn the_rotation_engine_rides_gcrimage_and_a_3_1_blob_gets_the_circuit() {
    use trx64_core::drive_snapshot::restore_drive_disk_image;
    need_roms!();
    let (m, _img) = mid_load(false);
    assert_eq!(m.drive8.rotation.complicated_image_loaded, 0, "a D64 that never wrote runs the simple engine");
    let blob = capture_drive_disk_image(&m.drive8).expect("an image is loaded");
    let mut d = m.drive8.clone();
    d.rotation.complicated_image_loaded = 1;
    restore_drive_disk_image(&mut d, &blob).expect("3.2 restores");
    assert_eq!(d.rotation.complicated_image_loaded, 0, "3.2 carries the simple engine");
    let mut d = m.drive8.clone();
    restore_drive_disk_image(&mut d, &as_gcrimage_3_1(&blob)).expect("3.1 restores");
    assert_eq!(d.rotation.complicated_image_loaded, 1, "3.1 gets VICE's circuit engine");
}

/// The capture brings each drive's lazy rotation up to its clock. That must not move
/// the machine: a machine that was captured and its uncaptured twin stay in lockstep.
#[test]
fn a_capture_leaves_the_machine_it_describes_alone() {
    need_roms!();
    for two in [false, true] {
        let (mut m, _img) = mid_load(two);
        let mut twin = m.clone();
        let _ = capture(&mut m);
        if let Err(e) = lockstep(&mut twin, &mut m) {
            panic!("{} drive(s): the captured machine left its twin — {e}", if two { 2 } else { 1 });
        }
    }
}
