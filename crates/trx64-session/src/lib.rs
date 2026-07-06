//! trx64-session — instance lifecycle.
//!
//! Boot-paused, idle-safe, opChain-serialized mutations, media mount, snapshot ring /
//! rewind tree. Phase-2 home of warp + parallel `explore()` over COW machine forks.

use trx64_core::cart::{CartMapper, ParsedCartridgeImage};
use trx64_core::drive::DiskImage;
use trx64_core::Machine;

/// Spec 786 — a cartridge held in the session's media registry while the
/// machine is powered OFF. The live mapper + parsed image are transplanted
/// verbatim (NOT rebuilt from bytes) so EasyFlash flash state persists across
/// a power-cycle: a physical cartridge does not lose its contents when the
/// C64 is switched off.
pub struct InsertedCart {
    pub image: ParsedCartridgeImage,
    pub mapper: Box<dyn CartMapper>,
    /// Host `.crt` path (persist/eject writeback + reporting). Empty for
    /// uploaded bytes with no backing file.
    pub path: String,
}

/// One session = one machine instance. Long-lived, outlives MCP reconnects.
pub struct Session {
    pub id: String,
    pub machine: Machine,
    /// Sessions boot PAUSED — no autonomous tick loop (idle-safe, Spec 744.3).
    pub running: bool,
    /// True once a CPU-isolated `monitor/exec` inject (`wr` / `r pc=`) has run.
    /// Distinguishes the CPU/chip-ISOLATED gates (which inject a program + set PC,
    /// then run on FlatRam/CiaBus/VicBus) from the FULL-MACHINE boot scenarios
    /// (session/create → session/run straight from the KERNAL reset vector, run
    /// on the assembled FullBus). False at boot ⇒ full-machine run path.
    pub injected: bool,
    /// True once a `wr io` (I/O-lens) inject has run — i.e. a render scenario that
    /// programmed the VIC/colour-RAM via `Machine::poke_io` and then runs a parked
    /// frame to SWEEP the per-cycle renderer. Unlike `injected` (which routes the
    /// run onto the chip-ISOLATED bus for cycle-exact CPU gates), an io-inject
    /// still needs the FULL VIC-ticked machine so the per-cycle draw accumulates
    /// the displayed frame. So: io_injected ⇒ keep the full-machine run path.
    pub io_injected: bool,
    /// Active trace: sibling `.c64retrace` path + accumulated meta. When set,
    /// session/run streams CpuStep/RAM_WRITE/IO_WRITE frames into a FrameSink and
    /// flushes to this path. `None` = no trace.
    pub trace: Option<TraceState>,
    pub disk_path: String,
    /// BUG-023-cart / Spec 742 — the host `.crt` path of the live cartridge (= the
    /// c64re `session.cartPath`), so a writable (EasyFlash) cart can write its
    /// programmed flash back on eject/persist. Empty when no cart or uploaded bytes
    /// with no backing path. Set by the CRT media/ingress path; cleared on eject.
    pub cart_path: String,
    /// Spec 786 — machine power state. `true` = built + booted (live);
    /// `false` = powered off (blank `Machine::new()`, no live state). Guards
    /// the three lifecycle primitives (`power_on` / `power_off` / `warm_reset`).
    pub powered: bool,
    /// Spec 786 — media registry: cartridge held WHILE POWERED OFF. A
    /// power-off transplants the live cart here (flash intact); a power-on
    /// transplants it back into the fresh machine. `None` while powered (the
    /// machine is the source of truth then). Cart insert/eject mutate this
    /// across a power-cycle (off → mutate → on).
    pub inserted_cart: Option<InsertedCart>,
    /// Spec 786 — media registry: disk image held while powered off (writes
    /// intact). Same off↔machine transplant as the cartridge.
    pub inserted_disk: Option<DiskImage>,
}

/// Trace bookkeeping for an active `.c64retrace` capture.
pub struct TraceState {
    /// Absolute `.c64retrace` path (sibling of the `.duckdb` outputPath).
    pub retrace_path: std::path::PathBuf,
    /// JSON meta string embedded in the file header.
    pub meta_json: String,
    /// Cycle at which the trace started (= TS cycleStart).
    pub cycle_start: u64,
    /// Accumulated frame buffer (header + events), flushed at trace/run/stop.
    pub buf: Vec<u8>,
    /// runId for status replies.
    pub run_id: String,
    pub event_count: u64,
    /// Domains requested for this trace (= TS trace/start_domains domains). The
    /// daemon maps these → channels to filter which records are emitted, and
    /// whether the VIC must be ticked (vic domain → VIC-isolated run path).
    pub domains: Vec<String>,
    /// T2.6 — manual phase markers: (cpu_clk, label). Matches TS `run.marks[]`
    /// (`trace-run.ts` ActiveRun.run.marks + markCount).
    pub marks: Vec<(u64, String)>,
    /// ws-trace-monitor-misc-23 — the real trace definition id (= TS RuntimeTraceRun
    /// .definitionId = def.id). For a `trace/start_domains` (captureAll) trace this is
    /// "live-capture"; for a `trace/run/start` of a registered definition it is THAT
    /// definition's id. finalize_trace echoes it (NOT a hardcoded "live-capture").
    pub definition_id: String,
    /// The definition's version (= TS RuntimeTraceRun.definitionVersion).
    pub definition_version: i64,
    /// Wall-clock ms at trace start (= TS ActiveRun.startWall = Date.now()); used to
    /// compute `overheadMs` at stop (= TS run.overheadMs = Date.now() - startWall).
    pub start_wall_ms: u128,
    /// The mounted-media identity SHA captured at start (= TS run.media.sha256), or
    /// empty when no disk was attached. Echoed in the stop descriptor's `media`.
    pub media_sha: String,
    /// The mounted-media source name (basename), echoed in the stop descriptor's
    /// `media` (= TS run.media.sourceName). Empty when none.
    pub media_name: String,
    /// Spec 708 §11 / 708.7 — the DECLARED capture kinds for this run (`cpu-row`,
    /// `mem-row`, `vic-row`, `iec-row`, `drive-cpu-row`). The trace DOMAINS open the
    /// channels; the CAPTURES select which rows are KEPT (= TS `declaredCaptures`,
    /// trace-run.ts:287). A `trace/start_domains` (captureAll) trace declares ALL the
    /// kinds its domains imply; a `trace/run/start` of a registered definition declares
    /// exactly the def's captures, so a def opening the `memory` domain but declaring
    /// only `cpu-row` DROPS mem rows (the 708.7 selection — not a silent no-op).
    pub captures: Vec<String>,
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            machine: Machine::new(),
            running: false,
            injected: false,
            io_injected: false,
            trace: None,
            disk_path: String::new(),
            cart_path: String::new(),
            powered: false,
            inserted_cart: None,
            inserted_disk: None,
        }
    }

    /// Boot the session: load ROMs from `rom_dir` and cold-reset the machine.
    /// This is the daemon-startup init; it leaves the session PAUSED
    /// (`running` stays false — Spec 744.3) but POWERED (the machine is now
    /// live). The run loop is started separately by the daemon/UI.
    pub fn boot(&mut self, rom_dir: &std::path::Path) -> Result<(), trx64_core::RomError> {
        self.machine.boot_from_dir(rom_dir)?;
        self.powered = true;
        Ok(())
    }

    // ---- Spec 786 — power lifecycle: 3 guarded primitives ----------------

    /// Power the machine ON: full initialisation IDENTICAL to the daemon
    /// startup (`Machine::new()` + `boot_from_dir`), then re-attach whatever
    /// media is registered. This is the ONLY path that yields fresh I/O chips
    /// (VIC / CIA1 / CIA2 come only from `Machine::new()`; `cold_reset` cannot
    /// clear stale chip state) — so nothing from a previous run survives.
    /// Comes up RUNNING. No-op if already powered (a real machine can't be
    /// switched on twice).
    pub fn power_on(&mut self, rom_dir: &std::path::Path) -> Result<(), trx64_core::RomError> {
        if self.powered {
            return Ok(());
        }
        let mut machine = Machine::new();
        machine.boot_from_dir(rom_dir)?;
        // Re-insert the registered cartridge: transplant the live mapper +
        // image (flash preserved), then cold-reset so the machine re-vectors
        // $FFFC THROUGH the cart (boots INTO it, like a real insert + power-on).
        // No cart ⇒ the KERNAL boot from `boot_from_dir` stands.
        if let Some(cart) = self.inserted_cart.take() {
            self.cart_path = cart.path.clone();
            machine.cartridge = Some(cart.mapper);
            machine.cartridge_image = Some(cart.image);
            machine.cold_reset();
        }
        // Re-attach the registered disk (writes intact).
        if let Some(disk) = self.inserted_disk.take() {
            machine.drive8.attach_disk(disk);
        }
        self.machine = machine;
        self.running = true;
        self.powered = true;
        self.injected = false;
        self.io_injected = false;
        Ok(())
    }

    /// Power the machine OFF: everything off, no live state. Flush pending disk
    /// writes, transplant the live media into the registry (physical media
    /// survive a power cut — EasyFlash flash + disk writes), then blank the
    /// machine to a dead `Machine::new()` (no ROMs). No-op if already off.
    pub fn power_off(&mut self) {
        if !self.powered {
            return;
        }
        // Physical cart survives: move the live mapper + image into the
        // registry so the next power-on re-inserts it (flash intact).
        if let Some(mapper) = self.machine.cartridge.take() {
            let image = self
                .machine
                .cartridge_image
                .take()
                .expect("cartridge_image present whenever cartridge is present");
            self.inserted_cart = Some(InsertedCart { image, mapper, path: self.cart_path.clone() });
        }
        // Physical disk survives: flush in-flight writes into the image bytes,
        // then move the image into the registry.
        self.machine.drive8.flush_disk_writeback();
        self.inserted_disk = self.machine.drive8.disk.take();
        self.machine = Machine::new();
        self.running = false;
        self.powered = false;
    }

    /// HW RESET line: jump through $FFFC ($FCE2 for the stock KERNAL), RAM +
    /// media preserved, I/O chips reset (= `Machine::warm_reset`). Recovers a
    /// running OR jammed machine. No-op if powered off.
    pub fn warm_reset(&mut self) {
        if !self.powered {
            return;
        }
        self.machine.warm_reset();
    }

    /// Spec 786 — register a cartridge in the media registry (insert). Parses
    /// the `.crt` bytes into a fresh mapper; takes effect on the next
    /// `power_on`. The daemon composes an insert as power_off → set → power_on.
    pub fn set_inserted_cart(
        &mut self,
        bytes: &[u8],
        name: &str,
        path: &str,
    ) -> Result<trx64_core::cart::MapperType, trx64_core::cart::CrtError> {
        let (image, mapper) = trx64_core::cart::load_cartridge_from_bytes(bytes, name, None)?;
        let mt = image.mapper_type;
        self.inserted_cart = Some(InsertedCart { image, mapper, path: path.to_string() });
        Ok(mt)
    }

    /// Spec 786 — drop the registered cartridge (eject). Takes effect on the
    /// next `power_on`. The daemon composes an eject as power_off → clear →
    /// power_on.
    pub fn clear_inserted_cart(&mut self) {
        self.inserted_cart = None;
        self.cart_path = String::new();
    }

    /// Native fast snapshot: clone the Machine (ADR-002).
    /// Zero-copy for Phase-2 COW forks.
    pub fn take_snapshot(&self) -> Machine {
        self.machine.clone()
    }

    /// Restore from a native snapshot.
    pub fn restore_snapshot(&mut self, snap: Machine) {
        self.machine = snap;
    }
}

/// Phase-2 mutation-search primitive (sketch — built after Phase-1 parity is green).
///
/// COW-fork `base` per overlay, warp `run_for(budget)` with probes, stream compact
/// verdicts back. The reason TRX64 exists; Node's single-thread loop can't do this.
pub struct Overlay {
    /// (addr, bytes) patches = coder overlay / crack applied to a forked machine.
    pub patches: Vec<(u16, Vec<u8>)>,
}
