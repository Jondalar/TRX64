//! The native emulator window (Part 2) + the main-thread event-loop owner.
//!
//! THREADING MODEL (the macOS constraint, handled head-on):
//!   - The MAIN thread owns the winit `EventLoop` (created up front in
//!     [`main_thread_loop`], BEFORE any window exists). winit requires this on macOS.
//!   - The TUI cockpit + the emulation pump run on WORKER threads (spawned in
//!     `main`), all sharing the SAME machine via the cloneable [`Engine`]
//!     (`Arc<Mutex<State>>`). So you play in the window and debug in the TUI on the
//!     one machine.
//!   - Audio runs on cpal's own thread, fed by a producer thread (see `audio.rs`).
//!
//! ON-DEMAND SPAWN (the `window` verb — the goal, shipped): the cockpit sends
//! `UiToMain::OpenWindow` over an mpsc channel. A small bridge thread forwards that to
//! the EventLoop as a `UserEvent::Open` via an `EventLoopProxy` (the only thread-safe
//! way to poke a running winit loop). `ApplicationHandler::user_event` then creates the
//! window lazily. No window exists until `window` is invoked (or `--window` requests
//! one at launch). `UiToMain::Quit` → `UserEvent::Quit` exits the loop.
//!
//! VIDEO: per redraw, pull the 384×272 palette+index frame (`pull_frame_buffer`),
//! expand through the 16-colour LUT → RGBA(0RGB u32) → softbuffer blit, scaled to the
//! window. ~50 Hz via `ControlFlow::WaitUntil`.
//! INPUT: host keyboard → c64re matrix ids (`session/key_down`/`key_up`); arrows +
//! space/lalt → joystick port 2 (`session/joystick_*`).

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::audio::AudioOutput;
use crate::engine::Engine;
use crate::keymap;
use crate::tui::UiToMain;

/// User events posted to the winit loop from the cockpit-bridge thread.
#[derive(Debug, Clone, Copy)]
enum UserEvent {
    /// Create + show the emulator window (the `window` verb / `--window`).
    Open,
    /// Tear the loop down (the `quit` verb / cockpit exit).
    Quit,
}

/// Run the main-thread loop. Owns the winit `EventLoop`; bridges the cockpit's
/// `UiToMain` signals into `UserEvent`s; blocks until quit. `open_at_launch` opens the
/// window immediately (the `--window` flag).
pub fn main_thread_loop(engine: &Engine, rx: Receiver<UiToMain>, open_at_launch: bool) {
    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(el) => el,
        Err(e) => {
            // No display / headless host: fall back to a plain wait so the cockpit
            // still runs (the window simply can't open here).
            eprintln!("[trx64-cli] no event loop ({e}); window disabled, cockpit only.");
            while let Ok(msg) = rx.recv() {
                if matches!(msg, UiToMain::Quit) {
                    break;
                }
                eprintln!("[trx64-cli] `window`: no display available on this host.");
            }
            return;
        }
    };
    event_loop.set_control_flow(ControlFlow::Wait);

    // Bridge thread: forward cockpit signals → winit user events via the proxy.
    let proxy = event_loop.create_proxy();
    std::thread::Builder::new()
        .name("trx64-cli-uibridge".into())
        .spawn(move || {
            while let Ok(msg) = rx.recv() {
                let ev = match msg {
                    UiToMain::OpenWindow => UserEvent::Open,
                    UiToMain::Quit => UserEvent::Quit,
                };
                if proxy.send_event(ev).is_err() {
                    break; // loop gone
                }
                if matches!(ev, UserEvent::Quit) {
                    break;
                }
            }
        })
        .ok();

    let mut app = App::new(engine.clone(), open_at_launch);
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("[trx64-cli] event loop ended: {e}");
    }
}

/// The C64 displayed canvas is 384×272; open at 2× by default.
const CANVAS_W: u32 = 384;
const CANVAS_H: u32 = 272;
const FRAME: Duration = Duration::from_millis(20); // ~50 Hz

struct App {
    engine: Engine,
    open_pending: bool,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    audio: Option<AudioOutput>,
    next_frame: Instant,
    /// Joystick (port 2) edge state, so we only push on change.
    joy: JoyState,
    /// Whether a host Shift is held — drives the symbolic char mapping (Spec 310).
    shift_held: bool,
}

#[derive(Default, Clone, Copy, PartialEq)]
struct JoyState {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    fire: bool,
}

impl App {
    fn new(engine: Engine, open_at_launch: bool) -> Self {
        Self {
            engine,
            open_pending: open_at_launch,
            window: None,
            surface: None,
            audio: None,
            next_frame: Instant::now(),
            joy: JoyState::default(),
            shift_held: false,
        }
    }

    fn create_window(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            // Already open — just focus it.
            if let Some(w) = &self.window {
                w.focus_window();
            }
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("TRX64 — C64")
            .with_inner_size(LogicalSize::new((CANVAS_W * 2) as f64, (CANVAS_H * 2) as f64))
            .with_min_inner_size(LogicalSize::new(CANVAS_W as f64, CANVAS_H as f64));
        let window = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                eprintln!("[trx64-cli] window: create failed: {e}");
                return;
            }
        };
        // softbuffer context + surface bound to the window.
        let context = match Context::new(window.clone()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[trx64-cli] window: softbuffer context failed: {e}");
                return;
            }
        };
        let surface = match Surface::new(&context, window.clone()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[trx64-cli] window: softbuffer surface failed: {e}");
                return;
            }
        };
        self.surface = Some(surface);
        self.window = Some(window);

        // Start audio (best-effort; muted on failure). The first audioDrain installs
        // the runtime's persistent reSID render thread.
        if self.audio.is_none() {
            self.audio = AudioOutput::start(self.engine.shared_state().clone());
        }

        self.next_frame = Instant::now();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
        // SILENT. The emulator window shares a terminal with the TUI, and the TUI OWNS
        // that terminal — it draws boxes at absolute positions. Anything printed to
        // stderr from here lands inside those boxes and stays until a full repaint: the
        // "RUNNING[trx64-cli] emulator window clos..." smeared across the MACHINE panel
        // was exactly this. The cockpit already says the window is open.
    }

    fn render(&mut self) {
        let (Some(window), Some(surface)) = (self.window.as_ref(), self.surface.as_mut()) else {
            return;
        };
        let size = window.inner_size();
        let (win_w, win_h) = (size.width.max(1), size.height.max(1));
        if surface
            .resize(NonZeroU32::new(win_w).unwrap(), NonZeroU32::new(win_h).unwrap())
            .is_err()
        {
            return;
        }

        // Pull the live frame (palette + indices) and expand to a 0RGB LUT.
        let fb = trx64_daemon::pull_frame_buffer(self.engine.shared_state());
        let src_w = fb.width.max(1);
        let src_h = fb.height.max(1);
        // 16-entry 0x00RGB LUT from the 48-byte palette.
        let mut lut = [0u32; 256];
        for i in 0..16usize {
            let r = fb.palette.get(i * 3).copied().unwrap_or(0) as u32;
            let g = fb.palette.get(i * 3 + 1).copied().unwrap_or(0) as u32;
            let b = fb.palette.get(i * 3 + 2).copied().unwrap_or(0) as u32;
            lut[i] = (r << 16) | (g << 8) | b;
        }

        let Ok(mut buffer) = surface.buffer_mut() else { return };
        // Nearest-neighbour scale src (src_w×src_h) → window (win_w×win_h).
        for y in 0..win_h {
            let sy = (y as u64 * src_h as u64 / win_h as u64) as u32;
            let row = (sy.min(src_h - 1) * src_w) as usize;
            let dst_row = (y * win_w) as usize;
            for x in 0..win_w {
                let sx = (x as u64 * src_w as u64 / win_w as u64) as u32;
                let idx = fb.indices.get(row + sx.min(src_w - 1) as usize).copied().unwrap_or(0);
                buffer[dst_row + x as usize] = lut[idx as usize];
            }
        }
        let _ = buffer.present();
    }

    // ── input ──────────────────────────────────────────────────────────────────

    /// Handle a key event. Returns true if it was consumed as joystick input (so it is
    /// not ALSO sent to the keyboard matrix).
    fn handle_key(&mut self, event: &winit::event::KeyEvent, is_synthetic: bool) {
        let pressed = event.state == ElementState::Pressed;

        // Diagnostics for a machine we do not have. Three desktop targets ship, two of
        // them are used by people who report back as a photograph of a screen, and this
        // is the one thing that cannot be inferred from a photograph: what the platform
        // actually delivered. Off by default, one `if` per event when it is.
        if keylog_enabled() {
            eprintln!(
                "[keylog] {:?} synthetic={is_synthetic} repeat={} phys={:?} logical={:?} text={:?}",
                event.state, event.repeat, event.physical_key, event.logical_key, event.text
            );
        }

        // Track host Shift for the symbolic char mapping — from EVERY event including the
        // synthetic ones, because a focus-change snapshot is exactly the authority on
        // which modifiers are held.
        if let PhysicalKey::Code(code) = event.physical_key {
            if matches!(code, KeyCode::ShiftLeft | KeyCode::ShiftRight) {
                self.shift_held = pressed;
            }
        }

        if !accepts(is_synthetic, pressed) {
            return;
        }

        // HOST HOTKEYS — F9..F12 are the rewind transport (Spec 808 §4). The C64
        // keyboard has F1..F8 only (F2/F4/F6/F8 being SHIFT+F1/F3/F5/F7), so these four
        // physical keys cannot collide with anything the emulated machine can see.
        //
        //   F9  ◀|   one frame back      F11 ⏸/▶  pause / play
        //   F10 ◀◀   play backwards      F12 |▶   one frame forward
        //
        // These are the controls you want without taking your hands off the game, which
        // is why they are keys at all and not only monitor verbs.
        //
        // The SAME mapping lives in `tui.rs` — deliberately, not by accident: the two
        // surfaces have separate key paths (winit here, crossterm there) and the whole
        // point of a host hotkey is that it does the same thing wherever the focus
        // happens to be. Changing one without the other is how F10 kept pausing after
        // 808 moved pause to F11.
        if let PhysicalKey::Code(code) = event.physical_key {
            if matches!(
                code,
                KeyCode::F9 | KeyCode::F10 | KeyCode::F11 | KeyCode::F12
            ) {
                if pressed {
                    let f = match code {
                        KeyCode::F9 => 9u8,
                        KeyCode::F10 => 10,
                        KeyCode::F11 => 11,
                        _ => 12,
                    };
                    if let Some(line) =
                        trx64_daemon::transport::key_verb(f, self.engine.is_running())
                    {
                        // SILENT for the same reason: the verb's output already reaches
                        // the cockpit log, and echoing it here painted "[F10] REPLAY
                        // frame 1/20" straight through the CPU and VIC boxes.
                        let _ = self.engine.exec_line(line);
                    }
                }
                return; // never reaches the C64 matrix
            }
        }

        // Joystick (C64RE Spec 310): WASD = directions, Space = fire — but ONLY when
        // joystick mode is enabled (`/joystick port1|port2`). When off (the default),
        // WASD/Space are normal keys, so typing into BASIC works. The arrow keys are
        // the CURSOR keys (handled by the keyboard mapping below), matching C64RE.
        let joy_port = self.engine.joystick_mode();
        if joy_port != 0 {
            if let PhysicalKey::Code(code) = event.physical_key {
                if let Some(bit) = keymap::joy_bit(code) {
                    let mut new = self.joy;
                    match bit {
                        keymap::JoyBit::Up => new.up = pressed,
                        keymap::JoyBit::Down => new.down = pressed,
                        keymap::JoyBit::Left => new.left = pressed,
                        keymap::JoyBit::Right => new.right = pressed,
                        keymap::JoyBit::Fire => new.fire = pressed,
                    }
                    if new != self.joy {
                        self.joy = new;
                        if new == JoyState::default() {
                            self.engine.joystick_clear(joy_port);
                        } else {
                            self.engine.joystick_set(
                                joy_port, new.up, new.down, new.left, new.right, new.fire,
                            );
                        }
                    }
                    return;
                }
            }
        }

        // Spec 310 symbolic mapping: SPECIAL keys by physical position (layout-
        // independent), then PRINTABLE keys by the LOGICAL character (host-layout +
        // shift resolved by the OS — correct on QWERTZ etc.).
        let ids: Option<Vec<&'static str>> = match event.physical_key {
            PhysicalKey::Code(code) => keymap::map_special(code)
                .or_else(|| char_for(event).and_then(|ch| keymap::map_char(ch, self.shift_held))),
            PhysicalKey::Unidentified(_) => {
                char_for(event).and_then(|ch| keymap::map_char(ch, self.shift_held))
            }
        };
        if let Some(ids) = ids {
            if pressed {
                for id in &ids {
                    self.engine.key_down(id);
                }
            } else {
                // Release in reverse so the base key lifts before its L_SHIFT.
                for id in ids.iter().rev() {
                    self.engine.key_up(id);
                }
            }
        }
    }
}

/// May this key event reach the emulated machine?
///
/// Winit's Windows backend answers `WM_SETFOCUS` and `WM_KILLFOCUS` by synthesising a
/// whole keyboard state — `Pressed` for everything `GetAsyncKeyState` reports as held on
/// focus gain, `Released` for everything on focus loss — and marks those `is_synthetic`.
/// The macOS backend synthesises nothing at all (every `is_synthetic` there is a literal
/// `false`), which is why this only ever mattered on one platform.
///
/// The two directions are worth opposite things, so a blanket filter would be wrong:
///
/// - synthetic **Released** must pass. It is the safety net: alt-tab away mid-keypress and
///   without it the key stays held in the matrix and the C64 repeats it forever.
/// - synthetic **Pressed** must not. It replays keys the user never struck into the
///   emulated machine, simply for putting the window back in front.
const fn accepts(is_synthetic: bool, pressed: bool) -> bool {
    !(is_synthetic && pressed)
}

/// Whether `TRX64_KEYLOG` asked for a trace of every key event. Read once — this is
/// consulted per keystroke.
fn keylog_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("TRX64_KEYLOG").map(|v| v != "0" && !v.is_empty()).unwrap_or(false)
    })
}

/// The host-layout + shift resolved character for a key event (winit `logical_key`),
/// for the Spec-310 symbolic printable mapping. `None` for non-character (named) keys.
fn char_for(event: &winit::event::KeyEvent) -> Option<char> {
    if let Key::Character(s) = &event.logical_key {
        s.chars().next()
    } else {
        None
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        // If a window was requested before the loop was ready (--window), open it now.
        if self.open_pending {
            self.open_pending = false;
            self.create_window(el);
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Open => self.create_window(el),
            UserEvent::Quit => el.exit(),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                // Closing the window does NOT quit the app — the cockpit keeps running.
                // Drop the window + audio; release any held keys/joystick.
                self.engine.joystick_clear(2);
                self.audio = None;
                self.surface = None;
                self.window = None;
                el.set_control_flow(ControlFlow::Wait);
                // SILENT — the cockpit's own status already reflects the closed window.
            }
            WindowEvent::RedrawRequested => {
                self.render();
            }
            // `is_synthetic` is taken, not discarded: it is the whole difference between a
            // key the user struck and one Windows replayed on a focus change. See `accepts`.
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                self.handle_key(&event, is_synthetic);
            }
            WindowEvent::Resized(_) => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.engine.should_quit() {
            el.exit();
            return;
        }
        if self.window.is_none() {
            el.set_control_flow(ControlFlow::Wait);
            return;
        }
        // ~50 Hz redraw cadence.
        let now = Instant::now();
        if now >= self.next_frame {
            self.next_frame = now + FRAME;
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.next_frame));
    }
}

#[cfg(test)]
mod tests {
    use super::accepts;

    #[test]
    fn synthetic_presses_are_dropped_but_synthetic_releases_are_not() {
        // Real input is untouched in both directions.
        assert!(accepts(false, true), "a real press reaches the machine");
        assert!(accepts(false, false), "a real release reaches the machine");

        // Winit's Windows backend replays a whole keyboard state on WM_SETFOCUS. Letting
        // that through types into the emulated C64 for merely alt-tabbing back in.
        assert!(!accepts(true, true), "a synthetic press must NOT reach the machine");

        // ...but the WM_KILLFOCUS half is the safety net that lifts keys still held when
        // focus goes away. Dropping it would leave the C64 repeating that key forever.
        assert!(accepts(true, false), "a synthetic release MUST still reach the machine");
    }
}
