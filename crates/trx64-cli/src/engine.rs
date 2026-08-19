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

    /// Asks the DAEMON. There is no client-side run flag any more — that is the whole
    /// point of the 808 rebuild. A cached mirror was tried and immediately went stale in
    /// any path that did not pump, which is the same class of bug as the flag it replaced.
    pub fn is_running(&self) -> bool {
        self.rpc("session/state", json!({}))
            .ok()
            .and_then(|v| v.get("runState").and_then(|r| r.as_str()).map(|s| s == "running"))
            .unwrap_or(false)
    }
    /// Reads the DAEMON. The cockpit does not keep its own pacing flag.
    pub fn is_warp(&self) -> bool {
        self.rpc("session/state", json!({}))
            .ok()
            .and_then(|v| v.get("warp").and_then(|w| w.as_bool()))
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
        // Spec 808 rebuild — the client hands over the real time that passed and renders
        // what comes back. It does NOT decide whether the machine should advance, whether
        // the transport should step, or how fast either happens. All of that is one
        // decision in `session/tick`, in the one place that knows the whole state.
        //
        // What this replaces: a client-side `running` flag that called itself "the
        // one client-side run flag that claimed to outrank the daemon's, plus a
        // `transport_playing` flag, plus a reconciliation hack for when they disagreed.
        // Every 808 state bug lived in that seam.
        match self.rpc("session/tick", json!({ "cycles": base_cycles })) {
            Ok(v) => {
                // Follow the daemon's audio epoch every tick. This is how power, reset
                // and a CRT mount reach the audio path without the client knowing they
                // are audio-relevant.
                if let Some(e) = v.get("audioEpoch").and_then(|e| e.as_u64()) {
                    if e != self.epoch.load(Ordering::SeqCst) {
                        self.epoch.store(e, Ordering::SeqCst);
                    }
                }
                v.get("c64Cycles").and_then(|c| c.as_u64()).unwrap_or(0)
            }
            Err(_) => 0,
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
        // Strip ONE surrounding pair of matched quotes so `/mount "a b.crt"` yields the
        // path `a b.crt`, not the literal `"a b.crt"` (the quotes were kept in the path →
        // file-not-found). `/mount a b.crt` (no quotes) already worked — join handles the
        // space — so this only rescues the quoted form.
        let joined = rest.join(" ");
        let arg = joined
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| joined.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(&joined)
            .to_string();

        match verb.as_str() {
            // The ONLY verbs that stay here are the ones the daemon cannot answer,
            // because they are about THIS terminal rather than about the machine.
            //
            // Everything else — power, reset, run, pause, step, warp, load, dump,
            // restore, ringdump, ringload, turbo, and every verb the daemon grows
            // tomorrow — is forwarded verbatim. The cockpit used to reimplement each
            // one: its own argument validation ("power: unknown sub 'x'"), its own
            // reply text, sometimes its own extra RPCs. That made this front-end a
            // second authority on what a verb means and what counts as valid, and a
            // second authority drifts — which is BUG-040, and which is why `turbo`
            // shipped in the daemon and read "unknown command" here.
            //
            // `window` is NOT forwarded even though the daemon has a verb by that
            // name: there it sets the checkpoint-ring window in seconds. Forwarding
            // it would silently retune the ring when someone asked for a window.
            "window" => CmdResult { output: "opening emulator window…".into(), open_window: true, quit: false },
            "settings" => self.verb_settings(),
            "help" => CmdResult::text(help_text()),
            "quit" | "exit" => {
                self.quit.store(true, Ordering::SeqCst);
                CmdResult { output: "bye.".into(), open_window: false, quit: true }
            }

            // Media and input have no monitor verb yet — the media handlers are a
            // 170-line block inside the RPC dispatch and have to be extracted first
            // (BUG-041 says so in as many words). Until then these stay as THIN
            // calls: no validation, no invented message, just the RPC and whatever
            // the daemon says back.
            "mount" => self.verb_mount(&arg),
            "eject" | "umount" => self.verb_eject(),
            "joystick" | "joy" => self.verb_joystick(rest.first().copied()),

            other => {
                let forwarded = if rest.is_empty() {
                    other.to_string()
                } else {
                    format!("{other} {}", rest.join(" "))
                };
                let out = self.verb_monitor(&forwarded);
                // A forwarded verb can power-cycle or reset the machine, and the
                // audio epoch is this front-end's follow-state. Following the daemon
                // afterwards is not validation — it is the client catching up with
                // the authority, which is the only thing a client should be doing.
                self.follow_daemon_audio_epoch();
                out
            }
        }
    }






    fn verb_mount(&self, path: &str) -> CmdResult {
        if path.is_empty() {
            return CmdResult::text("mount <path> — needs a .d64/.g64/.crt path.");
        }
        match self.rpc("media/mount", json!({ "path": path })) {
            Ok(v) => {
                // A CRT mount power-cycles the machine into running; a disk mount is a
                // live device op that does not change run-state. Either way the DAEMON
                // decides and reports it in `paused`, and this only forwards that as an
                // intent. (This used to reconcile a client-side run flag against the
                // daemon's — the seam Spec 808's rebuild removed.)
                if v.get("paused").and_then(|p| p.as_bool()) == Some(false) {
                    let _ = self.rpc("session/play", json!({}));
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
                    let _ = self.rpc("session/play", json!({}));
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





    /// Spec 808 — F11, the universal go/stop. A DECISION, not a key mapping: what
    /// "resume" means depends on where the transport stands, and the daemon owns that.
    /// One `transport/status` call per keypress (not per frame), then the verb.
    ///
    /// Also the only place the pump's second run-reason gets armed: starting playback
    /// has to switch the pump on, or the tick that steps the transport never fires —
    /// rewinding means the machine is NOT running, so gating the pump on `running`
    /// alone deadlocked it.
    pub fn transport_toggle(&self) -> CmdResult {
        // ONE event, ONE message, printed verbatim. The decision AND the wording are the
        // daemon's, because only it knows the whole state — and because a client that
        // assembles the text will assemble it differently at its second call site, which
        // is exactly how the buffer range showed up on `/pause` but not on F11.
        let v = self.rpc("transport/toggle", json!({})).unwrap_or_default();
        self.resync_after_transport_move();
        CmdResult::text(
            v.get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("transport: no reply")
                .to_string(),
        )
    }

    /// Send a transport verb. The client does not track what it did — the daemon's
    /// reply is the truth and the next tick renders it.
    pub fn transport_key(&self, verb: &str) -> CmdResult {
        let r = self.exec_line(verb);
        self.resync_after_transport_move();
        r
    }

    /// The audio queue is filled from the machine's clock, so any op that makes that
    /// clock discontinuous — power, reset, a restore, a transport move, pause, play —
    /// leaves queued sound belonging to a time we left. That is what the doubled audio
    /// on resume was.
    ///
    /// The client does NOT keep the list of which verbs those are. The DAEMON bumps an
    /// `audioEpoch` and this follows it: the list grows (the transport added four), a
    /// second front-end would have to learn it independently, and a missed entry is
    /// silent — you simply hear the old sound over the new position.
    fn follow_daemon_audio_epoch(&self) {
        let daemon = self
            .rpc("session/state", json!({}))
            .ok()
            .and_then(|v| v.get("audioEpoch").and_then(|e| e.as_u64()))
            .unwrap_or(0);
        if daemon != self.epoch.load(Ordering::SeqCst) {
            self.epoch.store(daemon, Ordering::SeqCst);
        }
    }

    /// Kept as the name the transport paths call; it now just follows the daemon.
    fn resync_after_transport_move(&self) {
        self.follow_daemon_audio_epoch();
    }

    fn verb_monitor(&self, command: &str) -> CmdResult {
        match self.rpc("monitor/exec", json!({ "command": command })) {
            Ok(v) => {
                // monitor/exec reports a VERB failure as a successful rpc carrying an
                // `error` field (matching the TS `{output}|{error}` shape), so reading
                // only `output` rendered every monitor error as a BLANK line in the
                // cockpit — the command looked like it did nothing at all.
                if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
                    return CmdResult::text(err.to_string());
                }
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
        // Both flags come out of the DAEMON's reply. Reading them from client-side
        // atomics is what let the header say PAUSE while the machine was running.
        let running = v
            .get("runState")
            .and_then(|r| r.as_str())
            .map(|s| s == "running")
            .unwrap_or(false);
        let warp = v.get("warp").and_then(|w| w.as_bool()).unwrap_or(false);
        StateSnapshot::from_json(&v, running, warp)
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
    /// Spec 808 — what the transport is doing, for the MACHINE header. The user presses
    /// a key and expects the header to name that key's action: PLAY, PAUSE, REWIND,
    /// STEP. "PAUSED" alone answered a different question.
    pub transport_mode: Option<String>,
    pub transport_line: Option<String>,
    /// "back" | "fwd" | "none" — the header shows REWIND only when actually going back.
    pub transport_direction: Option<String>,
    /// What drive 8 is DOING, from `session/state.device.drive8` — the same block
    /// `session/drive_status` serves, carried along so the cockpit draws its whole
    /// row from ONE snapshot instead of three RPCs (BUG-044).
    pub drive: DriveSnapshot,
    /// The cartridge panel, `None` when the port is empty.
    pub cart: Option<CartSnapshot>,
}

/// Drive 8's live panel. `led_pwm` is a DUTY CYCLE over the period since the last
/// poll (0..1000), not a level — see `Rotation::led_pwm` in the core.
#[derive(Clone, Debug, Default)]
pub struct DriveSnapshot {
    pub present: bool,
    pub led_on: bool,
    pub led_pwm: u16,
    pub motor_on: bool,
    /// "read" | "write" — what the drive's read/write mode says.
    pub rw_mode: String,
    pub half_track: u16,
    pub track: u16,
    pub sector: u16,
    pub drive_pc: u16,
    pub dd00_pra: u8,
    pub dd00_ddr: u8,
    /// "kernal" | "idle" | "custom".
    pub transfer_mode: String,
}

#[derive(Clone, Debug, Default)]
pub struct CartSnapshot {
    pub mapper: String,
    pub bank: u16,
    /// "write" | "read" | "idle".
    pub activity: String,
    pub source_name: Option<String>,
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
            transport_mode: v
                .get("transport")
                .and_then(|t| t.get("mode"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string()),
            transport_line: v
                .get("transport")
                .and_then(|t| t.get("line"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string()),
            transport_direction: v
                .get("transport")
                .and_then(|t| t.get("direction"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string()),
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
            drive: {
                let d = v.get("device").and_then(|d| d.get("drive8"));
                let b = |k: &str| d.and_then(|d| d.get(k)).and_then(|x| x.as_bool()).unwrap_or(false);
                let n = |k: &str| d.and_then(|d| d.get(k)).and_then(|x| x.as_u64()).unwrap_or(0);
                let s = |k: &str, dflt: &str| {
                    d.and_then(|d| d.get(k))
                        .and_then(|x| x.as_str())
                        .unwrap_or(dflt)
                        .to_string()
                };
                DriveSnapshot {
                    present: d.is_some(),
                    led_on: b("ledOn"),
                    led_pwm: n("ledPwm") as u16,
                    motor_on: b("motorOn"),
                    rw_mode: s("rwMode", "read"),
                    half_track: n("halfTrack") as u16,
                    track: n("track") as u16,
                    sector: n("sector") as u16,
                    drive_pc: n("drivePc") as u16,
                    dd00_pra: d
                        .and_then(|d| d.get("dd00"))
                        .and_then(|x| x.get("pra"))
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0) as u8,
                    dd00_ddr: d
                        .and_then(|d| d.get("dd00"))
                        .and_then(|x| x.get("ddr"))
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0) as u8,
                    transfer_mode: s("transferMode", "-"),
                }
            },
            cart: v
                .get("device")
                .and_then(|d| d.get("cart"))
                .filter(|c| !c.is_null())
                .map(|c| CartSnapshot {
                    mapper: c.get("type").and_then(|x| x.as_str()).unwrap_or("?").to_string(),
                    bank: c.get("bank").and_then(|x| x.as_u64()).unwrap_or(0) as u16,
                    activity: c
                        .get("activity")
                        .and_then(|x| x.as_str())
                        .unwrap_or("idle")
                        .to_string(),
                    source_name: c
                        .get("sourceName")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string()),
                }),
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
