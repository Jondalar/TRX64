//! trx64-cli — a cross-platform CLI cockpit + on-demand emulator window that drives
//! the TRX64 runtime IN-PROCESS.
//!
//! Spec: `docs/spec-trx64-cli.md`. This binary links `trx64-daemon` (the `[lib]`
//! target) and calls `dispatch()` + the A/V-pull helpers directly — no daemon, no
//! WebSocket, no FFI. One machine per process (`Arc<Mutex<State>>`), shared by:
//!   - the ratatui TUI cockpit (a worker thread),
//!   - the per-frame emulation pump (its own thread),
//!   - the optional winit/cpal emulator window (Part 2, on the MAIN thread).
//!
//! THREADING MODEL (macOS constraint): winit's `EventLoop` MUST run on the main
//! thread, so the main thread owns it; the TUI + pump run on worker threads, all
//! sharing the `Engine` (a cloneable handle around the `SharedState`). The `window`
//! verb signals the main thread (over an mpsc channel) to create the window.
//!
//! Modes:
//!   trx64-cli mon "d c000"   one-shot: run one command, print, exit (scripting/CI)
//!   trx64-cli                the TUI cockpit (default)
//!   trx64-cli --window       (Part 2) open the emulator window alongside the cockpit

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};

use trx64_cli::engine::Engine;
use trx64_cli::tui::{self, UiToMain};
use trx64_cli::window;
use trx64_cli::{boot_engine, default_rom_dir};

#[derive(Parser, Debug)]
#[command(name = "trx64-cli", version, about = "Cross-platform CLI cockpit + emulator window for the TRX64 runtime (in-process).")]
struct Cli {
    /// Open the native emulator window at launch (alongside the cockpit). Without this
    /// the window is spawned on demand via the cockpit's `window` verb.
    #[arg(long, default_value_t = false)]
    window: bool,

    /// ROM directory (KERNAL/BASIC/CHARGEN + 1541). Defaults to
    /// $C64RE_ROOT/resources/roms (matching the daemon's resolution).
    #[arg(long)]
    rom_dir: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// One-shot: run a single cockpit command (high-level verb OR monitor syntax),
    /// print its output, and exit. Pipeable / headless — no TUI. E.g.
    ///   trx64-cli mon "d c000"
    ///   trx64-cli mon "power on"
    Mon {
        /// The command to run (quote multi-word commands).
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let rom_dir = cli.rom_dir.clone().unwrap_or_else(default_rom_dir);

    // ── One-shot mode (no TUI, no window) ──────────────────────────────────────
    if let Some(Command::Mon { command }) = &cli.cmd {
        let line = command.join(" ");
        let engine = match boot_engine(&rom_dir) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        };
        let r = engine.exec_line(&line);
        if !r.output.is_empty() {
            println!("{}", r.output);
        }
        return;
    }

    // ── Interactive: cockpit (+ optional window) ────────────────────────────────
    let engine = match boot_engine(&rom_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    // Power-on semantics: a real C64 boots + runs when switched on. boot_engine
    // cold-boots to the reset vector; set the host run flag so the pump drives it
    // straight to READY instead of sitting frozen. (`/pause` to freeze.)
    let _ = engine.exec_line("/run");

    // The per-frame emulation pump: advances the machine one frame at a time while the
    // host run flag is set. Runs on its own thread, shares the Engine.
    let pump_engine = engine.clone();
    let pump = thread::spawn(move || {
        // Pace by WALL-CLOCK, like the SwiftUI AppModel pump: each tick advances the
        // cycles for the REAL time elapsed (`elapsed × PAL_CPU_HZ`), so the machine runs
        // at true PAL real-time and SID output matches 44100 Hz exactly. A fixed 50 fps
        // budget (19656 × 50 = 982800 cyc/s) ran slightly slow vs PAL's 985248 cyc/s →
        // SID production ≈ 43990/s < the 44100 output rate → the audio ring slowly
        // drained → constant underrun = crackle. A catch-up cap avoids a huge jump after
        // a stall; the ~5 ms tick also matches the audio drain cadence (steady, not
        // bursty). `pump_frame` no-ops while paused (host run flag clear).
        const PAL_CPU_HZ: f64 = 985_248.0; // PAL 6569 system clock
        const MAX_CATCHUP: u64 = 19_656 * 2; // ~2 frames
        let tick = Duration::from_millis(5);
        let mut last = std::time::Instant::now();
        loop {
            if pump_engine.should_quit() {
                break;
            }
            let now = std::time::Instant::now();
            let elapsed = now.duration_since(last).as_secs_f64();
            last = now;
            let cycles = ((elapsed * PAL_CPU_HZ) as u64).min(MAX_CATCHUP);
            pump_engine.pump_frame(cycles);
            thread::sleep(tick);
        }
    });

    // The cockpit runs on a worker thread; the MAIN thread stays free to own the
    // winit EventLoop (Part 2 macOS constraint).
    let (to_main_tx, to_main_rx) = mpsc::channel::<UiToMain>();
    let tui_engine = engine.clone();
    let tui = thread::spawn(move || {
        if let Err(e) = tui::run(tui_engine, to_main_tx) {
            eprintln!("cockpit error: {e}");
        }
    });

    // Main-thread loop: owns the winit EventLoop (macOS requires the main thread).
    // The cockpit's `window` verb opens the emulator window on demand; `--window`
    // opens it at launch. Both run alongside the cockpit on the SAME machine.
    window::main_thread_loop(&engine, to_main_rx, cli.window);

    engine_quit(&engine);
    let _ = tui.join();
    let _ = pump.join();
}

/// Ensure the quit flag is set so the pump + any window observe shutdown.
fn engine_quit(engine: &Engine) {
    let _ = engine.exec_line("/quit");
}
