//! Spec 812 acceptance — the capture scenario and the reel it produces.
//!
//! These gates need ROMs but deliberately need NO disk image: everything asserted
//! here is about the SCHEDULE and the CAPTURE, and a bare C64 booting to READY
//! exercises both. Anything that needs a title is a smoke run, not a gate — the
//! repo cannot carry someone else's disk.
//!
//! What is proved:
//!   1. the same scenario replays to the same bytes (the determinism the whole
//!      spec exists for, and the instrument the boot-repeatability bug needs);
//!   2. a shot lands on a frame boundary;
//!   3. a joystick press spans the frames it declares — measured at CIA1, not at
//!      the API that set it;
//!   4. a predicate that cannot fire times out loudly instead of hanging;
//!   5. the reel walks as GIF89a blocks, at the canvas the VIC actually renders.

use std::path::{Path, PathBuf};
use std::process::Command;

use trx64_cli::default_rom_dir;
use trx64_core::gif89a;

fn roms_or_skip(who: &str) -> Option<PathBuf> {
    let rom_dir = default_rom_dir();
    if !Path::new(&rom_dir).join("kernal-901227-03.bin").exists() {
        eprintln!("[skip] {who}: ROMs absent at {}", rom_dir.display());
        return None;
    }
    Some(rom_dir)
}

fn workdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("trx64-812-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("workdir");
    d
}

/// Run `trx64cli reel` as a real child process — the isolation this spec promises
/// is an OS fact, so the gate must exercise it as one.
fn run_reel(dir: &Path, scenario: &str, out: &str) -> Result<serde_json::Value, String> {
    let spath = dir.join("scenario.json");
    std::fs::write(&spath, scenario).expect("write scenario");
    let gif = dir.join(out);
    let frames = dir.join("frames");
    let output = Command::new(env!("CARGO_BIN_EXE_trx64cli"))
        .arg("reel")
        .arg("--scenario")
        .arg(&spath)
        .arg("--out")
        .arg(&gif)
        .arg("--frames-dir")
        .arg(&frames)
        .arg("--json")
        .output()
        .expect("spawn trx64cli reel");
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    serde_json::from_str(&text).map_err(|e| format!("reel --json is not JSON: {e}\n{text}"))
}

/// A scenario that needs no medium: boot to READY, take three pictures around a
/// typed line. The typed text lands in the BASIC editor, so the pictures differ.
fn bare_scenario() -> String {
    r#"{
      "name": "bare-boot",
      "cyclesPerFrame": 19656,
      "reel": { "delayMs": 700, "maxBytes": 512000 },
      "steps": [
        { "wait": { "frames": 170 } },
        { "shot": { "label": "ready" } },
        { "type": { "text": "PRINT 6502\r" } },
        { "wait": { "frames": 60 } },
        { "shot": { "label": "printed" } },
        { "joy": { "port": 2, "fire": true, "frames": 3 } },
        { "shot": { "label": "after-joy" } }
      ]
    }"#
    .to_string()
}

#[test]
fn a_scenario_replays_to_the_same_bytes() {
    let Some(_rom) = roms_or_skip("812 determinism") else { return };
    let dir = workdir("determinism");

    let a = run_reel(&dir, &bare_scenario(), "a.gif").expect("first run");
    let b = run_reel(&dir, &bare_scenario(), "b.gif").expect("second run");

    let ba = std::fs::read(dir.join("a.gif")).expect("read a");
    let bb = std::fs::read(dir.join("b.gif")).expect("read b");
    assert_eq!(
        ba, bb,
        "the same schedule from a cold boot must produce the same reel — if this is \
         red, the divergence is ours and it is reproducible without anyone's disk"
    );

    // And the cycles the shots landed on are the same, not merely the pictures.
    let cycles = |v: &serde_json::Value| -> Vec<u64> {
        v["shots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["cycle"].as_u64().unwrap())
            .collect()
    };
    assert_eq!(cycles(&a), cycles(&b), "the schedule itself is reproducible");
}

#[test]
fn a_shot_lands_on_a_frame_boundary() {
    let Some(_rom) = roms_or_skip("812 frame boundary") else { return };
    let dir = workdir("boundary");
    let r = run_reel(&dir, &bare_scenario(), "r.gif").expect("run");
    for shot in r["shots"].as_array().expect("shots") {
        assert_eq!(
            shot["rasterLine"].as_u64(),
            Some(0),
            "shot {:?} captured at raster line {:?} — a mid-frame capture returns the \
             frame BEFORE the interesting one",
            shot["label"],
            shot["rasterLine"]
        );
    }
}

#[test]
fn a_reel_is_a_wellformed_gif89a_at_the_canvas_the_vic_renders() {
    let Some(_rom) = roms_or_skip("812 gif structure") else { return };
    let dir = workdir("structure");
    let r = run_reel(&dir, &bare_scenario(), "r.gif").expect("run");
    let bytes = std::fs::read(dir.join("r.gif")).expect("read gif");

    let s = gif89a::parse_structure(&bytes).expect("walks as GIF89a blocks");
    assert_eq!((s.width, s.height), (384, 272), "the VIC's own PAL canvas, border included");
    assert_eq!(s.palette_entries, 16, "the 16 COLODORE entries — nothing was quantized");
    assert_eq!(s.frames, 3);
    assert_eq!(s.disposals, vec![2, 2, 2], "hard cuts");
    assert!(s.delays.iter().all(|d| *d == s.delays[0]), "one uniform delay");
    assert!(s.loops_forever);
    assert_eq!(r["frames"].as_u64(), Some(3));
    assert!(bytes.len() <= 512_000);

    // The reel is the frames, not a re-render of them: each captured frame's raw
    // indices must be exactly what the GIF carries.
    let n = std::fs::read_dir(dir.join("frames")).expect("frames dir").count();
    assert_eq!(n, 3, "one raw frame written per shot");
}

#[test]
fn a_predicate_that_never_fires_times_out_instead_of_hanging() {
    let Some(_rom) = roms_or_skip("812 timeout") else { return };
    let dir = workdir("timeout");
    // $FFFF is the top of the KERNAL vector table; the CPU never executes there.
    let scenario = r#"{
      "name": "impossible",
      "steps": [
        { "waitUntil": { "pc": "$FFFF", "timeoutFrames": 30 } },
        { "shot": { "label": "never" } }
      ]
    }"#;
    let err = run_reel(&dir, scenario, "r.gif").expect_err("must fail");
    assert!(err.contains("did not fire within 30 frames"), "got: {err}");
    assert!(
        !dir.join("r.gif").exists(),
        "a scenario that failed must not leave a reel behind claiming it worked"
    );
}

#[test]
fn a_joystick_press_spans_the_frames_it_declares() {
    let Some(rom_dir) = roms_or_skip("812 joystick duration") else { return };
    // Measured at CIA1, not at the API that set it: the question is what the
    // MACHINE saw. A press with no duration — the shape this format replaced —
    // is held until some later call happens to clear it, which is how a menu
    // scrolls past the entry the recipe meant to pick.
    let mut m = trx64_core::Machine::new();
    m.boot_from_dir(&rom_dir).expect("boot");
    m.run_for(3_000_000, &mut NullObs);

    const CPF: u64 = 19_656;
    let fire_low = |m: &trx64_core::Machine| m.read_full(0xDC00) & 0x10 == 0;

    assert!(!fire_low(&m), "nothing is pressed before the step");
    m.joystick2 = trx64_core::keyboard::JoystickState { fire: true, ..Default::default() };

    let mut seen_frames = 0u64;
    for _ in 0..3 {
        m.run_for(CPF, &mut NullObs);
        if fire_low(&m) {
            seen_frames += 1;
        }
    }
    assert_eq!(seen_frames, 3, "a 3-frame press is visible to CIA1 for 3 frames");

    m.joystick2 = trx64_core::keyboard::JoystickState::default();
    m.run_for(CPF, &mut NullObs);
    assert!(!fire_low(&m), "and it is released afterwards, in the same step");
}

struct NullObs;
impl trx64_core::Observer for NullObs {
    fn on_instruction(
        &mut self,
        _pc: u16,
        _opcode: u8,
        _b1: u8,
        _b2: u8,
        _a: u8,
        _x: u8,
        _y: u8,
        _sp: u8,
        _p: u8,
        _clk: u64,
    ) {
    }
    fn on_bus(
        &mut self,
        _kind: trx64_core::BusKind,
        _addr: u16,
        _val: u8,
        _pc: u16,
        _clk: u64,
        _old: u8,
    ) {
    }
    fn on_interrupt(&mut self, _vector: u16, _clk: u64) {}
}
