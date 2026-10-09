//! The C64C (issue #7): the custom-IC glue logic and the 8565/8562 grey dot.
//!
//!   1. every row that runs boots to READY — the C64C rows and the others, unchanged;
//!   2. glue type 1 (`c64gluelogic.c`, `GLUE_LOGIC_CUSTOM_IC`): a `$DD00` bank write reaches
//!      the VIC through the intermediate bank for one cycle and then settles, cycle for
//!      cycle as the C computes it; type 0 switches at once;
//!   3. the grey dot (`vicii-draw-cycle.c` `draw_colors_8565`): a colour-register write that
//!      changes nothing puts exactly one colour-15 pixel on the beam, on the 8565 and the
//!      8562, never on the 6569.
//!
//! The glue test restates `c64gluelogic.c` in the test (a model of the C, not a call into
//! `glue.rs`) and compares it with the bank the VIC fetched from in every cycle of a
//! recorded frame (`vic_line_trace`). The dot test works out the pixel from the C — see
//! `dot_x` — and compares it with the frame.
//!
//! The glue and dot tests need no ROMs (the program lives in RAM); the boot test prints a
//! SKIP without them.

use std::path::Path;
use trx64_core::model::{self, C64Model};
use trx64_core::vic_line_trace::{self, FrameWhich, LineTraceFrame};
use trx64_core::{BusKind, Machine, NullSink};

const ROM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../C64ReverseEngineeringMCP/resources/roms");

fn roms() -> bool {
    let ok = Path::new(ROM_DIR).join("kernal-901227-03.bin").exists();
    if !ok {
        eprintln!("SKIP: ROMs absent ({ROM_DIR})");
    }
    ok
}

fn row(name: &str) -> &'static C64Model {
    model::resolve(name).expect("model")
}

fn idle() -> impl FnMut(u16, u8, u8, u8, u8, u8, u64) {
    |_, _, _, _, _, _, _| {}
}

// ── 1 — the rows boot ─────────────────────────────────────────────────────────────────

/// Screen codes of `READY.`.
const READY: [u8; 6] = [0x12, 0x05, 0x01, 0x04, 0x19, 0x2e];

#[test]
fn every_runnable_row_boots_to_ready() {
    if !roms() {
        return;
    }
    let runnable: Vec<&C64Model> = model::models().iter().filter(|m| m.runs()).collect();
    let names: Vec<&str> = runnable.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["c64-pal", "c64-ntsc", "c64-paln", "c64c-pal", "c64c-ntsc"]);
    for m in runnable {
        let mut machine = Machine::new_with_model(m);
        machine.boot_from_dir(Path::new(ROM_DIR)).expect("boot ROMs");
        machine.run_for_full(3_000_000, &mut NullSink, idle());
        let screen = &machine.ram[0x0400..0x0400 + 1000];
        assert!(
            screen.windows(READY.len()).any(|w| w == READY),
            "{}: no READY. on the screen after 3 000 000 cycles",
            m.name
        );
    }
}

// ── 2 — the glue logic ────────────────────────────────────────────────────────────────

/// One CIA2 store of the test program.
#[derive(Clone, Copy, Debug)]
enum Step {
    Pra(u8),
    Ddra(u8),
}

/// `c64gluelogic.c` as the test states it: what a bank request does to the VIC's bank.
/// Returns the bank `perform_vbank_switch` is called with at once (`None` = not now) and
/// whether the alarm is set. `glue_type` is the row's.
fn c_glue_set_vbank(glue_type: u8, old_vbank: u8, vbank: u8, ddr_flag: bool) -> (Option<u8>, bool) {
    let mut new_vbank = vbank;
    let mut update_now = true;
    let mut alarm = false;
    if glue_type == 1 {
        if ((old_vbank ^ vbank) == 3) && ((vbank & vbank.wrapping_sub(1)) == 0) && (vbank != 0) {
            new_vbank = 3;
            alarm = true;
        } else if ddr_flag && (vbank < old_vbank) && ((old_vbank ^ vbank) != 3) {
            update_now = false;
            alarm = true;
        }
    }
    (update_now.then_some(new_vbank), alarm)
}

/// The expected `(from_clk, bank)` timeline for the stores of `steps`, each stamped with the
/// clock it was written at: a store at clk N changes the VIC's bank for the cycle at N+1;
/// the alarm (`maincpu_clk + 1`) runs in the `interrupt_delay` of the CLK_INC that leaves
/// N+1, so the settled bank is the cycle at N+2's.
fn expected_timeline(glue_type: u8, steps: &[(Step, u64)]) -> Vec<(u64, u8)> {
    let (mut pra, mut ddra) = (0x03u8, 0u8); // the program's first store is PRA = $03
    let (mut old_pa, mut old_vbank) = (0xffu8, 0u8);
    let mut events = Vec::new();
    for &(step, n) in steps {
        let ddr_flag = match step {
            Step::Pra(v) => {
                pra = v;
                false
            }
            Step::Ddra(v) => {
                let changed = ddra != v;
                ddra = v;
                changed
            }
        };
        let pa = pra | !ddra;
        if pa == old_pa {
            continue; // store_ciapa is called only when the composed output changed
        }
        old_pa = pa;
        let vbank = (!pa) & 3;
        if vbank == old_vbank {
            continue; // c64cia2.c: `if (new_vbank != vbank)`
        }
        let (now, alarm) = c_glue_set_vbank(glue_type, old_vbank, vbank, ddr_flag);
        if let Some(b) = now {
            events.push((n + 1, b));
        }
        if alarm {
            events.push((n + 2, vbank)); // glue_alarm_handler: perform_vbank_switch(old_vbank)
        }
        old_vbank = vbank;
    }
    events
}

fn bank_at(timeline: &[(u64, u8)], clk: u64) -> u8 {
    timeline.iter().filter(|(from, _)| *from <= clk).last().map(|&(_, b)| b).unwrap_or(0)
}

/// The stores the program makes, in order, and what each does under type 1:
///   0  DDRA = $3F   bank 0, nothing changes (PRA = $03 under it)
///   1  PRA  = $02   0 -> 1: plain
///   2  PRA  = $01   1 -> 2: xor 3, new is a power of two: the intermediate bank 3
///   3  PRA  = $02   2 -> 1: the same
///   4  PRA  = $00   1 -> 3: plain
///   5  DDRA = $3E   3 -> 2 by a DDR write: the "not quite accurate" branch, old bank held
///   6  PRA  = $02   2 -> 0: plain
///   7  PRA  = $00   0 -> 2: plain
///   8  DDRA = $3F   2 -> 3: plain
///   9  PRA  = $03   3 -> 0: xor 3, but the new bank is 0: plain
///  10  PRA  = $00   0 -> 3: plain
///  11  DDRA = $3D   3 -> 1 by a DDR write: the held branch again
///  12  DDRA = $3F   1 -> 3: plain
const STEPS: [Step; 13] = [
    Step::Ddra(0x3f),
    Step::Pra(0x02),
    Step::Pra(0x01),
    Step::Pra(0x02),
    Step::Pra(0x00),
    Step::Ddra(0x3e),
    Step::Pra(0x02),
    Step::Pra(0x00),
    Step::Ddra(0x3f),
    Step::Pra(0x03),
    Step::Pra(0x00),
    Step::Ddra(0x3d),
    Step::Ddra(0x3f),
];

/// `LDA #$03 / STA $DD00` first (PRA = $03 while DDRA is still 0: the port floats, no
/// change), then every step as `LDA #v / STA addr` with `NOP`s between so the effects do
/// not overlap, behind a delay that lands the stores in the middle of the frame.
fn glue_program() -> Vec<u8> {
    let mut p: Vec<u8> = vec![
        0xa9, 0x03, 0x8d, 0x00, 0xdd, // LDA #$03 / STA $DD00
        0xa0, 0x05, // LDY #5
        0xa2, 0x00, // LDX #0
        0xca, // DEX
        0xd0, 0xfd, // BNE -3
        0x88, // DEY
        0xd0, 0xf8, // BNE -8
    ];
    for s in STEPS {
        let (v, addr) = match s {
            Step::Pra(v) => (v, 0xdd00u16),
            Step::Ddra(v) => (v, 0xdd02u16),
        };
        p.extend_from_slice(&[0xa9, v, 0x8d, addr as u8, (addr >> 8) as u8]);
        p.extend_from_slice(&[0xea; 8]);
    }
    let here = 0xc000 + p.len() as u16;
    p.extend_from_slice(&[0x4c, here as u8, (here >> 8) as u8]);
    p
}

/// A machine of `name` standing a few hundred cycles before a frame start, running the
/// program (it has not started yet).
fn at_frame_end(name: &str, program: &[u8]) -> Machine {
    let mut m = Machine::new_with_model(row(name));
    m.write_full(0x0001, 0x37);
    m.poke(0xc000, &[0x4c, 0x00, 0xc0]);
    m.c64_core.reg_pc = 0xc000;
    let cpf = m.timing().cycles_per_frame;
    // Two frames of idling, then to 400 cycles before the next frame start.
    m.run_for_full(2 * cpf, &mut NullSink, idle());
    while vic_line_trace::frame_position(&m) < cpf - 400 {
        m.run_for_full_capped(1, 1, &mut NullSink, idle());
    }
    m.poke(0xc000, program);
    m.c64_core.reg_pc = 0xc000;
    m
}

fn record(m: &Machine) -> LineTraceFrame {
    let mut scratch = m.clone();
    let start = vic_line_trace::next_frame_start(m);
    vic_line_trace::record_frame(&mut scratch, start, FrameWhich::Next, None).expect("record the frame")
}

fn check_glue(name: &str, glue_type: u8) {
    let m = at_frame_end(name, &glue_program());
    assert_eq!(m.model().glue_kind(), glue_type, "{name}: the glue type is the row's");
    let f = record(&m);
    // The CIA2 stores of the steps, with the clock they were made at.
    let stores: Vec<(Step, u64)> = {
        // A store that changes the composed port output also shows up as a bus write of
        // the new output to $DD00 (the serial bus is driven from it); the step itself is
        // the write of its own value to its own register.
        let mut writes = f.accesses.iter().filter(|a| a.kind == BusKind::Write && (a.addr == 0xdd00 || a.addr == 0xdd02));
        let mut out = Vec::new();
        for s in STEPS {
            let (want_addr, want_v) = match s {
                Step::Pra(v) => (0xdd00, v),
                Step::Ddra(v) => (0xdd02, v),
            };
            let w = writes
                .by_ref()
                .find(|w| w.addr == want_addr && w.value == want_v)
                .unwrap_or_else(|| panic!("{name}: the store {s:?} is not in the frame"));
            out.push((s, w.clk));
        }
        out
    };
    let timeline = expected_timeline(glue_type, &stores);
    // Type 1 has the alarm in steps 2, 3, 5 and 11: two events for the first two (the
    // intermediate bank and the settled one), one for the other two (the settled one);
    // type 0 has one event per changing store.
    assert_eq!(timeline.len(), if glue_type == 1 { 14 } else { 12 }, "{timeline:?}");
    let first = stores[0].1;
    let last = stores[STEPS.len() - 1].1;
    let mut compared = 0;
    let mut deviations = 0;
    for c in f.cycles.iter().filter(|c| c.clk >= first - 4 && c.clk <= last + 6) {
        let want = bank_at(&timeline, c.clk) as u16 * 0x4000;
        assert_eq!(
            c.vbank, want,
            "{name}: cycle at clk {} (line {} cycle {}): the VIC fetched from bank ${:04x}, c64gluelogic.c gives ${want:04x}\n{timeline:?}",
            c.clk, c.line, c.cycle, c.vbank
        );
        // How many cycles are on a bank that is neither the settled one nor the previous.
        if c.clk >= first
            && stores.iter().any(|&(_, n)| c.clk == n + 1)
            && bank_at(&timeline, c.clk) != bank_at(&timeline, c.clk + 1)
        {
            deviations += 1;
        }
        compared += 1;
    }
    assert!(compared > 100, "{name}: compared {compared} cycles");
    if glue_type == 1 {
        assert_eq!(deviations, 4, "{name}: type 1 shows its four one-cycle banks");
    } else {
        assert_eq!(deviations, 0, "{name}: type 0 switches at once");
    }
}

#[test]
fn the_custom_ic_glue_goes_through_the_intermediate_bank_for_one_cycle() {
    check_glue("c64c-pal", 1);
    check_glue("c64c-ntsc", 1);
}

#[test]
fn the_discrete_glue_switches_at_once() {
    check_glue("c64-pal", 0);
    check_glue("c64-ntsc", 0);
}

/// Spelled out, so a mistake in the model above cannot hide the rule: one `PRA` write 1 -> 2
/// shows bank 3 for exactly one cycle on the custom IC, and not at all on the discrete glue.
#[test]
fn a_bank_1_to_2_write_shows_bank_3_for_exactly_one_cycle() {
    for (name, custom) in [("c64c-pal", true), ("c64-pal", false)] {
        let mut m = Machine::new_with_model(row(name));
        m.write_full(0x0001, 0x37);
        m.write_full(0xdd00, 0x03);
        m.write_full(0xdd02, 0x3f);
        m.write_full(0xdd00, 0x02); // bank 1
        // Settled after the cycles the machine ran since (none yet: the alarm is pending).
        let after_first = m.vic_bank_base();
        m.write_full(0xdd00, 0x01); // bank 2: xor 3, power of two
        let right_after = m.vic_bank_base();
        // The host store is at the same clock as the first, so the alarm of the first (if
        // any) was never run: type 1 has it pending from the 0 -> 1? No: that is plain.
        assert_eq!(after_first, 0x4000, "{name}");
        if custom {
            assert_eq!(right_after, 0xc000, "{name}: the intermediate bank");
            m.run_for_full_capped(2, 1, &mut NullSink, idle());
            // A `JMP *` is three cycles: the alarm has run.
            assert_eq!(m.vic_bank_base(), 0x8000, "{name}: settled on bank 2");
        } else {
            assert_eq!(right_after, 0x8000, "{name}: no intermediate bank");
        }
    }
}

// ── the glue state rides checkpoints and snapshots ────────────────────────────────────

fn with_pending_alarm(name: &str) -> Machine {
    let mut m = Machine::new_with_model(row(name));
    m.write_full(0x0001, 0x37);
    m.write_full(0xdd00, 0x03);
    m.write_full(0xdd02, 0x3f);
    m.write_full(0xdd00, 0x02); // bank 1
    m.write_full(0xdd00, 0x01); // bank 2: intermediate bank 3 pending
    assert_eq!(m.vic_bank_base(), 0xc000, "{name}: the alarm is pending");
    m
}

fn run_a_few_cycles(m: &mut Machine) {
    m.poke(0xc000, &[0x4c, 0x00, 0xc0]);
    m.c64_core.reg_pc = 0xc000;
    m.run_for_full_capped(2, 1, &mut NullSink, idle());
}

#[test]
fn a_pending_alarm_rides_the_checkpoint() {
    let m = with_pending_alarm("c64c-pal");
    let cp = trx64_core::c64re_snapshot::capture_runtime_checkpoint(&m, "", "", None, None, None, None);
    assert!(cp.get("glue").is_some_and(|g| g["alarmClk"].is_u64() && g["vbank"] == 3), "{}", cp["glue"]);

    let mut fresh = Machine::new_with_model(row("c64c-pal"));
    trx64_core::c64re_snapshot::restore_runtime_checkpoint(&mut fresh, &cp).expect("restore");
    assert_eq!(fresh.vic_bank_base(), 0xc000, "the intermediate bank came back");
    assert!(fresh.vic.glue.alarm.is_some());
    run_a_few_cycles(&mut fresh);
    assert_eq!(fresh.vic_bank_base(), 0x8000, "and settles");
    assert!(fresh.vic.glue.alarm.is_none());

    // Without a pending alarm there is no node, so such a checkpoint is the one it was.
    let cp2 = trx64_core::c64re_snapshot::capture_runtime_checkpoint(&fresh, "", "", None, None, None, None);
    assert!(cp2.get("glue").is_none());

    // A checkpoint from before the glue existed loads with no pending alarm, and the
    // requested bank is the one CIA2's port A names.
    let mut old = cp.clone();
    old.as_object_mut().unwrap().remove("glue");
    let mut m3 = with_pending_alarm("c64c-pal");
    trx64_core::c64re_snapshot::restore_runtime_checkpoint(&mut m3, &old).expect("restore");
    assert!(m3.vic.glue.alarm.is_none());
    assert_eq!(m3.vic_bank_base(), 0x8000);
    assert_eq!(m3.vic.glue.old_vbank, 2);
}

#[test]
fn the_vice_snapshot_carries_the_glue_module() {
    let mut m = with_pending_alarm("c64c-pal");
    let bytes = trx64_core::vsf_export::save_vice_vsf(&mut m);
    let at = bytes.windows(7).position(|w| w == b"C64GLUE").expect("C64GLUE module");
    // name[16] major minor size[4] | type old_vbank alarm_active
    assert_eq!(&bytes[at + 16..at + 18], &[1, 1], "version 1.1");
    assert_eq!(&bytes[at + 22..at + 25], &[1, 2, 1], "custom IC, old vbank 2, alarm active");

    let mut back = Machine::new_with_model(row("c64c-pal"));
    trx64_core::vsf::load_vsf(&mut back, &bytes).expect("load");
    assert!(back.vic.glue.alarm.is_some(), "VICE sets the alarm again, one cycle out");
    assert_eq!(back.vic.glue.old_vbank, 2);
    // A snapshot without a pending alarm loads with none.
    let mut quiet = Machine::new_with_model(row("c64c-pal"));
    quiet.write_full(0xdd00, 0x01);
    let bytes = trx64_core::vsf_export::save_vice_vsf(&mut quiet);
    let mut back2 = with_pending_alarm("c64c-pal");
    trx64_core::vsf::load_vsf(&mut back2, &bytes).expect("load");
    assert!(back2.vic.glue.alarm.is_none());
}

/// Spec 863 — the row decides the glue: a switch to a C64C and back changes the type, and
/// a bank write behaves by the type that is in force.
#[test]
fn a_model_switch_switches_the_glue_type() {
    let mut m = Machine::new_with_model(row("c64-pal"));
    assert_eq!(m.vic.glue.kind, 0);
    m.switch_model(row("c64c-pal")).expect("to the C64C");
    assert_eq!(m.vic.glue.kind, 1);
    m.write_full(0xdd00, 0x03);
    m.write_full(0xdd02, 0x3f);
    m.write_full(0xdd00, 0x02);
    m.write_full(0xdd00, 0x01);
    assert_eq!(m.vic_bank_base(), 0xc000, "custom IC: the intermediate bank");
    run_a_few_cycles(&mut m);
    m.switch_model(row("c64-pal")).expect("and back");
    assert_eq!(m.vic.glue.kind, 0);
    m.write_full(0xdd00, 0x02);
    assert_eq!(m.vic_bank_base(), 0x4000, "discrete: at once");
    m.write_full(0xdd00, 0x01);
    assert_eq!(m.vic_bank_base(), 0x8000, "discrete: no intermediate bank");
}

// ── 3 — the grey dot ──────────────────────────────────────────────────────────────────

/// The pixel `draw_colors_8565` puts the dot on, from the C. A colour-register store at
/// clk N reaches `vicii.last_color_reg` between the VIC cycle of N and the next. The next
/// cycle's `draw_colors8` does not know it yet (`update_cregs` takes it at the END of that
/// call); the one after runs `cregs[last_color_reg] = ...` and then, for pixel 0 only,
/// compares the pixel's token with `last_color_reg`. So the dot is pixel 0 of the 8 that
/// the VIC cycle at clk N+2 draws, which sit at `dbuf_offset = (raster_cycle - 1) * 8`
/// (`vicii_draw_cycle` resets it on raster cycle 1, `draw_colors8` adds 8): with
/// `raster_cycle` 0-based as `vicii.raster_cycle` is, and the store made while the VIC is
/// at `r`, that is `(r + 1) * 8`.
fn dot_x(raster_cycle_at_store: usize) -> usize {
    (raster_cycle_at_store + 1) * 8
}

/// One recorded frame of the dot program.
struct Run {
    canvas: Vec<u8>,
    width: usize,
    /// The line and the VIC cycle (0-based, `vicii.raster_cycle`) the store was made in.
    line: u16,
    raster_cycle: usize,
    /// Where the recorder says the VIC cycle at N+2 drew its 8 pixels.
    recorded_fb_x: usize,
    /// The canvas position of the dot, from the C (`dot_x`).
    at: (usize, usize),
    pad: usize,
}

/// `LDY / LDX / DEX / BNE / DEY / BNE` as in the glue program, `pad` NOPs, then
/// `LDA #value / STA target / JMP *`.
fn dot_program(target: u16, value: u8, pad: usize) -> Vec<u8> {
    let mut p: Vec<u8> = vec![0xa0, 0x04, 0xa2, 0x00, 0xca, 0xd0, 0xfd, 0x88, 0xd0, 0xf8];
    p.extend(std::iter::repeat(0xea).take(pad));
    p.extend_from_slice(&[0xa9, value, 0x8d, target as u8, (target >> 8) as u8]);
    let here = 0xc000 + p.len() as u16;
    p.extend_from_slice(&[0x4c, here as u8, (here >> 8) as u8]);
    p
}

/// The store to `reg` (or, for the control, to RAM at $0200) in a visible part of a line. The
/// picture is read from the scratch machine that recorded the frame.
fn run_dot(name: &str, reg: u16, value: u8, sprite_x: Option<u16>, to_register: bool) -> Run {
    let w = row(name).window;
    for pad in 0..60 {
        let program = dot_program(if to_register { reg } else { 0x0200 }, value, pad);
        let mut m = Machine::new_with_model(row(name));
        m.write_full(0x0001, 0x37);
        // Border and background black, display off (D011 = 0): all of the line is border.
        m.poke_io(0xd020, &[0, 0]);
        if let Some(x) = sprite_x {
            // The display window open (the border would cover the sprite); an empty text
            // screen shows background everywhere. Sprite 0: multicolour, expanded, every
            // pixel `01` = $D025.
            m.poke_io(0xd011, &[0x1b]);
            m.poke(0x2000, &[0x55; 63]);
            m.poke(0x07f8, &[0x80]);
            m.poke_io(0xd018, &[0x14]);
            m.poke_io(0xd000, &[x as u8, 50]);
            m.poke_io(0xd010, &[(x >> 8) as u8]);
            m.poke_io(0xd015, &[1]);
            m.poke_io(0xd01c, &[1]);
            m.poke_io(0xd01d, &[1]);
            m.poke_io(0xd017, &[1]); // 42 lines tall: the store's line is inside it
            m.poke_io(0xd025, &[5, 6]);
            m.poke_io(0xd027, &[7]);
        }
        let m = {
            let mut m = m;
            m.poke(0xc000, &[0x4c, 0x00, 0xc0]);
            m.c64_core.reg_pc = 0xc000;
            let cpf = m.timing().cycles_per_frame;
            m.run_for_full(2 * cpf, &mut NullSink, idle());
            while vic_line_trace::frame_position(&m) < cpf - 400 {
                m.run_for_full_capped(1, 1, &mut NullSink, idle());
            }
            m.poke(0xc000, &program);
            m.c64_core.reg_pc = 0xc000;
            m
        };
        let mut scratch = m.clone();
        let start = vic_line_trace::next_frame_start(&m);
        let f = vic_line_trace::record_frame(&mut scratch, start, FrameWhich::Next, None).expect("record");
        let store = f
            .accesses
            .iter()
            .find(|a| a.kind == BusKind::Write && a.addr == if to_register { reg } else { 0x0200 })
            .expect("the store");
        let c = f.cycles.iter().find(|c| c.clk == store.clk).expect("the store's cycle");
        let n2 = f.cycles.iter().find(|c| c.clk == store.clk + 2).expect("the cycle two after");
        let r = c.cycle as usize - 1;
        // A visible part of the line; with a sprite, well inside the display window.
        let wanted = if sprite_x.is_some() { 26..=44 } else { 14..=50 };
        if !wanted.contains(&r) {
            continue;
        }
        let (width, _h, canvas) = scratch.render_canvas_indices();
        return Run {
            canvas,
            width,
            line: c.line,
            raster_cycle: r,
            recorded_fb_x: n2.fb_x as usize,
            at: (dot_x(r) - w.x0(), w.row_of_line(c.line) as usize - w.first_line as usize),
            pad,
        };
    }
    panic!("{name}: no padding puts the store in a visible part of a line");
}

fn grey_pixels(r: &Run) -> Vec<(usize, usize)> {
    r.canvas.iter().enumerate().filter(|(_, &c)| c == 15).map(|(i, _)| (i % r.width, i / r.width)).collect()
}

fn differences(a: &Run, b: &Run) -> Vec<(usize, usize)> {
    assert_eq!(a.canvas.len(), b.canvas.len());
    a.canvas
        .iter()
        .zip(&b.canvas)
        .enumerate()
        .filter(|(_, (x, y))| x != y)
        .map(|(i, _)| (i % a.width, i / a.width))
        .collect()
}

#[test]
fn a_redundant_d020_write_puts_one_grey_pixel_on_the_8565_and_8562() {
    for name in ["c64c-pal", "c64c-ntsc"] {
        let d = run_dot(name, 0xd020, 0x00, None, true);
        let control = run_dot(name, 0xd020, 0x00, None, false);
        assert_eq!(d.raster_cycle, control.raster_cycle, "{name}: the control stores at the same cycle");
        let x0 = row(name).window.x0();
        assert_eq!(d.recorded_fb_x, dot_x(d.raster_cycle), "{name}: the draw of the VIC cycle two after the store is where the C puts it");
        assert_eq!(grey_pixels(&d), [d.at], "{name}: one pixel of colour 15, at the beam position of the store");
        assert_eq!(differences(&d, &control), [d.at], "{name}: and nothing else differs from the same run storing to RAM");
        eprintln!(
            "{name}: $D020 stored on line {} in VIC cycle {} (0-based {}, pad {}): grey dot at draw-buffer x {} = canvas ({}, {})",
            d.line, d.raster_cycle + 1, d.raster_cycle, d.pad, dot_x(d.raster_cycle), d.at.0, d.at.1
        );
        let _ = x0;
    }
}

#[test]
fn the_6569_and_6567_have_no_grey_dot() {
    for name in ["c64-pal", "c64-ntsc", "c64-paln"] {
        let d = run_dot(name, 0xd020, 0x00, None, true);
        let control = run_dot(name, 0xd020, 0x00, None, false);
        assert!(grey_pixels(&d).is_empty(), "{name}: {:?}", grey_pixels(&d));
        assert!(differences(&d, &control).is_empty(), "{name}: the write changes nothing");
    }
}

/// `$D025` (sprite multicolour 0) under a sprite whose pixels are all `01`: the dot replaces
/// one of the sprite's pixels.
#[test]
fn a_redundant_d025_write_puts_one_grey_pixel_on_the_sprite() {
    for name in ["c64c-pal", "c64c-ntsc"] {
        // Where does the sprite stand for X = 100? Measured in the control run.
        let probe = run_dot(name, 0xd025, 5, Some(100), false);
        let y = probe.at.1;
        let green: Vec<usize> = (0..probe.width).filter(|&x| probe.canvas[y * probe.width + x] == 5).collect();
        assert!(!green.is_empty(), "{name}: the sprite shows on canvas row {y}");
        let centre = (green[0] + green[green.len() - 1]) / 2;
        // Move the sprite so that its middle is under the dot.
        let sprite_x = (100 + probe.at.0 as i32 - centre as i32) as u16;
        let d = run_dot(name, 0xd025, 5, Some(sprite_x), true);
        let control = run_dot(name, 0xd025, 5, Some(sprite_x), false);
        assert_eq!(d.at, control.at);
        assert_eq!(d.recorded_fb_x, dot_x(d.raster_cycle), "{name}");
        assert_eq!(grey_pixels(&d), [d.at], "{name}: one grey pixel at the beam position (sprite X {sprite_x})");
        assert_eq!(differences(&d, &control), [d.at], "{name}: and nothing else differs");
        assert_eq!(control.canvas[d.at.1 * control.width + d.at.0], 5, "{name}: the pixel under the dot is the sprite's $D025 colour");
        eprintln!("{name}: $D025 stored on line {} in VIC cycle {}: grey dot on the sprite at canvas {:?}", d.line, d.raster_cycle + 1, d.at);
    }
}

#[test]
fn the_6569_has_no_grey_dot_on_a_sprite_either() {
    for name in ["c64-pal", "c64-ntsc"] {
        let probe = run_dot(name, 0xd025, 5, Some(100), false);
        let y = probe.at.1;
        let green: Vec<usize> = (0..probe.width).filter(|&x| probe.canvas[y * probe.width + x] == 5).collect();
        let centre = (green[0] + green[green.len() - 1]) / 2;
        let sprite_x = (100 + probe.at.0 as i32 - centre as i32) as u16;
        let d = run_dot(name, 0xd025, 5, Some(sprite_x), true);
        assert!(grey_pixels(&d).is_empty(), "{name}: {:?}", grey_pixels(&d));
        assert!(differences(&d, &probe_for(name, sprite_x)).is_empty());
    }
}

fn probe_for(name: &str, sprite_x: u16) -> Run {
    run_dot(name, 0xd025, 5, Some(sprite_x), false)
}

/// A colour store lands where VICE's draw puts it. The store waits in `last_color_reg`; the
/// draw after the store's takes it up, and the one after that applies it (`draw_colors8`).
/// On the 6569 `draw_colors_6569` resolves a pixel behind the token, so that draw's pixel 0 is
/// still the old colour and the new one starts at pixel 1: draw-buffer x `(r + 1) * 8 + 1`.
/// On the 8565 `draw_colors_8565` resolves in place: pixel 0 is the grey dot, the new colour
/// starts at pixel 1 as well. So the two chips change colour at the same pixel, and the
/// 8565 has the dot in front of it.
#[test]
fn a_colour_store_changes_the_colour_where_vice_puts_it_on_both_chips() {
    for (pal, c) in [("c64-pal", "c64c-pal"), ("c64-ntsc", "c64c-ntsc")] {
        let a = run_dot(pal, 0xd020, 1, None, true);
        let b = run_dot(c, 0xd020, 1, None, true);
        assert_eq!(a.raster_cycle, b.raster_cycle, "the same program stores in the same cycle");
        let x0 = row(pal).window.x0();
        let (_, y) = a.at;
        let first_new = |r: &Run| (0..r.width).find(|&x| r.canvas[y * r.width + x] != 0).unwrap();
        let want = dot_x(a.raster_cycle) + 1 - x0;
        assert_eq!(first_new(&a), want, "{pal}: the 6569 draws the new colour from pixel 1 of the second draw");
        assert_eq!(first_new(&b), want - 1, "{c}: the 8565's first changed pixel is the grey dot");
        assert_eq!(b.canvas[y * b.width + want - 1], 15);
        // Everything else is the same picture.
        let mut undotted = b.canvas.clone();
        undotted[y * b.width + want - 1] = 0;
        assert!(undotted == a.canvas, "{pal} and {c} differ by more than the dot");
        assert!((0..a.width).all(|x| x < want || a.canvas[y * a.width + x] == 1), "{pal}: the new colour holds to the end of the line");
    }
}
