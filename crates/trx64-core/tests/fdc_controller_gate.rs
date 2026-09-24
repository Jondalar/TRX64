//! Spec 875 §12 — a controller of the host's own in the 1581.
//!
//! Every test boots the real machine with the real KERNAL and the real 1581 DOS and puts
//! `SectorFdc` (tests/common/sector_fdc.rs) into position A's board through the public
//! `FdcController` trait, the way UE2 puts its model of the U64's `wd177x.vhd` there. The
//! DOS then talks to it through `$6000-$7FFF` and the CIA's glue lines, and nothing else.
//!
//! The D81s and the keyboard driving come from 872's gate (tests/common/d81_kit.rs).
//!
//!   cargo test --release -p trx64-core --test fdc_controller_gate -- --nocapture
//!   (the cost characterisation: `-- --ignored --nocapture`)

#![allow(dead_code, unused_imports)]

#[path = "common/sector_fdc.rs"]
mod sector_fdc;

include!("common/d81_kit.rs");

use sector_fdc::{Ev, SectorFdc};
use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, restore_runtime_checkpoint};
use trx64_core::drive_snapshot::{capture_drive1541, capture_drive_disk_image};
use trx64_core::fdc_controller::FdcController;

const NAME: &str = "SectorFdc";

/// The refusal a call gave (its `Ok` side carries no `Debug`).
fn refused<T>(r: Result<T, String>) -> String {
    match r {
        Err(e) => e,
        Ok(_) => panic!("expected a refusal"),
    }
}

// ── the machine with a controller ───────────────────────────────────────────────

/// Booted to READY with a 1581 in position A at unit 8 whose socket holds `SectorFdc`
/// with `img`, set up by `setup` before it is fitted.
fn booted_fdc(img: Vec<u8>, setup: impl FnOnce(&mut SectorFdc)) -> Machine {
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    m.set_drive_power(DrivePosition::A, false).unwrap();
    m.set_drive_type(DrivePosition::A, DriveType::Drive1581).expect("A off: type change");
    m.drive8.set_rom_1581(&rom_1581().unwrap()).unwrap();
    let mut f = SectorFdc::new(NAME, img);
    setup(&mut f);
    let ejected = m.attach_fdc_controller(DrivePosition::A, Box::new(f)).expect("fitted");
    assert!(ejected.is_none(), "no medium was mounted");
    m.set_drive_power(DrivePosition::A, true).unwrap();
    frames(&mut m, 200);
    m
}

fn fdc(m: &Machine) -> &SectorFdc {
    m.fdc_controller_as::<SectorFdc>(DrivePosition::A).expect("SectorFdc at A")
}

fn fdc_mut(m: &mut Machine) -> &mut SectorFdc {
    m.fdc_controller_as_mut::<SectorFdc>(DrivePosition::A).expect("SectorFdc at A")
}

/// The D81 as the controller holds it once the drive has let go of it.
fn fdc_persisted(m: &mut Machine) -> Vec<u8> {
    settle(m, DrivePosition::A);
    fdc(m).d81().to_vec()
}

/// Run a command to `READY.` and return the screen and the C64 cycle it got there.
fn timed(m: &mut Machine, cmd: &[u8], max_frames: u32) -> (String, u64) {
    let out = command(m, cmd, max_frames);
    (out, m.c64_core.clk)
}

// ── §12.2 — the DOS through it, byte-identical to TRX64's own WD ─────────────────

#[test]
fn the_directory_through_a_host_controller_is_the_builtin_listing() {
    need_roms!();
    let img = directory_image();
    let mut b = booted_1581(Some(img.clone()));
    let mut h = booted_fdc(img.clone(), |_| {});
    let lb = load_dir(&mut b, 8);
    let lh = load_dir(&mut h, 8);
    eprintln!("[§12.2] directory READY at C64 cycle: built-in {}, host {}", b.c64_core.clk, h.c64_core.clk);
    assert!(parse_listing(&lb).0.is_some(), "the built-in run lists:\n{lb}");
    assert_eq!(parse_listing(&lh), parse_listing(&lb), "the listing:\n{lh}");
    assert_eq!(fdc_persisted(&mut h), persisted(&mut b, DrivePosition::A), "a directory read writes nothing, either way");
    assert_eq!(fdc(&h).d81(), &img[..]);
    assert!(fdc(&h).busy_seen > 0, "the DOS saw BUSY");
}

#[test]
fn a_load_through_a_host_controller_is_the_builtin_load() {
    need_roms!();
    let (img, prg) = load_image();
    let mut b = booted_1581(Some(img.clone()));
    let mut h = booted_fdc(img, |_| {});
    let (ob, cb) = timed(&mut b, b"LOAD\"FILE\",8,1\r", 6000);
    let (oh, ch) = timed(&mut h, b"LOAD\"FILE\",8,1\r", 6000);
    eprintln!("[§12.2] LOAD READY at C64 cycle: built-in {cb}, host {ch}");
    assert!(ob.contains("LOADING") && !ob.contains("ERROR"), "built-in LOAD:\n{ob}");
    assert!(oh.contains("LOADING") && !oh.contains("ERROR"), "host LOAD:\n{oh}");
    assert_eq!(loaded(&b, &prg), prg[2..]);
    assert_eq!(loaded(&h, &prg), loaded(&b, &prg), "the loaded bytes");
    // Both sides of the disk were read.
    let sides: std::collections::BTreeSet<bool> =
        fdc(&h).log.iter().filter_map(|e| if let Ev::Served { cmd, side0, .. } = e { (cmd & 0xe0 == 0x80).then_some(*side0) } else { None }).collect();
    assert_eq!(sides.len(), 2, "READ SECTOR on both sides");
}

#[test]
fn a_save_through_a_host_controller_writes_the_builtin_d81() {
    need_roms!();
    let (img, prg) = load_image();
    let mut b = booted_1581(Some(img.clone()));
    let mut h = booted_fdc(img, |_| {});
    for m in [&mut b, &mut h] {
        let out = command(m, b"LOAD\"FILE\",8,1\r", 6000);
        assert!(!out.contains("ERROR"), "{out}");
    }
    let (ob, cb) = timed(&mut b, b"SAVE\"NEW\",8\r", 8000);
    let (oh, ch) = timed(&mut h, b"SAVE\"NEW\",8\r", 8000);
    eprintln!("[§12.2] SAVE READY at C64 cycle: built-in {cb}, host {ch}");
    assert!(ob.contains("SAVING") && !ob.contains("ERROR"), "built-in SAVE:\n{ob}");
    assert!(oh.contains("SAVING") && !oh.contains("ERROR"), "host SAVE:\n{oh}");
    let after_b = persisted(&mut b, DrivePosition::A);
    let after_h = fdc_persisted(&mut h);
    assert_eq!(read_file(&after_b, b"NEW").map(|f| f.1), Some(prg.clone()));
    let diff = changed_sectors(&after_b, &after_h);
    assert!(diff.is_empty(), "the D81 after SAVE differs in {diff:?}");
    assert!(after_b == after_h);
}

#[test]
fn a_format_through_a_host_controller_writes_the_builtin_d81() {
    need_roms!();
    let blank = vec![0xe5u8; D81_LEN];
    let mut b = booted_1581(Some(blank.clone()));
    let mut h = booted_fdc(blank, |_| {});
    let (ob, cb) = timed(&mut b, b"OPEN15,8,15,\"N:TEST,72\":CLOSE15\r", 20_000);
    let (oh, ch) = timed(&mut h, b"OPEN15,8,15,\"N:TEST,72\":CLOSE15\r", 20_000);
    eprintln!("[§12.2] format READY at C64 cycle: built-in {cb}, host {ch}");
    assert!(ob.contains("READY.") && !ob.contains("ERROR"), "built-in format:\n{ob}");
    assert!(oh.contains("READY.") && !oh.contains("ERROR"), "host format:\n{oh}");
    let after_b = persisted(&mut b, DrivePosition::A);
    let after_h = fdc_persisted(&mut h);
    let wt = fdc(&h).log.iter().filter(|e| matches!(e, Ev::Served { cmd, .. } if cmd & 0xf0 == 0xf0)).count();
    assert_eq!(wt, 160, "one WRITE TRACK per physical track and side");
    assert_eq!(static_dir(&after_h).blocks_free, 3160);
    let diff = changed_sectors(&after_b, &after_h);
    assert!(diff.is_empty(), "the D81 after format differs in {} sectors, first {:?}", diff.len(), diff.first());
    assert!(after_b == after_h);
}

// ── the BUSY contract (§3) ──────────────────────────────────────────────────────

/// A controller that finishes a command before the DOS's first status read (6 drive
/// cycles after the store) hangs the DOS in `$CBFA`; one that serves 48 cycles later
/// does not.
#[test]
fn a_controller_that_finishes_inside_the_store_hangs_the_dos_at_cbfa() {
    need_roms!();
    let img = directory_image();
    let mut bad = booted_fdc(img.clone(), |f| f.serve_in_store = true);
    type_in(&mut bad, b"\x93LOAD\"$\",8\r");
    frames(&mut bad, 600);
    let pc = bad.drive8.cpu().reg_pc;
    assert!((0xcbfa..=0xcbfe).contains(&pc), "the DOS waits for BUSY at $CBFA: PC ${pc:04X}");
    assert!(!screen(&bad).contains("READY."), "the LOAD never ends:\n{}", screen(&bad));
    assert_eq!(fdc(&bad).busy_seen, 0, "no status read ever saw BUSY");
    // The same disk with a controller that keeps the contract.
    let mut good = booted_fdc(img, |_| {});
    let out = load_dir(&mut good, 8);
    assert!(parse_listing(&out).0.is_some(), "{out}");
    let (stores, served): (Vec<u64>, Vec<u64>) = (
        fdc(&good).log.iter().filter_map(|e| if let Ev::Store { clk, reg: 0, .. } = e { Some(*clk) } else { None }).collect(),
        fdc(&good).log.iter().filter_map(|e| if let Ev::Served { clk, .. } = e { Some(*clk) } else { None }).collect(),
    );
    assert!(!served.is_empty());
    assert!(stores.iter().zip(&served).all(|(s, v)| v - s >= 48), "served no earlier than 48 cycles after the store");
}

// ── §12.3 — side and motor ──────────────────────────────────────────────────────

/// Step the machine one C64 instruction at a time through a LOAD that spans both sides,
/// sampling port A's pins as the CIA drives them after each step. Every change of PA0
/// or PA2 between two samples must be answered by exactly one `board_out` inside that
/// window of drive cycles, carrying the new pins; nothing else may be sent.
#[test]
fn side_and_motor_reach_the_controller_at_the_store_that_moved_them() {
    need_roms!();
    let (img, prg) = load_image();
    let mut m = booted_fdc(img, |_| {});
    let log0 = fdc(&m).log.len();
    let pins = |m: &Machine| {
        let pa = m.drive8.board_1581().unwrap().cia.pa_out();
        (pa & 0x01 != 0, pa & 0x04 == 0)
    };
    // Typed without the RETURN, which goes into the keyboard buffer right before the
    // stepping starts: nothing of the LOAD happens outside the sampled window.
    type_in(&mut m, b"\x93LOAD\"FILE\",8,1");
    assert_eq!(fdc(&m).log.len(), log0, "nothing reached the controller yet");
    m.poke(0x0277, b"\r");
    m.poke(0x00c6, &[1]);
    let mut changes: Vec<(u64, u64, (bool, bool))> = Vec::new();
    let (mut last, mut last_clk) = (pins(&m), m.drive8.drive_clk);
    let mut sink = NullSink;
    let start = m.c64_core.clk;
    let mut steps = 0u64;
    while m.c64_core.clk - start < 3000 * FRAME {
        m.run_for_full(1, &mut sink, |_, _, _, _, _, _, _| {});
        let (now, clk) = (pins(&m), m.drive8.drive_clk);
        if clk < last_clk {
            panic!("the drive reset during the LOAD");
        }
        if now != last {
            changes.push((last_clk, clk, now));
        }
        (last, last_clk) = (now, clk);
        steps += 1;
        if steps.is_multiple_of(5000) && screen(&m).contains("READY.") && !fdc(&m).core.motor_on {
            break;
        }
    }
    // Let the motor go off.
    frames(&mut m, 400);
    assert_eq!(loaded(&m, &prg), prg[2..], "the LOAD completed");
    let outs: Vec<(u64, (bool, bool))> =
        fdc(&m).log[log0..].iter().filter_map(|e| if let Ev::BoardOut { clk, out } = e { Some((*clk, *out)) } else { None }).collect();
    let stepped: Vec<(u64, (bool, bool))> = outs.iter().copied().filter(|(c, _)| *c <= last_clk).collect();
    assert!(!changes.is_empty());
    assert_eq!(stepped.len(), changes.len(), "one board_out per pin change: {stepped:?} vs {changes:?}");
    for ((c, out), (from, to, pins)) in stepped.iter().zip(&changes) {
        assert!(*from < *c && *c <= *to, "board_out at {c} outside the window ({from}, {to}] of the change");
        assert_eq!(out, pins);
    }
    let served_reads: Vec<u64> =
        fdc(&m).log[log0..].iter().filter_map(|e| if let Ev::Served { clk, cmd, .. } = e { (cmd & 0xe0 == 0x80).then_some(*clk) } else { None }).collect();
    let motor_on = outs.iter().find(|o| o.1 .1).expect("the motor rose").0;
    let motor_off = outs.iter().rev().find(|o| !o.1 .1).expect("the motor fell").0;
    assert!(motor_on < served_reads[0], "the motor rose before the first READ SECTOR");
    assert!(motor_off > *served_reads.last().unwrap(), "and fell after the last");
    assert!(outs.iter().any(|o| o.1 .0) && outs.iter().any(|o| !o.1 .0), "side0 both high and low");
    eprintln!("[§12.3] {} board_out calls during the LOAD, each inside its pin change; READ SECTOR at {:?}", outs.len(), served_reads);
}

// ── §12.4 — /RDY, disk change, write protect ────────────────────────────────────

/// Drive code at $0500: read port A and port B into $0580/$0581 (the CPU's own reads
/// through the port hooks).
const PORT_PROBE: [u8; 13] = [0xad, 0x00, 0x40, 0x8d, 0x80, 0x05, 0xad, 0x01, 0x40, 0x8d, 0x81, 0x05, 0x60];

fn probe_ports(m: &mut Machine) -> (u8, u8) {
    m.drive8.board_1581_mut().unwrap().ram_mut()[0x500..0x500 + PORT_PROBE.len()].copy_from_slice(&PORT_PROBE);
    let out = command(m, b"OPEN15,8,15,\"M-E\"+CHR$(0)+CHR$(5):CLOSE15\r", 600);
    assert!(out.contains("READY."), "{out}");
    let ram = m.drive8.ram();
    (ram[0x580], ram[0x581])
}

/// The input as the CIA reads it (a CPU read and a peek) and as `ports()` reports it.
fn check_inputs(m: &mut Machine, ready: bool, changed: bool, protected: bool) {
    let (pa, pb) = probe_ports(m);
    let p = m.drive8.board_1581().unwrap().ports();
    assert_eq!(pa & 0x02 == 0, ready, "PA1 as the CPU reads it");
    assert_eq!(pa & 0x80 == 0, changed, "PA7 as the CPU reads it");
    assert_eq!(pb & 0x40 == 0, protected, "PB6 as the CPU reads it");
    assert_eq!((p.not_ready, p.disk_changed, !p.writable), (!ready, changed, protected), "ports()");
    assert_eq!(m.drive8.drive_peek(0x4000) & 0x82, p.pa & 0x82, "peek = ports()");
    assert_eq!(m.drive8.drive_peek(0x4001) & 0x40, p.pb & 0x40);
}

#[test]
fn not_ready_is_drive_not_ready() {
    need_roms!();
    let mut m = booted_fdc(directory_image(), |f| f.set_input(false, false, false));
    check_inputs(&mut m, false, false, false);
    let out = command(&mut m, b"LOAD\"$\",8\r", 3000);
    assert!(!out.contains("BLOCKS FREE"), "{out}");
    let err = error_channel(&mut m, 8);
    assert!(err.contains("74") && err.contains("DRIVE NOT READY"), "error channel:\n{err}");
}

#[test]
fn a_disk_change_cleared_by_a_step_is_recovered_and_one_held_is_not_ready() {
    need_roms!();
    // Cleared by the first step pulse: the DOS's STEP-IN / STEP-OUT recovery, then the
    // listing. The DOS runs its first disk access (and the recovery) during its own
    // start-up, so the whole log counts.
    let mut m = booted_fdc(directory_image(), |f| f.set_input(true, true, false));
    let out = load_dir(&mut m, 8);
    assert!(parse_listing(&out).0.is_some(), "the listing follows:\n{out}");
    let cmds: Vec<u8> = fdc(&m).log.iter().filter_map(|e| if let Ev::Served { cmd, .. } = e { Some(*cmd) } else { None }).collect();
    let (si, so) = (cmds.iter().position(|c| c & 0xe0 == 0x40), cmds.iter().position(|c| c & 0xe0 == 0x60));
    assert!(si.is_some() && so.is_some() && si < so, "STEP-IN then STEP-OUT served: {cmds:02x?}");
    assert!(cmds[..si.unwrap()].iter().all(|c| c & 0x80 != 0 || c & 0xf0 == 0xd0), "the recovery is the first stepping: {cmds:02x?}");
    check_inputs(&mut m, true, false, false);
    // Held set whatever the steps.
    let mut m = booted_fdc(directory_image(), |f| {
        f.set_input(true, true, false);
        f.core.clear_change_on_step = false;
    });
    check_inputs(&mut m, true, true, false);
    let _ = command(&mut m, b"LOAD\"$\",8\r", 3000);
    let err = error_channel(&mut m, 8);
    assert!(err.contains("74") && err.contains("DRIVE NOT READY"), "error channel:\n{err}");
}

#[test]
fn write_protect_on_pb6_is_write_protect_on() {
    need_roms!();
    let (img, _) = load_image();
    let mut m = booted_fdc(img.clone(), |f| f.set_input(true, false, true));
    check_inputs(&mut m, true, false, true);
    let out = command(&mut m, b"LOAD\"FILE\",8,1\r", 6000);
    assert!(!out.contains("ERROR"), "{out}");
    let _ = command(&mut m, b"SAVE\"X\",8\r", 6000);
    let err = error_channel(&mut m, 8);
    assert!(err.contains("26") && err.contains("WRITE PROTECT ON"), "error channel:\n{err}");
    assert_eq!(fdc_persisted(&mut m), img, "the D81 unchanged");
}

// ── §12.5 — reset and power reach it ────────────────────────────────────────────

fn events_since(m: &Machine, from: usize) -> Vec<Ev> {
    fdc(m).log[from..].iter().filter(|e| !matches!(e, Ev::Store { .. } | Ev::Served { .. } | Ev::BoardOut { .. })).cloned().collect()
}

#[test]
fn reset_and_power_reach_the_controller() {
    need_roms!();
    let mut m = booted_fdc(directory_image(), |_| {});
    // Power-on at boot: power(true), then drive_reset(0).
    let boot: Vec<Ev> = events_since(&m, 0);
    assert!(matches!(boot[..3], [Ev::Rebase { .. }, Ev::Power { on: true }, Ev::DriveReset { clk: 0 }]), "{boot:?}");

    let mark = |m: &Machine| fdc(m).log.len();
    // The drive's RESET.
    let k = mark(&m);
    m.drive8.reset();
    frames(&mut m, 5);
    let ev = events_since(&m, k);
    assert!(matches!(ev[..], [Ev::DriveReset { clk: 0 }, Ev::FirstClockTo { .. }]), "{ev:?}");
    // A C64 warm reset: connected, a drive reset; cut, nothing.
    let k = mark(&m);
    m.warm_reset();
    frames(&mut m, 5);
    assert!(matches!(events_since(&m, k)[..], [Ev::DriveReset { clk: 0 }, Ev::FirstClockTo { .. }]));
    m.drive8.set_reset_line_connected(false);
    let k = mark(&m);
    m.warm_reset();
    frames(&mut m, 5);
    assert!(events_since(&m, k).is_empty(), "cut: nothing");
    m.drive8.set_reset_line_connected(true);
    // Hold and release: a reset each, nothing clocked between.
    frames(&mut m, 200);
    let k = mark(&m);
    m.drive8.set_reset_held(true);
    m.sync_drive_slots();
    let n = fdc(&m).clock_tos;
    frames(&mut m, 20);
    assert_eq!(fdc(&m).clock_tos, n, "held: no clock_to");
    m.drive8.set_reset_held(false);
    m.sync_drive_slots();
    frames(&mut m, 5);
    assert!(matches!(events_since(&m, k)[..], [Ev::DriveReset { clk: 0 }, Ev::DriveReset { clk: 0 }, Ev::FirstClockTo { .. }]), "{:?}", events_since(&m, k));
    // Power off: power(false), then nothing; on: power(true), drive_reset(0).
    frames(&mut m, 200);
    let k = mark(&m);
    let regs = |m: &Machine| (fdc(m).core.track, fdc(m).core.sector, fdc(m).core.data, fdc(m).core.status);
    let before = regs(&m);
    m.set_drive_power(DrivePosition::A, false).unwrap();
    let n = fdc(&m).clock_tos;
    frames(&mut m, 20);
    assert_eq!(fdc(&m).clock_tos, n, "off: no clock_to");
    assert_eq!(events_since(&m, k), vec![Ev::Power { on: false }]);
    assert_eq!(regs(&m), before, "power(false) resets nothing: the registers stand");
    m.set_drive_power(DrivePosition::A, true).unwrap();
    frames(&mut m, 5);
    assert!(matches!(events_since(&m, k)[1..], [Ev::Power { on: true }, Ev::DriveReset { clk: 0 }, Ev::FirstClockTo { .. }]), "{:?}", events_since(&m, k));
    // Stopped: the recorded clock does not move across 50 frames.
    frames(&mut m, 200);
    let k = mark(&m);
    m.drive8.set_stopped(true);
    let (n, last) = (fdc(&m).clock_tos, fdc(&m).last_clock_to);
    frames(&mut m, 50);
    assert_eq!((fdc(&m).clock_tos, fdc(&m).last_clock_to), (n, last), "stopped: nothing");
    m.drive8.set_stopped(false);
    frames(&mut m, 5);
    assert!(events_since(&m, k).is_empty());
    assert!(fdc(&m).last_clock_to > last);
    // After every drive_reset the next clock_to is at or after its clock; the clock
    // never ran backwards otherwise.
    assert!(fdc(&m).clock_regressions.is_empty(), "{:?}", fdc(&m).clock_regressions);
    assert!(load_dir(&mut m, 8).contains("THE1581DISK"), "and the DOS still lists");
}

// ── §12.6 — refusals ─────────────────────────────────────────────────────────────

#[test]
fn fitting_and_removing_are_refused_by_name() {
    need_roms!();
    let img = directory_image();
    let mut m = booted_fdc(img.clone(), |_| {});
    let other = || Box::new(SectorFdc::new("Other", vec![0; D81_LEN])) as Box<dyn FdcController>;
    let has = |e: &str, words: &[&str]| words.iter().all(|w| e.contains(w));
    // Powered.
    let e = refused(m.attach_fdc_controller(DrivePosition::A, other()));
    assert!(has(&e, &["position A", "powered", "Other"]), "{e}");
    let e = refused(m.detach_fdc_controller(DrivePosition::A));
    assert!(has(&e, &["position A", "powered", NAME, "removing"]), "{e}");
    let e = refused(m.set_drive_type(DrivePosition::A, DriveType::Drive1541));
    assert!(has(&e, &["position A", "powered"]), "{e}");
    // The medium is the host's.
    let e = refused(m.drive8.mount(d81(img.clone())));
    assert!(has(&e, &["position A", NAME, "medium is the host's"]), "{e}");
    // Off: a second controller, a 1541 position, a type change.
    m.set_drive_power(DrivePosition::A, false).unwrap();
    let e = refused(m.attach_fdc_controller(DrivePosition::A, other()));
    assert!(has(&e, &["position A", "already has", NAME]), "{e}");
    let e = refused(m.attach_fdc_controller(DrivePosition::B, other()));
    assert!(has(&e, &["position B", "holds a 1541", "Other"]), "{e}");
    let e = refused(m.set_drive_type(DrivePosition::A, DriveType::Drive1541));
    assert!(has(&e, &["position A", NAME, "remove it", "1541"]), "{e}");
    let e = refused(m.drive8.mount(d81(img.clone())));
    assert!(has(&e, &["position A", NAME]), "{e}");
    // Removed off: the box back; the built-in WD and a D81 work again.
    let back = m.detach_fdc_controller(DrivePosition::A).expect("off: removed");
    assert_eq!(back.name(), NAME);
    assert!(back.as_ref().as_any().is::<SectorFdc>());
    m.drive8.mount(d81(img.clone())).expect("a D81 fits again");
    m.set_drive_power(DrivePosition::A, true).unwrap();
    frames(&mut m, 200);
    assert!(load_dir(&mut m, 8).contains("THE1581DISK"), "TRX64's WD1772 is back");

    // Attach with a D81 mounted: that D81 comes back, written back.
    let (img, _) = load_image();
    let mut m = booted_1581(Some(img));
    let out = command(&mut m, b"LOAD\"FILE\",8,1\r", 6000);
    assert!(!out.contains("ERROR"), "{out}");
    let out = command(&mut m, b"SAVE\"NEW\",8\r", 8000);
    assert!(!out.contains("ERROR"), "{out}");
    settle(&mut m, DrivePosition::A);
    let as_written = m.drive8.disk_as_written().unwrap().bytes;
    m.set_drive_power(DrivePosition::A, false).unwrap();
    let ejected = m.attach_fdc_controller(DrivePosition::A, Box::new(SectorFdc::new(NAME, vec![0; D81_LEN]))).unwrap();
    let ejected = ejected.expect("the mounted D81 comes back");
    assert!(ejected.bytes == as_written, "written back");
    assert!(read_file(&ejected.bytes, b"NEW").is_some());
    assert!(m.drive8.get_attached_disk().is_none(), "no medium of TRX64's");
}

// ── §12.7 — checkpoints ──────────────────────────────────────────────────────────

fn capture(m: &mut Machine) -> serde_json::Value {
    let blob = capture_drive1541(&mut m.drive8);
    let overlay = capture_drive_disk_image(&m.drive8);
    capture_runtime_checkpoint(m, "", "d81", Some(&blob), overlay.as_deref(), None, None)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn mid_load(setup: impl FnOnce(&mut SectorFdc)) -> Machine {
    let (img, _) = load_image();
    let mut m = booted_fdc(img, setup);
    type_in(&mut m, b"\x93LOAD\"FILE\",8,1\r");
    assert!(run_until(&mut m, 600, |m| m.drive8.board_1581().unwrap().wd().busy), "the controller at work");
    frames(&mut m, 30);
    m
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

fn same(a: &Machine, b: &Machine) -> Result<(), String> {
    let s = |m: &Machine| {
        (m.c64_core.clk, m.drive8.drive_clk, m.cpu6510.reg_pc, m.drive8.cpu().reg_pc, m.iec.iecbus.cpu_port, m.iec.iecbus.drv_port)
    };
    if s(a) != s(b) {
        return Err(format!("{:?} vs {:?}", s(a), s(b)));
    }
    if a.ram[..] != b.ram[..] || a.drive8.ram() != b.drive8.ram() {
        return Err("RAM".into());
    }
    if capture_drive1541(&mut a.drive8.clone()) != capture_drive1541(&mut b.drive8.clone()) {
        return Err("the 1581's modules".into());
    }
    if fdc(a).core != fdc(b).core {
        return Err("the controller".into());
    }
    Ok(())
}

#[test]
fn a_checkpoint_opts_the_controller_out_and_names_it() {
    need_roms!();
    let mut m = mid_load(|_| {});
    let blob = capture_drive1541(&mut m.drive8);
    assert!(!contains(&blob, b"WD1770") && !contains(&blob, b"FDD0"), "no WD1770 / FDD module");
    assert!(contains(&blob, b"CIA1581D0") && contains(&blob, b"DRIVECPU0"), "the board's CPU and CIA ride");
    assert!(capture_drive_disk_image(&m.drive8).is_none(), "no IMAGE0");
    let cp = capture(&mut m);
    assert_eq!(cp["hostFdc"], serde_json::json!([{ "position": "A", "name": NAME, "state": null }]));
    assert!(cp["driveDiskImage"].is_null());
    // Into the same machine: the controller is rebased to the restored drive clock and
    // named uncovered.
    frames(&mut m, 20);
    let k = fdc(&m).log.len();
    restore_runtime_checkpoint(&mut m, &cp).expect("restore");
    let clk = m.drive8.board_1581().unwrap().core.clk;
    assert!(fdc(&m).log[k..].contains(&Ev::Rebase { clk }), "rebased to {clk}: {:?}", &fdc(&m).log[k..]);
    assert_eq!(m.fdc_uncovered(), vec![NAME.to_string()]);

    // Strict: a checkpoint without the node into a machine with the controller, and the
    // reverse, are refused by name.
    let mut plain = booted_1581(Some(load_image().0));
    let plain_cp = capture(&mut plain);
    assert!(plain_cp.get("hostFdc").is_none(), "without a controller there is no hostFdc key");
    let e = refused(restore_runtime_checkpoint(&mut m, &plain_cp));
    assert!(e.contains(NAME) && e.contains("position A"), "{e}");
    let e = refused(restore_runtime_checkpoint(&mut plain, &cp));
    assert!(e.contains(NAME) && e.contains("position A") && e.contains("none"), "{e}");
    // Another controller's name is refused too, and the refusal touches nothing.
    let mut other = mid_load(|f| f.name = "Other".into());
    let before = other.ram.to_vec();
    let e = refused(restore_runtime_checkpoint(&mut other, &cp));
    assert!(e.contains(NAME) && e.contains("Other"), "{e}");
    assert!(other.ram[..] == before[..], "refused before anything changed");
}

#[test]
fn a_checkpoint_with_hooks_restores_the_controller_in_lockstep() {
    need_roms!();
    let (_, prg) = load_image();
    let mut m = mid_load(|f| {
        f.hooks = true;
        f.cloneable = true;
    });
    let cp = capture(&mut m);
    assert!(cp["hostFdc"][0]["state"].is_object(), "the controller's own state rides");
    let straight = m.clone();
    assert!(straight.fdc_uncovered().is_empty(), "a controller that gives a copy leaves no vacancy");
    frames(&mut m, 50);
    restore_runtime_checkpoint(&mut m, &cp).expect("restore");
    assert!(m.fdc_uncovered().is_empty(), "covered");
    assert_eq!(fdc(&m).log.last(), Some(&Ev::Restored));
    same(&m, &straight).expect("restored = captured");
    let mut a = straight;
    for f in 1..=500 {
        frames(&mut a, 1);
        frames(&mut m, 1);
        if let Err(d) = same(&a, &m) {
            panic!("apart after frame {f}: {d}");
        }
    }
    for mach in [&mut a, &mut m] {
        run_until(mach, 6000, |m| screen(m).contains("READY."));
        assert_eq!(loaded(mach, &prg), prg[2..]);
    }
}

// ── §12.8 — clone ────────────────────────────────────────────────────────────────

#[test]
fn a_clone_has_a_vacancy_that_reads_the_open_bus() {
    need_roms!();
    let mut m = booted_fdc(directory_image(), |_| {});
    let mut c = m.clone();
    assert!(m.fdc_uncovered().is_empty(), "the original is untouched");
    assert_eq!(c.fdc_uncovered(), vec![NAME.to_string()]);
    assert!(c.fdc_controller_as::<SectorFdc>(DrivePosition::A).is_none(), "no chip in the clone's socket");
    assert!(c.drive8.board_1581().unwrap().host_fdc().unwrap().is_vacant());
    // The clone's drive CPU reads the open bus in $6000-$7FFF: `LDA $6000` puts the last
    // byte on the bus back — the address's high byte, $60.
    let code = [0xad, 0x00, 0x60, 0x8d, 0x80, 0x05, 0xad, 0xff, 0x7f, 0x8d, 0x81, 0x05, 0x60];
    c.drive8.board_1581_mut().unwrap().ram_mut()[0x500..0x500 + code.len()].copy_from_slice(&code);
    let out = command(&mut c, b"OPEN15,8,15,\"M-E\"+CHR$(0)+CHR$(5):CLOSE15\r", 600);
    assert!(out.contains("READY."), "{out}");
    assert_eq!((c.drive8.ram()[0x580], c.drive8.ram()[0x581]), (0x60, 0x7f), "the open bus");
    let b = c.drive8.board_1581().unwrap();
    assert_eq!(b.peek(0x6000), b.cpu_last_data, "a peek reads the open bus too");
    let p = b.ports();
    assert!(p.not_ready && !p.disk_changed && p.writable, "nothing drives the lines: all high");
    // A file off the directory track (the DOS's track cache holds track 40 since its
    // start-up, so a directory listing would not ask the socket).
    let out = command(&mut c, b"LOAD\"THIRD\",8\r", 3000);
    assert!(out.contains("READY.") && out.contains("ERROR"), "{out}");
    let err = error_channel(&mut c, 8);
    assert!(err.contains("74") && err.contains("DRIVE NOT READY"), "the clone's DOS: {out}\n{err}");
    // The original lists, and its controller never heard of the clone's run.
    let n = fdc(&m).log.len();
    let out = command(&mut m, b"LOAD\"THIRD\",8\r", 3000);
    assert!(out.contains("LOADING") && !out.contains("ERROR"), "{out}");
    assert!(fdc(&m).log.len() > n, "the original's controller served it");
}

// ── §12.11 — cost ────────────────────────────────────────────────────────────────

const SAMPLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/samples");

/// Frame time of `LOAD"*",8,1` of scramble over 1500 frames: B off (0), a 1581 idle at
/// 9 (1), a 1581 idle at 9 with `SectorFdc` in its socket (2).
fn timed_load(b: u8) -> f64 {
    let bytes = std::fs::read(format!("{SAMPLES}/scramble_infinity.d64")).expect("scramble sample");
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    if b > 0 {
        m.set_drive_type(DrivePosition::B, DriveType::Drive1581).unwrap();
        m.drive_b.set_rom_1581(&rom_1581().unwrap()).unwrap();
        let img = D81::new(b"IDLE", *b"ID").bytes;
        if b == 2 {
            m.attach_fdc_controller(DrivePosition::B, Box::new(SectorFdc::new(NAME, img))).unwrap();
        } else {
            m.drive_b.mount(d81(img)).unwrap();
        }
        m.set_drive_power(DrivePosition::B, true).unwrap();
    }
    frames(&mut m, 130);
    m.drive8.attach_disk(DiskImage { kind: DiskKind::D64, bytes, backing_path: None, read_only: false });
    frames(&mut m, 40);
    type_in(&mut m, b"LOAD\"*\",8,1\r");
    let n = 1500u32;
    let t0 = std::time::Instant::now();
    frames(&mut m, n);
    t0.elapsed().as_secs_f64() * 1000.0 / n as f64
}

#[test]
#[ignore = "characterisation §12.11; run with --ignored --nocapture (release)"]
fn characterise_the_cost_of_a_host_controller() {
    need_roms!();
    if !Path::new(SAMPLES).join("scramble_infinity.d64").exists() {
        eprintln!("[skip] no scramble sample");
        return;
    }
    let mut best = [f64::MAX; 3];
    for _ in 0..5 {
        for b in 0..3u8 {
            best[b as usize] = best[b as usize].min(timed_load(b));
        }
    }
    eprintln!(
        "\n[§12.11] frame time, LOAD\"*\",8,1 of scramble, 1500 frames, best of 5:\n  B off: {:.3} ms/frame\n  1581 idle at 9: {:.3} ms/frame ×{:.3}\n  1581 idle at 9 with SectorFdc: {:.3} ms/frame ×{:.3}",
        best[0],
        best[1],
        best[1] / best[0],
        best[2],
        best[2] / best[0]
    );
}
