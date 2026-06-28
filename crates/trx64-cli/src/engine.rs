//! The in-process machine driver shared by every front (one-shot, TUI, window).
//!
//! `Engine` wraps the `SharedState` (`Arc<Mutex<State>>` — one machine per process)
//! and is itself `Clone` + `Send + Sync`, so the TUI worker thread, the emulation
//! pump thread, and the winit/audio window all drive the SAME machine through it.
//!
//! It carries the high-level "machine verb" layer (power/run/pause/step/mount/…)
//! that maps each verb onto the SAME `dispatch()` JSON-RPC calls the WS daemon and
//! the FFI use — there is NO second runtime path. Anything that is not a high-level
//! verb is forwarded verbatim to `monitor/exec` (the ~128-verb VICE-superset).
//!
//! RUN-STATE MODEL (important — mirrors the FFI "embedded host drives the loop"
//! contract). TRX64 has no autonomous pacing loop: `debug/run` only flips the
//! controller `running` flag, and `session/run` REFUSES while `running==true` (so two
//! clocks can't double-advance). The host (us) owns the per-frame loop. So the
//! Engine keeps its OWN `running` flag (`AtomicBool`); the pump thread, while that
//! flag is set, advances the machine one PAL frame at a time via `session/run`
//! (which honours breakpoints + JAM) WITHOUT flipping the controller flag — exactly
//! the FFI pattern. `pause` clears the flag.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use trx64_daemon::{dispatch, Request, Response, SharedState};

/// One PAL frame ≈ 312 lines × 63 cycles = 19656; the daemon's `session/run`
/// default budget is 19705. We advance one frame's worth per pump tick.
pub const CYC_PER_FRAME: u64 = 19_656;

/// The shared, cloneable handle to the in-process machine.
#[derive(Clone)]
pub struct Engine {
    state: SharedState,
    /// Host-side run flag (the pump advances the machine while this is true). This is
    /// the AUTHORITY for the cockpit's RUN/PAUSE indicator, distinct from the
    /// controller's `session.running` (which we keep paused so `session/run` is legal).
    running: Arc<AtomicBool>,
    /// Warp pacing flag (8× frame budget per pump tick when set).
    warp: Arc<AtomicBool>,
    /// Set true when `quit` is issued — the pump + window observe it to shut down.
    quit: Arc<AtomicBool>,
    /// Monotonic generation bumped on every machine-mutating verb, so the window's
    /// audio first-drain / re-sync can notice resets without polling state.
    epoch: Arc<AtomicU64>,
    /// Virtual-joystick mode: 0 = off (WASD/Space are keyboard), 1 = port 1, 2 = port 2.
    /// When on, the window routes WASD+Space to the joystick (C64RE Spec 310).
    joystick_mode: Arc<AtomicU8>,
}

/// The outcome of a single command-line submission.
pub struct CmdResult {
    /// Text to append to the cockpit's output/log pane (may be multi-line).
    pub output: String,
    /// Set when the command was `window` — the main thread must create the window.
    pub open_window: bool,
    /// Set when the command was `quit`.
    pub quit: bool,
}

impl CmdResult {
    fn text(s: impl Into<String>) -> Self {
        Self { output: s.into(), open_window: false, quit: false }
    }
}

impl Engine {
    pub fn new(state: SharedState) -> Self {
        Self {
            state,
            running: Arc::new(AtomicBool::new(false)),
            warp: Arc::new(AtomicBool::new(false)),
            quit: Arc::new(AtomicBool::new(false)),
            epoch: Arc::new(AtomicU64::new(0)),
            joystick_mode: Arc::new(AtomicU8::new(0)),
        }
    }

    /// Virtual-joystick mode (0 = off, 1 = port 1, 2 = port 2). Read by the window.
    pub fn joystick_mode(&self) -> u8 {
        self.joystick_mode.load(Ordering::SeqCst)
    }

    /// The underlying shared machine — used by the emulator window's A/V pull loop
    /// (`pull_frame_buffer` / `pull_audio_drain`).
    pub fn shared_state(&self) -> &SharedState {
        &self.state
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
    pub fn is_warp(&self) -> bool {
        self.warp.load(Ordering::SeqCst)
    }
    pub fn should_quit(&self) -> bool {
        self.quit.load(Ordering::SeqCst)
    }
    /// Generation counter bumped on machine-mutating verbs — the Part 2 window's audio
    /// path watches it to flush its ring on resets.
    #[allow(dead_code)] // consumed by the Part 2 emulator window audio re-sync
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    // ── raw dispatch ─────────────────────────────────────────────────────────

    /// Build a JSON-RPC `Request` and call the SAME synchronous, socket-free
    /// `dispatch()` the WS transport uses. Returns the `result` value, or an `Err`
    /// string carrying the JSON-RPC error message.
    pub fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let req = Request {
            jsonrpc: "2.0".to_string(),
            id: json!(1),
            method: method.to_string(),
            params,
        };
        let Response { result, error, .. } = dispatch(req, &self.state);
        if let Some(err) = error {
            return Err(format!("error {}: {}", err.code, err.message));
        }
        Ok(result.unwrap_or(Value::Null))
    }

    fn bump_epoch(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    // ── the per-frame pump (called by the pump thread) ─────────────────────────

    /// Advance the machine by one frame's budget IF the host run flag is set, and
    /// return the cycles advanced (0 when paused). The controller stays paused
    /// (`session.running==false`) so `session/run` is legal; this mirrors the
    /// FFI/embedded-host loop. On a breakpoint/JAM halt the daemon's `session/run`
    /// returns early with a `breakpoint` object — we then clear the host run flag so
    /// the cockpit shows PAUSED at the hit.
    pub fn pump_frame(&self) -> u64 {
        if !self.running.load(Ordering::SeqCst) {
            return 0;
        }
        let budget = if self.warp.load(Ordering::SeqCst) {
            CYC_PER_FRAME * 8
        } else {
            CYC_PER_FRAME
        };
        match self.rpc("session/run", json!({ "cycles": budget })) {
            Ok(v) => {
                // A breakpoint/observer hit halts the run early — reflect it as PAUSE.
                // Spec 764 — a KIL/JAM also halts the run early (`jam` object in the
                // reply): clear the host run flag so the pump STOPS re-issuing
                // session/run on the jammed CPU (the spin/hang) and the cockpit shows
                // PAUSED at the KIL. The daemon already pushed debug/stopped reason=jam.
                if v.get("breakpoint").is_some() || v.get("jam").is_some() {
                    self.running.store(false, Ordering::SeqCst);
                }
                v.get("c64Cycles").and_then(|c| c.as_u64()).unwrap_or(0)
            }
            Err(_) => {
                // session/run only errors if the controller flag is set — which we
                // never do under the pump. Be defensive: pause on any error.
                self.running.store(false, Ordering::SeqCst);
                0
            }
        }
    }

    // ── high-level machine verbs ───────────────────────────────────────────────

    /// Parse + execute a single cockpit command line. Returns the text to log plus
    /// any side-channel signal (open the window / quit). High-level verbs map onto
    /// `dispatch` calls; everything else falls through to `monitor/exec`.
    pub fn exec_line(&self, line: &str) -> CmdResult {
        let line = line.trim();
        if line.is_empty() {
            return CmdResult::text("");
        }
        // `/`-prefixed = VM / high-level command (slash-command namespace); a bare
        // line = monitor passthrough (the ~128-verb VICE-superset — the primary
        // surface, so you type `d c000` / `r` / `bk e000` directly).
        let vm = match line.strip_prefix('/') {
            Some(rest) => rest.trim(),
            None => return self.verb_monitor(line),
        };
        if vm.is_empty() {
            return CmdResult::text(help_text()); // bare "/" → the VM help
        }
        let mut parts = vm.split_whitespace();
        let verb = parts.next().unwrap_or("").to_ascii_lowercase();
        let rest: Vec<&str> = parts.collect();
        let arg = rest.join(" ");

        match verb.as_str() {
            "power" => self.verb_power(rest.first().copied()),
            "reset" => self.verb_reset(rest.first().copied()),
            "run" => {
                // `/run` with no arg = resume; `/run <prg>` = load+autostart.
                if arg.is_empty() {
                    self.verb_run()
                } else {
                    self.verb_run_prg(&arg)
                }
            }
            "pause" => self.verb_pause(),
            "step" => self.verb_step(),
            "mount" => self.verb_mount(&arg),
            "eject" => self.verb_eject(),
            "load" => self.verb_load(&arg),
            "warp" => self.verb_warp(rest.first().copied()),
            "joystick" | "joy" => self.verb_joystick(rest.first().copied()),
            "window" => CmdResult { output: "opening emulator window…".into(), open_window: true, quit: false },
            "dump" => self.verb_dump(&arg),
            "restore" => self.verb_restore(&arg),
            "ringdump" => self.verb_ringdump(&arg),
            "ringload" => self.verb_ringload(&arg),
            "help" => CmdResult::text(help_text()),
            "quit" | "exit" => {
                self.quit.store(true, Ordering::SeqCst);
                CmdResult { output: "bye.".into(), open_window: false, quit: true }
            }
            // Unknown /verb — DON'T fall through to the monitor (the user explicitly
            // used the VM namespace); point them at /help.
            other => CmdResult::text(format!("unknown command: /{other} — try /help")),
        }
    }

    fn verb_power(&self, sub: Option<&str>) -> CmdResult {
        match sub.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("on") | None => {
                // Cold boot: fresh DRAM + cold reset, then start running.
                let r = self.rpc("session/reset", json!({ "mode": "cold" }));
                self.bump_epoch();
                match r {
                    Ok(v) => {
                        self.running.store(true, Ordering::SeqCst);
                        CmdResult::text(format!(
                            "POWER ON — cold boot @ PC=${:04X}, running.",
                            v.get("pc").and_then(|p| p.as_u64()).unwrap_or(0)
                        ))
                    }
                    Err(e) => CmdResult::text(format!("power on failed: {e}")),
                }
            }
            Some("off") => {
                // Powered-off state: stop the pump + cold-reset to a quiescent machine
                // (the runtime has no literal power-off; cold-reset + halted is the
                // closest faithful "off" — a blank machine waiting for power on).
                self.running.store(false, Ordering::SeqCst);
                let _ = self.rpc("debug/pause", json!({ "source": "cli" }));
                let r = self.rpc("session/reset", json!({ "mode": "cold" }));
                self.bump_epoch();
                match r {
                    Ok(_) => CmdResult::text("POWER OFF — machine halted + reset to powered-off state."),
                    Err(e) => CmdResult::text(format!("power off: reset failed: {e}")),
                }
            }
            Some(other) => CmdResult::text(format!("power: unknown sub '{other}' (use on|off)")),
        }
    }

    fn verb_reset(&self, sub: Option<&str>) -> CmdResult {
        // reset [cold|warm]; cold = power-cycle (fresh DRAM), warm = RESET line (RAM kept).
        let (mode, label) = match sub.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("warm") | Some("soft") => ("soft", "warm"),
            _ => ("cold", "cold"),
        };
        let r = self.rpc("session/reset", json!({ "mode": mode }));
        self.bump_epoch();
        // A real machine reset boots + runs — don't leave it frozen at the reset
        // vector. Set the host run flag so the pump drives it (like `power on`).
        self.running.store(true, Ordering::SeqCst);
        match r {
            Ok(v) => CmdResult::text(format!(
                "RESET ({label}) @ PC=${:04X}, running.",
                v.get("pc").and_then(|p| p.as_u64()).unwrap_or(0)
            )),
            Err(e) => CmdResult::text(format!("reset failed: {e}")),
        }
    }

    fn verb_run(&self) -> CmdResult {
        self.running.store(true, Ordering::SeqCst);
        CmdResult::text("RUN — free-running.")
    }

    fn verb_pause(&self) -> CmdResult {
        self.running.store(false, Ordering::SeqCst);
        // Sync the controller's stop info so monitor `r`/`bt` reflect a clean stop.
        let _ = self.rpc("debug/pause", json!({ "source": "cli" }));
        let pc = self.cur_pc();
        CmdResult::text(format!("PAUSE @ PC=${pc:04X}."))
    }

    fn verb_step(&self) -> CmdResult {
        self.running.store(false, Ordering::SeqCst);
        match self.rpc("debug/step", json!({ "source": "cli" })) {
            Ok(_) => {
                let pc = self.cur_pc();
                CmdResult::text(format!("STEP → PC=${pc:04X}."))
            }
            Err(e) => CmdResult::text(format!("step failed: {e}")),
        }
    }

    fn verb_mount(&self, path: &str) -> CmdResult {
        if path.is_empty() {
            return CmdResult::text("mount <path> — needs a .d64/.g64/.crt path.");
        }
        match self.rpc("media/mount", json!({ "path": path })) {
            Ok(v) => CmdResult::text(format!("MOUNT {path} → {}", compact(&v))),
            Err(e) => CmdResult::text(format!("mount failed: {e}")),
        }
    }

    fn verb_eject(&self) -> CmdResult {
        match self.rpc("media/unmount", json!({})) {
            Ok(_) => CmdResult::text("EJECT — drive8 unmounted."),
            Err(e) => CmdResult::text(format!("eject failed: {e}")),
        }
    }

    fn verb_load(&self, path: &str) -> CmdResult {
        if path.is_empty() {
            return CmdResult::text("load <prg> — needs a .prg path.");
        }
        match self.rpc("session/load_prg", json!({ "prg_path": path })) {
            Ok(v) => CmdResult::text(format!("LOAD {path} → {}", compact(&v))),
            Err(e) => CmdResult::text(format!("load failed: {e}")),
        }
    }

    fn verb_run_prg(&self, path: &str) -> CmdResult {
        // run <prg> = load + autostart, then free-run. Use session/load_prg then
        // runtime/run_prg via the file's bytes for the autostart semantics.
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => return CmdResult::text(format!("run {path}: read failed: {e}")),
        };
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let r = self.rpc("runtime/run_prg", json!({ "bytes_b64": b64 }));
        self.bump_epoch();
        match r {
            Ok(v) => {
                self.running.store(true, Ordering::SeqCst);
                CmdResult::text(format!("RUN {path} (autostart) → {}", compact(&v)))
            }
            Err(e) => CmdResult::text(format!("run {path} failed: {e}")),
        }
    }

    fn verb_warp(&self, sub: Option<&str>) -> CmdResult {
        match sub.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("on") => {
                self.warp.store(true, Ordering::SeqCst);
                let _ = self.rpc("session/set_pacing", json!({ "mode": "warp", "ratio": 8.0 }));
                CmdResult::text("WARP ON (8×).")
            }
            Some("off") | None => {
                self.warp.store(false, Ordering::SeqCst);
                let _ = self.rpc("session/set_pacing", json!({ "mode": "pal", "ratio": 1.0 }));
                CmdResult::text("WARP OFF (PAL real-time).")
            }
            Some(other) => CmdResult::text(format!("warp: unknown '{other}' (use on|off)")),
        }
    }

    fn verb_joystick(&self, sub: Option<&str>) -> CmdResult {
        // C64RE Spec 310: when ON, the window routes WASD+Space to the joystick; when
        // OFF they are normal keys. Default off so typing works.
        let prev = self.joystick_mode.load(Ordering::SeqCst);
        let mode = match sub.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("off") | None => 0u8,
            Some("port1") | Some("p1") | Some("1") => 1,
            Some("port2") | Some("p2") | Some("2") | Some("on") => 2,
            Some(other) => {
                return CmdResult::text(format!(
                    "joystick: unknown '{other}' (use off|port1|port2)"
                ))
            }
        };
        self.joystick_mode.store(mode, Ordering::SeqCst);
        // Release any held joystick on a port we're leaving (avoid a stuck direction).
        if prev != 0 && prev != mode {
            let _ = self.rpc("session/joystick_clear", json!({ "port": prev }));
        }
        let label = match mode {
            0 => "JOYSTICK OFF (WASD/Space type normally).".to_string(),
            p => format!("JOYSTICK ON — port {p} (WASD = directions, Space = fire)."),
        };
        CmdResult::text(label)
    }

    fn verb_dump(&self, path: &str) -> CmdResult {
        if path.is_empty() {
            return CmdResult::text("dump <path> — writes a .c64re snapshot.");
        }
        match self.rpc("snapshot/dump", json!({ "path": path })) {
            Ok(v) => CmdResult::text(format!("DUMP → {path} {}", compact(&v))),
            Err(e) => CmdResult::text(format!("dump failed: {e}")),
        }
    }

    fn verb_restore(&self, path: &str) -> CmdResult {
        if path.is_empty() {
            return CmdResult::text("restore <path> — loads a .c64re snapshot.");
        }
        let r = self.rpc("snapshot/undump", json!({ "path": path }));
        self.bump_epoch();
        match r {
            Ok(v) => CmdResult::text(format!("RESTORE ← {path} {}", compact(&v))),
            Err(e) => CmdResult::text(format!("restore failed: {e}")),
        }
    }

    fn verb_ringdump(&self, path: &str) -> CmdResult {
        if path.is_empty() {
            return CmdResult::text("ringdump <path> — writes a .c64rering reverse-debug buffer.");
        }
        match self.rpc("ringbuffer/dump", json!({ "path": path })) {
            Ok(v) => CmdResult::text(format!("RINGDUMP → {path} {}", compact(&v))),
            Err(e) => CmdResult::text(format!("ringdump failed: {e}")),
        }
    }

    fn verb_ringload(&self, path: &str) -> CmdResult {
        if path.is_empty() {
            return CmdResult::text("ringload <path> — loads a .c64rering reverse-debug buffer.");
        }
        let r = self.rpc("ringbuffer/restore", json!({ "path": path }));
        self.bump_epoch();
        match r {
            Ok(v) => CmdResult::text(format!("RINGLOAD ← {path} {}", compact(&v))),
            Err(e) => CmdResult::text(format!("ringload failed: {e}")),
        }
    }

    fn verb_monitor(&self, command: &str) -> CmdResult {
        match self.rpc("monitor/exec", json!({ "command": command })) {
            Ok(v) => {
                let out = v.get("output").and_then(|o| o.as_str()).unwrap_or("");
                CmdResult::text(out.to_string())
            }
            Err(e) => CmdResult::text(format!("monitor error: {e}")),
        }
    }

    // ── input (the emulator window forwards host keys/joystick through these) ────

    pub fn key_down(&self, key: &str) {
        let _ = self.rpc("session/key_down", json!({ "key": key }));
    }
    pub fn key_up(&self, key: &str) {
        let _ = self.rpc("session/key_up", json!({ "key": key }));
    }
    pub fn joystick_set(&self, port: u8, up: bool, down: bool, left: bool, right: bool, fire: bool) {
        let _ = self.rpc(
            "session/joystick_set",
            json!({ "port": port, "up": up, "down": down, "left": left, "right": right, "fire": fire }),
        );
    }
    pub fn joystick_clear(&self, port: u8) {
        let _ = self.rpc("session/joystick_clear", json!({ "port": port }));
    }

    // ── live state snapshot for the cockpit panels ──────────────────────────────

    /// Read `session/state` into a flat snapshot for the TUI panels.
    pub fn snapshot(&self) -> StateSnapshot {
        let v = self.rpc("session/state", json!({})).unwrap_or(Value::Null);
        StateSnapshot::from_json(&v, self.is_running(), self.is_warp())
    }

    fn cur_pc(&self) -> u16 {
        self.rpc("session/state", json!({}))
            .ok()
            .and_then(|v| v.get("cpu").and_then(|c| c.get("pc")).and_then(|p| p.as_u64()))
            .unwrap_or(0) as u16
    }
}

/// Flat, panel-ready view of `session/state`.
#[derive(Default, Clone)]
pub struct StateSnapshot {
    pub running: bool,
    pub warp: bool,
    pub c64_cycles: u64,
    pub drive_cycles: u64,
    pub pc: u16,
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub sp: u8,
    pub flags: u8,
    pub raster_line: u16,
    pub raster_cycle: u16,
    pub vic_mode: u8,
    pub border: u8,
    pub background: u8,
    pub irq_vec: u16,
    pub nmi_vec: u16,
    pub stop_reason: Option<String>,
}

impl StateSnapshot {
    fn from_json(v: &Value, running: bool, warp: bool) -> Self {
        let u = |path: &[&str]| -> u64 {
            let mut cur = v;
            for p in path {
                match cur.get(p) {
                    Some(n) => cur = n,
                    None => return 0,
                }
            }
            cur.as_u64().unwrap_or(0)
        };
        StateSnapshot {
            running,
            warp,
            c64_cycles: u(&["c64Cycles"]),
            drive_cycles: u(&["driveCycles"]),
            pc: u(&["cpu", "pc"]) as u16,
            a: u(&["cpu", "a"]) as u8,
            x: u(&["cpu", "x"]) as u8,
            y: u(&["cpu", "y"]) as u8,
            sp: u(&["cpu", "sp"]) as u8,
            flags: u(&["cpu", "flags"]) as u8,
            raster_line: u(&["vic", "rasterLine"]) as u16,
            raster_cycle: u(&["vic", "rasterCycle"]) as u16,
            vic_mode: u(&["vic", "mode"]) as u8,
            border: u(&["vic", "border"]) as u8,
            background: u(&["vic", "background"]) as u8,
            irq_vec: u(&["vectors", "irq"]) as u16,
            nmi_vec: u(&["vectors", "nmi"]) as u16,
            stop_reason: v.get("stopReason").and_then(|s| s.as_str()).map(|s| s.to_string()),
        }
    }

    /// 6502 flag byte → "NV-BDIZC" with set flags upper-cased.
    pub fn flags_str(&self) -> String {
        const NAMES: [char; 8] = ['N', 'V', '-', 'B', 'D', 'I', 'Z', 'C'];
        let mut s = String::with_capacity(8);
        for (i, c) in NAMES.iter().enumerate() {
            let bit = 7 - i;
            if (self.flags >> bit) & 1 == 1 {
                s.push(*c);
            } else {
                s.push(c.to_ascii_lowercase());
            }
        }
        s
    }
}

/// Compact a JSON value to a short one-line summary for the log pane.
fn compact(v: &Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    if s.len() > 160 {
        format!("{}…", &s[..160])
    } else {
        s
    }
}

pub fn help_text() -> String {
    "\
TRX64 cockpit — VM commands are /-prefixed; a bare line goes to the monitor.

  /-commands (the machine):
  /power on|off        cold boot / halt+reset to powered-off
  /reset [cold|warm]   power-cycle (fresh DRAM) / RESET line (RAM kept)
  /run                 resume free-running
  /run <prg>           load + autostart a .prg, then run
  /pause               freeze the machine
  /step                single-step one instruction
  /mount <path>        mount a .d64/.g64/.crt
  /eject               unmount drive8
  /load <prg>          load a .prg into RAM (no run)
  /warp on|off         8× / real-time PAL pacing
  /joystick off|port1|port2   route WASD+Space to the joystick (off = type)
  /window              spawn the native emulator window
  /dump <path>         write a .c64re snapshot
  /restore <path>      load a .c64re snapshot
  /ringdump <path>     write a .c64rering reverse-debug buffer
  /ringload <path>     load a .c64rering reverse-debug buffer
  /help                this help
  /quit                exit

  bare line → the VICE-superset monitor (~128 verbs), e.g.:
  d c000               disassemble    m 0400      memory dump
  r                    registers      bk e000     breakpoint
  g                    go             trace on    instruction trace
  whowrote d020        last writers   diff a b    checkpoint diff
"
    .to_string()
}
