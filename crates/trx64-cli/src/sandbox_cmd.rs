//! `trx64cli sandbox` — one-shot real-core execution sandbox (Spec 787 v1 + 788).
//!
//! Boot a FRESH machine (this process = one throwaway scratch instance, Spec 787
//! v1: the CLI already runs its own in-process machine, no daemon), load bytes,
//! run the title's OWN routine to a sentinel, and harvest a RAM slice. The routine
//! runs on the AUTHORITATIVE 6502 (`trx64-core`), not the TS `Cpu6502` shadow — so
//! a depacker that touches banking/IO executes for real. The harvest reads the raw
//! 64K `ram` field (ignores banking = the unpacked bytes as written).
//!
//! Two entry mechanisms:
//!
//!   * STUB (default): plant `sei; lda #io; sta $01; jsr entry; jmp self` and run.
//!     `jsr entry` pushes its own return address, so when `entry` RTSs the PC lands
//!     on the `jmp self` at `stub+8`, which we breakpoint. Banking ($01) is set via
//!     a real CPU store so the memconfig updates (a raw poke would not). NOTE: the
//!     `lda #io` CLOBBERS A, so this path cannot faithfully seed A at entry.
//!
//!   * DIRECT-ENTRY (`--direct-entry`, or auto-enabled by any `--reg-*`): the
//!     Spec-788 faithful match to the TS reference sandbox. Set PC = entry directly,
//!     seed A/X/Y/SP/P so the depacker ENTRY observes them, set banking by poking
//!     the CPU port + recomputing the PLA memconfig (no clobbering stub), and
//!     pre-stage the RTS sentinel on the stack EXACTLY like the TS runner
//!     (`sandbox-runner.ts:127-130`: $01FE=$FD, $01FF=$FF) so a top-level RTS pops
//!     $FFFD → $FFFE = the sentinel breakpoint. This reproduces the TS setup byte
//!     for byte: PC-set + reg-seed + stack-staged sentinel.
//!
//! Spec 788 Slice 1 piece A: this is the prerequisite for rerouting C64RE's
//! `sandbox_depack` off the flat-64K TS shadow onto the real core. The "match the
//! TS reference" here is a ONE-TIME migration cross-check (the TS shadow is being
//! replaced), not an eternal parity mandate.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::json;
use trx64_core::c64re_snapshot::restore_runtime_checkpoint;
use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::native_snapshot::read_native_snapshot;
use trx64_core::{BusKind, Machine, Observer, RunStop};

use crate::disasm_cmd::parse_addr;

const DEFAULT_STUB_ADDR: u16 = 0x02a7; // free RAM ($02a7-$02ff), untouched by our no-boot path
const DEFAULT_IO: u8 = 0x37; // KERNAL+BASIC+IO visible (standard post-reset config)
const DEFAULT_CYC_CAP: u64 = 100_000_000;
const DEFAULT_INSTR_CAP: u64 = 40_000_000;

/// The RTS-to-sentinel landing address, direct-entry mode. Matches the TS runner:
/// stack $01FE=$FD/$01FF=$FF ⇒ top-level RTS pops $FFFD, +1 → $FFFE.
const SENTINEL_PC: u16 = 0xfffe;
/// TS `Cpu6502` default flags when `initialFlags` is undefined (cpu6502.ts:46-51 →
/// getFlags()): N=V=D=I=C=0, Z=1, plus the unused bit ⇒ $22. Used as the direct-entry
/// P default so the entry P matches the TS reference when `--reg-p` is omitted.
const TS_DEFAULT_P: u8 = 0x22;

/// A `--load FILE@ADDR` spec. `addr = None` ⇒ FILE is a `.prg` whose 2-byte header
/// supplies the load address.
#[derive(Clone)]
pub struct SandboxLoad {
    pub file: PathBuf,
    pub addr: Option<u16>,
}

/// Parse `FILE@ADDR` (ADDR hex: `$c000` / `0xc000` / `c000`). No `@ADDR` ⇒ PRG header.
pub fn parse_load(s: &str) -> Result<SandboxLoad, String> {
    match s.rsplit_once('@') {
        Some((f, a)) => Ok(SandboxLoad { file: PathBuf::from(f), addr: Some(parse_addr(a)?) }),
        None => Ok(SandboxLoad { file: PathBuf::from(s), addr: None }),
    }
}

/// Parse `ADDR:LEN` — ADDR hex, LEN decimal or `0x`/`$` hex.
fn parse_harvest(s: &str) -> Result<(u16, usize), String> {
    let (a, l) = s
        .split_once(':')
        .ok_or_else(|| format!("bad --harvest '{s}' (want ADDR:LEN, e.g. $4000:0x800)"))?;
    let addr = parse_addr(a)?;
    let len = if let Some(h) = l.strip_prefix("0x").or_else(|| l.strip_prefix('$')) {
        usize::from_str_radix(h, 16)
    } else {
        l.parse::<usize>()
    }
    .map_err(|_| format!("bad --harvest length '{l}' (decimal or 0x-hex)"))?;
    Ok((addr, len))
}

/// Parse `ADDR=VAL` — a zero-page byte to seed before the run (ADDR 00-ff hex,
/// VAL hex). Depackers take their src/dst pointers here (e.g. `--zp $fb=$00`).
fn parse_zp(s: &str) -> Result<(u16, u8), String> {
    let (a, v) = s
        .split_once('=')
        .ok_or_else(|| format!("bad --zp '{s}' (want ADDR=VAL, e.g. $fb=$00)"))?;
    let addr = parse_addr(a)?;
    if addr > 0xff {
        return Err(format!("--zp address ${addr:04x} is not zero-page (00-ff)"));
    }
    // --zp values are always hex (unchanged from the original), e.g. `fd=40` = $40.
    let vh = v.strip_prefix('$').or_else(|| v.strip_prefix("0x")).unwrap_or(v);
    let val = u8::from_str_radix(vh, 16).map_err(|_| format!("bad --zp value '{v}' (hex byte)"))?;
    Ok((addr, val))
}

/// Parse a byte literal: `$xx` / `0xXX` hex, or decimal. Used by `--reg-*`.
fn parse_byte(s: &str) -> Result<u8, String> {
    if let Some(h) = s.strip_prefix('$').or_else(|| s.strip_prefix("0x")) {
        u8::from_str_radix(h, 16).map_err(|_| format!("bad byte '{s}' (hex)"))
    } else {
        s.parse::<u8>().map_err(|_| format!("bad byte '{s}' (decimal or $hex)"))
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Collapse a sorted-unique set of written addresses into contiguous runs
/// `[(lo,hi)]`. This is what the TS depack harvest needs: `genericSandboxDepack`
/// (`sandbox-depack-generic.ts:117-141`) either slices the run starting at a given
/// `dest`, or picks the LARGEST contiguous run in the write set. Emitting every run
/// lets the C64RE caller reproduce that selection from the JSON alone.
fn contiguous_runs(addrs: &[u16]) -> Vec<(u16, u16)> {
    let mut runs = Vec::new();
    let mut i = 0;
    while i < addrs.len() {
        let lo = addrs[i];
        let mut hi = lo;
        while i + 1 < addrs.len() && hi.checked_add(1) == Some(addrs[i + 1]) {
            hi = addrs[i + 1];
            i += 1;
        }
        runs.push((lo, hi));
        i += 1;
    }
    runs
}

/// The parsed, defaulted sandbox request.
pub struct SandboxArgs {
    pub rom_dir: PathBuf,
    /// Optional `.c64re` snapshot to restore into the fresh machine before running
    /// (loader-resident seed) — the routine then runs on top of that state.
    pub seed: Option<String>,
    /// Optional cart to attach on the cold machine (a cart-resident depacker's ROM).
    pub cart: Option<String>,
    /// Optional disk to attach on the cold machine (for a drive-reading routine).
    pub disk: Option<String>,
    pub loads: Vec<SandboxLoad>,
    pub entry: u16,
    pub harvest_addr: u16,
    pub harvest_len: usize,
    pub zp: Vec<(u16, u8)>,
    pub sentinel: Option<u16>,
    pub io: u8,
    pub stub_addr: u16,
    pub cyc_cap: u64,
    pub instr_cap: u64,
    /// Direct-entry mode (TS-faithful): PC=entry + reg-seed + staged RTS sentinel,
    /// instead of the `jsr entry` stub. Auto-enabled when any `reg_*` is set.
    pub direct_entry: bool,
    /// Registers observed at ENTRY (direct-entry only). `None` ⇒ TS defaults
    /// (A/X/Y=0, SP=$FD, P=$22).
    pub reg_a: Option<u8>,
    pub reg_x: Option<u8>,
    pub reg_y: Option<u8>,
    pub reg_sp: Option<u8>,
    pub reg_p: Option<u8>,
    pub json: bool,
}

impl SandboxArgs {
    /// True when the run should use the direct-entry mechanism (explicit flag or any
    /// register seed requested).
    fn wants_direct_entry(&self) -> bool {
        self.direct_entry
            || self.reg_a.is_some()
            || self.reg_x.is_some()
            || self.reg_y.is_some()
            || self.reg_sp.is_some()
            || self.reg_p.is_some()
    }
}

/// Thin CLI adapter: parse the raw clap fields, apply defaults, run.
#[allow(clippy::too_many_arguments)]
pub fn run_sandbox_cli(
    rom_dir: &Path,
    seed: Option<&str>,
    cart: Option<&str>,
    disk: Option<&str>,
    load: &[String],
    entry: u16,
    harvest: &str,
    zp: &[String],
    sentinel: Option<u16>,
    io: Option<&str>,
    stub_addr: Option<u16>,
    cyc_cap: Option<u64>,
    instr_cap: Option<u64>,
    direct_entry: bool,
    reg_a: Option<&str>,
    reg_x: Option<&str>,
    reg_y: Option<&str>,
    reg_sp: Option<&str>,
    reg_p: Option<&str>,
    json: bool,
) -> Result<String, String> {
    let loads = load.iter().map(|s| parse_load(s)).collect::<Result<Vec<_>, _>>()?;
    let (harvest_addr, harvest_len) = parse_harvest(harvest)?;
    let zp = zp.iter().map(|s| parse_zp(s)).collect::<Result<Vec<_>, _>>()?;
    let io = match io {
        // --io is always hex (unchanged from the original), e.g. `$37` / `37`.
        Some(s) => {
            let h = s.strip_prefix('$').or_else(|| s.strip_prefix("0x")).unwrap_or(s);
            u8::from_str_radix(h, 16).map_err(|_| format!("bad --io '{s}' (hex byte, e.g. $37)"))?
        }
        None => DEFAULT_IO,
    };
    let opt_byte = |o: Option<&str>, name: &str| -> Result<Option<u8>, String> {
        o.map(|s| parse_byte(s).map_err(|_| format!("bad --{name} '{s}' (hex byte, e.g. $ff)")))
            .transpose()
    };
    run_sandbox(&SandboxArgs {
        rom_dir: rom_dir.to_path_buf(),
        seed: seed.map(|s| s.to_string()),
        cart: cart.map(|s| s.to_string()),
        disk: disk.map(|s| s.to_string()),
        loads,
        entry,
        harvest_addr,
        harvest_len,
        zp,
        sentinel,
        io,
        stub_addr: stub_addr.unwrap_or(DEFAULT_STUB_ADDR),
        cyc_cap: cyc_cap.unwrap_or(DEFAULT_CYC_CAP),
        instr_cap: instr_cap.unwrap_or(DEFAULT_INSTR_CAP),
        direct_entry,
        reg_a: opt_byte(reg_a, "reg-a")?,
        reg_x: opt_byte(reg_x, "reg-x")?,
        reg_y: opt_byte(reg_y, "reg-y")?,
        reg_sp: opt_byte(reg_sp, "reg-sp")?,
        reg_p: opt_byte(reg_p, "reg-p")?,
        json,
    })
}

/// Counts retired instructions and records the set of addresses written during the
/// run. Real writes only (not the 6502 dummy-write cycle), and $0000-$01ff excluded
/// — the CPU port ($00/$01) and the stack page (jsr/rts + pushes, PHP/PHA) are
/// machinery, not the routine's output. Depack output lands in main RAM ($0200+).
struct SandboxObs {
    steps: u64,
    write_lo: Option<u16>,
    write_hi: Option<u16>,
    /// One bit per 16-bit address; true = the routine wrote there (>$01ff). Scanned
    /// at the end into contiguous runs for the JSON write-map.
    written: Box<[bool]>,
}

impl SandboxObs {
    fn new() -> Self {
        Self {
            steps: 0,
            write_lo: None,
            write_hi: None,
            written: vec![false; 0x1_0000].into_boxed_slice(),
        }
    }

    /// Sorted contiguous runs of the written address set (>$01ff).
    fn runs(&self) -> Vec<(u16, u16)> {
        let addrs: Vec<u16> = (0..0x1_0000usize)
            .filter(|&a| self.written[a])
            .map(|a| a as u16)
            .collect();
        contiguous_runs(&addrs)
    }
}

impl Observer for SandboxObs {
    #[allow(clippy::too_many_arguments)]
    fn on_instruction(
        &mut self,
        _pc: u16,
        _opcode: u8,
        _b1: u8,
        _b2: u8,
        _a: u8,
        _x: u8,
        _y: u8,
        _sp: u8,
        _p: u8,
        _clk: u64,
    ) {
        self.steps += 1;
    }
    fn on_bus(&mut self, kind: BusKind, addr: u16, _value: u8, _pc: u16, _clk: u64, _old: u8) {
        if matches!(kind, BusKind::Write) && addr > 0x01ff {
            self.write_lo = Some(self.write_lo.map_or(addr, |lo| lo.min(addr)));
            self.write_hi = Some(self.write_hi.map_or(addr, |hi| hi.max(addr)));
            self.written[addr as usize] = true;
        }
    }
    fn on_interrupt(&mut self, _vector: u16, _clk: u64) {}
}

/// The structured result of a sandbox run — formatted into text/JSON by
/// `run_sandbox`, asserted directly by the tests.
struct SandboxOutcome {
    /// `true` when the run stopped on a breakpoint (sentinel_rts / stop_pc), not a
    /// cap-out.
    ok: bool,
    /// TS `StopReason` vocab (see the mapping comment in `execute_sandbox`):
    /// `sentinel_rts` | `stop_pc` | `max_steps`.
    stop_reason: &'static str,
    pc: u16,
    cycles: u64,
    steps: u64,
    written_span: Option<(u16, u16)>,
    runs: Vec<(u16, u16)>,
    final_a: u8,
    final_x: u8,
    final_y: u8,
    final_sp: u8,
    final_p: u8,
    harvest_addr: u16,
    harvest: Vec<u8>,
}

pub fn run_sandbox(args: &SandboxArgs) -> Result<String, String> {
    let mut m = Machine::new();
    m.boot_from_dir(&args.rom_dir)
        .map_err(|e| format!("boot ROMs from {}: {e:?}", args.rom_dir.display()))?;

    // Optional seed: restore a .c64re snapshot into the booted machine (a loader-
    // resident state), then run the routine on top of it. Mirrors the daemon undump:
    // re-attach drive8 media FIRST, then restore_runtime_checkpoint.
    if let Some(seed) = &args.seed {
        let bytes = std::fs::read(seed).map_err(|e| format!("read seed {seed}: {e}"))?;
        let read = read_native_snapshot(&bytes).map_err(|e| format!("seed {seed}: {e}"))?;
        for rm in &read.media {
            if rm.reference.role != "drive8" {
                continue;
            }
            let Some(mbytes) = rm.bytes.clone() else { continue };
            let kind = if rm.reference.format == "d64" { DiskKind::D64 } else { DiskKind::G64 };
            m.drive8.attach_disk(DiskImage {
                kind,
                bytes: mbytes,
                backing_path: rm.reference.source_name.clone(),
                read_only: false,
            });
        }
        restore_runtime_checkpoint(&mut m, &read.checkpoint)
            .map_err(|e| format!("seed restore: {e}"))?;
    }

    // Optional cold media attach (a cart-resident depacker's ROM / a drive-read
    // routine's disk). Banking is the caller's job (--io / the routine).
    if let Some(cart) = &args.cart {
        let bytes = std::fs::read(cart).map_err(|e| format!("read cart {cart}: {e}"))?;
        let name = std::path::Path::new(cart)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("cart");
        m.attach_cart_from_bytes(&bytes, name)
            .map_err(|e| format!("attach cart {cart}: {e:?}"))?;
    }
    if let Some(disk) = &args.disk {
        let bytes = std::fs::read(disk).map_err(|e| format!("read disk {disk}: {e}"))?;
        let kind = if disk.to_ascii_lowercase().ends_with(".g64") {
            DiskKind::G64
        } else {
            DiskKind::D64
        };
        m.drive8.attach_disk(DiskImage {
            kind,
            bytes,
            backing_path: Some(disk.clone()),
            read_only: true,
        });
    }

    let direct = args.wants_direct_entry();

    // Apply --load blobs (the PRG header supplies the address when @ADDR is omitted).
    for ld in &args.loads {
        let data = std::fs::read(&ld.file).map_err(|e| format!("read {}: {e}", ld.file.display()))?;
        let (addr, body): (u16, &[u8]) = match ld.addr {
            Some(a) => (a, &data[..]),
            None => {
                if data.len() < 2 {
                    return Err(format!(
                        "{}: too short for a PRG (2-byte load-address header); use FILE@ADDR",
                        ld.file.display()
                    ));
                }
                ((data[0] as u16) | ((data[1] as u16) << 8), &data[2..])
            }
        };
        // A load that overlaps the entry stub would clobber it (stub mode only —
        // direct-entry plants no stub).
        if !direct {
            let s = args.stub_addr as u32;
            let (l0, l1) = (addr as u32, addr as u32 + body.len() as u32);
            if l0 < s + 11 && s < l1 {
                eprintln!(
                    "warning: load ${:04x}..+{} overlaps the entry stub ${:04x}..+11 — move it with --stub-addr",
                    addr,
                    body.len(),
                    args.stub_addr
                );
            }
        }
        m.poke(addr, body);
    }

    let outcome = execute_sandbox(&mut m, args);

    if args.json {
        let out = json!({
            "ok": outcome.ok,
            "stopReason": outcome.stop_reason,
            "pc": outcome.pc,
            "cycles": outcome.cycles,
            "steps": outcome.steps,
            "writtenSpan": outcome.written_span.map(|(lo, hi)| json!({ "lo": lo, "hi": hi })),
            // Compact write-map: the contiguous written runs (>$01ff), sorted. The
            // C64RE depack harvest slices `dest` OR the largest run from these.
            "writtenRuns": outcome
                .runs
                .iter()
                .map(|(lo, hi)| json!({ "lo": lo, "hi": hi }))
                .collect::<Vec<_>>(),
            // Registers at stop (TS SandboxRunResult.finalState).
            "finalRegs": {
                "a": outcome.final_a,
                "x": outcome.final_x,
                "y": outcome.final_y,
                "sp": outcome.final_sp,
                "p": outcome.final_p,
            },
            "harvest": {
                "addr": outcome.harvest_addr,
                "len": outcome.harvest.len(),
                "hex": hex(&outcome.harvest),
            },
        });
        serde_json::to_string(&out).map_err(|e| e.to_string())
    } else {
        let span = outcome
            .written_span
            .map(|(lo, hi)| format!(" writes=${lo:04x}..${hi:04x}"))
            .unwrap_or_default();
        let runs = if outcome.runs.is_empty() {
            String::new()
        } else {
            let parts: Vec<String> =
                outcome.runs.iter().map(|(lo, hi)| format!("${lo:04x}..${hi:04x}")).collect();
            format!(" runs=[{}]", parts.join(","))
        };
        Ok(format!(
            "sandbox: stop={} pc=${:04x} cycles={} steps={} a=${:02x} x=${:02x} y=${:02x} sp=${:02x} p=${:02x}{span}{runs}  harvest ${:04x}..+{} = {}",
            outcome.stop_reason,
            outcome.pc,
            outcome.cycles,
            outcome.steps,
            outcome.final_a,
            outcome.final_x,
            outcome.final_y,
            outcome.final_sp,
            outcome.final_p,
            outcome.harvest_addr,
            outcome.harvest.len(),
            hex(&outcome.harvest)
        ))
    }
}

/// Set up the entry mechanism on an already-booted + loaded machine, run to the
/// sentinel/cap, and harvest. Split out of `run_sandbox` so the register/sentinel/
/// write-map behaviour is testable without file I/O.
fn execute_sandbox(m: &mut Machine, args: &SandboxArgs) -> SandboxOutcome {
    // Seed zero-page bytes (depacker src/dst pointers etc.). $00/$01 are the CPU
    // port — in stub mode $01 is set by the stub via --io, so a --zp $01 would be
    // overwritten; in direct-entry the port is set below.
    for (addr, val) in &args.zp {
        m.poke(*addr, &[*val]);
    }

    let mut bp: HashSet<u16> = HashSet::new();
    let sentinel_landing: u16;

    if args.wants_direct_entry() {
        // ── DIRECT-ENTRY: TS-faithful (sandbox-runner.ts runSandbox). ────────────
        // Banking without a stub: poke the CPU port AND recompute the live PLA
        // memconfig (a raw poke of $01 alone would not update the memconfig; this
        // reproduces the stub's `sta $01` effect). port_dir stays at the boot $2f
        // so all low-3 port bits are outputs = the value we write is what the PLA
        // sees. --io $34 ⇒ loram=hiram=0 ⇒ RAM under $A000-$FFFF and $D000-$DFFF
        // (all-RAM), matching the TS flat-64K shadow.
        m.port_data = args.io;
        m.ram[0x0001] = args.io;
        m.memconfig = m.memconfig_table[m.pla_index()];

        // Seed the registers the depacker ENTRY observes (TS: cpu.pc/a/x/y/sp set
        // directly; setFlags only if initialFlags given ⇒ default P $22).
        m.c64_core.reg_a = args.reg_a.unwrap_or(0);
        m.c64_core.reg_x = args.reg_x.unwrap_or(0);
        m.c64_core.reg_y = args.reg_y.unwrap_or(0);
        m.c64_core.reg_sp = args.reg_sp.unwrap_or(0xfd);
        m.c64_core.set_status_composite(args.reg_p.unwrap_or(TS_DEFAULT_P));

        // Pre-stage the sentinel return on the stack EXACTLY like the TS runner
        // (sandbox-runner.ts:127-130): $01FE=$FD, $01FF=$FF ⇒ a top-level RTS pops
        // $FFFD, +1 → $FFFE = SENTINEL_PC (breakpointed below). The CPU never
        // executes $FFFE (the breakpoint fires at the boundary before it).
        m.ram[0x01fe] = 0xfd;
        m.ram[0x01ff] = 0xff;

        // PC = entry directly. The full run reads c64_core.reg_pc; set the legacy
        // core's too for consistency (Spec 788 §6 PC gotcha).
        m.c64_core.reg_pc = args.entry;
        m.cpu6510.reg_pc = args.entry;

        sentinel_landing = SENTINEL_PC;
    } else {
        // ── STUB (default, unchanged): sei; lda #io; sta $01; jsr entry; jmp self. ─
        // `entry`'s RTS returns to the jmp-self at stub+8 (jsr pushed it), which we
        // breakpoint. Banking ($01) is set by the real store inside the stub.
        let s = args.stub_addr;
        let ret = s.wrapping_add(8);
        let stub = [
            0x78, // sei
            0xa9, args.io, // lda #io
            0x85, 0x01, // sta $01
            0x20, (args.entry & 0xff) as u8, (args.entry >> 8) as u8, // jsr entry
            0x4c, (ret & 0xff) as u8, (ret >> 8) as u8, // jmp ret (self-loop)
        ];
        m.poke(s, &stub);

        // PC gotcha (Spec 788 §6): set BOTH cores' PC to the stub.
        m.c64_core.reg_pc = s;
        m.cpu6510.reg_pc = s;

        sentinel_landing = ret;
    }

    bp.insert(sentinel_landing); // routine finished (RTS'd back to the sentinel)
    if let Some(extra) = args.sentinel {
        bp.insert(extra);
    }

    let clk0 = m.c64_core.clk;
    let mut obs = SandboxObs::new();
    let stop = m.run_for_full_capped_dbg(
        args.cyc_cap,
        args.instr_cap,
        Some(&bp),
        None,
        None,
        &mut obs,
        |_, _, _, _, _, _, _| {},
    );
    let cycles = m.c64_core.clk.wrapping_sub(clk0);
    let steps = obs.steps;
    let written_span = match (obs.write_lo, obs.write_hi) {
        (Some(lo), Some(hi)) => Some((lo, hi)),
        _ => None,
    };
    let runs = obs.runs();

    // Stop-reason vocab → the TS `StopReason` the C64RE depack harvest expects
    // (sandbox-runner.ts:138 / sandbox-depack-generic.ts:109):
    //   * RunStop::Breakpoint at the RTS-sentinel landing  → "sentinel_rts"
    //     (top-level RTS: stub `jmp self`, or direct-entry $FFFE).
    //   * RunStop::Breakpoint at any OTHER bp (--sentinel / explicit stop-PC)
    //                                                       → "stop_pc"
    //   * RunStop::CycleBudget (cycle cap) OR ::Completed (instruction cap) OR
    //     ::Observer                                        → "max_steps" (cap-out).
    let stop_reason = match stop {
        RunStop::Breakpoint(pc) if pc == sentinel_landing => "sentinel_rts",
        RunStop::Breakpoint(_) => "stop_pc",
        RunStop::CycleBudget | RunStop::Completed | RunStop::Observer => "max_steps",
    };
    let ok = matches!(stop, RunStop::Breakpoint(_));

    // Harvest the raw RAM slice (ignores banking = the unpacked bytes as written).
    let start = args.harvest_addr as usize;
    let end = (start + args.harvest_len).min(0x1_0000);
    let harvest = m.ram[start..end].to_vec();

    SandboxOutcome {
        ok,
        stop_reason,
        pc: m.c64_core.reg_pc,
        cycles,
        steps,
        written_span,
        runs,
        final_a: m.c64_core.reg_a,
        final_x: m.c64_core.reg_x,
        final_y: m.c64_core.reg_y,
        final_sp: m.c64_core.reg_sp,
        final_p: m.c64_core.status(),
        harvest_addr: args.harvest_addr,
        harvest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_load_with_and_without_addr() {
        let a = parse_load("foo.bin@$2000").unwrap();
        assert_eq!(a.addr, Some(0x2000));
        let b = parse_load("game.prg").unwrap();
        assert_eq!(b.addr, None);
    }

    #[test]
    fn parse_harvest_dec_and_hex() {
        assert_eq!(parse_harvest("$4000:0x800").unwrap(), (0x4000, 0x800));
        assert_eq!(parse_harvest("c000:16").unwrap(), (0xc000, 16));
        assert!(parse_harvest("nope").is_err());
    }

    #[test]
    fn parse_zp_ok_and_bounds() {
        assert_eq!(parse_zp("$fb=$00").unwrap(), (0xfb, 0x00));
        assert_eq!(parse_zp("fd=40").unwrap(), (0xfd, 0x40));
        assert!(parse_zp("$1000=$00").is_err()); // not zero-page
        assert!(parse_zp("nope").is_err());
    }

    #[test]
    fn parse_byte_hex_and_decimal() {
        assert_eq!(parse_byte("$ff").unwrap(), 0xff);
        assert_eq!(parse_byte("0x37").unwrap(), 0x37);
        assert_eq!(parse_byte("16").unwrap(), 16);
        assert!(parse_byte("$1ff").is_err()); // > u8
        assert!(parse_byte("nope").is_err());
    }

    #[test]
    fn contiguous_runs_splits_on_gaps() {
        // Two runs + a singleton, sorted.
        let addrs = [0x4000u16, 0xe000, 0xe001, 0xe002, 0xe004, 0xe005];
        assert_eq!(
            contiguous_runs(&addrs),
            vec![(0x4000, 0x4000), (0xe000, 0xe002), (0xe004, 0xe005)]
        );
        assert!(contiguous_runs(&[]).is_empty());
        assert_eq!(contiguous_runs(&[0xffff]), vec![(0xffff, 0xffff)]);
    }

    // ── Integration tests: run a hand-written routine on the real core. ──────────
    // These need the C64 ROMs (boot_from_dir). On this dev machine `default_rom_dir`
    // resolves the in-tree C64RE `resources/roms`; elsewhere the test soft-skips
    // (there is no CI — see MEMORY: no GitHub CI/CD).

    fn rom_dir_or_skip() -> Option<PathBuf> {
        let d = crate::default_rom_dir();
        if d.join("kernal-901227-03.bin").exists() {
            Some(d)
        } else {
            eprintln!("[skip] no C64 ROMs at {} — integration test skipped", d.display());
            None
        }
    }

    fn base_args(rom_dir: PathBuf) -> SandboxArgs {
        SandboxArgs {
            rom_dir,
            seed: None,
            cart: None,
            disk: None,
            loads: Vec::new(),
            entry: 0xc000,
            harvest_addr: 0xc000,
            harvest_len: 0,
            zp: Vec::new(),
            sentinel: None,
            io: DEFAULT_IO,
            stub_addr: DEFAULT_STUB_ADDR,
            cyc_cap: 10_000_000,
            instr_cap: 1_000_000,
            direct_entry: false,
            reg_a: None,
            reg_x: None,
            reg_y: None,
            reg_sp: None,
            reg_p: None,
            json: false,
        }
    }

    fn booted(rom_dir: &Path) -> Machine {
        let mut m = Machine::new();
        m.boot_from_dir(rom_dir).expect("boot ROMs");
        m
    }

    /// Direct-entry seeds A/X/Y/SP/P so the depacker ENTRY observes them, and an
    /// explicit --sentinel hit maps to the TS "stop_pc" vocab. The routine copies
    /// each seeded register to RAM at entry (before touching the stack), then JMPs
    /// to a stop label.
    #[test]
    fn direct_entry_seeds_all_regs_observed_at_entry() {
        let Some(rom_dir) = rom_dir_or_skip() else { return };
        let mut m = booted(&rom_dir);
        // c000: STA $4000  (entry A)
        // c003: STX $4001  (entry X)
        // c006: STY $4002  (entry Y)
        // c009: PHP        (entry P → stack)
        // c00a: PLA        (P → A)
        // c00b: STA $4003  (entry P)
        // c00e: TSX        (X = SP)
        // c00f: STX $4005  (entry SP)
        // c012: JMP $c015  (to stop label)
        // c015: NOP        (--sentinel breakpoints here)
        let routine = [
            0x8d, 0x00, 0x40, // STA $4000
            0x8e, 0x01, 0x40, // STX $4001
            0x8c, 0x02, 0x40, // STY $4002
            0x08, // PHP
            0x68, // PLA
            0x8d, 0x03, 0x40, // STA $4003
            0xba, // TSX
            0x8e, 0x05, 0x40, // STX $4005
            0x4c, 0x15, 0xc0, // JMP $c015
            0xea, // NOP (stop)
        ];
        m.poke(0xc000, &routine);

        let mut args = base_args(rom_dir);
        args.entry = 0xc000;
        args.io = 0x34; // all-RAM
        args.reg_a = Some(0x11);
        args.reg_x = Some(0x22);
        args.reg_y = Some(0x33);
        args.reg_sp = Some(0x80);
        args.reg_p = Some(0x25); // I set ⇒ IRQ-safe; unused+I+C = $25
        args.sentinel = Some(0xc015);
        args.harvest_addr = 0x4000;
        args.harvest_len = 6;

        let out = execute_sandbox(&mut m, &args);
        assert_eq!(out.stop_reason, "stop_pc", "explicit --sentinel ⇒ stop_pc");
        assert!(out.ok);
        // Harvest $4000..$4005 = [A, X, Y, P, <untouched>, SP].
        assert_eq!(out.harvest[0], 0x11, "entry A");
        assert_eq!(out.harvest[1], 0x22, "entry X");
        assert_eq!(out.harvest[2], 0x33, "entry Y");
        // PHP forces the B bit (0x10) set in the pushed copy — mask it to compare
        // against the seeded P ($25).
        assert_eq!(out.harvest[3] & 0xef, 0x25, "entry P (B bit from PHP masked)");
        assert_eq!(out.harvest[5], 0x80, "entry SP");
    }

    /// All-RAM ($34) depack: a routine writing to $E000 (under the KERNAL window) is
    /// harvestable, the write-map carries the contiguous runs + writtenSpan, the
    /// final registers are reported, and a top-level RTS maps to "sentinel_rts".
    #[test]
    fn direct_entry_all_ram_harvest_and_write_map() {
        let Some(rom_dir) = rom_dir_or_skip() else { return };
        let mut m = booted(&rom_dir);
        // c000: SEI
        // c001: LDX #$00
        // c003: TXA            (A = X)
        // c004: STA $e000,X    (dest run 1: $e000..$e00f = 00..0f)
        // c007: INX
        // c008: CPX #$10
        // c00a: BNE $c003
        // c00c: LDA #$aa
        // c00e: STA $4000      (dest run 2: single byte)
        // c011: RTS            (top-level ⇒ sentinel_rts)
        let routine = [
            0x78, // SEI
            0xa2, 0x00, // LDX #$00
            0x8a, // TXA
            0x9d, 0x00, 0xe0, // STA $e000,X
            0xe8, // INX
            0xe0, 0x10, // CPX #$10
            0xd0, 0xf7, // BNE $c003
            0xa9, 0xaa, // LDA #$aa
            0x8d, 0x00, 0x40, // STA $4000
            0x60, // RTS
        ];
        m.poke(0xc000, &routine);

        let mut args = base_args(rom_dir);
        args.entry = 0xc000;
        args.io = 0x34; // all-RAM: $E000 is RAM, so the STA lands + is harvestable
        args.direct_entry = true; // no reg seeds; use TS defaults (A/X/Y=0, SP=$fd)
        args.harvest_addr = 0xe000;
        args.harvest_len = 16;

        let out = execute_sandbox(&mut m, &args);
        assert_eq!(out.stop_reason, "sentinel_rts");
        assert!(out.ok);
        // Harvest under all-RAM: the 16 dest bytes as written.
        let expected: Vec<u8> = (0u8..16).collect();
        assert_eq!(out.harvest, expected, "$E000 dest harvestable under all-RAM");
        // Write-map: two contiguous runs, sorted.
        assert_eq!(out.runs, vec![(0x4000, 0x4000), (0xe000, 0xe00f)]);
        // writtenSpan = (min written addr, max written addr) = ($4000, $e00f).
        assert_eq!(out.written_span, Some((0x4000, 0xe00f)));
        // Final registers at stop.
        assert_eq!(out.final_a, 0xaa, "final A");
        assert_eq!(out.final_x, 0x10, "final X");
        assert_eq!(out.final_sp, 0xff, "SP after RTS popped the 2-byte sentinel");
    }

    /// The DEFAULT stub path is unchanged: no direct-entry/regs ⇒ `jsr entry` stub,
    /// entry RTS lands on the jmp-self and maps to "sentinel_rts", harvest works.
    #[test]
    fn stub_path_default_unchanged() {
        let Some(rom_dir) = rom_dir_or_skip() else { return };
        let mut m = booted(&rom_dir);
        // A routine reached via `jsr entry`: fill $4000..$4003 with $ee, then RTS.
        // c000: LDX #$00
        // c002: LDA #$ee
        // c004: STA $4000,X
        // c007: INX
        // c008: CPX #$04
        // c00a: BNE $c004
        // c00c: RTS
        let routine = [
            0xa2, 0x00, // LDX #$00
            0xa9, 0xee, // LDA #$ee
            0x9d, 0x00, 0x40, // STA $4000,X
            0xe8, // INX
            0xe0, 0x04, // CPX #$04
            0xd0, 0xf8, // BNE $c004
            0x60, // RTS
        ];
        m.poke(0xc000, &routine);

        let mut args = base_args(rom_dir);
        args.entry = 0xc000;
        args.io = 0x34;
        // direct_entry stays false, no reg seeds ⇒ stub path.
        args.harvest_addr = 0x4000;
        args.harvest_len = 4;

        assert!(!args.wants_direct_entry(), "must take the stub path");
        let out = execute_sandbox(&mut m, &args);
        assert_eq!(out.stop_reason, "sentinel_rts", "jsr/rts ⇒ jmp-self bp");
        assert!(out.ok);
        assert_eq!(out.harvest, vec![0xee; 4]);
        assert_eq!(out.runs, vec![(0x4000, 0x4003)]);
    }
}
