//! The main-thread loop (Part 1 stub) + the native emulator window (Part 2).
//!
//! PART 1: the main thread owns this loop deliberately — on macOS winit's `EventLoop`
//! must run on the main thread, so the architecture reserves the main thread here from
//! the start. In Part 1 there is no window yet; the loop just waits for the cockpit's
//! `OpenWindow` / `Quit` signals and acknowledges them. Part 2 replaces the body with
//! a real winit `EventLoop` (window + input) + a softbuffer blit + a cpal audio
//! stream, driving the SAME `Engine` over `Arc<Mutex<State>>`.

use std::sync::mpsc::Receiver;

use crate::engine::Engine;
use crate::tui::UiToMain;

/// Block the main thread, servicing cockpit signals until `Quit`. Part-1 behaviour:
/// `OpenWindow` logs that the window ships in Part 2; `Quit` returns.
pub fn main_thread_loop(_engine: &Engine, rx: Receiver<UiToMain>) {
    while let Ok(msg) = rx.recv() {
        match msg {
            UiToMain::OpenWindow => {
                eprintln!(
                    "[trx64-cli] `window`: the native emulator window ships in Part 2 — \
                     the cockpit + monitor are fully usable now."
                );
            }
            UiToMain::Quit => break,
        }
    }
}
