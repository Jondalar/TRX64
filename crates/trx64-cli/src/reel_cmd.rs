//! `trx64cli reel` — Spec 812: run a capture scenario in an isolated process and
//! write the reel it produces.
//!
//! The problem this exists to remove: a capture recipe used to read *"press down,
//! then run about two million instructions"*. Two million instructions is not a
//! moment — it is two seconds of C64 time with the stick held, and a menu that
//! samples once per frame scrolls right through it. Run it again with a slightly
//! different budget and it stops somewhere else. So every step here carries its
//! own duration, and every schedule point is an absolute machine cycle. Nothing
//! is timed by how long a caller took to send the next command.
//!
//! ISOLATION (doctrine rule 2 / Spec 787): this boots its OWN in-process machine
//! through `boot_engine` — no daemon, no port, no shared session. The human's
//! session is never touched, and the machine dies with the process. That is also
//! why there is no budget-reaper here: there is nothing left running to reap.
//!
//! Determinism is the point and the gate: the same scenario file, run twice from
//! a cold boot, must produce byte-identical output.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use serde_json::{json, Value};

use crate::boot_engine;
use crate::engine::Engine;
use trx64_core::gif89a;

/// PAL. A scenario may override it (`cyclesPerFrame`) but never silently.
const PAL_CYCLES_PER_FRAME: u64 = 19_656;
/// `session/run` is a bounded drive; long waits are split so a breakpoint or a
/// JAM inside the scenario still stops the run at the right place.
const RUN_CHUNK: u64 = 19_656;

// ─────────────────────────────────────────────────────────────────────────────
// The scenario
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Step {
    /// Advance the machine, in cycles.
    Wait { cycles: u64 },
    /// Present keys to the keyboard buffer.
    Type { text: String },
    /// Hold a joystick state for `frames`, then RELEASE it. The press and its
    /// release are one step, because a press without a stated end is exactly the
    /// bug this spec was written for.
    Joy { port: u8, state: JoyBits, frames: u64 },
    /// Run until a predicate holds, or fail at `timeout_frames`.
    WaitUntil { pred: Predicate, timeout_frames: u64 },
    /// Capture a frame, on a frame boundary.
    Shot { label: String },
}

#[derive(Debug, Clone, Copy, Default)]
struct JoyBits {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    fire: bool,
}

#[derive(Debug, Clone)]
enum Predicate {
    /// The CPU reached this address.
    Pc(u16),
    /// The rendered frame has not changed for N consecutive frames.
    ScreenStable { frames: u64 },
    /// The drive WORKED and then STOPPED. Not "is idle now": right after a `LOAD`
    /// the C64 is still printing SEARCHING and the drive has not spun up, so a
    /// bare is-it-idle test passes instantly and captures the prompt instead of
    /// the loaded screen. The predicate therefore requires the busy→idle edge.
    DriveIdle,
}

#[derive(Debug)]
struct Scenario {
    name: String,
    media: Option<String>,
    cycles_per_frame: u64,
    delay_centis: u16,
    max_bytes: usize,
    steps: Vec<Step>,
}

fn as_u64(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(|x| x.as_u64())
}

/// `$C001`, `0xC001` and `49153` all name the same address.
fn parse_addr(v: &Value) -> Result<u16, String> {
    if let Some(n) = v.as_u64() {
        return u16::try_from(n).map_err(|_| format!("address {n} does not fit in 16 bits"));
    }
    let s = v.as_str().ok_or_else(|| "address must be a string or a number".to_string())?;
    let t = s.trim();
    let (digits, radix) = if let Some(r) = t.strip_prefix('$') {
        (r, 16)
    } else if let Some(r) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        (r, 16)
    } else {
        (t, 10)
    };
    u16::from_str_radix(digits, radix).map_err(|e| format!("bad address {s:?}: {e}"))
}

fn parse_scenario(v: &Value) -> Result<Scenario, String> {
    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("reel")
        .to_string();
    let media = v.get("media").and_then(|x| x.as_str()).map(String::from);
    let cycles_per_frame = as_u64(v, "cyclesPerFrame").unwrap_or(PAL_CYCLES_PER_FRAME);
    if cycles_per_frame == 0 {
        return Err("cyclesPerFrame must be > 0".into());
    }

    let reel = v.get("reel").cloned().unwrap_or_else(|| json!({}));
    let delay_ms = as_u64(&reel, "delayMs").unwrap_or(700);
    // GIF delays are centiseconds; a delay that rounds to 0 makes viewers pick
    // their own default, so refuse it rather than produce a reel that plays
    // differently everywhere.
    let delay_centis = u16::try_from(delay_ms / 10)
        .map_err(|_| format!("reel.delayMs {delay_ms} is out of range"))?;
    if delay_centis == 0 {
        return Err(format!(
            "reel.delayMs must be at least 10 ms (got {delay_ms}) — a zero GIF delay \
             leaves the frame rate to whatever the viewer decides"
        ));
    }
    let max_bytes = as_u64(&reel, "maxBytes").unwrap_or(512_000) as usize;

    let raw = v
        .get("steps")
        .and_then(|x| x.as_array())
        .ok_or_else(|| "scenario needs a `steps` array".to_string())?;

    let mut steps = Vec::with_capacity(raw.len());
    for (i, s) in raw.iter().enumerate() {
        steps.push(parse_step(s, cycles_per_frame).map_err(|e| format!("step {i}: {e}"))?);
    }
    if !steps.iter().any(|s| matches!(s, Step::Shot { .. })) {
        return Err("scenario has no `shot` step — it would produce an empty reel".into());
    }

    Ok(Scenario { name, media, cycles_per_frame, delay_centis, max_bytes, steps })
}

fn parse_step(s: &Value, cpf: u64) -> Result<Step, String> {
    if let Some(w) = s.get("wait") {
        let cycles = match (as_u64(w, "cycles"), as_u64(w, "frames")) {
            (Some(c), None) => c,
            (None, Some(f)) => f * cpf,
            (Some(_), Some(_)) => {
                return Err("wait takes `cycles` OR `frames`, not both".into())
            }
            (None, None) => return Err("wait needs `cycles` or `frames`".into()),
        };
        return Ok(Step::Wait { cycles });
    }
    if let Some(t) = s.get("type") {
        let text = t
            .get("text")
            .and_then(|x| x.as_str())
            .ok_or_else(|| "type needs `text`".to_string())?;
        // The scenario is JSON, so a real newline can be written directly; the
        // escaped forms are accepted because a recipe copied out of a shell
        // usually carries them.
        let decoded = text.replace("\\r", "\r").replace("\\n", "\r").replace('\n', "\r");
        return Ok(Step::Type { text: decoded });
    }
    if let Some(j) = s.get("joy") {
        let port = as_u64(j, "port").unwrap_or(2);
        if port != 1 && port != 2 {
            return Err(format!("joy.port must be 1 or 2, got {port}"));
        }
        let frames = as_u64(j, "frames").ok_or_else(|| {
            "joy needs `frames` — a press with no stated duration is the bug this \
             format exists to prevent"
                .to_string()
        })?;
        if frames == 0 {
            return Err("joy.frames must be at least 1 — a press shorter than one frame \
                        is never sampled"
                .into());
        }
        let b = |k: &str| j.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
        let state = JoyBits {
            up: b("up"),
            down: b("down"),
            left: b("left"),
            right: b("right"),
            fire: b("fire"),
        };
        return Ok(Step::Joy { port: port as u8, state, frames });
    }
    if let Some(u) = s.get("waitUntil") {
        let timeout_frames = as_u64(u, "timeoutFrames").ok_or_else(|| {
            "waitUntil needs `timeoutFrames` — a predicate that never fires must fail \
             loudly, not hang the capture"
                .to_string()
        })?;
        if timeout_frames == 0 {
            return Err("waitUntil.timeoutFrames must be > 0".into());
        }
        let pred = if let Some(pc) = u.get("pc") {
            Predicate::Pc(parse_addr(pc)?)
        } else if let Some(ss) = u.get("screenStable") {
            let frames = as_u64(ss, "frames").unwrap_or(60);
            if frames == 0 {
                return Err("waitUntil.screenStable.frames must be > 0".into());
            }
            Predicate::ScreenStable { frames }
        } else if u.get("driveIdle").and_then(|x| x.as_bool()).unwrap_or(false) {
            Predicate::DriveIdle
        } else {
            return Err("waitUntil needs one of `pc`, `screenStable`, `driveIdle`".into());
        };
        return Ok(Step::WaitUntil { pred, timeout_frames });
    }
    if let Some(sh) = s.get("shot") {
        let label = sh
            .get("label")
            .and_then(|x| x.as_str())
            .unwrap_or("shot")
            .to_string();
        return Ok(Step::Shot { label });
    }
    Err("unknown step — expected one of wait / type / joy / waitUntil / shot".into())
}

// ─────────────────────────────────────────────────────────────────────────────
// Execution
// ─────────────────────────────────────────────────────────────────────────────

/// One captured frame and where it came from.
struct Shot {
    label: String,
    cycle: u64,
    raster_line: u64,
    indices: Vec<u8>,
}

fn run_cycles(engine: &Engine, total: u64) -> Result<(), String> {
    let mut done = 0u64;
    while done < total {
        let step = RUN_CHUNK.min(total - done);
        engine.rpc("session/run", json!({ "cycles": step }))?;
        done += step;
    }
    Ok(())
}

/// FNV-1a over a frame's indices — enough to tell "the picture changed" apart
/// from "it did not", which is all `screenStable` asks.
fn frame_hash(indices: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in indices {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn frame_indices(engine: &Engine) -> Result<(Vec<u8>, u64, u64, u64, u64), String> {
    let r = engine.rpc("session/frame_indices", json!({}))?;
    let b64 = r.get("indices").and_then(|v| v.as_str()).unwrap_or("");
    let indices = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("frame_indices base64: {e}"))?;
    let w = r.get("width").and_then(|v| v.as_u64()).unwrap_or(0);
    let h = r.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
    let cyc = r.get("c64Cycles").and_then(|v| v.as_u64()).unwrap_or(0);
    let line = r.get("rasterLine").and_then(|v| v.as_u64()).unwrap_or(0);
    Ok((indices, w, h, cyc, line))
}

/// One read of the machine panel. `session/state` composes CPU, VIC and drive
/// under a single lock, so a predicate that needs two of them still costs one
/// call — the same reason the cockpit stopped polling three RPCs per frame.
fn machine_state(engine: &Engine) -> Result<Value, String> {
    engine.rpc("session/state", json!({}))
}

fn state_pc(s: &Value) -> u16 {
    s.get("cpu").and_then(|c| c.get("pc")).and_then(|v| v.as_u64()).unwrap_or(0) as u16
}

/// Is the drive WORKING? The activity LED, never the motor: a 1541 keeps
/// spinning after a load finishes, so `motorOn` answers a different question and
/// would report a finished load as still busy. (`ledOn` was once wired to the
/// motor here, and this is the bug that produced.)
fn state_drive_busy(s: &Value) -> Result<bool, String> {
    s.get("device")
        .and_then(|d| d.get("drive8"))
        .and_then(|d| d.get("ledOn"))
        .and_then(|v| v.as_bool())
        .ok_or_else(|| {
            "session/state carries no device.drive8.ledOn — `waitUntil.driveIdle` \
             cannot be answered, so it fails rather than guessing"
                .to_string()
        })
}

#[allow(clippy::too_many_arguments)]
pub fn run_reel(
    rom_dir: &Path,
    scenario_path: &str,
    out_gif: &str,
    manifest: Option<&str>,
    frames_dir: Option<&str>,
) -> Result<Value, String> {
    let text = std::fs::read_to_string(scenario_path)
        .map_err(|e| format!("cannot read scenario {scenario_path}: {e}"))?;
    let raw: Value = serde_json::from_str(&text)
        .map_err(|e| format!("scenario {scenario_path} is not valid JSON: {e}"))?;
    let sc = parse_scenario(&raw)?;

    let engine = boot_engine(rom_dir).map_err(|e| format!("{e}"))?;
    let mut log: Vec<String> = Vec::new();

    if let Some(path) = &sc.media {
        let m = engine.rpc("media/mount", json!({ "path": path }))?;
        log.push(format!("mount {path}: {}", serde_json::to_string(&m).unwrap_or_default()));
    }
    // A mount can flip the controller to running; `session/run` refuses while it
    // is, and this front owns the clock.
    let _ = engine.rpc("debug/pause", json!({ "source": "cli-reel" }));

    let mut shots: Vec<Shot> = Vec::new();
    let mut canvas: Option<(u64, u64)> = None;

    for (i, step) in sc.steps.iter().enumerate() {
        match step {
            Step::Wait { cycles } => {
                run_cycles(&engine, *cycles)?;
                log.push(format!("{i}: wait {cycles} cycles"));
            }
            Step::Type { text } => {
                engine.rpc("session/type", json!({ "text": text }))?;
                log.push(format!("{i}: type {text:?}"));
            }
            Step::Joy { port, state, frames } => {
                engine.rpc(
                    "session/joystick_set",
                    json!({
                        "port": port,
                        "up": state.up, "down": state.down,
                        "left": state.left, "right": state.right,
                        "fire": state.fire,
                    }),
                )?;
                run_cycles(&engine, frames * sc.cycles_per_frame)?;
                engine.rpc("session/joystick_clear", json!({ "port": port }))?;
                log.push(format!("{i}: joy port{port} {state:?} for {frames} frames, released"));
            }
            Step::WaitUntil { pred, timeout_frames } => {
                let fired = wait_until(&engine, pred, *timeout_frames, sc.cycles_per_frame)?;
                log.push(format!("{i}: waitUntil {pred:?} → after {fired} frames"));
            }
            Step::Shot { label } => {
                let landed = engine.rpc("session/advance_to_frame", json!({}))?;
                let (indices, w, h, cycle, raster_line) = frame_indices(&engine)?;
                match canvas {
                    None => canvas = Some((w, h)),
                    Some((cw, ch)) if (cw, ch) != (w, h) => {
                        return Err(format!(
                            "shot {label:?} is {w}×{h} but the reel is {cw}×{ch} — the \
                             canvas cannot change mid-reel"
                        ))
                    }
                    _ => {}
                }
                log.push(format!(
                    "{i}: shot {label:?} @ cycle {cycle} (advanced {})",
                    landed.get("cyclesAdvanced").and_then(|v| v.as_u64()).unwrap_or(0)
                ));
                shots.push(Shot { label: label.clone(), cycle, raster_line, indices });
            }
        }
    }

    let (w, h) = canvas.ok_or_else(|| "no frame was captured".to_string())?;
    let palette: Vec<[u8; 3]> = trx64_core::render::COLODORE.to_vec();
    let frames: Vec<gif89a::Frame> = shots
        .iter()
        .map(|s| gif89a::Frame { indices: s.indices.clone() })
        .collect();

    let encoded = gif89a::encode_within(
        w as u16,
        h as u16,
        &palette,
        &frames,
        sc.delay_centis,
        sc.max_bytes,
    )?;
    std::fs::write(out_gif, &encoded.bytes)
        .map_err(|e| format!("cannot write {out_gif}: {e}"))?;

    // Parse what we just wrote, as a structure and not as a byte count. A reel
    // that does not walk as GIF89a blocks is not a reel, whatever its size says.
    let structure = gif89a::parse_structure(&encoded.bytes)?;

    if let Some(dir) = frames_dir {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {dir}: {e}"))?;
        for (n, s) in shots.iter().enumerate() {
            let p = format!("{dir}/{n:02}-{}.raw", sanitise(&s.label));
            std::fs::write(&p, &s.indices).map_err(|e| format!("cannot write {p}: {e}"))?;
        }
    }

    let dropped: Vec<Value> = encoded
        .dropped
        .iter()
        .map(|&idx| json!({ "index": idx, "label": shots[idx].label }))
        .collect();

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for s in &sc.steps {
        let k = match s {
            Step::Wait { .. } => "wait",
            Step::Type { .. } => "type",
            Step::Joy { .. } => "joy",
            Step::WaitUntil { .. } => "waitUntil",
            Step::Shot { .. } => "shot",
        };
        *counts.entry(k).or_default() += 1;
    }

    let report = json!({
        "ok": true,
        "name": sc.name,
        "media": sc.media,
        "gif": out_gif,
        "bytes": encoded.bytes.len(),
        "maxBytes": sc.max_bytes,
        "width": structure.width,
        "height": structure.height,
        "frames": structure.frames,
        "delayCentiseconds": sc.delay_centis,
        "paletteEntries": structure.palette_entries,
        "loopsForever": structure.loops_forever,
        "captured": shots.len(),
        "dropped": dropped,
        "steps": counts.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_,_>>(),
        "shots": shots.iter().map(|s| json!({
            "label": s.label,
            "cycle": s.cycle,
            "rasterLine": s.raster_line,
        })).collect::<Vec<_>>(),
        "log": log,
    });

    if let Some(path) = manifest {
        let pretty = serde_json::to_string_pretty(&report).unwrap_or_default();
        std::fs::write(path, pretty).map_err(|e| format!("cannot write {path}: {e}"))?;
    }

    Ok(report)
}

fn sanitise(label: &str) -> String {
    label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

/// Run frame by frame until the predicate holds. Returns the number of frames it
/// took. A timeout is an error with the state that was actually reached — a
/// predicate that silently gave up would make a reel that is wrong rather than
/// missing.
fn wait_until(
    engine: &Engine,
    pred: &Predicate,
    timeout_frames: u64,
    cycles_per_frame: u64,
) -> Result<u64, String> {
    let mut stable_for = 0u64;
    let mut longest_stable = 0u64;
    let mut last_hash: Option<u64> = None;
    let mut ever_busy = false;

    for elapsed in 0..timeout_frames {
        match pred {
            Predicate::Pc(target) => {
                if state_pc(&machine_state(engine)?) == *target {
                    return Ok(elapsed);
                }
            }
            Predicate::ScreenStable { frames } => {
                let (indices, _, _, _, _) = frame_indices(engine)?;
                let h = frame_hash(&indices);
                if Some(h) == last_hash {
                    stable_for += 1;
                    longest_stable = longest_stable.max(stable_for);
                    if stable_for >= *frames {
                        return Ok(elapsed);
                    }
                } else {
                    stable_for = 0;
                    last_hash = Some(h);
                }
            }
            Predicate::DriveIdle => {
                let busy = state_drive_busy(&machine_state(engine)?)?;
                if busy {
                    ever_busy = true;
                } else if ever_busy {
                    return Ok(elapsed);
                }
            }
        }
        run_cycles(engine, cycles_per_frame)?;
    }

    let pc = machine_state(engine).map(|s| state_pc(&s)).unwrap_or(0);
    let extra = match pred {
        // The most common way this predicate is misused: at a BASIC prompt the
        // cursor blinks about every 20 frames, so the picture genuinely never
        // holds still, and no stability window longer than a blink can ever be
        // reached. Say so instead of leaving the caller to guess.
        Predicate::ScreenStable { frames } => format!(
            "; the longest still stretch was {longest_stable} frames, short of the {frames} \
             asked for — a blinking cursor or an animated screen never settles, so use \
             `driveIdle`, a `pc`, or a plain `wait` there"
        ),
        Predicate::DriveIdle if !ever_busy => {
            "; the drive never became busy at all, so there was no load to wait for — \
             the command may not have been accepted, or the keys arrived before the \
             editor was reading"
                .to_string()
        }
        _ => String::new(),
    };
    Err(format!(
        "waitUntil {pred:?} did not fire within {timeout_frames} frames (PC now ${pc:04X}) \
         — the scenario is wrong about this state, or the machine never reaches it{extra}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(v: Value) -> Result<Scenario, String> {
        parse_scenario(&v)
    }

    #[test]
    fn a_joystick_press_must_state_its_duration() {
        let err = sc(json!({
            "steps": [ { "joy": { "port": 2, "fire": true } }, { "shot": {} } ]
        }))
        .unwrap_err();
        assert!(err.contains("frames"), "got: {err}");
    }

    #[test]
    fn a_predicate_must_carry_a_timeout() {
        let err = sc(json!({
            "steps": [ { "waitUntil": { "pc": "$C001" } }, { "shot": {} } ]
        }))
        .unwrap_err();
        assert!(err.contains("timeoutFrames"), "got: {err}");
    }

    #[test]
    fn a_scenario_without_a_shot_is_refused() {
        let err = sc(json!({ "steps": [ { "wait": { "frames": 10 } } ] })).unwrap_err();
        assert!(err.contains("no `shot`"), "got: {err}");
    }

    #[test]
    fn frames_and_cycles_are_the_same_clock() {
        let s = sc(json!({
            "cyclesPerFrame": 100,
            "steps": [ { "wait": { "frames": 3 } }, { "shot": {} } ]
        }))
        .unwrap();
        match &s.steps[0] {
            Step::Wait { cycles } => assert_eq!(*cycles, 300),
            other => panic!("expected a wait, got {other:?}"),
        }
    }

    #[test]
    fn an_address_reads_in_every_notation() {
        assert_eq!(parse_addr(&json!("$C001")).unwrap(), 0xC001);
        assert_eq!(parse_addr(&json!("0xc001")).unwrap(), 0xC001);
        assert_eq!(parse_addr(&json!(49153)).unwrap(), 0xC001);
    }

    #[test]
    fn a_zero_delay_is_refused_because_viewers_would_each_pick_their_own() {
        let err = sc(json!({
            "reel": { "delayMs": 5 },
            "steps": [ { "shot": {} } ]
        }))
        .unwrap_err();
        assert!(err.contains("delayMs"), "got: {err}");
    }

    #[test]
    fn wait_takes_one_unit_not_two() {
        let err = sc(json!({
            "steps": [ { "wait": { "frames": 1, "cycles": 1 } }, { "shot": {} } ]
        }))
        .unwrap_err();
        assert!(err.contains("not both"), "got: {err}");
    }
}
