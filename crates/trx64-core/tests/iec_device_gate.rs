//! iec_device_gate.rs — Spec 874 §12: a device of the host's own on the IEC bus.
//!
//! Every test puts `ProbeListener` (tests/common/probe_listener.rs) on the bus through
//! the public `IecDevice` trait, the way UE2 puts the Ultimate's IEC processor there.
//! The machine code the C64 runs is poked to `$C000` and started with `SYS 49152`; it
//! sets `$C0FF` when it is done.
//!
//! Run: `cargo test --release -p trx64-core --test iec_device_gate`
//! (the cost characterisation: `-- --ignored --nocapture`).

#[path = "common/probe_listener.rs"]
mod probe_listener;

use std::path::Path;

use probe_listener::{Ev, ProbeListener};
use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, restore_runtime_checkpoint};
use trx64_core::drive::DrivePosition;
use trx64_core::expansion::Hold;
use trx64_core::iec_device::{IecLines, IecOut};
use trx64_core::{BusKind, Machine, NullSink, Observer};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const SAMPLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/samples");
const FRAME: u64 = 19_656;

fn roms_present() -> bool {
    let d = Path::new(ROM_DIR);
    d.join("kernal-901227-03.bin").exists()
        && (d.join("dos1541-325302-01+901229-05.bin").exists() || d.join("1541.bin").exists())
}

macro_rules! need_roms {
    () => {
        if !roms_present() {
            eprintln!("skip: ROMs absent ({ROM_DIR})");
            return;
        }
    };
}

// ── the machine ─────────────────────────────────────────────────────────────────────

fn frames(m: &mut Machine, n: u32) {
    let mut sink = NullSink;
    for _ in 0..n {
        m.run_for_full(FRAME, &mut sink, |_, _, _, _, _, _, _| {});
    }
}

/// Booted to READY; drive 8 off unless `drive8`.
fn booted(drive8: bool) -> Machine {
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    if !drive8 {
        m.drive8.set_power(false);
        m.sync_drive_slots();
    }
    frames(&mut m, 130);
    m
}

fn probe(m: &Machine, slot: u8) -> &ProbeListener {
    m.iec_device_as::<ProbeListener>(slot).expect("the probe")
}

fn probe_mut(m: &mut Machine, slot: u8) -> &mut ProbeListener {
    m.iec_device_as_mut::<ProbeListener>(slot).expect("the probe")
}

/// Poke `code` to `$C000`, clear the done flag, type `SYS49152`.
fn start(m: &mut Machine, code: &[u8]) {
    m.poke(0xc000, code);
    m.poke(0xc0ff, &[0]);
    for (i, b) in b"SYS49152\r".iter().enumerate() {
        m.poke(0x0277 + i as u16, &[*b]);
    }
    m.poke(0x00c6, &[9]);
}

/// Run until the program sets `$C0FF`, observing with `obs`.
fn run_until_done<O: Observer>(m: &mut Machine, obs: &mut O) {
    for _ in 0..2000 {
        m.run_for_full(2_000, obs, |_, _, _, _, _, _, _| {});
        if m.read_full(0xc0ff) != 0 {
            return;
        }
    }
    panic!("the program at $C000 did not finish");
}

/// Every `$DD00` access and every retired instruction, with its cycle.
#[derive(Default)]
struct Dd00 {
    reads: Vec<(u64, u8)>,
    writes: Vec<(u64, u8)>,
    ddr_writes: Vec<(u64, u8)>,
}

impl Observer for Dd00 {
    fn on_instruction(&mut self, _: u16, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u8, _: u64) {}
    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, _pc: u16, clk: u64, _old: u8) {
        match (kind, addr) {
            // A `$DD00` read is reported twice at its cycle (once among the side-effect
            // reads, then as the read); the last report carries what the CPU got.
            (BusKind::Read, 0xdd00) => match self.reads.last_mut() {
                Some(last) if last.0 == clk => last.1 = value,
                _ => self.reads.push((clk, value)),
            },
            (BusKind::Write, 0xdd00) => self.writes.push((clk, value)),
            (BusKind::Write, 0xdd02) => self.ddr_writes.push((clk, value)),
            _ => {}
        }
    }
    fn on_interrupt(&mut self, _: u16, _: u64) {}
}

const LDA_ABS: u8 = 0xad;
const STA_ABS: u8 = 0x8d;
const STA_ABX: u8 = 0x9d;

// ── §12.2 — what the C64 sees is cycle-exact ────────────────────────────────────────

/// `SEI`, then 256 × (`LDA $DD00` / `STA $C100,X` / `INX` / `BNE`), done.
fn read_loop() -> Vec<u8> {
    vec![
        0x78, // SEI
        0xa2, 0x00, // LDX #0
        LDA_ABS, 0x00, 0xdd, // loop: LDA $DD00
        STA_ABX, 0x00, 0xc1, // STA $C100,X
        0xe8, // INX
        0xd0, 0xf7, // BNE loop
        0xee, 0xff, 0xc0, // INC $C0FF
        0x58, // CLI
        0x60, // RTS
    ]
}

/// For each release cycle `P` over one loop pass: the first read that sees DATA high is
/// the first read at or after `P`, and every read was a sync at its own cycle.
fn cycle_exact_at(slot: u8, unit: Option<u8>) {
    let mut base = booted(false);
    let mut p = ProbeListener::new("probe", unit);
    p.clonable = true;
    p.hold_data_until = Some(u64::MAX);
    base.attach_iec_device(slot, Box::new(p)).expect("attach the probe");
    start(&mut base, &read_loop());

    // A dry run for the read cycles.
    let reads = {
        let mut m = base.clone();
        let mut obs = Dd00::default();
        run_until_done(&mut m, &mut obs);
        assert_eq!(obs.reads.len(), 256, "one read per pass");
        assert!(obs.reads.iter().all(|&(_, v)| v & 0x80 == 0), "DATA held low throughout");
        obs.reads
    };
    // One pass: the cycles after read 100 up to and including read 101.
    let (r0, r1) = (reads[100].0, reads[101].0);
    assert!(r1 - r0 >= 14, "one pass is at least 14 cycles");
    for release in (r0 + 1)..=r1 {
        let mut m = base.clone();
        probe_mut(&mut m, slot).hold_data_until = Some(release);
        let mut obs = Dd00::default();
        run_until_done(&mut m, &mut obs);
        assert_eq!(obs.reads.iter().map(|r| r.0).collect::<Vec<_>>(), reads.iter().map(|r| r.0).collect::<Vec<_>>());
        let first_high = obs.reads.iter().position(|&(_, v)| v & 0x80 != 0).expect("DATA released");
        let first_at_or_after = obs.reads.iter().position(|&(c, _)| c >= release).unwrap();
        assert_eq!(
            first_high, first_at_or_after,
            "slot {slot}: DATA released at {release}: first read high #{first_high} at {}, first read at/after #{first_at_or_after}",
            obs.reads[first_high].0
        );
        // Every read was preceded by a `clock_to` at its own cycle.
        let clocks: std::collections::HashSet<u64> = probe(&m, slot).clocks().iter().map(|c| c.0).collect();
        for &(c, _) in &obs.reads {
            assert!(clocks.contains(&c), "slot {slot}: no clock_to at the read cycle {c}");
        }
    }
    eprintln!("slot {slot}: release swept over cycles {}..={r1} ({} cycles, one pass)", r0 + 1, r1 - r0);
}

#[test]
fn cycle_exact_at_dd00_slot_4() {
    need_roms!();
    cycle_exact_at(4, None);
}

#[test]
fn cycle_exact_at_dd00_slot_9() {
    need_roms!();
    cycle_exact_at(9, Some(9));
}

// ── §12.3 — the ATN edge at its cycle ───────────────────────────────────────────────

/// ATN by `$DD00` (assert, read, release, read), then by `$DD02` (assert, read,
/// release, read). Reads to `$C100`-`$C103`.
fn atn_program() -> Vec<u8> {
    vec![
        0x78, // SEI
        LDA_ABS, 0x00, 0xdd, // LDA $DD00
        0x09, 0x08, // ORA #$08   ATN out
        STA_ABS, 0x00, 0xdd, // STA $DD00 (W1: assert)
        LDA_ABS, 0x00, 0xdd, STA_ABS, 0x00, 0xc1, // LDA $DD00 / STA $C100
        LDA_ABS, 0x00, 0xdd, 0x29, 0xf7, // LDA $DD00 / AND #$F7
        STA_ABS, 0x00, 0xdd, // STA $DD00 (W2: release)
        LDA_ABS, 0x00, 0xdd, STA_ABS, 0x01, 0xc1, // → $C101
        // $DD02: bit 3 as input reads 1 → the inverter asserts ATN.
        LDA_ABS, 0x02, 0xdd, 0x29, 0xf7, // LDA $DD02 / AND #$F7
        STA_ABS, 0x02, 0xdd, // STA $DD02 (W3: assert)
        LDA_ABS, 0x00, 0xdd, STA_ABS, 0x02, 0xc1, // → $C102
        LDA_ABS, 0x02, 0xdd, 0x09, 0x08, // LDA $DD02 / ORA #$08
        STA_ABS, 0x02, 0xdd, // STA $DD02 (W4: release)
        LDA_ABS, 0x00, 0xdd, STA_ABS, 0x03, 0xc1, // → $C103
        0xee, 0xff, 0xc0, // INC $C0FF
        0x58, 0x60, // CLI / RTS
    ]
}

#[test]
fn atn_edge_at_its_cycle() {
    need_roms!();
    let mut m = booted(false);
    m.attach_iec_device(4, Box::new(ProbeListener::new("probe", None))).unwrap();
    start(&mut m, &atn_program());
    let mut obs = Dd00::default();
    run_until_done(&mut m, &mut obs);
    let writes: Vec<u64> = obs.writes.iter().map(|w| w.0).chain(obs.ddr_writes.iter().map(|w| w.0)).collect();
    // The program's four writes are the last two of each kind.
    let w = [
        obs.writes[obs.writes.len() - 2].0,
        obs.writes[obs.writes.len() - 1].0,
        obs.ddr_writes[obs.ddr_writes.len() - 2].0,
        obs.ddr_writes[obs.ddr_writes.len() - 1].0,
    ];
    assert!(writes.len() >= 4);
    let log = &probe(&m, 4).log;
    for (i, (&wc, level)) in w.iter().zip([false, true, false, true]).enumerate() {
        let at = log.iter().position(|e| *e == Ev::Edge(wc + 1, level)).unwrap_or_else(|| panic!("write {i}: no atn_edge({}, {level})", wc + 1));
        match log.get(at + 1) {
            Some(Ev::Clock(c, l)) => {
                assert_eq!(*c, wc + 1, "write {i}: the clock_to after the edge");
                assert_eq!(l.atn, level, "write {i}: it carries the same ATN");
            }
            e => panic!("write {i}: after the edge came {e:?}"),
        }
        // Run to no later than the edge under the old lines.
        let before = log[..at].iter().rev().find_map(|e| if let Ev::Clock(c, l) = e { Some((*c, *l)) } else { None }).unwrap();
        assert!(before.0 <= wc + 1 && before.1.atn != level, "write {i}: ran to {} under the old ATN", before.0);
    }
    let edges = log.iter().filter(|e| matches!(e, Ev::Edge(..))).count();
    assert_eq!(edges, 4, "one edge per ATN change, none for a write that does not change it");
    let got: Vec<u8> = (0..4).map(|i| m.read_full(0xc100 + i) & 0x80).collect();
    assert_eq!(got, vec![0x00, 0x80, 0x00, 0x80], "DATA low under ATN, released after");
}

/// KERNAL `LISTEN 9` / `SECOND $6F` / `UNLSN`, ST to `$C0F0`.
fn listen_program(unit: u8) -> Vec<u8> {
    vec![
        0xa9, 0x00, 0x85, 0x90, // LDA #0 / STA $90
        0xa9, unit, 0x20, 0xb1, 0xff, // LDA #unit / JSR LISTEN
        0xa9, 0x6f, 0x20, 0x93, 0xff, // LDA #$6F / JSR SECOND
        0x20, 0xae, 0xff, // JSR UNLSN
        0xa5, 0x90, 0x8d, 0xf0, 0xc0, // LDA $90 / STA $C0F0
        0xee, 0xff, 0xc0, 0x60, // INC $C0FF / RTS
    ]
}

#[test]
fn kernal_listen_nine_finds_the_probe() {
    need_roms!();
    let mut m = booted(false);
    start(&mut m, &listen_program(9));
    run_until_done(&mut m, &mut NullSink);
    assert_eq!(m.read_full(0xc0f0) & 0x80, 0x80, "control: nobody at 9 is DEVICE NOT PRESENT");

    let mut m = booted(false);
    m.attach_iec_device(9, Box::new(ProbeListener::new("probe", Some(9)))).unwrap();
    start(&mut m, &listen_program(9));
    run_until_done(&mut m, &mut NullSink);
    assert_eq!(m.read_full(0xc0f0), 0, "the probe at 9 answers: no DEVICE NOT PRESENT");
    assert_eq!(probe(&m, 9).under_atn, vec![0x29, 0x6f, 0x3f], "LISTEN 9, SECOND $6F, UNLISTEN under ATN");
}

// ── §12.4 — under a hold ────────────────────────────────────────────────────────────

/// `SEI`, then forever `LDA $DD00` / `STA $C100`, with `$C0FF` set on entry.
fn spin_reader() -> Vec<u8> {
    vec![
        0x78, 0xee, 0xff, 0xc0, // SEI / INC $C0FF
        LDA_ABS, 0x00, 0xdd, STA_ABS, 0x00, 0xc1, // loop: LDA $DD00 / STA $C100
        0x4c, 0x04, 0xc0, // JMP loop
    ]
}

#[test]
fn clocked_through_a_cpu_hold() {
    need_roms!();
    let mut m = booted(false);
    let mut p = ProbeListener::new("probe", None);
    p.hold_data_until = Some(u64::MAX);
    m.attach_iec_device(4, Box::new(p)).unwrap();
    start(&mut m, &spin_reader());
    run_until_done(&mut m, &mut NullSink);
    frames(&mut m, 1);
    assert_eq!(m.read_full(0xc100) & 0x80, 0, "DATA held");

    let from = m.c64_core.clk;
    probe_mut(&mut m, 4).hold_data_until = Some(from + 5_000);
    probe_mut(&mut m, 4).log.clear();
    m.set_hold(Some(Hold::Cpu));
    m.run_for_full(20_000, &mut NullSink, |_, _, _, _, _, _, _| {});
    let end = m.c64_core.clk;
    let clocks = probe(&m, 4).clocks();
    assert_eq!(clocks.len(), 1, "one sync for the held span");
    assert_eq!(clocks[0].0, end, "clocked to the held span's end");
    assert_eq!(m.read_full(0xc100) & 0x80, 0, "the CPU did not run");
    m.set_hold(None);
    let mut obs = Dd00::default();
    m.run_for_full(40, &mut obs, |_, _, _, _, _, _, _| {});
    assert!(obs.reads[0].1 & 0x80 != 0, "the first read after the hold sees the release");
    assert!(m.read_full(0xc100) & 0x80 != 0);
}

#[test]
fn clocked_through_a_reset_hold() {
    need_roms!();
    let mut m = booted(false);
    let mut p = ProbeListener::new("probe", None);
    p.hold_data_until = Some(u64::MAX);
    m.attach_iec_device(4, Box::new(p)).unwrap();
    frames(&mut m, 1);
    assert_eq!(m.iec.iecbus.cpu_port & 0x80, 0, "DATA held");
    let from = m.c64_core.clk;
    probe_mut(&mut m, 4).hold_data_until = Some(from + 5_000);
    probe_mut(&mut m, 4).log.clear();
    m.set_hold(Some(Hold::Reset));
    m.run_for_full(20_000, &mut NullSink, |_, _, _, _, _, _, _| {});
    let end = m.c64_core.clk;
    let clocks = probe(&m, 4).clocks();
    assert_eq!(clocks.iter().map(|c| c.0).collect::<Vec<_>>(), vec![end], "clocked once, to the held span's end");
    assert!(m.iec.iecbus.cpu_port & 0x80 != 0, "the release inside the span is on the bus after it");
    m.set_hold(None);
}

/// The drive runs `$0500`: two `$1800` stores (each re-folds its bus), then forever
/// `LDA $1800` / `STA $0600` / `INC $0601`.
fn drive_reader() -> Vec<u8> {
    vec![
        0x78, // SEI
        0xa9, 0x02, 0x8d, 0x00, 0x18, // LDA #$02 / STA $1800
        0xa9, 0x00, 0x8d, 0x00, 0x18, // LDA #$00 / STA $1800
        0xad, 0x00, 0x18, 0x8d, 0x00, 0x06, // loop: LDA $1800 / STA $0600
        0xee, 0x01, 0x06, // INC $0601
        0x4c, 0x0b, 0x05, // JMP loop
    ]
}

/// Drive 8, held C64: what `$1800` bit 0 (DATA IN, 1 = low) shows after the drive's own
/// stores, with the probe at slot 4 holding DATA or not.
fn drive_sees(hold_data: bool) -> u8 {
    let mut m = booted(true);
    let mut p = ProbeListener::new("probe", None);
    p.hold_data_until = hold_data.then_some(u64::MAX);
    m.attach_iec_device(4, Box::new(p)).unwrap();
    frames(&mut m, 2);
    for (i, b) in drive_reader().iter().enumerate() {
        m.drive8.drive_ram_write(0x0500 + i as u16, *b);
    }
    m.drive8.drive_ram_write(0x0601, 0);
    m.drive8.core.reg_pc = 0x0500;
    m.set_hold(Some(Hold::Cpu));
    m.run_for_full(20_000, &mut NullSink, |_, _, _, _, _, _, _| {});
    m.set_hold(None);
    assert!(m.drive8.drive_ram_read(0x0601) > 10, "the drive ran its loop");
    m.drive8.drive_ram_read(0x0600)
}

#[test]
fn drive_eight_sees_slot_four_under_a_cpu_hold() {
    need_roms!();
    let held = drive_sees(true);
    let free = drive_sees(false);
    assert_eq!(held & 0x01, 0x01, "DATA IN low while the probe at slot 4 pulls it (after the drive's own stores): ${held:02X}");
    assert_eq!(free & 0x01, 0x00, "DATA IN high without the pull: ${free:02X}");
}

// ── §12.5 — refusals ────────────────────────────────────────────────────────────────

fn tmp_folder(tag: &str) -> std::sync::Arc<trx64_core::folder_device::HostFolder> {
    let dir = std::env::temp_dir().join(format!("trx64-874-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::sync::Arc::new(trx64_core::folder_device::HostFolder::new(&dir).unwrap())
}

#[test]
fn refusals_name_the_occupant() {
    let mut m = Machine::new();
    let p = |units: u16| {
        let mut p = ProbeListener::new("probe", None);
        p.claims = units;
        Box::new(p)
    };
    for slot in [3, 12] {
        let e = m.attach_iec_device(slot, p(0)).unwrap_err();
        assert!(e.contains(&format!("not {slot}")), "{e}");
    }
    // Drive A is powered at 8.
    let e = m.attach_iec_device(8, p(0)).unwrap_err();
    assert!(e.contains("drive position A is powered at unit 8"), "{e}");
    // Drive B's jumpers at 9, powered.
    m.drive_b.attach_disk(trx64_core::drive::DiskImage { kind: trx64_core::drive::DiskKind::D64, bytes: vec![0; 174_848], backing_path: None, read_only: false });
    m.set_drive_power(DrivePosition::B, true).unwrap();
    let e = m.attach_iec_device(9, p(0)).unwrap_err();
    assert!(e.contains("drive position B is powered at unit 9"), "{e}");
    // A claim of 9 with drive B at 9.
    let e = m.attach_iec_device(4, p(1 << 9)).unwrap_err();
    assert!(e.contains("cannot answer to unit 9") && e.contains("drive position B"), "{e}");
    m.set_drive_power(DrivePosition::B, false).unwrap();
    // A folder's unit.
    m.attach_folder(10, tmp_folder("refuse"), Default::default()).unwrap();
    let e = m.attach_iec_device(10, p(0)).unwrap_err();
    assert!(e.contains("folder 10 stands there"), "{e}");
    let e = m.attach_iec_device(5, p(1 << 10)).unwrap_err();
    assert!(e.contains("unit 10") && e.contains("folder 10"), "{e}");
    // Another device at the slot.
    m.attach_iec_device(4, p(1 << 9)).unwrap();
    let e = m.attach_iec_device(4, p(0)).unwrap_err();
    assert!(e.contains("probe stands there"), "{e}");
    // The reverse: drive B at 9 and a folder at 9 while the probe at 4 claims 9.
    let e = m.set_drive_power(DrivePosition::B, true).unwrap_err();
    assert!(e.contains("probe at slot 4 answers to unit 9"), "{e}");
    let e = m.attach_folder(9, tmp_folder("refuse9"), Default::default()).unwrap_err();
    assert!(e.contains("probe at slot 4 answers to unit 9"), "{e}");
    // And a device standing at 11 takes unit 11 from a drive.
    m.detach_iec_device(4).unwrap();
    m.attach_iec_device(11, p(0)).unwrap();
    m.set_drive_unit(DrivePosition::B, 11).ok();
    let e = m.set_drive_power(DrivePosition::B, true).unwrap_err();
    assert!(e.contains("probe stands at slot 11"), "{e}");
}

// ── §12.6 — checkpoints: the opt-out and the hooks ─────────────────────────────────

fn capture(m: &mut Machine) -> serde_json::Value {
    let blob = trx64_core::drive_snapshot::capture_drive1541(&mut m.drive8);
    capture_runtime_checkpoint(m, "", "d64", Some(&blob), None, None, None)
}

#[test]
fn checkpoint_opt_out_is_named() {
    need_roms!();
    let mut m = booted(false);
    assert!(capture(&mut m).get("iecDevices").is_none(), "no host device, no key");
    let mut p = ProbeListener::new("probe", None);
    p.hold_data_until = Some(u64::MAX);
    m.attach_iec_device(4, Box::new(p)).unwrap();
    frames(&mut m, 1);
    let cp = capture(&mut m);
    assert_eq!(cp["iecDevices"], serde_json::json!([{ "slot": 4, "name": "probe", "units": 0, "state": null }]));
    assert!(cp.get("folders").is_none());
    let taken_at = m.c64_core.clk;

    // Run on, release DATA, then restore the earlier state.
    frames(&mut m, 3);
    probe_mut(&mut m, 4).hold_data_until = Some(0);
    frames(&mut m, 1);
    assert_eq!(m.iec.iecbus.drv_bus[4], 0xc0, "released live");
    probe_mut(&mut m, 4).log.clear();
    restore_runtime_checkpoint(&mut m, &cp).expect("restore");
    assert_eq!(m.c64_core.clk, taken_at);
    assert_eq!(m.iec_devices_uncovered(), &["probe".to_string()], "named uncovered");
    assert_eq!(m.iec.iecbus.drv_bus[4], 0xc0, "its slot holds its live pull, not the captured one (0x40)");
    assert!(m.iec.iecbus.cpu_port & 0x80 != 0, "and the bus is folded with it");
    match probe(&m, 4).log.as_slice() {
        [Ev::Rebase(c, _)] => assert_eq!(*c, taken_at, "rebased to the restored clock"),
        l => panic!("expected one rebase, got {l:?}"),
    }
    // Detaching it clears the name.
    m.detach_iec_device(4).unwrap();
    assert!(m.iec_devices_uncovered().is_empty());
    assert_eq!(m.iec.iecbus.drv_bus[4], 0xff);
}

#[test]
fn checkpoint_hooks_round_trip() {
    need_roms!();
    let mut m = booted(false);
    let mut p = ProbeListener::new("probe", Some(9));
    p.hooks = true;
    m.attach_iec_device(9, Box::new(p)).unwrap();
    start(&mut m, &listen_program(9));
    run_until_done(&mut m, &mut NullSink);
    let cp = capture(&mut m);
    let saved = probe(&m, 9).line.clone();
    assert_eq!(cp["iecDevices"][0]["state"], serde_json::to_value(&saved).unwrap());
    frames(&mut m, 5);
    probe_mut(&mut m, 9).line.byte ^= 0xff;
    probe_mut(&mut m, 9).line.primary = 0x55;
    restore_runtime_checkpoint(&mut m, &cp).expect("restore");
    assert_eq!(probe(&m, 9).line, saved, "restore got back what checkpoint gave");
    assert!(m.iec_devices_uncovered().is_empty(), "covered");
}

// ── §12.7 — clone ───────────────────────────────────────────────────────────────────

#[test]
fn clone_leaves_a_released_vacancy() {
    need_roms!();
    let mut m = booted(false);
    let mut p = ProbeListener::new("probe", None);
    p.hold_data_until = Some(u64::MAX);
    m.attach_iec_device(4, Box::new(p)).unwrap();
    m.attach_folder(9, tmp_folder("clone"), Default::default()).unwrap();
    frames(&mut m, 1);
    assert_eq!(m.iec.iecbus.drv_bus[4], 0x40);
    let c = m.clone();
    assert_eq!(c.iec.iecbus.drv_bus[4], 0xc0, "the vacancy is released in the clone");
    assert_eq!(c.iec.iecbus.cpu_port & 0x80, 0x80, "and the clone's bus folded");
    assert!(c.iec_device_as::<ProbeListener>(4).is_none());
    assert_eq!(c.iec_devices.get(4).map(|d| d.name()), Some("probe".to_string()), "same slot, same name");
    assert_eq!(c.iec.device_slots, m.iec.device_slots, "the map unchanged");
    assert_eq!(c.iec.iecbus_callback, m.iec.iecbus_callback);
    assert_eq!(c.iec_devices_uncovered(), &["probe".to_string()]);
    assert!(c.folder(9).is_some(), "the folder is carried");
    // The original is untouched.
    assert_eq!(m.iec.iecbus.drv_bus[4], 0x40);
    assert!(m.iec_devices_uncovered().is_empty());
    assert!(m.iec_device_as::<ProbeListener>(4).is_some());
    // A clone of the clone keeps the vacancy.
    let cc = c.clone();
    assert_eq!(cc.iec_devices_uncovered(), &["probe".to_string()]);
}

// ── §12.8 — reset and model ─────────────────────────────────────────────────────────

#[test]
fn reset_and_model_reach_the_device() {
    need_roms!();
    let mut m = booted(false);
    m.attach_iec_device(4, Box::new(ProbeListener::new("probe", None))).unwrap();
    let pal = m.model().timing.cpu_hz;
    assert_eq!(probe(&m, 4).log[0], Ev::Hz(pal), "the clock rate at attach");
    assert!(matches!(probe(&m, 4).log[1], Ev::Rebase(..)), "then a rebase");
    m.warm_reset();
    assert!(probe(&m, 4).log.contains(&Ev::Reset), "c64_reset on a C64 reset");
    assert_eq!(m.iec.iecbus.drv_bus[4], 0xc0, "its slot back after the reset rebuilt the IEC core");
    assert_eq!(m.iec.device_slots, 1 << 4);
    let ntsc = trx64_core::model::resolve("c64-ntsc").unwrap();
    m.put_on_model(ntsc).unwrap();
    assert_eq!(probe(&m, 4).cpu_hz, ntsc.timing.cpu_hz);
    m.put_on_model(trx64_core::model::resolve("c64-pal").unwrap()).unwrap();
    assert_eq!(probe(&m, 4).cpu_hz, pal);
    let hz: Vec<u32> = probe(&m, 4).log.iter().filter_map(|e| if let Ev::Hz(h) = e { Some(*h) } else { None }).collect();
    assert_eq!(hz, vec![pal, ntsc.timing.cpu_hz, pal]);
}

// ── detach, the slot byte, the lines ────────────────────────────────────────────────

#[test]
fn detach_hands_the_device_back() {
    let mut m = Machine::new();
    let mut p = ProbeListener::new("probe", None);
    p.hold_data_until = Some(u64::MAX);
    m.attach_iec_device(4, Box::new(p)).unwrap();
    assert_eq!(m.iec.iecbus.drv_bus[4], 0x40, "seeded from outputs() at attach");
    assert_eq!(m.iec.iecbus_device[4], trx64_core::iec::IECBUS_DEVICE_IECDEVICE);
    let back = m.detach_iec_device(4).unwrap();
    assert_eq!(back.name(), "probe");
    assert!(back.as_ref().as_any().is::<ProbeListener>());
    assert_eq!(m.iec.iecbus.drv_bus[4], 0xff);
    assert_eq!(m.iec.iecbus_device[4], trx64_core::iec::IECBUS_DEVICE_NONE);
    assert_eq!(m.iec.device_slots, 0);
    assert!(m.detach_iec_device(4).is_err());
    assert_eq!(IecOut { clk: false, data: false }.slot_byte(), 0xc0);
    assert_eq!(IecOut { clk: true, data: true }.slot_byte(), 0x00);
    assert_eq!(IecLines::RELEASED.with(IecOut { clk: true, data: false }), IecLines { atn: true, clk: false, data: true });
}

// ── §12.13 — characterisation: cost ─────────────────────────────────────────────────

fn timed(what: &str) -> f64 {
    use std::time::Instant;
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    let dir = std::env::temp_dir().join(format!("trx64-874-cost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let big: Vec<u8> = [0x00, 0xc0].into_iter().chain((0..30_000u32).map(|i| ((i * 31 + 10) % 256) as u8)).collect();
    std::fs::write(dir.join("big.prg"), &big).unwrap();
    let src = std::sync::Arc::new(trx64_core::folder_device::HostFolder::new(&dir).unwrap());
    match what {
        "folder" | "serving" => m.attach_folder(9, src, Default::default()).unwrap(),
        "probe" => m.attach_iec_device(4, Box::new({
            let mut p = ProbeListener::new("probe", None);
            p.record = false;
            p
        })).unwrap(),
        _ => {}
    }
    frames(&mut m, 130);
    let bytes = std::fs::read(format!("{SAMPLES}/scramble_infinity.d64")).expect("scramble sample");
    m.drive8.attach_disk(trx64_core::drive::DiskImage { kind: trx64_core::drive::DiskKind::D64, bytes, backing_path: None, read_only: false });
    frames(&mut m, 40);
    let keys: &[u8] = if what == "serving" { b"LOAD\"BIG\",9,1\r" } else { b"LOAD\"*\",8,1\r" };
    for (i, b) in keys.iter().enumerate() {
        m.poke(0x0277 + i as u16, &[*b]);
    }
    m.poke(0x00c6, &[keys.len() as u8]);
    let n = 1500u32;
    let t0 = Instant::now();
    frames(&mut m, n);
    let ms = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
    let _ = std::fs::remove_dir_all(&dir);
    ms
}

#[test]
#[ignore = "characterisation §12.13; run with --ignored --nocapture (release)"]
fn characterise_the_cost_of_a_device() {
    need_roms!();
    let mut best = [f64::MAX; 4];
    for _ in 0..3 {
        for (i, w) in ["none", "folder", "probe", "serving"].iter().enumerate() {
            best[i] = best[i].min(timed(w));
        }
    }
    eprintln!(
        "\nframe time, 1500 frames, best of 3 (873: 4.949 / 5.125 / 3.368):\n  no device, LOAD\"*\",8,1 of scramble: {:.3} ms/frame\n  idle folder at 9, same load: {:.3} ms/frame (x{:.3})\n  idle probe at slot 4, same load: {:.3} ms/frame (x{:.3})\n  folder at 9 serving LOAD\"BIG\",9,1: {:.3} ms/frame",
        best[0],
        best[1],
        best[1] / best[0],
        best[2],
        best[2] / best[0],
        best[3]
    );
}
