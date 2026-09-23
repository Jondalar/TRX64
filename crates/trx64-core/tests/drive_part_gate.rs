//! Spec 870 — the drive as a part: power, its own reset, the reset line, stopped,
//! the ROM as a value, the unit number, and all of it through a checkpoint.
//!
//! Every test boots the real machine with the real KERNAL and DOS and asks the
//! question through the KERNAL: `LOAD"$",n` either lists the disk or ends in
//! DEVICE NOT PRESENT. Nothing here pokes the IEC core to make a point.
//!
//!   cargo test -p trx64-core --test drive_part_gate -- --nocapture

use std::path::Path;

use trx64_core::drive::{DiskImage, DiskKind, DrivePart};
use trx64_core::iec::IecbusCallback;
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const DOS_ROM: &str = "dos1541-325302-01+901229-05.bin";
const FRAME: u64 = 19_656;

fn roms_present() -> bool {
    let d = Path::new(ROM_DIR);
    d.join("kernal-901227-03.bin").exists() && d.join(DOS_ROM).exists()
}

fn sectors_per_track(t: u8) -> usize {
    match t {
        1..=17 => 21,
        18..=24 => 19,
        25..=30 => 18,
        _ => 17,
    }
}

/// A formatted, empty 35-track D64 (valid BAM, empty directory chain).
fn blank_formatted_d64() -> Vec<u8> {
    let mut d = vec![0u8; 174_848];
    let bam: usize = (1..18).map(sectors_per_track).sum::<usize>() * 256;
    d[bam] = 18;
    d[bam + 1] = 1;
    d[bam + 2] = 0x41;
    for t in 1..=35u8 {
        let n = sectors_per_track(t);
        let e = bam + 4 + (t as usize - 1) * 4;
        let used: u32 = if t == 18 { 0b11 } else { 0 };
        let free: u32 = ((1u32 << n) - 1) & !used;
        d[e] = free.count_ones() as u8;
        d[e + 1] = (free & 0xff) as u8;
        d[e + 2] = ((free >> 8) & 0xff) as u8;
        d[e + 3] = ((free >> 16) & 0xff) as u8;
    }
    for (i, b) in b"PARTDISK".iter().enumerate() {
        d[bam + 0x90 + i] = *b;
    }
    for i in 8..16 {
        d[bam + 0x90 + i] = 0xa0;
    }
    d[bam + 0xa0] = 0xa0;
    d[bam + 0xa1] = 0xa0;
    d[bam + 0xa2] = b'P';
    d[bam + 0xa3] = b'D';
    d[bam + 0xa4] = 0xa0;
    d[bam + 0xa5] = b'2';
    d[bam + 0xa6] = b'A';
    for i in 0xa7..0xab {
        d[bam + i] = 0xa0;
    }
    d[bam + 256] = 0x00;
    d[bam + 257] = 0xff;
    d
}

fn frames(m: &mut Machine, n: u32) {
    let mut sink = NullSink;
    for _ in 0..n {
        m.run_for_full(FRAME, &mut sink, |_, _, _, _, _, _, _| {});
    }
}

/// A booted machine at READY with the blank disk in drive 8.
fn booted() -> Machine {
    let mut m = Machine::new();
    m.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
    frames(&mut m, 130);
    m.drive8.attach_disk(DiskImage {
        kind: DiskKind::D64,
        bytes: blank_formatted_d64(),
        backing_path: None,
        read_only: false,
    });
    frames(&mut m, 40);
    m
}

/// The 40×25 screen as text (letters, digits, `.`, `$`, `"`; the rest a space).
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
                _ => ' ',
            });
        }
        s.push_str(line.trim_end());
        s.push('\n');
    }
    s
}

/// Put `s` in the keyboard buffer (≤ 10 chars).
fn type_in(m: &mut Machine, s: &[u8]) {
    assert!(s.len() <= 10);
    for (i, b) in s.iter().enumerate() {
        m.poke(0x0277 + i as u16, &[*b]);
    }
    m.poke(0x00c6, &[s.len() as u8]);
}

/// Clear the screen, type `cmd`, and run until the editor prints READY again.
fn command(m: &mut Machine, cmd: &[u8], max_frames: u32) -> String {
    type_in(m, b"\x93");
    frames(m, 5);
    type_in(m, cmd);
    for _ in 0..max_frames {
        frames(m, 1);
        if screen(m).contains("READY.") {
            break;
        }
    }
    screen(m)
}

/// `LOAD"$",unit` then `LIST`: the directory's last line when it loaded, or the
/// error the KERNAL printed.
fn load_dir(m: &mut Machine, unit: u8) -> String {
    let cmd = format!("LOAD\"$\",{unit}\r");
    let out = command(m, cmd.as_bytes(), 1500);
    if out.contains("ERROR") {
        return out;
    }
    let list = command(m, b"LIST\r", 200);
    format!("{out}{list}")
}

fn assert_lists(out: &str, what: &str) {
    assert!(
        out.contains("BLOCKS FREE") && out.contains("PARTDISK"),
        "{what}: the directory did not load:\n{out}"
    );
}

fn assert_not_present(out: &str, what: &str) {
    assert!(out.contains("DEVICE NOT PRESENT"), "{what}: expected DEVICE NOT PRESENT, got:\n{out}");
}

macro_rules! need_roms {
    () => {
        if !roms_present() {
            eprintln!("[skip] drive_part_gate: ROMs absent at {ROM_DIR}");
            return;
        }
    };
}

/// §8.1 + §8.2 — off leaves the bus alone; off → on is a power-on with the disk kept.
#[test]
fn off_releases_the_bus_and_on_is_a_power_on() {
    need_roms!();
    let mut m = booted();
    assert_lists(&load_dir(&mut m, 8), "the stock drive");

    m.drive8.set_power(false);
    let clk = m.drive8.core.clk;
    let out = load_dir(&mut m, 8);
    assert_not_present(&out, "drive 8 off");
    assert_eq!(m.drive8.core.clk, clk, "an off drive runs no cycle");
    assert_eq!(m.iec.drive_slot, None, "an off drive has no bus slot");
    assert_eq!(m.iec.iecbus_callback, IecbusCallback::Conf0, "the bus has no device on it");
    assert_eq!(m.iec.drv_bus(8), 0xff, "slot 8 released");

    // Mark the RAM, switch on: a power-on starts from cleared RAM, at the reset.
    m.drive8.drive_ram_write(0x0700, 0xa5);
    m.drive8.set_power(true);
    assert!(m.drive8.ram().iter().all(|&b| b == 0), "power-on clears the drive RAM");
    assert_eq!(m.drive8.core.clk, 0, "power-on starts the drive CPU at its reset");
    assert!(m.drive8.disk.is_some(), "the disk is a medium, it stays in the drive");
    // The DOS needs its own boot before it hears ATN — as on the real drive.
    frames(&mut m, 100);
    assert_lists(&load_dir(&mut m, 8), "drive 8 switched back on");
}

/// §8.3 — held in reset: nothing runs, nothing is driven, release = a fresh reset.
#[test]
fn held_in_reset_is_released_bus_and_a_fresh_reset() {
    need_roms!();
    let mut m = booted();
    m.drive8.set_reset_held(true);
    let clk = m.drive8.core.clk;
    frames(&mut m, 50);
    assert_eq!(m.drive8.core.clk, clk, "a held drive runs no cycle");
    assert_eq!(m.iec.drive_slot, None, "a held drive drives nothing");
    assert_not_present(&load_dir(&mut m, 8), "drive held in reset");

    m.drive8.set_reset_held(false);
    assert_eq!(m.drive8.core.clk, 0, "release runs the reset sequence");
    assert!(m.drive8.disk.is_some(), "the disk stays");
    frames(&mut m, 100);
    assert_lists(&load_dir(&mut m, 8), "drive released from reset");
}

/// §8.4 — the reset line: connected resets the drive with the C64, cut leaves it be.
#[test]
fn the_reset_line_decides_whether_a_c64_reset_reaches_the_drive() {
    need_roms!();
    let mut m = booted();
    assert!(m.drive8.reset_line_connected(), "connected by default");
    m.warm_reset();
    assert_eq!(m.drive8.core.clk, 0, "connected: the C64 reset resets the drive");
    assert!(m.drive8.disk.is_some(), "and the disk stays");
    frames(&mut m, 130);

    // Cut: the drive sits in its DOS idle loop; a C64 reset must not touch it.
    m.drive8.set_reset_line_connected(false);
    let before = (m.drive8.core.reg_pc, m.drive8.core.clk, m.drive8.ram().to_vec());
    m.warm_reset();
    let after = (m.drive8.core.reg_pc, m.drive8.core.clk, m.drive8.ram().to_vec());
    assert_eq!(before.0, after.0, "cut: drive PC unchanged");
    assert_eq!(before.1, after.1, "cut: drive clock unchanged");
    assert!(before.2 == after.2, "cut: drive RAM unchanged");
    frames(&mut m, 130);
    assert!(m.drive8.core.clk > before.1, "the drive carries on running");
    assert_lists(&load_dir(&mut m, 8), "after a C64 reset with the line cut");
}

/// §8.5 — the ROM from bytes: 16 K and 32 K boot, anything else is refused by size.
#[test]
fn the_rom_is_a_value_16k_or_32k() {
    need_roms!();
    let dos = std::fs::read(Path::new(ROM_DIR).join(DOS_ROM)).unwrap();

    let err = m_new_drive_rom_err(&[0u8; 1000]);
    assert!(err.contains("1000"), "the refusal names the size: {err}");
    let err = m_new_drive_rom_err(&vec![0u8; 0x6000]);
    assert!(err.contains("24576"), "the refusal names the size: {err}");

    // 32 K: a marker in $8000-$BFFF, the DOS at $C000. It is in force from the reset.
    let mut big = vec![0u8; 0x8000];
    big[0] = 0x5a;
    big[0x4000..].copy_from_slice(&dos);
    let mut m = booted();
    m.drive8.set_rom(&big).expect("32 K accepted");
    assert_eq!(m.drive8.drive_peek(0x8000), 0x00, "not before the drive's reset");
    m.drive8.reset();
    assert_eq!(m.drive8.drive_peek(0x8000), 0x5a, "32 K fills $8000-$FFFF");
    frames(&mut m, 60);
    assert_lists(&load_dir(&mut m, 8), "32 K ROM from bytes");

    // 16 K from bytes: $C000 up, $8000-$BFFF zero.
    m.drive8.set_rom(&dos).expect("16 K accepted");
    m.drive8.reset();
    assert_eq!(m.drive8.drive_peek(0x8000), 0x00, "16 K leaves $8000-$BFFF zero");
    assert_eq!(m.drive8.drive_peek(0xfffc), dos[0x3ffc], "16 K sits at $C000");
    frames(&mut m, 60);
    assert_lists(&load_dir(&mut m, 8), "16 K ROM from bytes");

    // The file loader still works (and boot_from_dir above used it).
    m.drive8.load_rom(Path::new(ROM_DIR)).expect("file loader");
}

fn m_new_drive_rom_err(bytes: &[u8]) -> String {
    let mut d = trx64_core::Drive1541::new();
    d.set_rom(bytes).expect_err("a bad size is refused").to_string()
}

/// §8.6 — the jumpers at 9: `LOAD"$",9` works and `LOAD"$",8` does not.
#[test]
fn unit_nine_answers_to_nine_and_not_to_eight() {
    need_roms!();
    let mut m = booted();
    assert!(m.drive8.set_unit(12).is_err(), "12 is not a jumper setting");
    assert!(m.drive8.set_unit(7).is_err(), "7 is not a jumper setting");
    m.drive8.set_unit(9).expect("9 is a jumper setting");
    assert_eq!(m.drive8.unit(), 8, "the jumpers take effect at the next reset");
    m.drive8.reset();
    assert_eq!(m.drive8.unit(), 9);
    frames(&mut m, 60);
    assert_lists(&load_dir(&mut m, 9), "unit 9");
    assert_eq!(m.iec.drive_slot, Some(9));
    assert_not_present(&load_dir(&mut m, 8), "unit 9 asked as 8");
}

/// §8.8 — stopped mid-transfer: frozen for a second, then the transfer completes.
#[test]
fn stopped_mid_transfer_resumes_where_it_stood() {
    need_roms!();
    let mut m = booted();
    type_in(&mut m, b"\x93");
    frames(&mut m, 5);
    type_in(&mut m, b"LOAD\"$\",8\r");
    // Run until the drive, now the talker, holds CLK: the KERNAL waits for it without
    // a timeout there, which is the moment a freeze can take without breaking the
    // protocol — the moment the U64 stops its drive in is the host's to choose.
    let mut sink = NullSink;
    let mut talking = false;
    for _ in 0..(FRAME * 400 / 50) {
        m.run_for_full(50, &mut sink, |_, _, _, _, _, _, _| {});
        if m.drive8.ports().motor_on && m.drive8.via1_pb_iec_output() & 0x08 != 0 && m.read_full(0x0090) == 0 {
            // Past the command phase: the C64 has released ATN (CIA2 PA bit 3 low).
            if m.read_full(0xdd00) & 0x08 == 0 {
                talking = true;
                break;
            }
        }
    }
    assert!(talking, "never reached the drive-holds-CLK point:\n{}", screen(&m));

    eprintln!(
        "stop at drive PC ${:04X}, C64 PC ${:04X}, half-track {}, screen:\n{}",
        m.drive8.core.reg_pc,
        m.c64_core.reg_pc,
        m.drive8.half_track(),
        screen(&m).trim_end()
    );
    m.drive8.set_stopped(true);
    let d = &m.drive8;
    let regs = |d: &trx64_core::Drive1541| (d.core.reg_pc, d.core.reg_a, d.core.reg_x, d.core.reg_y, d.core.reg_sp, d.core.clk);
    let vias = |d: &trx64_core::Drive1541| -> Vec<u8> {
        (0x1800u16..0x1810).chain(0x1c00..0x1c10).map(|a| d.drive_peek(a)).collect()
    };
    let rot = |d: &trx64_core::Drive1541| {
        (d.rotation.current_half_track, d.rotation.gcr_head_offset, d.rotation.rotation_last_clk, d.rotation.accum)
    };
    let (r0, v0, o0, p0, out0, data0) =
        (regs(d), vias(d), rot(d), d.ports(), d.via1_pb_iec_output(), m.iec.drv_data(8));

    frames(&mut m, 50);
    let d = &m.drive8;
    assert_eq!(regs(d), r0, "stopped: CPU and clock stand");
    assert_eq!(vias(d), v0, "stopped: VIA registers stand");
    assert_eq!(rot(d), o0, "stopped: the head and the disk stand");
    assert_eq!(d.ports(), p0, "stopped: ports stand");
    assert_eq!(d.via1_pb_iec_output(), out0, "stopped: IEC output kept");
    assert_eq!(m.iec.drv_data(8), data0, "stopped: still driving the bus");
    assert_eq!(m.iec.drive_slot, Some(8), "stopped is not off");

    m.drive8.set_stopped(false);
    assert_eq!(regs(&m.drive8), r0, "released: no reset, same place");
    frames(&mut m, 2);
    assert!(m.drive8.core.clk > r0.5, "released: running again");
    assert!(
        m.drive8.core.clk - r0.5 < 3 * FRAME,
        "released: the stopped second was not replayed ({} drive cycles in 2 frames)",
        m.drive8.core.clk - r0.5
    );
    for _ in 0..1500 {
        frames(&mut m, 1);
        if screen(&m).contains("READY.") {
            break;
        }
    }
    let out = screen(&m);
    assert!(!out.contains("ERROR"), "the transfer failed after the stop:\n{out}");
    assert_lists(&command(&mut m, b"LIST\r", 200), "the transfer after the stop");
}

/// §8.9 — power, held, stopped, connection and unit through a checkpoint; a
/// checkpoint without the node restores the stock drive.
#[test]
fn the_part_state_survives_a_checkpoint() {
    need_roms!();
    use trx64_core::c64re_snapshot::{capture_runtime_checkpoint, restore_runtime_checkpoint};

    let mut m = booted();
    assert_eq!(m.drive8.part(), DrivePart::default(), "a stock drive is the default part");
    m.drive8.set_unit(9).unwrap();
    m.drive8.reset();
    m.drive8.set_reset_held(true); // a reset of its own: latches the jumpers (9)
    m.drive8.set_unit(10).unwrap(); // moved since — not in force until the next reset
    m.drive8.set_reset_line_connected(false);
    m.drive8.set_stopped(true);
    m.drive8.set_power(false);
    frames(&mut m, 2);
    let want = m.drive8.part();
    assert_eq!(
        (want.powered, want.reset_held, want.stopped, want.reset_line_connected, want.unit, want.unit_jumpers),
        (false, true, true, false, 9, 10)
    );
    let cp = capture_runtime_checkpoint(&m, "", "", None, None, None, None);

    let mut m2 = booted();
    restore_runtime_checkpoint(&mut m2, &cp).expect("restore");
    assert_eq!(m2.drive8.part(), want, "the part state round-trips");
    assert_eq!(m2.iec.drive_slot, None, "and the bus map follows it");

    // A checkpoint from before 870: no node → the stock drive, whatever was live.
    let mut old = cp.clone();
    old.as_object_mut().unwrap().remove("drivePart");
    restore_runtime_checkpoint(&mut m2, &old).expect("restore old");
    assert_eq!(m2.drive8.part(), DrivePart::default(), "an old checkpoint restores the defaults");
    assert_eq!(m2.iec.drive_slot, Some(8));
    assert_eq!(m2.iec.iecbus_callback, IecbusCallback::Conf1);
}
