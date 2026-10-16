//! A freeze press, swept over many cycles, on the real Action Replay V5 PAL image: the
//! program must survive every one. Skips without the corpus.
//!
//!   cargo test --release -p trx64-core --test action_replay_sweep -- --nocapture

use std::path::PathBuf;
use trx64_core::{Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");
const IMAGE: &str = "/Users/alex/Development/C64/Tools/Cartridges/Action_Replay/Action_Replay_V5.0_5.0_1988_PAL.crt";

macro_rules! skip {
    ($($why:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), "SKIP: {} — {}", module_path!(), format!($($why)*));
        return;
    }};
}

fn run(m: &mut Machine, c: u64) {
    m.run_for_full_capped(c, u64::MAX, &mut NullSink, |_, _, _, _, _, _, _| {});
}
fn tap(m: &mut Machine, k: &str) {
    m.keyboard.key_down(k);
    run(m, 150_000);
    m.keyboard.key_up(k);
    run(m, 150_000);
}
fn screen_has(m: &Machine, text: &str) -> bool {
    (0..25).any(|r| {
        let row: String = (0..40)
            .map(|c| match m.peek_lens(0x0400 + r * 40 + c, "ram") & 0x7f {
                s @ 1..=26 => (b'A' + s - 1) as char,
                s @ 32..=63 => s as char,
                _ => '#',
            })
            .collect();
        row.contains(text)
    })
}

fn base(program: impl FnOnce(&mut Machine)) -> Option<Machine> {
    let rom = std::env::var("TRX64_ROM_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(ROM_DIR));
    let crt = std::fs::read(std::env::var("TRX64_AR_IMAGE").unwrap_or_else(|_| IMAGE.into())).ok()?;
    if !rom.join("kernal-901227-03.bin").exists() {
        return None;
    }
    let mut m = Machine::new();
    m.boot_from_dir(&rom).unwrap();
    m.attach_cart_from_bytes(&crt, "ar").unwrap();
    m.cold_reset();
    run(&mut m, 985_248 * 3);
    tap(&mut m, "F3"); // NORMAL RESET: out of the cartridge's own screen, into BASIC
    run(&mut m, 985_248 * 2);
    program(&mut m);
    run(&mut m, 985_248 * 2);
    Some(m)
}

fn counter16(m: &Machine, a: u16) -> u32 {
    m.read_full(a) as u32 | (m.read_full(a + 1) as u32) << 8
}

/// Press `k` cycles in, wait for the menu, F3, and see that the program runs on.
fn survives(base: &Machine, k: u64, ctr: u16) -> Result<(), String> {
    let mut m = base.clone();
    run(&mut m, k);
    m.cart_press_button("freeze").unwrap();
    run(&mut m, 1_500_000);
    if !screen_has(&m, "F3 - RESTART") {
        return Err("no freezer menu".into());
    }
    tap(&mut m, "F3");
    run(&mut m, 2_000_000);
    let c0 = counter16(&m, ctr);
    run(&mut m, 1_000_000);
    let c1 = counter16(&m, ctr);
    if screen_has(&m, "SYNTAX") {
        return Err("?SYNTAX ERROR".into());
    }
    if c1 == c0 {
        return Err(format!("the counter stands at {c0}"));
    }
    Ok(())
}

/// `10 A=A+1:POKE49152,A-INT(A/256)*256:POKE49153,INT(A/256):GOTO10`, counting in $C000/1.
fn basic_counter(m: &mut Machine) {
    let mut body: Vec<u8> = vec![];
    body.extend([0x41, 0xB2, 0x41, 0xAA, 0x31, 0x3A]);
    body.extend([0x97, 0x34, 0x39, 0x31, 0x35, 0x32, 0x2C, 0x41, 0xAB, 0xB5, 0x28, 0x41, 0xAD, 0x32, 0x35, 0x36, 0x29, 0xAC, 0x32, 0x35, 0x36, 0x3A]);
    body.extend([0x97, 0x34, 0x39, 0x31, 0x35, 0x33, 0x2C, 0xB5, 0x28, 0x41, 0xAD, 0x32, 0x35, 0x36, 0x29, 0x3A]);
    body.extend([0x89, 0x31, 0x30, 0x00]);
    let next = 0x0801 + 4 + body.len() as u16;
    let mut p = vec![(next & 0xff) as u8, (next >> 8) as u8, 10, 0];
    p.extend(body);
    p.extend([0, 0]);
    m.poke(0x0801, &p);
    let end = 0x0801 + p.len() as u16;
    for a in [0x2d, 0x2f, 0x31] {
        m.poke(a, &[(end & 0xff) as u8, (end >> 8) as u8]);
    }
    for k in ["R", "U", "N", "RETURN"] {
        tap(m, k);
    }
}

/// A machine-code loop that keeps calling a KERNAL routine (GETIN, at $F13E) with the IRQ on,
/// counting in $C100/1: the CPU spends its time at $E000+.
fn kernal_loop(m: &mut Machine) {
    m.poke(
        0xc000,
        &[
            0x20, 0xe4, 0xff, // loop: JSR $FFE4
            0xee, 0x00, 0xc1, // INC $C100
            0xd0, 0xf8, // BNE loop
            0xee, 0x01, 0xc1, // INC $C101
            0x4c, 0x00, 0xc0, // JMP loop
        ],
    );
    m.c64_core.reg_pc = 0xc000;
}

fn sweep(base: &Machine, ctr: u16, what: &str) {
    let mut broken = vec![];
    let mut n = 0;
    // 62 presses 31 cycles apart: every phase of the loops, and an IRQ in some of them
    for k in (0..62 * 31).step_by(31) {
        n += 1;
        if let Err(e) = survives(base, k, ctr) {
            broken.push(format!("+{k}: {e}"));
        }
    }
    eprintln!("{what}: {} broken of {n}", broken.len());
    assert!(broken.is_empty(), "{what}: {broken:?}");
}

#[test]
fn a_press_in_a_basic_loop_never_breaks_the_program() {
    let Some(b) = base(basic_counter) else { skip!("no corpus image or ROMs") };
    assert!(counter16(&b, 0xc000) > 0);
    sweep(&b, 0xc000, "BASIC counter");
}

#[test]
fn a_press_while_the_cpu_runs_kernal_code_never_breaks_the_program() {
    let Some(b) = base(kernal_loop) else { skip!("no corpus image or ROMs") };
    assert!(counter16(&b, 0xc100) > 0);
    sweep(&b, 0xc100, "KERNAL loop");
}
