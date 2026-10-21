//! Spec 910 P5–P7 — one run model on every daemon.
//!
//! A daemon's machine free-runs from startup, with or without a client; a run may be given a
//! stop (after N cycles, at a breakpoint) and stops on that exact cycle or instruction; no
//! door (reset, power, cart, model, project, overlay, scenario, export, access map,
//! transport) runs the machine behind the caller's back; and the per-frame work is the
//! loop's, so the ring does not depend on how a client chunks its calls.
//!
//! S8 / S9 / S10 drive the real binary over the wire, on free ports, as a shared machine and a
//! sandbox are driven: there is no second kind of daemon. S11 and the static check read the
//! daemon in-process (an embedded state advances itself when a request waits for a stop).
//!
//! ROMs: `TRX64_ROM_DIR`, `~/.trx64/roms`, or the sibling workbench; without them the tests
//! say so and skip.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value as J};
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const FCE2: u64 = 0xFCE2;
const COLD_START: u64 = 0x800C;
/// The KERNAL's keyboard wait loop in the BASIC idle: passed every few dozen cycles.
const IDLE_LOOP: u64 = 0xE5CD;

macro_rules! skip {
    ($($arg:tt)*) => {{
        eprintln!("SKIP one_run_model: {}. NOT green, NOT run.", format!($($arg)*));
        return;
    }};
}

fn rom_dir() -> Option<PathBuf> {
    let has = |p: &Path| p.join("kernal-901227-03.bin").exists();
    if let Ok(d) = std::env::var("TRX64_ROM_DIR") {
        let p = PathBuf::from(d);
        if has(&p) {
            return Some(p);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join(".trx64").join("roms");
        if has(&p) {
            return Some(p);
        }
    }
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for up in [4, 5] {
        let mut p = here.clone();
        for _ in 0..up {
            p = p.parent()?.to_path_buf();
        }
        let p = p.join("C64ReverseEngineeringMCP").join("resources").join("roms");
        if has(&p) {
            return Some(p);
        }
    }
    None
}

// ── the daemon, over the wire ────────────────────────────────────────────────────

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Daemon(Child, PathBuf);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn start(port: u16) -> Daemon {
    let dir = std::env::temp_dir().join(format!("trx64_910run_{port}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_trx64-daemon"));
    cmd.args(["--port", &port.to_string()]).current_dir(&dir).stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(r) = rom_dir() {
        cmd.env("TRX64_ROM_DIR", r);
    }
    Daemon(cmd.spawn().expect("spawn trx64-daemon"), dir)
}

async fn connect(port: u16) -> Ws {
    // `?av=0`: an RPC-only connection, no A/V client — the daemon has nobody watching.
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        match tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/?av=0")).await {
            Ok((ws, _)) => return ws,
            Err(_) if Instant::now() < until => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(e) => panic!("connect: {e}"),
        }
    }
}

async fn call(ws: &mut Ws, method: &str, params: J) -> Result<J, String> {
    let req = json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params });
    ws.send(Message::Text(req.to_string())).await.unwrap();
    while let Some(Ok(msg)) = ws.next().await {
        if let Message::Text(t) = msg {
            let v: J = serde_json::from_str(&t).unwrap();
            if v["id"] == 7 {
                return if v["error"].is_null() { Ok(v["result"].clone()) } else { Err(v["error"].to_string()) };
            }
        }
    }
    panic!("no reply to {method}");
}

async fn rpc(ws: &mut Ws, method: &str, params: J) -> J {
    call(ws, method, params).await.unwrap_or_else(|e| panic!("{method}: {e}"))
}

async fn mon(ws: &mut Ws, command: &str) -> String {
    let r = rpc(ws, "monitor/exec", json!({ "command": command })).await;
    r["output"].as_str().or_else(|| r["error"].as_str()).unwrap_or("").to_string()
}

async fn state(ws: &mut Ws) -> J {
    rpc(ws, "debug/state", json!({})).await
}

/// Wait until the machine is stopped, however it got there, and say where.
async fn wait_paused(ws: &mut Ws) -> J {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let s = state(ws).await;
        if s["runState"] == "paused" {
            return s;
        }
        assert!(Instant::now() < until, "the machine did not stop: {s}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn bp(ws: &mut Ws, pc: u64) {
    rpc(ws, "debug/break_add", json!({ "pc": pc })).await;
}

async fn bp_clear(ws: &mut Ws) {
    rpc(ws, "debug/break_del", json!({})).await;
}

/// Resume to the next stop (or for `cycles`), as fast as possible, and wait for it.
async fn go(ws: &mut Ws, cycles: u64) -> J {
    rpc(ws, "debug/run", json!({ "cycles": cycles, "pace": "warp", "wait": true })).await
}

// ── a banked cartridge whose cold start is known ─────────────────────────────────

/// An Ocean-type (banked, 8 KiB banks at $8000) CRT of two banks. Bank 0 carries the CBM80
/// signature and a cold-start vector to $800C, where `INC $D020 / JMP $800C` spins.
fn banked_crt() -> Vec<u8> {
    let mut bank0 = vec![0xffu8; 0x2000];
    bank0[0..2].copy_from_slice(&[0x0c, 0x80]); // cold start
    bank0[2..4].copy_from_slice(&[0x0c, 0x80]); // warm start
    bank0[4..9].copy_from_slice(&[0xc3, 0xc2, 0xcd, 0x38, 0x30]); // CBM80
    bank0[0x0c..0x12].copy_from_slice(&[0xee, 0x20, 0xd0, 0x4c, 0x0c, 0x80]);
    let bank1 = vec![0x55u8; 0x2000];
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   ");
    v.extend_from_slice(&0x40u32.to_be_bytes());
    v.extend_from_slice(&0x0100u16.to_be_bytes());
    v.extend_from_slice(&5u16.to_be_bytes()); // Ocean
    v.extend_from_slice(&[0, 1]); // EXROM 0, GAME 1: 8 KiB at $8000
    v.extend_from_slice(&[0u8; 6]);
    let mut name = [0u8; 32];
    name[..9].copy_from_slice(b"RUN MODEL");
    v.extend_from_slice(&name);
    for (n, data) in [bank0, bank1].iter().enumerate() {
        v.extend_from_slice(b"CHIP");
        v.extend_from_slice(&(0x10 + data.len() as u32).to_be_bytes());
        v.extend_from_slice(&0u16.to_be_bytes());
        v.extend_from_slice(&(n as u16).to_be_bytes());
        v.extend_from_slice(&0x8000u16.to_be_bytes());
        v.extend_from_slice(&(data.len() as u16).to_be_bytes());
        v.extend_from_slice(data);
    }
    v
}

// ── S8: a reset stops at the cart's cold start ───────────────────────────────────

/// What one door did: its name, the PC and cycle its own reply names, the cycle the machine
/// stood at when it stopped on $FCE2, and the cycles from there to the stop on $800C.
#[derive(Debug, PartialEq)]
struct DoorRun {
    door: &'static str,
    reply_pc: u64,
    reply_cycle: Option<u64>,
    fce2_cycle: u64,
    to_cold_start: u64,
}

async fn through_the_door(ws: &mut Ws, door: &'static str, crt: &Path) -> DoorRun {
    // A door is used on a machine that stands somewhere: stopped on the cold start of the
    // previous one. The breakpoints are armed beforehand, as a client arms them.
    let (reply_pc, reply_cycle) = match door {
        "monitor reset cold" => {
            let t = mon(ws, "reset cold").await;
            (pc_from_text(&t), None)
        }
        "monitor reset warm" => {
            let t = mon(ws, "reset").await;
            (pc_from_text(&t), None)
        }
        "monitor power off, power on" => {
            mon(ws, "power off").await;
            let t = mon(ws, "power on").await;
            (pc_from_text(&t), None)
        }
        "session/reset cold" => {
            let r = rpc(ws, "session/reset", json!({ "mode": "cold" })).await;
            (r["pc"].as_u64().unwrap(), r["c64Cycles"].as_u64())
        }
        "session/reset soft" => {
            let r = rpc(ws, "session/reset", json!({ "mode": "soft" })).await;
            (r["pc"].as_u64().unwrap(), r["c64Cycles"].as_u64())
        }
        "cart re-insert" => {
            rpc(ws, "media/mount", json!({ "path": crt.to_string_lossy() })).await;
            (0, None)
        }
        other => panic!("unknown door {other}"),
    };
    // The machine stops on the reset vector's first instruction ...
    let at_fce2 = wait_paused(ws).await;
    assert_eq!(at_fce2["pc"], FCE2, "{door}: stopped on $FCE2: {at_fce2}");
    assert_eq!(at_fce2["stop"]["pc"], FCE2, "{door}: {at_fce2}");
    let fce2_cycle = at_fce2["cycles"].as_u64().unwrap();
    if let Some(c) = reply_cycle {
        assert_eq!(c, fce2_cycle, "{door}: the reply names the cycle the door left the machine at");
    }
    if reply_pc != 0 {
        assert_eq!(reply_pc, FCE2, "{door}: the reply names the reset vector, not where the machine would be later");
    }
    // ... and, run on, at the cart's cold start.
    let at_cold = go(ws, 2_000_000).await;
    assert_eq!(at_cold["runState"], "paused", "{door}: {at_cold}");
    assert_eq!(at_cold["stop"]["reason"], "breakpoint", "{door}: {at_cold}");
    assert_eq!(at_cold["stop"]["pc"], COLD_START, "{door}: the cart's cold start: {at_cold}");
    let to_cold_start = at_cold["cycles"].as_u64().unwrap() - fce2_cycle;
    DoorRun { door, reply_pc, reply_cycle, fce2_cycle, to_cold_start }
}

fn pc_from_text(t: &str) -> u64 {
    let i = t.find("PC=$").unwrap_or_else(|| panic!("no PC in `{t}`")) + 4;
    u64::from_str_radix(&t[i..i + 4], 16).unwrap()
}

/// The whole sequence on one daemon: a fresh machine, the cart, the breakpoints, every door.
async fn door_sequence(port: u16, crt: &Path) -> Vec<DoorRun> {
    let mut ws = connect(port).await;
    bp(&mut ws, FCE2).await;
    bp(&mut ws, COLD_START).await;
    let mut out = Vec::new();
    // The insert itself is the first door.
    rpc(&mut ws, "media/mount", json!({ "path": crt.to_string_lossy() })).await;
    let at_fce2 = wait_paused(&mut ws).await;
    assert_eq!((at_fce2["pc"].as_u64(), at_fce2["cycles"].as_u64()), (Some(FCE2), Some(0)), "insert: {at_fce2}");
    let at_cold = go(&mut ws, 2_000_000).await;
    assert_eq!(at_cold["stop"]["pc"], COLD_START, "insert: {at_cold}");
    for door in [
        "monitor reset cold",
        "monitor reset warm",
        "monitor power off, power on",
        "session/reset cold",
        "session/reset soft",
        "cart re-insert",
    ] {
        out.push(through_the_door(&mut ws, door, crt).await);
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn s8_every_door_stops_at_fce2_then_at_the_carts_cold_start_on_both_daemons() {
    if rom_dir().is_none() {
        skip!("no ROM set (TRX64_ROM_DIR)");
    }
    // Two daemons: the one a human shares and the one the LLM starts for a purpose. They are
    // the same binary with the same flags; the test is that they answer the same.
    let (pa, pb) = (free_port(), free_port());
    let (a, b) = (start(pa), start(pb));
    let crt = a.1.join("run_model.crt");
    std::fs::write(&crt, banked_crt()).unwrap();
    let crt_b = b.1.join("run_model.crt");
    std::fs::write(&crt_b, banked_crt()).unwrap();
    let ra = door_sequence(pa, &crt).await;
    let rb = door_sequence(pb, &crt_b).await;
    assert_eq!(ra, rb, "both daemons answer identically");
    for r in &ra {
        // The cold start is reached by the reset code itself, not by 5 M cycles of someone
        // else's run: the bound is the KERNAL's RAM test plus the cartridge check.
        assert!(r.to_cold_start > 0 && r.to_cold_start < 400_000, "{r:?}");
        let _ = (r.reply_pc, r.reply_cycle, r.fce2_cycle);
    }
    eprintln!("S8 cycles from $FCE2 to the cold start: {:?}", ra.iter().map(|r| (r.door, r.to_cold_start)).collect::<Vec<_>>());
}

// ── S9: one run model, cycle-exact stops ─────────────────────────────────────────

/// The reference, with no daemon around it: a cold-reset machine, run the way the old blocking
/// `session/run` ran it — `run_for_full` over the budget, or the same run with an exec
/// breakpoint armed in the core.
fn reference(n: u64, bp_at: Option<u16>) -> (u64, u16) {
    use trx64_core::NullSink;
    let mut session = trx64_session::Session::new("ref");
    session.boot(&rom_dir().unwrap()).expect("boot");
    let m = &mut session.machine;
    match bp_at {
        None => m.run_for_full(n, &mut NullSink, |_, _, _, _, _, _, _| {}),
        Some(pc) => {
            let set: std::collections::HashSet<u16> = [pc].into_iter().collect();
            let stop = m.run_for_full_capped_dbg(n, u64::MAX, Some(&set), None, None, &mut NullSink, |_, _, _, _, _, _, _| {});
            assert!(matches!(stop, trx64_core::RunStop::Breakpoint(_)), "the reference reaches the breakpoint: {stop:?}");
        }
    }
    (m.clk, m.cpu6510.reg_pc)
}

/// A fresh machine at cycle 0 on a daemon: break on the reset vector, cold reset, wait.
async fn at_cycle_zero(ws: &mut Ws) {
    bp(ws, FCE2).await;
    rpc(ws, "session/reset", json!({ "mode": "cold" })).await;
    let s = wait_paused(ws).await;
    assert_eq!((s["pc"].as_u64(), s["cycles"].as_u64()), (Some(FCE2), Some(0)), "{s}");
    bp_clear(ws).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn s9_a_stop_is_the_cycle_and_the_pc_the_blocking_run_stopped_on() {
    if rom_dir().is_none() {
        skip!("no ROM set (TRX64_ROM_DIR)");
    }
    let (pa, pb) = (free_port(), free_port());
    let (_a, _b) = (start(pa), start(pb));
    for (name, port) in [("shared", pa), ("sandbox", pb)] {
        let mut ws = connect(port).await;

        // after N cycles: odd numbers, so the stop is an instruction boundary past the cap
        for n in [1_234_567u64, 2_000_003, 17] {
            let (want_clk, want_pc) = reference(n, None);
            at_cycle_zero(&mut ws).await;
            let r = go(&mut ws, n).await;
            assert_eq!(
                (r["cycles"].as_u64(), r["pc"].as_u64()),
                (Some(want_clk), Some(want_pc as u64)),
                "{name}: debug/run {{cycles: {n}}} stops where the blocking run stopped"
            );
            assert_eq!(r["stop"]["reason"], "budget", "{name}: {r}");
            at_cycle_zero(&mut ws).await;
            let legacy = rpc(&mut ws, "session/run", json!({ "cycles": n })).await;
            assert_eq!(legacy["c64Cycles"].as_u64(), Some(want_clk), "{name}: session/run is the same run");
            assert!(legacy.get("breakpoint").is_none() && legacy.get("jam").is_none(), "{name}: {legacy}");
            assert_eq!(state(&mut ws).await["pc"].as_u64(), Some(want_pc as u64));
        }

        // at a breakpoint: the IRQ handler's entry, first reached well into the boot
        let (want_clk, want_pc) = reference(3_000_000, Some(0xea31));
        at_cycle_zero(&mut ws).await;
        bp(&mut ws, 0xea31).await;
        let r = rpc(&mut ws, "session/run", json!({ "cycles": 3_000_000 })).await;
        assert_eq!(r["c64Cycles"].as_u64(), Some(want_clk), "{name}: {r}");
        assert_eq!(r["breakpoint"]["pc"].as_u64(), Some(want_pc as u64), "{name}: {r}");
        at_cycle_zero(&mut ws).await;
        bp(&mut ws, 0xea31).await;
        let r = go(&mut ws, 3_000_000).await;
        assert_eq!((r["cycles"].as_u64(), r["pc"].as_u64()), (Some(want_clk), Some(want_pc as u64)), "{name}: {r}");
        assert_eq!(r["stop"]["reason"], "breakpoint", "{name}: {r}");
        bp_clear(&mut ws).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn s9_a_running_machine_with_no_stop_keeps_advancing_with_no_client_attached() {
    let (pa, pb) = (free_port(), free_port());
    let (_a, _b) = (start(pa), start(pb));
    for port in [pa, pb] {
        // The only connection is `?av=0`: nobody subscribes to a picture, and nothing asked
        // the machine to run. It runs.
        let mut ws = connect(port).await;
        let s0 = rpc(&mut ws, "session/state", json!({})).await;
        assert_eq!(s0["runState"], "running", "{s0}");
        assert_eq!(s0["sid"]["streaming"], false, "nobody listens: {s0}");
        let c0 = s0["c64Cycles"].as_u64().unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let c1 = rpc(&mut ws, "session/state", json!({})).await["c64Cycles"].as_u64().unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let c2 = rpc(&mut ws, "session/state", json!({})).await["c64Cycles"].as_u64().unwrap();
        assert!(c1 > c0 + 150_000 && c2 > c1 + 150_000, "it advances with no client: {c0} {c1} {c2}");
        assert!(c2 - c0 < 2_000_000, "at real time, not unthrottled: {c0} {c2}");
    }
}

// ── S10: no hidden run ───────────────────────────────────────────────────────────

async fn boot_to_basic(ws: &mut Ws) {
    at_cycle_zero(ws).await;
    let r = go(ws, 3_000_000).await;
    assert_eq!(r["stop"]["reason"], "budget", "{r}");
}

/// Step the one instruction that takes the machine off the breakpoint it is parked on, so the
/// door under test starts from somewhere else and has to reach the breakpoint by running.
async fn leave_the_breakpoint(ws: &mut Ws) {
    rpc(ws, "debug/step", json!({})).await;
    let s = state(ws).await;
    assert_ne!(s["pc"], IDLE_LOOP, "{s}");
}

/// A fixed run on the live machine, with a breakpoint on a loop the idle machine is always in,
/// must stop at it, after far fewer cycles than the door was asked for.
async fn expect_stop_in_idle_loop(ws: &mut Ws, door: &str, asked: u64, start_cycle: u64) {
    let s = state(ws).await;
    assert_eq!(s["runState"], "paused", "{door}: {s}");
    assert_eq!(s["stop"]["reason"], "breakpoint", "{door}: the breakpoint stopped the door's run: {s}");
    assert_eq!(s["pc"], IDLE_LOOP, "{door}: {s}");
    let ran = s["cycles"].as_u64().unwrap() - start_cycle;
    assert!(ran < asked / 2, "{door}: ran {ran} of the {asked} asked for");
}

#[tokio::test(flavor = "multi_thread")]
async fn s10_no_door_runs_the_machine_past_a_breakpoint() {
    if rom_dir().is_none() {
        skip!("no ROM set (TRX64_ROM_DIR)");
    }
    let port = free_port();
    let d = start(port);
    let crt = d.1.join("run_model.crt");
    std::fs::write(&crt, banked_crt()).unwrap();
    let mut ws = connect(port).await;

    // ── the doors that restart the machine: the breakpoint on the very first instruction
    // stops it there, at the cycle the door's own reply names — the door ran nothing. ──
    boot_to_basic(&mut ws).await;
    bp(&mut ws, FCE2).await;
    for door in ["monitor reset cold", "monitor reset warm", "monitor power", "session/reset cold", "session/reset soft", "session/power", "media/mount cart", "media/unmount cart", "session/model ntsc", "session/model pal"] {
        let reply_cycle: Option<u64> = match door {
            "monitor reset cold" => { mon(&mut ws, "reset cold").await; None }
            "monitor reset warm" => { mon(&mut ws, "reset").await; None }
            "monitor power" => { mon(&mut ws, "power off").await; mon(&mut ws, "power on").await; None }
            "session/reset cold" => rpc(&mut ws, "session/reset", json!({ "mode": "cold" })).await["c64Cycles"].as_u64(),
            "session/reset soft" => rpc(&mut ws, "session/reset", json!({ "mode": "soft" })).await["c64Cycles"].as_u64(),
            "session/power" => {
                rpc(&mut ws, "session/power", json!({ "op": "off" })).await;
                rpc(&mut ws, "session/power", json!({ "op": "on" })).await["c64Cycles"].as_u64()
            }
            "media/mount cart" => { rpc(&mut ws, "media/mount", json!({ "path": crt.to_string_lossy() })).await; None }
            "media/unmount cart" => { rpc(&mut ws, "media/unmount", json!({ "role": "cartridge" })).await; None }
            // The model switch is not a restart: it carries the running machine to the frame
            // boundary. Its run is a run — the breakpoint below stops it.
            _ => continue,
        };
        let s = wait_paused(&mut ws).await;
        assert_eq!(s["pc"], FCE2, "{door}: stopped on the first instruction: {s}");
        if let Some(c) = reply_cycle {
            assert_eq!(s["cycles"].as_u64(), Some(c), "{door}: the reply names where the machine stood");
        }
        if door != "monitor reset warm" && door != "session/reset soft" {
            assert_eq!(s["cycles"], 0, "{door}: a power cycle starts at cycle 0 and ran nothing");
        }
    }
    bp_clear(&mut ws).await;

    // ── the doors that run a stretch: an ordinary run, breakpoints live ──
    boot_to_basic(&mut ws).await;
    bp(&mut ws, IDLE_LOOP).await;

    // the model switch advances to the frame boundary through the same run
    let c0 = state(&mut ws).await["cycles"].as_u64().unwrap();
    let r = call(&mut ws, "session/model", json!({ "name": "c64-ntsc" })).await;
    match r {
        Err(e) => assert!(e.contains("stopped"), "session/model with a breakpoint on the way: {e}"),
        Ok(v) => assert!(v["switched"] == true, "{v}"),
    }
    let _ = c0;
    // (back on PAL for what follows)
    bp_clear(&mut ws).await;
    let _ = call(&mut ws, "session/model", json!({ "name": "c64-pal" })).await;
    boot_to_basic(&mut ws).await;
    bp(&mut ws, IDLE_LOOP).await;

    // memory access map
    leave_the_breakpoint(&mut ws).await;
    let c0 = state(&mut ws).await["cycles"].as_u64().unwrap();
    rpc(&mut ws, "debug/memory_access_map", json!({ "cycles": 2_000_000 })).await;
    expect_stop_in_idle_loop(&mut ws, "debug/memory_access_map", 2_000_000, c0).await;

    // audio export
    leave_the_breakpoint(&mut ws).await;
    let wav = d.1.join("out.wav");
    let c0 = state(&mut ws).await["cycles"].as_u64().unwrap();
    rpc(&mut ws, "audio/export", json!({ "out_path": wav.to_string_lossy(), "duration_sec": 1.0 })).await;
    expect_stop_in_idle_loop(&mut ws, "audio/export", 985_248, c0).await;

    // a scenario replay
    leave_the_breakpoint(&mut ws).await;
    let c0 = state(&mut ws).await["cycles"].as_u64().unwrap();
    let r = rpc(&mut ws, "runtime/scenario_run", json!({ "scenario": { "id": "s", "inputs": [], "cycleBudget": 1_000_000 } })).await;
    assert_eq!(r["stopped"]["reason"], "breakpoint", "the replay says what stopped it: {r}");
    expect_stop_in_idle_loop(&mut ws, "runtime/scenario_run", 1_000_000, c0).await;

    // the overlay run, from an anchor
    let cp = rpc(&mut ws, "checkpoint/capture", json!({})).await;
    let id = cp["ref"]["id"].as_str().unwrap().to_string();
    let c0 = cp["ref"]["cycles"].as_u64().unwrap();
    rpc(&mut ws, "runtime/overlay_run", json!({ "anchor_id": id, "patches": [], "run_cycles": 1_000_000 })).await;
    expect_stop_in_idle_loop(&mut ws, "runtime/overlay_run", 1_000_000, c0).await;

    // the transport's step forward at the head: one frame, a run like any other
    leave_the_breakpoint(&mut ws).await;
    let c0 = state(&mut ws).await["cycles"].as_u64().unwrap();
    rpc(&mut ws, "transport/frame", json!({ "delta": 1 })).await;
    let s = state(&mut ws).await;
    assert_eq!(s["stop"]["reason"], "breakpoint", "transport/frame +1: {s}");
    assert_eq!(s["pc"], IDLE_LOOP, "{s}");
    assert!(s["cycles"].as_u64().unwrap() - c0 < 19_656, "transport/frame +1: {s}");
    bp_clear(&mut ws).await;
}

/// Static: no fixed-cycle run of the live session exists outside the one loop (the advance
/// and its machinery) and the run it hands to a request that waits (`run_inline`,
/// `park_until_stopped`, `start_run`). The checks read the source, not a comment: a line of
/// the daemon's non-test code that calls one of the run primitives must sit in a function of
/// the allowed list, each of which is named for what it is.
#[test]
fn s10_no_fixed_cycle_run_of_the_live_session_outside_the_loop() {
    let src = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs")).unwrap();
    let code = strip_test_modules(&src);
    // Where a run primitive may be called, and why.
    let allowed: &[(&str, &str)] = &[
        ("stream_debug_gated_advance", "the loop's step"),
        ("run_cycle_budget", "the primitive the loop's step uses (traced / exerciser path)"),
        ("run_machine_budget", "the primitive on a bare machine (a scratch clone)"),
        ("run_until_break", "the loop's armed step"),
        ("run_full_segment", "the loop's armed step"),
        ("run_isolated_segment", "the loop's armed step on an exerciser"),
        ("run_for", "MachineTarget::run_for — the bare machine of a sandbox clone"),
        ("switch_model", "MachineTarget::switch_model — the same bare machine"),
        ("sandbox_one", "the in-daemon sandbox fork: it runs a COPY"),
        ("transport_move_to", "the picture re-sim, followed by a restore of the anchor"),
        ("step_one_instruction", "a single-instruction step"),
        ("run_one_gated", "a single-instruction step"),
    ];
    let primitives = [
        "run_cycle_budget(", "run_machine_budget(", ".run_for_full(", "machine.run_for(", "machine.run_for_with(",
        "machine.run_for_sid(", "machine.run_for_cia(", "machine.run_for_drive_sampled(",
    ];
    let mut violations = Vec::new();
    let mut current = String::new();
    for (n, line) in code.lines().enumerate() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("fn ").or_else(|| t.strip_prefix("pub fn ")).or_else(|| t.strip_prefix("pub(crate) fn ")) {
            current = rest.split(|c: char| !(c.is_alphanumeric() || c == '_')).next().unwrap_or("").to_string();
        }
        if t.starts_with("//") {
            continue;
        }
        for p in primitives {
            if line.contains(p) && !line.contains(&format!("fn {}", p.trim_end_matches('(').trim_start_matches('.').trim_start_matches("machine."))) {
                if !allowed.iter().any(|(f, _)| current == *f) {
                    violations.push(format!("main.rs:{}: {} in fn {}", n + 1, p, current));
                }
            }
        }
    }
    assert!(violations.is_empty(), "fixed-cycle runs outside the loop:\n{}", violations.join("\n"));
}

/// The non-test part of a source: every `#[cfg(test)] mod … { … }` block removed (brace-matched).
fn strip_test_modules(src: &str) -> String {
    let mut out = String::new();
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if src[i..].starts_with("#[cfg(test)]") {
            let rest = &src[i..];
            if let Some(m) = rest.find("mod ") {
                if rest[..m].lines().count() <= 2 {
                    if let Some(open) = rest[m..].find('{') {
                        let mut depth = 0usize;
                        let mut j = i + m + open;
                        while j < b.len() {
                            match b[j] {
                                b'{' => depth += 1,
                                b'}' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        break;
                                    }
                                }
                                _ => {}
                            }
                            j += 1;
                        }
                        i = j + 1;
                        continue;
                    }
                }
            }
        }
        let ch = src[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

// ── S11: the ring does not depend on how the client chunks ───────────────────────

fn embedded() -> Option<trx64_daemon::SharedState> {
    let roms = rom_dir()?;
    Some(trx64_daemon::create_embedded_state(&roms).expect("boot the machine from ROMs"))
}

fn ask(state: &trx64_daemon::SharedState, method: &str, params: J) -> J {
    let req = trx64_daemon::Request { jsonrpc: "2.0".into(), id: json!(1), method: method.into(), params };
    let resp = trx64_daemon::dispatch(req, state);
    assert!(resp.error.is_none(), "{method}: {:?}", resp.error);
    resp.result.unwrap_or(J::Null)
}

/// Run to the absolute machine cycle `target` (the first instruction boundary at or past it)
/// as one `debug/run`, waited for.
fn run_to(state: &trx64_daemon::SharedState, target: u64) {
    let now = ask(state, "debug/state", json!({}))["cycles"].as_u64().unwrap();
    if now >= target {
        return;
    }
    ask(state, "debug/run", json!({ "cycles": target - now, "pace": "warp", "wait": true }));
}

/// (the cycles of the ring's anchors, how many times the observer fired) after running to
/// `end` in the chunks `targets` names.
fn chunked(targets: &[u64]) -> (Vec<u64>, u64) {
    let state = embedded().unwrap();
    ask(&state, "monitor/exec", json!({ "command": "cadence 5" }));
    // a `log` observer on the IRQ handler's entry: fires once per interrupt
    ask(&state, "monitor/exec", json!({ "command": "obs tick when exec ea31 do log" }));
    ask(&state, "session/reset", json!({ "mode": "cold" }));
    let s = ask(&state, "debug/state", json!({}));
    assert_eq!(s["cycles"], 0, "{s}");
    for &t in targets {
        run_to(&state, t);
    }
    let list = ask(&state, "checkpoint/list", json!({}));
    let anchors: Vec<u64> = list["checkpoints"].as_array().unwrap().iter().map(|c| c["cycles"].as_u64().unwrap()).collect();
    let obs = ask(&state, "monitor/exec", json!({ "command": "obs" }));
    let text = obs["output"].as_str().unwrap().to_string();
    let hits: u64 = text.split("hits=").nth(1).and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()).and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("no hit count in `{text}`"));
    (anchors, hits)
}

#[test]
fn s11_the_ring_and_the_observers_do_not_depend_on_how_the_client_chunks() {
    if rom_dir().is_none() {
        skip!("no ROM set (TRX64_ROM_DIR)");
    }
    const FRAME: u64 = 19_656;
    let end = 200 * FRAME;
    let one: Vec<u64> = vec![end];
    let each: Vec<u64> = (1..=200).map(|k| k * FRAME).collect();
    // seven uneven runs, the last ending where the others do
    let uneven: Vec<u64> = vec![12_345, 1_000_000, 1_000_031, 2_500_000, 2_500_001, 3_333_333, end];
    let (a, ha) = chunked(&one);
    let (b, hb) = chunked(&each);
    let (c, hc) = chunked(&uneven);
    assert!(a.len() >= 5, "the ring holds anchors: {a:?}");
    assert_eq!(a, b, "200 frames as one run == as 200 one-frame runs");
    assert_eq!(a, c, "200 frames as one run == as 7 uneven runs");
    assert!(ha > 100, "the observer fired through the run ({ha})");
    assert_eq!((ha, ha), (hb, hc), "the observer fired as often in all three");
}
