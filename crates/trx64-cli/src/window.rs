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
use winit::keyboard::{Key, PhysicalKey};
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
        eprintln!("[trx64-cli] emulator window open — play here, debug in the cockpit.");
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
    fn handle_key(&mut self, event: &winit::event::KeyEvent) {
        let pressed = event.state == ElementState::Pressed;

        // Joystick (port 2): arrows = directions, Left-Alt = fire. (Fire is NOT space,
        // so the spacebar stays free for typing into BASIC; cursor movement in BASIC is
        // sacrificed to the joystick — this is a play-focused window.) Push on change.
        if let PhysicalKey::Code(code) = event.physical_key {
            use winit::keyboard::KeyCode::*;
            let mut new = self.joy;
            let mut is_joy = true;
            match code {
                ArrowUp => new.up = pressed,
                ArrowDown => new.down = pressed,
                ArrowLeft => new.left = pressed,
                ArrowRight => new.right = pressed,
                AltLeft | AltRight => new.fire = pressed,
                _ => is_joy = false,
            }
            if is_joy {
                if new != self.joy {
                    self.joy = new;
                    if new == JoyState::default() {
                        self.engine.joystick_clear(2);
                    } else {
                        self.engine
                            .joystick_set(2, new.up, new.down, new.left, new.right, new.fire);
                    }
                }
                return;
            }
        }

        // Keyboard matrix: resolve the physical key (+ modifiers) to a c64re id.
        if let Some(mapped) = keymap::resolve(event.physical_key) {
            if mapped.shift && pressed {
                self.engine.key_down("L_SHIFT");
            }
            if pressed {
                self.engine.key_down(mapped.key);
            } else {
                self.engine.key_up(mapped.key);
                if mapped.shift {
                    self.engine.key_up("L_SHIFT");
                }
            }
            return;
        }
        // Fallback: named logical keys (modifiers winit reports only logically).
        if let Key::Named(named) = &event.logical_key {
            if let Some(id) = keymap::map_named(*named) {
                if pressed {
                    self.engine.key_down(id);
                } else {
                    self.engine.key_up(id);
                }
            }
        }
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
                eprintln!("[trx64-cli] emulator window closed — cockpit still running.");
            }
            WindowEvent::RedrawRequested => {
                self.render();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                self.handle_key(&event);
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
