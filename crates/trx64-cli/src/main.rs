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

use trx64_cli::boot_cmd;
use trx64_cli::disasm_cmd::{self, DisasmArgs};
use trx64_cli::sandbox_cmd;
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

    /// Static disassembly of a PRG or raw memory image — machine-free (no ROMs,
    /// no boot). Capability-cut step 1: the shared `trx64-static` decoder (the
    /// same disassembler as the monitor `d` verb). PRG by default (2-byte
    /// load-address header); `--load-address` switches to raw-image mode. E.g.
    ///   trx64cli disasm game.prg
    ///   trx64cli disasm dump.bin --load-address $c000 --count 32 --json
    Disasm {
        /// Input file (.prg by default; any raw image with --load-address).
        file: PathBuf,
        /// Treat FILE as a raw image loaded at this address (hex: $c000 / 0xc000 / c000).
        #[arg(long, value_parser = trx64_cli::disasm_cmd::parse_addr)]
        load_address: Option<u16>,
        /// First address to disassemble (default: the load address).
        #[arg(long, value_parser = trx64_cli::disasm_cmd::parse_addr)]
        start: Option<u16>,
        /// Maximum number of instructions to emit (default: to end of image).
        #[arg(long)]
        count: Option<usize>,
        /// Emit JSON (array of {addr, bytes, mnemonic, operand, text}) instead of text.
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// One-shot real-core execution sandbox (Spec 787 v1 + 788): boot a fresh
    /// machine (this process = one throwaway scratch instance), load bytes, run the
    /// title's OWN routine to a sentinel, harvest a RAM slice. Runs on the
    /// AUTHORITATIVE 6502 — not the TS shadow — so a banking/IO depacker executes
    /// for real. E.g.
    ///   trx64cli sandbox --load depacker.prg --load packed.bin@$2000 \
    ///                    --entry '$0334' --harvest '$4000:0x800' --json
    Sandbox {
        /// Optional .c64re snapshot to restore before running (loader-resident seed);
        /// the routine then runs on top of that state. Generate one with `boot`.
        #[arg(long)]
        seed: Option<String>,
        /// Attach a cart on the cold machine. NOTE: an EF/ultimax cart REMAPS memory
        /// ($8000/$E000 = cart ROM, $4000 = open bus), so a naive RAM stub+routine may
        /// not run cleanly — for a cart-resident depacker prefer --seed (a .c64re of
        /// the cart already running in its banked state).
        #[arg(long)]
        cart: Option<String>,
        /// Spec 790 — cart type for a raw `.bin` `--cart`: a VICE id (32, 86, 61, …)
        /// or mnemonic (ef/easyflash, gmod2, megabyter/mb, c64megacart/c64mc,
        /// magicdesk/md, md16, ocean, 8k, 16k, ultimax). REQUIRED for a raw `.bin`
        /// unless its type auto-detects structurally; an OPTIONAL header override for
        /// a `.crt`. Omit (or `auto`/`crt`) to auto-detect.
        #[arg(long = "cart-type")]
        cart_type: Option<String>,
        /// Attach a disk on the cold machine (.d64/.g64; for a drive-reading routine).
        #[arg(long)]
        disk: Option<String>,
        /// A blob to load: FILE@ADDR (ADDR hex), or FILE alone for a .prg (2-byte
        /// load-address header). Repeatable. Optional when --seed supplies the code.
        #[arg(long = "load")]
        load: Vec<String>,
        /// Inline byte load: $ADDR=<hexbytes> (e.g. --load-hex '$4000=a2ff8d').
        /// Repeatable. Like --load but from literal bytes (no temp file).
        #[arg(long = "load-hex")]
        load_hex: Vec<String>,
        /// Entry PC of the routine to call (hex).
        #[arg(long, value_parser = trx64_cli::disasm_cmd::parse_addr)]
        entry: u16,
        /// RAM range to harvest after the run: ADDR:LEN (LEN decimal or 0x-hex).
        /// Repeatable — the first is the primary range echoed in text + the
        /// back-compat `harvest` JSON field; all are returned in `harvests`.
        #[arg(long)]
        harvest: Vec<String>,
        /// Seed a zero-page byte before the run: ADDR=VAL (both hex). Repeatable —
        /// depackers take their src/dst pointers here (e.g. --zp $fb=$00 --zp $fc=$20).
        #[arg(long = "zp")]
        zp: Vec<String>,
        /// Extra sentinel breakpoint besides the routine's RTS-return (hex).
        #[arg(long, value_parser = trx64_cli::disasm_cmd::parse_addr)]
        sentinel: Option<u16>,
        /// $01 memory-config the entry stub sets before calling (hex byte, default $37).
        #[arg(long)]
        io: Option<String>,
        /// Address of the 11-byte entry stub (hex, default $02a7 free RAM). Stub mode only.
        #[arg(long, value_parser = trx64_cli::disasm_cmd::parse_addr)]
        stub_addr: Option<u16>,
        /// Cycle budget cap (default 100_000_000).
        #[arg(long)]
        cyc_cap: Option<u64>,
        /// Instruction cap (default 40_000_000).
        #[arg(long)]
        instr_cap: Option<u64>,
        /// Direct-entry mode (TS-faithful): set PC=entry, seed registers, and pre-stage
        /// the RTS sentinel on the stack ($01FE=$FD/$01FF=$FF ⇒ RTS → $FFFE) instead of
        /// the `jsr entry` stub — so A/X/Y are observed unclobbered at entry. Auto-enabled
        /// by any --reg-*.
        #[arg(long, default_value_t = false)]
        direct_entry: bool,
        /// Seed A observed at entry (hex $xx / 0x / decimal). Implies --direct-entry.
        #[arg(long = "reg-a")]
        reg_a: Option<String>,
        /// Seed X observed at entry (hex/decimal). Implies --direct-entry.
        #[arg(long = "reg-x")]
        reg_x: Option<String>,
        /// Seed Y observed at entry (hex/decimal). Implies --direct-entry.
        #[arg(long = "reg-y")]
        reg_y: Option<String>,
        /// Seed SP observed at entry (hex/decimal, default $FD). Implies --direct-entry.
        #[arg(long = "reg-sp")]
        reg_sp: Option<String>,
        /// Seed the P status register observed at entry (hex/decimal, default $22 =
        /// TS Cpu6502 power-on flags). Implies --direct-entry.
        #[arg(long = "reg-p")]
        reg_p: Option<String>,
        /// Stream-hook PC (hex): when the routine reaches this PC, DON'T execute it —
        /// inject A = next stream byte, clear carry, and RTS (ported from the TS
        /// sandbox get_byte hook). Repeatable. Feed bytes with --stream / --stream-hex.
        /// Works with both the stub path and --direct-entry.
        #[arg(long = "stream-hook")]
        stream_hook: Vec<String>,
        /// File whose bytes feed the --stream-hook PCs, consumed in order.
        #[arg(long)]
        stream: Option<String>,
        /// Inline hex bytes appended to the --stream-hook feed (e.g. --stream-hex
        /// 'de ad be ef'). Applied after --stream.
        #[arg(long = "stream-hex")]
        stream_hex: Option<String>,
        /// Emit JSON instead of text.
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Boot a disk/cart in an isolated process (own machine, no daemon, no shared
    /// session) to a state, then dump a .c64re snapshot — mints seeds/fixtures for
    /// `sandbox --seed`. E.g.
    ///   trx64cli boot --disk scramble.d64 --type 'LOAD"*",8,1\rRUN\r' \
    ///                 --cycles 90000000 --dump scramble.c64re
    Boot {
        /// Disk/cart to mount (.d64/.g64/.crt).
        #[arg(long)]
        disk: String,
        /// Cycles to run to the READY prompt before typing (default 3_000_000).
        #[arg(long, default_value_t = 3_000_000)]
        warmup: u64,
        /// Keystrokes to queue (\r = RETURN). Repeatable — put LOAD and RUN in
        /// SEPARATE --type flags. E.g. --type 'LOAD"*",8,1\r' --type 'RUN\r'.
        #[arg(long = "type")]
        type_text: Vec<String>,
        /// Cycles to run after EACH --type so its command completes (default 40_000_000).
        #[arg(long, default_value_t = 40_000_000)]
        type_gap: u64,
        /// Final settle cycles after the last --type (~985248/s PAL). Default 90_000_000.
        #[arg(long, default_value_t = 90_000_000)]
        cycles: u64,
        /// Per session/run-call cycle chunk (default 10_000_000).
        #[arg(long, default_value_t = 10_000_000)]
        chunk: u64,
        /// Output .c64re snapshot path.
        #[arg(long)]
        dump: String,
        /// Also write a PNG screenshot of the final screen (verify what booted).
        #[arg(long)]
        render: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let rom_dir = cli.rom_dir.clone().unwrap_or_else(default_rom_dir);

    // ── Static one-shot: disasm (no machine, no ROMs, no TUI) ──────────────────
    if let Some(Command::Disasm { file, load_address, start, count, json }) = &cli.cmd {
        match disasm_cmd::run_disasm(&DisasmArgs {
            file,
            load_address: *load_address,
            start: *start,
            count: *count,
            json: *json,
        }) {
            Ok(out) => println!("{out}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        }
        return;
    }

    // ── Real-core sandbox one-shot (Spec 787 v1 + 788; own machine, no TUI) ─────
    if let Some(Command::Sandbox {
        seed, cart, cart_type, disk, load, load_hex, entry, harvest, zp, sentinel, io, stub_addr,
        cyc_cap, instr_cap, direct_entry, reg_a, reg_x, reg_y, reg_sp, reg_p, stream_hook, stream,
        stream_hex, json,
    }) = &cli.cmd
    {
        match sandbox_cmd::run_sandbox_cli(
            &rom_dir, seed.as_deref(), cart.as_deref(), cart_type.as_deref(), disk.as_deref(), load,
            load_hex, *entry, harvest, zp, *sentinel, io.as_deref(), *stub_addr, *cyc_cap,
            *instr_cap, *direct_entry, reg_a.as_deref(), reg_x.as_deref(), reg_y.as_deref(),
            reg_sp.as_deref(), reg_p.as_deref(), stream.as_deref(), stream_hex.as_deref(),
            stream_hook, *json,
        ) {
            Ok(out) => println!("{out}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        }
        return;
    }

    // ── Boot-and-dump one-shot (isolated scratch instance → .c64re fixture) ─────
    if let Some(Command::Boot { disk, warmup, type_text, type_gap, cycles, chunk, dump, render }) = &cli.cmd {
        match boot_cmd::run_boot(&rom_dir, disk, *warmup, type_text, *type_gap, *cycles, *chunk, dump, render.as_deref()) {
            Ok(out) => println!("{out}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        }
        return;
    }

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
