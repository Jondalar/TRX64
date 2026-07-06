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
    /// Whether the DAEMON controller (`session.running`) is running — distinct from the
    /// host run flag. The pump reads this to adopt daemon-side run intents (a disk/CRT
    /// mount resume, monitor `g`/`x`) into the host pump (see [`Self::pump_frame`]).
    fn controller_running(&self) -> bool {
        self.rpc("session/state", json!({}))
            .ok()
            .and_then(|v| v.get("runState").and_then(|r| r.as_str()).map(|s| s == "running"))
            .unwrap_or(false)
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
    /// Advance the machine by `base_cycles` (the host pump passes the cycles for the
    /// REAL wall-clock time elapsed since the last tick — `elapsed × PAL_CPU_HZ` — so
    /// the machine runs at true PAL real-time and SID production matches 44100 Hz, like
    /// the SwiftUI AppModel pump; a fixed 50 fps budget drifted slow → audio crackle).
    pub fn pump_frame(&self, base_cycles: u64) -> u64 {
        // Reconcile the dual run-state. The CLI host pump is the clock, so the daemon
        // controller MUST stay paused (`session/run` refuses while `session.running`
        // is set). But a daemon op the CLI doesn't own can flip it true: a disk/CRT
        // mount resumes (`media/mount`), and the monitor `g`/`x` continue does too.
        // Treat that as a RUN INTENT — adopt the host run flag and force the controller
        // back to paused — so mount and `g` actually run, and `session/run` stays
        // legal. (Clearing the host flag on the resulting error was the "stuck PAUSED /
        // can't resume after mount" bug.)
        if self.controller_running() {
            self.running.store(true, Ordering::SeqCst);
            let _ = self.rpc("debug/pause", json!({ "source": "cli" }));
        }
        if !self.running.load(Ordering::SeqCst) {
            return 0;
        }
        let budget = if self.warp.load(Ordering::SeqCst) {
            base_cycles.saturating_mul(8)
        } else {
            base_cycles
        };
        if budget == 0 {
            return 0; // no wall-clock time elapsed this tick — nothing to advance
        }
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
                // A residual controller-running race — re-pause it and skip this tick.
                // KEEP the host flag so the next tick resumes (never strand the machine).
                let _ = self.rpc("debug/pause", json!({ "source": "cli" }));
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
        // `!`-prefixed = the FILESYSTEM namespace. The FS verbs (pwd/cd/ls/…) live in
        // the monitor, so `!ls` routes to monitor `ls`. The `!` prefix is a COCKPIT
        // routing layer ONLY — the shared `run_monitor` keeps every FS verb bare-
        // callable (C64RE drives them via `runtime_monitor`), so we never touch it.
        if let Some(rest) = line.strip_prefix('!') {
            let fs = rest.trim();
            if fs.is_empty() {
                return CmdResult::text(fs_help_text()); // bare "!" → the FS help
            }
            return self.verb_monitor(fs);
        }
        // `/`-prefixed = VM / high-level command (slash-command namespace); a bare
        // line = monitor passthrough (the ~128-verb VICE-superset — the primary
        // surface, so you type `d c000` / `r` / `bk e000` directly).
        let vm = match line.strip_prefix('/') {
            Some(rest) => rest.trim(),
            None => {
                // Cockpit nudge: the FS verbs now live behind `!`. If a bare line's
                // FIRST token is an FS verb, hint the `!` form instead of silently
                // running the monitor's copy. This is a cockpit-only routing hint —
                // `run_monitor` is unchanged, so the verbs stay bare-callable there.
                let first = line.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
                if FS_VERBS.contains(&first.as_str()) {
                    return CmdResult::text(format!(
                        "filesystem commands live behind '!' — try !{first}"
                    ));
                }
                return self.verb_monitor(line);
            }
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
            "eject" | "umount" => self.verb_eject(),
            "load" => self.verb_load(&arg),
            "warp" => self.verb_warp(rest.first().copied()),
            "joystick" | "joy" => self.verb_joystick(rest.first().copied()),
            "window" => CmdResult { output: "opening emulator window…".into(), open_window: true, quit: false },
            "dump" | "snapshot" => self.verb_dump(&arg),
            "restore" | "undump" | "loadsnapshot" => self.verb_restore(&arg),
            "ringdump" => self.verb_ringdump(&arg),
            "ringload" => self.verb_ringload(&arg),
            "settings" => self.verb_settings(),
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
        // Spec 786 — power is a first-class primitive on the daemon.
        match sub.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("on") | None => {
                // Full init (fresh machine, inserted media re-attached), running.
                let r = self.rpc("session/power", json!({ "op": "on" }));
                self.bump_epoch();
                match r {
                    Ok(v) => {
                        self.running.store(true, Ordering::SeqCst);
                        CmdResult::text(format!(
                            "POWER ON — full init @ PC=${:04X}, running.",
                            v.get("pc").and_then(|p| p.as_u64()).unwrap_or(0)
                        ))
                    }
                    Err(e) => CmdResult::text(format!("power on failed: {e}")),
                }
            }
            Some("off") => {
                // Everything off, no live state (blank dead machine, media registry kept).
                self.running.store(false, Ordering::SeqCst);
                let r = self.rpc("session/power", json!({ "op": "off" }));
                self.bump_epoch();
                match r {
                    Ok(_) => CmdResult::text("POWER OFF — machine off (no live state)."),
                    Err(e) => CmdResult::text(format!("power off failed: {e}")),
                }
            }
            Some(other) => CmdResult::text(format!("power: unknown sub '{other}' (use on|off)")),
        }
    }

    fn verb_reset(&self, sub: Option<&str>) -> CmdResult {
        // Spec 786 — reset [warm|cold]; DEFAULT warm (RESET line → $FCE2, RAM +
        // media kept). `cold` = power-cycle (power_off → power_on, fresh DRAM+chips).
        let (mode, label) = match sub.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("cold") => ("cold", "cold"),
            _ => ("soft", "warm"),
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
            Ok(v) => {
                // CLI-FEEL S7 — reconcile the dual run-state after the mount. A CRT
                // mount power-cycles the DAEMON into running (reply `paused:false`); adopt
                // that into the host run flag so the cockpit's pump resumes IMMEDIATELY
                // (the freshly cold-booted cart runs) instead of only after pump_frame's
                // next controller-running poll. A disk mount is a live device op that does
                // NOT change run-state — the reply reports the machine's REAL `paused`, so
                // a running machine stays running and a paused one stays paused (no false
                // resume). We therefore only ever SET the flag when the daemon says it is
                // running, never clear it.
                if v.get("paused").and_then(|p| p.as_bool()) == Some(false) {
                    self.running.store(true, Ordering::SeqCst);
                }
                CmdResult::text(format!("MOUNT {path} → {}", compact(&v)))
            }
            Err(e) => CmdResult::text(format!("mount failed: {e}")),
        }
    }

    fn verb_eject(&self) -> CmdResult {
        // CLI-FEEL S7 — smart target. The cockpit can't know what's mounted without a
        // round-trip, so it sends role:"auto" and the daemon resolves it against the live
        // machine: a cartridge is ejected if one is inserted, else the disk on drive8.
        // (The old `{}` payload made the daemon default to drive8, so `/eject` on a
        // cart-only machine tried to unmount an absent disk and left the cart in.)
        match self.rpc("media/unmount", json!({ "role": "auto" })) {
            Ok(v) => {
                // A cart eject power-cycles the daemon into running (`paused:false`) —
                // adopt it into the host run flag so the cockpit resumes immediately (same
                // reconcile as verb_mount).
                if v.get("paused").and_then(|p| p.as_bool()) == Some(false) {
                    self.running.store(true, Ordering::SeqCst);
                }
                let role = v
                    .get("detail")
                    .and_then(|d| d.get("role"))
                    .and_then(|r| r.as_str())
                    .unwrap_or("drive8");
                CmdResult::text(format!("EJECT — {role} unmounted."))
            }
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
            return CmdResult::text("dump <path> — writes a .c64re snapshot (alias: snapshot).");
        }
        match self.rpc("snapshot/dump", json!({ "path": path })) {
            Ok(v) => CmdResult::text(format!("DUMP → {path} {}", compact(&v))),
            Err(e) => CmdResult::text(format!("dump failed: {e}")),
        }
    }

    fn verb_restore(&self, path: &str) -> CmdResult {
        if path.is_empty() {
            return CmdResult::text("restore <path> — loads a .c64re snapshot (aliases: undump, loadsnapshot).");
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

    /// `/settings` — a read-only cockpit status summary: run-state, pacing/warp,
    /// the virtual-joystick mode, and the mounted disk + cartridge. Composed from
    /// the host run/warp flags plus read-only `session/state` / `session/list` /
    /// `session/cart_status` rpcs (no machine mutation).
    fn verb_settings(&self) -> CmdResult {
        let running = if self.is_running() { "running" } else { "paused" };
        let pacing = if self.is_warp() { "warp (8×)" } else { "PAL real-time (1×)" };
        let joy = match self.joystick_mode() {
            0 => "off (WASD/Space type normally)".to_string(),
            p => format!("port {p} (WASD = directions, Space = fire)"),
        };
        // Mounted disk (empty diskPath = none) — session/list carries it read-only.
        let disk = self
            .rpc("session/list", json!({}))
            .ok()
            .and_then(|v| {
                v.get(0)
                    .and_then(|s| s.get("diskPath"))
                    .and_then(|p| p.as_str())
                    .map(|p| p.to_string())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(none)".to_string());
        // Cartridge (null = none) — session/cart_status carries type + sourceName.
        let cart = match self.rpc("session/cart_status", json!({})) {
            Ok(Value::Null) | Err(_) => "(none)".to_string(),
            Ok(v) => {
                let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("cart");
                match v.get("sourceName").and_then(|s| s.as_str()) {
                    Some(name) if !name.is_empty() => format!("{name} ({ty})"),
                    _ => ty.to_string(),
                }
            }
        };
        let pc = self.cur_pc();
        CmdResult::text(format!(
            "TRX64 settings\n  \
             state:    {running} @ PC=${pc:04X}\n  \
             pacing:   {pacing}\n  \
             joystick: {joy}\n  \
             disk:     {disk}\n  \
             cart:     {cart}"
        ))
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

/// FS verbs that live in the monitor's file shell. Bare use of one of these in the
/// cockpit is NUDGED toward the `!` namespace (`!ls`); they remain bare-callable in
/// `run_monitor` itself (C64RE depends on that) — this list is a COCKPIT hint only.
pub const FS_VERBS: [&str; 10] =
    ["pwd", "cd", "ls", "dir", "mkdir", "rmdir", "load", "save", "bload", "bsave"];

pub fn help_text() -> String {
    "\
TRX64 cockpit = bash for the emulator. Three namespaces:
  /…  the machine   !…  the filesystem   bare  the monitor
Tab completes verbs in all three namespaces + paths for path arguments.

  /-commands (the machine):
  /power on|off        full init (fresh machine) / everything off, no state
  /reset [warm|cold]   RESET line → $FCE2 (default, RAM+media kept) / power-cycle
  /run                 resume free-running
  /run <prg>           load + autostart a .prg, then run
  /pause               freeze the machine
  /step                single-step one instruction
  /mount <path>        mount a .d64/.g64/.crt
  /eject | /umount     eject the cartridge or unmount drive8
  /load <prg>          load a .prg into RAM (no run)
  /warp on|off         8× / real-time PAL pacing
  /joystick off|port1|port2   route WASD+Space to the joystick (off = type)
  /window              spawn the native emulator window
  /dump | /snapshot <path>   write a .c64re snapshot
  /restore | /undump | /loadsnapshot <path>   load a .c64re snapshot
  /ringdump <path>     write a .c64rering reverse-debug buffer
  /ringload <path>     load a .c64rering reverse-debug buffer
  /settings            read-only status (pacing/warp/joystick/disk/cart)
  /help                this help
  /quit                exit

  !-commands (the filesystem — the monitor file shell, re-prefixed):
  !pwd  !cd <dir>  !ls|!dir [dir]  !mkdir <dir>  !rmdir <dir>
  !load \"<f>\" [addr]  !save \"<f>\" <a1> <a2>  !bload \"<f>\" <addr>  !bsave \"<f>\" <a1> <a2>

  bare line → the VICE-superset monitor (~128 verbs), e.g.:
  d c000               disassemble    m 0400      memory dump
  r                    registers      bk e000     breakpoint
  g                    go             trace on    instruction trace
  whowrote d020        last writers   diff a b    checkpoint diff
"
    .to_string()
}

/// Short help for a bare `!` (the filesystem namespace) — mirrors the monitor's file
/// shell verbs verbatim (argument shapes match `run_monitor`, main.rs:5379-5524).
pub fn fs_help_text() -> String {
    "\
!-commands (the filesystem — the monitor file shell, re-prefixed):
  !pwd                    print the working directory
  !cd <dir>               change directory (no arg → project dir)
  !ls | !dir [dir]        list a directory (default: cwd)
  !mkdir <dir>            make a directory (recursive)
  !rmdir <dir>            remove an empty directory
  !load \"<file>\" [addr]   PRG load into RAM (header load-addr, or override)
  !save \"<file>\" <a1> <a2>   save a RAM range as a PRG
  !bload \"<file>\" <addr>  raw binary load (no header)
  !bsave \"<file>\" <a1> <a2>  raw binary save (no header)
"
    .to_string()
}
